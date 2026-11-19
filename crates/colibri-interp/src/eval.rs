//! The recursive evaluator (ARCHITECTURE §2).

use std::io::Write;
use std::rc::Rc;

use colibri_check::hir::{
    Arg, ArithOp, Arm, BinOp, Block, Body, Callee, CmpOp, Expr, ExprKind, FnId, ForLoop, GlobalId,
    Intrinsic, Lit, LocalId, LoopId, Num, Operand, Program, Stmt, StmtKind, TypeDef, UnOp,
};
use colibri_check::types::IntTy;
use colibri_syntax::Span;

use crate::compare::{values_cmp, values_equal};
use crate::ctl::{Ctl, Eval, PanicInfo, TraceEntry, into_panic};
use crate::fiber::{Fiber, Frame};
use crate::format::format_value;
use crate::place::project;
use crate::show::show;
use crate::value::Value;

/// The interpreter: the loaded code, the module-level state and the output writer. Everything
/// about a running computation is in the [`Fiber`] passed to each method.
pub struct Interp {
    /// Looked up at call time, so a patched function takes effect at its next call.
    fns: Vec<Rc<Body>>,
    pub(crate) globals: Vec<Value>,
    /// The user-defined types, which `Show` needs for names.
    types: Vec<TypeDef>,
    inits: Vec<(GlobalId, Rc<Body>)>,
    script: Option<Rc<Body>>,
    out: Box<dyn Write>,
    max_depth: usize,
    native_stack_budget: usize,
}

/// The default for [`Interp::set_native_stack_budget`]: small enough for a default test thread.
pub const DEFAULT_NATIVE_STACK_BUDGET: usize = 1 << 20;

/// The current position on the native stack (it only has to be comparable with another one).
fn stack_position() -> usize {
    let marker = 0u8;
    std::hint::black_box(&marker) as *const u8 as usize
}

/// A fault in integer arithmetic.
enum Fault {
    Overflow,
    DivByZero,
    ModByZero,
}

impl Interp {
    pub fn new(out: Box<dyn Write>, max_call_depth: usize) -> Self {
        Interp {
            fns: Vec::new(),
            globals: Vec::new(),
            types: Vec::new(),
            inits: Vec::new(),
            script: None,
            out,
            max_depth: max_call_depth,
            native_stack_budget: DEFAULT_NATIVE_STACK_BUDGET,
        }
    }

    /// How many bytes of the Rust stack evaluation may use below the point the host called in.
    /// This is a backstop behind the call-depth limit: when a deep recursion would use more, it
    /// panics with "stack overflow" instead of crashing the host. Set it a little below the
    /// stack of the thread that runs the interpreter.
    pub fn set_native_stack_budget(&mut self, bytes: usize) {
        self.native_stack_budget = bytes;
    }

    /// Installs a checked program, replacing any earlier one and resetting module state.
    pub fn load(&mut self, program: &Program) {
        self.fns = program.fns.iter().map(|f| Rc::clone(&f.body)).collect();
        self.globals = vec![Value::Unit; program.globals.len()];
        self.types = program.types.clone();
        self.inits = program
            .init_order
            .iter()
            .map(|&g| (g, Rc::clone(&program.globals[g.0 as usize].init)))
            .collect();
        self.script = Some(Rc::clone(&program.script));
    }

    /// Runs the module-level initialisers in dependency order, then the top-level statements.
    pub fn run_module(&mut self, fiber: &mut Fiber) -> Result<(), Box<PanicInfo>> {
        let inits = self.inits.clone();
        for (g, body) in inits {
            let value = self.run_body(fiber, &body)?;
            self.globals[g.0 as usize] = value;
        }
        if let Some(script) = self.script.clone() {
            self.run_body(fiber, &script)?;
        }
        Ok(())
    }

