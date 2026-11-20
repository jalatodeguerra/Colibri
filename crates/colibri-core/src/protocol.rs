//! Panics and the instruction budget, for any [`Engine`].

use colibri_syntax::Span;

use crate::ctl::{Ctl, PanicInfo};
use crate::engine::Engine;

/// Making panics and counting steps. Blanket-implemented for every engine, so
/// `self.panic(fiber, span, "...")` and `self.step(fiber, span)` read as they always did.
#[doc(hidden)]
pub trait Protocol: Engine {
    /// Creates a panic at `span`, capturing the Colibri stack as it is now.
    fn panic(&self, fiber: &mut Self::Fiber, span: Span, message: impl Into<String>) -> Ctl {
        Self::note_position(fiber, span);
        let stack = self.stack_trace(fiber);
        Ctl::Panic(Box::new(PanicInfo {
            message: message.into(),
            span,
            stack,
            notes: Vec::new(),
            budget: false,
        }))
    }

    /// A panic for a state the checker should have made impossible.
    fn internal(&self, fiber: &mut Self::Fiber, span: Span, what: &str) -> Ctl {
        self.panic(fiber, span, format!("internal error: {what}"))
    }

    /// Counts one step; `Err` when the budget is used up.
    #[inline(always)]
    fn step(&mut self, fiber: &mut Self::Fiber, span: Span) -> crate::ctl::Eval<()> {
        let budget = &mut self.core_mut().budget;
        budget.left -= 1;
        if budget.left < 0 {
            return Err(self.budget_exhausted(fiber, span));
        }
        Ok(())
    }

    /// Counts `n` steps at once (a built-in that loops over `n` elements).
    #[inline]
    fn charge(&mut self, fiber: &mut Self::Fiber, span: Span, n: usize) -> crate::ctl::Eval<()> {
        let budget = &mut self.core_mut().budget;
        budget.left = budget
            .left
            .saturating_sub(i64::try_from(n).unwrap_or(i64::MAX));
        if budget.left < 0 {
            return Err(self.budget_exhausted(fiber, span));
        }
        Ok(())
    }

    /// The failure of a step already counted: the budget is exhausted.
    #[cold]
    #[inline(never)]
    fn step_failure(&mut self, fiber: &mut Self::Fiber, span: Span) -> Ctl {
        self.budget_exhausted(fiber, span)
    }

    #[cold]
    #[inline(never)]
    fn budget_exhausted(&mut self, fiber: &mut Self::Fiber, span: Span) -> Ctl {
        self.core_mut().budget.exhaust();
        match self.panic(fiber, span, "the instruction budget is exhausted") {
            Ctl::Panic(mut info) => {
                info.budget = true;
                Ctl::Panic(info)
            }
            other => other,
        }
    }
}

impl<E: Engine> Protocol for E {}
