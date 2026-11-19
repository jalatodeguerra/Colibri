//! Coroutines and their scheduler (DESIGN §14.2, ARCHITECTURE §16).
//!
//! A coroutine is a [`Fiber`] running the ordinary recursive evaluator on its own native stack
//! (`corosensei`). `wait` and friends suspend that stack; the scheduler ([`Scheduler`], one per
//! `Interp`) resumes the ones that are due when the host calls `tick`. Cancellation resumes a
//! suspended coroutine with a cancel signal, so its pause returns `Err(Ctl::Cancel)` and the
//! ordinary unwinding (`exit_scope`) runs its `defer`s and drops. No Rust panic or forced unwind
//! is part of the semantics: every coroutine is run to its end before its stack is freed.
//!
//! The `Interp` must not move while a coroutine exists (the suspended stacks hold references
//! into it); the runtime keeps it in a `Box`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::{Duration, Instant};

use colibri_check::hir::{Callee, Expr, ExprKind, Intrinsic};
use colibri_check::types::{COGROUP_TYPE, COROUTINE_TYPE, SIGNAL_TYPE, Ty};
use colibri_syntax::Span;
use corosensei::stack::DefaultStack;
use corosensei::{Coroutine, CoroutineResult, Yielder};

use crate::budget::Budget;
use crate::ctl::{Ctl, Eval, PanicInfo};
use crate::eval::{Interp, stack_position};
use crate::fiber::Fiber;
use crate::heap::ClassObj;
use crate::value::Value;

/// The default size of the native stack of one coroutine.
pub const DEFAULT_COROUTINE_STACK: usize = 16 * 1024 * 1024;

/// How much of a coroutine's stack the evaluator may use before it panics with "stack
/// overflow": the rest is for host functions and built-ins, which are not measured.
const STACK_USABLE_NUM: usize = 3;
const STACK_USABLE_DEN: usize = 4;

/// Slack when comparing times, so `wait(0.1)` five times in a row lands on the same tick as
/// `wait(0.5)` despite rounding.
const TIME_EPSILON: f64 = 1e-9;

/// Where scheduler time comes from, for `wait_real` and for the event loop (`colibri run`).
pub trait Clock {
    /// Seconds since an arbitrary, fixed origin.
    fn now(&mut self) -> f64;
    /// Waits for `seconds` (a fake clock just moves forward).
    fn sleep(&mut self, seconds: f64);
    /// Waits for up to `seconds`, but returns early when `wake(timeout)` reports that an
    /// external operation finished (`wake` blocks for up to `timeout` real time and says
    /// whether something arrived). The event loop calls this instead of `sleep` while
    /// background operations are pending (ARCHITECTURE section 22). The default is for fake
    /// clocks: it gives real time a millisecond to deliver, then moves forward by at most that.
    fn sleep_interruptible(&mut self, seconds: f64, wake: &mut dyn FnMut(Duration) -> bool) {
        if !wake(Duration::from_millis(1)) {
            self.sleep(seconds.min(0.001));
        }
    }
}

/// The wall clock.
pub struct SystemClock {
    origin: Instant,
}

