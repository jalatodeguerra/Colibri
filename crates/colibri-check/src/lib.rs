//! Name resolution and type checking for Colibri.
//!
//! [`check`] takes a parsed file and produces a typed [`hir::Program`] (when there are no
//! errors) plus diagnostics. Checking has two phases: collect every module-level declaration
//! (so declarations may appear in any order), then check each body with bidirectional type
//! inference scoped to that body. The checker keeps going after an error, using
//! [`types::Ty::Error`] to avoid cascades, and never panics on any input.

mod adt;
mod body;
mod builtins;
mod call;
mod checker;
mod closure;
mod coerce;
mod collections;
mod decls;
mod derive;
pub mod exhaust;
mod expr;
mod fnvalue;
pub mod format;
mod generics;
pub mod hir;
mod ifaces;
pub mod infer;
mod iter;
mod loops;
mod matching;
mod methods;
mod numeric;
mod obligations;
mod option;
mod optres;
mod pat;
mod place;
mod prelude;
mod range;
mod suggest;
pub mod traits;
mod tryop;
pub mod types;
mod zonk;

pub use checker::Checked;
pub use hir::Program;
pub use types::{FnSig, Ty};

/// How much native stack checking may use by default (see [`check_with_stack_budget`]).
pub const DEFAULT_STACK_BUDGET: usize = 1 << 20;

/// Checks a parsed file.
pub fn check(file: &colibri_syntax::ast::File) -> Checked {
    check_with_stack_budget(file, DEFAULT_STACK_BUDGET)
}

/// Checks a parsed file, reporting "nested too deeply" instead of overflowing the Rust stack
/// when expressions nest so deeply that checking them would use more than `budget` bytes of
/// stack. Raise it when running on a thread with a large stack.
pub fn check_with_stack_budget(file: &colibri_syntax::ast::File, budget: usize) -> Checked {
    checker::Checker::new(file, budget).run()
}
