//! The data of the coroutine scheduler (DESIGN §14.2, ARCHITECTURE §16): which coroutines
//! exist, what each waits for, the clock and the background operations. It knows nothing of how
//! a coroutine is run. An engine keeps the running state of each coroutine itself (the
//! tree-walker: a native stack) and is asked to run it through [`crate::engine::Engine`].

use std::collections::HashMap;
use std::rc::{Rc, Weak};

use colibri_check::types::{COGROUP_TYPE, COROUTINE_TYPE, SIGNAL_TYPE};

use crate::clock::{Clock, SystemClock};
use crate::ctl::PanicInfo;
use crate::external::Externals;
use crate::fiber::FiberState;
use crate::heap::ClassObj;
use crate::value::Value;

/// The default size of the native stack of one coroutine.
pub(crate) const DEFAULT_COROUTINE_STACK: usize = 16 * 1024 * 1024;

/// Slack when comparing times, so `wait(0.1)` five times in a row lands on the same tick as
/// `wait(0.5)` despite rounding.
pub(crate) const TIME_EPSILON: f64 = 1e-9;

/// How a coroutine ended.
#[doc(hidden)]
pub enum Finish {
    Done(Value),
    Panicked(Box<PanicInfo>),
    Cancelled,
}

/// When a suspended coroutine is due.
#[doc(hidden)]
pub enum Wake {
    /// At every tick (`wait_until` polls its condition on the coroutine itself).
    Ready,
    /// When scheduler time reaches this.
    Time(f64),
    /// When the real clock reaches this.
    Real(f64),
    /// When this coroutine handle is finished.
    Join(Weak<ClassObj>),
    /// `join_result` on the coroutine with this id: no polling, the coroutine is in the
    /// target's joiner list and is woken (as `Signaled`) when the target ends.
    JoinResult(u64),
    /// Waiting for this signal (no polling: the coroutine is in the signal's waiter list).
    Signal(u64),
    /// The signal was emitted; the coroutine resumes at the next tick, in emission order.
    Signaled,
    /// `join_all` on this group: when no other coroutine of it is alive.
    Group(u64),
    /// A background operation (`Interp::await_external`): when its thread has finished. Needs
    /// no polling: the event loop sleeps until the thread posts its completion.
    External(u64),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[doc(hidden)]
pub enum State {
    Suspended,
    Running,
    Finished,
}

/// What an engine needs to find its side of a coroutine, kept by the scheduler and handed back
/// at every resumption. The scheduler does not look inside: for the tree-walker it is the slot
/// of the coroutine's native stack; for an engine with heap frames it could be the frame list.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EngineSlot(pub u32);

/// What the scheduler knows of one coroutine.
pub(crate) struct Entry {
    pub(crate) id: u64,
    pub(crate) state: State,
    pub(crate) wake: Wake,
    pub(crate) handle: Weak<ClassObj>,
    pub(crate) cancel_requested: bool,
    /// The engine's side of the coroutine, opaque here.
    pub(crate) slot: EngineSlot,
    /// The `CoGroup` that owns this coroutine.
    pub(crate) group: Option<u64>,
    /// The tick in which this coroutine was already resumed (as a signal waiter).
    pub(crate) ticked: u64,
}

/// The per-`Interp` coroutine scheduler.
#[doc(hidden)]
pub struct Scheduler {
    pub(crate) entries: Vec<Entry>,
    pub(crate) next_id: u64,
    /// Scheduler time: the sum of the `dt` of the ticks so far.
    pub(crate) time: f64,
    /// How many ticks have run.
    pub(crate) frame: u64,
    pub(crate) in_tick: bool,
    /// The coroutines whose stacks are active, outermost first.
    pub(crate) running: Vec<u64>,
    pub(crate) failures: Vec<(u64, Box<PanicInfo>)>,
    pub(crate) finished: usize,
    pub(crate) shutting_down: bool,
    pub(crate) stack_size: usize,
    pub(crate) clock: Box<dyn Clock>,
    /// The coroutines waiting for each signal, in wait order.
    pub(crate) signals: HashMap<u64, Vec<u64>>,
    /// The coroutines waiting in `join_result` for each coroutine, in wait order.
    pub(crate) joiners: HashMap<u64, Vec<u64>>,
    /// Coroutines whose signal was emitted, in the order they will resume next tick.
    pub(crate) signaled: Vec<u64>,
    /// The live coroutines of each `CoGroup`, in start order.
    pub(crate) groups: HashMap<u64, Vec<u64>>,
    /// How many times any coroutine was resumed (for tests and tuning).
    pub(crate) resumptions: u64,
    pub(crate) ext: Externals,
}

impl Scheduler {
    pub fn new() -> Self {
        Scheduler {
            entries: Vec::new(),
            next_id: 1,
            time: 0.0,
            frame: 0,
            in_tick: false,
            running: Vec::new(),
            failures: Vec::new(),
            finished: 0,
            shutting_down: false,
            stack_size: DEFAULT_COROUTINE_STACK,
            clock: Box::new(SystemClock::new()),
            signals: HashMap::new(),
            joiners: HashMap::new(),
            signaled: Vec::new(),
            groups: HashMap::new(),
            resumptions: 0,
            ext: Externals::new(),
        }
    }

