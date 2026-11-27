//! The machine as an [`Engine`]: what `colibri-core` asks of it.

use colibri_check::hir::FnId;
use colibri_check::types::TypeId;
use colibri_core::engine::{Engine, Resumed};
use colibri_core::fiber::FiberState;
use colibri_core::host::{HostFault, HostHook};
use colibri_core::prelude::*;
use colibri_core::sched::EngineSlot;
use colibri_core::{Core, stack_position};
use colibri_syntax::{FileId, Span};

use colibri_core::ctl::{Ctl, Eval, PanicInfo, TraceEntry};
use colibri_core::value::{Value, unwrap_cell};

use crate::fiber::{Frame, FrameKind, VmFiber};
use crate::machine::Machine;

fn no_span() -> Span {
    Span::new(FileId(0), 0, 0)
}

impl Machine {
    /// Calls the chunk from Rust: the arguments are moved into a fresh window above every live
    /// frame, a `NativeEntry` frame runs in a nested dispatch loop, and the arguments are
    /// released afterwards, after the callee's own cleanups (where the tree-walker releases its
    /// parameters). For a `mut self` method the second result is the final `self`.
    pub(crate) fn enter(
        &mut self,
        fiber: &mut VmFiber,
        chunk: std::rc::Rc<crate::bytecode::Chunk>,
        args: Vec<Value>,
    ) -> (Eval, Option<Value>) {
        self.enter_with(fiber, chunk, args, None)
    }

    /// [`Machine::enter`] for the body of a closure: its captured variables are installed in
    /// the frame (they die with it).
    pub(crate) fn enter_with(
        &mut self,
        fiber: &mut VmFiber,
        chunk: std::rc::Rc<crate::bytecode::Chunk>,
        args: Vec<Value>,
        closure: Option<&colibri_core::closure::ClosureObj>,
    ) -> (Eval, Option<Value>) {
        self.enter_taking(fiber, chunk, args, closure, false)
    }

    /// [`Machine::enter_with`], optionally giving back the final value of the first parameter
    /// like a `mut self` method does (`take_first`), whatever the function is. `Mutex.with` uses
    /// it for the parameter its callback changes.
    pub(crate) fn enter_taking(
        &mut self,
        fiber: &mut VmFiber,
        chunk: std::rc::Rc<crate::bytecode::Chunk>,
        args: Vec<Value>,
        closure: Option<&colibri_core::closure::ClosureObj>,
        take_first: bool,
    ) -> (Eval, Option<Value>) {
        let body = std::rc::Rc::clone(&chunk.body);
        let nparams = chunk.nparams as usize;
        let base = match self.push_entry(fiber, chunk, args, closure) {
            Ok(base) => base,
            Err(refused) => return refused,
        };
        let holds_mark = fiber.holds.len();
        let result = self.run(fiber);
        // Landing pads release their holds. Only a refusal, which runs no pads, leaves any.
        debug_assert!(
            fiber.holds.len() == holds_mark || self.refused,
            "a call left a mutation guard held"
        );
        fiber.holds.truncate(holds_mark);
        self.finish_entry(
            fiber,
            base,
            nparams,
            body.mutates_self || take_first,
            result,
        )
    }

    /// Calls the function value `f` with `args`, and gives back the final value of its first
    /// parameter with the result, on every path (a function that changes its `mut` parameter
    /// leaves the change there).
    pub(crate) fn call_taking_first(
        &mut self,
        fiber: &mut VmFiber,
        f: &Value,
        args: Vec<Value>,
        span: Span,
    ) -> (Eval, Option<Value>) {
        Self::note_position(fiber, span);
        match f {
            Value::Fn(id, _) => match self.chunk_of(*id) {
                Some(Ok(chunk)) => self.enter_taking(fiber, chunk, args, None, true),
                _ => {
                    self.release_all(args);
                    (Err(self.internal(fiber, span, "unknown function")), None)
                }
            },
            Value::Closure(c) => match self.closure_chunk(c) {
                Ok(chunk) => self.enter_taking(fiber, chunk, args, Some(c), true),
                Err(msg) => {
                    self.release_all(args);
                    let ctl = self.panic(fiber, span, crate::calls::closure_message(&msg));
                    (Err(ctl), None)
                }
            },
            _ => {
                self.release_all(args);
                let ctl = self.internal(fiber, span, "a call of something that is not a function");
                (Err(ctl), None)
            }
        }
    }

