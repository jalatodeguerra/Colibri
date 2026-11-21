//! The dispatch loop (vm-plan §2.6).
//!
//! `exec` runs the top frame, which was pushed by [`Machine::enter`] with `NativeEntry`, until
//! that frame returns. A call to Colibri code pushes a frame and keeps looping; a return pops
//! one. The loop keeps the chunk, the window base and the pc in locals, and writes the pc to the
//! frame before anything that can call out (core operations, calls, panics).
//!
//! Safe indexing throughout: every register access is bounds checked. Step 14 may replace that
//! in one reviewed place if a profile shows it matters.

use std::io::Write;
use std::rc::Rc;

use colibri_core::ctl::{Ctl, Eval};
use colibri_core::num;
use colibri_core::prelude::*;
use colibri_core::value::{CellObj, Value, unwrap_cell};

use crate::bytecode::{Chunk, Op, Site};
use crate::fiber::{Frame, FrameKind, VmFiber, is_scalar_value};
use crate::machine::Machine;

impl Machine {
    /// Runs the entry frame on top of `fiber` to its end. On a panic every frame down to and
    /// including the entry frame is popped and every value they own is released.
    pub(crate) fn run(&mut self, fiber: &mut VmFiber) -> Eval {
        let depth = fiber.frames.len();
        match self.exec(fiber) {
            Ok(v) => Ok(v),
            Err(ctl) => {
                self.unwind(fiber, depth);
                Err(ctl)
            }
        }
    }

    /// Unwinding as of this step: pop the frames of the failed run and release everything
    /// above the entry frame's parameters, highest register first. Step 4 replaces the sweep by
    /// the compiler's landing pads, which run `defer`s and drop in scope order.
    fn unwind(&mut self, fiber: &mut VmFiber, depth: usize) {
        let entry = &fiber.frames[depth - 1];
        let first = entry.base as usize + entry.chunk.nparams as usize;
        let mark = entry.temps_mark as usize;
        fiber.frames.truncate(depth - 1);
        self.release_temps(fiber, mark);
        for i in (first..fiber.regs.len()).rev() {
            let v = std::mem::replace(&mut fiber.regs[i], Value::Unit);
            if !is_scalar_value(&v) {
                self.release(v);
            }
        }
    }

    /// Debug check of the Unit invariant when a frame pops: no register of its window holds a
    /// value (scalars left behind own nothing and are harmless). The parameters are the
    /// caller's, and the result register has been taken already.
    #[inline]
    fn check_window(fiber: &VmFiber, base: usize, chunk: &Chunk) {
        if cfg!(debug_assertions) {
            let window = &fiber.regs[base + chunk.nparams as usize..base + chunk.nregs as usize];
            for (i, v) in window.iter().enumerate() {
                assert!(
                    is_scalar_value(v),
                    "register r{} of `{}` still owns {v:?} when its frame pops",
                    i + chunk.nparams as usize,
                    chunk.body.name
                );
            }
        }
    }

