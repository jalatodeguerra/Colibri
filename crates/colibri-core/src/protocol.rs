//! Panics and the instruction budget, for any [`Engine`].

use colibri_syntax::{FileId, Span};

use crate::ctl::{Ctl, PanicInfo};
use crate::engine::Engine;
use crate::host::HostFault;

/// Making panics and counting steps. Blanket-implemented for every engine, so
/// `self.panic(fiber, span, "...")` and `self.step(fiber, span)` read as they always did.
#[doc(hidden)]
pub trait Protocol: Engine {
    /// Creates a panic at `span`, capturing the Colibri stack as it is now.
    fn panic(&self, fiber: &mut Self::Fiber, span: Span, message: impl Into<String>) -> Ctl {
        let span = Self::visible_span(fiber, span);
        Self::note_position(fiber, span);
        let stack = self.stack_trace(fiber);
        Ctl::Panic(Box::new(PanicInfo {
            message: message.into(),
            span,
            stack,
            notes: Vec::new(),
            budget: false,
            debug_serial: 0,
        }))
    }

    /// Native code is about to call user code (a hook, a `next`, an error's `message`): a
    /// coroutine cannot pause until [`Protocol::leave_native`] (`FiberState::native_depth`).
    #[inline]
    fn enter_native(fiber: &mut Self::Fiber) {
        Self::fiber_state_mut(fiber).native_depth += 1;
    }

    /// The call that [`Protocol::enter_native`] announced is over, however it ended.
    #[inline]
    fn leave_native(fiber: &mut Self::Fiber) {
        let state = Self::fiber_state_mut(fiber);
        state.native_depth = state.native_depth.saturating_sub(1);
    }

    /// A panic for a state the checker should have made impossible.
    fn internal(&self, fiber: &mut Self::Fiber, span: Span, what: &str) -> Ctl {
        self.panic(fiber, span, format!("internal error: {what}"))
    }

    /// The control flow a failed host function or host hook ends the running operation with:
    /// a panic at the position of the innermost frame, or cancellation.
    fn host_fault_ctl(&self, fiber: &mut Self::Fiber, fault: HostFault) -> Ctl {
        match fault {
            HostFault::Panic(message) | HostFault::Message(message) => {
                let span = Self::position(fiber).unwrap_or(Span::new(FileId(0), 0, 0));
                self.panic(fiber, span, message)
            }
            HostFault::Cancel => {
                Self::fiber_state_mut(fiber).cancelling = true;
                Ctl::Cancel
            }
        }
    }

    /// Counts one step; `Err` when the budget is used up.
    #[inline(always)]
    fn step(&mut self, fiber: &mut Self::Fiber, span: Span) -> crate::ctl::Eval<()> {
        let budget = &mut self.core_mut().budget;
        budget.left -= 1;
        if budget.left < 0 {
            return self.step_over(fiber, span);
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
            return self.step_over(fiber, span);
        }
        Ok(())
    }

    /// The window of the budget ended below zero after a step was counted. When only a slice
    /// ended, the safepoint runs and the step stands; when the budget is used up, `Err`.
    #[cold]
    #[inline(never)]
    fn step_over(&mut self, fiber: &mut Self::Fiber, span: Span) -> crate::ctl::Eval<()> {
        if self.core_mut().slice_ended() {
            return self.safepoint(fiber);
        }
        Err(self.budget_exhausted(fiber, span))
    }

    #[cold]
    #[inline(never)]
    fn budget_exhausted(&mut self, fiber: &mut Self::Fiber, span: Span) -> Ctl {
        self.core_mut().exhaust_budget();
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
