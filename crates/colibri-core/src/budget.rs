//! The instruction budget (EMBEDDING §5): a counter of steps that every statement, call, loop
//! iteration and element of a looping built-in decrements.
//!
//! The budget is an allotment for each outermost entry from the host (`call`, `load`, `run_main`):
//! reentrant calls from a host function share it. The hot path is one decrement and one branch.
//! When the allotment runs out, evaluation unwinds like a panic, so `defer` statements and `Drop`
//! hooks run, but only for a grace of `GRACE_STEPS` steps; after that remaining `defer`
//! statements and hooks are skipped (plain releases still happen), so a script cannot extend its
//! life with an infinite cleanup.

use std::num::NonZeroU32;

use crate::state::Core;

/// How many more steps cleanups get after the budget is exhausted.
pub(crate) const GRACE_STEPS: i64 = 10_000;

/// Where an entry stands with its budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BudgetState {
    Normal,
    /// Exhausted: unwinding, with [`GRACE_STEPS`] left for cleanups.
    Grace,
    /// The grace ran out too: cleanups are skipped.
    Dead,
}

/// The counters of the budget.
///
/// `left` is the part of the budget the hot path counts down: a window of `slice` steps (the last
/// one is the step that takes `left` below zero)
/// (see [`Core::want_safepoints`]), or all of what is left when nobody asked for slices. The
/// rest waits in `reserve`. Only the sum, [`Budget::real_left`], says whether the budget is used
/// up; everything but the decrement-and-test of the hot path reads that.
#[derive(Debug)]
pub struct Budget {
    /// The allotment of each outermost entry (`None`: unlimited).
    pub allotment: Option<u64>,
    /// Steps left in the current window; below zero, the window is over: either the slice ended
    /// (the reserve refills it) or the budget really ran out.
    pub left: i64,
    /// The steps of the phase that are not in `left` (zero without a slice).
    reserve: i64,
    /// The smallest interval any consumer of safepoints asked for.
    slice: Option<NonZeroU32>,
    /// What `left` was when the window began (the last refill, or the start of the phase).
    window: i64,
    /// What the whole phase (normal or grace) began with: `left + reserve` at its start.
    phase_start: i64,
    /// Steps used in earlier phases of this entry.
    banked: u64,
    state: BudgetState,
    /// How many entries from the host are active, one inside another.
    depth: u32,
    /// The steps the last finished entry used.
    last_used: u64,
}

impl Default for Budget {
    fn default() -> Self {
        Self::new()
    }
}

impl Budget {
    pub fn new() -> Self {
        Budget {
            allotment: None,
            left: i64::MAX,
            reserve: 0,
            slice: None,
            window: i64::MAX,
            phase_start: i64::MAX,
            banked: 0,
            state: BudgetState::Normal,
            depth: 0,
            last_used: 0,
        }
    }

    /// An armed budget for one resumption of a coroutine by the scheduler.
    pub fn fresh(allotment: Option<u64>, slice: Option<NonZeroU32>) -> Budget {
        let mut b = Budget::new();
        b.allotment = allotment;
        b.slice = slice;
        b.arm();
        b.depth = 1;
        b
    }

    fn arm(&mut self) {
        let start = self
            .allotment
            .map_or(i64::MAX, |a| i64::try_from(a).unwrap_or(i64::MAX));
        self.begin_phase(start);
        self.banked = 0;
        self.state = BudgetState::Normal;
    }

    /// Starts a phase of `real` steps, windowed by the slice.
    fn begin_phase(&mut self, real: i64) {
        self.phase_start = real;
        self.left = real;
        self.reserve = 0;
        self.rewindow();
    }

    /// Splits `left + reserve` (when it is not already overdrawn) into a window and the reserve.
    fn rewindow(&mut self) {
        let real = self.left.saturating_add(self.reserve);
        match self.slice {
            Some(slice) if real >= 0 => {
                self.left = real.min(i64::from(slice.get()) - 1);
                self.reserve = real - self.left;
            }
            _ => {
                self.left = real;
                self.reserve = 0;
            }
        }
        self.window = self.left;
    }

