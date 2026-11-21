//! The recursive evaluator (ARCHITECTURE §2).

use std::io::Write;
use std::rc::{Rc, Weak};

use colibri_check::hir::Storage;
use colibri_check::hir::{
    Arg, Arm, BinOp, Block, Body, Callee, Expr, ExprKind, FnId, GlobalId, Intrinsic, LoopId, Place,
    Program, Stmt, StmtKind, UnOp, UserOp,
};
use colibri_check::types::{Ty, TypeId};
use colibri_syntax::Span;

use colibri_core::Core;

use crate::ctl::{Ctl, Eval, PanicInfo, into_panic};
use crate::fiber::{Fiber, Frame};
use crate::format::format_value;
use crate::heap::ClassObj;
use crate::num;
use crate::ordered::OrderedMap;
use crate::place::project;
use crate::value::{CellObj, Value};
use colibri_core::prelude::*;

pub(crate) use colibri_core::stack_position;
pub(crate) use colibri_core::value::{unwrap_cell, unwrap_cell_ref};

/// The interpreter: the loaded code, the module-level state and the `colibri-core` state it
/// shares with every engine (`core`). Everything about a running computation is in the
/// [`Fiber`] passed to each method.
pub struct Interp {
    /// The buffer `eval_concat` builds a string in, kept between calls so that a string built
    /// from parts costs one allocation (the result's). Taken while in use, so a nested concat
    /// (or another coroutine's) starts with an empty one.
    concat_buf: String,
    /// Looked up at call time, so a patched function takes effect at its next call.
    pub(crate) fns: Vec<Rc<Body>>,
    pub(crate) globals: Vec<Value>,
    /// The native stacks of the coroutines that have not ended (ARCHITECTURE §16), a slab:
    /// the scheduler in `core` keeps each coroutine's slot. A stack is freed only after its
    /// coroutine ran to its end: `shutdown` runs `cancel_all_coroutines` first (from `Drop`),
    /// which resumes every suspended coroutine with a cancel, so the slab is empty by the time
    /// fields drop (`cancel_all_coroutines` asserts it in debug builds, and `Drop for Interp`
    /// forgets anything left). It is declared before `core`
    /// so that if a stack were ever left, it would drop (a forced unwind of its frames) while
    /// the state those frames point into is still alive.
    pub(crate) stacks: crate::coro::Stacks,
    /// The state shared with the engine-neutral runtime code (`colibri-core`).
    /// Fields drop in declaration order, so `core` (the host state and the host heap) goes
    /// before `host_impls` (the host function closures). Nothing observable depends on it
    /// today, because the `Vm` keeps its own reference to every host function.
    pub(crate) core: Core,
    /// Every global's initialiser, in initialisation order (`Program::init_order`).
    pub(crate) inits: Vec<(GlobalId, Rc<Body>)>,
    /// Where the globals the script initialises itself start in `inits`.
    pub(crate) script_from: usize,
    /// By global id, the name of each script value whose line has not run yet (empty when
    /// there is none). The checker rejects the uses it can see; this catches the rest (a
    /// call through an interface, a `Show` impl), DESIGN §4.2.
    pub(crate) uninit: Vec<Option<Rc<str>>>,
    /// Every global's name, by id, and whether `release_globals` emptied them all (between
    /// tests): a use then says so instead of blaming a script line that has not run.
    pub(crate) global_names: Vec<Rc<str>>,
    pub(crate) released: bool,
    script: Option<Rc<Body>>,
    max_depth: usize,
    pub(crate) native_stack_budget: usize,
    /// The host functions and class hooks the embedding program installed.
    pub(crate) host_impls: crate::host::HostImpls,
    /// The C layout of every `@repr(C)` struct of the loaded program, and the open libraries.
    pub(crate) cffi: crate::cffi::CffiState,
}

/// The default for [`Interp::set_native_stack_budget`]: small enough for a default test thread.
pub const DEFAULT_NATIVE_STACK_BUDGET: usize = 1 << 20;

