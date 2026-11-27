//! The data of the coroutine scheduler (DESIGN §14.2, ARCHITECTURE §16): which coroutines
//! exist, what each waits for, the clock and the background operations. It knows nothing of how
//! a coroutine is run. An engine keeps the running state of each coroutine itself and is asked
//! to run it through [`crate::engine::Engine`].

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};
use std::rc::{Rc, Weak};
use std::sync::Arc;

use colibri_check::types::{COGROUP_TYPE, COROUTINE_TYPE, SIGNAL_TYPE};

use crate::clock::{Clock, SystemClock};
use crate::ctl::PanicInfo;
use crate::external::{
    CancelToken, Completer, ExternalError, Externals, Waited, panic_message, unwrap_outcome,
};
use crate::fiber::FiberState;
use crate::heap::ClassObj;
use crate::value::Value;

/// Why a wait that cannot block fails (the web: the page's thread must never block).
const NO_BLOCKING: &str =
    "nothing can complete this background operation without blocking the thread";

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
    /// `join` on the coroutine with this id (no polling: the coroutine is in the target's
    /// joiner list and is woken, as `Woken`, when the target ends).
    Join(u64),
    /// `join_result` on the coroutine with this id: woken, as `Joined`, when the target ends,
    /// and the waiter takes over the target's panic.
    JoinResult(u64),
    /// Waiting for this signal (no polling: the coroutine is in the signal's waiter list).
    Signal(u64),
    /// The signal was emitted; the coroutine resumes at the next tick, in the order it was
    /// woken (a `join_result` waiter is queued the same way, as `Joined`).
    Signaled,
    /// The coroutine a `join` waited for ended, or the group a `join_all` waited for is
    /// empty of others: due in the ordinary pass of a tick, in start order, so in the tick
    /// that is running if the coroutine starts after the one that woke it, else in the next.
    /// (Exactly when the polling check used to find it due.)
    Woken,
    /// A `join_result` waiter woken by the end of this coroutine, queued like `Signaled`.
    /// While it has not run, a panic of that coroutine stays in the failure queue (a report
    /// skips it), so a waiter that is cancelled before it runs does not lose the panic.
    Joined(u64),
    /// `join_all` on this group (no polling: the coroutine is in the group's waiter list and
    /// is woken, as `Woken`, when no other coroutine of the group is alive).
    Group(u64),
    /// A background operation (`Machine::await_external`): when its thread has finished. Needs
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
/// at every resumption. The scheduler does not look inside: it can be the slot
/// of a native stack, or the frame list of an engine with heap frames.
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

/// The hasher of the scheduler's tables, which are keyed by the ids it hands out itself: small
/// integers that grow by one. One multiply spreads them; SipHash's guard against chosen keys
/// buys nothing here and cost a tenth of a coroutine-heavy program.
#[derive(Default)]
pub(crate) struct IdHasher(u64);