    pub(crate) fn find(&self, id: u64) -> Option<usize> {
        self.entries.binary_search_by_key(&id, |e| e.id).ok()
    }

    /// A fresh id, for a coroutine, a `CoGroup`, a `Signal` or a background operation. Ids
    /// only grow, so `entries` stays sorted by id.
    pub fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Records the coroutine `id` (from [`Scheduler::alloc_id`]) as started and not yet run: it
    /// is due at the next tick, and belongs to `group` if it has one. `slot` is whatever the
    /// engine needs to find its side of the coroutine; it is handed back to
    /// [`crate::engine::Engine::resume_stack`] at every resumption.
    pub fn register(
        &mut self,
        id: u64,
        handle: Weak<ClassObj>,
        group: Option<u64>,
        slot: EngineSlot,
    ) {
        self.entries.push(Entry {
            id,
            state: State::Suspended,
            wake: Wake::Ready,
            handle,
            cancel_requested: false,
            slot,
            group,
            ticked: 0,
        });
        if let Some(g) = group {
            self.groups.entry(g).or_default().push(id);
        }
    }

    /// The bookkeeping of a coroutine that is about to pause until `wake`: `Err` when it must
    /// not (it is cancelled, or the scheduler does not know it) and `fiber` is marked
    /// cancelling; otherwise it is recorded as waiting. The engine then does the pausing
    /// itself (the tree-walker switches stacks; a VM returns to its loop).
    #[inline]
    pub fn prepare_suspend(
        &mut self,
        id: u64,
        fiber: &mut FiberState,
        wake: Wake,
    ) -> Result<(), Cancelled> {
        let Some(idx) = self.find(id) else {
            return Err(Cancelled);
        };
        if fiber.cancelling || self.entries[idx].cancel_requested {
            fiber.cancelling = true;
            return Err(Cancelled);
        }
        match &wake {
            Wake::Signal(s) => self.signals.entry(*s).or_default().push(id),
            Wake::JoinResult(target) => self.joiners.entry(*target).or_default().push(id),
            _ => {}
        }
        self.entries[idx].wake = wake;
        Ok(())
    }
}

impl Scheduler {
    // ---- configuration and inspection ----

    /// The size of the native stack given to each coroutine (the engine's, if it uses one).
    pub fn stack_size(&self) -> usize {
        self.stack_size
    }

    /// Sets the size of the native stack given to each coroutine (at least 16 KiB).
    pub fn set_stack_size(&mut self, bytes: usize) {
        self.stack_size = bytes.max(16 * 1024);
    }

    /// Replaces the clock behind `wait_real` and the event loop.
    pub fn set_clock(&mut self, clock: Box<dyn Clock>) {
        self.clock = clock;
    }

    /// The reading of the clock, in seconds.
    pub fn clock_now(&mut self) -> f64 {
        self.clock.now()
    }

    /// Sleeps on the clock; a background operation that finishes ends the sleep early.
    pub fn clock_sleep(&mut self, seconds: f64) {
        if !self.ext.has_pending() {
            self.clock.sleep(seconds);
        } else {
            // Background operations are running: a completion ends the sleep early.
            let ext = &mut self.ext;
            self.clock
                .sleep_interruptible(seconds, &mut |d| ext.wait(d));
        }
    }

    /// The background operations: the pool, the tokens and the completions.
    pub fn ext(&mut self) -> &mut Externals {
        &mut self.ext
    }

    /// How many coroutines are registered as waiting for a signal or for another coroutine
    /// (for tests: a coroutine that is cancelled or woken must leave every list).
    pub fn waiter_count(&self) -> usize {
        self.signals.values().map(Vec::len).sum::<usize>()
            + self.joiners.values().map(Vec::len).sum::<usize>()
    }

