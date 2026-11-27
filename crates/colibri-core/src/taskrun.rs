//! Thread tasks as an engine sees them (DESIGN §14.1, ARCHITECTURE §36.4).
//!
//! `task.run(f)` copies `f` into its portable form and hands it to a [`TaskSpawner`], which queues
//! it on the worker pool of the program's `Vm`. The `Task` the program holds is a class object that
//! names an operation of the scheduler's background operations ([`crate::external`]); the
//! [`TaskTable`] of the [`Core`] keeps what the operation delivers until `join` takes it. A
//! coroutine that joins parks on `Wake::External(op)`, and the main fiber runs the event loop.
//!
//! The types that cross threads are here too: what a task delivers ([`TaskOutcome`]) and why it
//! failed ([`TaskFailure`]) are plain data.

use std::collections::HashMap;
use std::rc::Rc;

use colibri_check::types::TASK_TYPE;

use crate::ctl::PanicInfo;
use crate::external::{Completer, ExternalError, Waited};
use crate::heap::ClassObj;
use crate::portable::Portable;
use crate::sched::Pending;
use crate::state::Core;
use crate::value::Value;

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
    /// The task passed the memory limit of the `Vm` (the limit is the task's own: what the `Vm`
    /// and its other workers hold counts, but only the isolate that allocates past it fails).
    MemoryLimit { limit: u64 },
    /// An argument could not be rebuilt on the worker, or the result could not be copied out
    /// (the message names the reason).
    Copy(String),
    /// The worker could not be built (the program image does not match, or `worker_setup` failed),
    /// or the engine failed. A bug, not a failure of the task.
    Internal(String),
    /// The pool could not start a task: there are no threads on the web.
    CannotStart(String),
    /// The `Vm` was dropped before the task started, or while it ran (it was stopped at a
    /// safepoint).
    Cancelled,
}

impl std::fmt::Display for TaskFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TaskFailure::Panic { message, .. } => f.write_str(message),
            TaskFailure::BudgetExhausted => f.write_str("the instruction budget ran out"),
            TaskFailure::MemoryLimit { limit } => f.write_str(&crate::mem::limit_message(*limit)),
            TaskFailure::Copy(message) => f.write_str(message),
            TaskFailure::Internal(message) => write!(f, "internal error: {message}"),
            TaskFailure::CannotStart(message) => f.write_str(message),
            TaskFailure::Cancelled => f.write_str("the task was cancelled: the Vm was dropped"),
        }
    }
}

/// What a task delivers: its result's portable form, or why there is none.
pub type TaskOutcome = Result<TaskDone, TaskFailure>;

// A task's outcome crosses from the worker to the isolate that joins it.
const _: () = {
    const fn send<T: Send>() {}
    send::<TaskOutcome>();
    send::<StolenTask>();
};

/// The failure of a task that ended in the panic `info`, as plain data: a panic of the program,
/// or the instruction budget or memory limit running out. The locations are looked up in the
/// sources of `core`.
pub fn failure_of_panic(core: &Core, info: PanicInfo) -> TaskFailure {
    let sources = core.sources();
    let locate = |span: colibri_syntax::Span| {
        if !sources.contains(span.file) {
            return ("<host>".to_string(), 0, 0);
        }
        let (line, column) = sources.line_col(span.file, span.start);
        (sources.name(span.file).to_string(), line, column)
    };
    if info.budget {
        return match info.memory {
            Some(limit) => TaskFailure::MemoryLimit { limit },
            None => TaskFailure::BudgetExhausted,
        };
    }
    let mut trace = String::new();
    for frame in &info.stack {
        let (file, line, column) = locate(frame.span);
        trace.push_str(&format!("{} ({file}:{line}:{column})\n", frame.function));
    }
    if trace.is_empty() && (!info.stack.is_empty() || info.span.start > 0) {
        let (file, line, column) = locate(info.span);
        trace = format!("({file}:{line}:{column})\n");
    }
    TaskFailure::Panic {
        message: info.message,
        trace,
    }
}

/// What `task.run` asks the pool for.
pub struct SpawnRequest {
    /// The closure (or function) to call, copied.
    pub closure: Portable,
    /// The instruction budget of one entry, and the memory account and limit the task runs
    /// under: the submitter's.
    pub budget: Option<u64>,
    pub account: (u64, Option<u64>),
}

