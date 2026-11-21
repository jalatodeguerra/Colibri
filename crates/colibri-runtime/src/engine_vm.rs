//! The bytecode VM behind the runtime's engine traits. The only file of the runtime that
//! names `colibri_vm`.
//!
//! The VM runs a growing part of the language (see its crate docs); what it does not run yet
//! is a clean panic naming the missing feature. Host functions are recorded but cannot be
//! called from Colibri code yet, the REPL session entry points and host async waits refuse
//! with a message, and a closure value cannot be called from the host.

use std::any::{Any, TypeId as AnyTypeId};
use std::io::Write;

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
use colibri_vm::{Machine, VmFiber};

use crate::engine::{ClassHooks, HostCx, HostFnImpl, RuntimeEngine};

/// The VM with its main fiber.
pub(crate) struct VmEngine {
    machine: Machine,
    fiber: VmFiber,
}

impl HostCx for VmEngine {
    fn core(&self) -> &Core {
        Engine::core(&self.machine)
    }

    fn core_mut(&mut self) -> &mut Core {
        Engine::core_mut(&mut self.machine)
    }

    fn host_depth(&self) -> u32 {
        // No host function runs on the VM yet.
        0
    }

    fn release(&mut self, value: CoreValue) {
        self.machine.release(value);
    }

    fn error_report(&mut self, error: &CoreValue) -> String {
        let (machine, fiber) = (&mut self.machine, &mut self.fiber);
        machine.error_report(fiber, error)
    }

    fn fn_param_count(&self, f: FnId) -> Option<u32> {
        self.machine.fn_param_count(f)
    }

    fn call(&mut self, f: FnId, args: Vec<CoreValue>) -> Result<CoreValue, Box<PanicInfo>> {
        let (machine, fiber) = (&mut self.machine, &mut self.fiber);
        machine.call(fiber, f, args)
    }

    fn call_function(
        &mut self,
        f: &CoreValue,
        args: Vec<CoreValue>,
    ) -> Result<CoreValue, Box<PanicInfo>> {
        let (machine, fiber) = (&mut self.machine, &mut self.fiber);
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
        let (machine, fiber) = (&self.machine, &self.fiber);
        machine.host_borrow(in_call.then_some(fiber), index, generation, any, exclusive)
    }

    fn external_wait_runs_others(&self) -> bool {
        false
    }

    fn wait_for_completion(
        &mut self,
        _op: u64,
        _token: &CancelToken,
    ) -> Result<Waited, ExternalError> {
        // Nothing else can run meanwhile on the VM yet: the caller blocks.
        Ok(Waited::Blocked)
    }
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

const NO_SESSIONS: &str = "the bytecode VM does not support REPL sessions yet";

impl RuntimeEngine for VmEngine {
    fn supports_sessions(&self) -> bool {
        false
    }

    fn load(&mut self, program: &Program) {
        self.machine.load(program);
    }

    fn patch(&mut self, program: &Program) {
        self.machine.patch(program);
    }

    fn run_module(&mut self) -> Result<(), Box<PanicInfo>> {
        self.machine.run_module(&mut self.fiber)
    }

    fn run_initialisers(&mut self) -> Result<(), Box<PanicInfo>> {
        self.machine.run_initialisers(&mut self.fiber)
    }

    fn set_host_fn(&mut self, _id: HostFnId, _imp: HostFnImpl) {
        // Recorded nowhere: a call of a host function is a clean panic on the VM yet.
    }

    fn set_host_class_hooks(&mut self, ty: TypeId, hooks: ClassHooks) {
        Engine::core_mut(&mut self.machine).set_host_hook_set(
            ty,
            colibri_core::host::HookSet {
                show: hooks.show.is_some(),
                eq: hooks.eq.is_some(),
                hash: hooks.hash.is_some(),
            },
        );
    }

    fn bind_extern(&mut self, _id: HostFnId, _name: &str, _def: &ExternDef) -> Result<(), String> {
        Err("the bytecode VM does not support `extern` functions yet".to_string())
    }

    fn session_extend(&mut self, _: &Program, _: Vec<u32>, _: &[GlobalId]) -> Box<dyn Any> {
        unreachable!("{NO_SESSIONS}: `supports_sessions` is false")
    }

    fn session_run(&mut self, _: &Program, _: usize, _: &[GlobalId]) -> Result<(), Box<PanicInfo>> {
        unreachable!("{NO_SESSIONS}: `supports_sessions` is false")
    }

    fn session_commit(&mut self, _: Box<dyn Any>) {
        unreachable!("{NO_SESSIONS}: `supports_sessions` is false")
    }

    fn session_rollback(&mut self, _: Box<dyn Any>, _: &Program) {
        unreachable!("{NO_SESSIONS}: `supports_sessions` is false")
    }

    fn session_show(&mut self, _: GlobalId) -> Result<String, Box<PanicInfo>> {
        unreachable!("{NO_SESSIONS}: `supports_sessions` is false")
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
}
