//! The instruction budget (EMBEDDING §5): the steps every statement, call, loop iteration and
//! element of a looping built-in decrement. The counters are in [`colibri_core::budget`]; this
//! file runs them for the interpreter.

use colibri_syntax::Span;

use crate::ctl::{Ctl, Eval};
use crate::eval::Interp;
use crate::fiber::Fiber;

pub use colibri_core::budget::GRACE_STEPS;
pub(crate) use colibri_core::budget::{Budget, BudgetState};

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

    /// Whether the grace of an exhausted budget is used up: cleanups are skipped.
    pub(crate) fn budget_is_dead(&self) -> bool {
        self.core.budget.state == BudgetState::Dead
    }

    /// Whether the current entry ran out of budget (even if the program swallowed the panic).
    pub(crate) fn budget_was_exhausted(&self) -> bool {
        self.core.budget.state != BudgetState::Normal
    }

    /// Sets the allotment of each outermost entry from the host; `None` is unlimited.
    pub fn set_instruction_budget(&mut self, steps: Option<u64>) {
        self.core.budget.allotment = steps;
    }

    /// The steps the current outermost entry has used so far, or the last one's total when
    /// none is running. Includes the grace after an exhaustion.
    pub fn steps_used(&self) -> u64 {
        if self.core.budget.depth > 0 {
            self.core.budget.used()
        } else {
            self.core.budget.last_used
        }
    }

    /// Begins an entry from the host: the outermost one gets a fresh allotment.
    pub(crate) fn begin_entry(&mut self) -> bool {
        let outermost = self.core.budget.depth == 0;
        if outermost {
            self.core.budget.arm();
        }
        self.core.budget.depth += 1;
        outermost
    }

    /// Ends the entry `begin_entry` began. `record` keeps its step count for `steps_used`.
    pub(crate) fn end_entry(&mut self, outermost: bool, record: bool) {
        self.core.budget.depth -= 1;
        if outermost {
            if record {
                self.core.budget.last_used = self.core.budget.used();
            }
            self.core.budget.idle();
        }
    }
}
