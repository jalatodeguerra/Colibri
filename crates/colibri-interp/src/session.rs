//! Extending a running program, for `colibri repl` (ARCHITECTURE §20).
//!
//! A session grows one program: every entry is checked together with everything declared so
//! far, and the result replaces the code tables of the running [`Interp`] while the
//! module-level values stay. The caller (`Vm::session_commit`) has verified that every old
//! function, type and global keeps its id; closures are the one exception, because their
//! lifted bodies come after all declared functions, so a new function shifts them. Each
//! extension records how old ids map to new ones and a closure made earlier is looked up
//! through that map when it is called.

use std::rc::Rc;

use colibri_check::hir::{Body, GlobalId, Program, TypeDef};

use crate::ctl::PanicInfo;
use crate::eval::Interp;
use crate::fiber::Fiber;
use crate::value::{ClosureObj, Value};

/// What [`Interp::session_extend`] replaced, so that [`Interp::session_rollback`] can put it
/// back when the entry fails.
pub struct SessionUndo {
    fns: Vec<Rc<Body>>,
    types: Rc<[TypeDef]>,
    dispatch: crate::dispatch::Tables,
    inits: Vec<(GlobalId, Rc<Body>)>,
    globals_len: usize,
    /// The values of the module-level values that were re-initialised.
    replaced: Vec<(usize, Value)>,
    maps_len: usize,
}

impl Interp {
    /// The lifted body of a closure.
    pub(crate) fn closure_body(&self, c: &ClosureObj) -> Option<Rc<Body>> {
        self.closure_body_ref(c).cloned()
    }

    pub(crate) fn closure_body_ref(&self, c: &ClosureObj) -> Option<&Rc<Body>> {
        self.fns.get(self.core.closure_fn(c)?.0 as usize)
    }

    /// Puts the code of `program` (which extends the loaded one) to work: the function,
    /// type and dispatch tables are replaced, the module-level values stay, and room is made
    /// for the new ones. `closure_map[old id]` is the new id of each old function (`u32::MAX`
    /// for one that is gone). The module-level values in `reinit` are taken out (they come
    /// back with [`Interp::session_rollback`]); their initialisers run in
    /// [`Interp::session_run`].
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
            globals_len: self.globals.len(),
            replaced: reinit
                .iter()
                .filter_map(|g| {
                    let slot = self.globals.get_mut(g.0 as usize)?;
                    Some((g.0 as usize, std::mem::replace(slot, Value::Unit)))
                })
                .collect(),
            maps_len: self.core.closure_maps.len(),
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
        fiber: &mut Fiber,
        program: &Program,
        new_from: usize,
        reinit: &[GlobalId],
    ) -> Result<(), Box<PanicInfo>> {
        let outermost = self.core.begin_entry();
        if outermost {
            self.drain_root_drops();
        }
        // A session is checked with every value in dependency order (`CheckOptions`), so its
        // script initialises none itself.
        debug_assert_eq!(program.script_from, program.init_order.len());
        let inits: Vec<(GlobalId, Rc<Body>)> = program
            .init_order
            .iter()
            .filter(|g| g.0 as usize >= new_from || reinit.contains(g))
            .map(|&g| (g, Rc::clone(&program.globals[g.0 as usize].init)))
            .collect();
        let mut result = Ok(());
        for (g, body) in inits {
            match self.run_session_body(fiber, &body) {
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
            && let Some(script) = self.script_body()
        {
            match self.run_session_body(fiber, &script) {
                Ok(value) => self.release(value),
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
            globals_len,
            replaced,
            maps_len,
        } = undo;
        self.fns = fns;
        self.core.types = types;
        self.core.dispatch = dispatch;
        self.inits = inits;
        self.core.closure_maps.truncate(maps_len);
        self.set_script(old);
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
        fiber: &mut Fiber,
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
        })?;
        match shown {
            Value::Str(s) => Ok(s.to_string()),
            _ => Ok(String::new()),
        }
    }
}