    pub(crate) fn exec(&mut self, fiber: &mut VmFiber) -> Eval {
        let top = fiber.frames.last().expect("exec needs a frame");
        let mut chunk = Rc::clone(&top.chunk);
        let mut base = top.base as usize;
        let mut pc = top.pc as usize;

        macro_rules! reg {
            ($i:expr) => {
                fiber.regs[base + usize::from($i)]
            };
        }
        macro_rules! sync {
            () => {
                if let Some(f) = fiber.frames.last_mut() {
                    f.pc = pc as u32;
                }
            };
        }
        // A panic at the current instruction.
        macro_rules! fault {
            ($msg:expr) => {{
                sync!();
                let span = chunk.spans[pc - 1];
                return Err(self.panic(fiber, span, $msg));
            }};
        }
        macro_rules! internal {
            ($what:expr) => {{
                sync!();
                let span = chunk.spans[pc - 1];
                return Err(self.internal(fiber, span, $what));
            }};
        }
        macro_rules! int2 {
            ($a:expr, $b:expr) => {
                match (&reg!($a), &reg!($b)) {
                    (Value::Int(x), Value::Int(y)) => (*x, *y),
                    _ => internal!("ill-typed integer operation"),
                }
            };
        }
        macro_rules! float2 {
            ($a:expr, $b:expr) => {
                match (&reg!($a), &reg!($b)) {
                    (Value::Float(x), Value::Float(y)) => (*x, *y),
                    _ => internal!("ill-typed float operation"),
                }
            };
        }
        macro_rules! int_op {
            ($d:expr, $a:expr, $b:expr, $f:expr, $msg:expr) => {{
                let (x, y) = int2!($a, $b);
                match $f(x, y) {
                    Some(v) => reg!($d) = Value::Int(v),
                    None => fault!($msg),
                }
            }};
        }
        macro_rules! jump {
            ($off:expr) => {
                pc = (pc as isize + $off as isize) as usize
            };
        }
        // Compare an integer register with a constant and jump unless it holds.
        macro_rules! jmp_not_k {
            ($a:expr, $k:expr, $off:expr, $cmp:tt) => {{
                let Value::Int(x) = &reg!($a) else {
                    internal!("ill-typed integer comparison")
                };
                if !(*x $cmp i64::from($k)) {
                    jump!($off);
                }
            }};
        }
        macro_rules! jmp_not {
            ($a:expr, $b:expr, $off:expr, $cmp:tt) => {{
                let (x, y) = int2!($a, $b);
                if !(x $cmp y) {
                    jump!($off);
                }
            }};
        }
        macro_rules! cmp_i {
            ($d:expr, $a:expr, $b:expr, $cmp:tt) => {{
                let (x, y) = int2!($a, $b);
                reg!($d) = Value::Bool(x $cmp y);
            }};
        }
        macro_rules! cmp_f {
            ($d:expr, $a:expr, $b:expr, $cmp:tt) => {{
                let (x, y) = float2!($a, $b);
                reg!($d) = Value::Bool(x $cmp y);
            }};
        }
        macro_rules! arith_f {
            ($d:expr, $a:expr, $b:expr, $op:tt) => {{
                let (x, y) = float2!($a, $b);
                reg!($d) = Value::Float(x $op y);
            }};
        }

        loop {
            let op = chunk.code[pc];
            pc += 1;
            match op {
                Op::LoadUnit { d } => reg!(d) = Value::Unit,
                Op::LoadBool { d, b } => reg!(d) = Value::Bool(b),
                Op::LoadInt { d, i } => reg!(d) = Value::Int(i64::from(i)),
                Op::LoadK { d, k } => reg!(d) = chunk.consts[k as usize].clone(),
                Op::Copy { d, s } => {
                    let v = reg!(s).clone();
                    reg!(d) = v;
                }
                Op::Move { d, s } => {
                    let v = std::mem::replace(&mut reg!(s), Value::Unit);
                    reg!(d) = v;
                }
                Op::Drop { r } => {
                    let v = std::mem::replace(&mut reg!(r), Value::Unit);
                    if !is_scalar_value(&v) {
                        sync!();
                        self.release(v);
                    }
                }

                Op::StoreLocal { l, s } => {
                    let v = std::mem::replace(&mut reg!(s), Value::Unit);
                    let old = std::mem::replace(&mut reg!(l), v);
                    if !is_scalar_value(&old) {
                        sync!();
                        self.release(old);
                    }
                }
                Op::NewCell { l, s } => {
                    let v = std::mem::replace(&mut reg!(s), Value::Unit);
                    reg!(l) = Value::Cell(CellObj::new(v));
                }
                Op::CellGet { d, l } => {
                    let Value::Cell(c) = &reg!(l) else {
                        internal!("a captured variable is not in a cell")
                    };
                    let v = c.get();
                    reg!(d) = v;
                }
                Op::CellSet { l, s } => {
                    let v = std::mem::replace(&mut reg!(s), Value::Unit);
                    let Value::Cell(c) = &reg!(l) else {
                        internal!("a captured variable is not in a cell")
                    };
                    let old = c.set(v);
                    if !is_scalar_value(&old) {
                        sync!();
                        self.release(old);
                    }
                }
                Op::UnwrapCell { r } => {
                    let v = std::mem::replace(&mut reg!(r), Value::Unit);
                    reg!(r) = unwrap_cell(v);
                }

                Op::GetGlobal { d, g } => reg!(d) = self.globals[g as usize].clone(),
                Op::GetGlobalChecked { d, g } => {
                    if let Some(Some(_)) = self.uninit.get(g as usize) {
                        let msg = self.uninit_message(g);
                        fault!(msg);
                    }
                    reg!(d) = self.globals[g as usize].clone();
                }
                Op::CheckGlobal { g } => {
                    if let Some(Some(_)) = self.uninit.get(g as usize) {
                        let msg = self.uninit_message(g);
                        fault!(msg);
                    }
                }
                Op::SetGlobal { g, s } => {
                    let v = std::mem::replace(&mut reg!(s), Value::Unit);
                    let old = std::mem::replace(&mut self.globals[g as usize], v);
                    if !is_scalar_value(&old) {
                        sync!();
                        self.release(old);
                    }
                }
                Op::InitGlobal { g, s } => {
                    let v = std::mem::replace(&mut reg!(s), Value::Unit);
                    let old = std::mem::replace(&mut self.globals[g as usize], v);
                    if let Some(slot) = self.uninit.get_mut(g as usize) {
                        *slot = None;
                    }
                    if !is_scalar_value(&old) {
                        sync!();
                        self.release(old);
                    }
                }
                Op::CallInit { d, base: a, g } => {
                    let chunk_of_init =
                        match self.global_inits[usize::from(g)].chunk(None, &self.env) {
                            Ok(c) => c,
                            Err(msg) => internal!(&format!("cannot compile an initialiser: {msg}")),
                        };
                    sync!();
                    let span = chunk.spans[pc - 1];
                    self.push_call(
                        fiber,
                        chunk_of_init,
                        base + usize::from(a),
                        base + usize::from(d),
                        span,
                    )?;
                    let frame = fiber.frames.last().expect("just pushed");
                    chunk = Rc::clone(&frame.chunk);
                    base = frame.base as usize;
                    pc = 0;
                }

                Op::AddI { d, a, b } => int_op!(d, a, b, i64::checked_add, "integer overflow"),
                Op::SubI { d, a, b } => int_op!(d, a, b, i64::checked_sub, "integer overflow"),
                Op::MulI { d, a, b } => int_op!(d, a, b, i64::checked_mul, "integer overflow"),
                Op::DivI { d, a, b } => {
                    let (x, y) = int2!(a, b);
                    if y == 0 {
                        fault!("division by zero");
                    }
                    match x.checked_div(y) {
                        Some(v) => reg!(d) = Value::Int(v),
                        None => fault!("integer overflow"),
                    }
                }
                Op::RemI { d, a, b } => {
                    let (x, y) = int2!(a, b);
                    if y == 0 {
                        fault!("modulo by zero");
                    }
                    match x.checked_rem(y) {
                        Some(v) => reg!(d) = Value::Int(v),
                        None => fault!("integer overflow"),
                    }
                }
                Op::AddIK { d, a, i } => {
                    let Value::Int(x) = &reg!(a) else {
                        internal!("ill-typed integer operation")
                    };
                    match x.checked_add(i64::from(i)) {
                        Some(v) => reg!(d) = Value::Int(v),
                        None => fault!("integer overflow"),
                    }
                }
                Op::NegI { d, a } => {
                    let Value::Int(x) = &reg!(a) else {
                        internal!("ill-typed negation")
                    };
                    match x.checked_neg() {
                        Some(v) => reg!(d) = Value::Int(v),
                        None => fault!("integer overflow"),
                    }
                }
                Op::LtI { d, a, b } => cmp_i!(d, a, b, <),
                Op::LeI { d, a, b } => cmp_i!(d, a, b, <=),
                Op::EqI { d, a, b } => cmp_i!(d, a, b, ==),
                Op::NeI { d, a, b } => cmp_i!(d, a, b, !=),

                Op::AddF { d, a, b } => arith_f!(d, a, b, +),
                Op::SubF { d, a, b } => arith_f!(d, a, b, -),
                Op::MulF { d, a, b } => arith_f!(d, a, b, *),
                Op::DivF { d, a, b } => arith_f!(d, a, b, /),
                Op::NegF { d, a } => {
                    let Value::Float(x) = &reg!(a) else {
                        internal!("ill-typed negation")
                    };
                    reg!(d) = Value::Float(-*x);
                }
                Op::LtF { d, a, b } => cmp_f!(d, a, b, <),
                Op::LeF { d, a, b } => cmp_f!(d, a, b, <=),
                Op::EqF { d, a, b } => cmp_f!(d, a, b, ==),
                Op::NeF { d, a, b } => cmp_f!(d, a, b, !=),

                Op::Not { d, a } => {
                    let Value::Bool(x) = &reg!(a) else {
                        internal!("ill-typed `not`")
                    };
                    reg!(d) = Value::Bool(!*x);
                }
                Op::Binary { d, a, b } => {
                    let Op::Site { site } = chunk.code[pc] else {
                        internal!("a binary operation without its operator")
                    };
                    pc += 1;
                    let Site::Binary(bop) = &chunk.sites[usize::from(site)] else {
                        internal!("a binary operation without its operator")
                    };
                    let (l, r) = (reg!(a).clone(), reg!(b).clone());
                    sync!();
                    let span = chunk.spans[pc - 1];
                    let v = self.binary(fiber, *bop, l, r, span)?;
                    reg!(d) = v;
                }
                Op::Unary { d, a, site } => {
                    let Site::Unary(uop) = &chunk.sites[usize::from(site)] else {
                        internal!("a unary operation without its operator")
                    };
                    let v = reg!(a).clone();
                    sync!();
                    let span = chunk.spans[pc - 1];
                    let v = self.unary(fiber, *uop, v, span)?;
                    reg!(d) = v;
                }

                Op::Jmp { off } => jump!(off),
                Op::JmpIf { r, off } => {
                    if matches!(reg!(r), Value::Bool(true)) {
                        jump!(off);
                    }
                }
                Op::JmpIfNot { r, off } => {
                    if matches!(reg!(r), Value::Bool(false)) {
                        jump!(off);
                    }
                }
                Op::Loop { off } => {
                    self.core.budget.left -= 1;
                    if self.core.budget.left < 0 {
                        sync!();
                        let span = chunk.spans[pc - 1];
                        return Err(self.step_failure(fiber, span));
                    }
                    jump!(off);
                }
                Op::Step { n } => {
                    self.core.budget.left -= i64::from(n);
                    if self.core.budget.left < 0 {
                        sync!();
                        let span = chunk.spans[pc - 1];
                        return Err(self.step_failure(fiber, span));
                    }
                }
                Op::JmpNotLtI { a, b, off } => jmp_not!(a, b, off, <),
                Op::JmpNotLeI { a, b, off } => jmp_not!(a, b, off, <=),
                Op::JmpNotEqI { a, b, off } => jmp_not!(a, b, off, ==),
                Op::JmpNotNeI { a, b, off } => jmp_not!(a, b, off, !=),
                Op::JmpNotLtIK { a, k, off } => jmp_not_k!(a, k, off, <),
                Op::JmpNotLeIK { a, k, off } => jmp_not_k!(a, k, off, <=),
                Op::JmpNotGtIK { a, k, off } => jmp_not_k!(a, k, off, >),
                Op::JmpNotGeIK { a, k, off } => jmp_not_k!(a, k, off, >=),
                Op::JmpNotEqIK { a, k, off } => jmp_not_k!(a, k, off, ==),
                Op::JmpNotNeIK { a, k, off } => jmp_not_k!(a, k, off, !=),

                Op::ForRangeNext { i, end, off } => {
                    let (x, e) = int2!(i, end);
                    // `x < e` held before the step (the loop only runs then), so no overflow.
                    let next = x + 1;
                    reg!(i) = Value::Int(next);
                    if next < e {
                        self.core.budget.left -= 1;
                        if self.core.budget.left < 0 {
                            sync!();
                            let span = chunk.spans[pc - 1];
                            return Err(self.step_failure(fiber, span));
                        }
                        jump!(off);
                    }
                }
                Op::ForRangeNextIncl { i, end, off } => {
                    let (x, e) = int2!(i, end);
                    // An inclusive range must not step past its end (it may be `i64::MAX`).
                    if x != e {
                        reg!(i) = Value::Int(x + 1);
                        self.core.budget.left -= 1;
                        if self.core.budget.left < 0 {
                            sync!();
                            let span = chunk.spans[pc - 1];
                            return Err(self.step_failure(fiber, span));
                        }
                        jump!(off);
                    }
                }

                Op::Call { d, base: a, site } => {
                    let Site::Call(f) = &chunk.sites[usize::from(site)] else {
                        internal!("a call without its function")
                    };
                    let callee = match self.chunk_of(*f) {
                        Some(Ok(c)) => c,
                        Some(Err(msg)) => internal!(&format!("cannot compile a function: {msg}")),
                        None => internal!("unknown function"),
                    };
                    sync!();
                    let span = chunk.spans[pc - 1];
                    self.push_call(
                        fiber,
                        callee,
                        base + usize::from(a),
                        base + usize::from(d),
                        span,
                    )?;
                    let frame = fiber.frames.last().expect("just pushed");
                    chunk = Rc::clone(&frame.chunk);
                    base = frame.base as usize;
                    pc = 0;
                }
                Op::Return { s } => {
                    let v = std::mem::replace(&mut reg!(s), Value::Unit);
                    Self::check_window(fiber, base, &chunk);
                    match self.pop_frame(fiber, v) {
                        Some(v) => return Ok(v),
                        None => {
                            let frame = fiber.frames.last().expect("a caller");
                            chunk = Rc::clone(&frame.chunk);
                            base = frame.base as usize;
                            pc = frame.pc as usize;
                        }
                    }
                }
                Op::ReturnUnit => {
                    Self::check_window(fiber, base, &chunk);
                    match self.pop_frame(fiber, Value::Unit) {
                        Some(v) => return Ok(v),
                        None => {
                            let frame = fiber.frames.last().expect("a caller");
                            chunk = Rc::clone(&frame.chunk);
                            base = frame.base as usize;
                            pc = frame.pc as usize;
                        }
                    }
                }

                Op::Print { r } => {
                    sync!();
                    let span = chunk.spans[pc - 1];
                    #[cfg(test)]
                    if let Some(hook) = self.print_hook.clone() {
                        hook(self, fiber);
                    }
                    match &reg!(r) {
                        Value::Int(i) => {
                            let _ = writeln!(self.core.out, "{i}");
                        }
                        Value::Str(s) => {
                            let _ = writeln!(self.core.out, "{s}");
                        }
                        _ => {
                            let v = std::mem::replace(&mut reg!(r), Value::Unit);
                            let text = self.show_deep(fiber, &v, span);
                            reg!(r) = v;
                            // A closed pipe is not a Colibri error; output is best effort.
                            let _ = writeln!(self.core.out, "{}", text?);
                        }
                    }
                }
                Op::Show { d, r } => {
                    sync!();
                    let span = chunk.spans[pc - 1];
                    let v = std::mem::replace(&mut reg!(r), Value::Unit);
                    let text = self.show_deep(fiber, &v, span);
                    reg!(r) = v;
                    reg!(d) = Value::Str(Rc::from(text?));
                }
                Op::Format { d, r, site } => {
                    let Site::Format(spec) = &chunk.sites[usize::from(site)] else {
                        internal!("a format without its spec")
                    };
                    let text = colibri_core::format::format_value(&reg!(r), spec);
                    reg!(d) = Value::Str(Rc::from(text));
                }
                Op::StrConcat { d, base: a, n } => {
                    sync!();
                    let span = chunk.spans[pc - 1];
                    let mut out = String::new();
                    for i in 0..n {
                        let at = a + i;
                        match &reg!(at) {
                            Value::Str(s) => out.push_str(s),
                            Value::Int(x) => {
                                use std::fmt::Write as _;
                                let _ = write!(out, "{x}");
                            }
                            _ => {
                                let v = std::mem::replace(&mut reg!(at), Value::Unit);
                                let text = self.show_deep(fiber, &v, span);
                                reg!(at) = v;
                                out.push_str(&text?);
                            }
                        }
                    }
                    reg!(d) = Value::Str(Rc::from(out));
                }

                Op::Intrinsic { d, base: a, site } => {
                    sync!();
                    let span = chunk.spans[pc - 1];
                    let Site::Intrinsic { which, argc } = &chunk.sites[usize::from(site)] else {
                        internal!("an intrinsic without its site")
                    };
                    let (from, to) = (
                        base + usize::from(a),
                        base + usize::from(a) + usize::from(*argc),
                    );
                    let result = self.run_intrinsic(fiber, which, from..to, span)?;
                    reg!(d) = result;
                }
                Op::Coerce { d, s, site } => {
                    let Site::F32ToFloat = &chunk.sites[usize::from(site)] else {
                        internal!("a coercion without its kind")
                    };
                    let Value::F32(x) = &reg!(s) else {
                        internal!("ill-typed `f32` coercion")
                    };
                    reg!(d) = Value::Float(f64::from(*x));
                }

                Op::Site { .. } => internal!("an operand word was executed"),
                Op::Unsupported { site } => {
                    let Site::Unsupported(msg) = &chunk.sites[usize::from(site)] else {
                        internal!("an unsupported node without its message")
                    };
                    fault!(msg.clone());
                }
            }
        }
    }

