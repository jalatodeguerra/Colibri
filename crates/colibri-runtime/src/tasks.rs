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

use std::collections::VecDeque;
use std::io::Write;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, mpsc};
use std::thread::JoinHandle;

use colibri_check::package::{BuildCfg, MemoryFs};
pub use colibri_core::portable::Portable;

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

/// What a task that ran produced.
#[derive(Debug)]
pub struct TaskDone {
    pub value: Portable,
    /// The steps the entry used (the instruction budget counts in these).
    pub steps: u64,
}

/// Why a task gave no value. Plain data: it crosses back to the thread that submitted the task.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TaskFailure {
    /// The task panicked. `trace` is the stack, one frame a line.
    Panic { message: String, trace: String },
    /// The entry used up the instruction budget of the `Vm` (a worker's failure is not the main
    /// entry's: it reaches the joiner as an `err`).
    BudgetExhausted,
    /// The entry passed the memory limit of the `Vm`, which the worker shares.
    MemoryLimit { limit: u64 },
    /// An argument could not be rebuilt on the worker, or the result could not be copied out
    /// (the message names the reason).
    Copy(String),
    /// The worker could not be built (the program image does not match, or `worker_setup` failed),
    /// or the engine failed. A bug, not a failure of the task.
    Internal(String),
    /// The pool could not start a task: there are no threads on the web.
    CannotStart(String),
    /// The `Vm` was dropped before the task started.
    Cancelled,
}

impl std::fmt::Display for TaskFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TaskFailure::Panic { message, .. } => f.write_str(message),
            TaskFailure::BudgetExhausted => f.write_str("the instruction budget ran out"),
            TaskFailure::MemoryLimit { limit } => f.write_str(&colibri_core::mem::limit_message(*limit)),
            TaskFailure::Copy(message) => f.write_str(message),
            TaskFailure::Internal(message) => write!(f, "internal error: {message}"),
            TaskFailure::CannotStart(message) => f.write_str(message),
            TaskFailure::Cancelled => f.write_str("the task never ran: the Vm was dropped"),
        }
    }
}

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
}

/// What every worker needs from the `Vm` that owns the pool.
pub(crate) struct WorkerCfg {
    /// `None`: one thread per core, asked of the system when the threads start.
    pub threads: Option<usize>,
    pub setup: Option<WorkerSetup>,
    pub out: WorkerOut,
    pub caps: CapSet,
    pub max_call_depth: usize,
}

/// One task for a worker.
struct Job {
    image: Arc<ProgramImage>,
    hash: u64,
    function: String,
    args: Vec<Portable>,
    /// The instruction budget of one entry, and the memory account and limit of the parent.
    budget: Option<u64>,
    account: (u64, Option<u64>),
    reply: mpsc::Sender<Result<TaskDone, TaskFailure>>,
}

/// What a submission carries: see [`Vm::submit_portable`].
pub(crate) struct Submission {
    pub image: Arc<ProgramImage>,
    pub hash: u64,
    pub function: String,
    pub args: Vec<Portable>,
    pub budget: Option<u64>,
    pub account: (u64, Option<u64>),
}

struct Queue {
    jobs: VecDeque<Job>,
    closed: bool,
}

struct Shared {
    queue: Mutex<Queue>,
    wake: Condvar,
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
        TaskPool {
            cfg: Arc::new(cfg),
            shared: Arc::new(Shared {
                queue: Mutex::new(Queue {
                    jobs: VecDeque::new(),
                    closed: false,
                }),
                wake: Condvar::new(),
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
        if cfg!(target_family = "wasm") {
            return Err(TaskFailure::CannotStart(
                colibri_core::external::NO_THREADS.to_string(),
            ));
        }
        self.start_threads()?;
        let (reply, rx) = mpsc::channel();
        let job = Job {
            image: job.image,
            hash: job.hash,
            function: job.function,
            args: job.args,
            budget: job.budget,
            account: job.account,
            reply,
        };
        {
            let mut queue = lock(&self.shared.queue);
            if queue.closed {
                return Err(TaskFailure::Cancelled);
            }
            queue.jobs.push_back(job);
        }
        self.shared.wake.notify_one();
        Ok(PendingTask { rx })
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
                    move || worker_main(&shared, &cfg, &stats)
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
    /// running ends. Does not wait.
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

    /// Closes the pool and waits for every thread to end. A task that is running **finishes**
    /// first (the instruction budget bounds it when the `Vm` has one), then its thread drops the
    /// worker `Vm` and ends; no thread is left behind, and none is detached.
    pub(crate) fn shutdown(&mut self) {
        self.close();
        for handle in self.handles.drain(..) {
            // A worker that panicked outside a task has nothing more to say.
            let _ = handle.join();
        }
    }
}

impl Drop for TaskPool {
    fn drop(&mut self) {
        self.shutdown();
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
fn worker_main(shared: &Shared, cfg: &WorkerCfg, stats: &PoolStats) {
    let _alive = Alive(stats);
    // The isolate of this thread, and the image it was built from.
    let mut isolate: Option<(Vm, Arc<ProgramImage>, u64)> = None;
    loop {
        let job = {
            let mut queue = lock(&shared.queue);
            loop {
                if queue.closed {
                    return;
                }
                if let Some(job) = queue.jobs.pop_front() {
                    break job;
                }
                queue = shared
                    .wake
                    .wait(queue)
                    .unwrap_or_else(PoisonError::into_inner);
            }
        };
        let Job {
            image,
            hash,
            function,
            args,
            budget,
            account,
            reply,
        } = job;
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            let stale = match &isolate {
                Some((_, built, built_hash)) => !Arc::ptr_eq(built, &image) || *built_hash != hash,
                None => true,
            };
            if stale {
                // The old isolate goes first: its program and its host state are freed before the
                // new one is built.
                isolate = None;
                let vm = Vm::build_worker(cfg, &image, hash).map_err(TaskFailure::Internal)?;
                stats.builds.fetch_add(1, Ordering::SeqCst);
                isolate = Some((vm, Arc::clone(&image), hash));
            }
            let Some((vm, _, _)) = isolate.as_mut() else {
                return Err(TaskFailure::Internal("no worker".to_string()));
            };
            vm.run_task(&function, args, budget, account)
        }));
        let result = match outcome {
            Ok(result) => result,
            Err(payload) => {
                // A Rust panic in the engine: the isolate cannot be trusted, so the next task
                // builds a new one.
                isolate = None;
                Err(TaskFailure::Internal(format!(
                    "the worker panicked: {}",
                    crate::host::payload_message(payload.as_ref())
                )))
            }
        };
        stats.tasks.fetch_add(1, Ordering::SeqCst);
        // The submitter may have dropped its handle (a detached task).
        let _ = reply.send(result);
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
