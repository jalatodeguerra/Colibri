//! All state of one running Colibri computation lives here, on the heap (ARCHITECTURE §2): the
//! frames and the temporaries stack. Rust locals never own a droppable Colibri value across a
//! nested evaluation, so a coroutine is a second `Fiber` plus a stackful context (`coro.rs`).

use std::rc::Rc;

use colibri_check::hir::{Body, LocalId, Stmt};
use colibri_syntax::Span;

use colibri_core::fiber::FiberState;

use crate::value::Value;

/// One call: the body being run, its local slots, and the source position it is at (updated at
/// calls and panics, for stack traces).
#[derive(Debug)]
pub(crate) struct Frame {
    pub body: Rc<Body>,
    pub slots: Vec<Value>,
    pub span: Span,
    /// What must happen when scopes exit, innermost last (ARCHITECTURE §6): bindings to
    /// release and `defer` statements to run, in one last-in first-out list.
    pub cleanups: Vec<Cleanup>,
}

/// One entry of a frame's cleanup list.
#[derive(Debug)]
pub(crate) enum Cleanup {
    /// The binding in this slot dies.
    Slot(LocalId),
    /// A deferred statement runs.
    Defer(Rc<Stmt>),
}

/// The stack of one computation.
#[derive(Debug, Default)]
pub struct Fiber {
    pub(crate) frames: Vec<Frame>,
    /// Operands and other temporaries of the statements being evaluated.
    pub(crate) temps: Vec<Value>,
    /// The address of a local at the point the host called in, to measure native stack use.
    pub(crate) stack_base: usize,
    /// What the engine-neutral code reads of the computation: the host depth, which coroutine
    /// it is, whether it is being cancelled.
    pub(crate) state: FiberState,
    /// How to suspend this coroutine's native stack (ARCHITECTURE §16). Set on the fiber of a
    /// coroutine only; it points into the stack of the coroutine body that owns this fiber.
    pub(crate) yielder: Option<*const crate::coro::Y>,
}

impl Fiber {
    pub fn new() -> Self {
        Self::default()
    }

    /// How many calls are active.
    pub fn depth(&self) -> usize {
        self.frames.len()
    }

    /// How many host functions are active on this fiber.
    pub fn host_depth(&self) -> u32 {
        self.state.host_depth
    }

    /// How many temporaries are alive; zero whenever no code is running.
    pub fn pending_temps(&self) -> usize {
        self.temps.len()
    }
}
