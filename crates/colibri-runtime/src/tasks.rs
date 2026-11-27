//! Isolates and the worker pool for thread tasks (DESIGN §14.1, ARCHITECTURE §36.3).
//!
//! A thread task runs in an isolate: a second [`Vm`] on a pool thread, built from a
//! [`ProgramImage`] of the main `Vm`'s program, with its own object graph. Nothing but a
//! [`Portable`] (and, later, the `Arc` handle of a channel) crosses between the two: a [`Vm`]
//! holds `Rc`s, is `!Send`, and a worker builds its own on its own thread.
//!
//! The pool is [`TaskPool`]: `N` threads that start on the first task, each building its worker
//! `Vm` on its first job and keeping it for the next. This step has no `task.run` yet; a test or
//! the next step submits "call this function with these portable arguments" through
//! [`Vm::submit_portable`].
//!
//! `Vm` stays `!Send`, and nothing here needs it to be otherwise:
//!
//! ```compile_fail
//! fn needs_send<T: Send>() {}
//! needs_send::<colibri_runtime::Vm>();
//! ```

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::io::Write;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, mpsc};
use std::thread::JoinHandle;

use colibri_check::package::{BuildCfg, MemoryFs};
use colibri_core::external::Completer;
pub use colibri_core::portable::Portable;
use colibri_core::taskrun::{SpawnRequest, StolenTask, TaskOutcome, TaskSpawner};

use crate::cap::CapSet;
use crate::reload::OwnedEntry;
use crate::vm::Vm;

/// The stack of a pool thread. A worker runs the checker on it when it builds its `Vm`, and the
/// deep operations on values, like the thread of `colibri run` (the portable walks have a stack of
/// their own, see `colibri_vm::on_walk_stack`). The memory is reserved, not committed.
pub const WORKER_STACK: usize = 64 * 1024 * 1024;

/// What a worker's engine may use of its stack: the rest is a margin for the thread's own frames.
pub const WORKER_NATIVE_BUDGET: usize = WORKER_STACK - 8 * 1024 * 1024;

/// What a host passes to [`crate::VmBuilder::worker_setup`]: it registers the functions and
/// types that thread tasks may use, on the fresh `Vm` of each worker.
pub type WorkerSetup = Arc<dyn Fn(&mut Vm) + Send + Sync>;

/// A writer that threads share: `print` of the main `Vm` and of every worker writes through it.
pub(crate) type SharedOut = Arc<Mutex<Box<dyn Write + Send>>>;

/// Where a worker's `print` goes.
#[derive(Clone)]
pub(crate) enum WorkerOut {
    /// The process's standard output (the default writer of a `Vm` is that too).
    Process,
    /// The writer the host gave to [`crate::VmBuilder::stdout_shared`].
    Shared(SharedOut),
}

/// The writer of a `Vm` whose output the workers share: it takes the lock for each whole line, so
/// two threads never tear a line (`writeln!` makes a `print` in two writes).
pub(crate) struct LineWriter {
    out: SharedOut,
    pending: Vec<u8>,
}

impl LineWriter {
    pub(crate) fn new(out: SharedOut) -> Self {
        LineWriter {
            out,
            pending: Vec::new(),
        }
    }
}

impl Write for LineWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.pending.extend_from_slice(buf);
        if let Some(end) = self.pending.iter().rposition(|b| *b == b'\n') {
            let mut out = lock(&self.out);
            out.write_all(&self.pending[..=end])?;
            self.pending.drain(..=end);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let mut out = lock(&self.out);
        if !self.pending.is_empty() {
            out.write_all(&self.pending)?;
            self.pending.clear();
        }
        out.flush()
    }
}

