//! `task.run`, `join`, `done` and `try_join` of thread tasks (DESIGN §14.1, ARCHITECTURE §36.4).
//!
//! `task.run(f)` copies the closure into its portable form and hands it to the pool of the
//! program's `Vm` ([`colibri_core::taskrun::TaskSpawner`]); the handle it returns names a
//! background operation of the scheduler. `join` waits for the operation the way every other
//! wait for a background operation does:
//!
//! - a coroutine parks its heap fiber on `Wake::External(op)`, with no native stack (the call is
//!   an instruction of the coroutine's own code, so it is simply run again when the operation is
//!   done);
//! - the main fiber runs the event loop until the operation completes;
//! - where neither is possible (inside a `Drop` hook, a host callback or a native operation) the
//!   thread blocks.
//!
//! A worker that joins a task nobody has started takes it out of the queue and runs it itself,
//! so that a pool of one thread cannot deadlock on nested tasks.

use std::rc::Rc;
use std::time::Duration;

use colibri_check::hir::Builtin;
use colibri_core::ctl::{Ctl, Eval};
use colibri_core::host::{context_error_value, err_variant, error_value, ok_variant};
use colibri_core::portable::{RebuildError, Refused, from_portable};
use colibri_core::prelude::*;
use colibri_core::sched::Wake;
use colibri_core::taskrun::{
    SpawnRequest, StolenTask, TaskDone, TaskFailure, TaskOutcome, TaskPoll, failure_of_panic,
    task_op,
};
use colibri_core::value::Value;
use colibri_syntax::Span;

use crate::fiber::VmFiber;
use crate::machine::Machine;
use crate::stacks::{WalkRole, on_walk_stack};

impl Machine {
    /// `task.run(f)`: copies `f` and queues it. Gives the `Task` handle.
    pub(crate) fn task_run(&mut self, fiber: &mut VmFiber, f: Value, span: Span) -> Eval {
        let Some(spawner) = self.core.task_spawner() else {
            self.release(f);
            return Err(self.panic(
                fiber,
                span,
                "`task.run` cannot start a thread task here: this `Vm` has no worker pool",
            ));
        };
        // What the task's messages call it: the function, or the closure as a trace names it
        // (`<closure in main>`).
        let name = match &f {
            Value::Fn(_, name) => name.to_string(),
            Value::Closure(c) => self.core.closure_fn(c).map_or_else(
                || "<closure>".to_string(),
                |id| {
                    self.fns
                        .get(id.0 as usize)
                        .map_or_else(|| "<closure>".to_string(), |slot| slot.body.name.clone())
                },
            ),
            _ => "<closure>".to_string(),
        };
        let budget = self.native_stack_budget;
        let copied = on_walk_stack(WalkRole::Sender, budget, |guard| {
            self.to_portable(&f, guard)
        });
        self.release(f);
        let closure = match copied {
            Ok(closure) => closure,
            // Copying the closure would pass the memory limit of the isolate that sends it.
            Err(e) if e.refused == Refused::MemoryLimit => {
                return Err(self.memory_exceeded(fiber, span));
            }
            Err(e) => return Err(self.panic(fiber, span, e.to_string())),
        };
        let request = SpawnRequest {
            closure,
            budget: self.core.budget.allotment,
            account: (self.core.mem.tag(), self.core.memory_limit()),
        };
        match self.core.start_task(&*spawner, request, &name) {
            Ok(handle) => Ok(handle),
            Err(why) => Err(self.panic(
                fiber,
                span,
                format!("`task.run` could not start the task: {why}"),
            )),
        }
    }

    /// `join` of a thread task: `Ok(Some(result))` when it is done, `Ok(None)` when the coroutine
    /// parked (the instruction runs again when the operation completes).
    pub(crate) fn join_task(
        &mut self,
        fiber: &mut VmFiber,
        recv: &Value,
        span: Span,
    ) -> Result<Option<Value>, Ctl> {
        let Some(op) = task_op(recv) else {
            return Err(self.internal(fiber, span, "ill-typed task method"));
        };
        loop {
            if self.core.task_poll(op) == TaskPoll::Finished {
                return self.task_result(fiber, op, span).map(Some);
            }
            // Nobody has started it, and a worker is the one waiting: run it here.
            if let Some(stolen) = self.take_over(op) {
                self.run_stolen(fiber, stolen, span)?;
                continue;
            }
            let state = &fiber.state;
            let quiet = state.host_depth == 0
                && state.native_depth == 0
                && self.core.hook_depth == 0
                && !self.core.sched.is_shutting_down();
            if state.coro.is_none() {
                if quiet && !self.core.sched.is_busy() {
                    let driven = self.drive_main(fiber, span, "join", None, &mut |it| {
                        it.core.task_poll(op) == TaskPoll::Finished
                    });
                    self.settle_refusal(fiber, span, driven)?;
                } else {
                    self.block_on_task(op);
                }
                continue;
            }
            if quiet {
                let id = self.pause_context(fiber, span, "join")?;
                self.suspend(fiber, id, Wake::External(op), true, span)?;
                return Ok(None);
            }
            self.block_on_task(op);
        }
    }

    /// Blocks the thread until the task `op` has finished: the one place a wait cannot let other
    /// code run (a `Drop` hook, a host callback, native code that calls user code).
    fn block_on_task(&mut self, op: u64) {
        while self.core.task_poll(op) == TaskPoll::Running {
            self.core.sched.ext().wait(Duration::from_millis(50));
        }
    }

