//! What the runtime needs of an engine besides running code: calls from the host, the end of
//! the program's life, replacing bodies, leaks and the coroutine scheduler's entry points.

use std::rc::Rc;

use colibri_check::hir::{FnId, Program};
use colibri_core::coro::Sched;
use colibri_core::ctl::PanicInfo;
use colibri_core::leaks::Leak;
use colibri_core::prelude::*;
use colibri_core::sched::TickError;
use colibri_core::value::Value;

use crate::engine::bare_panic;
use crate::fiber::VmFiber;
use crate::machine::{Machine, Slot};

impl Machine {
    /// How many parameters function `f` takes; `None` if there is no such function.
    pub fn fn_param_count(&self, f: FnId) -> Option<u32> {
        self.fns.get(f.0 as usize).map(|s| s.body.param_count)
    }

    /// Calls a function value from the host. Only plain functions run yet; a closure is a
    /// clean panic.
    pub fn call_function(
        &mut self,
        fiber: &mut VmFiber,
        f: &Value,
        args: Vec<Value>,
    ) -> Result<Value, Box<PanicInfo>> {
        match f {
            Value::Fn(id, _) => self.call(fiber, *id, args),
            _ => {
                self.release_all(args);
                Err(bare_panic("the bytecode VM does not support closures yet"))
            }
        }
    }

    /// Swaps in the bodies of a re-checked program with the same declarations (hot reload):
    /// a call made from now on runs the new body, and frames already running keep their chunk.
    pub fn patch(&mut self, program: &Program) {
        debug_assert_eq!(self.fns.len(), program.fns.len());
        // Only the host functions' (empty) bodies: `load_host` would also forget the host
        // classes and hooks the runtime installed after loading, which a reload keeps.
        self.core.host.bodies = program
            .host_fns
            .iter()
            .map(|h| Rc::clone(&program.fns[h.f.0 as usize].body))
            .collect();
        self.fns = program
            .fns
            .iter()
            .map(|f| Slot::new(Rc::clone(&f.body)))
            .collect();
        self.core.types = Rc::from(program.types.clone());
        self.core.dispatch = colibri_core::dispatch::Tables::new(program);
        self.inits = program.init_order.clone();
        self.global_inits = program
            .globals
            .iter()
            .map(|g| Slot::new(Rc::clone(&g.init)))
            .collect();
        self.script_from = program.script_from;
        self.script = Some(Slot::new(Rc::clone(&program.script)));
    }

    /// Runs every module-level initialiser and nothing else: those in dependency order, then
    /// the script's `let`/`var` in source order. The state a test starts from.
    pub fn run_initialisers(&mut self, fiber: &mut VmFiber) -> Result<(), Box<PanicInfo>> {
        let outermost = self.core.begin_entry();
        if outermost {
            self.drain_root_drops();
        }
        let inits = self.inits.clone();
        let mut result = Ok(());
        for g in inits {
            match self.run_global_init(fiber, g.0 as usize) {
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

    /// Releases the module-level values but keeps the host's roots. Until the initialisers
    /// run again a use of a module value is an error that says so (as far as the compiled
    /// code checks it: see the notes on test-mode global checks).
    pub fn release_globals(&mut self) {
        self.cancel_all_coroutines();
        self.core.sched.forget_failures();
        let outermost = self.core.begin_entry();
        let order: Vec<usize> = self.inits.iter().map(|g| g.0 as usize).collect();
        for g in order.iter().rev() {
            if let Some(slot) = self.globals.get_mut(*g) {
                let v = std::mem::replace(slot, Value::Unit);
                if self.refused {
                    // Leaked on purpose: releasing would run hooks after the refusal.
                    std::mem::forget(v);
                } else {
                    self.release(v);
                }
            }
        }
        self.core.hook_panics.clear();
        self.uninit = order
            .iter()
            .fold(vec![None; self.globals.len()], |mut v, g| {
                v[*g] = self.global_names.get(*g).cloned();
                v
            });
        self.released = true;
        self.core.end_entry(outermost, false);
    }

    /// Resumes every due coroutine once.
    pub fn tick(&mut self, dt: f64) -> Result<(), Box<PanicInfo>> {
        Sched::tick(self, dt)
    }

    pub fn cancel_all_coroutines(&mut self) {
        Sched::cancel_all_coroutines(self);
    }

    /// Panics that no `tick` has reported yet.
    pub fn take_coroutine_failure(&mut self) -> Option<Box<PanicInfo>> {
        match self.core.sched.take_failures() {
            Err(TickError::Failure(info)) => Some(info),
            _ => None,
        }
    }

    /// The cycles that nothing reaches.
    pub fn find_leaks(&self) -> Vec<Leak> {
        self.core
            .find_leaks(&|f| self.fns.get(f.0 as usize).map(|s| &*s.body))
    }
}

impl Machine {
    /// Borrows a host object on behalf of the host function running on `fiber` (`None`: host
    /// code between calls).
    pub fn host_borrow(
        &self,
        fiber: Option<&VmFiber>,
        index: u32,
        generation: u32,
        any: std::any::TypeId,
        exclusive: bool,
    ) -> Result<colibri_core::hostheap::HostGuard, colibri_core::hostheap::HostBorrowError> {
        let holder = fiber
            .and_then(|f| f.frames.last())
            .map(|frame| Rc::clone(&frame.chunk.body));
        self.core
            .host_borrow(holder, index, generation, any, exclusive)
    }
}

#[cfg(test)]
mod tests {
    use std::any::TypeId as AnyTypeId;
    use std::collections::HashMap;

    use colibri_check::types::TypeId;
    use colibri_core::engine::Engine;

    use super::*;

    #[test]
    fn a_patch_keeps_the_host_classes_the_runtime_installed() {
        let (program, _) = crate::source::check_source("fn main() {}").expect("checks");
        let mut machine = Machine::new(Box::new(std::io::sink()), 100);
        machine.load(&program);
        // The runtime installs these after loading, and a hot reload does not redo it.
        let classes = HashMap::from([(AnyTypeId::of::<u8>(), TypeId(40))]);
        Engine::core_mut(&mut machine).set_host_classes(classes);
        machine.patch(&program);
        assert_eq!(
            Engine::core(&machine).host_class(AnyTypeId::of::<u8>()),
            Some(TypeId(40))
        );
    }
}