impl Hasher for IdHasher {
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.write_u64(u64::from(b));
        }
    }

    fn write_u64(&mut self, id: u64) {
        let x = (self.0 ^ id).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        self.0 = x ^ (x >> 32);
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

/// A table keyed by scheduler ids.
pub(crate) type IdMap<V> = HashMap<u64, V, BuildHasherDefault<IdHasher>>;

/// What a joiner's scheduler was doing when a task began running inline in its join.
pub(crate) struct SchedScope {
    floor: u64,
    in_tick: bool,
    running: Vec<u64>,
    failures: Vec<(u64, Box<PanicInfo>)>,
    signaled: Vec<u64>,
}

/// The per-`Core` coroutine scheduler.
#[doc(hidden)]
pub struct Scheduler {
    pub(crate) entries: Vec<Entry>,
    pub(crate) next_id: u64,
    /// Scheduler time: the sum of the `dt` of the ticks so far.
    pub(crate) time: f64,
    /// How many ticks have run.
    pub(crate) frame: u64,
    pub(crate) in_tick: bool,
    /// Ticks and the event loop see only the coroutines with an id of `floor` or more: a task
    /// that its joiner runs inline has the scheduler to itself (see [`Scheduler::enter_scope`]).
    pub(crate) floor: u64,
    /// The coroutines whose stacks are active, outermost first.
    pub(crate) running: Vec<u64>,
    pub(crate) failures: Vec<(u64, Box<PanicInfo>)>,
    pub(crate) finished: usize,
    pub(crate) shutting_down: bool,
    pub(crate) stack_size: usize,
    pub(crate) clock: Box<dyn Clock>,
    /// Whether `clock_sleep` measures the real time it takes (a time profile asks for it).
    pub(crate) measure_idle: bool,
    /// The real nanoseconds spent in `clock_sleep` since `measure_idle` was turned on.
    pub(crate) idle_nanos: u64,
    /// The coroutines waiting for each signal, in wait order.
    pub(crate) signals: IdMap<Vec<u64>>,
    /// The coroutines waiting in `join` or `join_result` for each coroutine, in wait order.
    pub(crate) joiners: IdMap<Vec<u64>>,
    /// The coroutines waiting in `join_all` for each `CoGroup`, in wait order.
    pub(crate) group_waiters: IdMap<Vec<u64>>,
    /// Coroutines whose signal was emitted, in the order they will resume next tick.
    pub(crate) signaled: Vec<u64>,
    /// The live coroutines of each `CoGroup`, in start order.
    pub(crate) groups: IdMap<Vec<u64>>,
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
            floor: 0,
            running: Vec::new(),
            failures: Vec::new(),
            finished: 0,
            shutting_down: false,
            stack_size: DEFAULT_COROUTINE_STACK,
            clock: Box::new(SystemClock::new()),
            measure_idle: false,
            idle_nanos: 0,
            signals: IdMap::default(),
            joiners: IdMap::default(),
            group_waiters: IdMap::default(),
            signaled: Vec::new(),
            groups: IdMap::default(),
            resumptions: 0,
            ext: Externals::new(),
        }
    }

    /// A scope of its own for a task that runs inline in a coroutine or in the main fiber: only
    /// coroutines started from now on are ticked, waited for and driven; the ones that exist
    /// wait (their timers go on, they do not run), and the task is the main fiber of the scope,
    /// whatever the joiner was doing. [`Scheduler::leave_scope`] gives the joiner's view back.
    pub(crate) fn enter_scope(&mut self) -> SchedScope {
        SchedScope {
            floor: std::mem::replace(&mut self.floor, self.next_id),
            in_tick: std::mem::replace(&mut self.in_tick, false),
            running: std::mem::take(&mut self.running),
            failures: std::mem::take(&mut self.failures),
            signaled: std::mem::take(&mut self.signaled),
        }
    }

    pub(crate) fn leave_scope(&mut self, saved: SchedScope) {
        self.floor = saved.floor;
        self.in_tick = saved.in_tick;
        self.running = saved.running;
        self.failures = saved.failures;
        self.signaled = saved.signaled;
    }

    pub(crate) fn find(&self, id: u64) -> Option<usize> {
        self.entries.binary_search_by_key(&id, |e| e.id).ok()
    }

    /// The id the next coroutine, group, signal or operation gets.
    pub fn next_id(&self) -> u64 {
        self.next_id
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
    /// itself (the VM returns to its loop).
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
            Wake::Join(target) | Wake::JoinResult(target) => {
                self.joiners.entry(*target).or_default().push(id)
            }
            Wake::Group(g) => self.group_waiters.entry(*g).or_default().push(id),
            _ => {}
        }
        self.entries[idx].wake = wake;
        Ok(())
    }
}