    /// Pushes the `NativeEntry` frame of `chunk` over `args`, which it owns, on top of every
    /// live frame; its window starts at the result. A call that cannot be made (too deep, out
    /// of budget) releases the arguments and gives the failure, with the receiver of a
    /// `mut self` method.
    pub(crate) fn push_entry(
        &mut self,
        fiber: &mut VmFiber,
        chunk: std::rc::Rc<crate::bytecode::Chunk>,
        mut args: Vec<Value>,
        closure: Option<&colibri_core::closure::ClosureObj>,
    ) -> Result<usize, (Eval, Option<Value>)> {
        let body = std::rc::Rc::clone(&chunk.body);
        let overflow = fiber.visible_depth() >= self.max_depth;
        self.core.budget.left -= 1;
        let mut over_budget = false;
        let mut safepoint_failure = None;
        if self.core.budget.left < 0 {
            if self.core.slice_ended() {
                safepoint_failure = self.safepoint(fiber).err();
            } else {
                over_budget = true;
            }
        }
        let too_deep = !overflow
            && !over_budget
            && stack_position().abs_diff(fiber.stack_base) > self.native_stack_budget;
        if overflow || over_budget || too_deep || safepoint_failure.is_some() {
            let span = Self::position(fiber).unwrap_or(body.span);
            let ctl = if let Some(ctl) = safepoint_failure {
                ctl
            } else if overflow {
                self.overflow(fiber, span)
            } else if too_deep {
                let message = format!(
                    "stack overflow: the native stack budget of {} bytes is used up",
                    self.native_stack_budget
                );
                self.panic(fiber, span, message)
            } else {
                self.budget_exhausted(fiber, span)
            };
            let receiver = if body.mutates_self && !args.is_empty() {
                Some(args.remove(0))
            } else {
                None
            };
            self.release_all(args);
            return Err((Err(ctl), receiver));
        }
        let base = fiber.top();
        let nparams = chunk.nparams as usize;
        let need = base + chunk.nregs as usize;
        if fiber.regs.len() < need {
            fiber.regs.resize(need, Value::Unit);
        }
        for (i, v) in args.into_iter().enumerate() {
            let slot = base + i;
            if i < nparams {
                fiber.regs[slot] = v;
            } else {
                self.release(v);
            }
        }
        if let Some(c) = closure {
            self.install_captures(fiber, base, c);
        }
        fiber.hidden += usize::from(chunk.hidden);
        fiber.frames.push(Frame {
            chunk,
            pc: 0,
            base: base as u32,
            ret: u32::MAX,
            kind: FrameKind::NativeEntry,
            noted: None,
            temps_mark: fiber.temps.len() as u32,
        });
        fiber.entries += 1;
        Ok(base)
    }

    /// What follows an entry's run, however it ended: the receiver of a `mut self` method goes
    /// back to the caller and every other parameter is released, last first, after the
    /// callee's own cleanups (where the tree-walker releases its parameters). The panics of
    /// `Drop` hooks that ran meanwhile fold into the result.
    pub(crate) fn finish_entry(
        &mut self,
        fiber: &mut VmFiber,
        base: usize,
        nparams: usize,
        mutates_self: bool,
        result: Eval,
    ) -> (Eval, Option<Value>) {
        fiber.entries -= 1;
        let first_owned = usize::from(mutates_self);
        let receiver = if mutates_self && nparams > 0 {
            Some(unwrap_cell(std::mem::replace(
                &mut fiber.regs[base],
                Value::Unit,
            )))
        } else {
            None
        };
        for i in (first_owned..nparams).rev() {
            let v = std::mem::replace(&mut fiber.regs[base + i], Value::Unit);
            if self.refused {
                // Leaked on purpose, as `catch` leaks registers: no hook runs after a refusal.
                std::mem::forget(v);
            } else {
                self.release(v);
            }
        }
        // The end of the call's scope: the panics of `Drop` hooks that ran meanwhile fold in.
        let result = self.fold_hook_panics(result);
        (result, receiver)
    }
}

impl Engine for Machine {
    type Fiber = VmFiber;

    #[inline]
    fn core(&self) -> &Core {
        &self.core
    }

    #[inline]
    fn core_mut(&mut self) -> &mut Core {
        &mut self.core
    }

    #[inline]
    fn fiber_state(fiber: &VmFiber) -> &FiberState {
        &fiber.state
    }

    #[inline]
    fn fiber_state_mut(fiber: &mut VmFiber) -> &mut FiberState {
        &mut fiber.state
    }

    #[inline]
    fn safepoint(&mut self, fiber: &mut VmFiber) -> Eval<()> {
        Machine::safepoint(self, fiber)
    }

    #[inline]
    fn position(fiber: &VmFiber) -> Option<Span> {
        fiber.frames.last().map(Frame::span)
    }

    #[inline]
    fn note_position(fiber: &mut VmFiber, span: Span) {
        if let Some(top) = fiber.frames.last_mut() {
            top.noted = Some((top.pc, span));
        }
    }

    fn visible_span(fiber: &VmFiber, span: Span) -> Span {
        if !colibri_check::is_prelude_span(span) {
            return span;
        }
        fiber
            .frames
            .iter()
            .rev()
            .find(|f| !f.chunk.hidden)
            .map_or(span, Frame::span)
    }

    fn stack_trace(&self, fiber: &VmFiber) -> Vec<TraceEntry> {
        fiber
            .frames
            .iter()
            .rev()
            .filter(|f| !f.chunk.hidden)
            .map(|f| TraceEntry {
                function: f.chunk.body.name.clone(),
                span: f.span(),
            })
            .collect()
    }

    #[inline]
    fn stack_base(fiber: &VmFiber) -> usize {
        fiber.stack_base
    }

