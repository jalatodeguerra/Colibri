//! Background operations: the pool of threads that run them, the cancellation token that ends
//! one, and the completion primitive a host uses to finish one from any thread.
//!
//! The waiting itself (suspending a coroutine, driving the event loop, blocking) is the
//! engine's; this module has only the data and the thread-safe pieces.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::Duration;

/// Why waiting for a background operation did not give a result.
#[derive(Debug)]
pub enum ExternalError {
    /// The waiting coroutine was cancelled: the operation is abandoned.
    Cancelled,
    /// The operation could not run or its thread panicked.
    Failed(String),
    /// The operation never started: the system would not give the pool another thread (or
    /// the pool is closed). Nothing of it ran.
    CannotStart(String),
}

/// How a wait for a background operation ended, when it did not fail: the engine's half of the
/// wait is over and the caller collects the result from the operation's receiver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Waited {
    /// Nothing could run meanwhile: the caller blocks on the receiver, then calls
    /// [`Externals::forget`].
    Blocked,
    /// The operation finished and the engine has already collected it
    /// ([`Externals::finish`]): the result is in the receiver.
    Finished,
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
    /// A token that has been cancelled already: every hook given to
    /// [`CancelToken::on_cancel`] runs at once.
    pub fn cancelled() -> Self {
        Self {
            flag: AtomicBool::new(true),
            hooks: Mutex::default(),
        }
    }

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

    /// Starts a resource that a hook must release (a child process) so that cancelling cannot
    /// slip between creating it and registering the hook. `f` makes the resource and returns
    /// it with its release hook; it runs only if the token is not cancelled yet, and the
    /// hook is registered before `cancel` can run: `cancel` takes the same lock, so it either
    /// comes first (`Ok(None)`, `f` never runs) or waits for `f` to return and then finds the
    /// hook. `f` runs under that lock: it must be quick (one `fork`/`exec`), and must not call
    /// [`CancelToken::on_cancel`]. An `Err` from `f` registers nothing.
    pub fn start<T, E>(
        &self,
        f: impl FnOnce() -> Result<(T, Box<dyn FnOnce() + Send>), E>,
    ) -> Result<Option<T>, E> {
        let mut hooks = self
            .hooks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.is_cancelled() {
            return Ok(None);
        }
        let (resource, hook) = f()?;
        hooks.push(hook);
        Ok(Some(resource))
    }

    #[doc(hidden)]
    pub fn cancel(&self) {
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
pub type Job = Box<dyn FnOnce() + Send>;

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
pub(crate) struct Pool {
    shared: Arc<PoolShared>,
    /// A pool that never starts a thread: every `submit` is refused (see [`NO_THREADS`]).
    threadless: bool,
}

/// What a pool without threads says when it is asked to run a job: on the web there are no
/// threads (`wasm32-unknown-unknown` cannot spawn one), so nothing ever runs in the background.
pub const NO_THREADS: &str = "no threads on the web";

impl Pool {
    pub(crate) fn new() -> Self {
        Pool::with_threads(!cfg!(target_family = "wasm"))
    }

    /// A pool that takes no job: `submit` fails with [`NO_THREADS`]. What [`Pool::new`] gives on
    /// the web; native code builds one to test that path.
    pub(crate) fn with_threads(threads: bool) -> Self {
        Pool {
            threadless: !threads,
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
    pub(crate) fn submit(&self, job: Job) -> Result<(), String> {
        if self.threadless {
            return Err(NO_THREADS.to_string());
        }
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
#[doc(hidden)]
pub struct Externals {
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
    pub fn new() -> Self {
        Self::with_pool(Pool::new())
    }

    /// Background operations whose pool starts no thread, as on the web: `submit` fails with
    /// [`NO_THREADS`].
    #[doc(hidden)]
    pub fn without_threads() -> Self {
        Self::with_pool(Pool::with_threads(false))
    }

    fn with_pool(pool: Pool) -> Self {
        let (tx, rx) = mpsc::channel();
        Externals {
            pool,
            tx,
            rx,
            pending: HashSet::new(),
            tokens: HashMap::new(),
            completed: HashSet::new(),
        }
    }

    /// The operation is over for the waiter, whichever way.
    pub fn forget(&mut self, id: u64) {
        self.pending.remove(&id);
        self.tokens.remove(&id);
    }

    fn note(&mut self, id: u64) {
        if self.pending.remove(&id) {
            self.completed.insert(id);
        }
    }

    /// Collects every completion that has arrived, without waiting.
    pub fn drain(&mut self) {
        while let Ok(id) = self.rx.try_recv() {
            self.note(id);
        }
    }

    /// Waits up to `timeout` for one completion; whether one arrived.
    pub fn wait(&mut self, timeout: Duration) -> bool {
        match self.rx.recv_timeout(timeout) {
            Ok(id) => {
                self.note(id);
                self.drain();
                true
            }
            Err(_) => false,
        }
    }

    /// Whether any operation is started and not yet finished or abandoned.
    pub(crate) fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Whether the operation `op` has finished and its waiter has not collected it.
    pub fn is_completed(&self, op: u64) -> bool {
        self.completed.contains(&op)
    }

    /// The waiter has collected the operation `op`, or gave it up after it finished.
    pub fn finish(&mut self, op: u64) {
        self.forget(op);
        self.completed.remove(&op);
    }

    /// Forgets every finished operation nobody collected (the scheduler is being cleared).
    pub(crate) fn clear_completed(&mut self) {
        self.completed.clear();
    }

    /// Whether nothing of any operation is left: none pending, none with a token, none
    /// finished and uncollected.
    pub fn is_idle(&self) -> bool {
        self.pending.is_empty() && self.tokens.is_empty() && self.completed.is_empty()
    }

    /// Runs `job` on an idle worker, or on a new thread if none is idle.
    pub fn submit(&self, job: Job) -> Result<(), String> {
        self.pool.submit(job)
    }

    /// Registers the operation `op` and makes the end that completes it, with the receiver
    /// its waiter reads the result from.
    pub fn begin<R>(&mut self, op: u64) -> (Completer<R>, mpsc::Receiver<Result<R, String>>) {
        let (result_tx, result_rx) = mpsc::channel::<Result<R, String>>();
        let token = Arc::new(CancelToken::default());
        self.pending.insert(op);
        self.tokens.insert(op, Arc::clone(&token));
        let completer = Completer::new(result_tx, self.tx.clone(), op, token);
        (completer, result_rx)
    }
}

impl Default for Externals {
    fn default() -> Self {
        Self::new()
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
pub(crate) struct Notify(pub(crate) mpsc::Sender<u64>, pub(crate) u64);

impl Drop for Notify {
    fn drop(&mut self) {
        let _ = self.0.send(self.1);
    }
}

/// The end of a background operation that its owner holds: whoever has it (any thread) gives
/// the waiting call its result with [`Completer::complete`], once. Made by
/// `Machine::await_completion`, which waits for it. The pool's own jobs
/// (`Machine::await_external_with`) are one kind of owner; a thread that watches for events
/// (a poller of sockets) is another.
///
/// What it promises:
///
/// - The result is sent before the waiter is told, so a waiter that wakes always finds it.
/// - If the waiting coroutine was cancelled, [`CancelToken::cancel`] has run the token's hooks
///   on the cancelling thread before the host function unwound, and a later `complete` is a
///   no-op for the waiter: the value is dropped, never delivered (usually on the completing
///   thread; one that lands in the channel just as the cancel happens is dropped by the waiter's
///   thread when the channel goes). A value whose loss matters must therefore not be the result.
/// - When the `Vm` ends, the token of every pending operation is cancelled (running its hooks)
///   and completers that are still held become inert: completing them is ignored.
/// - Dropping a completer without completing it wakes the waiter with an error
///   ([`ExternalError::Failed`], "the operation ended without a result"), never a hang.
#[doc(hidden)]
pub struct Completer<R> {
    result: Option<mpsc::Sender<Result<R, String>>>,
    /// Posts the operation's id on the scheduler when this is dropped, after the result is sent.
    _notify: Notify,
    token: Arc<CancelToken>,
}

impl<R> Completer<R> {
    /// The end that completes operation `op`: `result` carries the outcome to the waiter, and
    /// `ids` is told the operation ended when this is dropped.
    #[doc(hidden)]
    pub fn new(
        result: mpsc::Sender<Result<R, String>>,
        ids: mpsc::Sender<u64>,
        op: u64,
        token: Arc<CancelToken>,
    ) -> Self {
        Completer {
            result: Some(result),
            _notify: Notify(ids, op),
            token,
        }
    }

    /// The id of the operation this completes.
    #[doc(hidden)]
    pub fn op(&self) -> u64 {
        self._notify.1
    }

    /// The token of the waiting call: cancelled when its coroutine is cancelled or the `Vm`
    /// ends. Register a hook with [`CancelToken::on_cancel`] to release what the operation
    /// holds (a registration with a poller, say).
    pub fn token(&self) -> &Arc<CancelToken> {
        &self.token
    }

    /// Gives the waiter `value` and wakes it. Callable from any thread; consuming `self` makes
    /// it once. Ignored if the waiter is gone (cancelled, or the `Vm` has ended).
    pub fn complete(mut self, value: R) {
        self.send(Ok(value));
    }

    /// As [`Completer::complete`] for work that panicked: the waiter gets the panic's message
    /// as a failure.
    #[doc(hidden)]
    pub fn fail(mut self, message: String) {
        self.send(Err(message));
    }

    fn send(&mut self, outcome: Result<R, String>) {
        // A closed channel means the waiter is gone; the value is dropped here, as it is when
        // the waiter was cancelled.
        if let Some(tx) = self.result.take()
            && !self.token.is_cancelled()
        {
            let _ = tx.send(outcome);
        }
        // `_notify` drops after this, so the id is posted after the result was sent.
    }
}

pub(crate) const NO_RESULT: &str = "the operation ended without a result";

/// The text of a panic payload (`panic!("..")` gives a `&str` or a `String`).
#[doc(hidden)]
pub fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "(no message)".to_string())
}

/// A finished operation's result, or why there is none.
#[doc(hidden)]
pub fn unwrap_outcome<R>(outcome: Result<Result<R, String>, ()>) -> Result<R, ExternalError> {
    match outcome {
        Ok(Ok(r)) => Ok(r),
        Ok(Err(message)) => Err(ExternalError::Failed(format!(
            "the background operation panicked: {message}"
        ))),
        Err(_) => Err(ExternalError::Failed(NO_RESULT.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::{CancelToken, Completer, Notify};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, mpsc};

    type Hook = Box<dyn FnOnce() + Send>;

    /// Cancelling races starting a resource: either the resource is never made, or its hook
    /// has run by the time `cancel` returns. Never made-and-forgotten.
    #[test]
    fn cancel_racing_start_never_leaves_a_resource_behind() {
        let (mut never, mut released) = (0, 0);
        for _ in 0..3000 {
            let token = Arc::new(CancelToken::default());
            let made = Arc::new(AtomicUsize::new(0));
            let freed = Arc::new(AtomicUsize::new(0));
            let starter = {
                let (token, made, freed) = (token.clone(), made.clone(), freed.clone());
                std::thread::spawn(move || {
                    token
                        .start(|| {
                            made.fetch_add(1, Ordering::SeqCst);
                            let hook: Box<dyn FnOnce() + Send> = Box::new(move || {
                                freed.fetch_add(1, Ordering::SeqCst);
                            });
                            Ok::<_, ()>(((), hook))
                        })
                        .unwrap()
                        .is_some()
                })
            };
            let canceller = {
                let token = token.clone();
                std::thread::spawn(move || token.cancel())
            };
            let started = starter.join().unwrap();
            canceller.join().unwrap();
            assert_eq!(made.load(Ordering::SeqCst), usize::from(started));
            assert_eq!(
                freed.load(Ordering::SeqCst),
                made.load(Ordering::SeqCst),
                "a resource was made and its hook did not run"
            );
            if started {
                released += 1;
            } else {
                never += 1;
            }
        }
        // Both orders happen (or at least the invariant held for every run).
        assert_eq!(never + released, 3000);
    }

    #[test]
    fn start_after_cancel_makes_nothing_and_an_error_registers_no_hook() {
        let token = CancelToken::cancelled();
        let ran = token.start(|| -> Result<((), Hook), ()> { panic!("ran") });
        assert!(matches!(ran, Ok(None)));
        let token = CancelToken::default();
        let failed = token.start(|| -> Result<((), Hook), &str> { Err("no") });
        assert_eq!(failed.err(), Some("no"));
        assert!(token.hooks.lock().unwrap().is_empty());
    }

    /// A value counted when it is dropped, to tell "delivered" from "dropped by the completer".
    struct Probe(Arc<AtomicUsize>);

    impl Drop for Probe {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Completing races cancelling, from two threads. Whichever order the threads take: a hook
    /// ran exactly once, the value is either with the waiter or was dropped (never both, never
    /// neither), the id was posted exactly once, a `complete` that began after `cancel` had
    /// returned delivered nothing, and one that had returned before `cancel` began delivered.
    #[test]
    fn completing_racing_a_cancel_delivers_exactly_when_the_cancel_lost() {
        let (mut delivered_runs, mut dropped_runs) = (0, 0);
        for _ in 0..3000 {
            let (ids_tx, ids_rx) = mpsc::channel::<u64>();
            let (result_tx, result_rx) = mpsc::channel::<Result<Probe, String>>();
            let token = Arc::new(CancelToken::default());
            let hooks = Arc::new(AtomicUsize::new(0));
            let drops = Arc::new(AtomicUsize::new(0));
            {
                let hooks = Arc::clone(&hooks);
                token.on_cancel(move || {
                    hooks.fetch_add(1, Ordering::SeqCst);
                });
            }
            let completer = Completer {
                result: Some(result_tx),
                _notify: Notify(ids_tx, 7),
                token: Arc::clone(&token),
            };
            let cancel_returned = Arc::new(AtomicBool::new(false));
            let complete_returned = Arc::new(AtomicBool::new(false));
            let completing = {
                let (cancel_returned, complete_returned) =
                    (Arc::clone(&cancel_returned), Arc::clone(&complete_returned));
                let probe = Probe(Arc::clone(&drops));
                std::thread::spawn(move || {
                    let after_cancel = cancel_returned.load(Ordering::SeqCst);
                    completer.complete(probe);
                    complete_returned.store(true, Ordering::SeqCst);
                    after_cancel
                })
            };
            let cancelling = {
                let (token, complete_returned) =
                    (Arc::clone(&token), Arc::clone(&complete_returned));
                let cancel_returned = Arc::clone(&cancel_returned);
                std::thread::spawn(move || {
                    let after_complete = complete_returned.load(Ordering::SeqCst);
                    token.cancel();
                    cancel_returned.store(true, Ordering::SeqCst);
                    after_complete
                })
            };
            let complete_after_cancel = completing.join().unwrap();
            let cancel_after_complete = cancelling.join().unwrap();
            assert_eq!(hooks.load(Ordering::SeqCst), 1, "the hook runs once");
            assert_eq!(ids_rx.try_iter().collect::<Vec<_>>(), vec![7], "one id");
            let delivered = match result_rx.try_recv() {
                Ok(Ok(value)) => {
                    // With the waiter: not dropped yet.
                    assert_eq!(drops.load(Ordering::SeqCst), 0);
                    drop(value);
                    true
                }
                _ => false,
            };
            assert_eq!(
                drops.load(Ordering::SeqCst),
                1,
                "the value was dropped once"
            );
            assert!(
                !(complete_after_cancel && delivered),
                "a complete after the cancel returned delivered a value"
            );
            assert!(
                !cancel_after_complete || delivered,
                "a complete that returned before the cancel began lost its value"
            );
            if delivered {
                delivered_runs += 1;
            } else {
                dropped_runs += 1;
            }
        }
        // Both outcomes are legal and the invariants held for every run.
        assert_eq!(delivered_runs + dropped_runs, 3000);
    }

    #[test]
    fn a_dropped_completer_posts_its_id_and_leaves_no_result() {
        let (ids_tx, ids_rx) = mpsc::channel::<u64>();
        let (result_tx, result_rx) = mpsc::channel::<Result<u8, String>>();
        let completer = Completer {
            result: Some(result_tx),
            _notify: Notify(ids_tx, 3),
            token: Arc::new(CancelToken::default()),
        };
        drop(completer);
        assert_eq!(ids_rx.try_recv().unwrap(), 3);
        // The sender is gone too: a blocking waiter would see the end, not hang.
        assert!(matches!(
            result_rx.try_recv(),
            Err(mpsc::TryRecvError::Disconnected)
        ));
    }

    /// The pool of the web build: nothing is started, the job is dropped unrun, and the
    /// scheduler reports `CannotStart` with the web's message and keeps no trace of the operation.
    #[test]
    fn a_pool_without_threads_refuses_work_with_cannot_start() {
        use super::{ExternalError, Externals, NO_THREADS};
        let mut sched = crate::sched::Scheduler::new();
        *sched.ext() = Externals::without_threads();
        let ran = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&ran);
        let out = sched.submit_work(move |_| flag.store(true, Ordering::SeqCst));
        match out {
            Err(ExternalError::CannotStart(message)) => {
                assert_eq!(message, "no threads on the web");
                assert_eq!(message, NO_THREADS);
            }
            other => panic!("expected CannotStart, got {:?}", other.err()),
        }
        assert!(!ran.load(Ordering::SeqCst), "the job must not run");
        assert!(
            sched.ext().is_idle(),
            "the refused operation leaves nothing behind"
        );
        // Dropping the pool must not hang or start anything either.
        drop(sched);
    }

    /// The pool of a native build still runs work, so the refusal above is the `threadless`
    /// switch and not a pool that always fails.
    #[test]
    fn a_pool_with_threads_still_runs_work() {
        let mut sched = crate::sched::Scheduler::new();
        let pending = sched.submit_work(|_| 7).expect("the pool starts a thread");
        let done = sched
            .collect(pending, super::Waited::Blocked)
            .expect("result");
        assert_eq!(done, 7);
    }

    /// A blocked wait that may not block (the web's) takes a result that is there and fails
    /// when there is none, instead of waiting for a completer that nothing will run.
    #[test]
    fn a_blocked_wait_that_may_not_block_fails_instead_of_hanging() {
        let mut sched = crate::sched::Scheduler::new();
        // The completer stays alive, so a blocking receive would never return.
        let (completer, pending) = sched.begin_completion::<i32>();
        match sched.collect_with(pending, super::Waited::Blocked, false) {
            Err(super::ExternalError::Failed(message)) => {
                assert!(message.contains("without blocking"), "{message}");
            }
            other => panic!("expected a failure, got {:?}", other.err()),
        }
        // A completer that ended with no result is the usual error, not "cannot block".
        drop(completer);
        let (completer, pending) = sched.begin_completion::<i32>();
        drop(completer);
        match sched.collect_with(pending, super::Waited::Blocked, false) {
            Err(super::ExternalError::Failed(message)) => {
                assert!(!message.contains("without blocking"), "{message}");
            }
            other => panic!("expected a failure, got {:?}", other.err()),
        }
        // With the result already there, it is taken.
        let (completer, pending) = sched.begin_completion::<i32>();
        completer.complete(5);
        let done = sched.collect_with(pending, super::Waited::Blocked, false);
        assert_eq!(done.expect("the result that was there"), 5);
    }
}
