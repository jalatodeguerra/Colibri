//! The end of the program's life: module-level values die in reverse initialisation order
//! (DESIGN §11.2). The release of values itself is `colibri_core::release`.

use colibri_core::release::Release;

use crate::eval::Interp;
use crate::value::Value;

impl Interp {
    /// Releases the module-level values in reverse initialisation order and leaves the slots
    /// empty, like [`Interp::shutdown`] but keeping the host's roots (the program stays loaded).
    /// Used between tests: a `Drop` hook that panics here has nobody to tell.
    pub fn release_globals(&mut self) {
        self.cancel_all_coroutines();
        self.sched_forget_failures();
        let outermost = self.core.begin_entry();
        let order: Vec<usize> = self.inits.iter().map(|(g, _)| g.0 as usize).collect();
        for g in order.into_iter().rev() {
            if let Some(slot) = self.globals.get_mut(g) {
                let v = std::mem::replace(slot, Value::Unit);
                self.release(v);
            }
        }
        self.core.hook_panics.clear();
        // Until the initialisers run again, a use of a module value is an error that says so.
        self.uninit = self.inits.iter().map(|(g, _)| g.0 as usize).fold(
            vec![None; self.globals.len()],
            |mut v, g| {
                v[g] = self.global_names.get(g).cloned();
                v
            },
        );
        self.released = true;
        self.core.end_entry(outermost, false);
    }

    /// Ends the program's life: module-level values die in reverse initialisation order
    /// (DESIGN §11.2). Safe to call twice.
    pub fn shutdown(&mut self) {
        // Coroutines unwind first: their locals may use module-level values.
        self.cancel_all_coroutines();
        let outermost = self.core.begin_entry();
        // Roots die first, then the module's values (EMBEDDING §3).
        self.clear_roots();
        let order: Vec<usize> = self.inits.iter().map(|(g, _)| g.0 as usize).collect();
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

impl Drop for Interp {
    fn drop(&mut self) {
        self.shutdown();
        debug_assert!(
            self.stacks.is_empty(),
            "a coroutine stack outlived the shutdown that runs every coroutine to its end"
        );
        // Roots and module values are gone; what is left in the host heap dies last, in
        // index order (EMBEDDING §3).
        self.core.drop_host_heap();
    }
}