/// A task that was still queued when its joiner asked for it: the closure to run, and the
/// completer that delivers its outcome.
pub struct StolenTask {
    pub closure: Portable,
    pub completer: Completer<TaskOutcome>,
    /// What names the task for what is derived per task (see [`TaskScope`]).
    pub serial: u64,
    /// The hash of the program the task was submitted for (the isolate that runs it inline must
    /// have been built for the same one).
    pub hash: u64,
}

/// What runs around a task so that it does not depend on which thread runs it or what that thread
/// ran before: it is given the task's serial number (the same on every run of the program) and
/// the host state of the isolate, and gives what puts that state back. The standard library
/// seeds its random generator from the serial.
pub type TaskScope = Rc<dyn Fn(&mut Core, u64) -> Box<dyn FnOnce(&mut Core)>>;

/// The pool of a program's `Vm`, as the engine sees it (`colibri-runtime` implements it).
pub trait TaskSpawner {
    /// Queues the task. `completer` delivers its outcome to the isolate that called. Gives the
    /// key to [`TaskSpawner::steal`] it back with, or the reason the task could not start.
    fn spawn(
        &self,
        request: SpawnRequest,
        completer: Completer<TaskOutcome>,
    ) -> Result<u64, TaskFailure>;

    /// Takes the task `key` out of the queue if no thread has started it, so that the isolate
    /// that waits for it runs it itself. Only a worker does that (a pool of one thread would
    /// deadlock on a task that joins another); the main `Vm` has nothing to take over.
    fn steal(&self, key: u64) -> Option<StolenTask>;

    /// A joiner that waits for the queued task `key` instead of running it (it has coroutines of
    /// its own to keep running) asks for a thread to take it if none is free. A pool thread that
    /// does so gets a temporary one.
    fn make_room(&self, _key: u64) {}

    /// The thread begins running the task `serial` (inline in a join, or as a job): what it
    /// starts is numbered from `serial`. The result puts back what was there.
    fn enter_task(&self, _serial: u64) -> Box<dyn FnOnce()> {
        Box::new(|| {})
    }
}

/// Where a task is.
enum TaskState {
    /// Started; its outcome has not been collected.
    Running(Pending<TaskOutcome>),
    /// Collected, and nobody has taken it.
    Ready(TaskOutcome),
    /// Taken by a `join` or `try_join`.
    Joined,
}

struct TaskEntry {
    name: Rc<str>,
    key: u64,
    state: TaskState,
}

/// What `poll` found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskPoll {
    /// Not finished.
    Running,
    /// Finished: its outcome is here, or was taken.
    Finished,
}

/// The tasks a `Core` started that have a handle, by the id of their operation.
#[derive(Default)]
pub struct TaskTable {
    entries: HashMap<u64, TaskEntry>,
    spawner: Option<Rc<dyn TaskSpawner>>,
    scope: Option<TaskScope>,
}

impl Core {
    /// Sets what runs around every task this engine runs for the pool (see [`TaskScope`]).
    pub fn set_task_scope(&mut self, scope: Option<TaskScope>) {
        self.tasks.scope = scope;
    }

    /// Enters the scope of the task `serial`; the result puts the state back.
    pub fn enter_task_scope(&mut self, serial: u64) -> Box<dyn FnOnce(&mut Core)> {
        let spawner_back = self.tasks.spawner.as_ref().map(|s| s.enter_task(serial));
        let scope_back: Box<dyn FnOnce(&mut Core)> = match self.tasks.scope.clone() {
            Some(scope) => scope(self, serial),
            None => Box::new(|_: &mut Core| {}),
        };
        Box::new(move |core| {
            scope_back(core);
            if let Some(back) = spawner_back {
                back();
            }
        })
    }

    /// Gives the engine the pool that `task.run` submits to.
    pub fn set_task_spawner(&mut self, spawner: Option<Rc<dyn TaskSpawner>>) {
        self.tasks.spawner = spawner;
    }

    /// The pool `task.run` submits to; `None` when the host has not given one.
    pub fn task_spawner(&self) -> Option<Rc<dyn TaskSpawner>> {
        // The web has no threads: no spawner, so all the code behind it can be left out of the
        // web runner (it is embedded in `colibri`, whose size is gated).
        if cfg!(target_family = "wasm") {
            return None;
        }
        self.tasks.spawner.clone()
    }

