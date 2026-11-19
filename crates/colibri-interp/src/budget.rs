//! The instruction budget (EMBEDDING §5): a counter of steps that every statement, call, loop
//! iteration and element of a looping built-in decrements.
//!
//! The budget is an allotment for each outermost entry from the host (`call`, `load`, `run_main`):
//! reentrant calls from a host function share it. The hot path is one decrement and one branch.
//! When the allotment runs out, evaluation unwinds like a panic, so `defer` statements and `Drop`
//! hooks run, but only for a grace of [`GRACE_STEPS`] steps; after that remaining `defer`
//! statements and hooks are skipped (plain releases still happen), so a script cannot extend its
//! life with an infinite cleanup.

use colibri_syntax::Span;

use crate::ctl::{Ctl, Eval};
use crate::eval::Interp;
use crate::fiber::Fiber;

/// How many more steps cleanups get after the budget is exhausted.
pub const GRACE_STEPS: i64 = 10_000;

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
#[derive(Debug)]
pub(crate) struct Budget {
    /// The allotment of each outermost entry (`None`: unlimited).
    pub allotment: Option<u64>,
    /// Steps left; the budget is exhausted when it goes below zero.
    pub left: i64,
    /// What `left` was when the current phase (normal or grace) began.
    phase_start: i64,
    /// Steps used in earlier phases of this entry.
    banked: u64,
    pub state: BudgetState,
    /// How many entries from the host are active, one inside another.
    pub depth: u32,
    /// The steps the last finished entry used.
    pub last_used: u64,
}

impl Budget {
    pub fn new() -> Self {
        Budget {
            allotment: None,
            left: i64::MAX,
            phase_start: i64::MAX,
            banked: 0,
            state: BudgetState::Normal,
            depth: 0,
            last_used: 0,
        }
    }

    /// An armed budget for one resumption of a coroutine by the scheduler.
    pub fn fresh(allotment: Option<u64>) -> Budget {
        let mut b = Budget::new();
        b.allotment = allotment;
        b.arm();
        b.depth = 1;
        b
    }

    fn arm(&mut self) {
        let start = self
            .allotment
            .map_or(i64::MAX, |a| i64::try_from(a).unwrap_or(i64::MAX));
        self.left = start;
        self.phase_start = start;
        self.banked = 0;
        self.state = BudgetState::Normal;
    }

    /// Between entries nothing is counted (and nothing limited).
    fn idle(&mut self) {
        self.left = i64::MAX;
        self.phase_start = i64::MAX;
        self.banked = 0;
        self.state = BudgetState::Normal;
    }

    fn used(&self) -> u64 {
        self.banked + u64::try_from(self.phase_start.saturating_sub(self.left)).unwrap_or(0)
    }

    /// The first exhaustion starts the grace; the second ends it.
    fn exhaust(&mut self) {
        match self.state {
            BudgetState::Normal => {
                self.banked += u64::try_from(self.phase_start).unwrap_or(0);
                self.left = GRACE_STEPS;
                self.phase_start = GRACE_STEPS;
                self.state = BudgetState::Grace;
            }
            BudgetState::Grace => {
                self.banked += u64::try_from(self.phase_start).unwrap_or(0);
                self.left = -1;
                self.phase_start = 0;
                self.state = BudgetState::Dead;
            }
            BudgetState::Dead => {}
        }
    }
}

impl Interp {
    /// Counts one step; `Err` when the budget is used up.
    #[inline(always)]
    pub(crate) fn step(&mut self, fiber: &mut Fiber, span: Span) -> Eval<()> {
        self.budget.left -= 1;
        if self.budget.left < 0 {
            return Err(self.budget_exhausted(fiber, span));
        }
        Ok(())
    }

    /// Counts `n` steps at once (a built-in that loops over `n` elements).
    #[inline]
    pub(crate) fn charge(&mut self, fiber: &mut Fiber, span: Span, n: usize) -> Eval<()> {
        self.budget.left = self
            .budget
            .left
            .saturating_sub(i64::try_from(n).unwrap_or(i64::MAX));
        if self.budget.left < 0 {
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
        self.budget.exhaust();
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
        self.budget.state == BudgetState::Dead
    }

    /// Whether the current entry ran out of budget (even if the program swallowed the panic).
    pub(crate) fn budget_was_exhausted(&self) -> bool {
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
    pub(crate) fn begin_entry(&mut self) -> bool {
        let outermost = self.budget.depth == 0;
        if outermost {
            self.budget.arm();
        }
        self.budget.depth += 1;
        outermost
    }

    /// Ends the entry `begin_entry` began. `record` keeps its step count for `steps_used`.
    pub(crate) fn end_entry(&mut self, outermost: bool, record: bool) {
        self.budget.depth -= 1;
        if outermost {
            if record {
                self.budget.last_used = self.budget.used();
            }
            self.budget.idle();
        }
    }
}