impl Drop for LineWriter {
    fn drop(&mut self) {
        let _ = self.flush();
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

// What crosses between threads is plain data. These fail to compile when something that is not
// `Send` (an `Rc`, a `Value`) finds its way into one of the types that do.
const _: () = {
    const fn send<T: Send>() {}
    const fn send_sync<T: Send + Sync>() {}
    send_sync::<ProgramImage>();
    send_sync::<PoolStats>();
    send::<Job>();
    send::<Submission>();
    send::<Portable>();
    send::<TaskDone>();
    send::<TaskFailure>();
    send::<PendingTask>();
};

// ---- the program image ----

/// What a worker needs to build the isolate of the program the main `Vm` loaded (plan §7, L17).
///
/// It is made of plain data, so it is `Send + Sync` without any `unsafe`: the text of every file
/// the load read, the build configuration, the text of the host interface the program was
/// checked against, and which entry to load. The hash of the checked program's tables travels
/// beside it (see [`program_hash`]).
#[derive(Debug)]
pub struct ProgramImage {
    /// Every file the load read, as the main `Vm` saw it (a `RecordingFs` snapshot).
    pub(crate) files: MemoryFs,
    pub(crate) entry: OwnedEntry,
    pub(crate) cfg: BuildCfg,
    /// The interface text of each host module, in the order of `Vm::module_names`.
    pub(crate) iface: Vec<(String, String)>,
}

impl ProgramImage {
    /// The names of the files in the image, for tests: the paths the load asked for.
    #[doc(hidden)]
    pub fn entry_path(&self) -> PathBuf {
        match &self.entry {
            OwnedEntry::File(p) | OwnedEntry::Package(p) => p.clone(),
        }
    }
}

/// A hash of the checked program's type, function, global and implementation tables: two checks
/// of the same sources against the same interface give the same hash, and any difference in what
/// an id means (the order of types, a signature, a host function) changes it. The ids of
/// portable values (a function, a type, a variant) are only meaningful between isolates whose
/// hashes are equal.
pub(crate) fn program_hash(program: &colibri_check::hir::Program) -> u64 {
    use std::fmt::Write as _;
    let mut text = String::new();
    for t in &program.types {
        let _ = write!(text, "{t:?};");
    }
    text.push('|');
    for f in &program.fns {
        let _ = write!(text, "{}:{:?}:{};", f.name, f.sig, f.type_params);
    }
    // The captures of a closure (a portable closure names its cells by index), in the order and
    // with the storage the checker gave them. No table hashed here holds a map, whose `Debug`
    // order would differ from one thread to the next.
    text.push('|');
    for f in &program.fns {
        for c in &f.body.captures {
            let local = f.body.locals.get(c.0 as usize);
            let _ = write!(
                text,
                "{}:{:?},",
                c.0,
                local.map(|l| (&l.ty, l.storage, l.mutable))
            );
        }
        text.push(';');
    }
    text.push('|');
    for h in &program.host_fns {
        let _ = write!(text, "{}.{}:{:?};", h.module, h.name, h.requires);
    }
    text.push('|');
    for g in &program.globals {
        let _ = write!(text, "{}:{:?}:{}:{};", g.name, g.ty, g.mutable, g.constant);
    }
    text.push('|');
    for i in &program.impls {
        let _ = write!(text, "{i:?};");
    }
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in text.bytes() {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

// ---- results ----

pub use colibri_core::taskrun::{TaskDone, TaskFailure};

/// A task that was submitted: wait for what it produced.
#[derive(Debug)]
pub struct PendingTask {
    rx: mpsc::Receiver<Result<TaskDone, TaskFailure>>,
}

impl PendingTask {
    /// Blocks the thread until the task has run.
    pub fn wait(self) -> Result<TaskDone, TaskFailure> {
        self.rx.recv().unwrap_or(Err(TaskFailure::Cancelled))
    }

    /// The task's result if it has finished.
    pub fn try_wait(&self) -> Option<Result<TaskDone, TaskFailure>> {
        match self.rx.try_recv() {
            Ok(r) => Some(r),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => Some(Err(TaskFailure::Cancelled)),
        }
    }
}

// ---- the pool ----

/// What the pool counts, for tests and for a host that wants to know. Shared with the threads, so
/// it can be read after the `Vm` is gone.
#[derive(Debug, Default)]
pub struct PoolStats {
    /// Threads that were started.
    pub started: AtomicUsize,
    /// Threads that are running (started, and their loop not left yet).
    pub alive: AtomicUsize,
    /// Worker `Vm`s that were built (once per thread, and again if the program changes).
    pub builds: AtomicUsize,
    /// Tasks that ran to a result or a failure.
    pub tasks: AtomicUsize,
    /// Worker `Vm`s that could not be built (a mismatch, a panicking `worker_setup`). A thread
    /// remembers the failure for the image it failed on, so one image costs a thread one attempt.
    pub failed_builds: AtomicUsize,
    /// Threads that were joined: counted when `join` of the thread returned.
    pub joined: AtomicUsize,
}

/// What every worker needs from the `Vm` that owns the pool.
pub(crate) struct WorkerCfg {
    /// `None`: one thread per core, asked of the system when the threads start.
    pub threads: Option<usize>,
    pub setup: Option<WorkerSetup>,
    pub out: WorkerOut,
    pub caps: CapSet,
    pub max_call_depth: usize,
    /// Whether the isolates track their objects for the leak detector (the host asked for it on
    /// the main `Vm`); each worker then reports what it leaks when the pool shuts down.
    pub track_leaks: bool,
    /// The most temporary threads at a time (see [`Shared::make_room`]); [`MAX_EXTRA_THREADS`]
    /// unless a test lowers it.
    pub max_extra: usize,
    /// Whether this platform can start threads (not the web): the pool's one seam to the
    /// platform, so that a test can see the refusal without a web build.
    pub threads_supported: bool,
}

/// What a worker runs.
pub(crate) enum JobEntry {
    /// A function of the program by name, with portable arguments (the substrate's own tests).
    Call {
        function: String,
        args: Vec<Portable>,
    },
    /// A closure (or function value) that `task.run` copied.
    Closure(Portable),
}

/// Where the outcome of a task goes.
pub(crate) enum Reply {
    /// To a [`PendingTask`], which waits on a channel.
    Channel(mpsc::Sender<Result<TaskDone, TaskFailure>>),
    /// To the isolate that started it, which waits on a background operation.
    Completer(Completer<TaskOutcome>),
}

impl Reply {
    fn send(self, outcome: Result<TaskDone, TaskFailure>) {
        match self {
            // The submitter may have dropped its handle (a detached task).
            Reply::Channel(tx) => {
                let _ = tx.send(outcome);
            }
            Reply::Completer(completer) => completer.complete(outcome),
        }
    }
}

/// One task for a worker.
struct Job {
    /// What `steal` finds it by.
    key: u64,
    image: Arc<ProgramImage>,
    hash: u64,
    entry: JobEntry,
    /// The instruction budget of one entry, and the memory account and limit of the parent.
    budget: Option<u64>,
    account: (u64, Option<u64>),
    /// Names the task for what is derived per task (the seed of the random generator).
    serial: u64,
    reply: Reply,
}

/// What a submission carries: see [`Vm::submit_portable`].
pub(crate) struct Submission {
    pub image: Arc<ProgramImage>,
    pub hash: u64,
    pub entry: JobEntry,
    pub budget: Option<u64>,
    pub account: (u64, Option<u64>),
    pub serial: u64,
}

struct Queue {
    jobs: VecDeque<Job>,
    closed: bool,
}

pub(crate) struct Shared {
    queue: Mutex<Queue>,
    wake: Condvar,
    /// The key of the next job.
    next_key: AtomicU64,
    /// Set when the pool shuts down: the task that runs stops at its next safepoint, like the
    /// debugger's pause request (`SafepointKind::Cancel`).
    pub(crate) cancel: Arc<AtomicBool>,
    /// How many pool threads wait for a job (not running one).
    idle: AtomicUsize,
    /// Set before the pool shuts down when the workers are to report their leaks: each thread
    /// shuts its isolate down as it ends and adds what the leak detector finds to `leaks`.
    collect_leaks: AtomicBool,
    leaks: Mutex<Vec<colibri_core::leaks::Leak>>,
    /// Counts the `Shared`s whose last handle died: an idle thread that sees it change frees the
    /// frozen copies its isolate kept.
    sweep: AtomicU64,
    /// How many threads that run a job sleep, waiting for a task or a channel (see
    /// [`TaskSpawner::waiting`]).
    blocked: AtomicUsize,
    /// The temporary threads a join started because every pool thread was busy.
    extras: Mutex<Vec<JoinHandle<()>>>,
    cfg: Arc<WorkerCfg>,
    stats: Arc<PoolStats>,
}

/// The most temporary threads at a time: they are bounded by the depth of nested joins, and this
/// keeps a program that nests without end from starting threads without end.
pub(crate) const MAX_EXTRA_THREADS: usize = 256;

impl Shared {
    /// A waiter that no thread may be free to help asks for room: whatever is queued must run, and
    /// the pool lends a temporary thread for it, which runs jobs until the queue is empty and then
    /// ends. So a pool of a fixed size cannot be
    /// deadlocked by tasks that wait for tasks or for channels the queued ones feed.
    ///
    /// A thread is lent only when it is needed: no thread is idle (an idle one takes the job), and
    /// every thread that runs a job is waiting itself (one that computes reaches the queue when it
    /// ends). `caller_waits` says that the caller is a pool thread about to sleep, which is not
    /// counted yet. A pool that is merely busy therefore never grows. At the limit of temporary
    /// threads the waiter is told, to panic with it, instead of waiting for ever.
    /// A `Shared` lost its last handle: idle workers sweep their copies. The queue lock is taken
    /// between the bump and the notify, so a worker that has read the old count under it is
    /// already waiting (or has not yet read, and sees the new one): the wakeup is never lost.
    fn announce_sweep(&self) {
        self.sweep.fetch_add(1, Ordering::SeqCst);
        drop(lock(&self.queue));
        self.wake.notify_all();
    }

    fn make_room(self: &Arc<Self>, caller_waits: bool) -> Result<(), String> {
        {
            let queue = lock(&self.queue);
            if queue.closed
                || queue.jobs.is_empty()
                || self.idle.load(Ordering::SeqCst) > 0
                || self.blocked.load(Ordering::SeqCst) + usize::from(caller_waits)
                    < self.stats.alive.load(Ordering::SeqCst)
            {
                return Ok(());
            }
        }
        let mut extras = lock(&self.extras);
        // The finished ones are joined as they are found.
        let mut i = 0;
        while i < extras.len() {
            if extras[i].is_finished() {
                let _ = extras.swap_remove(i).join();
                self.stats.joined.fetch_add(1, Ordering::SeqCst);
            } else {
                i += 1;
            }
        }
        if lock(&self.queue).closed {
            return Ok(());
        }
        if extras.len() >= self.cfg.max_extra {
            return Err(format!(
                "tasks are waiting for tasks that no thread is free to run, and the pool has already lent its limit of {} temporary threads: a chain of nested waits that deep cannot be run (wait less deeply, or finish tasks before starting more)",
                self.cfg.max_extra
            ));
        }
        let (shared, cfg, stats) = (
            Arc::clone(self),
            Arc::clone(&self.cfg),
            Arc::clone(&self.stats),
        );
        stats.alive.fetch_add(1, Ordering::SeqCst);
        let spawned = std::thread::Builder::new()
            .name("colibri-task-extra".to_string())
            .stack_size(WORKER_STACK)
            .spawn({
                let stats = Arc::clone(&stats);
                move || worker_main(&shared, &cfg, &stats, true)
            });
        match spawned {
            Ok(handle) => {
                stats.started.fetch_add(1, Ordering::SeqCst);
                extras.push(handle);
                Ok(())
            }
            Err(e) => {
                stats.alive.fetch_sub(1, Ordering::SeqCst);
                Err(format!(
                    "tasks are waiting for tasks that no thread is free to run, and the operating system cannot start a temporary thread: {e}"
                ))
            }
        }
    }

    fn key(&self) -> u64 {
        self.next_key.fetch_add(1, Ordering::Relaxed)
    }

    /// Queues a job and wakes a thread. A closed queue (the `Vm` is going away) refuses it.
    fn enqueue(&self, job: Job) -> Result<(), TaskFailure> {
        {
            let mut queue = lock(&self.queue);
            if queue.closed {
                return Err(TaskFailure::Cancelled);
            }
            queue.jobs.push_back(job);
        }
        self.wake.notify_one();
        Ok(())
    }

    /// Takes the closure job `key` out of the queue, if no thread has started it.
    fn steal(&self, key: u64) -> Option<StolenTask> {
        let mut queue = lock(&self.queue);
        let at = queue
            .jobs
            .iter()
            .position(|job| job.key == key && matches!(job.entry, JobEntry::Closure(_)))?;
        let job = queue.jobs.remove(at)?;
        match (job.entry, job.reply) {
            (JobEntry::Closure(closure), Reply::Completer(completer)) => Some(StolenTask {
                closure,
                completer,
                serial: job.serial,
                hash: job.hash,
            }),
            _ => None,
        }
    }
}

/// The worker threads of a `Vm`. Dropping the pool shuts it down: see [`TaskPool::shutdown`].
pub(crate) struct TaskPool {
    cfg: Arc<WorkerCfg>,
    shared: Arc<Shared>,
    stats: Arc<PoolStats>,
    handles: Vec<JoinHandle<()>>,
}

impl TaskPool {
    /// A pool that has started nothing: the threads start with the first task.
    pub(crate) fn new(cfg: WorkerCfg, stats: Arc<PoolStats>) -> Self {
        let cfg = Arc::new(cfg);
        TaskPool {
            cfg: Arc::clone(&cfg),
            shared: Arc::new(Shared {
                queue: Mutex::new(Queue {
                    jobs: VecDeque::new(),
                    closed: false,
                }),
                wake: Condvar::new(),
                next_key: AtomicU64::new(1),
                cancel: Arc::new(AtomicBool::new(false)),
                idle: AtomicUsize::new(0),
                blocked: AtomicUsize::new(0),
                sweep: AtomicU64::new(0),
                collect_leaks: AtomicBool::new(false),
                leaks: Mutex::new(Vec::new()),
                extras: Mutex::new(Vec::new()),
                cfg,
                stats: Arc::clone(&stats),
            }),
            stats,
            handles: Vec::new(),
        }
    }

    /// How many threads the pool runs (once started).
    pub(crate) fn size(&self) -> usize {
        self.cfg.threads.unwrap_or_else(|| {
            std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get)
        })
    }

    /// Queues a task, starting the threads if this is the first.
    pub(crate) fn submit(&mut self, job: Submission) -> Result<PendingTask, TaskFailure> {
        let (reply, rx) = mpsc::channel();
        self.submit_with(job, Reply::Channel(reply))?;
        Ok(PendingTask { rx })
    }

    /// [`TaskPool::submit`] with the outcome going to `reply`; gives the key of the job.
    pub(crate) fn submit_with(
        &mut self,
        job: Submission,
        reply: Reply,
    ) -> Result<u64, TaskFailure> {
        if !self.cfg.threads_supported {
            return Err(TaskFailure::CannotStart(
                colibri_core::external::NO_THREADS.to_string(),
            ));
        }
        self.start_threads()?;
        let key = self.shared.key();
        self.shared.enqueue(Job {
            key,
            image: job.image,
            hash: job.hash,
            entry: job.entry,
            budget: job.budget,
            account: job.account,
            serial: job.serial,
            reply,
        })?;
        Ok(key)
    }

    fn start_threads(&mut self) -> Result<(), TaskFailure> {
        if !self.handles.is_empty() {
            return Ok(());
        }
        for index in 0..self.size().max(1) {
            let shared = Arc::clone(&self.shared);
            let cfg = Arc::clone(&self.cfg);
            let stats = Arc::clone(&self.stats);
            // Counted before the thread runs, so a reader that saw the thread start sees it alive.
            stats.alive.fetch_add(1, Ordering::SeqCst);
            let spawned = std::thread::Builder::new()
                .name(format!("colibri-task-{index}"))
                .stack_size(WORKER_STACK)
                .spawn({
                    let stats = Arc::clone(&stats);
                    move || worker_main(&shared, &cfg, &stats, false)
                });
            match spawned {
                Ok(handle) => {
                    stats.started.fetch_add(1, Ordering::SeqCst);
                    self.handles.push(handle);
                }
                Err(e) => {
                    stats.alive.fetch_sub(1, Ordering::SeqCst);
                    if self.handles.is_empty() {
                        return Err(TaskFailure::CannotStart(format!(
                            "the operating system cannot start a thread: {e}"
                        )));
                    }
                    // Fewer threads than asked for still run the tasks.
                    break;
                }
            }
        }
        Ok(())
    }

    /// Stops taking tasks: the ones still queued never run (their handles report
    /// [`TaskFailure::Cancelled`]), and the threads leave their loop once the task each is
    /// running ends. Does not wait. (The queue is emptied here and every thread also looks at
    /// `closed` before it takes a job: either alone would do, and a thread that was between
    /// the two when the pool closed must not start a job.)
    pub(crate) fn close(&mut self) {
        let dropped = {
            let mut queue = lock(&self.shared.queue);
            queue.closed = true;
            std::mem::take(&mut queue.jobs)
        };
        // Dropping a job drops its reply sender; the portable arguments are freed here.
        drop(dropped);
        self.shared.wake.notify_all();
    }

    /// Closes the pool and waits for every thread to end. A task that is running is **cancelled**:
    /// it stops at its next safepoint (a few thousand steps), reports [`TaskFailure::Cancelled`],
    /// and its thread drops the worker `Vm` and ends. No thread is left behind, and none is
    /// detached. What a cancellation cannot interrupt is a host function that blocks (a read
    /// from standard input, a sleep): the shutdown waits for it to return.
    pub(crate) fn shutdown_and_collect_leaks(&mut self) -> Vec<colibri_core::leaks::Leak> {
        self.shared.collect_leaks.store(true, Ordering::SeqCst);
        self.shutdown();
        std::mem::take(&mut *lock(&self.shared.leaks))
    }

    pub(crate) fn shutdown(&mut self) {
        self.shared.cancel.store(true, Ordering::Release);
        self.close();
        let extras = std::mem::take(&mut *lock(&self.shared.extras));
        for handle in self.handles.drain(..).chain(extras) {
            // A worker that panicked outside a task has nothing more to say.
            let _ = handle.join();
            self.stats.joined.fetch_add(1, Ordering::SeqCst);
        }
    }
}

impl Drop for TaskPool {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// ---- what the engine submits through ----

/// A number that names a task: the same on every run of a program, whatever thread runs it. A
/// task that `parent` (0 for the main program) started as its `n`-th gets it from both, so that
/// nested tasks are named by where they were started and not by when they were queued.
fn task_serial(parent: u64, n: u64) -> u64 {
    // SplitMix64's finaliser.
    let mut z = parent
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(n.wrapping_mul(0xBF58_476D_1CE4_E5B9))
        .wrapping_add(0x94D0_49BB_1331_11EB);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    (z ^ (z >> 31)) | 1
}

/// The pool of the main `Vm`, with what a task is submitted with: `task.run` goes through it.
pub(crate) struct Launch {
    pool: RefCell<TaskPool>,
    image: RefCell<Option<Arc<ProgramImage>>>,
    program: RefCell<Option<Rc<colibri_check::hir::Program>>>,
    hash: Cell<Option<u64>>,
    /// How many tasks the program has started.
    started: Cell<u64>,
}

impl Launch {
    pub(crate) fn new(pool: TaskPool) -> Launch {
        Launch {
            pool: RefCell::new(pool),
            image: RefCell::new(None),
            program: RefCell::new(None),
            hash: Cell::new(None),
            started: Cell::new(0),
        }
    }

    pub(crate) fn pool(&self) -> std::cell::RefMut<'_, TaskPool> {
        self.pool.borrow_mut()
    }

    /// A program was loaded: workers build their isolates from `image`.
    pub(crate) fn set_image(&self, image: Option<Arc<ProgramImage>>) {
        *self.image.borrow_mut() = image;
        self.hash.set(None);
    }

    /// The loaded program, which the hash that workers must reproduce is taken of.
    pub(crate) fn set_program(&self, program: Option<Rc<colibri_check::hir::Program>>) {
        *self.program.borrow_mut() = program;
        self.hash.set(None);
    }

    pub(crate) fn image(&self) -> Option<Arc<ProgramImage>> {
        self.image.borrow().clone()
    }

    /// The hash of the loaded program's tables, made when the first task is submitted.
    pub(crate) fn hash(&self) -> Option<u64> {
        if self.hash.get().is_none() {
            self.hash
                .set(self.program.borrow().as_deref().map(program_hash));
        }
        self.hash.get()
    }

    /// Makes the next task carry a hash its workers will not reproduce (tests of the check).
    pub(crate) fn corrupt_hash(&self) {
        let hash = self.hash();
        self.hash.set(hash.map(|h| h ^ 1));
    }

    /// The image and hash a task is submitted with.
    pub(crate) fn spec(&self) -> Option<(Arc<ProgramImage>, u64)> {
        Some((self.image()?, self.hash()?))
    }
}

impl TaskSpawner for Launch {
    fn spawn(
        &self,
        request: SpawnRequest,
        completer: Completer<TaskOutcome>,
    ) -> Result<u64, TaskFailure> {
        let Some((image, hash)) = self.spec() else {
            return Err(TaskFailure::CannotStart(
                "no program image is loaded that a worker could build its isolate from (a thread task needs a program loaded to run, not a REPL session)".to_string(),
            ));
        };
        let n = self.started.get() + 1;
        self.started.set(n);
        self.pool.borrow_mut().submit_with(
            Submission {
                image,
                hash,
                entry: JobEntry::Closure(request.closure),
                budget: request.budget,
                account: request.account,
                serial: task_serial(0, n),
            },
            Reply::Completer(completer),
        )
    }

    /// The main `Vm` has nothing to take over: its thread is not one of the pool's.
    fn steal(&self, _key: u64) -> Option<StolenTask> {
        None
    }

    // Main's wait is only a latency gain: every worker that waits asks for room itself on each
    // of its 50 ms polls (with itself counted), which is the stronger check. Asking here too
    // saves the main thread up to one poll. `try_borrow`: the call must never panic.
    fn make_room(&self) -> Result<(), String> {
        let Ok(pool) = self.pool.try_borrow() else {
            return Ok(());
        };
        let shared = Arc::clone(&pool.shared);
        drop(pool);
        shared.make_room(false)
    }

    fn shared_gone(&self) {
        if let Ok(pool) = self.pool.try_borrow() {
            pool.shared.announce_sweep();
        }
    }
}

/// The program and the serial of the task a worker is running.
type RunningTask = Option<(Arc<ProgramImage>, u64, u64)>;

/// What a worker submits through: the queue of the pool it belongs to, for the task it runs.
pub(crate) struct WorkerLaunch {
    shared: Arc<Shared>,
    now: Rc<RefCell<RunningTask>>,
    started: Rc<Cell<u64>>,
}

impl WorkerLaunch {
    pub(crate) fn new(shared: Arc<Shared>) -> WorkerLaunch {
        WorkerLaunch {
            shared,
            now: Rc::new(RefCell::new(None)),
            started: Rc::new(Cell::new(0)),
        }
    }

    /// The worker begins the task `serial` of `image`: tasks it starts are queued for the same
    /// program.
    pub(crate) fn enter(&self, image: &Arc<ProgramImage>, hash: u64, serial: u64) {
        *self.now.borrow_mut() = Some((Arc::clone(image), hash, serial));
        self.started.set(0);
    }
}

impl WorkerLaunch {
    /// The serial of the next task the running task starts: numbered from the running task's own
    /// serial and from how many it has started. `None` outside of a task.
    fn next_serial(&self) -> Option<u64> {
        let parent = self.now.borrow().as_ref()?.2;
        let n = self.started.get() + 1;
        self.started.set(n);
        Some(task_serial(parent, n))
    }
}

impl TaskSpawner for WorkerLaunch {
    fn spawn(
        &self,
        request: SpawnRequest,
        completer: Completer<TaskOutcome>,
    ) -> Result<u64, TaskFailure> {
        let Some((image, hash, _)) = self.now.borrow().clone() else {
            return Err(TaskFailure::Internal(
                "a worker started a task outside of one".to_string(),
            ));
        };
        let serial = self.next_serial().expect("checked above");
        let key = self.shared.key();
        self.shared.enqueue(Job {
            key,
            image,
            hash,
            entry: JobEntry::Closure(request.closure),
            budget: request.budget,
            account: request.account,
            serial,
            reply: Reply::Completer(completer),
        })?;
        Ok(key)
    }

    fn make_room(&self) -> Result<(), String> {
        self.shared.make_room(true)
    }

    fn shared_gone(&self) {
        self.shared.announce_sweep();
    }

    fn waiting(&self, on: bool) {
        if on {
            self.shared.blocked.fetch_add(1, Ordering::SeqCst);
        } else {
            self.shared.blocked.fetch_sub(1, Ordering::SeqCst);
        }
    }

    fn steal(&self, key: u64) -> Option<StolenTask> {
        let stolen = self.shared.steal(key)?;
        // One program per pool: a job of another program would run on an isolate built for this
        // one.
        debug_assert!(
            self.now
                .borrow()
                .as_ref()
                .is_none_or(|(_, hash, _)| *hash == stolen.hash),
            "a task of another program was taken over"
        );
        Some(stolen)
    }

    /// The tasks that the task `serial` starts are numbered from it, and the counter of the task
    /// that was running (the joiner, when `serial` runs inline in its join) comes back after.
    fn enter_task(&self, serial: u64) -> Box<dyn FnOnce()> {
        let was = self.now.borrow().clone();
        let started = self.started.get();
        if let Some((image, hash, _)) = &was {
            *self.now.borrow_mut() = Some((Arc::clone(image), *hash, serial));
        }
        self.started.set(0);
        let (now, counter) = (Rc::clone(&self.now), Rc::clone(&self.started));
        Box::new(move || {
            *now.borrow_mut() = was;
            counter.set(started);
        })
    }
}

/// Decrements the count of live threads when the thread's loop ends, after its `Vm` is dropped.
struct Alive<'a>(&'a PoolStats);

impl Drop for Alive<'_> {
    fn drop(&mut self) {
        self.0.alive.fetch_sub(1, Ordering::SeqCst);
    }
}

/// The loop of a pool thread.
/// A `temporary` thread (started by a join, see [`Shared::make_room`]) ends when it finds no job.
fn worker_main(shared: &Arc<Shared>, cfg: &WorkerCfg, stats: &PoolStats, temporary: bool) {
    let _alive = Alive(stats);
    // The isolate of this thread, and the image it was built from.
    let mut isolate: Option<(Vm, Arc<ProgramImage>, u64)> = None;
    worker_loop(shared, cfg, stats, temporary, &mut isolate);
    // The pool is closing. An isolate that tracked its objects says what it leaks, once its
    // program has ended; the `Vm` that owns the pool collects the reports.
    if shared.collect_leaks.load(Ordering::SeqCst)
        && let Some((vm, _, _)) = isolate.as_mut()
    {
        let found = catch_unwind(AssertUnwindSafe(|| {
            vm.shutdown();
            vm.find_leaks()
        }));
        if let Ok(found) = found {
            lock(&shared.leaks).extend(found);
        }
    }
}

/// The jobs of a pool thread, until the pool closes.
fn worker_loop(
    shared: &Arc<Shared>,
    cfg: &WorkerCfg,
    stats: &PoolStats,
    temporary: bool,
    isolate: &mut Option<(Vm, Arc<ProgramImage>, u64)>,
) {
    // The image this thread failed to build an isolate from, and why: the tasks that wait for the
    // same image fail alike without another full check.
    let mut failed: Option<(Arc<ProgramImage>, u64, String)> = None;
    // The count of `Shared`s that died that this thread has acted on.
    let mut swept = 0;
    loop {
        let job = loop {
            let mut queue = lock(&shared.queue);
            if queue.closed {
                return;
            }
            if let Some(job) = queue.jobs.pop_front() {
                break job;
            }
            if temporary {
                return;
            }
            let died = shared.sweep.load(Ordering::SeqCst);
            if died != swept {
                // A `Shared` lost its last handle: the copy this isolate kept of it is garbage
                // now, and an idle thread would hold it until its next task. (Not under the
                // lock: a `Drop` hook may run.)
                swept = died;
                drop(queue);
                if let Some((vm, _, _)) = isolate.as_mut() {
                    let _ = catch_unwind(AssertUnwindSafe(|| vm.sweep_when_idle()));
                }
                continue;
            }
            shared.idle.fetch_add(1, Ordering::SeqCst);
            queue = shared
                .wake
                .wait(queue)
                .unwrap_or_else(PoisonError::into_inner);
            shared.idle.fetch_sub(1, Ordering::SeqCst);
            drop(queue);
        };
        let Job {
            image,
            hash,
            entry,
            budget,
            account,
            serial,
            reply,
            ..
        } = job;
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            let stale = match &*isolate {
                Some((_, built, built_hash)) => !Arc::ptr_eq(built, &image) || *built_hash != hash,
                None => true,
            };
            if shared.cancel.load(Ordering::Acquire) {
                return Err(TaskFailure::Cancelled);
            }
            if stale {
                if let Some((bad, bad_hash, why)) = &failed
                    && Arc::ptr_eq(bad, &image)
                    && *bad_hash == hash
                {
                    return Err(TaskFailure::Internal(why.clone()));
                }
                // The old isolate goes first: its program and its host state are freed before the
                // new one is built.
                *isolate = None;
                match Vm::build_worker(cfg, &image, hash, shared) {
                    Ok(vm) => {
                        stats.builds.fetch_add(1, Ordering::SeqCst);
                        failed = None;
                        *isolate = Some((vm, Arc::clone(&image), hash));
                    }
                    Err(why) => {
                        stats.failed_builds.fetch_add(1, Ordering::SeqCst);
                        failed = Some((Arc::clone(&image), hash, why.clone()));
                        return Err(TaskFailure::Internal(why));
                    }
                }
            }
            let Some((vm, _, _)) = isolate.as_mut() else {
                return Err(TaskFailure::Internal("no worker".to_string()));
            };
            vm.run_task(entry, budget, account, serial, (&image, hash))
        }));
        let result = match outcome {
            Ok(result) => result,
            Err(payload) => {
                // A Rust panic in the engine: the isolate cannot be trusted, so the next task
                // builds a new one.
                *isolate = None;
                Err(TaskFailure::Internal(format!(
                    "the worker panicked: {}",
                    crate::host::payload_message(payload.as_ref())
                )))
            }
        };
        stats.tasks.fetch_add(1, Ordering::SeqCst);
        reply.send(result);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sink that records each `write` call on its own, so that a test can see how the writer
    /// cut the text.
    struct Calls(Arc<Mutex<Vec<String>>>);

    impl Write for Calls {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .unwrap()
                .push(String::from_utf8_lossy(buf).into_owned());
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_platform_without_threads_refuses_the_task_and_starts_nothing() {
        // The seam of the web build: `threads_supported` is `false` there.
        let stats = Arc::new(PoolStats::default());
        let cfg = WorkerCfg {
            threads: Some(2),
            setup: None,
            out: WorkerOut::Process,
            caps: CapSet::default(),
            max_call_depth: 100,
            track_leaks: false,
            max_extra: MAX_EXTRA_THREADS,
            threads_supported: false,
        };
        let mut pool = TaskPool::new(cfg, Arc::clone(&stats));
        let image = Arc::new(ProgramImage {
            files: MemoryFs::default(),
            entry: OwnedEntry::File(PathBuf::from("a.colibri")),
            cfg: BuildCfg::default(),
            iface: Vec::new(),
        });
        let refused = pool
            .submit(Submission {
                image,
                hash: 0,
                entry: JobEntry::Call {
                    function: "f".to_string(),
                    args: Vec::new(),
                },
                budget: None,
                account: (0, None),
                serial: 0,
            })
            .unwrap_err();
        assert_eq!(
            refused,
            TaskFailure::CannotStart(colibri_core::external::NO_THREADS.to_string())
        );
        assert_eq!(stats.started.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn the_tasks_a_task_starts_are_numbered_from_it_and_from_nothing_else() {
        let pool = TaskPool::new(
            WorkerCfg {
                threads: Some(1),
                setup: None,
                out: WorkerOut::Process,
                caps: CapSet::default(),
                max_call_depth: 100,
                track_leaks: false,
                max_extra: MAX_EXTRA_THREADS,
                threads_supported: false,
            },
            Arc::new(PoolStats::default()),
        );
        let shared = Arc::clone(&pool.shared);
        let launch = WorkerLaunch::new(shared);
        let image = Arc::new(ProgramImage {
            files: MemoryFs::default(),
            entry: OwnedEntry::File(PathBuf::from("a.colibri")),
            cfg: BuildCfg::default(),
            iface: Vec::new(),
        });
        assert_eq!(launch.next_serial(), None);
        launch.enter(&image, 1, 100);
        assert_eq!(launch.next_serial(), Some(task_serial(100, 1)));
        assert_eq!(launch.next_serial(), Some(task_serial(100, 2)));
        // A task that the worker runs next starts counting again.
        launch.enter(&image, 1, 200);
        assert_eq!(launch.next_serial(), Some(task_serial(200, 1)));
        // A task run inline in a join numbers what it starts from its own serial, and the task that
        // joined goes on where it was.
        let back = launch.enter_task(300);
        assert_eq!(launch.next_serial(), Some(task_serial(300, 1)));
        assert_eq!(launch.next_serial(), Some(task_serial(300, 2)));
        back();
        assert_eq!(launch.next_serial(), Some(task_serial(200, 2)));
    }

    #[test]
    fn a_line_writer_holds_a_partial_line_until_it_is_whole() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let sink: SharedOut = Arc::new(Mutex::new(Box::new(Calls(Arc::clone(&calls)))));
        let mut one = LineWriter::new(Arc::clone(&sink));
        let mut two = LineWriter::new(sink);
        // `writeln!` writes the text and the newline apart; two threads' pieces interleave.
        one.write_all(b"from one").unwrap();
        two.write_all(b"from two").unwrap();
        two.write_all(b"\n").unwrap();
        one.write_all(b"\n").unwrap();
        // Each line went out whole, in the order it was finished.
        assert_eq!(*calls.lock().unwrap(), ["from two\n", "from one\n"]);
        // Text without a newline waits for a flush.
        one.write_all(b"tail").unwrap();
        assert_eq!(calls.lock().unwrap().len(), 2);
        one.flush().unwrap();
        assert_eq!(calls.lock().unwrap().last().unwrap(), "tail");
    }
}