    /// Calls a function from the host.
    pub fn call(
        &mut self,
        fiber: &mut Fiber,
        f: FnId,
        args: Vec<Value>,
    ) -> Result<Value, Box<PanicInfo>> {
        let Some(body) = self.fns.get(f.0 as usize).cloned() else {
            return Err(self.bare_panic("internal error: unknown function"));
        };
        self.enter_from_host(fiber, |this, fiber| this.call_body(fiber, body, args))
    }

    /// Writes buffered output through to the writer.
    pub fn flush(&mut self) {
        let _ = self.out.flush();
    }

    /// The module-level value `g` (for hosts and tests).
    pub fn global(&self, g: GlobalId) -> Option<&Value> {
        self.globals.get(g.0 as usize)
    }

    fn run_body(&mut self, fiber: &mut Fiber, body: &Rc<Body>) -> Result<Value, Box<PanicInfo>> {
        let body = Rc::clone(body);
        self.enter_from_host(fiber, |this, fiber| this.call_body(fiber, body, Vec::new()))
    }

    /// Runs `f` as a call from outside the interpreter; on any failure the fiber is cleaned up
    /// to what it was, so it stays usable.
    fn enter_from_host(
        &mut self,
        fiber: &mut Fiber,
        f: impl FnOnce(&mut Self, &mut Fiber) -> Eval,
    ) -> Result<Value, Box<PanicInfo>> {
        let (frames, temps) = (fiber.frames.len(), fiber.temps.len());
        if frames == 0 {
            fiber.stack_base = stack_position();
        }
        let result = f(self, fiber);
        fiber.frames.truncate(frames);
        self.release_temps(fiber, temps);
        match result {
            Ok(v) => Ok(v),
            Err(Ctl::Return(v)) => Ok(v),
            Err(ctl) => Err(into_panic(ctl, Span::new(colibri_syntax::FileId(0), 0, 0))),
        }
    }

    fn bare_panic(&self, message: &str) -> Box<PanicInfo> {
        Box::new(PanicInfo {
            message: message.to_string(),
            span: Span::new(colibri_syntax::FileId(0), 0, 0),
            stack: Vec::new(),
        })
    }

    // ---- panics ----

    /// Creates a panic at `span`, capturing the Colibri stack as it is now.
    pub(crate) fn panic(&self, fiber: &mut Fiber, span: Span, message: impl Into<String>) -> Ctl {
        if let Some(top) = fiber.frames.last_mut() {
            top.span = span;
        }
        let stack = fiber
            .frames
            .iter()
            .rev()
            .map(|fr| TraceEntry {
                function: fr.body.name.clone(),
                span: fr.span,
            })
            .collect();
        Ctl::Panic(Box::new(PanicInfo {
            message: message.into(),
            span,
            stack,
        }))
    }

    pub(crate) fn internal(&self, fiber: &mut Fiber, span: Span, what: &str) -> Ctl {
        self.panic(fiber, span, format!("internal error: {what}"))
    }

    // ---- calls and scopes ----

    fn call_body(&mut self, fiber: &mut Fiber, body: Rc<Body>, args: Vec<Value>) -> Eval {
        self.call_body_inout(fiber, body, args).0
    }

    /// Runs a call. For a `var self` method the second result is the final value of `self`,
    /// taken out of the frame before its locals are released, for the caller to write back;
    /// it is present on every path, including a panic.
    fn call_body_inout(
        &mut self,
        fiber: &mut Fiber,
        body: Rc<Body>,
        args: Vec<Value>,
    ) -> (Eval, Option<Value>) {
        if fiber.frames.len() >= self.max_depth {
            let span = fiber.frames.last().map_or(body.span, |f| f.span);
            let message = format!("stack overflow: more than {} nested calls", self.max_depth);
            let ctl = self.panic(fiber, span, message);
            let receiver = if body.mutates_self {
                args.into_iter().next()
            } else {
                None
            };
            return (Err(ctl), receiver);
        }
        let mut slots = args;
        slots.resize(body.locals.len(), Value::Unit);
        fiber.frames.push(Frame {
            body: Rc::clone(&body),
            slots,
            span: body.span,
        });
        let result = self.run_block(fiber, &body.block);
        let receiver = match fiber.frames.last_mut() {
            Some(frame) if body.mutates_self => frame
                .slots
                .first_mut()
                .map(|s| std::mem::replace(s, Value::Unit)),
            _ => None,
        };
        self.exit_scope(fiber, &body.block);
        fiber.frames.pop();
        let result = match result {
            Err(Ctl::Return(v)) => Ok(v),
            other => other,
        };
        (result, receiver)
    }