    /// Scheduler time: the sum of the `dt` of the ticks so far.
    pub fn time(&self) -> f64 {
        self.time
    }

    /// How many coroutines are alive (started and not yet finished).
    pub fn coroutine_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| e.state != State::Finished)
            .count()
    }

    /// How many coroutine resumptions have happened (starts, ticks and cancellations).
    pub fn resumptions(&self) -> u64 {
        self.resumptions
    }

    /// Whether a coroutine is running now (on any level of a resume chain).
    pub fn coroutine_running(&self) -> bool {
        !self.running.is_empty()
    }

    /// Whether a tick is running or a coroutine is: the scheduler cannot be ticked or driven.
    pub fn is_busy(&self) -> bool {
        self.in_tick || self.coroutine_running()
    }

    /// Whether the program is being torn down: nothing new may start.
    pub fn is_shutting_down(&self) -> bool {
        self.shutting_down
    }

    /// Whether the `CoGroup` `group` has a live member other than `me`.
    pub fn group_has_others(&self, group: u64, me: Option<u64>) -> bool {
        self.groups
            .get(&group)
            .is_some_and(|m| m.iter().any(|c| Some(*c) != me))
    }

    /// How long until the next coroutine needs a tick, for an event loop: `None` when nothing
    /// is alive or every live coroutine waits for a signal (nothing but an emit can wake it),
    /// a small polling interval (`poll`) when something is checked at every tick (`wait_until`,
    /// `join`), else the time to the earliest timed wake.
    pub fn next_wake(&mut self, poll: f64) -> Option<f64> {
        let mut best: Option<f64> = None;
        let mut real_now: Option<f64> = None;
        let time = self.time;
        let mut any = false;
        for e in &self.entries {
            if e.state == State::Finished {
                continue;
            }
            // A coroutine waiting for a signal needs no tick of its own: only an emit (from
            // code that runs at a tick or before it) can wake it.
            if e.state == State::Suspended
                && matches!(e.wake, Wake::Signal(_) | Wake::JoinResult(_))
            {
                continue;
            }
            any = true;
            let delay = match &e.wake {
                Wake::Signaled => 0.0,
                Wake::Time(d) => (d - time).max(0.0),
                Wake::Real(d) => {
                    let now = *real_now.get_or_insert_with(|| self.clock.now());
                    (d - now).max(0.0)
                }
                // Only a completion (which ends the sleep) can wake it.
                Wake::External(_) => f64::INFINITY,
                Wake::Ready
                | Wake::Join(_)
                | Wake::Group(_)
                | Wake::Signal(_)
                | Wake::JoinResult(_) => poll,
            };
            best = Some(best.map_or(delay, |b| b.min(delay)));
        }
        if any {
            best
        } else if !self.ext.has_pending() {
            None
        } else {
            // The main fiber waits for a background operation: only its completion helps.
            Some(f64::INFINITY)
        }
    }

    // ---- bookkeeping ----

    /// Drops the entries of finished coroutines.
    pub fn compact(&mut self) {
        if self.finished > 0 {
            self.entries.retain(|e| e.state != State::Finished);
            self.finished = 0;
        }
    }

    /// After a start: compacts when many finished entries make up most of the list (and no
    /// tick is running, whose loop indexes the entries).
    pub fn compact_if_crowded(&mut self) {
        if !self.in_tick && self.finished > 32 && self.finished * 2 > self.entries.len() {
            self.compact();
        }
    }

    /// The panics of coroutines that nobody has collected, as one failure (the first, the
    /// others as notes); `Ok` when there are none.
    pub fn take_failures(&mut self) -> Result<(), TickError> {
        let mut failures = std::mem::take(&mut self.failures).into_iter();
        let Some((_, mut first)) = failures.next() else {
            return Ok(());
        };
        for (_, other) in failures {
            first.notes.push(format!(
                "another coroutine also panicked: {}",
                other.message
            ));
        }
        Err(TickError::Failure(first))
    }

    /// Forgets coroutine panics nobody collected (between tests).
    pub fn forget_failures(&mut self) {
        self.failures.clear();
    }

    /// Forgets the panic of the coroutine `id`: whoever joined it reports it instead.
    pub fn forget_failure_of(&mut self, id: u64) {
        self.failures.retain(|(f, _)| *f != id);
    }

    /// The coroutine `target` has ended: every coroutine waiting in `join_result` on it is
    /// woken the way a signal's waiters are (resumed at the start of the next tick, in the
    /// order they began waiting, before the other due coroutines). Returns whether one of them
    /// will receive the target's outcome, which is what makes a panic "handed over".
    pub(crate) fn wake_joiners(&mut self, target: u64) -> bool {
        let Some(waiters) = self.joiners.remove(&target) else {
            return false;
        };
        let mut received = false;
        for id in waiters {
            if let Some(idx) = self.find(id) {
                let e = &mut self.entries[idx];
                if e.state == State::Suspended
                    && matches!(e.wake, Wake::JoinResult(t) if t == target)
                {
                    e.wake = Wake::Signaled;
                    self.signaled.push(id);
                    received = true;
                }
            }
        }
        received
    }

    /// A waiter that is cancelled leaves the list of what it waited for: no stale entry
    /// stays behind, and nothing wakes a coroutine that is gone.
    pub(crate) fn forget_waiter(&mut self, id: u64, wake: &Wake) {
        let (map, key) = match wake {
            Wake::Signal(sig) => (&mut self.signals, *sig),
            Wake::JoinResult(target) => (&mut self.joiners, *target),
            _ => return,
        };
        if let Some(list) = map.get_mut(&key) {
            if let Some(pos) = list.iter().rposition(|w| *w == id) {
                list.remove(pos);
            }
            if list.is_empty() {
                map.remove(&key);
            }
        }
    }

    /// The coroutine `id` is no longer a member of `group`.
    pub(crate) fn leave_group(&mut self, group: u64, id: u64) {
        if let Some(members) = self.groups.get_mut(&group) {
            if let Some(pos) = members.iter().rposition(|m| *m == id) {
                members.remove(pos);
            }
            if members.is_empty() {
                self.groups.remove(&group);
            }
        }
    }
}

