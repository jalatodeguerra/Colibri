//! The engine-neutral runtime pieces of Colibri: values, heap objects, the deep operations on
//! them, and the state an engine shares with its host.
//!
//! The tree-walking interpreter in `colibri-interp` is the only engine today; a bytecode VM is
//! meant to share this crate. The API is workspace-internal and unstable: hosts use
//! `colibri-runtime`, which re-exports what they need.

pub mod budget;
pub(crate) mod builders;
pub(crate) mod builtin;
pub(crate) mod bytesfn;
pub mod clock;
pub(crate) mod clone;
pub mod closure;
pub mod compare;
pub mod coro;
pub mod ctl;
pub(crate) mod deep;
pub mod dispatch;
pub mod engine;
pub(crate) mod error;
pub mod external;
pub mod fiber;
pub mod format;
pub mod heap;
pub(crate) mod hooks;
pub mod host;
pub mod hostheap;
pub(crate) mod iter;
pub mod leaks;
pub(crate) mod listfn;
pub mod num;
pub mod ops;
pub mod ordered;
pub(crate) mod protocol;
pub mod release;
pub(crate) mod results;
pub mod sched;
pub(crate) mod show;
pub(crate) mod state;
pub(crate) mod strfn;
pub mod value;

pub use state::Core;

/// The extension traits of the engine-neutral code, for `use colibri_core::prelude::*;` in an
/// engine. Their methods read as methods of the engine (`self.release(v)`).
#[doc(hidden)]
pub mod prelude {
    pub use crate::builtin::Builtins as _;
    pub use crate::bytesfn::BytesOps as _;
    pub use crate::coro::Sched as _;
    pub use crate::deep::Deep as _;
    pub use crate::dispatch::Dispatch as _;
    pub use crate::engine::Engine as _;
    pub use crate::iter::IterOps as _;
    pub use crate::listfn::ListFnOps as _;
    pub use crate::ops::Operators as _;
    pub use crate::protocol::Protocol as _;
    pub use crate::release::Release as _;
    pub use crate::results::ResultOps as _;
    pub use crate::strfn::StrOps as _;
}

/// The current position on the native stack (it only has to be comparable with another one).
#[doc(hidden)]
#[inline]
pub fn stack_position() -> usize {
    let marker = 0u8;
    std::hint::black_box(&marker) as *const u8 as usize
}
