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
    /// A thread task's worker: the pool asks the running task to stop (see
    /// `Core::set_cancel_flag`). Runs last, and only ever fails.
    Cancel,
}

/// How many kinds there are.
const KINDS: usize = 4;

impl SafepointKind {
    /// Every kind, in the order the consumers run.
    pub const ALL: [SafepointKind; KINDS] = [
        SafepointKind::Memory,
        SafepointKind::Debugger,
        SafepointKind::Profiler,
        SafepointKind::Cancel,
    ];

    fn index(self) -> usize {
        self as usize
    }
}

/// One consumer whose interval has passed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Due {
    pub kind: SafepointKind,
    /// How many of its intervals have passed since it last ran: more than one after a native
    /// loop charged many steps at once. A profiler weighs its sample by this.
    pub intervals: u64,
    /// The interval the consumer asked for.
    pub every: u32,
}

/// What the consumers asked for, and how far each has got.
#[derive(Debug, Default)]
pub struct Safepoints {
    /// The interval each kind asked for.
    every: [Option<NonZeroU32>; KINDS],
    /// The steps since each kind last ran.
    since: [u64; KINDS],
    /// How many intervals of each kind have run (a sample that stood for several counts them all).
    hits: [u64; KINDS],
    /// The smallest interval asked for.
    slice: Option<NonZeroU32>,
    /// The steps spent in the slice that is running, across budgets: a host that calls in for 42
    /// steps at a time, or a coroutine that runs for a few steps between waits, still reaches
    /// the end of a slice.
    pub(crate) progress: u64,
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

    /// `spent` steps went by in a budget that is set aside, or that ended its window.
    pub(crate) fn credit(&mut self, spent: u64) {
        let Some(slice) = self.slice else { return };
        for since in &mut self.since {
            *since = since.saturating_add(spent);
        }
        self.progress = self.progress.saturating_add(spent) % u64::from(slice.get());
    }
}

impl Core {
    /// Asks for a safepoint for `kind` about every `every` steps (at the first instruction
    /// boundary that charges a step after that many; a native loop that charges many steps at
    /// once counts as them all). Asking again changes the interval. Nothing is lost when the
    /// slice changes: the steps are only counted differently.
    pub fn want_safepoints(&mut self, kind: SafepointKind, every: NonZeroU32) {
        self.resync(|sp| sp.every[kind.index()] = Some(every), kind);
    }

    /// Gives this engine a flag that stops it: once the flag is set, the next safepoint of the
    /// kind `Cancel` (asked for with [`Core::want_safepoints`]) ends the running entry with the
    /// panic that the budget's exhaustion raises. A worker of the thread pool gets the flag of
    /// its pool, which is set when the `Vm` that owns the pool is dropped.
    pub fn set_cancel_flag(&mut self, flag: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>) {
        self.cancel = flag;
    }

    /// Whether the flag given to [`Core::set_cancel_flag`] is set.
    pub fn cancelled(&self) -> bool {
        self.cancel
            .as_ref()
            .is_some_and(|f| f.load(std::sync::atomic::Ordering::Acquire))
    }

    /// Stops the safepoints of `kind`. With no consumer left the budget counts as before.
    pub fn drop_safepoints(&mut self, kind: SafepointKind) {
        self.resync(|sp| sp.every[kind.index()] = None, kind);
    }

    fn resync(&mut self, change: impl FnOnce(&mut Safepoints), kind: SafepointKind) {
        self.close_budget();
        change(&mut self.safepoints);
        self.safepoints.since[kind.index()] = 0;
        self.safepoints.refresh();
        self.safepoints.progress = match self.safepoints.slice {
            Some(slice) => self.safepoints.progress % u64::from(slice.get()),
            None => 0,
        };
        self.sync_slice();
        self.open_budget();
    }

    /// Gives a budget that was set aside (a suspended entry's) the current slice.
    #[inline]
    pub(crate) fn sync_slice(&mut self) {
        self.budget.set_slice(self.safepoints.slice);
    }

    /// How many intervals of the safepoints of `kind` have run.
    pub fn safepoint_hits(&self, kind: SafepointKind) -> u64 {
        self.safepoints.hits[kind.index()]
    }

    /// Takes the steps that `kind` has not been told about yet: those charged since its last
    /// safepoint, the ones of the budget that is open included. A consumer that wants the whole
    /// total (a profiler at the end of the program) flushes them as a last sample.
    pub fn take_pending_steps(&mut self, kind: SafepointKind) -> u64 {
        self.close_budget();
        let pending = std::mem::take(&mut self.safepoints.since[kind.index()]);
        self.open_budget();
        pending
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
        self.safepoints.credit(spent);
        self.safepoints.progress = 0;
        self.safepoint_due = true;
        true
    }

    /// The consumers whose interval has passed, in the order they run (`None` for the others);
    /// takes the safepoint off the due list. Every due consumer is taken at once: the engine's
    /// `safepoint` gives each its sample even when an earlier one fails.
    pub fn take_due_safepoints(&mut self) -> [Option<Due>; KINDS] {
        self.safepoint_due = false;
        let mut due = [None; KINDS];
        let sp = &mut self.safepoints;
        for kind in SafepointKind::ALL {
            let i = kind.index();
            if let Some(every) = sp.every[i] {
                let every = u64::from(every.get());
                if sp.since[i] >= every {
                    let intervals = sp.since[i] / every;
                    sp.since[i] %= every;
                    sp.hits[i] += intervals;
                    due[i] = Some(Due {
                        kind,
                        intervals,
                        every: every as u32,
                    });
                }
            }
        }
        due
    }
}
