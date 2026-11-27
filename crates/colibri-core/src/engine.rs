//! What the engine-neutral code needs from an engine.
//!
//! An engine (the bytecode VM) owns a [`Core`] and a
//! type for one running computation (`Fiber`). Everything in this crate that has to run
//! Colibri code, build a panic with a stack, or reach a temporary of the running computation
//! asks the engine through this trait. The shared code is written as extension traits with
//! blanket impls over `E: Engine` (see `protocol`, [`crate::release`],
//! `deep`), so a call site still reads `self.release(v)` or `self.panic(fiber, ..)`.
//!
//! The trait is generic, not `dyn`: built-ins are hot, an associated `Fiber` avoids downcasts,
//! and the instantiations are in the engine's crate (and in the runtime, for the few
//! shared methods it calls), where they inline.

use colibri_check::hir::FnId;
use colibri_check::types::TypeId;
use colibri_syntax::Span;

use crate::ctl::{Eval, TraceEntry};
use crate::fiber::FiberState;
use crate::host::{HostFault, HostHook};
use crate::sched::{EngineSlot, Finish};
use crate::state::Core;
use crate::value::Value;

/// How a resumed coroutine stopped.
#[doc(hidden)]
pub enum Resumed {
    /// It paused (it waits for something).
    Suspended,
    /// It ran to its end.
    Finished(Finish),
    /// The engine has no stack for it.
    Gone,
}

/// An engine: the owner of a [`Core`] and of the computations that run against it.
#[doc(hidden)]
pub trait Engine: Sized {
    /// The state of one running computation (the VM: its frame stack).
    type Fiber;

    /// The state shared with the engine-neutral code.
    fn core(&self) -> &Core;
    fn core_mut(&mut self) -> &mut Core;

    /// What the shared code reads and writes of a running computation.
    fn fiber_state(fiber: &Self::Fiber) -> &FiberState;
    fn fiber_state_mut(fiber: &mut Self::Fiber) -> &mut FiberState;

    // ---- positions and panics ----

    /// Records `span` as where the running call is: the position of its frame in the stack
    /// trace of a panic made now.
    fn note_position(fiber: &mut Self::Fiber, span: Span);

    /// Where the running call is, as last recorded by [`Engine::note_position`] or by the call
    /// itself; `None` when no call is active.
    fn position(fiber: &Self::Fiber) -> Option<Span>;

    /// `span` as a panic reports it. A position inside code of the prelude, which a program
    /// never sees (a closure built-in written in Colibri), is where the innermost call of the
    /// program's own code stands: the call of the built-in. The default has no such code.
    #[inline]
    fn visible_span(_fiber: &Self::Fiber, span: Span) -> Span {
        span
    }

    /// The Colibri stack, innermost call first.
    fn stack_trace(&self, fiber: &Self::Fiber) -> Vec<TraceEntry>;

    /// Where the native stack stood when the host called in, to measure how deep the native
    /// recursion of a deep operation has gone.
    fn stack_base(fiber: &Self::Fiber) -> usize;

    /// How many bytes of the native stack a deep operation may use below [`Engine::stack_base`].
    fn native_stack_budget(&self) -> usize;

    /// Runs the consumers of safepoints that are due (`Core::take_due_safepoints`), in their
    /// fixed order. `Err` ends the running operation (the memory limit); the consumers that are
    /// due still get their sample. A consumer only looks: it must not change the frames or
    /// their pcs, nor run program code, because the loop that called goes on with the state it
    /// has cached. The default has none.
    fn safepoint(&mut self, _fiber: &mut Self::Fiber) -> crate::ctl::Eval<()> {
        self.core_mut().take_due_safepoints();
        Ok(())
    }

    // ---- calling Colibri code ----

    /// Calls the function `f`; the callee owns `args`. For a `mut self` method the second
    /// result is the final value of `self`, taken out of the frame after every other cleanup
    /// ran, for the caller to write back; it is present on every path, including a panic.
    fn call_fn_inout(
        &mut self,
        fiber: &mut Self::Fiber,
        f: FnId,
        args: Vec<Value>,
    ) -> (Eval, Option<Value>);

    /// Calls the function `f`; the callee owns `args`.
    #[inline]
    fn call_fn(&mut self, fiber: &mut Self::Fiber, f: FnId, args: Vec<Value>) -> Eval {
        self.call_fn_inout(fiber, f, args).0
    }

    /// Calls the function value `f` (a function, a closure or a bound method) with `args`,
    /// which the callee owns.
    fn call_value(
        &mut self,
        fiber: &mut Self::Fiber,
        f: &Value,
        args: Vec<Value>,
        span: Span,
    ) -> Eval;