    /// The queued task `op`, taken out of the pool's queue, if this isolate may run it.
    fn take_over(&mut self, op: u64) -> Option<StolenTask> {
        let key = self.core.task_key(op)?;
        let spawner = self.core.task_spawner()?;
        spawner.steal(key)
    }

    /// Runs a task that was queued, on this thread, inside the join that waits for it ("help
    /// while waiting"). It shares this isolate, so its steps count in the joiner's budget and its
    /// memory in the joiner's account; its outcome is delivered the way a worker's would be.
    fn run_stolen(
        &mut self,
        fiber: &mut VmFiber,
        stolen: StolenTask,
        span: Span,
    ) -> Result<(), Ctl> {
        let StolenTask {
            closure,
            completer,
            serial,
        } = stolen;
        let budget = self.native_stack_budget;
        let rebuilt = on_walk_stack(WalkRole::Receiver, budget, |guard| {
            from_portable(closure, &mut self.core, guard)
        });
        let f = match rebuilt {
            Ok(f) => f,
            Err(e) => {
                completer.complete(Err(self.rebuild_failure(e)));
                return Ok(());
            }
        };
        // The task nests inside this join on the same native stack: the budget of that stack is
        // counted from where this fiber started, so a chain of tasks that each join the next
        // one ends in a panic of the innermost, not in a stack overflow.
        let mut inner = VmFiber::new();
        inner.stack_base = fiber.stack_base;
        // The task sees the world it would see on a thread of its own.
        let restore = self.core.enter_task_scope(serial);
        let called = self.call_value(&mut inner, &f, Vec::new(), span);
        restore(&mut self.core);
        self.release(f);
        let outcome: TaskOutcome = match called {
            Ok(value) => {
                let copied = on_walk_stack(WalkRole::Sender, budget, |guard| {
                    self.to_portable(&value, guard)
                });
                self.release(value);
                match copied {
                    Ok(value) => Ok(TaskDone { value, steps: 0 }),
                    Err(e) if e.refused == Refused::MemoryLimit => Err(TaskFailure::MemoryLimit {
                        limit: self.core.memory_limit().unwrap_or(0),
                    }),
                    Err(e) => Err(TaskFailure::Copy(e.to_string())),
                }
            }
            Err(Ctl::Panic(info)) => Err(failure_of_panic(&self.core, *info)),
            Err(Ctl::Cancel) => Err(TaskFailure::Cancelled),
        };
        completer.complete(outcome);
        Ok(())
    }

    fn rebuild_failure(&self, e: RebuildError) -> TaskFailure {
        match e {
            RebuildError::MemoryLimit => TaskFailure::MemoryLimit {
                limit: self.core.memory_limit().unwrap_or(0),
            },
            other => TaskFailure::Copy(other.to_string()),
        }
    }

    /// The `Result` a `join` of the finished task `op` gives.
    fn task_result(&mut self, fiber: &mut VmFiber, op: u64, span: Span) -> Eval {
        let name = self
            .core
            .task_name(op)
            .unwrap_or_else(|| Rc::from("<closure>"));
        let Some(outcome) = self.core.task_take(op) else {
            return Ok(err_variant(error_value(&format!(
                "the task `{name}` was joined already: its result can be taken once"
            ))));
        };
        match outcome {
            Ok(done) => {
                let budget = self.native_stack_budget;
                let rebuilt = on_walk_stack(WalkRole::Receiver, budget, |guard| {
                    from_portable(done.value, &mut self.core, guard)
                });
                match rebuilt {
                    Ok(v) => Ok(ok_variant(v)),
                    // The joiner's own memory: its limit is what failed.
                    Err(RebuildError::MemoryLimit) => Err(self.memory_exceeded(fiber, span)),
                    Err(e) => Ok(err_variant(error_value(&format!(
                        "the task `{name}` finished, but its result cannot be received: {e}"
                    )))),
                }
            }
            Err(failure) => Ok(err_variant(failure_error(&name, &failure))),
        }
    }

    /// `t.done()` and `t.try_join()`.
    pub(crate) fn task_ops(
        &mut self,
        fiber: &mut VmFiber,
        op: Builtin,
        recv: &Value,
        span: Span,
    ) -> Eval {
        let Some(id) = task_op(recv) else {
            return Err(self.internal(fiber, span, "ill-typed task method"));
        };
        let finished = self.core.task_poll(id) == TaskPoll::Finished;
        match op {
            Builtin::TaskDone => Ok(Value::Bool(finished)),
            Builtin::TaskTryJoin if finished => {
                let result = self.task_result(fiber, id, span)?;
                Ok(self.core.option(Some(result)))
            }
            Builtin::TaskTryJoin => Ok(self.core.option(None)),
            _ => Err(self.internal(
                fiber,
                span,
                "a task method that is not `done` or `try_join`",
            )),
        }
    }
}

/// The error a failed task gives its joiner. A panic is the message with the trace under it
/// (`e.message()` is the message, `e.cause()` the trace, `{e}` both).
fn failure_error(name: &str, failure: &TaskFailure) -> Value {
    match failure {
        TaskFailure::Panic { message, trace } => {
            let headline = format!("the task `{name}` panicked: {message}");
            if trace.is_empty() {
                error_value(&headline)
            } else {
                context_error_value(error_value(trace.trim_end()), &headline)
            }
        }
        TaskFailure::Cancelled => error_value(&format!("the task `{name}` was cancelled")),
        other => error_value(&format!("the task `{name}` failed: {other}")),
    }
}
