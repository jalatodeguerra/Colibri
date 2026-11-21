//! The machine as an [`Engine`]: what `colibri-core` asks of it.

use colibri_check::hir::{Builtin, FnId};
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
    /// parameters). For a `var self` method the second result is the final `self`.
    pub(crate) fn enter(
        &mut self,
        fiber: &mut VmFiber,
        chunk: std::rc::Rc<crate::bytecode::Chunk>,
        mut args: Vec<Value>,
    ) -> (Eval, Option<Value>) {
        let body = std::rc::Rc::clone(&chunk.body);
        let overflow = fiber.frames.len() >= self.max_depth;
        self.core.budget.left -= 1;
        let over_budget = self.core.budget.left < 0;
        let too_deep = !overflow
            && !over_budget
            && stack_position().abs_diff(fiber.stack_base) > self.native_stack_budget;
        if overflow || over_budget || too_deep {
            let span = Self::position(fiber).unwrap_or(body.span);
            let ctl = if overflow {
                self.overflow(fiber, span)
            } else if too_deep {
                let message = format!(
                    "stack overflow: the native stack budget of {} bytes is used up",
                    self.native_stack_budget
                );
                self.panic(fiber, span, message)
            } else {
                self.step_failure(fiber, span)
            };
            let receiver = if body.mutates_self && !args.is_empty() {
                Some(args.remove(0))
            } else {
                None
            };
            self.release_all(args);
            return (Err(ctl), receiver);
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
        fiber.frames.push(Frame {
            chunk,
            pc: 0,
            base: base as u32,
            ret: u32::MAX,
            kind: FrameKind::NativeEntry,
            noted: None,
            temps_mark: fiber.temps.len() as u32,
        });
        let result = self.run(fiber);
        // The receiver of a `var self` method goes back to the caller; every other parameter
        // is released, last first.
        let first_owned = usize::from(body.mutates_self);
        let receiver = if body.mutates_self && nparams > 0 {
            Some(unwrap_cell(std::mem::replace(
                &mut fiber.regs[base],
                Value::Unit,
            )))
        } else {
            None
        };
        for i in (first_owned..nparams).rev() {
            let v = std::mem::replace(&mut fiber.regs[base + i], Value::Unit);
            self.release(v);
        }
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
    fn position(fiber: &VmFiber) -> Option<Span> {
        fiber.frames.last().map(Frame::span)
    }

    #[inline]
    fn note_position(fiber: &mut VmFiber, span: Span) {
        if let Some(top) = fiber.frames.last_mut() {
            top.noted = Some((top.pc, span));
        }
    }

    fn stack_trace(&self, fiber: &VmFiber) -> Vec<TraceEntry> {
        fiber
            .frames
            .iter()
            .rev()
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
            _ => {
                self.release_all(args);
                Err(self.panic(fiber, span, "the bytecode VM does not support closures yet"))
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
        _fiber: &mut VmFiber,
        _ty: TypeId,
        _hook: HostHook,
        args: Vec<Value>,
    ) -> Result<Value, HostFault> {
        self.release_all(args);
        Err(HostFault::Message(
            "the bytecode VM does not support host classes yet".to_string(),
        ))
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

    fn resume_stack(&mut self, _id: u64, _slot: EngineSlot, _cancel: bool) -> Resumed {
        Resumed::Gone
    }

    fn pausing_builtin(
        &mut self,
        fiber: &mut VmFiber,
        _op: Builtin,
        _recv: &Value,
        span: Span,
    ) -> Eval {
        Err(self.panic(
            fiber,
            span,
            "the bytecode VM does not support coroutines yet",
        ))
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
    })
}

/// What a failed run ends with, for the host.
pub(crate) fn budget_panic() -> Ctl {
    Ctl::Panic(Box::new(PanicInfo {
        message: "the instruction budget is exhausted".to_string(),
        span: no_span(),
        stack: Vec::new(),
        notes: Vec::new(),
        budget: true,
    }))
}
