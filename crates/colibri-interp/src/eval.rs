//! The recursive evaluator (ARCHITECTURE §2).

use std::io::Write;
use std::rc::{Rc, Weak};

use colibri_check::hir::Storage;
use colibri_check::hir::{
    Arg, ArithOp, Arm, BinOp, Block, Body, Callee, CmpOp, Expr, ExprKind, FnId, GlobalId,
    Intrinsic, Lit, LoopId, Num, Operand, Place, Program, Stmt, StmtKind, TypeDef, UnOp,
};
use colibri_check::types::IntTy;
use colibri_syntax::Span;

use crate::compare::values_identical;
use crate::ctl::{Ctl, Eval, PanicInfo, TraceEntry, into_panic};
use crate::fiber::{Fiber, Frame};
use crate::format::format_value;
use crate::heap::ClassObj;
use crate::ordered::OrderedMap;
use crate::place::project;
use crate::value::{CellObj, Value};

/// The value of a variable slot, looking through a cell.
pub(crate) fn unwrap_cell_ref(v: &Value) -> Value {
    match v {
        Value::Cell(c) => c.get(),
        other => other.clone(),
    }
}

/// The value a cell holds, for a variable that is handed back to its owner.
pub(crate) fn unwrap_cell(v: Value) -> Value {
    match v {
        Value::Cell(c) if Rc::strong_count(&c) == 1 => c.take(),
        Value::Cell(c) => c.get(),
        other => other,
    }
}

/// The interpreter: the loaded code, the module-level state and the output writer. Everything
/// about a running computation is in the [`Fiber`] passed to each method.
pub struct Interp {
    /// Looked up at call time, so a patched function takes effect at its next call.
    pub(crate) fns: Vec<Rc<Body>>,
    pub(crate) globals: Vec<Value>,
    /// The user-defined types, which `Show` and drop tracing need for names.
    pub(crate) types: Rc<[TypeDef]>,
    pub(crate) inits: Vec<(GlobalId, Rc<Body>)>,
    script: Option<Rc<Body>>,
    out: Box<dyn Write>,
    max_depth: usize,
    native_stack_budget: usize,
    /// The labels of destroyed class objects, when tracing is on (`set_trace_drops`).
    pub(crate) trace: Option<Vec<String>>,
    /// Interface implementations and the user-written methods that replace derived behaviour.
    pub(crate) dispatch: crate::dispatch::Tables,
    /// Panics in `Drop` hooks that no running statement has picked up yet.
    pub(crate) hook_panics: Vec<PanicInfo>,
    /// How many `Drop` hooks are running, one inside another.
    pub(crate) hook_depth: u32,
}

/// The default for [`Interp::set_native_stack_budget`]: small enough for a default test thread.
pub const DEFAULT_NATIVE_STACK_BUDGET: usize = 1 << 20;

