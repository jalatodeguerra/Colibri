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
use colibri_core::value::{CellObj, SmallVec, Value, unwrap_cell};

use crate::bytecode::{Chunk, Op, Site};
use crate::fiber::{Frame, FrameKind, VmFiber, is_scalar_value};
use crate::machine::Machine;
use crate::unchecked;

/// How `exec` ends when it does not fail.
pub(crate) enum Exit {
    /// The entry frame returned this value.
    Return(Value),
    /// A coroutine paused: its frames stay as they are, each with its pc saved, for
    /// `resume_stack` to run on.
    Suspend,
}

impl Machine {
    /// Runs the entry frame on top of `fiber` to its end. On a panic every frame down to and
    /// including the entry frame has run its landing pads and been popped (`catch`).
    pub(crate) fn run(&mut self, fiber: &mut VmFiber) -> Eval {
        match self.exec(fiber)? {
            Exit::Return(v) => Ok(v),
            // `Machine::suspend` pauses only where the coroutine's own entry is the one that
            // runs, so a nested loop never sees it.
            Exit::Suspend => {
                let span = Self::position(fiber).unwrap_or(colibri_syntax::Span::new(
                    colibri_syntax::FileId(0),
                    0,
                    0,
                ));
                Err(self.internal(fiber, span, "a coroutine paused inside a native call"))
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

    /// The dispatch loop. Every way out goes through `throw!` (an error) or `return Ok(Exit)`,
    /// so landing pads and suspension have one place to hook into.
    pub(crate) fn exec(&mut self, fiber: &mut VmFiber) -> Result<Exit, Ctl> {
        let top = fiber.frames.last().expect("exec needs a frame");
        let mut chunk = unchecked::Running::new(Rc::clone(&top.chunk));
        let mut base = top.base as usize;
        let mut pc = top.pc as usize;

        // The registers of the running frame, read and written without a bounds check. SAFETY
        // (for every `rd!` and `wr!`, which only the arms of the instructions that
        // `Op::unchecked_regs_end` lists may use): the frame's window `base .. base + nregs`
        // lies inside `fiber.regs` (`push_call`, `op_call_hidden` and `enter_with` grow the file
        // before they push a frame, and nothing shrinks it while frames exist), and the compiler's
        // `Chunk::verify_registers` refused any chunk whose instruction names a register of that
        // list at or beyond `nregs`. Debug builds check the access, and that the index is below `nregs`.
        macro_rules! rd {
            ($i:expr) => {{
                debug_assert!(
                    usize::from($i) < usize::from(chunk.nregs),
                    "r{} of {} registers",
                    $i,
                    chunk.nregs
                );
                #[allow(unsafe_code)]
                let r: &Value = unsafe { unchecked::get(&fiber.regs, base + usize::from($i)) };
                r
            }};
        }
        macro_rules! wr {
            ($i:expr) => {{
                debug_assert!(
                    usize::from($i) < usize::from(chunk.nregs),
                    "r{} of {} registers",
                    $i,
                    chunk.nregs
                );
                #[allow(unsafe_code)]
                let r: &mut Value =
                    unsafe { unchecked::get_mut(&mut fiber.regs, base + usize::from($i)) };
                r
            }};
        }
        // Writes a result into a register the compiler says is empty (Unit invariant: it holds
        // a scalar, which owns nothing). Debug builds assert that, so a write that would lose a
        // value fails loudly; release builds forget the scalar, which needs no drop glue.
        macro_rules! put {
            ($i:expr, $v:expr) => {{
                let v = $v;
                let slot = wr!($i);
                debug_assert!(
                    is_scalar_value(slot),
                    "a write into r{} of `{}` would lose {:?}",
                    $i,
                    chunk.body.name,
                    slot
                );
                std::mem::forget(std::mem::replace(slot, v));
            }};
        }
        macro_rules! sync {
            () => {
                if let Some(f) = fiber.frames.last_mut() {
                    f.pc = pc as u32;
                }
            };
        }
        // The single way to fail: the failure goes to the landing pad of the instruction, or
        // out of the frames that have none; it leaves `exec` only when the entry frame is gone.
        macro_rules! throw {
            ($ctl:expr) => {{
                let ctl = $ctl;
                match self.catch(fiber, ctl, false) {
                    Ok(()) => {
                        reload!();
                        continue;
                    }
                    Err(ctl) => return Err(ctl),
                }
            }};
        }
        // Unwraps a fallible step, throwing its error.
        macro_rules! tri {
            ($e:expr) => {
                match $e {
                    Ok(v) => v,
                    Err(ctl) => throw!(ctl),
                }
            };
        }
        // A panic at the current instruction.
        macro_rules! fault {
            ($msg:expr) => {{
                sync!();
                let span = chunk.spans[pc - 1];
                throw!(self.panic(fiber, span, $msg))
            }};
        }
        macro_rules! internal {
            ($what:expr) => {{
                sync!();
                let span = chunk.spans[pc - 1];
                throw!(self.internal(fiber, span, $what))
            }};
        }
        // Charges budget steps; throws when it runs out.
        macro_rules! charge {
            ($n:expr) => {{
                self.core.budget.left -= i64::from($n);
                if self.core.budget.left < 0 {
                    sync!();
                    let span = chunk.spans[pc - 1];
                    throw!(self.step_failure(fiber, span));
                }
            }};
        }
        macro_rules! int2 {
            ($a:expr, $b:expr) => {
                match (rd!($a), rd!($b)) {
                    (Value::Int(x), Value::Int(y)) => (*x, *y),
                    _ => internal!("ill-typed integer operation"),
                }
            };
        }
        macro_rules! float2 {
            ($a:expr, $b:expr) => {
                match (rd!($a), rd!($b)) {
                    (Value::Float(x), Value::Float(y)) => (*x, *y),
                    _ => internal!("ill-typed float operation"),
                }
            };
        }
        macro_rules! int_op {
            ($d:expr, $a:expr, $b:expr, $f:expr, $msg:expr) => {{
                let (x, y) = int2!($a, $b);
                match $f(x, y) {
                    Some(v) => put!($d, Value::Int(v)),
                    None => fault!($msg),
                }
            }};
        }
        // `l.idx op= b` for a float field of a struct this register holds alone; a struct that is
        // shared (it is copied first), or anything unexpected, is `op_field_update`'s.
        macro_rules! field_f {
            ($op:expr, $idx:expr, $l:expr, $b:expr, $sym:tt) => {{
                let y = match rd!($b) {
                    Value::Float(y) => *y,
                    _ => internal!("ill-typed field update"),
                };
                if let Value::Struct(st) = wr!($l)
                    && let Some(sv) = Rc::get_mut(st)
                    && let Some(Value::Float(x)) = sv.fields.get_mut(usize::from($idx))
                {
                    *x $sym y;
                } else {
                    sync!();
                    tri!(self.op_field_update(fiber, &chunk, base, pc, $op));
                }
            }};
        }
        macro_rules! field_i {
            ($op:expr, $idx:expr, $l:expr, $b:expr, $f:ident) => {{
                let y = match rd!($b) {
                    Value::Int(y) => *y,
                    _ => internal!("ill-typed field update"),
                };
                if let Value::Struct(st) = wr!($l)
                    && let Some(sv) = Rc::get_mut(st)
                    && let Some(Value::Int(x)) = sv.fields.get_mut(usize::from($idx))
                {
                    match x.$f(y) {
                        Some(v) => *x = v,
                        None => fault!("integer overflow"),
                    }
                } else {
                    sync!();
                    tri!(self.op_field_update(fiber, &chunk, base, pc, $op));
                }
            }};
        }
        macro_rules! jump {
            ($off:expr) => {
                pc = (pc as isize + $off as isize) as usize
            };
        }
        // A back edge: jumps by `off` and charges the budget step of the edge together with the
        // `Step` that opens the target's run, which it then skips.
        macro_rules! back_edge {
            ($off:expr) => {{
                let mut next = (pc as isize + $off as isize) as usize;
                let mut n = 1;
                if let Op::Step { n: run } = chunk.ops()[next] {
                    n += run;
                    next += 1;
                }
                // Charged before the jump, so that a failure names the edge, not its target.
                charge!(n);
                pc = next;
            }};
        }
        // Compare an integer register with a constant and jump unless it holds.
        macro_rules! jmp_not_k {
            ($a:expr, $k:expr, $off:expr, $cmp:tt) => {{
                let Value::Int(x) = rd!($a) else {
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
                put!($d, Value::Bool(x $cmp y));
            }};
        }
        macro_rules! cmp_f {
            ($d:expr, $a:expr, $b:expr, $cmp:tt) => {{
                let (x, y) = float2!($a, $b);
                put!($d, Value::Bool(x $cmp y));
            }};
        }
        macro_rules! arith_f {
            ($d:expr, $a:expr, $b:expr, $op:tt) => {{
                let (x, y) = float2!($a, $b);
                put!($d, Value::Float(x $op y));
            }};
        }
        // Reloads the loop state from the top frame (after a push or a pop).
        macro_rules! reload {
            () => {{
                let frame = fiber.frames.last().expect("a frame");
                chunk.set(Rc::clone(&frame.chunk));
                base = frame.base as usize;
                pc = frame.pc as usize;
            }};
        }

        loop {
            let op = chunk.ops()[pc];
            pc += 1;
            match op {
                Op::LoadUnit { d } => put!(d, Value::Unit),
                Op::LoadBool { d, b } => put!(d, Value::Bool(b)),
                Op::LoadInt { d, i } => put!(d, Value::Int(i64::from(i))),
                Op::LoadK { d, k } => put!(d, chunk.consts[k as usize].dup()),
                Op::Copy { d, s } => {
                    let v = rd!(s).dup();
                    put!(d, v);
                }
                Op::Move { d, s } => {
                    let v = std::mem::replace(wr!(s), Value::Unit);
                    put!(d, v);
                }
                Op::Drop { r } => {
                    let v = std::mem::replace(wr!(r), Value::Unit);
                    if !is_scalar_value(&v) {
                        sync!();
                        self.release(v);
                    }
                }

                Op::DropInert { r } => {
                    // A string or bytes value: the plain drop is all it needs.
                    *wr!(r) = Value::Unit;
                }
                Op::DropN { r, n } => {
                    sync!();
                    for i in (0..n).rev() {
                        let v = std::mem::replace(wr!(r + i), Value::Unit);
                        if !is_scalar_value(&v) {
                            self.release(v);
                        }
                    }
                }

                Op::StoreLocal { l, s } => {
                    // An explicit replace: the old value is released.
                    let v = std::mem::replace(wr!(s), Value::Unit);
                    let old = std::mem::replace(wr!(l), v);
                    if !is_scalar_value(&old) {
                        sync!();
                        self.release(old);
                    }
                }
                Op::NewCell { l, s } => {
                    let v = std::mem::replace(wr!(s), Value::Unit);
                    put!(l, Value::Cell(CellObj::new(v)));
                }
                Op::CellGet { d, l } => {
                    let Value::Cell(c) = rd!(l) else {
                        internal!("a captured variable is not in a cell")
                    };
                    let v = c.get();
                    put!(d, v);
                }
                Op::CellSet { l, s } => {
                    let v = std::mem::replace(wr!(s), Value::Unit);
                    let Value::Cell(c) = rd!(l) else {
                        internal!("a captured variable is not in a cell")
                    };
                    let old = c.set(v);
                    if !is_scalar_value(&old) {
                        sync!();
                        self.release(old);
                    }
                }
                Op::UnwrapCell { r } => {
                    let v = std::mem::replace(wr!(r), Value::Unit);
                    put!(r, unwrap_cell(v));
                }

                Op::GetGlobal { d, g } => put!(d, self.globals[g as usize].dup()),
                Op::GetGlobalChecked { .. }
                | Op::CheckGlobal { .. }
                | Op::SetGlobal { .. }
                | Op::InitGlobal { .. } => {
                    sync!();
                    tri!(self.op_global(fiber, &chunk, base, pc, op));
                }
                Op::CallInit { .. } => {
                    sync!();
                    tri!(self.op_call_init(fiber, &chunk, base, pc, op));
                    reload!();
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
                        Some(v) => put!(d, Value::Int(v)),
                        None => fault!("integer overflow"),
                    }
                }
                Op::RemI { d, a, b } => {
                    let (x, y) = int2!(a, b);
                    if y == 0 {
                        fault!("modulo by zero");
                    }
                    match x.checked_rem(y) {
                        Some(v) => put!(d, Value::Int(v)),
                        None => fault!("integer overflow"),
                    }
                }
                Op::AddIK { d, a, i } => {
                    let Value::Int(x) = rd!(a) else {
                        internal!("ill-typed integer operation")
                    };
                    match x.checked_add(i64::from(i)) {
                        Some(v) => put!(d, Value::Int(v)),
                        None => fault!("integer overflow"),
                    }
                }
                Op::MulIK { d, a, i } => {
                    let Value::Int(x) = rd!(a) else {
                        internal!("ill-typed integer operation")
                    };
                    match x.checked_mul(i64::from(i)) {
                        Some(v) => put!(d, Value::Int(v)),
                        None => fault!("integer overflow"),
                    }
                }
                // The divisor is neither 0 nor -1 (the compiler's condition), so these cannot fail.
                Op::DivIK { d, a, i } => {
                    let Value::Int(x) = rd!(a) else {
                        internal!("ill-typed integer operation")
                    };
                    let v = x.wrapping_div(i64::from(i));
                    put!(d, Value::Int(v));
                }
                Op::RemIK { d, a, i } => {
                    let Value::Int(x) = rd!(a) else {
                        internal!("ill-typed integer operation")
                    };
                    let v = x.wrapping_rem(i64::from(i));
                    put!(d, Value::Int(v));
                }
                Op::EqIK { d, a, i } => {
                    let Value::Int(x) = rd!(a) else {
                        internal!("ill-typed integer comparison")
                    };
                    let v = *x == i64::from(i);
                    put!(d, Value::Bool(v));
                }
                Op::NeIK { d, a, i } => {
                    let Value::Int(x) = rd!(a) else {
                        internal!("ill-typed integer comparison")
                    };
                    let v = *x != i64::from(i);
                    put!(d, Value::Bool(v));
                }
                Op::NegI { d, a } => {
                    let Value::Int(x) = rd!(a) else {
                        internal!("ill-typed negation")
                    };
                    match x.checked_neg() {
                        Some(v) => put!(d, Value::Int(v)),
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
                    let Value::Float(x) = rd!(a) else {
                        internal!("ill-typed negation")
                    };
                    put!(d, Value::Float(-*x));
                }
                Op::LtF { d, a, b } => cmp_f!(d, a, b, <),
                Op::LeF { d, a, b } => cmp_f!(d, a, b, <=),
                Op::EqF { d, a, b } => cmp_f!(d, a, b, ==),
                Op::NeF { d, a, b } => cmp_f!(d, a, b, !=),

                Op::Not { d, a } => {
                    let Value::Bool(x) = rd!(a) else {
                        internal!("ill-typed `not`")
                    };
                    put!(d, Value::Bool(!*x));
                }
                Op::Binary { .. } => {
                    pc += 1; // the operand word
                    sync!();
                    tri!(self.op_binary(fiber, &chunk, base, pc, op));
                }
                Op::Unary { .. } => {
                    sync!();
                    tri!(self.op_unary(fiber, &chunk, base, pc, op));
                }

                Op::Jmp { off } => jump!(off),
                Op::JmpIf { r, off } => match rd!(r) {
                    Value::Bool(true) => jump!(off),
                    Value::Bool(false) => {}
                    _ => internal!("a condition that is not a bool"),
                },
                Op::JmpIfNot { r, off } => match rd!(r) {
                    Value::Bool(false) => jump!(off),
                    Value::Bool(true) => {}
                    _ => internal!("a condition that is not a bool"),
                },
                Op::Loop { off } => back_edge!(off),
                Op::Step { n } => charge!(n),
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
                    put!(i, Value::Int(next));
                    if next < e {
                        back_edge!(off);
                    }
                }
                Op::ForRangeNextIncl { i, end, off } => {
                    let (x, e) = int2!(i, end);
                    // An inclusive range must not step past its end (it may be `i64::MAX`).
                    if x != e {
                        put!(i, Value::Int(x + 1));
                        back_edge!(off);
                    }
                }

                Op::Call { d, base: a, site } => {
                    let Site::Call(f) = &chunk.sites[usize::from(site)] else {
                        internal!("a call without its function")
                    };
                    let callee = match self.fns.get(f.0 as usize).and_then(|s| s.chunk.get()) {
                        Some(c) => Rc::clone(c),
                        None => {
                            sync!();
                            tri!(self.callee_slow(fiber, &chunk, pc, *f))
                        }
                    };
                    sync!();
                    tri!(self.push_call(
                        fiber,
                        callee,
                        base + usize::from(a),
                        base + usize::from(d),
                        &chunk,
                        pc,
                    ));
                    reload!();
                }
                Op::CallHidden { d, base: a, site } => {
                    sync!();
                    tri!(self.op_call_hidden(fiber, &chunk, base, pc, [d, a, site]));
                    reload!();
                }
                Op::CallHost { .. } => {
                    sync!();
                    if tri!(self.op_call_host(fiber, &chunk, base, pc, op)) {
                        // The coroutine waits inside the host function.
                        return Ok(Exit::Suspend);
                    }
                }
                Op::CallValue { d, base: a } => {
                    sync!();
                    tri!(self.op_call_value(fiber, &chunk, base, pc, d, a));
                    reload!();
                }
                Op::CallValueIn { d, f, base: a } => {
                    sync!();
                    tri!(self.op_call_value_in(fiber, &chunk, base, pc, [d, f, a]));
                    reload!();
                }
                Op::CallBound { d, base: a, site } | Op::CallDyn { d, base: a, site } => {
                    sync!();
                    tri!(self.op_call_iface(fiber, &chunk, base, pc, [d, a, site]));
                    reload!();
                }
                Op::CallIfaceInout { d, base: a, site } => {
                    sync!();
                    tri!(self.op_call_iface_inout(fiber, &chunk, base, pc, [d, a, site]));
                    reload!();
                }
                Op::Rewrap { shell, recv } => self.op_rewrap(fiber, base, shell, recv),
                Op::CallOperator { d, base: a, site } => {
                    sync!();
                    tri!(self.op_call_operator(fiber, &chunk, base, pc, [d, a, site]));
                    reload!();
                }
                Op::MakeClosure { d, site } => {
                    sync!();
                    tri!(self.op_make_closure(fiber, &chunk, base, pc, d, site));
                }
                Op::IterIsUser { a } => {
                    if self.iter_is_user(rd!(a)) {
                        pc += 1;
                    }
                }
                Op::IterNextCall { a, w } => {
                    sync!();
                    tri!(self.op_iter_next_call(fiber, &chunk, base, pc, a, w));
                    reload!();
                }
                Op::IterNextEnd { d, a, w } => {
                    sync!();
                    if tri!(self.op_iter_next_end(fiber, &chunk, base, pc, [d, a, w])) {
                        pc += 1;
                    }
                }
                Op::Return { s } => {
                    let v = std::mem::replace(wr!(s), Value::Unit);
                    Self::check_window(fiber, base, &chunk);
                    match self.pop_frame(fiber, v) {
                        Some(v) => return Ok(Exit::Return(v)),
                        None => reload!(),
                    }
                }
                Op::ReturnUnit => {
                    Self::check_window(fiber, base, &chunk);
                    match self.pop_frame(fiber, Value::Unit) {
                        Some(v) => return Ok(Exit::Return(v)),
                        None => reload!(),
                    }
                }

                Op::Print { r } => {
                    sync!();
                    tri!(self.op_print(fiber, &chunk, base, pc, r));
                }
                Op::Show { d, r } => {
                    sync!();
                    tri!(self.op_show(fiber, &chunk, base, pc, d, r));
                }
                Op::Format { .. } => {
                    sync!();
                    tri!(self.op_format(fiber, &chunk, base, pc, op));
                }
                Op::StrConcat { .. } => {
                    sync!();
                    tri!(self.op_str_concat(fiber, &chunk, base, pc, op));
                }
                Op::Intrinsic { .. } => {
                    sync!();
                    tri!(self.op_intrinsic(fiber, &chunk, base, pc, op));
                }
                Op::Builtin { .. } => {
                    sync!();
                    tri!(self.op_builtin(fiber, &chunk, base, pc, op));
                }
                Op::ListPush { l, v } => {
                    let item = rd!(v).dup();
                    let Value::List(list) = rd!(l) else {
                        self.release(item);
                        internal!("an ill-typed `push`")
                    };
                    if let Err(rejected) = list.push(item) {
                        sync!();
                        self.release(rejected.args);
                        let span = chunk.spans[pc - 1];
                        throw!(self.violation(fiber, span, rejected.violation));
                    }
                }
                Op::ListLen { d, l } => {
                    let Value::List(list) = rd!(l) else {
                        internal!("an ill-typed `len`")
                    };
                    let n = list.len() as i64;
                    put!(d, Value::Int(n));
                }
                Op::StrLen { d, s } => {
                    let Value::Str(text) = rd!(s) else {
                        internal!("an ill-typed `len`")
                    };
                    let n = text.len() as i64;
                    put!(d, Value::Int(n));
                }
                Op::Coerce { d, s, site } => match &chunk.sites[usize::from(site)] {
                    Site::F32ToFloat => {
                        let Value::F32(x) = rd!(s) else {
                            internal!("ill-typed `f32` coercion")
                        };
                        put!(d, Value::Float(f64::from(*x)));
                    }
                    Site::Coerce(how) => {
                        sync!();
                        let v = rd!(s).clone();
                        let out = self.coerce(v, how);
                        put!(d, out);
                    }
                    _ => internal!("a coercion without its kind"),
                },

                Op::GetField { d, s, idx } => {
                    let field = match rd!(s) {
                        Value::Struct(st) => st.fields.get(usize::from(idx)).map(Value::dup),
                        _ => None,
                    };
                    match field {
                        Some(v) => put!(d, v),
                        None => internal!("ill-typed projection"),
                    }
                }
                Op::FieldAddF { idx, l, b } => field_f!(op, idx, l, b, +=),
                Op::FieldSubF { idx, l, b } => field_f!(op, idx, l, b, -=),
                Op::FieldMulF { idx, l, b } => field_f!(op, idx, l, b, *=),
                Op::FieldDivF { idx, l, b } => field_f!(op, idx, l, b, /=),
                Op::FieldAddI { idx, l, b } => field_i!(op, idx, l, b, checked_add),
                Op::FieldSubI { idx, l, b } => field_i!(op, idx, l, b, checked_sub),
                Op::FieldMulI { idx, l, b } => field_i!(op, idx, l, b, checked_mul),
                Op::SetField { l, s, idx } => {
                    // The common write: a scalar over a scalar field of a struct nothing else
                    // holds, with nothing to release and nothing to copy.
                    if is_scalar_value(rd!(s))
                        && let Value::Struct(st) = rd!(l)
                        && Rc::strong_count(st) == 1
                        && Rc::weak_count(st) == 0
                    {
                        let v = std::mem::replace(wr!(s), Value::Unit);
                        if let Value::Struct(st) = wr!(l)
                            && let Some(sv) = Rc::get_mut(st)
                            && let Some(slot) = sv.fields.get_mut(usize::from(idx))
                            && is_scalar_value(slot)
                        {
                            std::mem::forget(std::mem::replace(slot, v));
                            continue;
                        }
                        put!(s, v);
                    }
                    sync!();
                    tri!(self.op_set_field(fiber, &chunk, base, pc, [l, s, idx]));
                }
                Op::NewStruct { d, site } => {
                    sync!();
                    tri!(self.op_new_struct(fiber, &chunk, base, pc, [d, site]));
                }
                Op::NewObject { d, site } => {
                    sync!();
                    tri!(self.op_new_object(fiber, &chunk, base, pc, [d, site]));
                }
                Op::NewVariant { d, site } => {
                    sync!();
                    tri!(self.op_new_variant(fiber, &chunk, base, pc, [d, site]));
                }
                Op::ObjGet { d, o, idx } => {
                    let got = match rd!(o) {
                        Value::Obj(obj) => obj.field(usize::from(idx)),
                        _ => None,
                    };
                    match got {
                        Some(v) => put!(d, v),
                        None => internal!("a field of something that is not an object"),
                    }
                }
                Op::NewTuple { .. }
                | Op::NewList { .. }
                | Op::NewMap { .. }
                | Op::NewRange { .. }
                | Op::GetPath { .. }
                | Op::FromWeak { .. } => {
                    sync!();
                    tri!(self.op_new(fiber, &chunk, base, pc, op));
                }
                Op::ToWeak { d, s } => {
                    sync!();
                    tri!(self.op_to_weak(fiber, &chunk, base, pc, d, s));
                }
                Op::MapInsert { m, k, v } => {
                    sync!();
                    tri!(self.op_map_insert(fiber, &chunk, base, pc, [m, k, v]));
                }

                Op::PlaceGet { .. }
                | Op::PlaceTake { .. }
                | Op::PlaceSet { .. }
                | Op::PlaceHold { .. } => {
                    pc += 1; // the site word
                    sync!();
                    tri!(self.op_place(fiber, &chunk, base, pc, op));
                }
                Op::ListGetAt { .. } | Op::ListSetAt { .. } => {
                    sync!();
                    tri!(self.op_elem(fiber, &chunk, base, pc, op));
                }
                Op::MapGetAt { .. } | Op::MapSetAt { .. } => {
                    sync!();
                    tri!(self.op_map_at(fiber, &chunk, base, pc, op));
                }
                Op::Unhold => {
                    let held = fiber.holds.pop();
                    debug_assert!(held.is_some(), "a hold released that was never taken");
                }
                Op::SetPath { l, s, site } => {
                    sync!();
                    tri!(self.op_set_path(fiber, &chunk, base, pc, [l, s, site]));
                }
                Op::CanonKey { d, m, k } => {
                    sync!();
                    tri!(self.op_canon_key(fiber, &chunk, base, pc, [d, m, k]));
                }
                Op::HoldIter { .. } | Op::HoldListPlace { .. } | Op::HoldListLend { .. } => {
                    sync!();
                    tri!(self.op_hold(fiber, &chunk, base, pc, op));
                }
                Op::ForListNext { d, a }
                | Op::ForListNextVar { d, a }
                | Op::ForListNextLend { d, a } => {
                    let var = !matches!(op, Op::ForListNext { .. });
                    let lend = matches!(op, Op::ForListNextLend { .. });
                    // The common trip, with no call out: an item is there, the budget lasts, and
                    // the item is bound. Everything else (the end of the list, a failing step, a
                    // source that is not a list) is `op_for_list_next`'s, which starts over.
                    let fast = 'trip: {
                        let Value::List(list) = rd!(a) else {
                            break 'trip false;
                        };
                        let Value::Int(i) = *rd!(a + 1) else {
                            break 'trip false;
                        };
                        if !chunk.hidden && self.core.budget.left <= 0 {
                            break 'trip false;
                        }
                        // A lent slot: the item moves out and nothing keeps a second reference.
                        let lent = lend && matches!(rd!(a + 3), Value::Bool(true));
                        let Some(item) = (if lent {
                            list.lend_at(i as usize)
                        } else {
                            list.at(i as usize)
                        }) else {
                            break 'trip false;
                        };
                        if !chunk.hidden {
                            self.core.budget.left -= 1;
                        }
                        if let Value::Int(n) = wr!(a + 1) {
                            *n += 1;
                        }
                        if var && !lent {
                            put!(a + 2, item.dup());
                        }
                        put!(d, item);
                        true
                    };
                    if fast {
                        // An item skips the jump that ends the loop.
                        pc += 1;
                    } else {
                        sync!();
                        if tri!(self.op_for_list_next(fiber, &chunk, base, pc, [d, a], var, lend)) {
                            pc += 1;
                        }
                    }
                }
                Op::ForMapNext { .. } | Op::ForSetNext { .. } | Op::ForIterNext { .. } => {
                    sync!();
                    // An item skips the jump that ends the loop.
                    if tri!(self.op_for_next(fiber, &chunk, base, pc, op)) {
                        pc += 1;
                    }
                }
                Op::ForWriteBack { l, a } => {
                    sync!();
                    self.op_for_write_back(fiber, base, l, a);
                }
                Op::ForWriteBackLend { l, a } => {
                    if let Value::Bool(true) = rd!(a + 3)
                        && let Value::Int(next) = *rd!(a + 1)
                        && let Value::List(list) = rd!(a)
                    {
                        // The binding goes back into the slot it was lent from. The binding is a
                        // plain slot (the compiler checked), and the slot holds `()`.
                        let list = Rc::clone(list);
                        let v = std::mem::replace(wr!(l), Value::Unit);
                        if let Err(lost) = list.unlend_at((next - 1) as usize, v) {
                            sync!();
                            self.release(lost);
                        }
                    } else {
                        sync!();
                        self.op_for_write_back(fiber, base, l, a);
                    }
                }

                // Only the variant is compared: the checker's types make the enum and the arity
                // of the payload the pattern's (the tree-walker checks them too, redundantly).
                Op::TestVariant { r, v } => {
                    let Value::Enum(e) = rd!(r) else {
                        internal!("a variant pattern on a value that is not an enum")
                    };
                    if e.variant == v {
                        pc += 1;
                    }
                }
                Op::TestInt { r, k } => {
                    let hit = match rd!(r) {
                        Value::Int(x) => *x == i64::from(k),
                        Value::U64(x) => u64::try_from(k) == Ok(*x),
                        _ => internal!("an integer pattern on another value"),
                    };
                    if hit {
                        pc += 1;
                    }
                }
                Op::TestLit { .. } | Op::TestRange { .. } | Op::TestConst { .. } => {
                    sync!();
                    if tri!(self.op_test(fiber, &chunk, base, pc, op)) {
                        pc += 1;
                    }
                }
                Op::Unpack { d, s, i } => {
                    let i = usize::from(i);
                    let part = match rd!(s) {
                        Value::Enum(e) => e.payload.get(i).cloned(),
                        Value::Tuple(items) => items.get(i).cloned(),
                        Value::Struct(st) => st.fields.get(i).cloned(),
                        Value::Obj(o) => o.field(i),
                        _ => None,
                    };
                    match part {
                        Some(v) => put!(d, v),
                        None => internal!("a pattern on a value of another shape"),
                    }
                }

                Op::SkipIfDead { off } => {
                    if self.core.budget_is_dead() {
                        jump!(off);
                    }
                }
                Op::DropCheck { r } => {
                    let v = std::mem::replace(wr!(r), Value::Unit);
                    if !is_scalar_value(&v) {
                        sync!();
                        self.release(v);
                    }
                    if !self.core.hook_panics.is_empty() {
                        sync!();
                        if let Err(ctl) = self.fold_hook_panics(Ok(Value::Unit)) {
                            throw!(ctl);
                        }
                    }
                }
                Op::CheckHooks => {
                    if !self.core.hook_panics.is_empty() {
                        sync!();
                        if let Err(ctl) = self.fold_hook_panics(Ok(Value::Unit)) {
                            throw!(ctl);
                        }
                    }
                }
                Op::FoldHooks => {
                    if !self.core.hook_panics.is_empty() {
                        sync!();
                        tri!(self.op_fold_hooks(fiber, &chunk, pc));
                    }
                }
                Op::Resume => {
                    sync!();
                    let idx = fiber.frames.len() - 1;
                    let Some(ctl) = fiber.take_unwinding(idx) else {
                        internal!("a landing pad ended with nothing to resume")
                    };
                    match self.catch(fiber, *ctl, true) {
                        Ok(()) => {
                            reload!();
                            continue;
                        }
                        Err(ctl) => return Err(ctl),
                    }
                }

                Op::SuspendReady => {
                    sync!();
                    tri!(self.op_suspend_ready(fiber, &chunk, pc));
                    return Ok(Exit::Suspend);
                }
                Op::Go { d, base: a, site } => {
                    sync!();
                    tri!(self.op_go(fiber, &chunk, base, pc, [d, a, site]));
                }
                Op::Pause { a, kind } => {
                    sync!();
                    if tri!(self.op_pause(fiber, &chunk, base, pc, a, kind)) {
                        return Ok(Exit::Suspend);
                    }
                }
                Op::Join { d, a, kind } => {
                    sync!();
                    if tri!(self.op_join(fiber, &chunk, base, pc, [d, a], kind)) {
                        return Ok(Exit::Suspend);
                    }
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

    /// Folds the panics of `Drop` hooks into the panic this frame's pad is running for.
    #[cold]
    #[inline(never)]
    fn op_fold_hooks(&mut self, fiber: &mut VmFiber, chunk: &Chunk, pc: usize) -> Result<(), Ctl> {
        let idx = fiber.frames.len() - 1;
        let Some(inflight) = fiber.take_unwinding(idx) else {
            let span = chunk.spans[pc - 1];
            return Err(self.internal(fiber, span, "a hook fold outside a landing pad"));
        };
        let folded = self.fold_hook_panics(Err(*inflight));
        if let Err(ctl) = folded {
            fiber.set_unwinding(idx, Box::new(ctl));
        }
        Ok(())
    }

    // ---- the cold arms of `exec`, out of line so the loop's state stays in registers ----
    // Each is called with the pc already synced and past the instruction, so `pc - 1` is the
    // instruction whose span a panic reports.

    #[inline(never)]
    fn op_global(
        &mut self,
        fiber: &mut VmFiber,
        chunk: &Chunk,
        base: usize,
        pc: usize,
        op: Op,
    ) -> Result<(), Ctl> {
        let span = chunk.spans[pc - 1];
        let r = |i: u16| base + usize::from(i);
        match op {
            Op::GetGlobalChecked { d, g } => {
                if let Some(Some(_)) = self.uninit.get(g as usize) {
                    let msg = self.uninit_message(g);
                    return Err(self.panic(fiber, span, msg));
                }
                Self::put_reg(fiber, r(d), self.globals[g as usize].clone());
            }
            Op::CheckGlobal { g } => {
                if let Some(Some(_)) = self.uninit.get(g as usize) {
                    let msg = self.uninit_message(g);
                    return Err(self.panic(fiber, span, msg));
                }
            }
            Op::SetGlobal { g, s } => {
                let v = std::mem::replace(&mut fiber.regs[r(s)], Value::Unit);
                let old = std::mem::replace(&mut self.globals[g as usize], v);
                if !is_scalar_value(&old) {
                    self.release(old);
                }
            }
            Op::InitGlobal { g, s } => {
                let v = std::mem::replace(&mut fiber.regs[r(s)], Value::Unit);
                let old = std::mem::replace(&mut self.globals[g as usize], v);
                if let Some(slot) = self.uninit.get_mut(g as usize) {
                    *slot = None;
                }
                if !is_scalar_value(&old) {
                    self.release(old);
                }
            }
            _ => return Err(self.internal(fiber, span, "not a global instruction")),
        }
        Ok(())
    }

    /// Writes a result into an empty absolute register (see `put!`).
    #[inline]
    pub(crate) fn put_reg(fiber: &mut VmFiber, at: usize, v: Value) {
        let slot = &mut fiber.regs[at];
        debug_assert!(
            is_scalar_value(slot),
            "a write into r{at} would lose {slot:?}"
        );
        std::mem::forget(std::mem::replace(slot, v));
    }

    #[inline(never)]
    fn op_call_init(
        &mut self,
        fiber: &mut VmFiber,
        chunk: &Chunk,
        base: usize,
        pc: usize,
        op: Op,
    ) -> Result<(), Ctl> {
        let Op::CallInit { d, base: a, g } = op else {
            unreachable!("op_call_init is called for its own instruction")
        };
        let init = match self.global_inits[usize::from(g)].chunk(None, &self.env) {
            Ok(c) => c,
            Err(msg) => {
                let span = chunk.spans[pc - 1];
                return Err(self.internal(
                    fiber,
                    span,
                    &format!("cannot compile an initialiser: {msg}"),
                ));
            }
        };
        self.push_call(
            fiber,
            init,
            base + usize::from(a),
            base + usize::from(d),
            chunk,
            pc,
        )
    }

    /// The chunk of a function that is not compiled yet (the fast path is in `exec`).
    #[cold]
    #[inline(never)]
    pub(crate) fn callee_slow(
        &mut self,
        fiber: &mut VmFiber,
        chunk: &Chunk,
        pc: usize,
        f: colibri_check::hir::FnId,
    ) -> Result<Rc<Chunk>, Ctl> {
        let span = chunk.spans[pc - 1];
        match self.chunk_of(f) {
            Some(Ok(c)) => Ok(c),
            Some(Err(msg)) => {
                Err(self.internal(fiber, span, &format!("cannot compile a function: {msg}")))
            }
            None => Err(self.internal(fiber, span, "unknown function")),
        }
    }

    #[inline(never)]
    fn op_binary(
        &mut self,
        fiber: &mut VmFiber,
        chunk: &Chunk,
        base: usize,
        pc: usize,
        op: Op,
    ) -> Result<(), Ctl> {
        let Op::Binary { d, a, b } = op else {
            unreachable!("op_binary is called for its own instruction")
        };
        let span = chunk.spans[pc - 1];
        let Op::Site { site } = chunk.code[pc - 1] else {
            return Err(self.internal(fiber, span, "a binary operation without its operator"));
        };
        let Site::Binary(bop) = &chunk.sites[usize::from(site)] else {
            return Err(self.internal(fiber, span, "a binary operation without its operator"));
        };
        let (l, r) = (
            fiber.regs[base + usize::from(a)].clone(),
            fiber.regs[base + usize::from(b)].clone(),
        );
        let v = self.binary(fiber, *bop, l, r, span)?;
        Self::put_reg(fiber, base + usize::from(d), v);
        Ok(())
    }

    #[inline(never)]
    fn op_unary(
        &mut self,
        fiber: &mut VmFiber,
        chunk: &Chunk,
        base: usize,
        pc: usize,
        op: Op,
    ) -> Result<(), Ctl> {
        let Op::Unary { d, a, site } = op else {
            unreachable!("op_unary is called for its own instruction")
        };
        let span = chunk.spans[pc - 1];
        let Site::Unary(uop) = &chunk.sites[usize::from(site)] else {
            return Err(self.internal(fiber, span, "a unary operation without its operator"));
        };
        let v = fiber.regs[base + usize::from(a)].clone();
        let v = self.unary(fiber, *uop, v, span)?;
        Self::put_reg(fiber, base + usize::from(d), v);
        Ok(())
    }

    #[inline(never)]
    fn op_print(
        &mut self,
        fiber: &mut VmFiber,
        chunk: &Chunk,
        base: usize,
        pc: usize,
        r: u16,
    ) -> Result<(), Ctl> {
        let span = chunk.spans[pc - 1];
        let at = base + usize::from(r);
        #[cfg(test)]
        if let Some(hook) = self.print_hook.clone() {
            hook(self, fiber);
        }
        match &fiber.regs[at] {
            Value::Int(i) => {
                let _ = writeln!(self.core.out, "{i}");
            }
            Value::Str(s) => {
                let _ = writeln!(self.core.out, "{s}");
            }
            _ => {
                let v = std::mem::replace(&mut fiber.regs[at], Value::Unit);
                let text = self.show_deep(fiber, &v, span);
                fiber.regs[at] = v;
                // A closed pipe is not a Colibri error; output is best effort.
                let _ = writeln!(self.core.out, "{}", text?);
            }
        }
        Ok(())
    }

    #[inline(never)]
    fn op_show(
        &mut self,
        fiber: &mut VmFiber,
        chunk: &Chunk,
        base: usize,
        pc: usize,
        d: u16,
        r: u16,
    ) -> Result<(), Ctl> {
        let at = base + usize::from(r);
        if let Value::Int(x) = fiber.regs[at] {
            let mut out = std::mem::take(&mut self.scratch);
            out.clear();
            crate::unchecked::push_int(&mut out, x);
            let text: Rc<str> = Rc::from(out.as_str());
            self.scratch = out;
            Self::put_reg(fiber, base + usize::from(d), Value::Str(text));
            return Ok(());
        }
        let span = chunk.spans[pc - 1];
        let v = std::mem::replace(&mut fiber.regs[at], Value::Unit);
        let text = self.show_deep(fiber, &v, span);
        fiber.regs[at] = v;
        Self::put_reg(fiber, base + usize::from(d), Value::Str(Rc::from(text?)));
        Ok(())
    }

    #[inline(never)]
    fn op_format(
        &mut self,
        fiber: &mut VmFiber,
        chunk: &Chunk,
        base: usize,
        pc: usize,
        op: Op,
    ) -> Result<(), Ctl> {
        let Op::Format { d, r, site } = op else {
            unreachable!("op_format is called for its own instruction")
        };
        let span = chunk.spans[pc - 1];
        let Site::Format(spec) = &chunk.sites[usize::from(site)] else {
            return Err(self.internal(fiber, span, "a format without its spec"));
        };
        let text = colibri_core::format::format_value(&fiber.regs[base + usize::from(r)], spec);
        Self::put_reg(fiber, base + usize::from(d), Value::Str(Rc::from(text)));
        Ok(())
    }

    #[inline(never)]
    fn op_str_concat(
        &mut self,
        fiber: &mut VmFiber,
        chunk: &Chunk,
        base: usize,
        pc: usize,
        op: Op,
    ) -> Result<(), Ctl> {
        let Op::StrConcat { d, base: a, n } = op else {
            unreachable!("op_str_concat is called for its own instruction")
        };
        let span = chunk.spans[pc - 1];
        // The scratch text keeps its capacity from one string to the next: the only allocation
        // of a concatenation is the string it makes.
        let mut out = std::mem::take(&mut self.scratch);
        out.clear();
        for i in 0..n {
            let at = base + usize::from(a + i);
            match &fiber.regs[at] {
                Value::Str(s) => out.push_str(s),
                Value::Int(x) => crate::unchecked::push_int(&mut out, *x),
                _ => {
                    let v = std::mem::replace(&mut fiber.regs[at], Value::Unit);
                    let text = self.show_deep(fiber, &v, span);
                    fiber.regs[at] = v;
                    out.push_str(&text?);
                }
            }
        }
        let text: Rc<str> = Rc::from(out.as_str());
        self.scratch = out;
        Self::put_reg(fiber, base + usize::from(d), Value::Str(text));
        Ok(())
    }

    #[inline(never)]
    fn op_intrinsic(
        &mut self,
        fiber: &mut VmFiber,
        chunk: &Chunk,
        base: usize,
        pc: usize,
        op: Op,
    ) -> Result<(), Ctl> {
        let Op::Intrinsic { d, base: a, site } = op else {
            unreachable!("op_intrinsic is called for its own instruction")
        };
        let span = chunk.spans[pc - 1];
        let Site::Intrinsic { which, argc } = &chunk.sites[usize::from(site)] else {
            return Err(self.internal(fiber, span, "an intrinsic without its site"));
        };
        let from = base + usize::from(a);
        let to = from + usize::from(*argc);
        let result = self.run_intrinsic(fiber, which, from..to, span)?;
        Self::put_reg(fiber, base + usize::from(d), result);
        Ok(())
    }

    /// `Builtin`: core runs the built-in of the site on references of its own to the arguments
    /// of the window (the arguments stay there for the caller to drop).
    #[inline(never)]
    fn op_builtin(
        &mut self,
        fiber: &mut VmFiber,
        chunk: &Chunk,
        base: usize,
        pc: usize,
        op: Op,
    ) -> Result<(), Ctl> {
        let Op::Builtin { d, base: a, site } = op else {
            unreachable!("op_builtin is called for its own instruction")
        };
        let span = chunk.spans[pc - 1];
        let Some(Site::Intrinsic {
            which: colibri_check::hir::Intrinsic::Builtin(which),
            argc,
        }) = chunk.sites.get(usize::from(site))
        else {
            return Err(self.internal(fiber, span, "a built-in without its site"));
        };
        let from = base + usize::from(a);
        let mut args: SmallVec<[Value; 4]> = fiber.regs[from..from + usize::from(*argc)]
            .iter()
            .map(Value::dup)
            .collect();
        let result = self.builtin_args(fiber, *which, &mut args, span);
        for arg in args {
            self.release(arg);
        }
        // Cancelling resumes the coroutines to unwind them: a refusal they meet is raised here.
        let result = if matches!(
            which,
            colibri_check::hir::Builtin::CoroutineCancel
                | colibri_check::hir::Builtin::CoGroupCancel
        ) {
            self.settle_refusal(fiber, span, result)
        } else {
            result
        }?;
        Self::put_reg(fiber, base + usize::from(d), result);
        if colibri_core::compare::take_gave_up() {
            return Err(self.lookup_gave_up(fiber, chunk, pc, Some(base + usize::from(d))));
        }
        Ok(())
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
    /// result goes to the absolute register `ret`. Checks the call depth and charges the call
    /// together with the callee's opening `Step`, which it then skips. A failure is reported
    /// at the instruction before `pc` of `caller`.
    #[inline(always)]
    pub(crate) fn push_call(
        &mut self,
        fiber: &mut VmFiber,
        callee: Rc<Chunk>,
        new_base: usize,
        ret: usize,
        caller: &Chunk,
        pc: usize,
    ) -> Result<(), Ctl> {
        let (mut steps, mut start) = (1, 0);
        if let Op::Step { n } = callee.code[0] {
            steps += n;
            start = 1;
        }
        self.core.budget.left -= i64::from(steps);
        if fiber.visible_depth() >= self.max_depth || self.core.budget.left < 0 {
            return Err(self.call_refused(fiber, caller, pc));
        }
        let need = new_base + callee.nregs as usize;
        if fiber.regs.len() < need {
            fiber.regs.resize(need, Value::Unit);
        }
        fiber.hidden += usize::from(callee.hidden);
        fiber.frames.push(Frame {
            chunk: callee,
            pc: start,
            base: new_base as u32,
            ret: ret as u32,
            kind: FrameKind::Colibri,
            noted: None,
            temps_mark: fiber.temps.len() as u32,
        });
        Ok(())
    }

    /// `CallHidden`: a function of the prelude. The native built-in it replaces charged one step
    /// for each element of its receiver and nothing else; so does this, and the code of the
    /// prelude, which is compiled without budget steps, charges nothing more.
    #[inline(never)]
    fn op_call_hidden(
        &mut self,
        fiber: &mut VmFiber,
        chunk: &Chunk,
        base: usize,
        pc: usize,
        [d, a, site]: [u16; 3],
    ) -> Result<(), Ctl> {
        let span = chunk.spans[pc - 1];
        let Site::Hidden { f, work } = &chunk.sites[usize::from(site)] else {
            return Err(self.internal(fiber, span, "a call without its function"));
        };
        let at = base + usize::from(a);
        if let Some(b) = work
            && let Some(n) = colibri_core::work_of(*b, fiber.regs.get(at))
        {
            self.charge(fiber, span, n)?;
        }
        // A closure that is a leaf runs in a native loop; anything it cannot do alone is the
        // prelude's function below, from the start.
        if let Some(
            b @ (colibri_check::hir::Builtin::ListMap
            | colibri_check::hir::Builtin::ListFilter
            | colibri_check::hir::Builtin::ListAny
            | colibri_check::hir::Builtin::ListAll
            | colibri_check::hir::Builtin::ListFind
            | colibri_check::hir::Builtin::ListFindIndex
            | colibri_check::hir::Builtin::ListFold),
        ) = work
            && self.leaf_hof(fiber, base, a, d, *b)
        {
            return Ok(());
        }
        let callee = self.callee_chunk(fiber, chunk, pc, *f)?;
        // No depth check against the limit: the built-in adds no visible call, and the closure it
        // calls is checked by `push_call` and `enter_with`. This only bounds the hidden frames.
        if fiber.frames.len() >= 2 * self.max_depth + 64 {
            return Err(self.internal(fiber, span, "the prelude's frames outgrew the call limit"));
        }
        fiber.hidden += usize::from(callee.hidden);
        let need = at + callee.nregs as usize;
        if fiber.regs.len() < need {
            fiber.regs.resize(need, Value::Unit);
        }
        fiber.frames.push(Frame {
            chunk: callee,
            pc: 0,
            base: at as u32,
            ret: (base + usize::from(d)) as u32,
            kind: FrameKind::Colibri,
            noted: None,
            temps_mark: fiber.temps.len() as u32,
        });
        Ok(())
    }

    /// A call that cannot be made: too deep, or out of budget.
    #[cold]
    #[inline(never)]
    fn call_refused(&mut self, fiber: &mut VmFiber, caller: &Chunk, pc: usize) -> Ctl {
        let span = caller.spans[pc - 1];
        if fiber.visible_depth() >= self.max_depth {
            self.overflow(fiber, span)
        } else {
            self.step_failure(fiber, span)
        }
    }

    #[cold]
    #[inline(never)]
    pub(crate) fn overflow(&mut self, fiber: &mut VmFiber, span: colibri_syntax::Span) -> Ctl {
        let message = format!("stack overflow: more than {} nested calls", self.max_depth);
        self.panic(fiber, span, message)
    }

    /// Pops the top frame, delivering `v`. `Some(v)` when the frame was an entry from Rust:
    /// the loop ends with that value.
    #[inline(always)]
    fn pop_frame(&mut self, fiber: &mut VmFiber, v: Value) -> Option<Value> {
        let frame = fiber.frames.pop().expect("a frame to pop");
        fiber.hidden -= usize::from(frame.chunk.hidden);
        debug_assert!(
            fiber.temps.len() == frame.temps_mark as usize,
            "held temporaries outlive their frame"
        );
        match frame.kind {
            FrameKind::NativeEntry => Some(v),
            FrameKind::Colibri => {
                Self::put_reg(fiber, frame.ret as usize, v);
                None
            }
            FrameKind::HostShim => unreachable!("a host call frame is popped by its call"),
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
            Intrinsic::Ptr(op) => {
                let values: Vec<Value> = args
                    .map(|i| std::mem::replace(&mut fiber.regs[i], Value::Unit))
                    .collect();
                self.ptr_intrinsic(fiber, op, values, span)
            }
            // The check before the loop of `wait_until`: may this code pause?
            Intrinsic::WaitUntil => {
                self.pause_context(fiber, span, "wait_until")?;
                // A coroutine that is being cancelled (in a `defer`) does not even ask.
                if fiber.state.cancelling {
                    return Err(Ctl::Cancel);
                }
                Ok(Value::Unit)
            }
            Intrinsic::TaskTick => match fiber.regs[args.start] {
                Value::Float(dt) => self.task_tick(fiber, dt, span),
                _ => Err(self.internal(fiber, span, "ill-typed tick")),
            },
            Intrinsic::NewCoGroup => {
                Ok(self.new_scheduler_object(colibri_check::types::COGROUP_TYPE))
            }
            Intrinsic::NewSignal => {
                Ok(self.new_scheduler_object(colibri_check::types::SIGNAL_TYPE))
            }
            Intrinsic::HostAlive => Ok(Value::Bool(match &fiber.regs[args.start] {
                Value::Host(r) => self.core.host_alive(r.index, r.generation),
                _ => false,
            })),
            Intrinsic::ErrorIs(_) | Intrinsic::ErrorAs(_) => {
                let arg = fiber.regs[args.start].clone();
                let result = self.error_intrinsic(fiber, which, &arg, span);
                self.release(arg);
                result
            }
            Intrinsic::Compare => {
                let values: Vec<Value> = fiber.regs[args].to_vec();
                self.compare_values(fiber, values, span)
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
