//! The engine-neutral runtime pieces of Colibri: values, heap objects, the deep operations on
//! them, and the state an engine shares with its host.
//!
//! The register bytecode VM in `colibri-vm` is the engine that builds on this crate. The API is
//! workspace-internal and unstable: hosts use `colibri-runtime`, which re-exports what they need.

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
pub mod mem;
pub mod num;
pub mod ops;
pub mod ordered;
pub mod pattern;
pub mod portable;
pub(crate) mod protocol;
pub mod release;
pub(crate) mod results;
pub mod safepoint;
pub mod sched;
pub(crate) mod show;
pub(crate) mod state;
pub(crate) mod strfn;
pub mod value;

#[doc(hidden)]
pub use builtin::{THREADS_NOT_BUILT, work_of};
#[doc(hidden)]
pub use iter::item_of;
pub use state::Core;

/// The extension traits of the engine-neutral code, for `use colibri_core::prelude::*;` in an
/// engine. Their methods read as methods of the engine (`self.release(v)`).
#[doc(hidden)]
pub mod prelude {
    pub use crate::builtin::Builtins;
    pub use crate::bytesfn::BytesOps;
    pub use crate::coro::Sched;
    pub use crate::deep::Deep;
    pub use crate::dispatch::Dispatch;
    pub use crate::engine::Engine;
    pub use crate::iter::IterOps;
    pub use crate::listfn::ListFnOps;
    pub use crate::ops::Operators;
    pub use crate::protocol::Protocol;
    pub use crate::release::Release;
    pub use crate::results::ResultOps;
    pub use crate::strfn::StrOps;
}

/// The current position on the native stack (it only has to be comparable with another one).
#[doc(hidden)]
#[inline]
pub fn stack_position() -> usize {
    let marker = 0u8;
    std::hint::black_box(&marker) as *const u8 as usize
}
