//! What the runtime needs of an engine besides running code: calls from the host, the end of
//! the program's life, replacing bodies, leaks and the coroutine scheduler's entry points.

use std::rc::Rc;

use colibri_check::hir::{FnId, GlobalId, Program, TypeDef};
use colibri_core::coro::Sched;
use colibri_core::ctl::PanicInfo;
use colibri_core::dispatch::Tables;
use colibri_core::leaks::Leak;
use colibri_core::prelude::*;
use colibri_core::sched::TickError;
use colibri_core::value::Value;

use crate::compile::CompileEnv;
use crate::engine::bare_panic;
use crate::fiber::VmFiber;
use crate::machine::{Machine, Slot};

/// What [`Machine::session_extend`] replaced, so that [`Machine::session_rollback`] can put it
/// back when the entry fails.
pub struct SessionUndo {
    fns: Vec<Slot>,
    types: Rc<[TypeDef]>,
    dispatch: Tables,
    inits: Vec<GlobalId>,
    global_inits: Vec<Slot>,
    global_names: Vec<Rc<str>>,
    script: Option<Slot>,
    script_from: usize,
    env: CompileEnv,
    globals_len: usize,
    /// The module-level values that were re-initialised.
    replaced: Vec<(usize, Value)>,
    maps_len: usize,
    /// The first coroutine id the entry could have started.
    first_coro: u64,
}

impl Machine {
    /// How many parameters function `f` takes; `None` if there is no such function.
    pub fn fn_param_count(&self, f: FnId) -> Option<u32> {
        self.fns.get(f.0 as usize).map(|s| s.body.param_count)
    }

    /// Calls a function value from the host.
    pub fn call_function(
        &mut self,
        fiber: &mut VmFiber,
        f: &Value,
        args: Vec<Value>,
    ) -> Result<Value, Box<PanicInfo>> {
        match f {
            Value::Fn(id, _) => self.call(fiber, *id, args),
            Value::Closure(c) => {
                let chunk = match self.closure_chunk(c) {
                    Ok(chunk) => chunk,
                    Err(m) => {
                        self.release_all(args);
                        return Err(bare_panic(&crate::calls::closure_message(&m)));
                    }
                };
                let c = Rc::clone(c);
                self.enter_from_host(fiber, true, |this, fiber| {
                    this.enter_with(fiber, chunk, args, Some(&c)).0
                })
            }
            _ => {
                self.release_all(args);
                Err(bare_panic(
                    "internal error: a call of something that is not a function",
                ))
            }
        }
    }

    /// Swaps in the bodies of a re-checked program with the same declarations (hot reload):
    /// a call made from now on runs the new body, and frames already running keep their chunk.
    ///
    /// A coroutine parked in a host function keeps the id, body and type arguments of the call
    /// and is checked against this machine's host table when it resumes, so the host table must
    /// not change under it. A program whose host functions are numbered, typed or named
    /// otherwise, or that has another number of functions, is refused and nothing changes (the
    /// runtime restarts instead, which cancels the coroutines first).
    pub fn patch(&mut self, program: &Program) -> Result<(), String> {
        if self.fns.len() != program.fns.len() {
            return Err("the number of functions changed".to_string());
        }
        self.same_host_table(program)?;
        self.swap_program(program);
        Ok(())
    }

    /// Whether `program` has the host functions of the loaded one, by id.
    fn same_host_table(&self, program: &Program) -> Result<(), String> {
        let host = &self.core.host;
        let same = host.bodies.len() == program.host_fns.len()
            && host.type_param_ids.len() == program.host_fns.len()
            && program.host_fns.iter().enumerate().all(|(i, h)| {
                let body = &program.fns[h.f.0 as usize].body;
                let old = &host.bodies[i];
                host.type_param_ids[i] == h.type_params
                    && old.name == body.name
                    && old.ret == body.ret
                    && old.param_count == body.param_count
            });
        if same {
            Ok(())
        } else {
            Err("the host functions changed".to_string())
        }
    }

