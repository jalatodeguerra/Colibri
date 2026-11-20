//! The instruction budget (EMBEDDING §5): the steps every statement, call, loop iteration and
//! element of a looping built-in decrement. The counters are in [`colibri_core::budget`]; this
//! file runs them for the interpreter.

use colibri_syntax::Span;

use crate::ctl::{Ctl, Eval};
use crate::eval::Interp;
use crate::fiber::Fiber;

pub(crate) use colibri_core::budget::Budget;
pub use colibri_core::budget::GRACE_STEPS;

impl Interp {
    /// Counts one step; `Err` when the budget is used up.
    #[inline(always)]
    pub(crate) fn step(&mut self, fiber: &mut Fiber, span: Span) -> Eval<()> {
        self.core.budget.left -= 1;
        if self.core.budget.left < 0 {
            return Err(self.budget_exhausted(fiber, span));
        }
        Ok(())
    }

    /// Counts `n` steps at once (a built-in that loops over `n` elements).
    #[inline]
    pub(crate) fn charge(&mut self, fiber: &mut Fiber, span: Span, n: usize) -> Eval<()> {
        self.core.budget.left = self
            .core
            .budget
            .left
            .saturating_sub(i64::try_from(n).unwrap_or(i64::MAX));
        if self.core.budget.left < 0 {
            return Err(self.budget_exhausted(fiber, span));
        }
        Ok(())
    }

    /// The failure of a step already counted: the budget is exhausted.
    #[cold]
    #[inline(never)]
    pub(crate) fn step_failure(&mut self, fiber: &mut Fiber, span: Span) -> Ctl {
        self.budget_exhausted(fiber, span)
    }

    #[cold]
    #[inline(never)]
    fn budget_exhausted(&mut self, fiber: &mut Fiber, span: Span) -> Ctl {
        self.core.budget.exhaust();
        match self.panic(fiber, span, "the instruction budget is exhausted") {
            Ctl::Panic(mut info) => {
                info.budget = true;
                Ctl::Panic(info)
            }
            other => other,
        }
    }

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