impl Interp {
    pub fn new(out: Box<dyn Write>, max_call_depth: usize) -> Self {
        Interp {
            fns: Vec::new(),
            globals: Vec::new(),
            core: Core::new(out),
            inits: Vec::new(),
            script_from: 0,
            uninit: Vec::new(),
            global_names: Vec::new(),
            released: false,
            script: None,
            max_depth: max_call_depth,
            native_stack_budget: DEFAULT_NATIVE_STACK_BUDGET,
            host_impls: crate::host::HostImpls::default(),
            stacks: Default::default(),
            cffi: crate::cffi::CffiState::default(),
            concat_buf: String::new(),
        }
    }

    pub(crate) fn max_depth(&self) -> usize {
        self.max_depth
    }

    /// How many bytes of the Rust stack evaluation may use below the point the host called in.
    /// This is a backstop behind the call-depth limit: when a deep recursion would use more, it
    /// panics with "stack overflow" instead of crashing the host. Set it a little below the
    /// stack of the thread that runs the interpreter.
    pub fn set_native_stack_budget(&mut self, bytes: usize) {
        self.native_stack_budget = bytes;
    }

    /// Installs a checked program, replacing any earlier one and resetting module state (the
    /// old module-level values die first).
    pub fn load(&mut self, program: &Program) {
        self.shutdown();
        self.core.closure_maps.clear();
        self.load_host(program);
        self.fns = program.fns.iter().map(|f| Rc::clone(&f.body)).collect();
        self.globals = vec![Value::Unit; program.globals.len()];
        self.core.types = Rc::from(program.types.clone());
        self.core.dispatch = crate::dispatch::Tables::new(program);
        self.inits = program
            .init_order
            .iter()
            .map(|&g| (g, Rc::clone(&program.globals[g.0 as usize].init)))
            .collect();
        self.script_from = program.script_from;
        self.uninit = Vec::new();
        self.released = false;
        self.global_names = program
            .globals
            .iter()
            .map(|g| Rc::from(g.name.as_str()))
            .collect();
        if program.script_from < program.init_order.len() {
            self.uninit = vec![None; program.globals.len()];
            for g in &program.init_order[program.script_from..] {
                self.uninit[g.0 as usize] =
                    Some(program.globals[g.0 as usize].name.as_str().into());
            }
        }
        self.script = Some(Rc::clone(&program.script));
        self.cffi.load(program);
    }

    /// Swaps in the bodies of a re-checked program that has the same declarations as the loaded
    /// one (hot reload, ARCHITECTURE §17): every function id, global id and type id means what
    /// it meant before. Module-level values and live coroutines are untouched; a call made from
    /// now on runs the new body, while a coroutine suspended inside an old body keeps the
    /// `Rc<Body>` it holds and finishes on the old code. The caller has checked that the
    /// declarations are identical.
    pub fn patch(&mut self, program: &Program) {
        debug_assert_eq!(self.fns.len(), program.fns.len());
        self.swap_program(program);
    }

    /// Replaces every table that comes from the checked program (not the module-level values).
    pub(crate) fn swap_program(&mut self, program: &Program) {
        self.fns = program.fns.iter().map(|f| Rc::clone(&f.body)).collect();
        self.core.types = Rc::from(program.types.clone());
        self.core.dispatch = crate::dispatch::Tables::new(program);
        self.inits = program
            .init_order
            .iter()
            .map(|&g| (g, Rc::clone(&program.globals[g.0 as usize].init)))
            .collect();
        self.script_from = program.script_from;
        self.set_script(program);
        self.cffi.load(program);
        // The host's view of the host functions' (empty) bodies.
        self.core.host.bodies = program
            .host_fns
            .iter()
            .map(|h| Rc::clone(&program.fns[h.f.0 as usize].body))
            .collect();
    }

    pub(crate) fn set_script(&mut self, program: &Program) {
        self.script = Some(Rc::clone(&program.script));
    }

    pub(crate) fn script_body(&self) -> Option<Rc<Body>> {
        self.script.clone()
    }

    pub(crate) fn run_session_body(
        &mut self,
        fiber: &mut Fiber,
        body: &Rc<Body>,
    ) -> Result<Value, Box<PanicInfo>> {
        self.run_body(fiber, body)
    }