    /// Evaluates a block and leaves its scope on every path (normal, `break`, `return`, panic).
    fn eval_block(&mut self, fiber: &mut Fiber, block: &Block) -> Eval {
        let result = self.run_block(fiber, block);
        self.exit_scope(fiber, block);
        result
    }

    fn run_block(&mut self, fiber: &mut Fiber, block: &Block) -> Eval {
        for stmt in &block.stmts {
            self.exec_stmt(fiber, stmt)?;
        }
        match &block.tail {
            Some(tail) => self.eval(fiber, tail),
            None => Ok(Value::Unit),
        }
    }

    /// Releases the block's locals in reverse declaration order. The one place scope-end
    /// cleanup happens (`defer` joins it later).
    fn exit_scope(&mut self, fiber: &mut Fiber, block: &Block) {
        for local in block.locals.iter().rev() {
            let old = match fiber.frames.last_mut() {
                Some(frame) => std::mem::replace(&mut frame.slots[local.0 as usize], Value::Unit),
                None => continue,
            };
            self.release(old);
        }
    }

    /// An owned value dies here. Nothing observes drops yet; when classes and `Drop` arrive,
    /// the last-reference logic and drop tracing live in this function.
    fn release(&mut self, value: Value) {
        drop(value);
    }

    fn release_temps(&mut self, fiber: &mut Fiber, mark: usize) {
        while fiber.temps.len() > mark {
            if let Some(v) = fiber.temps.pop() {
                self.release(v);
            }
        }
    }

    /// Evaluates an operand that is not an ownership transfer onto the temporaries stack.
    fn eval_temp(&mut self, fiber: &mut Fiber, e: &Expr) -> Eval<()> {
        let v = self.eval(fiber, e)?;
        fiber.temps.push(v);
        Ok(())
    }

    fn pop_temp(&mut self, fiber: &mut Fiber) -> Value {
        fiber.temps.pop().unwrap_or(Value::Unit)
    }

    // ---- statements ----

    /// Every statement is a temporary scope: whatever it left on the temps stack dies at its
    /// end, even when it ends early.
    fn exec_stmt(&mut self, fiber: &mut Fiber, stmt: &Stmt) -> Eval<()> {
        let mark = fiber.temps.len();
        let result = self.exec_stmt_inner(fiber, stmt);
        self.release_temps(fiber, mark);
        result
    }

    fn exec_stmt_inner(&mut self, fiber: &mut Fiber, stmt: &Stmt) -> Eval<()> {
        match &stmt.kind {
            StmtKind::Let { local, init } => {
                let value = self.eval(fiber, init)?;
                let old = match fiber.frames.last_mut() {
                    Some(frame) => std::mem::replace(&mut frame.slots[local.0 as usize], value),
                    None => return Err(self.internal(fiber, stmt.span, "no frame")),
                };
                self.release(old);
                Ok(())
            }
            StmtKind::LetPat { pat, init } => {
                let value = self.eval(fiber, init)?;
                let mut binds = Vec::new();
                if !self.match_pat(pat, &value, &mut binds) {
                    return Err(self.internal(fiber, stmt.span, "an irrefutable pattern failed"));
                }
                self.release(value);
                self.bind_all(fiber, binds);
                Ok(())
            }
            StmtKind::Expr(e) => {
                let value = self.eval(fiber, e)?;
                self.release(value);
                Ok(())
            }
            StmtKind::Assign { place, op, rhs } => {
                let rhs_value = self.eval(fiber, rhs)?;
                let new = match op {
                    None => rhs_value,
                    Some(op) => {
                        let old = self.read_place(fiber, place, stmt.span)?;
                        self.binary(fiber, *op, old, rhs_value, stmt.span)?
                    }
                };
                let old = self.write_place(fiber, place, new, stmt.span)?;
                self.release(old);
                Ok(())
            }
            StmtKind::Return(value) => {
                let v = match value {
                    Some(e) => self.eval(fiber, e)?,
                    None => Value::Unit,
                };
                Err(Ctl::Return(v))
            }
            StmtKind::Break(id) => Err(Ctl::Break(*id, Value::Unit)),
            StmtKind::Continue(id) => Err(Ctl::Continue(*id)),
        }
    }

