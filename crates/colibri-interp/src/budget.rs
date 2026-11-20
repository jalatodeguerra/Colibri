//! The instruction budget (EMBEDDING §5): the steps every statement, call, loop iteration and
//! element of a looping built-in decrement. The counters are in [`colibri_core::budget`]; this
//! file runs them for the interpreter.

use crate::eval::Interp;

pub(crate) use colibri_core::budget::Budget;
pub use colibri_core::budget::GRACE_STEPS;

impl Interp {
    /// Sets the allotment of each outermost entry from the host; `None` is unlimited.
    pub fn set_instruction_budget(&mut self, steps: Option<u64>) {
        self.core.set_instruction_budget(steps);
    }

    /// The steps the current outermost entry has used so far, or the last one's total when
    /// none is running. Includes the grace after an exhaustion.
    pub fn steps_used(&self) -> u64 {
        self.core.steps_used()
    }
}
