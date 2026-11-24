//! Host functions that wait for a background operation (vm-plan §3.2, EMBEDDING §6).
//!
//! `await_external` and its relatives submit work to the scheduler's pool and wait for it. How
//! the wait goes depends on who waits. A coroutine whose host function runs on a native stack of
//! its own (`stacks`) parks that stack: the scheduler is told what the coroutine waits for
//! (`Wake::External`), the stack switches back to the dispatch loop, which ends its run with
//! `Exit::Suspend`, and the coroutine's frames and the stack wait in the fiber
//! ([`HostCont`]). When the operation is done the scheduler resumes the coroutine, which first
//! switches back onto the stack ([`Machine::continue_host`]); the host function sees its wait
//! over, finishes, and its call completes as one that never paused. The main fiber runs the
//! event loop while it waits, where it may; anywhere else (a callback, a `Drop` hook, native
//! code that calls user code, the web) the thread blocks.

use std::rc::Rc;

use colibri_check::hir::{Body, HostFnId};
use colibri_check::types::Ty;
use colibri_core::coro::Sched;
use colibri_core::ctl::Ctl;
use colibri_core::external::{CancelToken, Completer, ExternalError, Waited};
use colibri_core::sched::Wake;
use colibri_core::value::Value;
use colibri_syntax::Span;

use crate::fiber::VmFiber;
use crate::machine::Machine;
use crate::run::Exit;
use crate::stacks::{self, Parked, Step};

/// Where the result of a host call goes when the call completes after a pause.
#[derive(Clone, Copy, Debug)]
pub(crate) enum HostDest {
    /// An absolute register of the fiber: the call was an instruction of the coroutine's code.
    Reg(usize),
    /// The coroutine's own result: the host function is what `go` started.
    Entry,
}

/// What a call of a host function came to.
pub(crate) enum Called {
    /// The call is over, with this result.
    Done(Value),
    /// The host function is waiting on its stack, parked in `VmFiber::pending_host`: the
    /// dispatch loop must end its run with `Exit::Suspend`.
    Parked,
}

/// A host call that is waiting: the stack, and what completing the call needs of the call
/// that began it.
pub(crate) struct HostCont {
    parked: Parked,
    id: HostFnId,
    body: Rc<Body>,
    /// The type arguments of the call, which the host function reads while it runs.
    type_args: Vec<Ty>,
    span: Span,
    dest: HostDest,
}

impl std::fmt::Debug for HostCont {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostCont")
            .field("host_fn", &self.id.0)
            .field("dest", &self.dest)
            .finish_non_exhaustive()
    }
}

impl HostCont {
    pub(crate) fn new(
        parked: Parked,
        id: HostFnId,
        body: Rc<Body>,
        type_args: Vec<Ty>,
        span: Span,
        dest: HostDest,
    ) -> HostCont {
        HostCont {
            parked,
            id,
            body,
            type_args,
            span,
            dest,
        }
    }
}

impl Machine {
    /// Whether the call of host function `id` that is being made now (its frame is pushed and
    /// the host depth counts it) may run on a stack of its own, so that it can park: a coroutine
    /// calls it from its own code, not from a callback, a `Drop` hook or native code that calls
    /// user code, and the function may wait. On the web nothing parks.
    pub(crate) fn may_park(&self, fiber: &VmFiber, id: HostFnId) -> bool {
        cfg!(not(target_arch = "wasm32"))
            && fiber.state.coro.is_some()
            && fiber.state.host_depth == 1
            && fiber.state.native_depth == 0
            && fiber.entries <= 1
            && self.core.hook_depth == 0
            && !self.host_impls.is_no_await(id)
    }

    /// Whether the coroutine on `fiber` may suspend for a background operation: its host function
    /// runs on a stack that parks, at most one host function deep, outside a `Drop` hook and
    /// outside code that a built-in operation calls (a coroutine may not pause there, whatever
    /// it waits for).
    pub(crate) fn can_suspend_for_external(&self, fiber: &VmFiber) -> bool {
        fiber.state.coro.is_some()
            && fiber.state.host_depth <= 1
            && fiber.state.native_depth == 0
            && self.core.hook_depth == 0
            && stacks::on_host_stack(fiber)
    }

    /// Whether a wait for a background operation that `fiber` makes lets the rest of the program
    /// run meanwhile: a coroutine whose host function parks is suspended, and the main fiber
    /// drives the event loop where it may. When this is `false` the wait blocks the thread, and
    /// nothing that another coroutine still has to do can happen until the operation is done.
    pub fn external_wait_runs_others(&self, fiber: &VmFiber) -> bool {
        self.can_suspend_for_external(fiber) || self.can_drive(fiber, 1)
    }