    // ---- expressions ----

    /// The evaluator's hot recursion. Each arm hands off to a function of its own, which keeps
    /// this frame small: an unoptimised build pays for every arm's locals on every nested call.
    fn eval(&mut self, fiber: &mut Fiber, e: &Expr) -> Eval {
        // Every recursion passes through here, so this one check also bounds deeply nested code.
        if stack_position().abs_diff(fiber.stack_base) > self.native_stack_budget {
            let message = format!(
                "stack overflow: the native stack budget of {} bytes is used up",
                self.native_stack_budget
            );
            return Err(self.panic(fiber, e.span, message));
        }
        match &e.kind {
            ExprKind::Lit(lit) => Ok(literal(lit)),
            ExprKind::Read(place) => self.read_place(fiber, place, e.span),
            ExprKind::Unary(op, operand) => self.eval_unary(fiber, *op, operand, e.span),
            ExprKind::Binary(op, lhs, rhs) => self.eval_binary(fiber, *op, lhs, rhs, e.span),
            ExprKind::And(lhs, rhs) => self.eval_and(fiber, lhs, rhs),
            ExprKind::Or(lhs, rhs) => self.eval_or(fiber, lhs, rhs),
            ExprKind::Call(call) => self.eval_call(fiber, &call.callee, &call.args, e.span),
            ExprKind::StrConcat(parts) => self.eval_concat(fiber, parts),
            ExprKind::If { cond, then, else_ } => {
                self.eval_if(fiber, cond, then, else_.as_deref(), e.span)
            }
            ExprKind::Block(block) => self.eval_block(fiber, block),
            ExprKind::Loop { id, body } => self.eval_loop(fiber, *id, body),
            ExprKind::For(f) => self.eval_for(fiber, f, e.span),
            ExprKind::Struct { ty, fields } => {
                let values = self.eval_values(fiber, fields)?;
                Ok(Value::strukt(*ty, values))
            }
            ExprKind::Variant { ty, variant, args } => {
                let values = self.eval_values(fiber, args)?;
                Ok(Value::variant(*ty, *variant, values))
            }
            ExprKind::Tuple(items) => {
                let values = self.eval_values(fiber, items)?;
                Ok(Value::tuple(values))
            }
            ExprKind::Project(base, proj) => {
                let value = self.eval(fiber, base)?;
                match project(&value, *proj) {
                    Some(part) => Ok(part.clone()),
                    None => Err(self.internal(fiber, e.span, "ill-typed projection")),
                }
            }
            ExprKind::Match { scrutinee, arms } => self.eval_match(fiber, scrutinee, arms, e.span),
            ExprKind::Error => Err(self.internal(fiber, e.span, "unchecked expression")),
        }
    }

    #[inline(never)]
    fn eval_unary(&mut self, fiber: &mut Fiber, op: UnOp, operand: &Expr, span: Span) -> Eval {
        let v = self.eval(fiber, operand)?;
        self.unary(fiber, op, v, span)
    }

    #[inline(never)]
    fn eval_binary(
        &mut self,
        fiber: &mut Fiber,
        op: BinOp,
        lhs: &Expr,
        rhs: &Expr,
        span: Span,
    ) -> Eval {
        self.eval_temp(fiber, lhs)?;
        self.eval_temp(fiber, rhs)?;
        let r = self.pop_temp(fiber);
        let l = self.pop_temp(fiber);
        self.binary(fiber, op, l, r, span)
    }