    fn uninit_message(&self, g: u32) -> String {
        let name = self
            .uninit
            .get(g as usize)
            .and_then(|n| n.clone())
            .unwrap_or_else(|| Rc::from("?"));
        if self.released {
            format!(
                "`{name}` is used before it is initialised (module values do not exist while no test runs, for instance in an `on_test_start` hook)"
            )
        } else {
            format!(
                "`{name}` is used before it is initialised (the script has not reached its line)"
            )
        }
    }

    /// Pushes a frame for `callee` whose window starts at the absolute register `new_base`; the
    /// result goes to the absolute register `ret`. Checks the call depth and charges a step.
    /// `span` is the call, for a panic.
    #[inline]
    fn push_call(
        &mut self,
        fiber: &mut VmFiber,
        callee: Rc<Chunk>,
        new_base: usize,
        ret: usize,
        span: colibri_syntax::Span,
    ) -> Result<(), Ctl> {
        if fiber.frames.len() >= self.max_depth {
            return Err(self.overflow(fiber, span));
        }
        self.core.budget.left -= 1;
        if self.core.budget.left < 0 {
            return Err(self.step_failure(fiber, span));
        }
        let need = new_base + callee.nregs as usize;
        if fiber.regs.len() < need {
            fiber.regs.resize(need, Value::Unit);
        }
        fiber.frames.push(Frame {
            chunk: callee,
            pc: 0,
            base: new_base as u32,
            ret: ret as u32,
            kind: FrameKind::Colibri,
            noted: None,
            temps_mark: fiber.temps.len() as u32,
        });
        Ok(())
    }

