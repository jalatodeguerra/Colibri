//! The leak detector as the interpreter offers it (ARCHITECTURE §18). The registry and the
//! detector are in [`colibri_core::leaks`]; this file gives them the function table of the
//! interpreter, which names closures in the report.

use crate::eval::Interp;
use colibri_core::leaks::Leak;

impl Interp {
    /// The cycles that nothing reaches (see [`colibri_core::leaks`]). Empty when tracking is
    /// off. Leaks come in allocation order of their oldest object.
    pub fn find_leaks(&self) -> Vec<Leak> {
        self.core
            .find_leaks(&|f| self.fns.get(f.0 as usize).map(|body| &**body))
    }
}