    #[inline(never)]
    fn eval_and(&mut self, fiber: &mut Fiber, lhs: &Expr, rhs: &Expr) -> Eval {
        match self.eval(fiber, lhs)? {
            Value::Bool(true) => self.eval(fiber, rhs),
            other => Ok(other),
        }
    }

    #[inline(never)]
    fn eval_or(&mut self, fiber: &mut Fiber, lhs: &Expr, rhs: &Expr) -> Eval {
        match self.eval(fiber, lhs)? {
            Value::Bool(false) => self.eval(fiber, rhs),
            other => Ok(other),
        }
    }

    #[inline(never)]
    fn eval_concat(&mut self, fiber: &mut Fiber, parts: &[Expr]) -> Eval {
        let base = fiber.temps.len();
        for part in parts {
            self.eval_temp(fiber, part)?;
        }
        let values = fiber.temps.split_off(base);
        let mut out = String::new();
        for v in values {
            out.push_str(&show(&self.types, &v));
            self.release(v);
        }
        Ok(Value::Str(Rc::from(out)))
    }

    #[inline(never)]
    fn eval_if(
        &mut self,
        fiber: &mut Fiber,
        cond: &Expr,
        then: &Block,
        else_: Option<&Block>,
        span: Span,
    ) -> Eval {
        // The condition's temporaries are gone before either branch runs.
        let taken = match self.eval(fiber, cond)? {
            Value::Bool(b) => b,
            _ => return Err(self.internal(fiber, span, "`if` condition is not a bool")),
        };
        if taken {
            self.eval_block(fiber, then)
        } else if let Some(block) = else_ {
            self.eval_block(fiber, block)
        } else {
            Ok(Value::Unit)
        }
    }

    #[inline(never)]
    fn eval_loop(&mut self, fiber: &mut Fiber, id: LoopId, body: &Block) -> Eval {
        loop {
            match self.eval_block(fiber, body) {
                Ok(v) => self.release(v),
                Err(Ctl::Continue(target)) if target == id => {}
                Err(Ctl::Break(target, v)) if target == id => return Ok(v),
                Err(other) => return Err(other),
            }
        }
    }

    /// `for i in a..b`: the bounds are evaluated once; each iteration gets a fresh `i`.
    #[inline(never)]
    fn eval_for(&mut self, fiber: &mut Fiber, f: &ForLoop, span: Span) -> Eval {
        let (Value::Int(start), Value::Int(end)) =
            (self.eval(fiber, &f.start)?, self.eval(fiber, &f.end)?)
        else {
            return Err(self.internal(fiber, span, "a `for` range is not an integer range"));
        };
        let mut i = start;
        loop {
            if (f.inclusive && i > end) || (!f.inclusive && i >= end) {
                break;
            }
            if let Some(binding) = f.binding {
                let old = match fiber.frames.last_mut() {
                    Some(frame) => {
                        std::mem::replace(&mut frame.slots[binding.0 as usize], Value::Int(i))
                    }
                    None => return Err(self.internal(fiber, span, "no frame")),
                };
                self.release(old);
            }
            match self.eval_block(fiber, &f.body) {
                Ok(v) => self.release(v),
                Err(Ctl::Continue(target)) if target == f.id => {}
                Err(Ctl::Break(target, v)) if target == f.id => {
                    self.release(v);
                    break;
                }
                Err(other) => return Err(other),
            }
            // The last value of an inclusive range must not be stepped past (it may be i64::MAX).
            if f.inclusive && i == end {
                break;
            }
            i += 1;
        }
        Ok(Value::Unit)
    }

    /// Stores pattern bindings in their slots.
    pub(crate) fn bind_all(&mut self, fiber: &mut Fiber, binds: Vec<(LocalId, Value)>) {
        for (local, value) in binds {
            let old = match fiber.frames.last_mut() {
                Some(frame) => std::mem::replace(&mut frame.slots[local.0 as usize], value),
                None => continue,
            };
            self.release(old);
        }
    }