    /// Runs the module-level initialisers in dependency order, then the top-level statements,
    /// which initialise the script's own `let`/`var` on their lines (DESIGN §4.2).
    pub fn run_module(&mut self, fiber: &mut Fiber) -> Result<(), Box<PanicInfo>> {
        // The initialisers and the top-level statements share one allotment.
        let outermost = self.core.begin_entry();
        if outermost {
            self.drain_root_drops();
        }
        let result = self.run_module_parts(fiber);
        self.core.end_entry(outermost, true);
        result
    }

    /// Runs every module-level initialiser and nothing else (no top-level statements): those
    /// in dependency order, then the script's `let`/`var` in source order. The state a test
    /// starts from (ARCHITECTURE §15).
    pub fn run_initialisers(&mut self, fiber: &mut Fiber) -> Result<(), Box<PanicInfo>> {
        let outermost = self.core.begin_entry();
        if outermost {
            self.drain_root_drops();
        }
        let inits = self.inits.clone();
        let mut result = Ok(());
        for (g, body) in inits {
            match self.run_body(fiber, &body) {
                Ok(value) => {
                    let old = std::mem::replace(&mut self.globals[g.0 as usize], value);
                    self.release(old);
                    if let Some(slot) = self.uninit.get_mut(g.0 as usize) {
                        *slot = None;
                    }
                }
                Err(e) => {
                    result = Err(e);
                    break;
                }
            }
        }
        self.released = false;
        self.core.end_entry(outermost, true);
        result
    }

    fn run_module_parts(&mut self, fiber: &mut Fiber) -> Result<(), Box<PanicInfo>> {
        let mut inits = self.inits.clone();
        inits.truncate(self.script_from);
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
        self.enter_from_host(fiber, true, |this, fiber| this.call_body(fiber, body, args))
    }

    /// The module-level value `g` (for hosts and tests).
    pub fn global(&self, g: GlobalId) -> Option<&Value> {
        self.globals.get(g.0 as usize)
    }

    fn run_body(&mut self, fiber: &mut Fiber, body: &Rc<Body>) -> Result<Value, Box<PanicInfo>> {
        let body = Rc::clone(body);
        self.enter_from_host(fiber, false, |this, fiber| {
            this.call_body(fiber, body, Vec::new())
        })
    }

