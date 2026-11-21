//! Entry points: running the module, calling a function from the host, shutting down.

use colibri_check::hir::FnId;
use colibri_core::ctl::{Ctl, Eval, PanicInfo, into_panic};
use colibri_core::prelude::*;
use colibri_core::stack_position;
use colibri_core::value::Value;

use crate::engine::{bare_panic, budget_panic};
use crate::fiber::VmFiber;
use crate::machine::{Machine, Slot};

impl Machine {
    /// Runs the module-level initialisers in dependency order, then the top-level statements,
    /// which initialise the script's own `let`/`var` on their lines (DESIGN §4.2).
    pub fn run_module(&mut self, fiber: &mut VmFiber) -> Result<(), Box<PanicInfo>> {
        // The initialisers and the top-level statements share one allotment.
        let outermost = self.core.begin_entry();
        if outermost {
            self.drain_root_drops();
        }
        let result = self.run_module_parts(fiber);
        self.core.end_entry(outermost, true);
        result
    }

    fn run_module_parts(&mut self, fiber: &mut VmFiber) -> Result<(), Box<PanicInfo>> {
        let inits: Vec<_> = self.inits[..self.script_from.min(self.inits.len())].to_vec();
        for g in inits {
            let value = self.run_global_init(fiber, g.0 as usize)?;
            let old = std::mem::replace(&mut self.globals[g.0 as usize], value);
            self.release(old);
        }
        if let Some(chunk) = self.script.as_ref().map(|s| s.chunk(None, &self.env)) {
            let chunk = chunk.map_err(|m| bare_panic(&format!("internal error: {m}")))?;
            let value = self.enter_from_host(fiber, false, |this, fiber| {
                this.enter(fiber, chunk, Vec::new()).0
            })?;
            self.release(value);
        }
        Ok(())
    }

    pub(crate) fn run_global_init(
        &mut self,
        fiber: &mut VmFiber,
        g: usize,
    ) -> Result<Value, Box<PanicInfo>> {
        let slot: &Slot = &self.global_inits[g];
        let chunk = slot
            .chunk(None, &self.env)
            .map_err(|m| bare_panic(&format!("internal error: {m}")))?;
        self.enter_from_host(fiber, false, |this, fiber| {
            this.enter(fiber, chunk, Vec::new()).0
        })
    }

    /// Calls a function from the host.
    pub fn call(
        &mut self,
        fiber: &mut VmFiber,
        f: FnId,
        args: Vec<Value>,
    ) -> Result<Value, Box<PanicInfo>> {
        let chunk = match self.chunk_of(f) {
            Some(Ok(c)) => c,
            Some(Err(m)) => {
                self.release_all(args);
                return Err(bare_panic(&format!("internal error: {m}")));
            }
            None => {
                self.release_all(args);
                return Err(bare_panic("internal error: unknown function"));
            }
        };
        self.enter_from_host(fiber, true, |this, fiber| this.enter(fiber, chunk, args).0)
    }

    /// Runs `f` as a call from outside the machine; on any failure the fiber is cleaned up to
    /// what it was, so it stays usable.
    ///
    /// The outermost entry gets a fresh instruction budget; entries from a host function that
    /// the script called share it. `record` keeps the entry's step count for `steps_used`.
    pub(crate) fn enter_from_host(
        &mut self,
        fiber: &mut VmFiber,
        record: bool,
        f: impl FnOnce(&mut Self, &mut VmFiber) -> Eval,
    ) -> Result<Value, Box<PanicInfo>> {
        let (frames, temps) = (fiber.frames.len(), fiber.temps.len());
        if frames == 0 {
            fiber.stack_base = stack_position();
        }
        let outermost = self.core.begin_entry();
        if outermost {
            self.drain_root_drops();
            // What the host compared before this entry is not this script's failure.
            colibri_core::compare::take_gave_up();
        }
        let result = f(self, fiber);
        fiber.frames.truncate(frames);
        self.release_temps(fiber, temps);
        // A script that swallowed the exhaustion (through a host function that returned
        // normally) still ran out of budget.
        let swallowed = outermost && self.core.budget_was_exhausted();
        self.core.end_entry(outermost, record);
        let result = match result {
            Ok(v) if swallowed => {
                self.release(v);
                Err(budget_panic())
            }
            other => other,
        };
        match result {
            Ok(v) => Ok(v),
            Err(Ctl::Return(v)) => Ok(v),
            Err(ctl) => Err(into_panic(
                ctl,
                colibri_syntax::Span::new(colibri_syntax::FileId(0), 0, 0),
            )),
        }
    }

    /// Ends the program's life: module-level values die in reverse initialisation order
    /// (DESIGN §11.2). Safe to call twice.
    pub fn shutdown(&mut self) {
        let outermost = self.core.begin_entry();
        // Roots die first, then the module's values (EMBEDDING §3).
        self.clear_roots();
        let order: Vec<usize> = self.inits.iter().map(|g| g.0 as usize).collect();
        for g in order.into_iter().rev() {
            if let Some(slot) = self.globals.get_mut(g) {
                let v = std::mem::replace(slot, Value::Unit);
                self.release(v);
            }
        }
        // A `Drop` hook that panics while the program is shutting down has nobody to tell.
        self.core.hook_panics.clear();
        self.core.end_entry(outermost, false);
    }
}

impl Drop for Machine {
    fn drop(&mut self) {
        self.shutdown();
        // Roots and module values are gone; what is left in the host heap dies last.
        self.core.drop_host_heap();
    }
}
