//! The tree-walking interpreter: the reference engine for the checked HIR.
//!
//! A recursive evaluator over [`colibri_check::hir`] following ARCHITECTURE §2: all Colibri state
//! is in a heap [`Fiber`], control flow is one [`Ctl`] channel, every scope exit goes through
//! one `exit_scope`, and calls look functions up by id at call time. Hosts use it through
//! `colibri-runtime`, not directly.

mod budget;
mod builtin;
mod cffi;
mod clone;
mod closure;
mod compare;
mod coro;
mod ctl;
mod dispatch;
mod eval;
mod fiber;
mod forloop;
mod format;
mod heap;
mod hooks;
mod host;
mod hostheap;
mod iter;
mod leaks;
mod listfn;
mod num;
mod ordered;
mod pattern;
mod place;
mod release;
mod results;
mod scope;
mod session;
mod show;
mod strfn;
mod value;
mod vecmath;
pub use vecmath::{FVec, IVec};

pub use budget::GRACE_STEPS;
pub use compare::{TooDeep, values_cmp, values_equal};
pub use coro::{Clock, ExternalError, SystemClock};
pub use ctl::{Ctl, Eval, PanicInfo, TraceEntry};
pub use eval::{DEFAULT_NATIVE_STACK_BUDGET, Interp};
pub use fiber::Fiber;
pub use format::format_value;
pub use heap::{ClassObj, GuardToken, ListObj, MapObj, SetObj};
pub use host::{HostFault, HostImpl, RootDropQueue, RootHandle, map_value};
pub use hostheap::{HostBorrowError, HostGuard, HostRef};
pub use leaks::Leak;
pub use session::SessionUndo;
pub use show::{show, show_float};
pub use value::{DynVal, EnumVal, RangeVal, StructVal, Value};