impl SystemClock {
    pub fn new() -> Self {
        SystemClock {
            origin: Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now(&mut self) -> f64 {
        self.origin.elapsed().as_secs_f64()
    }
    fn sleep(&mut self, seconds: f64) {
        if seconds > 0.0 {
            std::thread::sleep(clamp_sleep(seconds));
        }
    }
    fn sleep_interruptible(&mut self, seconds: f64, wake: &mut dyn FnMut(Duration) -> bool) {
        if seconds > 0.0 {
            wake(clamp_sleep(seconds));
        }
    }
}

/// A sleep length that cannot overflow `Duration` (an event loop waiting only for background
/// operations asks for an unbounded sleep; it is cut to a day and asked again).
fn clamp_sleep(seconds: f64) -> Duration {
    Duration::from_secs_f64(seconds.clamp(0.0, 86_400.0))
}

/// Why waiting for a background operation did not give a result.
#[derive(Debug)]
pub enum ExternalError {
    /// The waiting coroutine was cancelled: the operation is abandoned.
    Cancelled,
    /// The operation could not run or its thread panicked.
    Failed(String),
}

/// Tells a background operation that nobody waits for its result any more. Cancelling is
/// synchronous: the hooks given to [`CancelToken::on_cancel`] run on the cancelling side,
/// before it goes on. An operation that holds something that must not outlive its waiter (a
/// child process) registers a hook that releases it, instead of noticing a flag some time
/// later.
#[derive(Default)]
pub struct CancelToken {
    flag: AtomicBool,
    hooks: Mutex<Vec<Box<dyn FnOnce() + Send>>>,
}

impl CancelToken {
    /// Whether the waiter has given up.
    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    /// Runs `hook` when the operation is cancelled, on the cancelling thread; at once if it
    /// has been. A hook runs at most once and must not wait for the operation.
    pub fn on_cancel(&self, hook: impl FnOnce() + Send + 'static) {
        let mut hooks = self
            .hooks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Checked under the lock `cancel` takes, so a hook is never lost between the two.
        if self.is_cancelled() {
            drop(hooks);
            hook();
        } else {
            hooks.push(Box::new(hook));
        }
    }

    fn cancel(&self) {
        let hooks = {
            let mut hooks = self
                .hooks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            self.flag.store(true, Ordering::SeqCst);
            std::mem::take(&mut *hooks)
        };
        for hook in hooks {
            hook();
        }
    }
}

/// A piece of background work for the pool.
type Job = Box<dyn FnOnce() + Send>;

/// Idle workers kept for reuse; the ones above this many end after [`IDLE_LIFETIME`].
const CORE_THREADS: usize = 4;
/// How long a worker above the core waits for more work before it ends.
const IDLE_LIFETIME: Duration = Duration::from_secs(5);

/// What the pool and its workers share.
struct PoolState {
    /// Jobs an idle worker has been told about and not yet taken.
    queue: VecDeque<Job>,
    /// Workers waiting for a job.
    idle: usize,
    /// Workers alive.
    workers: usize,
    closed: bool,
}

struct PoolShared {
    state: Mutex<PoolState>,
    wake: Condvar,
}

impl PoolShared {
    fn lock(&self) -> std::sync::MutexGuard<'_, PoolState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// The threads that run background operations. Background operations mostly wait (for a
/// socket, a disk, a child process), and one may wait for another to be started, so there
/// is no upper limit: a job goes to an idle worker if there is one, and otherwise to a new
/// thread. It never waits behind a blocked worker. Idle workers are kept for reuse; those
/// above [`CORE_THREADS`] end after [`IDLE_LIFETIME`]. All end when the pool is dropped
/// (after the job they are running).
struct Pool {
    shared: Arc<PoolShared>,
}

impl Pool {
    fn new() -> Self {
        Pool {
            shared: Arc::new(PoolShared {
                state: Mutex::new(PoolState {
                    queue: VecDeque::new(),
                    idle: 0,
                    workers: 0,
                    closed: false,
                }),
                wake: Condvar::new(),
            }),
        }
    }

    /// Runs `job` on an idle worker, or on a new thread if none is idle.
    fn submit(&self, job: Job) -> Result<(), String> {
        let mut state = self.shared.lock();
        if state.closed {
            return Err("the background pool is closed".to_string());
        }
        if state.idle > state.queue.len() {
            state.queue.push_back(job);
            drop(state);
            self.shared.wake.notify_one();
            return Ok(());
        }
        // The job moves into the thread; a failed spawn drops it with the closure.
        let shared = Arc::clone(&self.shared);
        state.workers += 1;
        let spawned = std::thread::Builder::new()
            .name("colibri-async".to_string())
            .spawn(move || Pool::work(&shared, job));
        if let Err(e) = spawned {
            state.workers -= 1;
            return Err(format!("cannot start a background thread: {e}"));
        }
        Ok(())
    }

    fn work(shared: &PoolShared, first: Job) {
        let run = |job: Job| {
            // A job that panics must not take its worker with it (the closure `submit` is
            // given reports the panic to the waiter itself).
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
        };
        run(first);
        let mut state = shared.lock();
        loop {
            if let Some(job) = state.queue.pop_front() {
                drop(state);
                run(job);
                state = shared.lock();
                continue;
            }
            if state.closed {
                state.workers -= 1;
                return;
            }
            state.idle += 1;
            let (next, timeout) = shared
                .wake
                .wait_timeout(state, IDLE_LIFETIME)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state = next;
            state.idle -= 1;
            if timeout.timed_out() && state.queue.is_empty() && state.workers > CORE_THREADS {
                state.workers -= 1;
                return;
            }
        }
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        self.shared.lock().closed = true;
        self.shared.wake.notify_all();
    }
}

/// Background operations: pool threads that post their id here when they finish.
struct Externals {
    pool: Pool,
    tx: mpsc::Sender<u64>,
    rx: mpsc::Receiver<u64>,
    /// Started and not yet finished or abandoned.
    pending: HashSet<u64>,
    /// The tokens of the operations in `pending` that can still be cancelled. When the
    /// scheduler is dropped (the `Vm` ends) they are cancelled, so a child process does not
    /// outlive the program.
    tokens: HashMap<u64, Arc<CancelToken>>,
    /// Finished, not yet collected by the waiter.
    completed: HashSet<u64>,
}

impl Externals {
    fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        Externals {
            pool: Pool::new(),
            tx,
            rx,
            pending: HashSet::new(),
            tokens: HashMap::new(),
            completed: HashSet::new(),
        }
    }

    /// The operation is over for the waiter, whichever way.
    fn forget(&mut self, id: u64) {
        self.pending.remove(&id);
        self.tokens.remove(&id);
    }

    fn note(&mut self, id: u64) {
        if self.pending.remove(&id) {
            self.completed.insert(id);
        }
    }

    /// Collects every completion that has arrived, without waiting.
    fn drain(&mut self) {
        while let Ok(id) = self.rx.try_recv() {
            self.note(id);
        }
    }

    /// Waits up to `timeout` for one completion; whether one arrived.
    fn wait(&mut self, timeout: Duration) -> bool {
        match self.rx.recv_timeout(timeout) {
            Ok(id) => {
                self.note(id);
                self.drain();
                true
            }
            Err(_) => false,
        }
    }
}

impl Drop for Externals {
    fn drop(&mut self) {
        for (_, token) in self.tokens.drain() {
            token.cancel();
        }
    }
}

/// Tells the scheduler an operation ended, also when its thread panics.
struct Notify(mpsc::Sender<u64>, u64);

impl Drop for Notify {
    fn drop(&mut self) {
        let _ = self.0.send(self.1);
    }
}

/// What a resumption tells the coroutine.
pub(crate) struct Resume {
    pub interp: *mut Interp,
    pub cancel: bool,
}

type Y = Yielder<Resume, *mut Interp>;
type Co = Coroutine<Resume, *mut Interp, Finish, DefaultStack>;

/// How a coroutine ended.
pub(crate) enum Finish {
    Done(Value),
    Panicked(Box<PanicInfo>),
    Cancelled,
}

/// What a fiber knows about the coroutine it runs.
#[derive(Debug)]
pub(crate) struct CoroCtx {
    pub id: u64,
    yielder: *const Y,
}

/// When a suspended coroutine is due.
enum Wake {
    /// At every tick (`wait_until` polls its condition on the coroutine itself).
    Ready,
    /// When scheduler time reaches this.
    Time(f64),
    /// When the real clock reaches this.
    Real(f64),
    /// When this coroutine handle is finished.
    Join(Weak<ClassObj>),
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
enum State {
    Suspended,
    Running,
    Finished,
}

struct Entry {
    id: u64,
    state: State,
    wake: Wake,
    co: Option<Box<Co>>,
    handle: Weak<ClassObj>,
    cancel_requested: bool,
    stack_budget: usize,
    /// The `CoGroup` that owns this coroutine.
    group: Option<u64>,
    /// The tick in which this coroutine was already resumed (as a signal waiter).
    ticked: u64,
}

/// The per-`Interp` coroutine scheduler.
pub(crate) struct Scheduler {
    entries: Vec<Entry>,
    next_id: u64,
    /// Scheduler time: the sum of the `dt` of the ticks so far.
    time: f64,
    /// How many ticks have run.
    frame: u64,
    in_tick: bool,
    /// The coroutines whose stacks are active, outermost first.
    running: Vec<u64>,
    failures: Vec<(u64, Box<PanicInfo>)>,
    finished: usize,
    shutting_down: bool,
    pub stack_size: usize,
    clock: Box<dyn Clock>,
    /// The coroutines waiting for each signal, in wait order.
    signals: HashMap<u64, Vec<u64>>,
    /// Coroutines whose signal was emitted, in the order they will resume next tick.
    signaled: Vec<u64>,
    /// The live coroutines of each `CoGroup`, in start order.
    groups: HashMap<u64, Vec<u64>>,
    /// How many times any coroutine was resumed (for tests and tuning).
    resumptions: u64,
    ext: Externals,
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
            signaled: Vec::new(),
            groups: HashMap::new(),
            resumptions: 0,
            ext: Externals::new(),
        }
    }

    fn find(&self, id: u64) -> Option<usize> {
        self.entries.binary_search_by_key(&id, |e| e.id).ok()
    }
}

/// Handle layout: `[id, status, result, name]`.
const H_ID: usize = 0;
const H_STATUS: usize = 1;
const H_RESULT: usize = 2;
const H_NAME: usize = 3;
const RUNNING: i64 = 0;
const DONE: i64 = 1;
const PANICKED: i64 = 2;
const CANCELLED: i64 = 3;

fn handle_status(h: &ClassObj) -> i64 {
    match h.field(H_STATUS) {
        Some(Value::Int(s)) => s,
        _ => CANCELLED,
    }
}

fn handle_id(h: &ClassObj) -> u64 {
    match h.field(H_ID) {
        Some(Value::Int(i)) => i as u64,
        _ => 0,
    }
}

fn handle_name(h: &ClassObj) -> String {
    match h.field(H_NAME) {
        Some(Value::Str(s)) => s.to_string(),
        _ => "?".to_string(),
    }
}

fn handle_of(v: &Value) -> Option<&Rc<ClassObj>> {
    match v {
        Value::Obj(o) if o.ty() == COROUTINE_TYPE => Some(o),
        _ => None,
    }
}

/// The id of a `CoGroup` value.
fn group_id(v: &Value) -> Option<u64> {
    match v {
        Value::Obj(o) if o.ty() == COGROUP_TYPE => match o.field(0) {
            Some(Value::Int(i)) => Some(i as u64),
            _ => None,
        },
        _ => None,
    }
}

/// The id of a `Signal` value.
fn signal_id(v: &Value) -> Option<u64> {
    match v {
        Value::Obj(o) if o.ty() == SIGNAL_TYPE => match o.field(0) {
            Some(Value::Int(i)) => Some(i as u64),
            _ => None,
        },
        _ => None,
    }
}

const PANICKED_THREAD: &str = "the background operation failed (its thread panicked)";

/// The text of a panic payload (`panic!("..")` gives a `&str` or a `String`).
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "(no message)".to_string())
}

