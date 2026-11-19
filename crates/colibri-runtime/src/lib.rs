//! Embedding Colibri in a Rust program.
//!
//! Hosts write `use colibri_runtime as colibri;`:
//!
//! ```
//! use colibri_runtime as colibri;
//!
//! # fn main() -> Result<(), colibri::Error> {
//! let out = colibri::SharedBuffer::new();
//! let mut vm = colibri::Vm::builder().stdout(Box::new(out.clone())).build();
//! vm.load_source("math.colibri", "fn add(a: int, b: int) -> int => a + b")?;
//! let add = vm.get_fn::<(i64, i64), i64>("add")?;
//! assert_eq!(add.call(&mut vm, (1, 2))?, 3);
//! # Ok(())
//! # }
//! ```
//!
//! A [`Vm`] runs one program. Functions are called through typed [`Fn`] handles whose Rust
//! signature is checked against the Colibri one when the handle is made. A Colibri panic comes
//! back as an [`Error::Panic`] and leaves the `Vm` usable. No interpreter type appears in this
//! API; values cross through [`IntoColibri`] and [`FromColibri`].
//!
//! Each active Colibri call uses Rust stack, so run a `Vm` with a high
//! [`VmBuilder::max_call_depth`] on a thread with a large stack (the `colibri` tool does).

mod buffer;
mod convert;
mod error;
mod func;
mod render;
mod vm;

pub use buffer::SharedBuffer;
pub use colibri_interp::DEFAULT_NATIVE_STACK_BUDGET;
pub use colibri_syntax::{Diagnostic, Severity, SourceMap, Span};
pub use convert::{Args, FromColibri, IntoColibri, Type, Value};
pub use error::{Error, Location, Report, StackFrame};
pub use func::Fn;
pub use render::render_diagnostic;
pub use vm::{DEFAULT_MAX_CALL_DEPTH, Vm, VmBuilder};

use std::path::Path;

/// Parses and checks `source` without running it. Warnings and errors are in the report.
pub fn check_source(name: &str, source: &str) -> Report {
    check_source_with_stack_budget(name, source, DEFAULT_NATIVE_STACK_BUDGET)
}

/// Like [`check_source`], with the native stack budget of [`VmBuilder::native_stack_budget`]:
/// how deeply nested an expression may be before it is rejected rather than overflowing the
/// Rust stack.
pub fn check_source_with_stack_budget(name: &str, source: &str, budget: usize) -> Report {
    let mut sources = SourceMap::new();
    let file = sources.add_file(name, source);
    let (ast, mut diagnostics) = colibri_syntax::parse_file(file, source);
    if !diagnostics.iter().any(Diagnostic::is_error) {
        diagnostics = colibri_check::check_with_stack_budget(&ast, budget).diagnostics;
    }
    Report {
        sources,
        diagnostics,
    }
}

/// Like [`check_source`], reading the source from a file.
pub fn check_file(path: impl AsRef<Path>) -> Result<Report, Error> {
    check_file_with_stack_budget(path, DEFAULT_NATIVE_STACK_BUDGET)
}

/// Like [`check_source_with_stack_budget`], reading the source from a file.
pub fn check_file_with_stack_budget(
    path: impl AsRef<Path>,
    budget: usize,
) -> Result<Report, Error> {
    let path = path.as_ref();
    let text = std::fs::read_to_string(path).map_err(|e| Error::Io {
        path: path.display().to_string(),
        message: e.to_string(),
    })?;
    Ok(check_source_with_stack_budget(
        &path.display().to_string(),
        &text,
        budget,
    ))
}
