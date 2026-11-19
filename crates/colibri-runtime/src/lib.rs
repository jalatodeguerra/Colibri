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
//! # Host functions and state
//!
//! Scripts call Rust closures registered with [`Vm::register_fn`]. A closure may take a leading
//! `&mut` [`Ctx`] to reach host state ([`Vm::insert_state`]); a `Result<T, E: Display>` return
//! becomes a Colibri `Result[T]`, and a Rust panic becomes a Colibri panic:
//!
//! ```
//! use colibri_runtime as colibri;
//!
//! struct Score(i64);
//!
//! # fn main() -> Result<(), colibri::Error> {
//! let out = colibri::SharedBuffer::new();
//! let mut vm = colibri::Vm::builder().stdout(Box::new(out.clone())).build();
//! vm.insert_state(Score(0));
//! vm.register_fn("add_score", |cx: &mut colibri::Ctx, points: i64| {
//!     let score = cx.state_mut::<Score>().expect("the score was inserted");
//!     score.0 += points;
//!     score.0
//! })?;
//! vm.load_source("game.colibri", "fn main() {\n    add_score(3)\n    print(add_score(4))\n}\n")?;
//! vm.run_main()?;
//! assert_eq!(out.contents(), "7\n");
//! assert_eq!(vm.state_mut::<Score>().map(|s| s.0), Some(7));
//! # Ok(())
//! # }
//! ```
//!
//! What the host registered is described by an interface text (`.cbi`, [`Vm::interface_text`],
//! [`Vm::write_interface`], [`Vm::verify_interface`]) that `colibri check --interface` reads.
//! Items that reach outside the program are tagged with a [`Cap`] and callable only when the
//! host granted it ([`VmBuilder::allow`]); [`VmBuilder::instruction_budget`] bounds how long one
//! entry may run ([`Error::BudgetExhausted`]); [`Vm::find_fn`] gives a [`Callback`] that the
//! host can keep and call later.
//!
//! Each active Colibri call uses Rust stack, so run a `Vm` with a high
//! [`VmBuilder::max_call_depth`] on a thread with a large stack (the `colibri` tool does).

mod buffer;
mod callback;
mod cap;
mod convert;
mod ctx;
mod error;
mod func;
mod host;
mod iface;
mod render;
mod types;
mod vm;

pub use buffer::SharedBuffer;
pub use callback::Callback;
pub use cap::{Cap, CapSet};
pub use colibri_interp::DEFAULT_NATIVE_STACK_BUDGET;
pub use colibri_syntax::{Diagnostic, Severity, SourceMap, Span};
pub use convert::{Args, FromColibri, IntoColibri, Value};
pub use ctx::{Caller, Ctx, Cx};
pub use error::{Error, Location, Report, StackFrame};
pub use func::Fn;
pub use host::{HostFault, HostFn, Plain, RawHostFn, Registration, WithCtx};
pub use iface::Interface;
pub use render::render_diagnostic;
pub use types::{SigDesc, TypeDesc, TypeRegistry};
pub use vm::{DEFAULT_MAX_CALL_DEPTH, Vm, VmBuilder};

use std::path::Path;

/// What checking a source needs besides the source: the host interfaces it may use and the
/// capabilities the host grants (the defaults are no interface and no capability, which is
/// also what [`check_source`] uses).
#[derive(Clone, Debug)]
pub struct CheckConfig {
    /// How much native stack checking may use (see [`VmBuilder::native_stack_budget`]).
    pub stack_budget: usize,
    /// Interface (`.cbi`) files describing what the host provides, as `colibri check
    /// --interface engine.cbi` reads them.
    pub interfaces: Vec<Interface>,
    pub caps: CapSet,
}

impl Default for CheckConfig {
    fn default() -> Self {
        CheckConfig {
            stack_budget: DEFAULT_NATIVE_STACK_BUDGET,
            interfaces: Vec::new(),
            caps: CapSet::none(),
        }
    }
}

/// Parses and checks `source` without running it. Warnings and errors are in the report.
pub fn check_source(name: &str, source: &str) -> Report {
    check_source_with(name, source, &CheckConfig::default())
}

/// Like [`check_source`], with the native stack budget of [`VmBuilder::native_stack_budget`]:
/// how deeply nested an expression may be before it is rejected rather than overflowing the
/// Rust stack.
pub fn check_source_with_stack_budget(name: &str, source: &str, budget: usize) -> Report {
    let config = CheckConfig {
        stack_budget: budget,
        ..CheckConfig::default()
    };
    check_source_with(name, source, &config)
}

/// Like [`check_source`], against the host interfaces and capabilities of `config`.
pub fn check_source_with(name: &str, source: &str, config: &CheckConfig) -> Report {
    let mut sources = SourceMap::new();
    let file = sources.add_file(name, source);
    let (ast, mut diagnostics) = colibri_syntax::parse_file(file, source);
    if !diagnostics.iter().any(Diagnostic::is_error) {
        let mut env = colibri_check::HostEnv::new();
        for interface in &config.interfaces {
            env.merge(interface.env.clone());
        }
        for cap in config.caps.iter() {
            env.grant(cap.name());
        }
        diagnostics = colibri_check::check_with_host(&ast, &env, config.stack_budget).diagnostics;
    }
    Report {
        sources,
        diagnostics,
    }
}

/// Like [`check_source`], reading the source from a file.
pub fn check_file(path: impl AsRef<Path>) -> Result<Report, Error> {
    check_file_with(path, &CheckConfig::default())
}

/// Like [`check_source_with_stack_budget`], reading the source from a file.
pub fn check_file_with_stack_budget(
    path: impl AsRef<Path>,
    budget: usize,
) -> Result<Report, Error> {
    let config = CheckConfig {
        stack_budget: budget,
        ..CheckConfig::default()
    };
    check_file_with(path, &config)
}

/// Like [`check_source_with`], reading the source from a file.
pub fn check_file_with(path: impl AsRef<Path>, config: &CheckConfig) -> Result<Report, Error> {
    let path = path.as_ref();
    let text = std::fs::read_to_string(path).map_err(|e| Error::Io {
        path: path.display().to_string(),
        message: e.to_string(),
    })?;
    Ok(check_source_with(
        &path.display().to_string(),
        &text,
        config,
    ))
}