    /// [`Engine::call_value`] from a call at `span`: the position of the running call is
    /// `span` while the callee runs, so a panic in it shows where it was called from.
    #[inline]
    fn call_value_at(
        &mut self,
        fiber: &mut Self::Fiber,
        f: &Value,
        args: Vec<Value>,
        span: Span,
    ) -> Eval {
        Self::note_position(fiber, span);
        self.call_value(fiber, f, args, span)
    }

    /// Runs `f` on a fresh computation that belongs to no fiber: a `Drop` hook, which can run
    /// anywhere a value dies.
    fn call_detached(&mut self, f: FnId, args: Vec<Value>) -> Eval;

    /// Where `f` is declared, or `None` for an id the engine does not have. Callers that must
    /// know whether an id exists ask this; the engine's own `call_fn` reports an unknown id
    /// as an internal error at its own position.
    fn fn_span(&self, f: FnId) -> Option<Span>;

    /// Whether `f` is a `mut self` method, which hands the final `self` back to its caller.
    /// `false` for an id the engine does not have.
    fn fn_mutates_self(&self, f: FnId) -> bool;

    /// Runs the `hook` of the host class `ty` on `args`. The class has that hook (the caller
    /// asked `Core::host_hook_set` first).
    fn call_host_hook(
        &mut self,
        fiber: &mut Self::Fiber,
        ty: TypeId,
        hook: HostHook,
        args: Vec<Value>,
    ) -> Result<Value, HostFault>;

    // ---- temporaries ----

    /// Where the temporaries of the running computation stand now, to be passed to
    /// [`Engine::release_temps`] later.
    fn temp_mark(fiber: &Self::Fiber) -> usize;

    /// Keeps `v` alive until [`Engine::release_temps`] reaches below it: a value a Rust local
    /// holds across a call that can run user code has such a twin, so an early exit never
    /// loses the last reference to it.
    fn hold_temp(fiber: &mut Self::Fiber, v: Value);

    /// Releases the temporaries above `mark`, newest first.
    fn release_temps(&mut self, fiber: &mut Self::Fiber, mark: usize);

    // ---- coroutines ----

    /// Runs the coroutine `id`, which the scheduler has marked running, until it pauses or
    /// ends. `cancel` resumes it with a cancel signal, so its pause returns `Ctl::Cancel` and it
    /// unwinds. The scheduler's bookkeeping (state, signals, budget) is done by the caller.
    ///
    /// An engine with heap frames runs the coroutine's
    /// frames from where it paused until it waits again or ends. The engine must not hold a
    /// reference into [`Core`] across the call (the coroutine uses it meanwhile). `slot` is what
    /// the engine gave to `Scheduler::register` for this coroutine. `Gone` means the engine has
    /// nothing to run for `id` (it is already running, or has ended). The scheduler only resumes
    /// suspended coroutines, so this is a broken invariant: the coroutine ends as a failure with
    /// an internal error.
    fn resume_stack(&mut self, id: u64, slot: EngineSlot, cancel: bool) -> Resumed;

    /// `t.done()` and `t.try_join()` of a thread task. The result of a task is rebuilt on a walk
    /// stack, which only an engine can map, so an engine that runs thread tasks does this; one
    /// that does not has no `Task` to ask.
    fn task_builtin(
        &mut self,
        fiber: &mut Self::Fiber,
        op: colibri_check::hir::Builtin,
        recv: &Value,
        span: Span,
    ) -> Eval;

    /// `try_recv`, `close` and `iter` of a `Channel` (`args[0]` is the channel). A value is copied
    /// to and from its portable form on a walk stack, which only an engine can map. `send` and
    /// `recv` wait, so they are instructions of the engine's own, and reach here only when the
    /// caller is not one (they block).
    fn channel_builtin(
        &mut self,
        fiber: &mut Self::Fiber,
        op: colibri_check::hir::Builtin,
        args: &mut [Value],
        span: Span,
    ) -> Eval;

    /// `get` of a `Shared` (`recv` is the handle): the frozen copy this isolate keeps, made on
    /// the first call. Rebuilding it needs a walk stack, which only an engine can map.
    fn shared_get(&mut self, fiber: &mut Self::Fiber, recv: &Value, span: Span) -> Eval;

    /// The next value of a channel's iterator (`for x in ch` outside the instruction that waits
    /// properly, `ch.iter().enumerate()`): `None` when the channel is closed and empty. It waits
    /// by blocking this thread where it cannot pause.
    fn channel_next(
        &mut self,
        fiber: &mut Self::Fiber,
        channel: &Value,
        span: Span,
    ) -> Eval<Option<Value>>;
}
