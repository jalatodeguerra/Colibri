//! The bytecode VM behind the runtime's engine traits. The only file of the runtime that
//! names `colibri_vm`.
//!
//! The VM runs a growing part of the language (see its crate docs); what it does not run yet
//! is a clean panic naming the missing feature. A host function that waits for a background
//! operation suspends its coroutine (it runs on a native stack of its own while a coroutine
//! calls it, vm-plan §3.2), or runs the event loop on the main fiber.

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
use colibri_vm::{HostClassHooks, HostImpl, Machine, SessionUndo, VmFiber};

use crate::engine::{ClassHooks, HostCx, HostFnImpl, RuntimeEngine};

/// The VM with its main fiber.
pub(crate) struct VmEngine {
    machine: Machine,
    fiber: VmFiber,
}

/// The context of a host function the VM is calling: its machine and the fiber it runs on.
struct VmCx<'a> {
    machine: &'a mut Machine,
    fiber: &'a mut VmFiber,
}

/// Where a context finds the machine and the fiber.
trait Parts {
    fn parts(&mut self) -> (&mut Machine, &mut VmFiber);
    fn shared(&self) -> (&Machine, &VmFiber);
}

impl Parts for VmEngine {
    fn parts(&mut self) -> (&mut Machine, &mut VmFiber) {
        (&mut self.machine, &mut self.fiber)
    }
    fn shared(&self) -> (&Machine, &VmFiber) {
        (&self.machine, &self.fiber)
    }
}

impl Parts for VmCx<'_> {
    fn parts(&mut self) -> (&mut Machine, &mut VmFiber) {
        (&mut *self.machine, &mut *self.fiber)
    }
    fn shared(&self) -> (&Machine, &VmFiber) {
        (&*self.machine, &*self.fiber)
    }
}

// Two types share one body: a blanket impl over `Parts` would clash with the tree engine's.
macro_rules! host_cx {
    ($ty:ty) => {
        impl HostCx for $ty {
            fn core(&self) -> &Core {
                Engine::core(self.shared().0)
            }

            fn core_mut(&mut self) -> &mut Core {
                Engine::core_mut(self.parts().0)
            }

            fn host_depth(&self) -> u32 {
                self.shared().1.host_depth()
            }

            fn release(&mut self, value: CoreValue) {
                self.parts().0.release(value);
            }

            fn error_report(&mut self, error: &CoreValue) -> String {
                let (machine, fiber) = self.parts();
                machine.error_report(fiber, error)
            }

            fn fn_param_count(&self, f: FnId) -> Option<u32> {
                self.shared().0.fn_param_count(f)
            }

            fn call(&mut self, f: FnId, args: Vec<CoreValue>) -> Result<CoreValue, Box<PanicInfo>> {
                let (machine, fiber) = self.parts();
                machine.call(fiber, f, args)
            }

            fn call_function(
                &mut self,
                f: &CoreValue,
                args: Vec<CoreValue>,
            ) -> Result<CoreValue, Box<PanicInfo>> {
                let (machine, fiber) = self.parts();
                machine.call_function(fiber, f, args)
            }

            fn host_borrow(
                &self,
                in_call: bool,
                index: u32,
                generation: u32,
                any: AnyTypeId,
                exclusive: bool,
            ) -> Result<HostGuard, HostBorrowError> {
                let (machine, fiber) = self.shared();
                machine.host_borrow(in_call.then_some(fiber), index, generation, any, exclusive)
            }

            fn external_wait_runs_others(&self) -> bool {
                let (machine, fiber) = self.shared();
                machine.external_wait_runs_others(fiber)
            }

            fn wait_for_completion(
                &mut self,
                op: u64,
                token: &CancelToken,
            ) -> Result<Waited, ExternalError> {
                let (machine, fiber) = self.parts();
                machine.wait_for_completion(fiber, op, token)
            }
        }
    };
}

