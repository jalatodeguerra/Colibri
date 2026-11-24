//! The tree-walking interpreter behind the runtime's engine traits. The only file of the
//! runtime that names `colibri_interp`.

use std::any::{Any, TypeId as AnyTypeId};
use std::io::Write;
use std::rc::Rc;

use colibri_check::hir::{ExternDef, FnId, GlobalId, HostFnId, Program};
use colibri_check::types::TypeId;
use colibri_core::Core;
use colibri_core::ctl::PanicInfo;
use colibri_core::engine::Engine;
use colibri_core::external::{CancelToken, ExternalError, Waited};
use colibri_core::hostheap::{HostBorrowError, HostGuard};
use colibri_core::leaks::Leak;
use colibri_core::prelude::*;
use colibri_core::value::Value as CoreValue;
use colibri_interp::{Fiber, HostClassHooks, HostImpl, Interp, SessionUndo};

use crate::engine::{ClassHooks, HostCx, HostFnImpl, RuntimeEngine};

#[doc(hidden)]
pub use colibri_interp::DEFAULT_NATIVE_STACK_BUDGET;

/// The interpreter with its main fiber.
pub(crate) struct TreeEngine {
    // Boxed because suspended coroutines point into it: it must never move. Dropped before
    // the fiber.
    interp: Box<Interp>,
    fiber: Fiber,
}

/// The context of a host function the interpreter is calling: its interpreter and the fiber
/// it runs on.
struct TreeCx<'a> {
    interp: &'a mut Interp,
    fiber: &'a mut Fiber,
}

/// Where a context finds the interpreter and the fiber.
trait Parts {
    fn parts(&mut self) -> (&mut Interp, &mut Fiber);
    fn shared(&self) -> (&Interp, &Fiber);
}

impl Parts for TreeEngine {
    fn parts(&mut self) -> (&mut Interp, &mut Fiber) {
        (&mut self.interp, &mut self.fiber)
    }
    fn shared(&self) -> (&Interp, &Fiber) {
        (&self.interp, &self.fiber)
    }
}

impl Parts for TreeCx<'_> {
    fn parts(&mut self) -> (&mut Interp, &mut Fiber) {
        (&mut *self.interp, &mut *self.fiber)
    }
    fn shared(&self) -> (&Interp, &Fiber) {
        (&*self.interp, &*self.fiber)
    }
}

impl<T: Parts> HostCx for T {
    fn core(&self) -> &Core {
        self.shared().0.core()
    }

    fn core_mut(&mut self) -> &mut Core {
        self.parts().0.core_mut()
    }

    fn host_depth(&self) -> u32 {
        self.shared().1.host_depth()
    }

    fn release(&mut self, value: CoreValue) {
        self.parts().0.release(value);
    }

    fn error_report(&mut self, error: &CoreValue) -> String {
        let (interp, fiber) = self.parts();
        interp.error_report(fiber, error)
    }

    fn fn_param_count(&self, f: FnId) -> Option<u32> {
        self.shared().0.fn_param_count(f)
    }

    fn call(&mut self, f: FnId, args: Vec<CoreValue>) -> Result<CoreValue, Box<PanicInfo>> {
        let (interp, fiber) = self.parts();
        interp.call(fiber, f, args)
    }

    fn call_function(
        &mut self,
        f: &CoreValue,
        args: Vec<CoreValue>,
    ) -> Result<CoreValue, Box<PanicInfo>> {
        let (interp, fiber) = self.parts();
        interp.call_function(fiber, f, args)
    }

    fn host_borrow(
        &self,
        in_call: bool,
        index: u32,
        generation: u32,
        any: AnyTypeId,
        exclusive: bool,
    ) -> Result<HostGuard, HostBorrowError> {
        let (interp, fiber) = self.shared();
        interp.host_borrow(in_call.then_some(fiber), index, generation, any, exclusive)
    }

    fn external_wait_runs_others(&self) -> bool {
        let (interp, fiber) = self.shared();
        interp.external_wait_runs_others(fiber)
    }

    fn wait_for_completion(
        &mut self,
        op: u64,
        token: &CancelToken,
    ) -> Result<Waited, ExternalError> {
        let (interp, fiber) = self.parts();
        interp.wait_for_completion(fiber, op, token)
    }
}

