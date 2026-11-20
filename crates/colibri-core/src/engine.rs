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

use std::rc::Rc;

use colibri_check::hir::{Body, FnId};
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

    /// Calls the function `f`; the callee owns `args`.
    fn call_fn(&mut self, fiber: &mut Self::Fiber, f: FnId, args: Vec<Value>) -> Eval;

    /// Runs `f` on a fresh computation that belongs to no fiber: a `Drop` hook, which can run
    /// anywhere a value dies.
    fn call_detached(&mut self, f: FnId, args: Vec<Value>) -> Eval;

    /// The checked body of `f`, or `None` for an id the engine does not have.
    fn fn_body(&self, f: FnId) -> Option<&Rc<Body>>;

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

    // ---- temporary scaffolding ----

    /// A `CoGroup` object is dying: its coroutines are cancelled, newest first.
    ///
    /// Temporary: step 8 of the core split moves the scheduler policy into core and this
    /// method is deleted.
    fn group_died(&mut self, obj: &ClassObj);
}