    /// `match`: the scrutinee stays on the temporaries stack for the whole construct, and the
    /// first arm whose pattern matches and whose guard holds runs.
    #[inline(never)]
    fn eval_match(
        &mut self,
        fiber: &mut Fiber,
        scrutinee: &Expr,
        arms: &[Arm],
        span: Span,
    ) -> Eval {
        let mark = fiber.temps.len();
        self.eval_temp(fiber, scrutinee)?;
        let result = self.run_arms(fiber, mark, arms, span);
        self.release_temps(fiber, mark);
        result
    }

    fn run_arms(&mut self, fiber: &mut Fiber, scrutinee: usize, arms: &[Arm], span: Span) -> Eval {
        for arm in arms {
            let value = fiber.temps[scrutinee].clone();
            let mut binds = Vec::new();
            if !self.match_pat(&arm.pat, &value, &mut binds) {
                continue;
            }
            drop(value);
            self.bind_all(fiber, binds);
            if let Some(guard) = &arm.guard {
                let held = match self.eval(fiber, guard) {
                    Ok(Value::Bool(b)) => b,
                    Ok(_) => false,
                    Err(ctl) => {
                        self.release_arm(fiber, &arm.body);
                        return Err(ctl);
                    }
                };
                if !held {
                    self.release_arm(fiber, &arm.body);
                    continue;
                }
            }
            return self.eval_block(fiber, &arm.body);
        }
        Err(self.internal(fiber, span, "no `match` arm matched"))
    }

    /// Drops an arm's bindings when its guard fails.
    fn release_arm(&mut self, fiber: &mut Fiber, body: &Block) {
        self.exit_scope(fiber, body);
    }

    /// Evaluates expressions left to right into one vector of values.
    fn eval_values(&mut self, fiber: &mut Fiber, exprs: &[Expr]) -> Eval<Vec<Value>> {
        let base = fiber.temps.len();
        for e in exprs {
            self.eval_temp(fiber, e)?;
        }
        Ok(fiber.temps.split_off(base))
    }

    fn eval_call(&mut self, fiber: &mut Fiber, callee: &Callee, args: &[Arg], span: Span) -> Eval {
        let base = fiber.temps.len();
        // The receiver of a `var self` method is taken last, so the other arguments still see
        // the variable it lives in.
        let mut receiver = None;
        for arg in args {
            match arg {
                Arg::Value(e) => self.eval_temp(fiber, e)?,
                Arg::Place(place) => receiver = Some(place),
            }
        }
        let mut values = fiber.temps.split_off(base);
        match callee {
            Callee::Fn(id) => {
                let Some(body) = self.fns.get(id.0 as usize).cloned() else {
                    return Err(self.internal(fiber, span, "unknown function"));
                };
                if let Some(frame) = fiber.frames.last_mut() {
                    frame.span = span;
                }
                let Some(place) = receiver else {
                    return self.call_body(fiber, body, values);
                };
                let value = self.take_place(fiber, place, span)?;
                values.insert(0, value);
                let (result, out) = self.call_body_inout(fiber, body, values);
                if let Some(updated) = out {
                    let old = self.write_place(fiber, place, updated, span)?;
                    self.release(old);
                }
                result
            }
            Callee::Intrinsic(intrinsic) => self.intrinsic(fiber, intrinsic, values, span),
        }
    }

    fn intrinsic(
        &mut self,
        fiber: &mut Fiber,
        intrinsic: &Intrinsic,
        mut args: Vec<Value>,
        span: Span,
    ) -> Eval {
        let Some(arg) = args.pop() else {
            return Err(self.internal(fiber, span, "intrinsic without an argument"));
        };
        let result = match intrinsic {
            Intrinsic::Print => {
                // A closed pipe is not a Colibri error; output is best effort.
                let _ = writeln!(self.out, "{}", show(&self.types, &arg));
                Value::Unit
            }
            Intrinsic::Show => Value::Str(Rc::from(show(&self.types, &arg).as_ref())),
            Intrinsic::Format(spec) => Value::Str(Rc::from(format_value(&arg, spec))),
        };
        self.release(arg);
        Ok(result)
    }

