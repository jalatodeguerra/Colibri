//! Safepoint slices: a check that runs every N steps, and costs nothing when nobody asks.
//!
//! A consumer (the memory limit, the debugger, the profiler) registers an interval with
//! [`Core::want_safepoints`]. The budget counts steps down in windows of the smallest interval
//! any consumer asked for (`Budget::slice`); the end of a window takes the same slow path as the
//! end of the budget, finds the budget is not used up, refills and runs the consumers whose
//! interval has passed. Without a consumer there is no window: the hot path is what it always was.
//!
//! The consumers run in a fixed order, the order of [`SafepointKind`]: the memory limit first (it
//! may stop the program), then the debugger (it may pause), then the profiler (it only looks).

use std::num::NonZeroU32;

use crate::state::Core;

/// Who wants safepoints. The order is the order the consumers run in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SafepointKind {
    Memory,
    Debugger,
    Profiler,
}

impl SafepointKind {
    /// Every kind, in the order the consumers run.
    pub const ALL: [SafepointKind; 3] = [
        SafepointKind::Memory,
        SafepointKind::Debugger,
        SafepointKind::Profiler,
    ];

    fn index(self) -> usize {
        self as usize
    }
}

/// What the consumers asked for, and how far each has got.
#[derive(Debug, Default)]
pub struct Safepoints {
    /// The interval each kind asked for.
    every: [Option<NonZeroU32>; 3],
    /// The steps since each kind last ran.
    since: [u64; 3],
    /// How many times each kind ran.
    hits: [u64; 3],
    slice: Option<NonZeroU32>,
}

impl Safepoints {
    /// The smallest interval asked for.
    #[inline]
    pub(crate) fn slice(&self) -> Option<NonZeroU32> {
        self.slice
    }

    fn refresh(&mut self) {
        self.slice = self.every.iter().flatten().copied().min();
    }
}

impl Core {
    /// Asks for a safepoint for `kind` about every `every` steps (at the first instruction
    /// boundary that charges a step after that many; a native loop that charges many steps at
    /// once counts as them all). Asking again changes the interval. Nothing is lost when the
    /// slice changes: the steps are only counted differently.
    pub fn want_safepoints(&mut self, kind: SafepointKind, every: NonZeroU32) {
        self.safepoints.every[kind.index()] = Some(every);
        self.safepoints.since[kind.index()] = 0;
        self.safepoints.refresh();
        self.sync_slice();
    }

    /// Stops the safepoints of `kind`. With no consumer left the budget counts as before.
    pub fn drop_safepoints(&mut self, kind: SafepointKind) {
        self.safepoints.every[kind.index()] = None;
        self.safepoints.since[kind.index()] = 0;
        self.safepoints.refresh();
        self.sync_slice();
    }

    /// Gives a budget that was set aside (a suspended entry's) the current slice.
    #[inline]
    pub(crate) fn sync_slice(&mut self) {
        self.budget.set_slice(self.safepoints.slice);
    }

    /// How many times the safepoint of `kind` has run.
    pub fn safepoint_hits(&self, kind: SafepointKind) -> u64 {
        self.safepoints.hits[kind.index()]
    }

    /// The slow path of a counted-down window that went below zero: when only the slice ended
    /// (the budget has steps left), refills the window, marks a safepoint due and says `true`.
    /// `false`: the budget really ran out.
    #[cold]
    #[inline(never)]
    pub fn slice_ended(&mut self) -> bool {
        let Some(spent) = self.budget.refill() else {
            return false;
        };
        for since in &mut self.safepoints.since {
            *since = since.saturating_add(spent);
        }
        self.safepoint_due = true;
        true
    }

    /// The kinds whose interval has passed, in the order they run; takes the safepoint off the
    /// due list. The engine's `safepoint` runs them.
    pub fn take_due_safepoints(&mut self) -> impl Iterator<Item = SafepointKind> + use<> {
        self.safepoint_due = false;
        let mut due = [None; 3];
        let sp = &mut self.safepoints;
        for kind in SafepointKind::ALL {
            let i = kind.index();
            if let Some(every) = sp.every[i]
                && sp.since[i] >= u64::from(every.get())
            {
                sp.since[i] %= u64::from(every.get());
                sp.hits[i] += 1;
                due[i] = Some(kind);
            }
        }
        due.into_iter().flatten()
    }
}
