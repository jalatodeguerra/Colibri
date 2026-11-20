//! The engine-neutral runtime pieces of Colibri: values, heap objects, the deep operations on
//! them, and the state an engine shares with its host.
//!
//! The tree-walking interpreter in `colibri-interp` is the only engine today; a bytecode VM is
//! meant to share this crate. The API is workspace-internal and unstable: hosts use
//! `colibri-runtime`, which re-exports what they need.

pub mod budget;
pub mod builders;
pub mod clone;
pub mod closure;
pub mod compare;
pub mod ctl;
pub mod dispatch;
pub mod error;
pub mod format;
pub mod heap;
pub mod hooks;
pub mod host;
pub mod hostheap;
pub mod iter;
pub mod leaks;
pub mod num;
pub mod ordered;
pub mod show;
pub mod state;
pub mod value;

pub use state::Core;

/// The current position on the native stack (it only has to be comparable with another one).
#[doc(hidden)]
#[inline]
pub fn stack_position() -> usize {
    let marker = 0u8;
    std::hint::black_box(&marker) as *const u8 as usize
}
