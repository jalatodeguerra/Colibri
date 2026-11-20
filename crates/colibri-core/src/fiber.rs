//! The part of a running computation that the engine-neutral code reads and writes.

/// What the shared code needs to know about one running computation, wherever the engine keeps
/// the rest of it (the tree-walker: frames and temporaries in its own `Fiber`).
#[derive(Debug, Default, Clone)]
#[doc(hidden)]
pub struct FiberState {
    /// How many host functions are running, one inside another (they may call back into
    /// Colibri). A coroutine switch is not allowed across one (DESIGN §14.2).
    pub host_depth: u32,
    /// Set on the computation of a coroutine (ARCHITECTURE §16): which one.
    pub coro: Option<u64>,
    /// This coroutine is being cancelled: every further pause returns `Ctl::Cancel` at once.
    pub cancelling: bool,
}
