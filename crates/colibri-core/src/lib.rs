//! The engine-neutral runtime pieces of Colibri: values, heap objects, the deep operations on
//! them, and the state an engine shares with its host.
//!
//! The tree-walking interpreter in `colibri-interp` is the only engine today; a bytecode VM is
//! meant to share this crate. The API is workspace-internal and unstable: hosts use
//! `colibri-runtime`, which re-exports what they need.

pub mod budget;
pub mod builders;
pub mod builtin;
pub mod bytesfn;
pub mod clock;
pub mod clone;
pub mod closure;
pub mod compare;
pub mod ctl;
pub mod deep;
pub mod dispatch;
pub mod engine;
pub mod error;
pub mod external;
pub mod fiber;
pub mod format;
pub mod heap;
pub mod hooks;
pub mod host;
pub mod hostheap;
pub mod iter;
pub mod leaks;
pub mod listfn;
pub mod num;
pub mod ordered;
pub mod protocol;
pub mod release;
pub mod results;
pub mod sched;
pub mod show;
pub mod state;
pub mod strfn;
pub mod value;

pub use state::Core;

/// The extension traits of the engine-neutral code, for `use colibri_core::prelude::*;` in an
/// engine. Their methods read as methods of the engine (`self.release(v)`).
#[doc(hidden)]
pub mod prelude {
    pub use crate::builtin::Builtins as _;
    pub use crate::bytesfn::BytesOps as _;
    pub use crate::deep::Deep as _;
    pub use crate::dispatch::Dispatch as _;
    pub use crate::engine::Engine as _;
    pub use crate::iter::IterOps as _;
    pub use crate::listfn::ListFnOps as _;
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