/// A finished operation's result, or why there is none.
fn unwrap_outcome<R>(outcome: Result<Result<R, String>, ()>) -> Result<R, ExternalError> {
    match outcome {
        Ok(Ok(r)) => Ok(r),
        Ok(Err(message)) => Err(ExternalError::Failed(format!(
            "the background operation panicked: {message}"
        ))),
        Err(_) => Err(ExternalError::Failed(PANICKED_THREAD.to_string())),
    }
}

/// How driving the scheduler from the main fiber ended.
enum Drive {
    Done,
    Failure(Box<PanicInfo>),
    /// Nothing is scheduled that could end the wait.
    Stuck,
}

/// Why a tick did not happen or did not end well.
pub enum TickError {
    /// `tick` was called from a coroutine, or from inside a tick.
    Reentrant,
    /// A coroutine panicked (the first failure, the others as notes).
    Failure(Box<PanicInfo>),
}

impl Interp {
    // ---- configuration and inspection ----

    /// The size of the native stack given to each coroutine (default 16 MiB).
    pub fn set_coroutine_stack_size(&mut self, bytes: usize) {
        self.sched.stack_size = bytes.max(16 * 1024);
    }

    /// Replaces the clock behind `wait_real` and the event loop.
    pub fn set_clock(&mut self, clock: Box<dyn Clock>) {
        self.sched.clock = clock;
    }

    /// The reading of the clock, in seconds.
    pub fn clock_now(&mut self) -> f64 {
        self.sched.clock.now()
    }

    /// Sleeps on the clock.
    pub fn clock_sleep(&mut self, seconds: f64) {
        let s = &mut self.sched;
        if s.ext.pending.is_empty() {
            s.clock.sleep(seconds);
        } else {
            // Background operations are running: a completion ends the sleep early.
            let ext = &mut s.ext;
            s.clock.sleep_interruptible(seconds, &mut |d| ext.wait(d));
        }
    }

    /// How many coroutines are alive (started and not yet finished).
    pub fn coroutine_count(&self) -> usize {
        self.sched
            .entries
            .iter()
            .filter(|e| e.state != State::Finished)
            .count()
    }

    /// How many coroutine resumptions have happened (starts, ticks and cancellations).
    pub fn coroutine_resumptions(&self) -> u64 {
        self.sched.resumptions
    }

    /// How long until the next coroutine needs a tick, for an event loop: `None` when nothing
    /// is alive or every live coroutine waits for a signal (nothing but an emit can wake it), `Some(0.0)`-ish small polling interval when something is checked at every tick
    /// (`wait_until`, `join`), else the time to the earliest timed wake.
    pub fn next_wake(&mut self, poll: f64) -> Option<f64> {
        let mut best: Option<f64> = None;
        let mut real_now: Option<f64> = None;
        let time = self.sched.time;
        let mut any = false;
        for e in &self.sched.entries {
            if e.state == State::Finished {
                continue;
            }
            // A coroutine waiting for a signal needs no tick of its own: only an emit (from
            // code that runs at a tick or before it) can wake it.
            if e.state == State::Suspended && matches!(e.wake, Wake::Signal(_)) {
                continue;
            }
            any = true;
            let delay = match &e.wake {
                Wake::Signaled => 0.0,
                Wake::Time(d) => (d - time).max(0.0),
                Wake::Real(d) => {
                    let now = *real_now.get_or_insert_with(|| self.sched.clock.now());
                    (d - now).max(0.0)
                }
                // Only a completion (which ends the sleep) can wake it.
                Wake::External(_) => f64::INFINITY,
                Wake::Ready | Wake::Join(_) | Wake::Group(_) | Wake::Signal(_) => poll,
            };
            best = Some(best.map_or(delay, |b| b.min(delay)));
        }
        if any {
            best
        } else if self.sched.ext.pending.is_empty() {
            None
        } else {
            // The main fiber waits for a background operation: only its completion helps.
            Some(f64::INFINITY)
        }
    }

    // ---- ticking ----

    /// Resumes every due coroutine once, in start order. A coroutine started during the tick
    /// is not resumed in it. Each resumption gets its own instruction budget.
    pub fn tick(&mut self, fiber: &mut Fiber, dt: f64) -> Result<(), Box<PanicInfo>> {
        let _ = fiber;
        let outermost = self.begin_entry();
        if outermost {
            self.drain_root_drops();
        }
        let result = self.tick_core(dt);
        self.end_entry(outermost, false);
        match result {
            Ok(()) => Ok(()),
            Err(TickError::Failure(info)) => Err(info),
            Err(TickError::Reentrant) => Err(Box::new(PanicInfo {
                message: "`tick` cannot be called from a coroutine or from inside a tick"
                    .to_string(),
                span: Span::new(colibri_syntax::FileId(0), 0, 0),
                stack: Vec::new(),
                notes: Vec::new(),
                budget: false,
            })),
        }
    }

