//! Name resolution and type checking for Colibri.
//!
//! [`check`] takes a parsed file and produces a typed [`hir::Program`] (when there are no
//! errors) plus diagnostics. Checking has two phases: collect every module-level declaration
//! (so declarations may appear in any order), then check each body with bidirectional type
//! inference scoped to that body. The checker keeps going after an error, using
//! [`types::Ty::Error`] to avoid cascades, and never panics on any input.

mod adt;
mod attrs;
mod body;
mod builtins;
mod call;
mod checker;
mod closure;
mod coerce;
mod collections;
mod decls;
mod deprecated;
mod derive;
pub mod exhaust;
mod expr;
mod ffi;
mod fnvalue;
pub mod format;
mod generics;
pub mod hir;
pub mod host;
mod ifaces;
pub mod infer;
pub mod insight;
mod iter;
mod loops;
mod matching;
mod members;
mod methods;
mod modules;
mod numeric;
mod obligations;
mod operators;
mod option;
mod optres;
pub mod package;
mod pat;
mod place;
mod prelude;
mod range;
mod suggest;
pub mod traits;
mod tryop;
pub mod types;
mod unused;
mod vector;
mod zonk;

pub use checker::Checked;
pub use hir::Program;
pub use host::HostEnv;
pub use prelude::is_reserved_type_name;
pub use types::{FnSig, Ty};

/// How much native stack checking may use by default (see [`check_with_stack_budget`]).
pub const DEFAULT_STACK_BUDGET: usize = 1 << 20;

/// Checks a parsed file against no host: it provides nothing and grants nothing.
pub fn check(file: &colibri_syntax::ast::File) -> Checked {
    check_with_stack_budget(file, DEFAULT_STACK_BUDGET)
}

/// Checks a parsed file, reporting "nested too deeply" instead of overflowing the Rust stack
/// when expressions nest so deeply that checking them would use more than `budget` bytes of
/// stack. Raise it when running on a thread with a large stack.
pub fn check_with_stack_budget(file: &colibri_syntax::ast::File, budget: usize) -> Checked {
    check_with_host(file, &HostEnv::new(), budget)
}

/// Checks a parsed file against what the host provides (EMBEDDING §4): its functions and value
/// types are in scope, and the capabilities it granted decide what the file may use. The file
/// is the whole program: it has no siblings to `import` (a named host module still can be).
pub fn check_with_host(file: &colibri_syntax::ast::File, host: &HostEnv, budget: usize) -> Checked {
    let meta = package::ModuleMeta {
        name: String::new(),
        package: 0,
        entry: true,
        script: false,
        dependency: false,
        ffi: true,
        imports: std::collections::HashMap::new(),
        excluded: Vec::new(),
    };
    let packages = [package::PackageNode {
        name: String::new(),
    }];
    let inputs = [ModuleInput { file, meta: &meta }];
    check_modules(&inputs, &packages, host, budget)
}

/// One module of a program: its syntax tree and what the loader found out about it.
#[derive(Clone, Copy, Debug)]
pub struct ModuleInput<'a> {
    pub file: &'a colibri_syntax::ast::File,
    pub meta: &'a package::ModuleMeta,
}

/// Checks a whole program as one unit (ARCHITECTURE §11.3). `inputs` are in initialisation
/// order (an imported module before its importer) with the entry last, as
/// [`package::Loaded::inputs`] gives them; `packages` is what their metas index into.
pub fn check_modules(
    inputs: &[ModuleInput<'_>],
    packages: &[package::PackageNode],
    host: &HostEnv,
    budget: usize,
) -> Checked {
    checker::Checker::new(inputs, packages, host, budget).run()
}

/// How [`check_modules_with`] checks a program.
#[derive(Clone, Copy, Debug, Default)]
pub struct CheckOptions {
    /// Initialise the entry file's `let`/`var` in dependency order before its top-level
    /// statements, even when it has no `fn main`. A script normally runs top to bottom
    /// (DESIGN §4.2); `colibri repl` sets this because its session program is not one file
    /// read in order (it runs each entry in order itself).
    pub dependency_order: bool,
}

/// [`check_modules`] with [`CheckOptions`].
pub fn check_modules_with(
    inputs: &[ModuleInput<'_>],
    packages: &[package::PackageNode],
    host: &HostEnv,
    budget: usize,
    options: CheckOptions,
) -> Checked {
    checker::Checker::new(inputs, packages, host, budget)
        .dependency_order(options.dependency_order)
        .run()
}

/// Like [`check_modules`], and also records what every expression and name resolved to, for
/// hover and go to definition (see [`insight`]). Only this entry point pays for it. The
/// [`insight::Insight`] is built even when the program has errors.
pub fn check_modules_with_insight(
    inputs: &[ModuleInput<'_>],
    packages: &[package::PackageNode],
    host: &HostEnv,
    budget: usize,
) -> (Checked, insight::Insight) {
    let (checked, insight) = checker::Checker::new(inputs, packages, host, budget)
        .recording()
        .run_with_insight();
    (checked, insight.unwrap_or_default())
}