    #[cold]
    #[inline(never)]
    pub(crate) fn overflow(&mut self, fiber: &mut VmFiber, span: colibri_syntax::Span) -> Ctl {
        let message = format!("stack overflow: more than {} nested calls", self.max_depth);
        self.panic(fiber, span, message)
    }

    /// Pops the top frame, delivering `v`. `Some(v)` when the frame was an entry from Rust:
    /// the loop ends with that value.
    #[inline]
    fn pop_frame(&mut self, fiber: &mut VmFiber, v: Value) -> Option<Value> {
        let frame = fiber.frames.pop().expect("a frame to pop");
        debug_assert!(
            fiber.temps.len() == frame.temps_mark as usize,
            "held temporaries outlive their frame"
        );
        match frame.kind {
            FrameKind::NativeEntry => Some(v),
            FrameKind::Colibri => {
                fiber.regs[frame.ret as usize] = v;
                None
            }
        }
    }

    /// The intrinsics that are not instructions of their own.
    #[inline(never)]
    fn run_intrinsic(
        &mut self,
        fiber: &mut VmFiber,
        which: &colibri_check::hir::Intrinsic,
        args: std::ops::Range<usize>,
        span: colibri_syntax::Span,
    ) -> Eval {
        use colibri_check::hir::Intrinsic;
        match which {
            Intrinsic::Panic => {
                let arg = std::mem::replace(&mut fiber.regs[args.start], Value::Unit);
                let result = self.error_intrinsic(fiber, which, &arg, span);
                self.release(arg);
                result
            }
            Intrinsic::Assert | Intrinsic::AssertEq => {
                let values: Vec<Value> = args
                    .map(|i| std::mem::replace(&mut fiber.regs[i], Value::Unit))
                    .collect();
                self.assertion(fiber, which, values, span)
            }
            Intrinsic::Convert { from, to } => num::convert(*from, *to, &fiber.regs[args.start])
                .map_err(|msg| self.panic(fiber, span, msg)),
            Intrinsic::NumMethod(m, n) => {
                num::method(*m, *n, &fiber.regs[args]).map_err(|msg| self.panic(fiber, span, msg))
            }
            _ => Err(self.internal(fiber, span, "an intrinsic the compiler does not emit")),
        }
    }
}
