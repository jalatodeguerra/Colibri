//! What the engine-neutral code needs from an engine.
//!
//! An engine (the tree-walking interpreter today, a bytecode VM later) owns a [`Core`] and a
//! type for one running computation (`Fiber`). Everything in this crate that has to run
//! Colibri code, build a panic with a stack, or reach a temporary of the running computation
//! asks the engine through this trait. The shared code is written as extension traits with
//! blanket impls over `E: Engine` (see [`crate::protocol`], [`crate::release`],
//! [`crate::deep`]), so a call site still reads `self.release(v)` or `self.panic(fiber, ..)`.
//!
//! The trait is generic, not `dyn`: built-ins are hot, an associated `Fiber` avoids downcasts,
//! and every instantiation is in the engine's crate, where it inlines.

use colibri_check::hir::{Builtin, FnId};
use colibri_check::types::TypeId;
use colibri_syntax::Span;

use crate::ctl::{Ctl, Eval, TraceEntry};
use crate::heap::ClassObj;
use crate::host::{HostFault, HostHook};
use crate::state::Core;
use crate::value::Value;

/// An engine: the owner of a [`Core`] and of the computations that run against it.
#[doc(hidden)]
pub trait Engine: Sized {
    /// The state of one running computation (tree-walker: its `Fiber`; VM: its frame stack).
    type Fiber;

    /// The state shared with the engine-neutral code.
    fn core(&self) -> &Core;
    fn core_mut(&mut self) -> &mut Core;

    // ---- positions and panics ----

    /// Records `span` as where the running call is: the position of its frame in the stack
    /// trace of a panic made now.
    fn note_position(fiber: &mut Self::Fiber, span: Span);

    /// The Colibri stack, innermost call first.
    fn stack_trace(&self, fiber: &Self::Fiber) -> Vec<TraceEntry>;

    /// Where the native stack stood when the host called in, to measure how deep the native
    /// recursion of a deep operation has gone.
    fn stack_base(fiber: &Self::Fiber) -> usize;

    /// How many bytes of the native stack a deep operation may use below [`Engine::stack_base`].
    fn native_stack_budget(&self) -> usize;

    // ---- calling Colibri code ----

    /// Calls the function `f`; the callee owns `args`. For a `var self` method the second
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

    /// Whether `f` is a `var self` method, which hands the final `self` back to its caller.
    /// `false` for an id the engine does not have.
    fn fn_mutates_self(&self, f: FnId) -> bool;

    /// Runs the `hook` of the host class `ty` on `args`. The class has that hook (the caller
    /// asked [`Core::host_hook_set`] first).
    fn call_host_hook(
        &mut self,
        fiber: &mut Self::Fiber,
        ty: TypeId,
        hook: HostHook,
        args: Vec<Value>,
    ) -> Result<Value, HostFault>;

    /// The control flow a failed host function or host hook ends the running operation with:
    /// a panic at the position of the innermost frame, or cancellation.
    ///
    /// Temporary: step 7 of the core split moves the cancellation flag into the shared fiber
    /// state, and this becomes ordinary shared code.
    fn host_fault_ctl(&mut self, fiber: &mut Self::Fiber, fault: HostFault) -> Ctl;

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

    // ---- temporary scaffolding ----

    /// The built-ins that may pause the running coroutine (`join`, `cancel`, `emit` and the
    /// like on a `Coroutine`, `CoGroup` or `Signal`); `recv` is the receiver.
    ///
    /// Temporary: step 8 of the core split moves the scheduler policy into core and this
    /// method is deleted.
    fn pausing_builtin(
        &mut self,
        fiber: &mut Self::Fiber,
        op: Builtin,
        recv: &Value,
        span: Span,
    ) -> Eval;

    /// A `CoGroup` object is dying: its coroutines are cancelled, newest first.
    ///
    /// Temporary: step 8 of the core split moves the scheduler policy into core and this
    /// method is deleted.
    fn group_died(&mut self, obj: &ClassObj);
}