    /// Submits `request` and makes the `Task` handle of it. `name` is what the task's messages
    /// call it. `Err` is the reason the task did not start.
    pub fn start_task(
        &mut self,
        spawner: &dyn TaskSpawner,
        request: SpawnRequest,
        name: &str,
    ) -> Result<Value, String> {
        let mut key = 0;
        let pending = self
            .sched
            .start_completion::<TaskOutcome>(|completer| {
                key = spawner
                    .spawn(request, completer)
                    .map_err(|failure| failure.to_string())?;
                Ok(())
            })
            .map_err(|e| match e {
                ExternalError::CannotStart(message) | ExternalError::Failed(message) => message,
                ExternalError::Cancelled => "the task was cancelled".to_string(),
            })?;
        let op = pending.op;
        self.tasks.entries.insert(
            op,
            TaskEntry {
                name: Rc::from(name),
                key,
                state: TaskState::Running(pending),
            },
        );
        Ok(Value::Obj(ClassObj::new(
            TASK_TYPE,
            vec![Value::Int(op as i64)],
        )))
    }

    /// Whether the task `op` has finished, collecting its outcome if it has.
    pub fn task_poll(&mut self, op: u64) -> TaskPoll {
        let Some(entry) = self.tasks.entries.get_mut(&op) else {
            return TaskPoll::Finished;
        };
        if !matches!(entry.state, TaskState::Running(_)) {
            return TaskPoll::Finished;
        }
        self.sched.ext().drain();
        if !self.sched.ext().is_completed(op) {
            return TaskPoll::Running;
        }
        let TaskState::Running(pending) = std::mem::replace(&mut entry.state, TaskState::Joined)
        else {
            unreachable!("checked above")
        };
        // The operation stays marked completed until its handle dies: a coroutine that waits for
        // it is woken by that mark, and there may be several.
        let outcome = match self.sched.collect(pending, Waited::Finished) {
            Ok(outcome) => outcome,
            // The queue was closed and the job dropped, or the worker went away.
            Err(_) => Err(TaskFailure::Cancelled),
        };
        if let Some(entry) = self.tasks.entries.get_mut(&op) {
            entry.state = TaskState::Ready(outcome);
        }
        TaskPoll::Finished
    }

    /// The name of the task `op`, for its messages.
    pub fn task_name(&self, op: u64) -> Option<Rc<str>> {
        self.tasks.entries.get(&op).map(|e| Rc::clone(&e.name))
    }

    /// The key the pool knows the task `op` by, while its outcome is not collected.
    pub fn task_key(&self, op: u64) -> Option<u64> {
        let entry = self.tasks.entries.get(&op)?;
        matches!(entry.state, TaskState::Running(_)).then_some(entry.key)
    }

    /// Takes the outcome of a finished task: `Some` once, and `None` after (the task was joined).
    pub fn task_take(&mut self, op: u64) -> Option<TaskOutcome> {
        let entry = self.tasks.entries.get_mut(&op)?;
        match std::mem::replace(&mut entry.state, TaskState::Joined) {
            TaskState::Ready(outcome) => Some(outcome),
            other => {
                entry.state = other;
                None
            }
        }
    }

    /// The handle of the task `op` was dropped: nobody will ask for its outcome. The task goes on
    /// running (it is detached), and its outcome is dropped when it arrives.
    pub fn task_died(&mut self, op: u64) {
        if self.tasks.entries.remove(&op).is_some() {
            self.sched.ext().finish(op);
        }
    }

    /// How many tasks have a handle (for tests).
    pub fn task_count(&self) -> usize {
        self.tasks.entries.len()
    }
}

/// The operation id inside a `Task` handle.
pub fn task_op(v: &Value) -> Option<u64> {
    match v {
        Value::Obj(o) => task_op_of(o),
        _ => None,
    }
}

/// [`task_op`] of a class object.
pub fn task_op_of(o: &ClassObj) -> Option<u64> {
    if o.ty() != TASK_TYPE {
        return None;
    }
    match o.field(0) {
        Some(Value::Int(n)) => Some(n as u64),
        _ => None,
    }
}