    #[inline]
    fn native_stack_budget(&self) -> usize {
        self.native_stack_budget
    }

    fn call_fn_inout(
        &mut self,
        fiber: &mut VmFiber,
        f: FnId,
        args: Vec<Value>,
    ) -> (Eval, Option<Value>) {
        match self.chunk_of(f) {
            Some(Ok(chunk)) => self.enter(fiber, chunk, args),
            Some(Err(msg)) => {
                self.release_all(args);
                let ctl = self.internal(fiber, no_span(), &format!("cannot compile: {msg}"));
                (Err(ctl), None)
            }
            None => {
                self.release_all(args);
                let ctl = self.internal(fiber, no_span(), "unknown function");
                (Err(ctl), None)
            }
        }
    }

    fn call_value(&mut self, fiber: &mut VmFiber, f: &Value, args: Vec<Value>, span: Span) -> Eval {
        match f {
            Value::Fn(id, _) => self.call_fn(fiber, *id, args),
            Value::Closure(c) => match self.closure_chunk(c) {
                Ok(chunk) => self.enter_with(fiber, chunk, args, Some(c)).0,
                Err(msg) => {
                    self.release_all(args);
                    Err(self.panic(fiber, span, crate::calls::closure_message(&msg)))
                }
            },
            _ => {
                self.release_all(args);
                Err(self.internal(fiber, span, "a call of something that is not a function"))
            }
        }
    }

    fn call_detached(&mut self, f: FnId, args: Vec<Value>) -> Eval {
        let mut fiber = VmFiber::new();
        fiber.stack_base = stack_position();
        self.call_fn(&mut fiber, f, args)
    }

    #[inline]
    fn fn_span(&self, f: FnId) -> Option<Span> {
        self.fns.get(f.0 as usize).map(|s| s.body.span)
    }

    #[inline]
    fn fn_mutates_self(&self, f: FnId) -> bool {
        self.fns
            .get(f.0 as usize)
            .is_some_and(|s| s.body.mutates_self)
    }

    fn call_host_hook(
        &mut self,
        fiber: &mut VmFiber,
        ty: TypeId,
        hook: HostHook,
        args: Vec<Value>,
    ) -> Result<Value, HostFault> {
        self.run_host_hook(fiber, ty, hook, args)
    }

    #[inline]
    fn temp_mark(fiber: &VmFiber) -> usize {
        fiber.temps.len()
    }

    #[inline]
    fn hold_temp(fiber: &mut VmFiber, v: Value) {
        fiber.temps.push(v);
    }

    fn release_temps(&mut self, fiber: &mut VmFiber, mark: usize) {
        while fiber.temps.len() > mark {
            if let Some(v) = fiber.temps.pop() {
                self.release(v);
            }
        }
    }

    fn resume_stack(&mut self, id: u64, slot: EngineSlot, cancel: bool) -> Resumed {
        self.resume_fiber(id, slot, cancel)
    }

    fn task_builtin(
        &mut self,
        fiber: &mut VmFiber,
        op: colibri_check::hir::Builtin,
        recv: &Value,
        span: Span,
    ) -> Eval {
        self.task_ops(fiber, op, recv, span)
    }

    fn channel_builtin(
        &mut self,
        fiber: &mut VmFiber,
        op: colibri_check::hir::Builtin,
        args: &mut [Value],
        span: Span,
    ) -> Eval {
        self.channel_ops(fiber, op, args, span)
    }

    fn mutex_builtin(
        &mut self,
        fiber: &mut VmFiber,
        op: colibri_check::hir::Builtin,
        args: &mut [Value],
        span: Span,
    ) -> Eval {
        self.mutex_ops(fiber, op, args, span)
    }

    fn shared_get(&mut self, fiber: &mut VmFiber, recv: &Value, span: Span) -> Eval {
        self.shared_copy(fiber, recv, span)
    }

    fn channel_next(
        &mut self,
        fiber: &mut VmFiber,
        channel: &Value,
        span: Span,
    ) -> Eval<Option<Value>> {
        self.channel_next_value(fiber, channel, span)
    }
}

/// A panic with no position, for failures before anything runs.
pub(crate) fn bare_panic(message: &str) -> Box<PanicInfo> {
    Box::new(PanicInfo {
        message: message.to_string(),
        span: no_span(),
        stack: Vec::new(),
        notes: Vec::new(),
        budget: false,
        debug_serial: 0,
        memory: None,
    })
}

/// What a run that ran out of a limit the host set ends with, for the host.
pub(crate) fn limit_panic(what: colibri_core::budget::Exhaustion) -> Ctl {
    use colibri_core::budget::Exhaustion;
    let (message, memory) = match what {
        Exhaustion::Budget => ("the instruction budget is exhausted".to_string(), None),
        Exhaustion::Memory(limit) => (colibri_core::mem::limit_message(limit), Some(limit)),
    };
    Ctl::Panic(Box::new(PanicInfo {
        message,
        span: no_span(),
        stack: Vec::new(),
        notes: Vec::new(),
        budget: true,
        debug_serial: 0,
        memory,
    }))
}
