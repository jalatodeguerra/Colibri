//! The bytecode VM behind the runtime's engine traits. The only file of the runtime that
//! names `colibri_vm`.
//!
//! The VM runs a growing part of the language (see its crate docs); what it does not run yet
//! is a clean panic naming the missing feature. The REPL session entry points refuse with a
//! message, a closure value cannot be called from the host, and a host function that waits for
//! a background operation blocks (there are no coroutines to run meanwhile).

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
use colibri_vm::{HostClassHooks, HostImpl, Machine, VmFiber};

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
                if fiber.host_depth() == 0 {
                    // A new entry of the host: an earlier refusal does not carry over.
                    machine.host_wait_refused = false;
                }
                let result = machine.call(fiber, f, args);
                // A coroutine that met the refusal ends with a panic of its own, which the
                // program may never look at: the host entry still reports the refusal.
                if result.is_ok() && fiber.host_depth() == 0 && machine.host_wait_refused {
                    return Err(host_wait_refusal());
                }
                result
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
                false
            }

            fn wait_for_completion(
                &mut self,
                op: u64,
                token: &CancelToken,
            ) -> Result<Waited, ExternalError> {
                // Suspending a coroutine for a background operation, and driving the event loop
                // while the main fiber waits, are host async (vm-plan step 12). Until then a wait
                // that would stop other coroutines from running is refused; with none alive the
                // caller just blocks.
                if self.parts().1.is_coroutine()
                    || self.core().sched.coroutine_count() > 0
                    || self.parts().0.host_wait_refused
                {
                    self.parts().0.host_wait_refused = true;
                    token.cancel();
                    self.core_mut().sched.ext().finish(op);
                    // A panic of the program, not an error value the host function could
                    // turn into something else: the refusal must show as the refusal it is.
                    std::panic::resume_unwind(Box::new(crate::ctx::RaisedPanic(
                        HOST_WAIT_REFUSAL.to_string(),
                    )));
                }
                Ok(Waited::Blocked)
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

const HOST_WAIT_REFUSAL: &str = "the bytecode VM does not support a host function that waits for a background operation while coroutines are alive yet";

fn host_wait_refusal() -> Box<PanicInfo> {
    Box::new(PanicInfo {
        message: HOST_WAIT_REFUSAL.to_string(),
        span: colibri_syntax::Span::new(colibri_syntax::FileId(0), 0, 0),
        stack: Vec::new(),
        notes: Vec::new(),
        budget: false,
    })
}

const NO_SESSIONS: &str = "the bytecode VM does not support REPL sessions yet";

fn no_sessions() -> Box<PanicInfo> {
    Box::new(PanicInfo {
        message: NO_SESSIONS.to_string(),
        span: colibri_syntax::Span::new(colibri_syntax::FileId(0), 0, 0),
        stack: Vec::new(),
        notes: Vec::new(),
        budget: false,
    })
}

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

    fn set_host_fn(&mut self, id: HostFnId, imp: HostFnImpl) {
        self.machine.set_host_fn(id, adapt(imp));
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

    // The `Vm` methods check `supports_sessions` first, so none of these is reached; they
    // still answer cleanly instead of panicking, should one be.
    fn session_extend(&mut self, _: &Program, _: Vec<u32>, _: &[GlobalId]) -> Box<dyn Any> {
        Box::new(())
    }

    fn session_run(&mut self, _: &Program, _: usize, _: &[GlobalId]) -> Result<(), Box<PanicInfo>> {
        Err(no_sessions())
    }

    fn session_commit(&mut self, _: Box<dyn Any>) {}

    fn session_rollback(&mut self, _: Box<dyn Any>, _: &Program) {}

    fn session_show(&mut self, _: GlobalId) -> Result<String, Box<PanicInfo>> {
        Err(no_sessions())
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
        self.machine.tick(dt)?;
        if self.machine.host_wait_refused {
            return Err(host_wait_refusal());
        }
        Ok(())
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
