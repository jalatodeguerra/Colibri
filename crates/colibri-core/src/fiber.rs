//! The part of a running computation that the engine-neutral code reads and writes.

/// What the shared code needs to know about one running computation, wherever the engine keeps
/// the rest of it (the tree-walker: frames and temporaries in its own `Fiber`).
#[derive(Debug, Default, Clone)]
#[doc(hidden)]
pub struct FiberState {
    /// How many host functions are running, one inside another (they may call back into
    /// Colibri). A coroutine switch is not allowed across one (DESIGN §14.2).
    pub host_depth: u32,
    /// How many operations of the runtime that call user code are active on this computation: a
    /// `Show`, `Eq`, `Ord`, `Hash` or `Clone` impl run for `print`, `==`, a sort or a map lookup,
    /// an iterator's `next` run by a built-in, the `message` of an error, a host class's hook.
    /// Native code is on the stack there, so a coroutine cannot pause inside (DESIGN §14.2).
    /// A call the program makes itself (`x.show()`) and the callbacks of the closure built-ins
    /// do not count.
    pub native_depth: u32,
    /// Set on the computation of a coroutine (ARCHITECTURE §16): which one.
    pub coro: Option<u64>,
    /// This coroutine is being cancelled: every further pause returns `Ctl::Cancel` at once.
    pub cancelling: bool,
}