    /// Runs `f` as a call from outside the interpreter; on any failure the fiber is cleaned up
    /// to what it was, so it stays usable.
    ///
    /// The outermost entry gets a fresh instruction budget; entries from a host function that
    /// the script called share it. `record` keeps the entry's step count for `steps_used`.
    pub(crate) fn enter_from_host(
        &mut self,
        fiber: &mut Fiber,
        record: bool,
        f: impl FnOnce(&mut Self, &mut Fiber) -> Eval,
    ) -> Result<Value, Box<PanicInfo>> {
        let (frames, temps) = (fiber.frames.len(), fiber.temps.len());
        if frames == 0 {
            fiber.stack_base = stack_position();
        }
        let outermost = self.core.begin_entry();
        if outermost {
            self.drain_root_drops();
            // What the host compared before this entry is not this script's failure.
            crate::compare::take_gave_up();
        }
        let result = f(self, fiber);
        fiber.frames.truncate(frames);
        self.release_temps(fiber, temps);
        // A script that swallowed the exhaustion (through a host function that returned
        // normally) still ran out of budget.
        let swallowed = outermost && self.core.budget_was_exhausted();
        self.core.end_entry(outermost, record);
        let result = match result {
            Ok(v) | Err(Ctl::Return(v)) if swallowed => {
                self.release(v);
                Err(Ctl::Panic(Box::new(PanicInfo {
                    message: "the instruction budget is exhausted".to_string(),
                    span: Span::new(colibri_syntax::FileId(0), 0, 0),
                    stack: Vec::new(),
                    notes: Vec::new(),
                    budget: true,
                })))
            }
            other => other,
        };
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
            budget: false,
        })
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
        let overflow = fiber.frames.len() >= self.max_depth;
        self.core.budget.left -= 1;
        if overflow || self.core.budget.left < 0 {
            let span = fiber.frames.last().map_or(body.span, |f| f.span);
            let ctl = if overflow {
                let message = format!("stack overflow: more than {} nested calls", self.max_depth);
                self.panic(fiber, span, message)
            } else {
                self.step_failure(fiber, span)
            };
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
        let result = match self.step(fiber, stmt.span) {
            Ok(()) => self.exec_stmt_inner(fiber, stmt),
            Err(ctl) => Err(ctl),
        };
        self.release_temps(fiber, mark);
        if self.core.hook_panics.is_empty() {
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
            StmtKind::InitGlobal { global, init } => {
                // The initialiser runs as a call from this line: a panic in it names the line.
                if let Some(frame) = fiber.frames.last_mut() {
                    frame.span = stmt.span;
                }
                let value = self.call_body(fiber, Rc::clone(init), Vec::new())?;
                let old = std::mem::replace(&mut self.globals[global.0 as usize], value);
                self.release(old);
                if let Some(slot) = self.uninit.get_mut(global.0 as usize) {
                    *slot = None;
                }
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
            Some(BinOp::User(user)) => {
                // The operator of a user type runs script code, which must not change the shape
                // of the collection the place is an element of (DESIGN §11.6), like a `var self`
                // method.
                let _hold = self.hold_place(&anchor, place);
                let old = match self.get_resolved(fiber, &anchor, &place.path, span) {
                    Ok(old) => old,
                    Err(ctl) => {
                        self.release(rhs_value);
                        return Err(ctl);
                    }
                };
                self.call_user_op(fiber, user, vec![old, rhs_value], span)?
            }
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
        // A key lookup or a constant pattern of a statement gave up: stop before its stand-in
        // answer is used.
        if crate::compare::take_gave_up() {
            return Err(self.gave_up(fiber, e.span, None));
        }
        let result = match &e.kind {
            ExprKind::Lit(lit) => Ok(colibri_core::ops::literal(lit)),
            ExprKind::Read(place) => self.read_place(fiber, place, e.span),
            ExprKind::Unary(op, operand) => self.eval_unary(fiber, *op, operand, e.span),
            ExprKind::Binary(op, lhs, rhs) => self.eval_binary(fiber, *op, lhs, rhs, e.span),
            ExprKind::And(lhs, rhs) => self.eval_and(fiber, lhs, rhs),
            ExprKind::Or(lhs, rhs) => self.eval_or(fiber, lhs, rhs),
            ExprKind::Call(call) => {
                self.eval_call(fiber, &call.callee, &call.type_args, &call.args, e.span)
            }
            ExprKind::Go(call) => self.eval_go(fiber, call, e.span),
            ExprKind::GoIn(group, call) => self.eval_go_group(fiber, group, call, e.span),
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
            ExprKind::Struct { ty, fields } => self.eval_struct(fiber, *ty, fields),
            ExprKind::Object { ty, fields } => self.eval_object(fiber, *ty, fields, e.span),
            ExprKind::Variant { ty, variant, args } => {
                self.eval_variant(fiber, *ty, *variant, args)
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
        };
        // The same for this expression's own lookups: its value was computed from the stand-in.
        if crate::compare::take_gave_up() {
            return Err(self.gave_up(fiber, e.span, result.ok()));
        }
        result
    }

    /// The panic for a comparison that had no way to report that it gave up (a `Map` or `Set`
    /// key, a constant pattern); `computed` is the value that depended on its stand-in answer.
    #[cold]
    #[inline(never)]
    fn gave_up(&mut self, fiber: &mut Fiber, span: Span, computed: Option<Value>) -> Ctl {
        if let Some(value) = computed {
            self.release(value);
        }
        self.panic(
            fiber,
            span,
            "these values nest too deeply to compare".to_string(),
        )
    }

    #[inline(never)]
    fn eval_unary(&mut self, fiber: &mut Fiber, op: UnOp, operand: &Expr, span: Span) -> Eval {
        if let UnOp::User(user) = op {
            let v = self.eval_twin(fiber, operand)?;
            return self.call_user_op(fiber, user, vec![v], span);
        }
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
        if let BinOp::User(user) = op {
            return self.call_user_op(fiber, user, vec![l, r], span);
        }
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
        let mut values = Vec::with_capacity(parts.len());
        for part in parts {
            values.push(self.eval_twin(fiber, part)?);
        }
        let mut out = std::mem::take(&mut self.concat_buf);
        for v in &values {
            if let Err(ctl) = self.show_deep_into(fiber, v, span, &mut out) {
                self.release_all(values);
                out.clear();
                self.concat_buf = out;
                return Err(ctl);
            }
        }
        self.release_all(values);
        let result = Rc::from(out.as_str());
        out.clear();
        // Do not keep a huge buffer alive for a string that is built once.
        if out.capacity() <= 1 << 16 {
            self.concat_buf = out;
        }
        Ok(Value::Str(result))
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
        // A `Drop` hook of those temporaries that panicked is thrown now, before any branch.
        let held = self.fold_hook_panics(Ok(held))?;
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
            self.step(fiber, body.span)?;
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
        for (k_expr, v) in entries {
            let k = self.eval_twin(fiber, k_expr)?;
            let v = self.eval_twin(fiber, v)?;
            // A key that a user `Eq` calls equal to a stored one is that key.
            let k = self.canonical_key(
                fiber,
                || map.iter().map(|(key, _)| key.clone()).collect(),
                k,
                k_expr.span,
            )?;
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
            Value::Weak(w) => Ok(self.core.option(w.upgrade().map(Value::Obj))),
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
                // A `Drop` hook of the guard's temporaries that panicked is thrown now, before
                // the arm body (or the next arm) runs.
                let held = self.fold_hook_panics(held);
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

    // The inline field storage lives in these frames, not in the one of `eval`, which every
    // nested expression pays for.
    #[inline(never)]
    fn eval_struct(&mut self, fiber: &mut Fiber, ty: TypeId, fields: &[Expr]) -> Eval {
        let values = self.eval_fields::<colibri_core::value::StructInline>(fiber, fields)?;
        Ok(Value::strukt(ty, values))
    }

    #[inline(never)]
    fn eval_object(&mut self, fiber: &mut Fiber, ty: TypeId, fields: &[Expr], span: Span) -> Eval {
        let values = self.eval_fields::<colibri_core::value::ObjInline>(fiber, fields)?;
        let obj = ClassObj::new(ty, values);
        self.core.track_object(&obj, Some(span));
        Ok(Value::Obj(obj))
    }

    #[inline(never)]
    fn eval_variant(&mut self, fiber: &mut Fiber, ty: TypeId, variant: u32, args: &[Expr]) -> Eval {
        if args.is_empty()
            && ty == colibri_check::types::OPTION_TYPE
            && variant == colibri_check::types::VARIANT_NONE
        {
            return Ok(self.core.none());
        }
        let values = self.eval_fields::<colibri_core::value::PayloadInline>(fiber, args)?;
        Ok(Value::variant(ty, variant, values))
    }

    /// [`Interp::eval_values`] into inline storage of exactly the needed capacity.
    fn eval_fields<A: colibri_core::value::Array<Item = Value>>(
        &mut self,
        fiber: &mut Fiber,
        exprs: &[Expr],
    ) -> Eval<colibri_core::value::SmallVec<A>> {
        let mut out = colibri_core::value::SmallVec::with_capacity(exprs.len());
        for e in exprs {
            out.push(self.eval_twin(fiber, e)?);
        }
        Ok(out)
    }

    /// Evaluates expressions left to right into one vector of values. Each value also stays
    /// on the temporaries stack until the statement ends (see [`Interp::eval_twin`]).
    fn eval_values<C: FromIterator<Value>>(
        &mut self,
        fiber: &mut Fiber,
        exprs: &[Expr],
    ) -> Eval<C> {
        exprs.iter().map(|e| self.eval_twin(fiber, e)).collect()
    }

    fn eval_call(
        &mut self,
        fiber: &mut Fiber,
        callee: &Callee,
        type_args: &[Ty],
        args: &[Arg],
        span: Span,
    ) -> Eval {
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
            Callee::Fn(id) if receiver.is_some() => {
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
            Callee::Bound { iface, method } | Callee::Dyn { iface, method }
                if receiver.is_some() =>
            {
                self.call_interface(fiber, *iface, *method, values, receiver, span)
            }
            Callee::Fn(_)
            | Callee::Host(_)
            | Callee::Bound { .. }
            | Callee::Dyn { .. }
            | Callee::Value => self.call_values(fiber, callee, type_args, values, span),
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

    /// Calls `callee` with already evaluated arguments (the callee owns them): everything but
    /// a `var self` receiver. `go` runs this on the new coroutine's fiber.
    pub(crate) fn call_values(
        &mut self,
        fiber: &mut Fiber,
        callee: &Callee,
        type_args: &[Ty],
        mut values: Vec<Value>,
        span: Span,
    ) -> Eval {
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
            Callee::Host(id) => self.call_host(fiber, *id, type_args, values, span),
            Callee::Bound { iface, method } | Callee::Dyn { iface, method } => {
                self.call_interface(fiber, *iface, *method, values, None, span)
            }
            Callee::Value => {
                if values.is_empty() {
                    return Err(self.internal(fiber, span, "a call without a function"));
                }
                let f = values.remove(0);
                if let Some(frame) = fiber.frames.last_mut() {
                    frame.span = span;
                }
                let result = self.call_function_value(fiber, &f, values, span);
                // The callee is an owned value: a temporary closure (the one of `go fn() {..}()`)
                // is dropped here, and may hold the last reference to what it captured.
                self.release(f);
                result
            }
            Callee::Intrinsic(_) => Err(self.internal(fiber, span, "an intrinsic is not a call")),
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
        if let Intrinsic::Ptr(op) = intrinsic {
            return self.ptr_intrinsic(fiber, op, args, span);
        }
        if matches!(
            intrinsic,
            Intrinsic::Wait
                | Intrinsic::WaitUntil
                | Intrinsic::WaitReal
                | Intrinsic::WaitFor
                | Intrinsic::NewCoGroup
                | Intrinsic::NewSignal
                | Intrinsic::TaskTick
        ) {
            return self.coro_intrinsic(fiber, intrinsic, args, span);
        }
        if matches!(intrinsic, Intrinsic::Assert | Intrinsic::AssertEq) {
            return self.assertion(fiber, intrinsic, args, span);
        }
        if matches!(intrinsic, Intrinsic::Compare) {
            return self.compare_values(fiber, args, span);
        }
        let numeric = match intrinsic {
            Intrinsic::Convert { from, to } => args.first().map(|v| num::convert(*from, *to, v)),
            Intrinsic::NumMethod(m, n) => Some(num::method(*m, *n, &args)),
            _ => None,
        };
        if let Some(result) = numeric {
            return result.map_err(|msg| self.panic(fiber, span, msg));
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
                let _ = writeln!(self.core.out, "{text}");
                Value::Unit
            }
            Intrinsic::Show => match self.show_deep_rc(fiber, &arg, span) {
                Ok(text) => Value::Str(text),
                Err(ctl) => {
                    self.release(arg);
                    return Err(ctl);
                }
            },
            Intrinsic::Format(spec) => Value::Str(Rc::from(format_value(&arg, spec))),
            Intrinsic::Panic | Intrinsic::ErrorIs(_) | Intrinsic::ErrorAs(_) => {
                match self.error_intrinsic(fiber, intrinsic, &arg, span) {
                    Ok(v) => v,
                    Err(ctl) => {
                        self.release(arg);
                        return Err(ctl);
                    }
                }
            }
            Intrinsic::HostAlive => match &arg {
                Value::Host(r) => Value::Bool(self.core.host_alive(r.index, r.generation)),
                _ => Value::Bool(false),
            },
            Intrinsic::Builtin(_)
            | Intrinsic::Ptr(_)
            | Intrinsic::Assert
            | Intrinsic::AssertEq
            | Intrinsic::Compare
            | Intrinsic::Convert { .. }
            | Intrinsic::NumMethod(..)
            | Intrinsic::Wait
            | Intrinsic::WaitUntil
            | Intrinsic::WaitReal
            | Intrinsic::WaitFor
            | Intrinsic::NewCoGroup
            | Intrinsic::NewSignal
            | Intrinsic::TaskTick => Value::Unit,
        };
        self.release(arg);
        Ok(result)
    }

    // ---- operators ----

    /// Runs the impl of an operator interface on its operands, which the call then owns.
    #[inline(never)]
    fn call_user_op(
        &mut self,
        fiber: &mut Fiber,
        op: UserOp,
        values: Vec<Value>,
        span: Span,
    ) -> Eval {
        match op {
            UserOp::Fn(f) => self.call_values(fiber, &Callee::Fn(f), &[], values, span),
            UserOp::Bound { iface, rhs } => self.call_operator(fiber, iface, rhs, values, span),
        }
    }
}