    /// Replaces the code tables with those of `program`; module-level values, coroutines and
    /// the host functions the runtime installed stay.
    fn swap_program(&mut self, program: &Program) {
        self.epoch = self.epoch.wrapping_add(1);
        self.env = CompileEnv::for_program(program);
        self.env.count_rc = self.rc.is_some();
        self.env.debug = self.debug.as_ref().map(|d| Rc::clone(&d.shared));
        // Only the host functions' (empty) bodies: `load_host` would also
        // forget the host classes and hooks the runtime installed after loading, which a reload
        // keeps.
        self.core.host.bodies = program
            .host_fns
            .iter()
            .map(|h| Rc::clone(&program.fns[h.f.0 as usize].body))
            .collect();
        // The type parameters of the host functions are what the result checks of parked and
        // new calls read by id: neither a patch (`same_host_table`) nor a session extension
        // (every module is registered when the session starts) changes them.
        debug_assert!(
            self.core
                .host
                .type_param_ids
                .iter()
                .eq(program.host_fns.iter().map(|h| &h.type_params)),
            "the host functions' type parameters changed under a running program"
        );
        self.cffi.load(program);
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
        self.global_names = program
            .globals
            .iter()
            .map(|g| Rc::from(g.name.as_str()))
            .collect();
        self.script_from = program.script_from;
        self.script = Some(Slot::new(Rc::clone(&program.script)));
        self.compile_everything();
    }

    // ---- REPL sessions (ARCHITECTURE §20) ----

    /// Puts the code of `program`, which extends the loaded one, to work: the code tables are
    /// replaced (frames and closures that exist keep their chunks, and a closure finds its body
    /// through `closure_map`), the module-level values stay, and room is made for the new ones.
    /// The values in `reinit` are taken out; [`Machine::session_rollback`] returns them.
    pub fn session_extend(
        &mut self,
        program: &Program,
        closure_map: Vec<u32>,
        reinit: &[GlobalId],
    ) -> SessionUndo {
        let undo = SessionUndo {
            fns: std::mem::take(&mut self.fns),
            types: Rc::clone(&self.core.types),
            dispatch: std::mem::take(&mut self.core.dispatch),
            inits: std::mem::take(&mut self.inits),
            global_inits: std::mem::take(&mut self.global_inits),
            global_names: std::mem::take(&mut self.global_names),
            script: self.script.take(),
            script_from: self.script_from,
            env: std::mem::take(&mut self.env),
            globals_len: self.globals.len(),
            replaced: reinit
                .iter()
                .filter_map(|g| {
                    let slot = self.globals.get_mut(g.0 as usize)?;
                    Some((g.0 as usize, std::mem::replace(slot, Value::Unit)))
                })
                .collect(),
            maps_len: self.core.closure_maps.len(),
            first_coro: self.core.sched.next_id(),
        };
        self.core.closure_maps.push(closure_map);
        self.globals.resize(program.globals.len(), Value::Unit);
        self.swap_program(program);
        undo
    }

    /// Runs the initialisers of the module-level values from `new_from` on and of `reinit`,
    /// in dependency order, then the top-level statements of `program`.
    pub fn session_run(
        &mut self,
        fiber: &mut VmFiber,
        program: &Program,
        new_from: usize,
        reinit: &[GlobalId],
    ) -> Result<(), Box<PanicInfo>> {
        let outermost = self.core.begin_entry();
        if outermost {
            self.refused = false;
            self.discard_pending_refusal();
            self.drain_root_drops();
        }
        // A session is checked with every value in dependency order, so its script initialises
        // none itself.
        debug_assert_eq!(program.script_from, program.init_order.len());
        let order: Vec<GlobalId> = program
            .init_order
            .iter()
            .copied()
            .filter(|g| g.0 as usize >= new_from || reinit.contains(g))
            .collect();
        let mut result = Ok(());
        for g in order {
            match self.run_global_init(fiber, g.0 as usize) {
                Ok(value) => {
                    let old = std::mem::replace(&mut self.globals[g.0 as usize], value);
                    self.release(old);
                }
                Err(e) => {
                    result = Err(e);
                    break;
                }
            }
        }
        if result.is_ok()
            && let Some(slot) = &self.script
        {
            match slot
                .chunk(None, &self.env)
                .map_err(|m| bare_panic(&format!("internal error: {m}")))
            {
                Ok(chunk) => {
                    match self.enter_from_host(fiber, false, |this, fiber| {
                        this.enter(fiber, chunk, Vec::new()).0
                    }) {
                        Ok(value) => self.release(value),
                        Err(e) => result = Err(e),
                    }
                }
                Err(e) => result = Err(e),
            }
        }
        self.core.end_entry(outermost, true);
        result
    }

