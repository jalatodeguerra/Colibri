//! The tree-walking interpreter: the reference engine for the checked HIR.
//!
//! A recursive evaluator over [`colibri_check::hir`] following ARCHITECTURE §2: all Colibri state
//! is in a heap [`Fiber`], control flow is one [`Ctl`] channel, every scope exit goes through
//! one `exit_scope`, and calls look functions up by id at call time. Hosts use it through
//! `colibri-runtime`, not directly.

mod budget;
mod cffi;
mod closure;
mod coro;
mod dispatch;
mod engine;
mod eval;
mod fiber;
mod forloop;
mod host;
mod hostheap;
mod leaks;
mod pattern;
mod place;
mod release;
mod scope;
mod session;

pub(crate) use colibri_core::{compare, ctl, format, heap, num, ordered, value};

pub use eval::{DEFAULT_NATIVE_STACK_BUDGET, Interp};
pub use fiber::Fiber;
pub use host::{HostClassHooks, HostImpl};
pub use session::SessionUndo;

// What the interpreter's own tests name; the engine-neutral types are `colibri_core`'s.
#[doc(hidden)]
pub use colibri_core::ctl::PanicInfo;
#[doc(hidden)]
pub use colibri_core::hostheap::HostBorrowError;
#[doc(hidden)]
pub use colibri_core::value::Value;