    // ---- operators ----

    fn unary(&mut self, fiber: &mut Fiber, op: UnOp, v: Value, span: Span) -> Eval {
        match (op, v) {
            (UnOp::Neg(Num::Int(IntTy::I64)), Value::Int(i)) => match i.checked_neg() {
                Some(n) => Ok(Value::Int(n)),
                None => Err(self.panic(fiber, span, "integer overflow")),
            },
            (UnOp::Neg(Num::Float), Value::Float(x)) => Ok(Value::Float(-x)),
            (UnOp::Not, Value::Bool(b)) => Ok(Value::Bool(!b)),
            _ => Err(self.internal(fiber, span, "ill-typed unary operation")),
        }
    }

    fn binary(&mut self, fiber: &mut Fiber, op: BinOp, l: Value, r: Value, span: Span) -> Eval {
        match (op, l, r) {
            (BinOp::Arith(op, Num::Int(IntTy::I64)), Value::Int(a), Value::Int(b)) => {
                match int_arith(op, a, b) {
                    Ok(n) => Ok(Value::Int(n)),
                    Err(Fault::Overflow) => Err(self.panic(fiber, span, "integer overflow")),
                    Err(Fault::DivByZero) => Err(self.panic(fiber, span, "division by zero")),
                    Err(Fault::ModByZero) => Err(self.panic(fiber, span, "modulo by zero")),
                }
            }
            (BinOp::Arith(op, Num::Float), Value::Float(a), Value::Float(b)) => {
                Ok(Value::Float(match op {
                    ArithOp::Add => a + b,
                    ArithOp::Sub => a - b,
                    ArithOp::Mul => a * b,
                    ArithOp::Div => a / b,
                    ArithOp::Rem => a % b,
                }))
            }
            (BinOp::Concat, Value::Str(a), Value::Str(b)) => {
                let mut s = String::with_capacity(a.len() + b.len());
                s.push_str(&a);
                s.push_str(&b);
                Ok(Value::Str(Rc::from(s)))
            }
            (BinOp::Cmp(op, Operand::Structural), l, r) => match structural(op, &l, &r) {
                Ok(b) => Ok(Value::Bool(b)),
                Err(_) => Err(self.panic(fiber, span, "these values nest too deeply to compare")),
            },
            (BinOp::Cmp(op, operand), l, r) => match compare(op, operand, &l, &r) {
                Some(b) => Ok(Value::Bool(b)),
                None => Err(self.internal(fiber, span, "ill-typed comparison")),
            },
            _ => Err(self.internal(fiber, span, "ill-typed binary operation")),
        }
    }
}

fn literal(lit: &Lit) -> Value {
    match lit {
        Lit::Unit => Value::Unit,
        Lit::Bool(b) => Value::Bool(*b),
        Lit::Int(i) => Value::Int(*i),
        Lit::Float(x) => Value::Float(*x),
        Lit::Char(c) => Value::Char(*c),
        Lit::Str(s) => Value::Str(Rc::clone(s)),
    }
}

/// Checked 64-bit integer arithmetic: overflow and division by zero are faults, `/` truncates
/// toward zero and `%` takes the sign of the dividend.
fn int_arith(op: ArithOp, a: i64, b: i64) -> Result<i64, Fault> {
    match op {
        ArithOp::Add => a.checked_add(b).ok_or(Fault::Overflow),
        ArithOp::Sub => a.checked_sub(b).ok_or(Fault::Overflow),
        ArithOp::Mul => a.checked_mul(b).ok_or(Fault::Overflow),
        ArithOp::Div if b == 0 => Err(Fault::DivByZero),
        ArithOp::Div => a.checked_div(b).ok_or(Fault::Overflow),
        ArithOp::Rem if b == 0 => Err(Fault::ModByZero),
        ArithOp::Rem => a.checked_rem(b).ok_or(Fault::Overflow),
    }
}