    fn tick_core(&mut self, dt: f64) -> Result<(), TickError> {
        self.tick_run(dt)?;
        self.take_failures()
    }

    /// One tick, leaving the coroutines' failures queued.
    fn tick_run(&mut self, dt: f64) -> Result<(), TickError> {
        if self.sched.in_tick || !self.sched.running.is_empty() {
            return Err(TickError::Reentrant);
        }
        let dt = if dt.is_finite() && dt > 0.0 { dt } else { 0.0 };
        self.sched.in_tick = true;
        self.sched.ext.drain();
        self.sched.time += dt;
        self.sched.frame += 1;
        let mut real_now: Option<f64> = None;
        let count = self.sched.entries.len();
        // Coroutines whose signal was emitted before this tick resume first, in emission order.
        let queue = std::mem::take(&mut self.sched.signaled);
        let frame = self.sched.frame;
        for id in queue {
            let Some(idx) = self.sched.find(id) else {
                continue;
            };
            let e = &mut self.sched.entries[idx];
            if e.state == State::Suspended && matches!(e.wake, Wake::Signaled) {
                e.ticked = frame;
                self.resume_coro(id, false, true);
            }
        }
        for i in 0..count {
            let s = &mut self.sched;
            let e = &s.entries[i];
            if e.state != State::Suspended || e.ticked == s.frame {
                continue;
            }
            let due = match &e.wake {
                Wake::Signal(_) | Wake::Signaled => false,
                Wake::Group(g) => s.groups.get(g).is_none_or(|m| m.iter().all(|c| *c == e.id)),
                Wake::Ready => true,
                Wake::Time(d) => s.time + TIME_EPSILON >= *d,
                Wake::Real(d) => *real_now.get_or_insert_with(|| s.clock.now()) >= *d,
                Wake::Join(w) => w.upgrade().is_none_or(|h| handle_status(&h) != RUNNING),
                Wake::External(op) => s.ext.completed.contains(op),
            };
            if due {
                let id = e.id;
                self.resume_coro(id, false, true);
            }
        }
        self.sched.in_tick = false;
        self.compact();
        Ok(())
    }

    fn compact(&mut self) {
        if self.sched.finished > 0 {
            self.sched.entries.retain(|e| e.state != State::Finished);
            self.sched.finished = 0;
        }
    }