    /// Between entries nothing is counted (and nothing limited).
    fn idle(&mut self) {
        self.left = i64::MAX;
        self.reserve = 0;
        self.window = i64::MAX;
        self.phase_start = i64::MAX;
        self.banked = 0;
        self.state = BudgetState::Normal;
    }

    /// The steps the budget really has left: the window and the reserve. Negative when it is
    /// overdrawn. Anything that decides on the budget, rather than counting it down, reads this
    /// and never `left`.
    #[inline(always)]
    pub fn real_left(&self) -> i64 {
        self.left.saturating_add(self.reserve)
    }

    pub(crate) fn slice(&self) -> Option<NonZeroU32> {
        self.slice
    }

    /// Whether no step is left for a fast path that wants one (`real_left() <= 0`). A window with
    /// steps in it is answered without looking at the reserve.
    #[inline(always)]
    pub fn none_left(&self) -> bool {
        self.left <= 0 && self.real_left() <= 0
    }

    /// Changes the slice of a budget that is running: the window shrinks or grows to it.
    pub(crate) fn set_slice(&mut self, slice: Option<NonZeroU32>) {
        if self.slice == slice {
            return;
        }
        self.slice = slice;
        self.rewindow();
    }

    fn used(&self) -> u64 {
        self.banked + u64::try_from(self.phase_start.saturating_sub(self.real_left())).unwrap_or(0)
    }

    /// The window ended below zero: when the reserve covers it, refills the window and says how
    /// many steps the window used. `None` when the budget really ran out (nothing changes then).
    #[cold]
    #[inline(never)]
    pub(crate) fn refill(&mut self) -> Option<u64> {
        // The window holds `slice - 1` steps below the step that ends it: `slice` steps in all.
        let cap = i64::from(self.slice?.get()) - 1;
        if self.real_left() < 0 {
            return None;
        }
        let spent = self.window.saturating_sub(self.left);
        // The overdraft of the last charge comes out of the new window.
        let take = self.reserve.min(cap.saturating_sub(self.left));
        self.left += take;
        self.reserve -= take;
        self.window = self.left;
        Some(u64::try_from(spent).unwrap_or(0))
    }

    /// The first exhaustion starts the grace; the second ends it.
    pub(crate) fn exhaust(&mut self) {
        match self.state {
            BudgetState::Normal => {
                self.banked += u64::try_from(self.phase_start).unwrap_or(0);
                self.begin_phase(GRACE_STEPS);
                self.state = BudgetState::Grace;
            }
            BudgetState::Grace => {
                self.banked += u64::try_from(self.phase_start).unwrap_or(0);
                self.left = -1;
                self.reserve = 0;
                self.window = -1;
                self.phase_start = 0;
                self.state = BudgetState::Dead;
            }
            BudgetState::Dead => {}
        }
    }
}

impl Core {
    /// Whether the grace of an exhausted budget is used up: cleanups are skipped.
    #[inline]
    pub fn budget_is_dead(&self) -> bool {
        self.budget.state == BudgetState::Dead
    }

    /// Whether the current entry ran out of budget (even if the program swallowed the panic).
    #[inline]
    pub fn budget_was_exhausted(&self) -> bool {
        self.budget.state != BudgetState::Normal
    }

    /// Sets the allotment of each outermost entry from the host; `None` is unlimited.
    pub fn set_instruction_budget(&mut self, steps: Option<u64>) {
        self.budget.allotment = steps;
    }

    /// The steps the current outermost entry has used so far, or the last one's total when
    /// none is running. Includes the grace after an exhaustion.
    pub fn steps_used(&self) -> u64 {
        if self.budget.depth > 0 {
            self.budget.used()
        } else {
            self.budget.last_used
        }
    }

    /// Begins an entry from the host: the outermost one gets a fresh allotment.
    #[inline]
    pub fn begin_entry(&mut self) -> bool {
        let outermost = self.budget.depth == 0;
        if outermost {
            self.budget.arm();
        }
        self.budget.depth += 1;
        outermost
    }

    /// Ends the entry `begin_entry` began. `record` keeps its step count for `steps_used`.
    #[inline]
    pub fn end_entry(&mut self, outermost: bool, record: bool) {
        self.budget.depth -= 1;
        if outermost {
            if record {
                self.budget.last_used = self.budget.used();
            }
            self.budget.idle();
        }
    }
}
