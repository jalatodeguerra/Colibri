//! The interpreter as an [`Engine`]: what `colibri-core` asks of it.

use colibri_check::hir::{Builtin, FnId};
use colibri_check::types::TypeId;
use colibri_core::engine::{Engine, Resumed};
use colibri_core::fiber::FiberState;
use colibri_core::host::HostHook;
use colibri_core::prelude::*;
use colibri_core::release::Release;
use colibri_core::sched::EngineSlot;
use colibri_core::{Core, stack_position};
use colibri_syntax::{FileId, Span};

use crate::ctl::{Ctl, Eval, PanicInfo, TraceEntry};
use crate::eval::Interp;
use crate::fiber::Fiber;
use crate::host::HostFault;
use crate::value::Value;

impl Engine for Interp {
    type Fiber = Fiber;

    #[inline]
    fn core(&self) -> &Core {
        &self.core
    }

    #[inline]
    fn core_mut(&mut self) -> &mut Core {
        &mut self.core
    }

    #[inline]
    fn fiber_state(fiber: &Fiber) -> &FiberState {
        &fiber.state
    }

    #[inline]
    fn fiber_state_mut(fiber: &mut Fiber) -> &mut FiberState {
        &mut fiber.state
    }

    #[inline]
    fn position(fiber: &Fiber) -> Option<Span> {
        fiber.frames.last().map(|frame| frame.span)
    }

    #[inline]
    fn note_position(fiber: &mut Fiber, span: Span) {
        if let Some(top) = fiber.frames.last_mut() {
            top.span = span;
        }
    }

    fn stack_trace(&self, fiber: &Fiber) -> Vec<TraceEntry> {
        fiber
            .frames
            .iter()
            .rev()
            .map(|fr| TraceEntry {
                function: fr.body.name.clone(),
                span: fr.span,
            })
            .collect()
    }

    #[inline]
    fn stack_base(fiber: &Fiber) -> usize {
        fiber.stack_base
    }

    #[inline]
    fn native_stack_budget(&self) -> usize {
        self.native_stack_budget
    }

    fn call_fn_inout(
        &mut self,
        fiber: &mut Fiber,
        f: FnId,
        args: Vec<Value>,
    ) -> (Eval, Option<Value>) {
        let Some(body) = self.fns.get(f.0 as usize).cloned() else {
            let ctl = self.internal(fiber, Span::new(FileId(0), 0, 0), "unknown function");
            return (Err(ctl), None);
        };
        self.call_body_inout(fiber, body, args)
    }

    #[inline]
    fn call_value(&mut self, fiber: &mut Fiber, f: &Value, args: Vec<Value>, span: Span) -> Eval {
        self.call_function_value(fiber, f, args, span)
    }

    fn call_detached(&mut self, f: FnId, args: Vec<Value>) -> Eval {
        let mut fiber = Fiber::new();
        fiber.stack_base = stack_position();
        let Some(body) = self.fns.get(f.0 as usize).cloned() else {
            return Err(Ctl::Panic(Box::new(PanicInfo {
                message: "internal error: unknown function".to_string(),
                span: Span::new(FileId(0), 0, 0),
                stack: Vec::new(),
                notes: Vec::new(),
                budget: false,
            })));
        };
        self.call_body(&mut fiber, body, args)
    }

    #[inline]
    fn fn_span(&self, f: FnId) -> Option<Span> {
        self.fns.get(f.0 as usize).map(|body| body.span)
    }

    #[inline]
    fn fn_mutates_self(&self, f: FnId) -> bool {
        self.fns
            .get(f.0 as usize)
            .is_some_and(|body| body.mutates_self)
    }

    fn call_host_hook(
        &mut self,
        fiber: &mut Fiber,
        ty: TypeId,
        hook: HostHook,
        args: Vec<Value>,
    ) -> Result<Value, HostFault> {
        let hooks = self.host_impls.class_hooks.get(&ty);
        let hook = hooks.and_then(|h| match hook {
            HostHook::Show => h.show.clone(),
            HostHook::Eq => h.eq.clone(),
            HostHook::Hash => h.hash.clone(),
        });
        match hook {
            Some(hook) => hook(self, fiber, args),
            None => Err(HostFault::Message(
                "internal error: the host class has no such hook".to_string(),
            )),
        }
    }

    #[inline]
    fn temp_mark(fiber: &Fiber) -> usize {
        fiber.temps.len()
    }

    #[inline]
    fn hold_temp(fiber: &mut Fiber, v: Value) {
        fiber.temps.push(v);
    }

    fn release_temps(&mut self, fiber: &mut Fiber, mark: usize) {
        while fiber.temps.len() > mark {
            if let Some(v) = fiber.temps.pop() {
                self.release(v);
            }
        }
    }

    fn pausing_builtin(
        &mut self,
        fiber: &mut Fiber,
        op: Builtin,
        recv: &Value,
        span: Span,
    ) -> Eval {
        self.pausing_op(fiber, op, recv, span)
    }

    #[inline]
    fn resume_stack(&mut self, _id: u64, slot: EngineSlot, cancel: bool) -> Resumed {
        self.resume_native_stack(slot, cancel)
    }
}
