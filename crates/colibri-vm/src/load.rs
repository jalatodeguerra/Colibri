//! Entry points: running the module, calling a function from the host, shutting down.

use colibri_check::hir::{Callee, FnId};
use colibri_core::ctl::{Ctl, Eval, PanicInfo, into_panic};
use colibri_core::prelude::*;
use colibri_core::stack_position;
use colibri_core::value::Value;

use crate::engine::{bare_panic, limit_panic};
use crate::fiber::VmFiber;
use crate::machine::{Machine, Slot};

/// What [`Machine::start_entry`] starts as a coroutine.
#[derive(Clone, Copy, Debug)]
pub enum EntryPart {
    /// The top-level statements of a script.
    Script,
    /// `fn main` (the function's id).
    Main(FnId),
}

impl Machine {
    /// Runs the module-level initialisers in dependency order, then the top-level statements,
    /// which initialise the script's own `let`/`let mut` on their lines (DESIGN §4.2).
    pub fn run_module(&mut self, fiber: &mut VmFiber) -> Result<(), Box<PanicInfo>> {
        // The initialisers and the top-level statements share one allotment.
        let entry = self.core.begin_entry();
        if entry.is_outermost() {
            // A refusal ends with the run it stopped, not with the Vm.
            self.refused = false;
            self.discard_pending_refusal();
            self.drain_root_drops();
            // A time profile starts here: checking and compiling were not the program's time.
            if let Some(profiler) = self.profiler.as_mut() {
                profiler.restart_clock(self.core.sched.idle_nanos());
            }
        }
        let result = self.run_module_parts(fiber);
        self.core.end_entry(entry, true);
        result
    }

    /// The first half of [`Machine::run_module`] and nothing else: the module-level
    /// initialisers of the program's modules, in dependency order. The script's top-level
    /// statements do not run (a host that cannot block starts them with
    /// [`Machine::start_entry`]).
    pub fn run_module_inits(&mut self, fiber: &mut VmFiber) -> Result<(), Box<PanicInfo>> {
        let entry = self.core.begin_entry();
        if entry.is_outermost() {
            self.refused = false;
            self.discard_pending_refusal();
            self.drain_root_drops();
            // A time profile starts here, as in `run_module`.
            if let Some(profiler) = self.profiler.as_mut() {
                profiler.restart_clock(self.core.sched.idle_nanos());
            }
        }
        let result = self.run_inits_only(fiber);
        self.core.end_entry(entry, true);
        result
    }

    fn run_inits_only(&mut self, fiber: &mut VmFiber) -> Result<(), Box<PanicInfo>> {
        let inits: Vec<_> = self.inits[..self.script_from.min(self.inits.len())].to_vec();
        for g in inits {
            let value = self.run_global_init(fiber, g.0 as usize)?;
            let old = std::mem::replace(&mut self.globals[g.0 as usize], value);
            self.release(old);
        }
        Ok(())
    }

    /// Whether the program has top-level statements (it is a script).
    pub fn has_script(&self) -> bool {
        self.script.is_some()
    }

    /// Starts a part of the program's entry as a coroutine, and gives the handle of that
    /// coroutine (the caller owns it). It runs until its first pause before this returns. For a
    /// host that cannot block on a `wait` (the web): the top-level statements and `main` then
    /// pause like any coroutine, and the host's ticks resume them. The coroutine's panic is
    /// reported by the tick after it, like any coroutine's.
    pub fn start_entry(
        &mut self,
        fiber: &mut VmFiber,
        part: EntryPart,
    ) -> Result<Value, Box<PanicInfo>> {
        let (callee, name, script) = match part {
            EntryPart::Script => {
                let slot = self
                    .script
                    .as_ref()
                    .ok_or_else(|| bare_panic("internal error: the program has no script"))?;
                let chunk = slot
                    .chunk(None, &self.env)
                    .map_err(|m| bare_panic(&format!("internal error: {m}")))?;
                (Callee::Value, "<script>".to_string(), Some(chunk))
            }
            EntryPart::Main(f) => {
                let name = self
                    .fns
                    .get(f.0 as usize)
                    .map_or_else(|| "main".to_string(), |s| s.body.name.clone());
                (Callee::Fn(f), name, None)
            }
        };
        let span = colibri_syntax::Span::new(colibri_syntax::FileId(0), 0, 0);
        self.enter_from_host(fiber, false, |this, fiber| {
            this.start_coroutine(
                fiber,
                callee,
                Vec::new(),
                Vec::new(),
                name,
                None,
                span,
                script,
            )
        })
    }

