//! The instruction budget (EMBEDDING §5): a counter of steps that every statement, call, loop
//! iteration and element of a looping built-in decrements.
//!
//! The budget is an allotment for each outermost entry from the host (`call`, `load`, `run_main`):
//! reentrant calls from a host function share it. The hot path is one decrement and one branch.
//! When the allotment runs out, evaluation unwinds like a panic, so `defer` statements and `Drop`
//! hooks run, but only for a grace of `GRACE_STEPS` steps; after that remaining `defer`
//! statements and hooks are skipped (plain releases still happen), so a script cannot extend its
//! life with an infinite cleanup.

use std::cell::Cell;
use std::num::NonZeroU32;
use std::rc::Rc;

use crate::mem::EntryGuard;
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

/// Where an entry stands with the memory limit (the sibling of [`BudgetState`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MemState {
    Normal,
    /// The limit was passed: unwinding, with [`MEMORY_GRACE_BYTES`] of room for cleanups.
    Grace,
    /// The grace was used up too: cleanups are skipped.
    Dead,
}

/// How many steps pass between two looks at the memory account (the slice of its safepoint).
pub const MEMORY_SLICE: u32 = 4_096;

/// How many bytes more than it held when the limit was passed the cleanups of an unwinding entry
/// may allocate.
pub const MEMORY_GRACE_BYTES: u64 = 1 << 20;

/// The memory limit of an engine and where the current entry stands with it.
#[derive(Debug)]
pub struct MemLimit {
    /// The host's limit in bytes (`None`: unlimited).
    limit: Option<u64>,
    state: MemState,
    /// The limit the account has while the entry is in its grace.
    grace_limit: u64,
}

impl MemLimit {
    pub(crate) fn new() -> MemLimit {
        MemLimit {
            limit: None,
            state: MemState::Normal,
            grace_limit: 0,
        }
    }
}

/// What [`Core::begin_inline_entry`] set aside.
pub struct InlineEntry {
    budget: Budget,
    memory: SavedMemState,
}

/// The memory state of the entry a fresh coroutine resumption interrupted (see
/// [`Core::begin_resumption_memory`]).
#[derive(Clone, Copy, Debug)]
pub struct SavedMemState {
    state: MemState,
    grace_limit: u64,
}