/// What the caller of a background operation waits on: the operation's id, its token and the
/// receiver of its result.
pub struct Pending<R> {
    pub op: u64,
    pub token: Arc<CancelToken>,
    rx: std::sync::mpsc::Receiver<Result<R, String>>,
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
        // A sleep of nothing (the event loop yielding) is not worth two clock readings.
        let began = (self.measure_idle && seconds > 0.0).then(crate::clock::Stopwatch::start);
        self.sleep_on_clock(seconds);
        if let Some(began) = began {
            self.idle_nanos = self.idle_nanos.saturating_add(began.elapsed_nanos());
        }
    }

    /// Asks `clock_sleep` to add up the real time it spends sleeping (for a time profile).
    pub fn measure_idle(&mut self, on: bool) {
        self.measure_idle = on;
    }

    /// The real nanoseconds `clock_sleep` has spent sleeping while it was measured.
    pub fn idle_nanos(&self) -> u64 {
        self.idle_nanos
    }

    fn sleep_on_clock(&mut self, seconds: f64) {
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

    // ---- the engine-neutral half of waiting for a background operation ----
    //
    // The wait itself (suspend, drive the event loop, block) is the engine's; what comes before
    // and after it is here, so every engine and the runtime's `Ctx` share it.

    /// Registers a new operation and makes the end that completes it.
    pub fn begin_completion<R>(&mut self) -> (Completer<R>, Pending<R>) {
        let op = self.alloc_id();
        let (completer, rx) = self.ext.begin::<R>(op);
        let token = Arc::clone(completer.token());
        (completer, Pending { op, token, rx })
    }

    /// Runs `work` on a background thread (an idle one of the pool, or a new one) and returns
    /// what the caller waits on. `Err(CannotStart)`: the pool would not take it.
    pub fn submit_work<R: Send + 'static>(
        &mut self,
        work: impl FnOnce(&CancelToken) -> R + Send + 'static,
    ) -> Result<Pending<R>, ExternalError> {
        let (completer, pending) = self.begin_completion::<R>();
        let op = pending.op;
        let queued = self.ext.submit(Box::new(move || {
            // Set when the waiter gives up, so an operation still in the queue is not run.
            if completer.token().is_cancelled() {
                return;
            }
            let token = Arc::clone(completer.token());
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(&token)));
            match outcome {
                Ok(value) => completer.complete(value),
                Err(payload) => completer.fail(panic_message(payload.as_ref())),
            }
        }));
        if let Err(message) = queued {
            self.ext.forget(op);
            return Err(ExternalError::CannotStart(message));
        }
        Ok(pending)
    }

    /// Makes an operation that some other thread completes: `start` runs at once and is given
    /// the [`Completer`]. A `start` that fails or panics leaves nothing of the operation
    /// behind: its entries go, and the hooks it registered on the token run (a panic goes on).
    pub fn start_completion<R: Send + 'static>(
        &mut self,
        start: impl FnOnce(Completer<R>) -> Result<(), String>,
    ) -> Result<Pending<R>, ExternalError> {
        let (completer, pending) = self.begin_completion::<R>();
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| start(completer))) {
            Ok(Ok(())) => Ok(pending),
            Ok(Err(message)) => {
                self.ext.forget(pending.op);
                pending.token.cancel();
                Err(ExternalError::CannotStart(message))
            }
            Err(payload) => {
                self.ext.forget(pending.op);
                // The dropped completer has posted its id; collecting it later finds the
                // operation no longer pending and ignores it. A hook that panics too does not
                // replace the panic of `start`, which is the one that is passed on.
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    pending.token.cancel()
                }));
                std::panic::resume_unwind(payload);
            }
        }
    }

    /// The result of `pending` once the engine's wait has ended with `waited`.
    ///
    /// A [`Waited::Blocked`] wait blocks this thread, except on the web, where the only thread
    /// is the page's and nothing else could complete the operation: see
    /// [`Scheduler::collect_with`].
    pub fn collect<R>(&mut self, pending: Pending<R>, waited: Waited) -> Result<R, ExternalError> {
        self.collect_with(pending, waited, !cfg!(target_family = "wasm"))
    }

    /// [`Scheduler::collect`], where `may_block` says whether a blocked wait may block the
    /// thread. When it may not, a result that is already there is taken, and otherwise the wait
    /// fails with an error (it would never end).
    pub fn collect_with<R>(
        &mut self,
        pending: Pending<R>,
        waited: Waited,
        may_block: bool,
    ) -> Result<R, ExternalError> {
        match waited {
            Waited::Blocked => {
                // Nothing could run meanwhile: block this thread until the result is there.
                // Every end of a completer sends or drops the result's sender, so this does not
                // hang.
                let blocked = if may_block {
                    pending.rx.recv().map_err(|_| ())
                } else {
                    match pending.rx.try_recv() {
                        Ok(outcome) => Ok(outcome),
                        // The completer ended without a result: the usual "no result" error.
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => Err(()),
                        Err(std::sync::mpsc::TryRecvError::Empty) => {
                            self.ext.forget(pending.op);
                            return Err(ExternalError::Failed(NO_BLOCKING.to_string()));
                        }
                    }
                };
                self.ext.forget(pending.op);
                unwrap_outcome(blocked)
            }
            Waited::Finished => unwrap_outcome(pending.rx.try_recv().map_err(|_| ())),
        }
    }

    /// How many coroutines are registered as waiting for a signal, a coroutine or a group
    /// (for tests: a coroutine that is cancelled or woken must leave every list).
    pub fn waiter_count(&self) -> usize {
        self.signals.values().map(Vec::len).sum::<usize>()
            + self.joiners.values().map(Vec::len).sum::<usize>()
            + self.group_waiters.values().map(Vec::len).sum::<usize>()
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

    /// For a debugger: the coroutines that have not finished, in start order, each with the name
    /// of its handle while the program still holds the handle.
    pub fn debug_coroutines(&self) -> Vec<(u64, Option<String>)> {
        self.entries
            .iter()
            .filter(|e| e.state != State::Finished)
            .map(|e| (e.id, e.handle.upgrade().map(|h| handle_name(&h))))
            .collect()
    }

    /// The coroutines whose stacks are active now, outermost first (each resumed the next).
    pub fn running_coroutines(&self) -> &[u64] {
        &self.running
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
    /// is alive or every live coroutine waits for an event (a signal, the end of a coroutine,
    /// the end of a group: nothing but code that runs at a tick or before it can wake it), a
    /// small polling interval (`poll`) when something is checked at every tick (`wait_until`
    /// only), else the time to the earliest timed wake.
    #[inline]
    pub fn next_wake(&mut self, poll: f64) -> Option<f64> {
        let mut best: Option<f64> = None;
        let mut real_now: Option<f64> = None;
        let time = self.time;
        let mut any = false;
        for e in &self.entries {
            if e.state == State::Finished || e.id < self.floor {
                continue;
            }
            // A coroutine waiting for an event (a signal, the end of a coroutine or of a group)
            // needs no tick of its own: only code that runs at a tick or before it can wake it.
            if e.state == State::Suspended
                && matches!(
                    e.wake,
                    Wake::Signal(_) | Wake::Join(_) | Wake::JoinResult(_) | Wake::Group(_)
                )
            {
                continue;
            }
            any = true;
            let delay = match &e.wake {
                Wake::Signaled | Wake::Woken | Wake::Joined(_) => 0.0,
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
        if self.failures.is_empty() {
            return Ok(());
        }
        // A panic is held back while a `join_result` waiter woken by that coroutine's end has
        // not run yet: the waiter takes it (`forget_failure_of`), or, cancelled first, releases
        // it for the next report. A budget panic is never held.
        let all = std::mem::take(&mut self.failures);
        let (held, report): (Vec<_>, Vec<_>) = all.into_iter().partition(|(id, info)| {
            !info.budget
                && self.entries.iter().any(|e| {
                    e.state == State::Suspended && matches!(e.wake, Wake::Joined(t) if t == *id)
                })
        });
        self.failures = held;
        let mut failures = report.into_iter();
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

    /// Forgets the panic of the coroutine `id` (not a budget panic): whoever joined it reports
    /// it instead.
    pub fn forget_failure_of(&mut self, id: u64) {
        // A budget panic is the host's limit: nobody can take it over.
        self.failures.retain(|(f, info)| *f != id || info.budget);
    }

    /// The coroutine `target` has ended, and its waiters are woken, in the order they began
    /// waiting. A `join_result` waiter is woken the way a signal's waiters are (queued: it
    /// resumes at the start of the next tick, before the other due coroutines). A `join`
    /// waiter becomes due in the ordinary pass of a tick ([`Wake::Woken`]), where the polling
    /// check found it. A panic of the target stays queued while a woken `join_result` waiter
    /// has not run ([`Wake::Joined`], see `take_failures`).
    pub(crate) fn wake_joiners(&mut self, target: u64) {
        // Most coroutines end with nobody waiting for them: no hashing then.
        if self.joiners.is_empty() {
            return;
        }
        let Some(waiters) = self.joiners.remove(&target) else {
            return;
        };
        for id in waiters {
            if let Some(idx) = self.find(id) {
                let e = &mut self.entries[idx];
                let result = matches!(e.wake, Wake::JoinResult(t) if t == target);
                if e.state != State::Suspended {
                    continue;
                }
                if result {
                    e.wake = Wake::Joined(target);
                    self.signaled.push(id);
                } else if matches!(e.wake, Wake::Join(t) if t == target) {
                    e.wake = Wake::Woken;
                }
            }
        }
    }

    /// A waiter that is cancelled leaves the list of what it waited for: no stale entry
    /// stays behind, and nothing wakes a coroutine that is gone.
    pub(crate) fn forget_waiter(&mut self, id: u64, wake: &Wake) {
        let (map, key) = match wake {
            Wake::Signal(sig) => (&mut self.signals, *sig),
            Wake::Join(target) | Wake::JoinResult(target) => (&mut self.joiners, *target),
            Wake::Group(g) => (&mut self.group_waiters, *g),
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
        self.wake_group_waiters(group);
    }

    /// A member left `group` (or the group was dropped): each coroutine waiting in `join_all`
    /// on it for which no other member is alive is woken ([`Wake::Woken`]).
    fn wake_group_waiters(&mut self, group: u64) {
        if self.group_waiters.is_empty() {
            return;
        }
        let Some(waiters) = self.group_waiters.get_mut(&group) else {
            return;
        };
        let members = self.groups.get(&group);
        let mut woken = Vec::new();
        waiters.retain(|w| {
            let others = members.is_some_and(|m| m.iter().any(|c| c != w));
            if !others {
                woken.push(*w);
            }
            others
        });
        if waiters.is_empty() {
            self.group_waiters.remove(&group);
        }
        for id in woken {
            if let Some(idx) = self.find(id) {
                let e = &mut self.entries[idx];
                if e.state == State::Suspended && matches!(e.wake, Wake::Group(g) if g == group) {
                    e.wake = Wake::Woken;
                }
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