    fn run_module_parts(&mut self, fiber: &mut VmFiber) -> Result<(), Box<PanicInfo>> {
        self.run_inits_only(fiber)?;
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
            self.note_stack_root();
        }
        let entry = self.core.begin_entry();
        if entry.is_outermost() {
            self.refused = false;
            self.discard_pending_refusal();
            self.drain_root_drops();
            // What the host compared before this entry is not this script's failure.
            colibri_core::compare::take_gave_up();
        }
        let result = f(self, fiber);
        if fiber.frames.len() > frames {
            fiber.frames.truncate(frames);
            fiber.hidden = fiber.frames.iter().filter(|f| f.chunk.hidden).count();
        }
        debug_assert!(fiber.unwinding.iter().all(|&(i, _)| (i as usize) < frames));
        // Only a bug could leave one; it must not leak into a later failure at the same index.
        fiber.unwinding.retain(|&(i, _)| (i as usize) < frames);
        self.release_temps(fiber, temps);
        // A script that swallowed the exhaustion (through a host function that returned
        // normally) still ran out of budget.
        let outermost = entry.is_outermost();
        let swallowed = if outermost {
            self.core.exhaustion()
        } else {
            None
        };
        self.core.end_entry(entry, record);
        let result = match (result, swallowed) {
            (Ok(v), Some(what)) => {
                self.release(v);
                Err(limit_panic(what))
            }
            (other, _) => other,
        };
        // A coroutine met a refusal that nothing settled (it was cancelled by this entry's last
        // act): it is this entry's error.
        let result = match (outermost, self.take_pending_refusal()) {
            (true, Some(info)) => {
                if let Ok(v) = result {
                    self.release(v);
                }
                Err(Ctl::Panic(Box::new(info)))
            }
            (_, _) => result,
        };
        match result {
            Ok(v) => Ok(v),
            Err(ctl) => Err(into_panic(
                ctl,
                colibri_syntax::Span::new(colibri_syntax::FileId(0), 0, 0),
            )),
        }
    }

    /// Ends the program's life: module-level values die in reverse initialisation order
    /// (DESIGN §11.2). Safe to call twice.
    pub fn shutdown(&mut self) {
        // Coroutines unwind first: their locals may use module-level values.
        self.cancel_all_coroutines();
        let entry = self.core.begin_entry();
        // Roots die first, then the module's values (EMBEDDING §3).
        self.clear_roots();
        let order: Vec<usize> = self.inits.iter().map(|g| g.0 as usize).collect();
        for g in order.into_iter().rev() {
            if let Some(slot) = self.globals.get_mut(g) {
                let v = std::mem::replace(slot, Value::Unit);
                if self.refused {
                    // Leaked on purpose: releasing would run hooks after the refusal.
                    std::mem::forget(v);
                } else {
                    self.release(v);
                }
            }
        }
        // The frozen copies of `Shared`s go after the module's values, which may hold handles.
        if self.refused {
            std::mem::forget(self.core.take_shared_copies());
        } else {
            self.free_shared_copies();
        }
        // A `Drop` hook that panics while the program is shutting down has nobody to tell.
        self.core.hook_panics.clear();
        self.core.end_entry(entry, false);
    }
}

impl Drop for Machine {
    fn drop(&mut self) {
        self.shutdown();
        // `shutdown` ended every coroutine: what is left is plain data.
        self.fibers.clear();
        // Roots and module values are gone; what is left in the host heap dies last.
        self.core.drop_host_heap();
    }
}