/// The current position on the native stack (it only has to be comparable with another one).
pub(crate) fn stack_position() -> usize {
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
            types: Rc::from(Vec::new()),
            inits: Vec::new(),
            script: None,
            out,
            max_depth: max_call_depth,
            native_stack_budget: DEFAULT_NATIVE_STACK_BUDGET,
            trace: None,
            dispatch: crate::dispatch::Tables::default(),
            hook_panics: Vec::new(),
            hook_depth: 0,
        }
    }

    /// How many bytes of the Rust stack evaluation may use below the point the host called in.
    /// This is a backstop behind the call-depth limit: when a deep recursion would use more, it
    /// panics with "stack overflow" instead of crashing the host. Set it a little below the
    /// stack of the thread that runs the interpreter.
    pub fn set_native_stack_budget(&mut self, bytes: usize) {
        self.native_stack_budget = bytes;
    }

    pub(crate) fn native_stack_budget(&self) -> usize {
        self.native_stack_budget
    }

    /// Installs a checked program, replacing any earlier one and resetting module state (the
    /// old module-level values die first).
    pub fn load(&mut self, program: &Program) {
        self.shutdown();
        self.fns = program.fns.iter().map(|f| Rc::clone(&f.body)).collect();
        self.globals = vec![Value::Unit; program.globals.len()];
        self.types = Rc::from(program.types.clone());
        self.dispatch = crate::dispatch::Tables::new(program);
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
            let old = std::mem::replace(&mut self.globals[g.0 as usize], value);
            self.release(old);
        }
        if let Some(script) = self.script.clone() {
            let value = self.run_body(fiber, &script)?;
            self.release(value);
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
            notes: Vec::new(),
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
            notes: Vec::new(),
        }))
    }

    pub(crate) fn internal(&self, fiber: &mut Fiber, span: Span, what: &str) -> Ctl {
        self.panic(fiber, span, format!("internal error: {what}"))
    }

    // ---- calls and scopes ----

    pub(crate) fn call_body(
        &mut self,
        fiber: &mut Fiber,
        body: Rc<Body>,
        args: Vec<Value>,
    ) -> Eval {
        self.call_body_inout(fiber, body, args).0
    }

    /// Runs a call. The arguments become the callee's parameters, which it releases when it
    /// ends; the caller keeps its own references on its temporaries. For a `var self` method
    /// the second result is the final value of `self`, taken out of the frame after every
    /// other cleanup ran, for the caller to write back; it is present on every path, including
    /// a panic.
    pub(crate) fn call_body_inout(
        &mut self,
        fiber: &mut Fiber,
        body: Rc<Body>,
        args: Vec<Value>,
    ) -> (Eval, Option<Value>) {
        self.call_frame(fiber, body, args, Vec::new())
    }

    /// [`Interp::call_body_inout`] for a closure's body too: `captures` are the closure's
    /// captured values, installed in the body's capture locals (which die with the frame like
    /// parameters). A parameter that is captured and assigned lives in a fresh cell.
    pub(crate) fn call_frame(
        &mut self,
        fiber: &mut Fiber,
        body: Rc<Body>,
        args: Vec<Value>,
        captures: Vec<(colibri_check::hir::LocalId, Value)>,
    ) -> (Eval, Option<Value>) {
        if fiber.frames.len() >= self.max_depth {
            let span = fiber.frames.last().map_or(body.span, |f| f.span);
            let message = format!("stack overflow: more than {} nested calls", self.max_depth);
            let ctl = self.panic(fiber, span, message);
            let mut args = args;
            let receiver = if body.mutates_self && !args.is_empty() {
                Some(args.remove(0))
            } else {
                None
            };
            self.release_all(args);
            self.release_all(captures.into_iter().map(|(_, v)| v).collect());
            return (Err(ctl), receiver);
        }
        let mut slots = args;
        slots.resize(body.locals.len(), Value::Unit);
        for (i, slot) in slots.iter_mut().enumerate().take(body.param_count as usize) {
            if body
                .locals
                .get(i)
                .is_some_and(|l| l.storage == Storage::Cell)
            {
                *slot = Value::Cell(CellObj::new(std::mem::replace(slot, Value::Unit)));
            }
        }
        let mut frame = Frame {
            body: Rc::clone(&body),
            slots,
            span: body.span,
            cleanups: Vec::new(),
        };
        // The receiver of a `var self` method is not released here: it goes back to the caller.
        let first_owned = u32::from(body.mutates_self);
        for i in first_owned..body.param_count {
            frame
                .cleanups
                .push(crate::fiber::Cleanup::Slot(colibri_check::hir::LocalId(i)));
        }
        for (local, value) in captures {
            if let Some(slot) = frame.slots.get_mut(local.0 as usize) {
                *slot = value;
                frame.cleanups.push(crate::fiber::Cleanup::Slot(local));
            }
        }
        fiber.frames.push(frame);
        let result = self.run_block(fiber, &body.block);
        let result = self.exit_scope(fiber, 0, result);
        let receiver = match fiber.frames.last_mut() {
            Some(frame) if body.mutates_self => frame
                .slots
                .first_mut()
                .map(|s| std::mem::replace(s, Value::Unit))
                .map(unwrap_cell),
            _ => None,
        };
        fiber.frames.pop();
        let result = match result {
            Err(Ctl::Return(v)) => Ok(v),
            other => other,
        };
        (result, receiver)
    }

    /// Evaluates a block and leaves its scope on every path (normal, `break`, `return`, panic).
    pub(crate) fn eval_block(&mut self, fiber: &mut Fiber, block: &Block) -> Eval {
        let mark = Self::scope_mark(fiber);
        let result = self.run_block(fiber, block);
        self.exit_scope(fiber, mark, result)
    }

    /// Runs a block's statements and tail. The tail's temporaries die when it has its value.
    fn run_block(&mut self, fiber: &mut Fiber, block: &Block) -> Eval {
        for stmt in &block.stmts {
            self.exec_stmt(fiber, stmt)?;
        }
        match &block.tail {
            Some(tail) => {
                let mark = fiber.temps.len();
                let value = self.eval(fiber, tail)?;
                self.release_temps(fiber, mark);
                Ok(value)
            }
            None => Ok(Value::Unit),
        }
    }

    pub(crate) fn release_temps(&mut self, fiber: &mut Fiber, mark: usize) {
        while fiber.temps.len() > mark {
            if let Some(v) = fiber.temps.pop() {
                self.release(v);
            }
        }
    }

    /// Evaluates an operand onto the temporaries stack and returns where its value sits (the
    /// operand's own leftover temporaries lie below it).
    pub(crate) fn eval_temp(&mut self, fiber: &mut Fiber, e: &Expr) -> Eval<usize> {
        let v = self.eval(fiber, e)?;
        fiber.temps.push(v);
        Ok(fiber.temps.len() - 1)
    }

    /// Evaluates `e` and keeps a second reference on the temporaries stack, so the temporary
    /// dies at the end of the statement while the caller uses the returned value. Every value
    /// a Rust local holds across a nested evaluation has such a twin, so an early exit never
    /// loses the last reference to anything.
    pub(crate) fn eval_twin(&mut self, fiber: &mut Fiber, e: &Expr) -> Eval {
        let v = self.eval(fiber, e)?;
        fiber.temps.push(v.clone());
        Ok(v)
    }

    // ---- statements ----

    /// Every statement is a temporary scope: whatever it left on the temps stack dies at its
    /// end, even when it ends early.
    pub(crate) fn exec_stmt(&mut self, fiber: &mut Fiber, stmt: &Stmt) -> Eval<()> {
        let mark = fiber.temps.len();
        let result = self.exec_stmt_inner(fiber, stmt);
        self.release_temps(fiber, mark);
        if self.hook_panics.is_empty() {
            return result;
        }
        self.fold_hook_panics(result.map(|()| Value::Unit))
            .map(|_| ())
    }

    fn exec_stmt_inner(&mut self, fiber: &mut Fiber, stmt: &Stmt) -> Eval<()> {
        match &stmt.kind {
            StmtKind::Let { local, init } => {
                let value = self.eval(fiber, init)?;
                self.bind_local(fiber, *local, value);
                Ok(())
            }
            StmtKind::LetPat { pat, init } => {
                let value = self.eval(fiber, init)?;
                let mut binds = Vec::new();
                if !self.match_pat(pat, &value, &mut binds) {
                    self.release(value);
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
                self.exec_assign(fiber, place, *op, rhs, stmt.span)
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
            StmtKind::Defer(deferred) => {
                Self::push_defer(fiber, Rc::clone(deferred));
                Ok(())
            }
        }
    }

    /// `place = rhs` and `place op= rhs`: the anchor's operands are evaluated once, then the
    /// right-hand side, then the place is read (for `op=`) and written.
    fn exec_assign(
        &mut self,
        fiber: &mut Fiber,
        place: &Place,
        op: Option<BinOp>,
        rhs: &Expr,
        span: Span,
    ) -> Eval<()> {
        let anchor = self.resolve_anchor(fiber, place, span)?;
        let rhs_value = self.eval(fiber, rhs)?;
        let new = match op {
            None => rhs_value,
            Some(op) => {
                let old = match self.get_resolved(fiber, &anchor, &place.path, span) {
                    Ok(old) => old,
                    Err(ctl) => {
                        self.release(rhs_value);
                        return Err(ctl);
                    }
                };
                self.binary(fiber, op, old, rhs_value, span)?
            }
        };
        let old = self.write_resolved(fiber, &anchor, &place.path, new, true, span)?;
        self.release(old);
        Ok(())
    }

    // ---- expressions ----

    /// The evaluator's hot recursion. Each arm hands off to a function of its own, which keeps
    /// this frame small: an unoptimised build pays for every arm's locals on every nested call.
    pub(crate) fn eval(&mut self, fiber: &mut Fiber, e: &Expr) -> Eval {
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
            ExprKind::Coerce(inner, how) => {
                let value = self.eval(fiber, inner)?;
                Ok(self.coerce(value, how))
            }
            ExprKind::StrConcat(parts) => self.eval_concat(fiber, parts, e.span),
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
            ExprKind::Object { ty, fields } => {
                let values = self.eval_values(fiber, fields)?;
                Ok(Value::Obj(ClassObj::new(*ty, values)))
            }
            ExprKind::Variant { ty, variant, args } => {
                let values = self.eval_values(fiber, args)?;
                Ok(Value::variant(*ty, *variant, values))
            }
            ExprKind::Tuple(items) => {
                let values = self.eval_values(fiber, items)?;
                Ok(Value::tuple(values))
            }
            ExprKind::ListLit(items) => {
                let values = self.eval_values(fiber, items)?;
                Ok(Value::list(values))
            }
            ExprKind::MapLit(entries) => self.eval_map_lit(fiber, entries),
            ExprKind::ToWeak(inner) => self.eval_to_weak(fiber, inner, e.span),
            ExprKind::FromWeak(inner) => self.eval_from_weak(fiber, inner, e.span),
            ExprKind::Project(base, proj) => {
                // The projected-from value is a temporary: it dies at the end of the statement.
                let value = self.eval_twin(fiber, base)?;
                let part = project(&value, *proj).cloned();
                drop(value);
                match part {
                    Some(part) => Ok(part),
                    None => Err(self.internal(fiber, e.span, "ill-typed projection")),
                }
            }
            ExprKind::Match { scrutinee, arms } => self.eval_match(fiber, scrutinee, arms, e.span),
            ExprKind::FnRef { f, .. } => {
                let name = self.fns.get(f.0 as usize).map_or("?", |b| b.name.as_str());
                Ok(Value::Fn(*f, Rc::from(name)))
            }
            ExprKind::Closure(c) => self.make_closure(fiber, c, e.span),
            ExprKind::Range {
                start,
                end,
                inclusive,
            } => {
                let (Value::Int(start), Value::Int(end)) =
                    (self.eval(fiber, start)?, self.eval(fiber, end)?)
                else {
                    return Err(self.internal(fiber, e.span, "a range of non-integers"));
                };
                Ok(Value::range(start, end, *inclusive))
            }
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
        let l = self.eval_twin(fiber, lhs)?;
        let r = self.eval_twin(fiber, rhs)?;
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
    fn eval_concat(&mut self, fiber: &mut Fiber, parts: &[Expr], span: Span) -> Eval {
        let values = self.eval_values(fiber, parts)?;
        let mut out = String::new();
        for v in &values {
            match self.show_deep(fiber, v, span) {
                Ok(text) => out.push_str(&text),
                Err(ctl) => {
                    self.release_all(values);
                    return Err(ctl);
                }
            }
        }
        self.release_all(values);
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
        // The condition's temporaries are gone before either branch runs (DESIGN §11.2).
        let mark = fiber.temps.len();
        let held = self.eval(fiber, cond)?;
        self.release_temps(fiber, mark);
        let Value::Bool(taken) = held else {
            return Err(self.internal(fiber, span, "`if` condition is not a bool"));
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

    /// `{k: v, ..}`: a repeated key keeps its first position and the last value.
    #[inline(never)]
    fn eval_map_lit(&mut self, fiber: &mut Fiber, entries: &[(Expr, Expr)]) -> Eval {
        let mut map = OrderedMap::new();
        for (k, v) in entries {
            let k = self.eval_twin(fiber, k)?;
            let v = self.eval_twin(fiber, v)?;
            let (old, spare) = map.insert(k, v);
            if let Some(old) = old {
                self.release(old);
            }
            if let Some(spare) = spare {
                self.release(spare);
            }
        }
        Ok(Value::Map(crate::heap::MapObj::new(map)))
    }

    /// A strong reference (or an `Option` of one) stored weakly. The strong value dies at once
    /// if this was its only owner.
    #[inline(never)]
    fn eval_to_weak(&mut self, fiber: &mut Fiber, inner: &Expr, span: Span) -> Eval {
        let v = self.eval(fiber, inner)?;
        let weak = match &v {
            Value::Obj(rc) => Some(Rc::downgrade(rc)),
            Value::Enum(e) => match e.payload.first() {
                Some(Value::Obj(rc)) => Some(Rc::downgrade(rc)),
                None => Some(Weak::new()),
                _ => None,
            },
            _ => None,
        };
        self.release(v);
        match weak {
            Some(w) => Ok(Value::Weak(w)),
            None => Err(self.internal(fiber, span, "a weak reference to a non-object")),
        }
    }

    /// The value of a weak field as `T?`: `some` while the object lives, `none` after.
    #[inline(never)]
    fn eval_from_weak(&mut self, fiber: &mut Fiber, inner: &Expr, span: Span) -> Eval {
        match self.eval(fiber, inner)? {
            Value::Weak(w) => Ok(Value::option(w.upgrade().map(Value::Obj))),
            _ => Err(self.internal(fiber, span, "a weak read of a strong value")),
        }
    }

    /// Stores pattern bindings in their slots and registers them for release.
    pub(crate) fn bind_all(
        &mut self,
        fiber: &mut Fiber,
        binds: Vec<(colibri_check::hir::LocalId, Value)>,
    ) {
        for (local, value) in binds {
            self.bind_local(fiber, local, value);
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
        let at = self.eval_temp(fiber, scrutinee)?;
        let result = self.run_arms(fiber, at, arms, span);
        self.release_temps(fiber, mark);
        result
    }

    fn run_arms(&mut self, fiber: &mut Fiber, scrutinee: usize, arms: &[Arm], span: Span) -> Eval {
        for arm in arms {
            let value = fiber.temps[scrutinee].clone();
            let mut binds = Vec::new();
            let matched = self.match_pat(&arm.pat, &value, &mut binds);
            self.release(value);
            if !matched {
                self.release_all(binds.into_iter().map(|(_, v)| v).collect());
                continue;
            }
            let mark = Self::scope_mark(fiber);
            self.bind_all(fiber, binds);
            if let Some(guard) = &arm.guard {
                let temps = fiber.temps.len();
                let held = self.eval(fiber, guard);
                self.release_temps(fiber, temps);
                match held {
                    Ok(Value::Bool(true)) => {}
                    Ok(other) => {
                        self.release(other);
                        self.exit_scope(fiber, mark, Ok(Value::Unit))?;
                        continue;
                    }
                    Err(ctl) => return self.exit_scope(fiber, mark, Err(ctl)),
                }
            }
            let result = self.eval_block(fiber, &arm.body);
            return self.exit_scope(fiber, mark, result);
        }
        Err(self.internal(fiber, span, "no `match` arm matched"))
    }

    /// Evaluates expressions left to right into one vector of values. Each value also stays
    /// on the temporaries stack until the statement ends (see [`Interp::eval_twin`]).
    fn eval_values(&mut self, fiber: &mut Fiber, exprs: &[Expr]) -> Eval<Vec<Value>> {
        exprs.iter().map(|e| self.eval_twin(fiber, e)).collect()
    }

    fn eval_call(&mut self, fiber: &mut Fiber, callee: &Callee, args: &[Arg], span: Span) -> Eval {
        // The receiver of a `var self` method is taken last, so the other arguments still see
        // the variable it lives in. The arguments stay on the caller's temporaries, so they die
        // at the end of the caller's statement (ARCHITECTURE §6); the callee gets its own
        // references.
        let mut receiver = None;
        let mut values = Vec::with_capacity(args.len());
        for arg in args {
            match arg {
                Arg::Value(e) => values.push(self.eval_twin(fiber, e)?),
                Arg::Place(place) => receiver = Some(place),
            }
        }
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
                self.call_with_receiver(fiber, body, values, place, span)
            }
            Callee::Bound { iface, method } | Callee::Dyn { iface, method } => {
                self.call_interface(fiber, *iface, *method, values, receiver, span)
            }
            Callee::Value => {
                if values.is_empty() {
                    return Err(self.internal(fiber, span, "a call without a function"));
                }
                let f = values.remove(0);
                if let Some(frame) = fiber.frames.last_mut() {
                    frame.span = span;
                }
                self.call_function_value(fiber, &f, values, span)
            }
            Callee::Intrinsic(intrinsic) => {
                if let Some(place) = receiver {
                    let r = self.resolve_anchor(fiber, place, span)?;
                    let v = self.get_resolved(fiber, &r, &place.path, span)?;
                    values.insert(0, v);
                }
                self.intrinsic(fiber, intrinsic, values, span)
            }
        }
    }

    /// A `var self` call: the receiver is copied in (or, for a local, moved in), the callee
    /// mutates its own copy, and the final value is written back on every path. An element
    /// of a collection stays guarded for the whole call (DESIGN §11.6), so the callee cannot
    /// change the collection's shape under its own receiver.
    fn call_with_receiver(
        &mut self,
        fiber: &mut Fiber,
        body: Rc<Body>,
        mut values: Vec<Value>,
        place: &Place,
        span: Span,
    ) -> Eval {
        let anchor = match self.resolve_anchor(fiber, place, span) {
            Ok(a) => a,
            Err(ctl) => {
                self.release_all(values);
                return Err(ctl);
            }
        };
        let _hold = self.hold_place(&anchor, place);
        let current = match self.take_receiver(fiber, &anchor, &place.path, span) {
            Ok(v) => v,
            Err(ctl) => {
                self.release_all(values);
                return Err(ctl);
            }
        };
        values.insert(0, current);
        let (result, out) = self.call_body_inout(fiber, body, values);
        if let Some(updated) = out {
            match self.write_resolved(fiber, &anchor, &place.path, updated, false, span) {
                Ok(old) => self.release(old),
                Err(ctl) => {
                    self.release_outcome(result);
                    return Err(ctl);
                }
            }
        }
        result
    }

    fn intrinsic(
        &mut self,
        fiber: &mut Fiber,
        intrinsic: &Intrinsic,
        mut args: Vec<Value>,
        span: Span,
    ) -> Eval {
        if let Intrinsic::Builtin(op) = intrinsic {
            return self.builtin(fiber, *op, args, span);
        }
        let Some(arg) = args.pop() else {
            return Err(self.internal(fiber, span, "intrinsic without an argument"));
        };
        let result = match intrinsic {
            Intrinsic::Print => {
                let text = match self.show_deep(fiber, &arg, span) {
                    Ok(text) => text,
                    Err(ctl) => {
                        self.release(arg);
                        return Err(ctl);
                    }
                };
                // A closed pipe is not a Colibri error; output is best effort.
                let _ = writeln!(self.out, "{text}");
                Value::Unit
            }
            Intrinsic::Show => match self.show_deep(fiber, &arg, span) {
                Ok(text) => Value::Str(Rc::from(text)),
                Err(ctl) => {
                    self.release(arg);
                    return Err(ctl);
                }
            },
            Intrinsic::Format(spec) => Value::Str(Rc::from(format_value(&arg, spec))),
            Intrinsic::Builtin(_) => Value::Unit,
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

    /// Applies a binary operator and releases both operands.
    fn binary(&mut self, fiber: &mut Fiber, op: BinOp, l: Value, r: Value, span: Span) -> Eval {
        let result = self.binary_of(fiber, op, &l, &r, span);
        self.release(l);
        self.release(r);
        result
    }

    fn binary_of(
        &mut self,
        fiber: &mut Fiber,
        op: BinOp,
        l: &Value,
        r: &Value,
        span: Span,
    ) -> Eval {
        match (op, l, r) {
            (BinOp::Arith(op, Num::Int(IntTy::I64)), Value::Int(a), Value::Int(b)) => {
                match int_arith(op, *a, *b) {
                    Ok(n) => Ok(Value::Int(n)),
                    Err(Fault::Overflow) => Err(self.panic(fiber, span, "integer overflow")),
                    Err(Fault::DivByZero) => Err(self.panic(fiber, span, "division by zero")),
                    Err(Fault::ModByZero) => Err(self.panic(fiber, span, "modulo by zero")),
                }
            }
            (BinOp::Arith(op, Num::Float), Value::Float(a), Value::Float(b)) => {
                let (a, b) = (*a, *b);
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
                s.push_str(a);
                s.push_str(b);
                Ok(Value::Str(Rc::from(s)))
            }
            (BinOp::Is, l, r) => Ok(Value::Bool(values_identical(l, r))),
            (BinOp::Cmp(op, Operand::Structural), l, r) => {
                Ok(Value::Bool(self.structural_cmp(fiber, op, l, r, span)?))
            }
            (BinOp::Cmp(op, operand), l, r) => match compare(op, operand, l, r) {
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