/// What ran out in an entry whose program swallowed the panic that said so.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exhaustion {
    Budget,
    /// The memory limit, in bytes.
    Memory(u64),
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
    pub fn fresh(allotment: Option<u64>, slice: Option<NonZeroU32>, progress: u64) -> Budget {
        let mut b = Budget::new();
        b.allotment = allotment;
        b.slice = slice;
        b.arm(progress);
        b.depth = 1;
        b
    }

    fn arm(&mut self, progress: u64) {
        let start = self
            .allotment
            .map_or(i64::MAX, |a| i64::try_from(a).unwrap_or(i64::MAX));
        self.begin_phase(start, progress);
        self.banked = 0;
        self.state = BudgetState::Normal;
    }

    /// Starts a phase of `real` steps, windowed by the slice.
    fn begin_phase(&mut self, real: i64, progress: u64) {
        self.phase_start = real;
        self.left = real;
        self.reserve = 0;
        self.rewindow(progress);
    }

    /// Splits `left + reserve` (when it is not already overdrawn) into a window and the reserve.
    /// `progress` steps of the current slice are already spent (in other budgets, or earlier
    /// entries): the window ends where the slice does.
    fn rewindow(&mut self, progress: u64) {
        let real = self.left.saturating_add(self.reserve);
        match self.slice {
            Some(slice) if real >= 0 => {
                let room = i64::from(slice.get()) - 1 - i64::try_from(progress).unwrap_or(i64::MAX);
                self.left = real.min(room.max(0));
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

    /// Closes the window: the reserve goes back into `left`, so that nothing is split. Says
    /// how many steps the window spent (0 when there is no slice), for the countdown of the
    /// slice to carry on elsewhere (`Core::credit`).
    pub(crate) fn close(&mut self) -> u64 {
        if self.slice.is_none() {
            return 0;
        }
        // A charge that overdrew the budget itself (not only the window) is refused: its steps
        // were never taken, and `used()` leaves them out too.
        let refused = self.real_left().min(0).unsigned_abs();
        let spent = u64::try_from(self.window.saturating_sub(self.left))
            .unwrap_or(0)
            .saturating_sub(refused);
        self.left = self.real_left();
        self.reserve = 0;
        self.window = self.left;
        spent
    }

    /// Opens a window on a closed budget, `progress` steps into the slice. An idle budget (no
    /// entry running) is left alone.
    pub(crate) fn open(&mut self, progress: u64) {
        if self.depth > 0 {
            self.rewindow(progress);
        }
    }

    /// Sets the slice of a closed budget.
    pub(crate) fn set_slice(&mut self, slice: Option<NonZeroU32>) {
        self.slice = slice;
    }

    fn used(&self) -> u64 {
        self.banked
            + u64::try_from(self.phase_start.saturating_sub(self.real_left().max(0))).unwrap_or(0)
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
    pub(crate) fn exhaust(&mut self, progress: u64) {
        match self.state {
            BudgetState::Normal => {
                self.banked += u64::try_from(self.phase_start).unwrap_or(0);
                self.begin_phase(GRACE_STEPS, progress);
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

/// An entry that has begun and not ended (see [`Core::begin_entry`]).
#[must_use = "an entry must be ended with `end_entry`"]
pub struct Entry {
    outermost: bool,
    /// Restores the thread's memory account if the entry is dropped by an unwind.
    mem: EntryGuard,
    /// Where an entry that is dropped without having ended says so (see [`Core::begin_entry`]).
    lost: Rc<Cell<u32>>,
    ended: bool,
}

impl Entry {
    /// Whether this is the outermost entry of its engine (the one with a fresh budget).
    #[inline]
    pub fn is_outermost(&self) -> bool {
        self.outermost
    }
}

impl Drop for Entry {
    fn drop(&mut self) {
        if !self.ended {
            // A panic unwound past the entry: the engine settles its books at the next entry.
            self.lost.set(self.lost.get() + 1);
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

    /// Begins an entry from the host: the outermost one gets a fresh allotment. The result goes
    /// to [`Core::end_entry`]; if a panic unwinds past it instead, dropping it gives the thread
    /// back its previous memory account and tells the engine, which settles its books (the depth
    /// of entries, the budget and the memory limit) before the next entry or end.
    #[inline]
    pub fn begin_entry(&mut self) -> Entry {
        if self.lost_entries.get() != 0 {
            self.settle_lost_entries();
        }
        let outermost = self.budget.depth == 0;
        let mem = if outermost {
            self.budget.arm(self.safepoints.progress);
            self.arm_memory_limit();
            self.mem.enter()
        } else {
            EntryGuard::inert()
        };
        self.budget.depth += 1;
        Entry {
            outermost,
            mem,
            lost: Rc::clone(&self.lost_entries),
            ended: false,
        }
    }

    /// Ends the entry `begin_entry` began. `record` keeps its step count for `steps_used`.
    #[inline]
    pub fn end_entry(&mut self, mut entry: Entry, record: bool) {
        if self.lost_entries.get() != 0 {
            self.settle_lost_entries();
        }
        entry.ended = true;
        self.budget.depth -= 1;
        if entry.outermost {
            if record {
                self.budget.last_used = self.budget.used();
            }
            self.close_budget();
            self.budget.idle();
            self.rest_memory_limit();
            let guard = std::mem::replace(&mut entry.mem, EntryGuard::inert());
            self.mem.leave(guard);
        }
    }

    /// Entries were dropped without ending (a Rust panic unwound past them): takes them out of
    /// the depth, and when none is left, gives the budget and the memory limit the state of an
    /// engine between entries, so that the next entry is outermost with a fresh allotment and
    /// limit.
    #[cold]
    #[inline(never)]
    fn settle_lost_entries(&mut self) {
        let lost = self.lost_entries.replace(0);
        self.budget.depth = self.budget.depth.saturating_sub(lost);
        if self.budget.depth == 0 {
            self.close_budget();
            self.budget.idle();
            self.rest_memory_limit();
        }
    }

    /// Bytes this engine's entries hold (allocated under its account and not freed), or `None`
    /// when the counting allocator is not installed.
    pub fn memory_used(&self) -> Option<u64> {
        self.mem.used()
    }

    /// Sets the memory limit in bytes (`None` for none). The allocator raises a flag past it,
    /// which a safepoint of 4,096 steps reads (and the built-ins that allocate in proportion to
    /// an argument check first with [`Core::mem_reserve`]); the enforcement is a panic like the
    /// budget's. The engine must be between entries.
    pub fn set_memory_limit(&mut self, limit: Option<u64>) {
        self.memlimit.limit = limit;
        self.memlimit.state = MemState::Normal;
        match limit {
            Some(_) => {
                if let Some(every) = NonZeroU32::new(MEMORY_SLICE) {
                    self.want_safepoints(crate::safepoint::SafepointKind::Memory, every);
                }
            }
            None => self.drop_safepoints(crate::safepoint::SafepointKind::Memory),
        }
        self.mem.set_limit(limit);
    }

    /// The memory limit in bytes, if one is set.
    pub fn memory_limit(&self) -> Option<u64> {
        self.memlimit.limit
    }

    /// Whether the account went past its limit since the flag was last cleared.
    pub fn memory_over(&self) -> bool {
        self.mem.over()
    }

    /// An outermost entry begins: the account has its limit (the host may have set it, or
    /// something may have been freed since the flag was raised).
    #[inline]
    fn arm_memory_limit(&mut self) {
        if self.memlimit.limit.is_some() {
            self.memlimit.state = MemState::Normal;
            self.mem.set_limit(self.memlimit.limit);
        }
    }

    /// The outermost entry ended (or was lost): back to the host's limit, with the grace over.
    #[inline]
    fn rest_memory_limit(&mut self) {
        if self.memlimit.limit.is_some() || self.memlimit.state != MemState::Normal {
            self.memlimit.state = MemState::Normal;
            self.mem.set_limit(self.memlimit.limit);
        }
    }

    /// Whether `bytes` more fit under the memory limit. A built-in that allocates in proportion
    /// to an argument asks before it allocates, so that one statement cannot allocate gigabytes
    /// between two safepoints; `false` means it fails with the memory-limit panic
    /// ([`Protocol::mem_reserve`](crate::protocol::Protocol::mem_reserve) does that). With no
    /// limit, this is one comparison.
    #[inline]
    pub fn mem_reserve(&self, bytes: usize) -> bool {
        if self.memlimit.limit.is_none() {
            return true;
        }
        self.mem_reserve_checked(bytes)
    }

    #[cold]
    #[inline(never)]
    fn mem_reserve_checked(&self, bytes: usize) -> bool {
        self.memlimit.state != MemState::Dead && !self.mem.would_exceed(bytes as u64)
    }

    /// Whether the memory safepoint must fail: the account passed its limit, or the grace of an
    /// earlier failure of this entry is used up.
    #[inline]
    pub fn memory_exceeded(&self) -> bool {
        self.memlimit.state == MemState::Dead || self.mem.over()
    }

    /// The memory limit was passed: the first time begins the grace (the account gets
    /// [`MEMORY_GRACE_BYTES`] more than it holds, or than its limit when that is more), the second
    /// ends it. Gives the host's limit, for the message.
    #[doc(hidden)]
    pub fn exhaust_memory(&mut self) -> u64 {
        let limit = self.memlimit.limit.unwrap_or(0);
        match self.memlimit.state {
            MemState::Normal => {
                // The grace is sized like the limit is measured: on what this isolate holds itself
                // (for a worker's account, everything the `Vm` and its workers hold).
                let used = self.mem.measured_used().unwrap_or(0);
                self.memlimit.grace_limit = limit.max(used).saturating_add(MEMORY_GRACE_BYTES);
                self.memlimit.state = MemState::Grace;
                self.mem.set_limit(Some(self.memlimit.grace_limit));
            }
            MemState::Grace => self.memlimit.state = MemState::Dead,
            MemState::Dead => {}
        }
        limit
    }

    /// Begins an entry of its own inside the running one, for a task that a joiner runs itself: it
    /// gets a fresh instruction allotment and a memory-limit state of its own (it starts `Normal`
    /// under the host's limit), as a coroutine's resumption does, so that what it uses up, and
    /// the grace after it, are not the joiner's. Gives what [`Core::end_inline_entry`] puts back.
    pub fn begin_inline_entry(&mut self) -> InlineEntry {
        let allotment = self.budget.allotment;
        let slice = self.safepoints.slice();
        self.close_budget();
        let fresh = Budget::fresh(allotment, slice, self.safepoints.progress);
        let budget = std::mem::replace(&mut self.budget, fresh);
        let memory = self.begin_resumption_memory();
        InlineEntry { budget, memory }
    }

    /// The inline entry is over: the joiner's budget and memory state come back. Gives the steps
    /// the entry used.
    pub fn end_inline_entry(&mut self, saved: InlineEntry) -> u64 {
        let steps = self.budget.used();
        self.end_resumption_memory(saved.memory);
        self.close_budget();
        self.budget = saved.budget;
        self.sync_slice();
        self.open_budget();
        steps
    }

    /// A coroutine resumption of the scheduler gets a memory state of its own, as it gets a budget
    /// of its own: it starts `Normal` under the host's limit, so one coroutine's breach (and its
    /// grace) does not decide the fate of the others in the same tick. Gives what to restore.
    pub fn begin_resumption_memory(&mut self) -> SavedMemState {
        let saved = SavedMemState {
            state: self.memlimit.state,
            grace_limit: self.memlimit.grace_limit,
        };
        if self.memlimit.limit.is_some() {
            self.memlimit.state = MemState::Normal;
            self.mem.set_limit(self.memlimit.limit);
        }
        saved
    }

    /// The resumption is over: the interrupted entry's state comes back.
    pub fn end_resumption_memory(&mut self, saved: SavedMemState) {
        if self.memlimit.limit.is_none() {
            return;
        }
        self.memlimit.state = saved.state;
        self.memlimit.grace_limit = saved.grace_limit;
        let limit = match saved.state {
            MemState::Normal => self.memlimit.limit,
            _ => Some(saved.grace_limit),
        };
        self.mem.set_limit(limit);
    }

    /// Whether the current entry passed the memory limit (even if the program swallowed the
    /// panic).
    #[inline]
    pub fn memory_was_exhausted(&self) -> bool {
        self.memlimit.state != MemState::Normal
    }

    /// Whether remaining `defer` statements and `Drop` hooks are skipped: the grace of an
    /// exhausted budget or of the memory limit is used up.
    #[inline]
    pub fn cleanups_are_dead(&self) -> bool {
        self.budget.state == BudgetState::Dead || self.memlimit.state == MemState::Dead
    }

    /// What the current entry ran out of, if anything (the budget first).
    pub fn exhaustion(&self) -> Option<Exhaustion> {
        if self.budget_was_exhausted() {
            Some(Exhaustion::Budget)
        } else if self.memory_was_exhausted() {
            Some(Exhaustion::Memory(self.memlimit.limit.unwrap_or(0)))
        } else {
            None
        }
    }
}

impl Core {
    /// Closes the window of the budget and credits what it spent to the countdown of the slice,
    /// so that the next budget (the next entry, another coroutine) carries on from there.
    #[inline]
    pub(crate) fn close_budget(&mut self) {
        if self.safepoints.slice().is_none() {
            return;
        }
        let spent = self.budget.close();
        self.safepoints.credit(spent);
    }

    /// Opens the window of a closed budget where the countdown of the slice stands.
    #[inline]
    pub(crate) fn open_budget(&mut self) {
        if self.safepoints.slice().is_none() {
            return;
        }
        self.budget.open(self.safepoints.progress);
    }

    /// The budget really ran out: the grace begins (or ends).
    pub(crate) fn exhaust_budget(&mut self) {
        self.close_budget();
        self.budget.exhaust(self.safepoints.progress);
    }
}