    fn take_failures(&mut self) -> Result<(), TickError> {
        let mut failures = std::mem::take(&mut self.sched.failures).into_iter();
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
    pub(crate) fn sched_forget_failures(&mut self) {
        self.sched.failures.clear();
    }

    /// Panics that no `tick` has reported yet (a coroutine that failed while starting or
    /// being cancelled), for the host to collect after the program ended.
    pub fn take_coroutine_failure(&mut self) -> Option<Box<PanicInfo>> {
        match self.take_failures() {
            Err(TickError::Failure(info)) => Some(info),
            _ => None,
        }
    }

    // ---- starting ----

    /// `go call(..)`: evaluates the call's arguments here, then runs the call on a new
    /// coroutine until it first pauses.
    pub(crate) fn eval_go(&mut self, fiber: &mut Fiber, call: &Expr, span: Span) -> Eval {
        self.eval_go_in(fiber, None, call, span)
    }

    /// `group.go(call(..))`: the group is evaluated first, then the arguments.
    pub(crate) fn eval_go_group(
        &mut self,
        fiber: &mut Fiber,
        group: &Expr,
        call: &Expr,
        span: Span,
    ) -> Eval {
        let g = self.eval_twin(fiber, group)?;
        let gid = group_id(&g);
        let result = self.eval_go_in(fiber, gid, call, span);
        self.release(g);
        result
    }

    fn eval_go_in(
        &mut self,
        fiber: &mut Fiber,
        group: Option<u64>,
        call: &Expr,
        span: Span,
    ) -> Eval {
        let ExprKind::Call(c) = &call.kind else {
            return Err(self.internal(fiber, span, "`go` without a call"));
        };
        let mut values = Vec::with_capacity(c.args.len());
        for arg in &c.args {
            match arg {
                colibri_check::hir::Arg::Value(e) => values.push(self.eval_twin(fiber, e)?),
                colibri_check::hir::Arg::Place(_) => {
                    return Err(self.internal(fiber, span, "`go` with a receiver place"));
                }
            }
        }
        let name = match &c.callee {
            Callee::Fn(id) => self.fns.get(id.0 as usize).map(|b| b.name.clone()),
            Callee::Host(id) => self.host.bodies.get(id.0 as usize).map(|b| b.name.clone()),
            Callee::Value => match values.first() {
                Some(Value::Fn(_, name)) => Some(name.to_string()),
                _ => Some("<closure>".to_string()),
            },
            _ => None,
        }
        .unwrap_or_else(|| "<method>".to_string());
        if let Some(frame) = fiber.frames.last_mut() {
            frame.span = span;
        }
        self.start_coroutine(
            fiber,
            c.callee.clone(),
            c.type_args.clone(),
            values,
            name,
            group,
            span,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn start_coroutine(
        &mut self,
        fiber: &mut Fiber,
        callee: Callee,
        type_args: Vec<Ty>,
        values: Vec<Value>,
        name: String,
        group: Option<u64>,
        span: Span,
    ) -> Eval {
        let id = self.sched.next_id;
        self.sched.next_id += 1;
        let mut fields = vec![
            Value::Int(id as i64),
            Value::Int(RUNNING),
            Value::Unit,
            Value::Str(Rc::from(name.as_str())),
        ];
        if self.sched.shutting_down {
            // Nothing new may start while the program is being torn down.
            self.release_all(values);
            fields[H_STATUS] = Value::Int(CANCELLED);
            return Ok(Value::Obj(ClassObj::new(COROUTINE_TYPE, fields)));
        }
        let handle = ClassObj::new(COROUTINE_TYPE, fields);
        let size = self.sched.stack_size;
        let stack = match DefaultStack::new(size) {
            Ok(s) => s,
            Err(e) => {
                self.release_all(values);
                self.release(Value::Obj(handle));
                let msg = format!("cannot allocate a coroutine stack of {size} bytes: {e}");
                return Err(self.panic(fiber, span, msg));
            }
        };
        let body = move |yielder: &Y, first: Resume| -> Finish {
            // SAFETY: the pointer comes from `resume_coro`, which holds the interpreter
            // for the whole resumption; the interpreter does not move while coroutines live.
            let interp: &mut Interp = unsafe { &mut *first.interp };
            let mut fiber = Fiber::new();
            fiber.stack_base = stack_position();
            fiber.coro = Some(CoroCtx {
                id,
                yielder: yielder as *const Y,
            });
            let result = if first.cancel {
                interp.release_all(values);
                Err(Ctl::Cancel)
            } else {
                interp.call_values(&mut fiber, &callee, &type_args, values, span)
            };
            fiber.frames.clear();
            interp.release_temps(&mut fiber, 0);
            match result {
                Ok(v) | Err(Ctl::Return(v)) => Finish::Done(v),
                Err(Ctl::Cancel) => Finish::Cancelled,
                Err(Ctl::Panic(info)) => Finish::Panicked(info),
                Err(Ctl::Break(_, v)) => {
                    interp.release(v);
                    Finish::Cancelled
                }
                Err(Ctl::Continue(_)) => Finish::Cancelled,
            }
        };
        let co: Co = Coroutine::with_stack(stack, body);
        let budget = size * STACK_USABLE_NUM / STACK_USABLE_DEN;
        self.sched.entries.push(Entry {
            id,
            state: State::Suspended,
            wake: Wake::Ready,
            co: Some(Box::new(co)),
            handle: Rc::downgrade(&handle),
            cancel_requested: false,
            stack_budget: budget,
            group,
            ticked: 0,
        });
        if let Some(g) = group {
            self.sched.groups.entry(g).or_default().push(id);
        }
        // The first run shares the starter's instruction budget: it is part of the starter's
        // own execution.
        self.resume_coro(id, false, false);
        if !self.sched.in_tick
            && self.sched.finished > 32
            && self.sched.finished * 2 > self.sched.entries.len()
        {
            self.compact();
        }
        Ok(Value::Obj(handle))
    }

    // ---- resuming ----

    /// Runs coroutine `id` until it pauses or ends. `fresh` gives the resumption an instruction
    /// budget of its own (the scheduler's ticks); a start shares its starter's.
    fn resume_coro(&mut self, id: u64, cancel: bool, fresh: bool) {
        let Some(idx) = self.sched.find(id) else {
            return;
        };
        let e = &mut self.sched.entries[idx];
        if e.state != State::Suspended {
            return;
        }
        let Some(mut co) = e.co.take() else {
            return;
        };
        e.state = State::Running;
        let stack_budget = e.stack_budget;
        if cancel && let Wake::Signal(s) = e.wake {
            // A cancelled waiter leaves the signal's list.
            if let Some(list) = self.sched.signals.get_mut(&s) {
                if let Some(pos) = list.iter().rposition(|w| *w == id) {
                    list.remove(pos);
                }
                if list.is_empty() {
                    self.sched.signals.remove(&s);
                }
            }
        }
        self.sched.resumptions += 1;
        self.sched.running.push(id);
        let saved_stack = std::mem::replace(&mut self.native_stack_budget, stack_budget);
        let saved_budget = fresh.then(|| {
            let allotment = self.budget.allotment;
            std::mem::replace(&mut self.budget, Budget::fresh(allotment))
        });
        let ptr: *mut Interp = self;
        let outcome = co.resume(Resume {
            interp: ptr,
            cancel,
        });
        std::hint::black_box(ptr);
        let exhausted = fresh && self.budget_was_exhausted();
        if let Some(b) = saved_budget {
            self.budget = b;
        }
        self.native_stack_budget = saved_stack;
        self.sched.running.pop();
        let Some(idx) = self.sched.find(id) else {
            // Cannot happen (nothing removes a running entry); free the stack safely.
            std::mem::forget(co);
            return;
        };
        match outcome {
            CoroutineResult::Yield(_) => {
                let e = &mut self.sched.entries[idx];
                e.co = Some(co);
                e.state = State::Suspended;
            }
            CoroutineResult::Return(finish) => {
                drop(co);
                self.sched.entries[idx].state = State::Finished;
                self.sched.finished += 1;
                self.finish_coro(idx, id, finish, exhausted);
            }
        }
    }

    fn finish_coro(&mut self, idx: usize, id: u64, finish: Finish, exhausted: bool) {
        let handle = self.sched.entries[idx].handle.upgrade();
        if let Some(g) = self.sched.entries[idx].group {
            self.leave_group(g, id);
        }
        let finish = match finish {
            Finish::Panicked(info) => Finish::Panicked(info),
            other if exhausted => {
                if let Finish::Done(v) = other {
                    self.release(v);
                }
                let mut info = self.bare_panic_info("the instruction budget is exhausted");
                info.budget = true;
                Finish::Panicked(info)
            }
            other => other,
        };
        match finish {
            Finish::Done(v) => match &handle {
                Some(h) => {
                    let _ = h.set_field(H_STATUS, Value::Int(DONE));
                    if let Ok(old) = h.set_field(H_RESULT, v) {
                        self.release(old);
                    }
                }
                None => self.release(v),
            },
            Finish::Cancelled => {
                if let Some(h) = &handle {
                    let _ = h.set_field(H_STATUS, Value::Int(CANCELLED));
                }
            }
            Finish::Panicked(info) => {
                if let Some(h) = &handle {
                    let _ = h.set_field(H_STATUS, Value::Int(PANICKED));
                    let old = h.set_field(H_RESULT, Value::Str(Rc::from(info.message.as_str())));
                    if let Ok(old) = old {
                        self.release(old);
                    }
                }
                self.sched.failures.push((id, info));
            }
        }
    }

    fn leave_group(&mut self, group: u64, id: u64) {
        if let Some(members) = self.sched.groups.get_mut(&group) {
            if let Some(pos) = members.iter().rposition(|m| *m == id) {
                members.remove(pos);
            }
            if members.is_empty() {
                self.sched.groups.remove(&group);
            }
        }
    }

    fn bare_panic_info(&self, message: &str) -> Box<PanicInfo> {
        Box::new(PanicInfo {
            message: message.to_string(),
            span: Span::new(colibri_syntax::FileId(0), 0, 0),
            stack: Vec::new(),
            notes: Vec::new(),
            budget: false,
        })
    }

    // ---- pausing ----

    /// Common checks of `wait` and `join`: a coroutine is running, and no host function or
    /// `Drop` hook sits on its stack.
    fn pause_context(&mut self, fiber: &mut Fiber, span: Span, what: &str) -> Result<u64, Ctl> {
        let Some(ctx) = &fiber.coro else {
            let message = format!(
                "`{what}` can only pause a coroutine, but this code is not running in one (start one with `go`)"
            );
            return Err(self.panic(fiber, span, message));
        };
        let id = ctx.id;
        if fiber.host_depth > 0 {
            let message = format!(
                "`{what}` cannot pause inside a host function's callback: the host's stack frame cannot be suspended"
            );
            return Err(self.panic(fiber, span, message));
        }
        if self.hook_depth > 0 {
            let message = format!("`{what}` cannot pause inside a `Drop` hook");
            return Err(self.panic(fiber, span, message));
        }
        Ok(id)
    }

    /// Suspends the running coroutine until `wake`; `Err(Cancel)` if it is cancelled.
    fn suspend(&mut self, fiber: &mut Fiber, id: u64, wake: Wake) -> Eval<()> {
        let Some(idx) = self.sched.find(id) else {
            return Err(Ctl::Cancel);
        };
        let e = &mut self.sched.entries[idx];
        if fiber.cancelling || e.cancel_requested {
            fiber.cancelling = true;
            return Err(Ctl::Cancel);
        }
        if let Wake::Signal(s) = &wake {
            self.sched.signals.entry(*s).or_default().push(id);
        }
        let e = &mut self.sched.entries[idx];
        e.wake = wake;
        let Some(ctx) = &fiber.coro else {
            return Err(Ctl::Cancel);
        };
        // SAFETY: the yielder lives as long as the coroutine's body, which is running us.
        let yielder: &Y = unsafe { &*ctx.yielder };
        let ptr: *mut Interp = self;
        let input = yielder.suspend(ptr);
        std::hint::black_box(ptr);
        if input.cancel {
            fiber.cancelling = true;
            return Err(Ctl::Cancel);
        }
        Ok(())
    }

    /// `wait`, `wait_until`, `wait_real` and `task.tick`.
    pub(crate) fn coro_intrinsic(
        &mut self,
        fiber: &mut Fiber,
        intrinsic: &Intrinsic,
        mut args: Vec<Value>,
        span: Span,
    ) -> Eval {
        let name = match intrinsic {
            Intrinsic::Wait => "wait",
            Intrinsic::WaitUntil => "wait_until",
            Intrinsic::WaitReal => "wait_real",
            Intrinsic::WaitFor => "wait_for",
            _ => "task.tick",
        };
        if matches!(intrinsic, Intrinsic::NewCoGroup | Intrinsic::NewSignal) {
            let id = self.sched.next_id;
            self.sched.next_id += 1;
            let ty = if matches!(intrinsic, Intrinsic::NewCoGroup) {
                COGROUP_TYPE
            } else {
                SIGNAL_TYPE
            };
            return Ok(Value::Obj(ClassObj::new(ty, vec![Value::Int(id as i64)])));
        }
        let arg = args.pop().unwrap_or(Value::Unit);
        if matches!(intrinsic, Intrinsic::TaskTick) {
            let Value::Float(dt) = arg else {
                return Err(self.internal(fiber, span, "ill-typed tick"));
            };
            if !self.sched.running.is_empty() || self.sched.in_tick {
                let message = if fiber.coro.is_some() {
                    "`task.tick` cannot be called from a coroutine"
                } else {
                    "`task.tick` cannot be called from inside a tick"
                };
                return Err(self.panic(fiber, span, message));
            }
            return match self.tick_core(dt) {
                Ok(()) => Ok(Value::Unit),
                Err(TickError::Failure(info)) => Err(Ctl::Panic(info)),
                Err(TickError::Reentrant) => Err(self.panic(fiber, span, "`task.tick` is busy")),
            };
        }
        if let (Intrinsic::Wait, Value::Float(s), None) = (intrinsic, &arg, &fiber.coro) {
            if s.is_nan() {
                return Err(self.panic(fiber, span, "`wait` needs a number of seconds, not NaN"));
            }
            let s = s.max(0.0);
            return self.wait_on_main(fiber, s, span).map(|()| Value::Unit);
        }
        let id = match self.pause_context(fiber, span, name) {
            Ok(id) => id,
            Err(ctl) => {
                self.release(arg);
                return Err(ctl);
            }
        };
        match (intrinsic, arg) {
            (Intrinsic::Wait, Value::Float(s)) | (Intrinsic::WaitReal, Value::Float(s)) => {
                if s.is_nan() {
                    return Err(self.panic(
                        fiber,
                        span,
                        format!("`{name}` needs a number of seconds, not NaN"),
                    ));
                }
                let s = s.max(0.0);
                let wake = if matches!(intrinsic, Intrinsic::Wait) {
                    Wake::Time(self.sched.time + s)
                } else {
                    Wake::Real(self.sched.clock.now() + s)
                };
                self.suspend(fiber, id, wake)?;
                Ok(Value::Unit)
            }
            (Intrinsic::WaitFor, s) => {
                let result = match signal_id(&s) {
                    // `s` stays alive while this coroutine waits on it.
                    Some(sid) => self.suspend(fiber, id, Wake::Signal(sid)),
                    None => Err(self.internal(fiber, span, "ill-typed wait_for")),
                };
                self.release(s);
                result.map(|()| Value::Unit)
            }
            (Intrinsic::WaitUntil, f) => {
                let result = self.wait_until(fiber, id, &f, span);
                self.release(f);
                result.map(|()| Value::Unit)
            }
            (_, other) => {
                self.release(other);
                Err(self.internal(fiber, span, "ill-typed wait"))
            }
        }
    }

    fn wait_until(&mut self, fiber: &mut Fiber, id: u64, f: &Value, span: Span) -> Eval<()> {
        loop {
            if fiber.cancelling {
                return Err(Ctl::Cancel);
            }
            if let Some(frame) = fiber.frames.last_mut() {
                frame.span = span;
            }
            match self.call_function_value(fiber, f, Vec::new(), span)? {
                Value::Bool(true) => return Ok(()),
                _ => self.suspend(fiber, id, Wake::Ready)?,
            }
        }
    }

    // ---- driving the scheduler from the main fiber ----

    /// May the main fiber run the event loop now? `depth` is how many host functions may be
    /// active (0 for a Colibri-level wait, 1 for a host function waiting on its own thread).
    fn can_drive(&self, fiber: &Fiber, depth: u32) -> bool {
        fiber.coro.is_none()
            && fiber.host_depth <= depth
            && self.hook_depth == 0
            && !self.sched.in_tick
            && self.sched.running.is_empty()
            && !self.sched.shutting_down
    }

    /// The checks of a wait on the main fiber, as panics with a message that says why.
    fn check_drive(&mut self, fiber: &mut Fiber, span: Span, what: &str) -> Eval<()> {
        let why = if fiber.host_depth > 0 {
            "inside a host function's callback"
        } else if self.hook_depth > 0 {
            "inside a `Drop` hook"
        } else if self.sched.in_tick || !self.sched.running.is_empty() {
            "inside a tick"
        } else if self.sched.shutting_down {
            "while the program is shutting down"
        } else {
            return Ok(());
        };
        let message = format!(
            "`{what}` on the main fiber runs the event loop, which cannot be done {why} (ARCHITECTURE section 22)"
        );
        Err(self.panic(fiber, span, message))
    }

    /// Runs the event loop on the main fiber, as the loop after `main` does (ticks with the
    /// time elapsed on the clock, sleeping until the next wake or a completion), until `done`
    /// holds or the clock reaches `deadline`. With `report`, a coroutine's failure ends the
    /// wait with that panic unless `done` holds. Always ticks once first.
    fn drive(
        &mut self,
        deadline: Option<f64>,
        report: bool,
        done: &mut dyn FnMut(&mut Interp) -> bool,
    ) -> Drive {
        let mut last = self.sched.clock.now();
        loop {
            let now = self.sched.clock.now();
            let dt = (now - last).max(0.0);
            last = now;
            if self.tick_run(dt).is_err() {
                return Drive::Stuck;
            }
            if done(self) {
                return Drive::Done;
            }
            if report && let Err(TickError::Failure(info)) = self.take_failures() {
                return Drive::Failure(info);
            }
            let now = self.sched.clock.now();
            let mut delay = match (self.next_wake(0.001), deadline) {
                (Some(d), _) => d,
                (None, Some(_)) => f64::INFINITY,
                (None, None) => return Drive::Stuck,
            };
            if let Some(d) = deadline {
                if now + TIME_EPSILON >= d {
                    return Drive::Done;
                }
                delay = delay.min(d - now);
            }
            self.clock_sleep(delay);
        }
    }

    fn drive_main(
        &mut self,
        fiber: &mut Fiber,
        span: Span,
        what: &str,
        deadline: Option<f64>,
        done: &mut dyn FnMut(&mut Interp) -> bool,
    ) -> Eval<()> {
        self.check_drive(fiber, span, what)?;
        match self.drive(deadline, true, done) {
            Drive::Done => Ok(()),
            Drive::Failure(info) => Err(Ctl::Panic(info)),
            Drive::Stuck => {
                let message = format!(
                    "`{what}` on the main fiber would wait forever: no coroutine is left that a clock or a completion could wake"
                );
                Err(self.panic(fiber, span, message))
            }
        }
    }

    /// `wait(seconds)` on the main fiber: runs the event loop for that long.
    fn wait_on_main(&mut self, fiber: &mut Fiber, seconds: f64, span: Span) -> Eval<()> {
        let deadline = self.sched.clock.now() + seconds;
        self.drive_main(fiber, span, "wait", Some(deadline), &mut |_| false)
    }

    // ---- background operations ----

    /// Runs `work` on a background thread (an idle one of the pool, or a new one: an
    /// operation never waits behind another) and waits for its result without blocking other
    /// coroutines: the calling coroutine is suspended with `Wake::External`, and the event
    /// loop (or the host's `tick`) resumes it once the thread has finished. On the main
    /// fiber the wait runs the event loop instead, so coroutines keep running. Where neither
    /// is possible (inside a host callback or a `Drop` hook) it just blocks. Meant to be
    /// called from a host function (host depth 1); it must not hold a borrow of host objects
    /// across the call, because other coroutines run meanwhile. `Err(Cancelled)` means the
    /// coroutine was cancelled while waiting: the host function must unwind (its result would
    /// be discarded; an operation whose thread has not started it yet is dropped).
    pub fn await_external<R: Send + 'static>(
        &mut self,
        fiber: &mut Fiber,
        work: impl FnOnce() -> R + Send + 'static,
    ) -> Result<R, ExternalError> {
        self.await_external_with(fiber, move |_| work())
    }

    /// [`Interp::await_external`] for work that can stop early: `work` is given a
    /// [`CancelToken`] that is cancelled when the waiting coroutine is cancelled or the `Vm`
    /// ends. An operation that holds a child process registers a hook that kills it; the hook
    /// runs on the cancelling side before it goes on, so nothing is left running.
    pub fn await_external_with<R: Send + 'static>(
        &mut self,
        fiber: &mut Fiber,
        work: impl FnOnce(&CancelToken) -> R + Send + 'static,
    ) -> Result<R, ExternalError> {
        let op = self.sched.next_id;
        self.sched.next_id += 1;
        let (result_tx, result_rx) = mpsc::channel::<Result<R, String>>();
        let notify = Notify(self.sched.ext.tx.clone(), op);
        // Set when the waiter gives up, so an operation still in the queue is not run.
        let token = Arc::new(CancelToken::default());
        let skip = Arc::clone(&token);
        self.sched.ext.pending.insert(op);
        self.sched.ext.tokens.insert(op, Arc::clone(&token));
        let queued = self.sched.ext.pool.submit(Box::new(move || {
            let _notify = notify;
            if skip.is_cancelled() {
                return;
            }
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(&skip)))
                .map_err(|payload| panic_message(payload.as_ref()));
            let _ = result_tx.send(outcome);
        }));
        if let Err(message) = queued {
            self.sched.ext.forget(op);
            return Err(ExternalError::Failed(message));
        }
        let coro = fiber.coro.as_ref().map(|c| c.id);
        let suspendable = fiber.host_depth <= 1 && self.hook_depth == 0;
        let waited = if let (Some(id), true) = (coro, suspendable) {
            let mut outcome = Ok(());
            while !self.sched.ext.completed.contains(&op) {
                if self.suspend(fiber, id, Wake::External(op)).is_err() {
                    token.cancel();
                    outcome = Err(ExternalError::Cancelled);
                    break;
                }
            }
            outcome
        } else if self.can_drive(fiber, 1) {
            self.drive(None, false, &mut |me| me.sched.ext.completed.contains(&op));
            Ok(())
        } else {
            // Nothing can run meanwhile: block this thread until the result is there.
            let blocked = result_rx.recv();
            self.sched.ext.forget(op);
            return unwrap_outcome(blocked.map_err(|_| ()));
        };
        self.sched.ext.forget(op);
        self.sched.ext.completed.remove(&op);
        waited?;
        unwrap_outcome(result_rx.try_recv().map_err(|_| ()))
    }

    // ---- handle methods ----

    pub(crate) fn coroutine_builtin(
        &mut self,
        fiber: &mut Fiber,
        op: colibri_check::hir::Builtin,
        recv: &Value,
        span: Span,
    ) -> Eval {
        use colibri_check::hir::Builtin;
        let Some(h) = handle_of(recv) else {
            return Err(self.internal(fiber, span, "ill-typed coroutine method"));
        };
        let h = Rc::clone(h);
        let result = match op {
            Builtin::CoroutineDone => Ok(Value::Bool(handle_status(&h) != RUNNING)),
            Builtin::CoroutineCancel => self.cancel_coroutine(fiber, &h).map(|()| Value::Unit),
            _ => self.join_coroutine(fiber, &h, span),
        };
        // `h` is a clone of a handle the caller still holds, so this is never the last owner.
        drop(h);
        result
    }

    fn cancel_coroutine(&mut self, fiber: &mut Fiber, h: &ClassObj) -> Eval<()> {
        if handle_status(h) != RUNNING {
            return Ok(());
        }
        let id = handle_id(h);
        if fiber.coro.as_ref().is_some_and(|c| c.id == id) {
            // A coroutine cancelling itself stops at once.
            fiber.cancelling = true;
            return Err(Ctl::Cancel);
        }
        self.cancel_id(id);
        Ok(())
    }

    /// Cancels the coroutine `id` (not the running one): a suspended coroutine unwinds at
    /// once, one that is mid-run higher up the resume chain stops at its next pause.
    fn cancel_id(&mut self, id: u64) {
        let Some(idx) = self.sched.find(id) else {
            return;
        };
        match self.sched.entries[idx].state {
            State::Finished => {}
            State::Running => self.sched.entries[idx].cancel_requested = true,
            State::Suspended => self.resume_coro(id, true, false),
        }
    }

    /// The last reference to a `CoGroup` died: its coroutines are cancelled, newest first.
    pub(crate) fn group_died(&mut self, obj: &ClassObj) {
        let Some(Value::Int(g)) = obj.field(0) else {
            return;
        };
        if let Some(members) = self.sched.groups.remove(&(g as u64)) {
            for id in members.into_iter().rev() {
                self.cancel_id(id);
            }
        }
    }

    pub(crate) fn cogroup_builtin(
        &mut self,
        fiber: &mut Fiber,
        op: colibri_check::hir::Builtin,
        recv: &Value,
        span: Span,
    ) -> Eval {
        use colibri_check::hir::Builtin;
        let Some(g) = group_id(recv) else {
            return Err(self.internal(fiber, span, "ill-typed CoGroup method"));
        };
        let me = fiber.coro.as_ref().map(|c| c.id);
        match op {
            Builtin::CoGroupCount => {
                let n = self.sched.groups.get(&g).map_or(0, Vec::len);
                Ok(Value::Int(n as i64))
            }
            Builtin::CoGroupCancel => {
                let members = self.sched.groups.get(&g).cloned().unwrap_or_default();
                let mut cancel_self = false;
                for id in members.into_iter().rev() {
                    if Some(id) == me {
                        cancel_self = true;
                    } else {
                        self.cancel_id(id);
                    }
                }
                if cancel_self {
                    // A member cancelling its own group stops last, at once.
                    fiber.cancelling = true;
                    return Err(Ctl::Cancel);
                }
                Ok(Value::Unit)
            }
            _ => {
                let others = |s: &Scheduler| {
                    s.groups
                        .get(&g)
                        .is_some_and(|m| m.iter().any(|c| Some(*c) != me))
                };
                if !others(&self.sched) {
                    return Ok(Value::Unit);
                }
                if me.is_none() {
                    self.drive_main(fiber, span, "join_all", None, &mut |it| !others(&it.sched))?;
                    return Ok(Value::Unit);
                }
                let id = self.pause_context(fiber, span, "join_all")?;
                while others(&self.sched) {
                    self.suspend(fiber, id, Wake::Group(g))?;
                }
                Ok(Value::Unit)
            }
        }
    }

    pub(crate) fn signal_builtin(&mut self, fiber: &mut Fiber, recv: &Value, span: Span) -> Eval {
        let Some(sid) = signal_id(recv) else {
            return Err(self.internal(fiber, span, "ill-typed Signal method"));
        };
        if let Some(waiters) = self.sched.signals.remove(&sid) {
            for id in waiters {
                if let Some(idx) = self.sched.find(id) {
                    let e = &mut self.sched.entries[idx];
                    if e.state == State::Suspended && matches!(e.wake, Wake::Signal(s) if s == sid)
                    {
                        e.wake = Wake::Signaled;
                        self.sched.signaled.push(id);
                    }
                }
            }
        }
        Ok(Value::Unit)
    }

    fn join_coroutine(&mut self, fiber: &mut Fiber, h: &Rc<ClassObj>, span: Span) -> Eval {
        if handle_status(h) == RUNNING && fiber.coro.is_none() {
            let target = Rc::clone(h);
            self.drive_main(fiber, span, "join", None, &mut |_| {
                handle_status(&target) != RUNNING
            })?;
        }
        if handle_status(h) == RUNNING {
            let id = self.pause_context(fiber, span, "join")?;
            if handle_id(h) == id {
                return Err(self.panic(fiber, span, "a coroutine cannot join itself"));
            }
            while handle_status(h) == RUNNING {
                self.suspend(fiber, id, Wake::Join(Rc::downgrade(h)))?;
            }
        }
        match handle_status(h) {
            DONE => Ok(h.field(H_RESULT).unwrap_or(Value::Unit)),
            PANICKED => {
                let id = handle_id(h);
                self.sched.failures.retain(|(f, _)| *f != id);
                let msg = match h.field(H_RESULT) {
                    Some(Value::Str(s)) => s.to_string(),
                    _ => String::new(),
                };
                let message = format!("the joined coroutine `{}` panicked: {msg}", handle_name(h));
                Err(self.panic(fiber, span, message))
            }
            _ => {
                let message = format!("the joined coroutine `{}` was cancelled", handle_name(h));
                Err(self.panic(fiber, span, message))
            }
        }
    }

    // ---- shutdown ----

    /// Cancels every live coroutine, newest first, so their `defer`s and drops run before the
    /// module-level values die. Nothing can start meanwhile.
    pub fn cancel_all_coroutines(&mut self) {
        if self.sched.entries.is_empty() || !self.sched.running.is_empty() {
            return;
        }
        let outermost = self.begin_entry();
        self.sched.shutting_down = true;
        loop {
            let next = self
                .sched
                .entries
                .iter()
                .rev()
                .find(|e| e.state == State::Suspended)
                .map(|e| e.id);
            let Some(id) = next else { break };
            self.resume_coro(id, true, true);
        }
        self.sched.shutting_down = false;
        for e in &mut self.sched.entries {
            if let Some(co) = e.co.take() {
                // Unreachable in practice; a forced unwind must never happen.
                std::mem::forget(co);
            }
        }
        self.sched.entries.clear();
        self.sched.finished = 0;
        self.sched.signals.clear();
        self.sched.signaled.clear();
        self.sched.groups.clear();
        self.sched.ext.completed.clear();
        self.end_entry(outermost, false);
    }
}
