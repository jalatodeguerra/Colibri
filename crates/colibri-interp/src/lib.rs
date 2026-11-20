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

pub(crate) use colibri_core::{compare, ctl, format, heap, num, ordered, show, value};

pub use budget::GRACE_STEPS;
pub use colibri_core::clock::{Clock, SystemClock};
pub use colibri_core::external::{CancelToken, Completer, ExternalError};
pub use colibri_core::leaks::Leak;
pub use compare::{TooDeep, values_cmp, values_equal};
pub use ctl::{Ctl, Eval, PanicInfo, TraceEntry};
pub use eval::{DEFAULT_NATIVE_STACK_BUDGET, Interp};
pub use fiber::Fiber;
pub use format::format_value;
pub use heap::{ClassObj, GuardToken, ListObj, MapObj, SetObj};
pub use host::{HostClassHooks, HostFault, HostImpl, RootDropQueue, RootHandle, map_value};
pub use hostheap::{HostBorrowError, HostGuard, HostRef};
pub use session::SessionUndo;
pub use show::{show, show_float};
pub use value::{DynVal, EnumVal, RangeVal, StructVal, Value};
