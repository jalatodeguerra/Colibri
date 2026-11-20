//! The interpreter as an [`Engine`]: what `colibri-core` asks of it, and the wrappers that keep
//! the methods `colibri-runtime` calls on [`Interp`].

use std::rc::Rc;

use colibri_check::hir::{Body, FnId};
use colibri_check::types::TypeId;
use colibri_core::engine::Engine;
use colibri_core::host::HostHook;
use colibri_core::prelude::*;
use colibri_core::release::Release;
use colibri_core::{Core, stack_position};
use colibri_syntax::{FileId, Span};

use crate::ctl::{Ctl, Eval, PanicInfo, TraceEntry};
use crate::eval::Interp;
use crate::fiber::Fiber;
use crate::heap::ClassObj;
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

    fn call_fn(&mut self, fiber: &mut Fiber, f: FnId, args: Vec<Value>) -> Eval {
        let Some(body) = self.fns.get(f.0 as usize).cloned() else {
            return Err(self.internal(fiber, Span::new(FileId(0), 0, 0), "unknown function"));
        };
        self.call_body(fiber, body, args)
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
    fn fn_body(&self, f: FnId) -> Option<&Rc<Body>> {
        self.fns.get(f.0 as usize)
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

    fn host_fault_ctl(&mut self, fiber: &mut Fiber, fault: HostFault) -> Ctl {
        match fault {
            HostFault::Panic(message) | HostFault::Message(message) => {
                let span = fiber
                    .frames
                    .last()
                    .map_or(Span::new(FileId(0), 0, 0), |frame| frame.span);
                self.panic(fiber, span, message)
            }
            HostFault::Cancel => {
                fiber.cancelling = true;
                Ctl::Cancel
            }
        }
    }

    fn group_died(&mut self, obj: &ClassObj) {
        // The inherent method of `coro.rs` (it wins over this one in a path).
        Interp::group_died(self, obj);
    }
}

impl Interp {
    /// Turns drop tracing on or off (for the test harness); a trace starts empty.
    pub fn set_trace_drops(&mut self, on: bool) {
        self.core.set_trace_drops(on);
    }

    /// The labels of the objects destroyed since tracing was turned on or last taken, in
    /// destruction order.
    pub fn take_trace(&mut self) -> Vec<String> {
        self.core.take_trace()
    }

    /// An owned value dies here. If this was the last reference to a class object, the object
    /// is destroyed and everything only it owned dies with it.
    #[inline]
    pub fn release(&mut self, value: Value) {
        Release::release(self, value);
    }

    /// Lets go of a root; the value is released (a `Drop` hook may run). A stale handle is
    /// ignored.
    pub fn unroot(&mut self, handle: colibri_core::host::RootHandle) {
        Release::unroot(self, handle);
    }

    /// Releases a value outside any call, with a budget of its own for the `Drop` hooks it
    /// may run.
    pub fn release_in_entry(&mut self, value: Value) {
        Release::release_in_entry(self, value);
    }
}