host_cx!(VmEngine);
host_cx!(VmCx<'_>);

/// A host function of the runtime as the VM installs it.
fn adapt(imp: HostFnImpl) -> HostImpl {
    Rc::new(
        move |machine: &mut Machine, fiber: &mut VmFiber, args: Vec<CoreValue>| {
            imp(&mut VmCx { machine, fiber }, args)
        },
    )
}

/// The VM as the engine of a [`crate::Vm`].
pub(crate) fn new_engine(
    out: Box<dyn Write>,
    max_call_depth: usize,
    native_stack_budget: usize,
) -> Box<dyn RuntimeEngine> {
    let mut machine = Machine::new(out, max_call_depth);
    machine.set_native_stack_budget(native_stack_budget);
    Box::new(VmEngine {
        machine,
        fiber: VmFiber::new(),
    })
}

impl RuntimeEngine for VmEngine {
    fn set_compile_all(&mut self, on: bool) {
        self.machine.set_compile_all(on);
    }

    fn load(&mut self, program: &Program) {
        self.machine.load(program);
    }

    fn patch(&mut self, program: &Program) -> Result<(), String> {
        self.machine.patch(program)
    }

    fn run_module(&mut self) -> Result<(), Box<PanicInfo>> {
        self.machine.run_module(&mut self.fiber)
    }

    fn run_module_inits(&mut self) -> Result<(), Box<PanicInfo>> {
        self.machine.run_module_inits(&mut self.fiber)
    }

    fn has_script(&self) -> bool {
        self.machine.has_script()
    }

    fn start_entry(&mut self, main: Option<FnId>) -> Result<CoreValue, Box<PanicInfo>> {
        let part = main.map_or(colibri_vm::EntryPart::Script, colibri_vm::EntryPart::Main);
        self.machine.start_entry(&mut self.fiber, part)
    }

    fn run_initialisers(&mut self) -> Result<(), Box<PanicInfo>> {
        self.machine.run_initialisers(&mut self.fiber)
    }

    fn set_host_stack_size(&mut self, bytes: usize) {
        self.machine.set_host_stack_size(bytes);
    }

    fn set_host_fn(&mut self, id: HostFnId, imp: HostFnImpl, no_await: bool) {
        self.machine.set_host_fn(id, adapt(imp));
        if no_await {
            self.machine.set_host_no_await(id);
        }
    }

    fn set_host_class_hooks(&mut self, ty: TypeId, hooks: ClassHooks) {
        self.machine.set_host_class_hooks(
            ty,
            HostClassHooks {
                show: hooks.show.map(adapt),
                eq: hooks.eq.map(adapt),
                hash: hooks.hash.map(adapt),
            },
        );
    }

    fn bind_extern(&mut self, id: HostFnId, name: &str, def: &ExternDef) -> Result<(), String> {
        self.machine.bind_extern(id, name, def)
    }

    fn session_extend(
        &mut self,
        program: &Program,
        closure_map: Vec<u32>,
        reinit: &[GlobalId],
    ) -> Box<dyn Any> {
        Box::new(self.machine.session_extend(program, closure_map, reinit))
    }

    fn session_run(
        &mut self,
        program: &Program,
        new_from: usize,
        reinit: &[GlobalId],
    ) -> Result<(), Box<PanicInfo>> {
        self.machine
            .session_run(&mut self.fiber, program, new_from, reinit)
    }

    fn session_commit(&mut self, undo: Box<dyn Any>) {
        match undo.downcast::<SessionUndo>() {
            Ok(undo) => self.machine.session_commit(*undo),
            Err(_) => unreachable!("a session undo of another engine"),
        }
    }

    fn session_rollback(&mut self, undo: Box<dyn Any>, old: &Program) {
        match undo.downcast::<SessionUndo>() {
            Ok(undo) => self.machine.session_rollback(*undo, old),
            Err(_) => unreachable!("a session undo of another engine"),
        }
    }

    fn session_show(&mut self, g: GlobalId) -> Result<String, Box<PanicInfo>> {
        self.machine.session_show(&mut self.fiber, g)
    }

    fn release_globals(&mut self) {
        self.machine.release_globals();
    }

    fn shutdown(&mut self) {
        self.machine.shutdown();
    }

    fn release_in_entry(&mut self, value: CoreValue) {
        self.machine.release_in_entry(value);
    }

    fn cancel_all_coroutines(&mut self) {
        self.machine.cancel_all_coroutines();
    }

    fn tick(&mut self, dt: f64) -> Result<(), Box<PanicInfo>> {
        self.machine.tick(dt)
    }

    fn next_wake(&mut self, poll: f64) -> Option<f64> {
        Engine::core_mut(&mut self.machine).sched.next_wake(poll)
    }

    fn take_coroutine_failure(&mut self) -> Option<Box<PanicInfo>> {
        self.machine.take_coroutine_failure()
    }

    fn find_leaks(&self) -> Vec<Leak> {
        self.machine.find_leaks()
    }

    fn start_profile(&mut self, weight: colibri_vm::ProfileWeight) {
        self.machine.start_profile(weight);
    }

    fn finish_profile(
        &mut self,
        locate: &dyn Fn(colibri_syntax::Span) -> Option<(String, u32)>,
    ) -> Option<colibri_vm::Profile> {
        self.machine.finish_profile(locate)
    }

    fn set_debugger(
        &mut self,
        debugger: Box<dyn colibri_vm::debug::Debugger>,
        locator: std::rc::Rc<dyn colibri_vm::debug::Locator>,
    ) {
        self.machine.set_debugger(debugger, locator);
    }

    fn clear_debugger(&mut self) {
        self.machine.clear_debugger();
    }

    fn set_debug_locator(&mut self, locator: std::rc::Rc<dyn colibri_vm::debug::Locator>) {
        self.machine.set_debug_locator(locator);
    }

    fn debug_set_breakpoints(
        &mut self,
        file: colibri_syntax::FileId,
        specs: &[colibri_vm::debug::BreakpointSpec],
    ) -> Vec<colibri_vm::debug::BreakpointInfo> {
        self.machine.debug_set_breakpoints(file, specs)
    }

    fn debug_break_on_panic(&mut self, on: bool) {
        self.machine.debug_break_on_panic(on);
    }

    fn debug_pause_handle(&self) -> colibri_vm::debug::PauseHandle {
        self.machine.debug_pause_handle()
    }

    fn start_rc_profile(&mut self) {
        self.machine.start_rc_profile();
    }

    fn finish_rc_profile(
        &mut self,
        locate: &dyn Fn(colibri_syntax::Span) -> Option<(String, u32)>,
    ) -> Option<colibri_vm::RcProfile> {
        self.machine.finish_rc_profile(locate)
    }
}