/// The derived comparison of two structs, enums or tuples.
fn structural(op: CmpOp, l: &Value, r: &Value) -> Result<bool, crate::compare::TooDeep> {
    use std::cmp::Ordering::{Equal, Greater, Less};
    Ok(match op {
        CmpOp::Eq => values_equal(l, r)?,
        CmpOp::Ne => !values_equal(l, r)?,
        CmpOp::Lt => values_cmp(l, r)? == Some(Less),
        CmpOp::Le => matches!(values_cmp(l, r)?, Some(Less | Equal)),
        CmpOp::Gt => values_cmp(l, r)? == Some(Greater),
        CmpOp::Ge => matches!(values_cmp(l, r)?, Some(Greater | Equal)),
    })
}

/// Compares two values of the same type; `None` if they are not what the operand kind says.
fn compare(op: CmpOp, operand: Operand, l: &Value, r: &Value) -> Option<bool> {
    fn by_ord<T: PartialOrd + ?Sized>(op: CmpOp, a: &T, b: &T) -> bool {
        match op {
            CmpOp::Eq => a == b,
            CmpOp::Ne => a != b,
            CmpOp::Lt => a < b,
            CmpOp::Le => a <= b,
            CmpOp::Gt => a > b,
            CmpOp::Ge => a >= b,
        }
    }
    Some(match (operand, l, r) {
        (Operand::Int(_), Value::Int(a), Value::Int(b)) => by_ord(op, a, b),
        (Operand::Float, Value::Float(a), Value::Float(b)) => by_ord(op, a, b),
        (Operand::Bool, Value::Bool(a), Value::Bool(b)) => by_ord(op, a, b),
        (Operand::Char, Value::Char(a), Value::Char(b)) => by_ord(op, a, b),
        (Operand::Str, Value::Str(a), Value::Str(b)) => by_ord::<str>(op, a, b),
        (Operand::Unit, Value::Unit, Value::Unit) => by_ord(op, &(), &()),
        (Operand::Structural, ..) => return None,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integer_division_truncates_and_rem_follows_the_dividend() {
        assert_eq!(int_arith(ArithOp::Div, 7, 2).ok(), Some(3));
        assert_eq!(int_arith(ArithOp::Div, -7, 2).ok(), Some(-3));
        assert_eq!(int_arith(ArithOp::Rem, -7, 3).ok(), Some(-1));
        assert_eq!(int_arith(ArithOp::Rem, 7, -3).ok(), Some(1));
    }

    #[test]
    fn integer_faults() {
        assert!(matches!(
            int_arith(ArithOp::Add, i64::MAX, 1),
            Err(Fault::Overflow)
        ));
        assert!(matches!(
            int_arith(ArithOp::Sub, i64::MIN, 1),
            Err(Fault::Overflow)
        ));
        assert!(matches!(
            int_arith(ArithOp::Mul, i64::MAX, 2),
            Err(Fault::Overflow)
        ));
        assert!(matches!(
            int_arith(ArithOp::Div, i64::MIN, -1),
            Err(Fault::Overflow)
        ));
        assert!(matches!(
            int_arith(ArithOp::Div, 1, 0),
            Err(Fault::DivByZero)
        ));
        assert!(matches!(
            int_arith(ArithOp::Rem, 1, 0),
            Err(Fault::ModByZero)
        ));
    }

    #[test]
    fn nan_compares_like_ieee() {
        let nan = Value::Float(f64::NAN);
        assert_eq!(compare(CmpOp::Eq, Operand::Float, &nan, &nan), Some(false));
        assert_eq!(compare(CmpOp::Ne, Operand::Float, &nan, &nan), Some(true));
        assert_eq!(
            compare(CmpOp::Lt, Operand::Float, &nan, &Value::Float(1.0)),
            Some(false)
        );
    }
}