/// A host function of the runtime as the interpreter installs it.
fn adapt(imp: HostFnImpl) -> HostImpl {
    Rc::new(
        move |interp: &mut Interp, fiber: &mut Fiber, args: Vec<CoreValue>| {
            imp(&mut TreeCx { interp, fiber }, args)
        },
    )
}

/// The interpreter as the engine of a [`crate::Vm`].
pub(crate) fn new_engine(
    out: Box<dyn Write>,
    max_call_depth: usize,
    native_stack_budget: usize,
) -> Box<dyn RuntimeEngine> {
    let mut interp = Box::new(Interp::new(out, max_call_depth));
    interp.set_native_stack_budget(native_stack_budget);
    Box::new(TreeEngine {
        interp,
        fiber: Fiber::new(),
    })
}

impl RuntimeEngine for TreeEngine {
    fn load(&mut self, program: &Program) {
        self.interp.load(program);
    }

    fn patch(&mut self, program: &Program) -> Result<(), String> {
        self.interp.patch(program);
        Ok(())
    }

    fn run_module(&mut self) -> Result<(), Box<PanicInfo>> {
        self.interp.run_module(&mut self.fiber)
    }

    fn run_initialisers(&mut self) -> Result<(), Box<PanicInfo>> {
        self.interp.run_initialisers(&mut self.fiber)
    }

    fn set_host_fn(&mut self, id: HostFnId, imp: HostFnImpl, _no_await: bool) {
        self.interp.set_host_fn(id, adapt(imp));
    }

    fn set_host_class_hooks(&mut self, ty: TypeId, hooks: ClassHooks) {
        self.interp.set_host_class_hooks(
            ty,
            HostClassHooks {
                show: hooks.show.map(adapt),
                eq: hooks.eq.map(adapt),
                hash: hooks.hash.map(adapt),
            },
        );
    }

    fn bind_extern(&mut self, id: HostFnId, name: &str, def: &ExternDef) -> Result<(), String> {
        self.interp.bind_extern(id, name, def)
    }

    fn session_extend(
        &mut self,
        program: &Program,
        closure_map: Vec<u32>,
        reinit: &[GlobalId],
    ) -> Box<dyn Any> {
        Box::new(self.interp.session_extend(program, closure_map, reinit))
    }

    fn session_run(
        &mut self,
        program: &Program,
        new_from: usize,
        reinit: &[GlobalId],
    ) -> Result<(), Box<PanicInfo>> {
        self.interp
            .session_run(&mut self.fiber, program, new_from, reinit)
    }

    fn session_commit(&mut self, undo: Box<dyn Any>) {
        match undo.downcast::<SessionUndo>() {
            Ok(undo) => self.interp.session_commit(*undo),
            Err(_) => unreachable!("a session undo of another engine"),
        }
    }

    fn session_rollback(&mut self, undo: Box<dyn Any>, old: &Program) {
        match undo.downcast::<SessionUndo>() {
            Ok(undo) => self.interp.session_rollback(*undo, old),
            Err(_) => unreachable!("a session undo of another engine"),
        }
    }

    fn session_show(&mut self, g: GlobalId) -> Result<String, Box<PanicInfo>> {
        self.interp.session_show(&mut self.fiber, g)
    }

    fn release_globals(&mut self) {
        self.interp.release_globals();
    }

    fn shutdown(&mut self) {
        self.interp.shutdown();
    }

    fn release_in_entry(&mut self, value: CoreValue) {
        self.interp.release_in_entry(value);
    }

    fn cancel_all_coroutines(&mut self) {
        self.interp.cancel_all_coroutines();
    }

    fn tick(&mut self, dt: f64) -> Result<(), Box<PanicInfo>> {
        self.interp.tick(&mut self.fiber, dt)
    }

    fn next_wake(&mut self, poll: f64) -> Option<f64> {
        self.interp.next_wake(poll)
    }

    fn take_coroutine_failure(&mut self) -> Option<Box<PanicInfo>> {
        self.interp.take_coroutine_failure()
    }

    fn find_leaks(&self) -> Vec<Leak> {
        self.interp.find_leaks()
    }
}