    /// Runs `work` on a background thread (an idle one of the pool, or a new one: an operation
    /// never waits behind another) and waits for its result without blocking other coroutines:
    /// see the module. Meant to be called from a host function (host depth 1); it must not hold
    /// a borrow of host objects across the call, because other coroutines run meanwhile.
    /// `Err(Cancelled)` means the coroutine was cancelled while waiting: the host function must
    /// unwind (an operation whose thread has not started it yet is dropped).
    pub fn await_external<R: Send + 'static>(
        &mut self,
        fiber: &mut VmFiber,
        work: impl FnOnce() -> R + Send + 'static,
    ) -> Result<R, ExternalError> {
        self.await_external_with(fiber, move |_| work())
    }

    /// [`Machine::await_external`] for work that can stop early: `work` is given a
    /// [`CancelToken`] that is cancelled when the waiting coroutine is cancelled or the machine
    /// ends. An operation that holds a child process registers a hook that kills it; the hook
    /// runs on the cancelling side before it goes on, so nothing is left running.
    pub fn await_external_with<R: Send + 'static>(
        &mut self,
        fiber: &mut VmFiber,
        work: impl FnOnce(&CancelToken) -> R + Send + 'static,
    ) -> Result<R, ExternalError> {
        let pending = self.core.sched.submit_work(work)?;
        let waited = self.wait_for_completion(fiber, pending.op, &pending.token)?;
        self.core.sched.collect(pending, waited)
    }

    /// Waits for an operation that some other thread completes. `start` runs at once on the
    /// calling thread and is given the [`Completer`] to hand to whoever will finish the
    /// operation; an `Err` from it is [`ExternalError::CannotStart`] and nothing waits. Then the
    /// call waits as [`Machine::await_external_with`] does. A completer dropped without a
    /// result is `Err(Failed)`.
    #[doc(hidden)]
    pub fn await_completion<R: Send + 'static>(
        &mut self,
        fiber: &mut VmFiber,
        start: impl FnOnce(Completer<R>) -> Result<(), String>,
    ) -> Result<R, ExternalError> {
        let pending = self.core.sched.start_completion(start)?;
        let waited = self.wait_for_completion(fiber, pending.op, &pending.token)?;
        self.core.sched.collect(pending, waited)
    }

    /// Runs the cancel hooks of an operation whose waiter is giving up. A hook that panics must
    /// not leave the operation's entries behind: they are removed, and the panic is passed on
    /// (so it becomes a panic of the host function that is waiting).
    fn cancel_completion(&mut self, op: u64, token: &CancelToken) {
        if let Err(payload) =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| token.cancel()))
        {
            self.core.sched.ext().finish(op);
            std::panic::resume_unwind(payload);
        }
    }

    /// The part of a wait that depends on the engine and not on the type of the result: suspend
    /// the coroutine, or drive the event loop, until operation `op` is completed. `Blocked`
    /// means neither was possible, and the caller blocks on its receiver.
    pub fn wait_for_completion(
        &mut self,
        fiber: &mut VmFiber,
        op: u64,
        token: &CancelToken,
    ) -> Result<Waited, ExternalError> {
        let coro = fiber.state.coro;
        let waited = if let (Some(id), true) = (coro, self.can_suspend_for_external(fiber)) {
            let mut outcome = Ok(());
            while !self.core.sched.ext().is_completed(op) {
                if self.park_host(fiber, id, Wake::External(op)).is_err() {
                    self.cancel_completion(op, token);
                    outcome = Err(ExternalError::Cancelled);
                    break;
                }
            }
            outcome
        } else if self.can_drive(fiber, 1) {
            self.drive(None, false, &mut |me| me.core.sched.ext().is_completed(op));
            Ok(())
        } else {
            return Ok(Waited::Blocked);
        };
        self.core.sched.ext().finish(op);
        waited?;
        Ok(Waited::Finished)
    }

    /// Parks the coroutine's host function until `wake`; `Err` if the coroutine is cancelled
    /// (before or while parked).
    fn park_host(&mut self, fiber: &mut VmFiber, id: u64, wake: Wake) -> Result<(), ()> {
        if self
            .core
            .sched
            .prepare_suspend(id, &mut fiber.state, wake)
            .is_err()
        {
            return Err(());
        }
        if stacks::park(self, fiber) {
            fiber.state.cancelling = true;
            return Err(());
        }
        Ok(())
    }

    /// Resumes the coroutine of `fiber`, which parked in a host function: the stack goes on, and
    /// when the host function is over its call completes. Then the coroutine's code goes on as
    /// from any call (`Exit::Suspend` when the function parked again). `cancel`: the coroutine
    /// was cancelled while it was parked, so the function's wait ends with `Cancelled` and the
    /// function unwinds.
    pub(crate) fn continue_host(&mut self, fiber: &mut VmFiber, cancel: bool) -> Result<Exit, Ctl> {
        let Some(cont) = fiber.pending_host.take() else {
            unreachable!("continue_host is called for a fiber with a parked host call")
        };
        let HostCont {
            parked,
            id,
            body,
            type_args,
            span,
            dest,
        } = *cont;
        // The function reads the type arguments of its own call; whatever was set by the code
        // that runs now is put back when it parks or ends.
        let outer = std::mem::replace(&mut self.core.host.type_args, type_args);
        let step = stacks::resume(self, fiber, parked, cancel);
        let own = std::mem::replace(&mut self.core.host.type_args, outer);
        let result = match step {
            Step::Parked(parked) => {
                fiber.pending_host = Some(Box::new(HostCont::new(
                    parked, id, body, own, span, dest,
                )));
                return Ok(Exit::Suspend);
            }
            Step::Done(result) => result,
        };
        let outcome = self.finish_host_call(fiber, id, &body, &own, span, result);
        match (dest, outcome) {
            (HostDest::Entry, outcome) => outcome.map(Exit::Return),
            (HostDest::Reg(at), Ok(v)) => {
                Self::put_reg(fiber, at, v);
                self.exec(fiber)
            }
            // As the failure of any instruction: to the landing pad of the call.
            (HostDest::Reg(_), Err(ctl)) => match self.catch(fiber, ctl, false) {
                Ok(()) => self.exec(fiber),
                Err(ctl) => Err(ctl),
            },
        }
    }
}
