//! The recursive evaluator (ARCHITECTURE §2).

use std::io::Write;
use std::rc::Rc;

use colibri_check::hir::{
    Anchor, Arg, ArithOp, BinOp, Block, Body, Callee, CmpOp, Expr, ExprKind, FnId, GlobalId,
    Intrinsic, Lit, LoopId, Num, Operand, Place, Program, Stmt, StmtKind, UnOp,
};
use colibri_check::types::IntTy;
use colibri_syntax::Span;

use crate::ctl::{Ctl, Eval, PanicInfo, TraceEntry, into_panic};
use crate::fiber::{Fiber, Frame};
use crate::format::format_value;
use crate::value::{Value, show};

/// The interpreter: the loaded code, the module-level state and the output writer. Everything
/// about a running computation is in the [`Fiber`] passed to each method.
pub struct Interp {
    /// Looked up at call time, so a patched function takes effect at its next call.
    fns: Vec<Rc<Body>>,
    globals: Vec<Value>,
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
    fn panic(&self, fiber: &mut Fiber, span: Span, message: impl Into<String>) -> Ctl {
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

    fn internal(&self, fiber: &mut Fiber, span: Span, what: &str) -> Ctl {
        self.panic(fiber, span, format!("internal error: {what}"))
    }

    // ---- calls and scopes ----

    fn call_body(&mut self, fiber: &mut Fiber, body: Rc<Body>, args: Vec<Value>) -> Eval {
        if fiber.frames.len() >= self.max_depth {
            let span = fiber.frames.last().map_or(body.span, |f| f.span);
            let message = format!("stack overflow: more than {} nested calls", self.max_depth);
            return Err(self.panic(fiber, span, message));
        }
        let mut slots = args;
        slots.resize(body.locals.len(), Value::Unit);
        fiber.frames.push(Frame {
            body: Rc::clone(&body),
            slots,
            span: body.span,
        });
        let result = self.eval_block(fiber, &body.block);
        fiber.frames.pop();
        match result {
            Err(Ctl::Return(v)) => Ok(v),
            other => other,
        }
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

    // ---- places ----

    fn read_place(&mut self, fiber: &mut Fiber, place: &Place, span: Span) -> Eval {
        if !place.path.is_empty() {
            return Err(self.internal(fiber, span, "projections are not supported yet"));
        }
        match &place.anchor {
            Anchor::Local(id) => match fiber.frames.last() {
                Some(frame) => Ok(frame.slots[id.0 as usize].clone()),
                None => Err(self.internal(fiber, span, "no frame")),
            },
            Anchor::Global(id) => Ok(self.globals[id.0 as usize].clone()),
            _ => Err(self.internal(fiber, span, "this place is not supported yet")),
        }
    }

    /// Stores `value` and returns the value it replaced.
    fn write_place(&mut self, fiber: &mut Fiber, place: &Place, value: Value, span: Span) -> Eval {
        if !place.path.is_empty() {
            return Err(self.internal(fiber, span, "projections are not supported yet"));
        }
        match &place.anchor {
            Anchor::Local(id) => match fiber.frames.last_mut() {
                Some(frame) => Ok(std::mem::replace(&mut frame.slots[id.0 as usize], value)),
                None => Err(self.internal(fiber, span, "no frame")),
            },
            Anchor::Global(id) => Ok(std::mem::replace(&mut self.globals[id.0 as usize], value)),
            _ => Err(self.internal(fiber, span, "this place is not supported yet")),
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
            out.push_str(&show(&v));
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

    fn eval_call(&mut self, fiber: &mut Fiber, callee: &Callee, args: &[Arg], span: Span) -> Eval {
        let base = fiber.temps.len();
        for arg in args {
            match arg {
                Arg::Value(e) => self.eval_temp(fiber, e)?,
                Arg::Place(_) => return Err(self.internal(fiber, span, "place arguments")),
            }
        }
        let values = fiber.temps.split_off(base);
        match callee {
            Callee::Fn(id) => {
                let Some(body) = self.fns.get(id.0 as usize).cloned() else {
                    return Err(self.internal(fiber, span, "unknown function"));
                };
                if let Some(frame) = fiber.frames.last_mut() {
                    frame.span = span;
                }
                self.call_body(fiber, body, values)
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
                let _ = writeln!(self.out, "{}", show(&arg));
                Value::Unit
            }
            Intrinsic::Show => Value::Str(Rc::from(show(&arg).as_ref())),
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