/// The running coroutine is cancelled and must not pause.
#[doc(hidden)]
#[derive(Debug, Clone, Copy)]
pub struct Cancelled;

impl Default for Scheduler {
    fn default() -> Self {
        Self::new()
    }
}

/// Handle layout: `[id, status, result, name]`.
pub(crate) const H_ID: usize = 0;
pub const H_STATUS: usize = 1;
pub const H_RESULT: usize = 2;
pub(crate) const H_NAME: usize = 3;
pub const RUNNING: i64 = 0;
pub const DONE: i64 = 1;
pub const PANICKED: i64 = 2;
pub const CANCELLED: i64 = 3;

#[doc(hidden)]
pub fn handle_status(h: &ClassObj) -> i64 {
    match h.field(H_STATUS) {
        Some(Value::Int(s)) => s,
        _ => CANCELLED,
    }
}

#[doc(hidden)]
pub fn handle_id(h: &ClassObj) -> u64 {
    match h.field(H_ID) {
        Some(Value::Int(i)) => i as u64,
        _ => 0,
    }
}

#[doc(hidden)]
pub fn handle_name(h: &ClassObj) -> String {
    match h.field(H_NAME) {
        Some(Value::Str(s)) => s.to_string(),
        _ => "?".to_string(),
    }
}

#[doc(hidden)]
pub fn handle_of(v: &Value) -> Option<&Rc<ClassObj>> {
    match v {
        Value::Obj(o) if o.ty() == COROUTINE_TYPE => Some(o),
        _ => None,
    }
}

/// The id of a `CoGroup` value.
#[doc(hidden)]
pub fn group_id(v: &Value) -> Option<u64> {
    match v {
        Value::Obj(o) if o.ty() == COGROUP_TYPE => match o.field(0) {
            Some(Value::Int(i)) => Some(i as u64),
            _ => None,
        },
        _ => None,
    }
}

/// The id of a `Signal` value.
#[doc(hidden)]
pub fn signal_id(v: &Value) -> Option<u64> {
    match v {
        Value::Obj(o) if o.ty() == SIGNAL_TYPE => match o.field(0) {
            Some(Value::Int(i)) => Some(i as u64),
            _ => None,
        },
        _ => None,
    }
}

/// How driving the scheduler from the main fiber ended.
#[doc(hidden)]
pub enum Drive {
    Done,
    Failure(Box<PanicInfo>),
    /// Nothing is scheduled that could end the wait.
    Stuck,
}

/// Why a tick did not happen or did not end well.
#[doc(hidden)]
pub enum TickError {
    /// `tick` was called from a coroutine, or from inside a tick.
    Reentrant,
    /// A coroutine panicked (the first failure, the others as notes).
    Failure(Box<PanicInfo>),
}