    /// The entry succeeded: the values it replaced are released.
    pub fn session_commit(&mut self, undo: SessionUndo) {
        let outermost = self.core.begin_entry();
        for (_, value) in undo.replaced {
            self.release(value);
        }
        self.core.end_entry(outermost, false);
    }

    /// The entry failed: the code of `old` (the program before it) is back, the values it
    /// created are released and the ones it replaced return. What the entry did to values
    /// that already existed (`x += 1` before a panic) stays.
    pub fn session_rollback(&mut self, undo: SessionUndo, old: &Program) {
        let outermost = self.core.begin_entry();
        let SessionUndo {
            fns,
            types,
            dispatch,
            inits,
            global_inits,
            global_names,
            script,
            script_from,
            env,
            globals_len,
            replaced,
            maps_len,
            first_coro,
        } = undo;
        // The coroutines the entry started may run code that goes away with it: they end here,
        // while its code is still in place, so their `defer`s and drops run.
        self.note_stack_root();
        Sched::cancel_coroutines_from(self, first_coro);
        self.core
            .retire_extension_closures(maps_len, self.fns.len());
        // Chunks made under the failed extension are stale, whatever the cache says.
        self.epoch = self.epoch.wrapping_add(1);
        self.fns = fns;
        self.core.types = types;
        self.core.dispatch = dispatch;
        self.inits = inits;
        self.global_inits = global_inits;
        self.global_names = global_names;
        self.script = script;
        self.script_from = script_from;
        self.env = env;
        self.cffi.load(old);
        self.core.host.bodies = old
            .host_fns
            .iter()
            .map(|h| Rc::clone(&old.fns[h.f.0 as usize].body))
            .collect();
        let extra: Vec<Value> = self.globals.drain(globals_len..).collect();
        for value in extra {
            self.release(value);
        }
        for (slot, value) in replaced {
            let failed = std::mem::replace(&mut self.globals[slot], value);
            self.release(failed);
        }
        self.core.end_entry(outermost, false);
    }

    /// The `Show` text of module-level value `g`, honouring a user-written `impl Show`.
    pub fn session_show(
        &mut self,
        fiber: &mut VmFiber,
        g: GlobalId,
    ) -> Result<String, Box<PanicInfo>> {
        let value = self
            .globals
            .get(g.0 as usize)
            .cloned()
            .unwrap_or(Value::Unit);
        let span = colibri_syntax::Span::new(colibri_syntax::FileId(0), 0, 0);
        let shown = self.enter_from_host(fiber, false, |this, fiber| {
            let text = this.show_deep(fiber, &value, span)?;
            Ok(Value::Str(text.into()))
        });
        self.release(value);
        match shown? {
            Value::Str(s) => Ok(s.to_string()),
            _ => Ok(String::new()),
        }
    }

    /// Runs every module-level initialiser and nothing else: those in dependency order, then
    /// the script's `let`/`let mut` in source order. The state a test starts from.
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
        self.note_stack_root();
        let result = Sched::tick(self, dt);
        // The refusal is the failure `tick` returns; it is not for a later entry.
        self.take_pending_refusal();
        result
    }

    /// How long until the next coroutine needs a tick (see `Scheduler::next_wake`).
    pub fn next_wake(&mut self, poll: f64) -> Option<f64> {
        self.core.sched.next_wake(poll)
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
        machine.patch(&program).expect("same declarations");
        assert_eq!(
            Engine::core(&machine).host_class(AnyTypeId::of::<u8>()),
            Some(TypeId(40))
        );
    }
}
