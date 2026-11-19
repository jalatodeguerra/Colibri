//! The tree-walking interpreter: the reference engine for the checked HIR.
//!
//! A recursive evaluator over [`colibri_check::hir`] following ARCHITECTURE §2: all Colibri state
//! is in a heap [`Fiber`], control flow is one [`Ctl`] channel, every scope exit goes through
//! one `exit_scope`, and calls look functions up by id at call time. Hosts use it through
//! `colibri-runtime`, not directly.

mod ctl;
mod eval;
mod fiber;
mod format;
mod value;

pub use ctl::{Ctl, Eval, PanicInfo, TraceEntry};
pub use eval::{DEFAULT_NATIVE_STACK_BUDGET, Interp};
pub use fiber::Fiber;
pub use format::format_value;
pub use value::{Value, show, show_float};
