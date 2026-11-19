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

// The code `#[colibri::export]` writes names the crate `colibri_runtime`; this makes that name
// work inside the crate too (its own tests and docs).
extern crate self as colibri_runtime;

mod buffer;
mod callback;
mod cap;
mod class;
mod convert;
mod ctx;
mod error;
mod func;
mod handle;
mod host;
mod iface;
mod render;
mod types;
mod vm;

pub use buffer::SharedBuffer;
pub use callback::Callback;
pub use cap::{Cap, CapSet};
pub use class::{HostMethod, HostMethodMut, HostType, TypeBuilder};
/// Exports a Rust struct, enum, `impl` block or function to Colibri (EMBEDDING §2).
///
/// * `#[colibri::export(value)]` on a `Copy` struct with `pub` fields, or on an enum without
///   payloads: a Colibri `struct` or `enum`, converted by value.
/// * `#[colibri::export]` on a struct: an opaque host class ([`HostType`]).
/// * `#[colibri::export] impl Name { .. }`: its `pub fn`s become the class's methods (`&self`,
///   `&mut self`) and static functions, under their Rust names and parameter names.
/// * `#[colibri::export] fn name(..)`: register it with `name::register(&mut vm)`.
///
/// ```
/// use colibri_runtime as colibri;
///
/// #[colibri::export(value)]
/// #[derive(Clone, Copy, Debug, PartialEq)]
/// pub struct Vec2 {
///     pub x: f32,
///     pub y: f32,
/// }
///
/// #[colibri::export]
/// pub struct Ship {
///     pos: Vec2,
/// }
///
/// #[colibri::export]
/// impl Ship {
///     pub fn new(pos: Vec2) -> Ship {
///         Ship { pos }
///     }
///     pub fn push(&mut self, dx: f32, dy: f32) {
///         self.pos = Vec2 { x: self.pos.x + dx, y: self.pos.y + dy };
///     }
///     pub fn pos(&self) -> Vec2 {
///         self.pos
///     }
/// }
///
/// #[colibri::export]
/// pub fn double(n: i64) -> i64 {
///     n * 2
/// }
///
/// # fn main() -> Result<(), colibri::Error> {
/// let out = colibri::SharedBuffer::new();
/// let mut vm = colibri::Vm::builder().stdout(Box::new(out.clone())).build();
/// vm.register_type::<Ship>()?;
/// double::register(&mut vm)?;
/// vm.load_source(
///     "a.colibri",
///     "fn main() {\n    let s = Ship.new(Vec2 { x: 1.0, y: 2.0 })\n    s.push(dx: 0.5, dy: 0.5)\n    print(s.pos())\n    print(double(21))\n}\n",
/// )?;
/// vm.run_main()?;
/// assert_eq!(out.contents(), "Vec2 { x: 1.5, y: 2.5 }\n42\n");
/// # Ok(())
/// # }
/// ```
///
/// A `value` struct must be `Copy` (the macro asserts it) and every field `pub`:
///
/// ```compile_fail
/// use colibri_runtime as colibri;
///
/// #[colibri::export(value)]
/// pub struct NotCopy {
///     pub name: String,
/// }
/// ```
///
/// ```compile_fail
/// use colibri_runtime as colibri;
///
/// #[colibri::export(value)]
/// #[derive(Clone, Copy)]
/// pub struct Hidden {
///     pub x: f32,
///     y: f32,
/// }
/// ```
pub use colibri_bind::export;
pub use colibri_interp::DEFAULT_NATIVE_STACK_BUDGET;
pub use colibri_syntax::{Diagnostic, Severity, SourceMap, Span};
pub use convert::{Args, FromColibri, IntoColibri, Value, ValueKind};
pub use ctx::{Caller, Ctx, Cx};
pub use error::{Error, Location, Report, StackFrame};
pub use func::Fn;
pub use handle::{Handle, Ref, RefMut};
pub use host::{HostFault, HostFn, Plain, RawHostFn, Registration, WithCtx};
pub use iface::Interface;
pub use render::render_diagnostic;
pub use types::{SigDesc, TypeDesc, TypeRegistry};
pub use vm::{DEFAULT_MAX_CALL_DEPTH, HostModule, TestInfo, Vm, VmBuilder};

/// The type descriptors of the loaded program, for host functions that work on any type
/// (serializers, DESIGN §7.5): what [`Ctx::type_args`] and [`Ctx::type_defs`] return.
pub mod reflect {
    pub use colibri_check::hir::{FieldDef, TypeDef, TypeDefKind, VariantDef};
    pub use colibri_check::types::{
        ERROR_TYPE, IntTy, OPTION_TYPE, RESULT_TYPE, Ty, TypeId, VARIANT_NONE, VARIANT_SOME,
    };
}

use std::path::Path;

use colibri_check::package::{DiskFs, Entry, MemoryFs, SourceFs};

/// What `#[colibri::export]` expands to. Not part of the API: the names may change with the
/// VM, which is why the macro only goes through this module (EMBEDDING §0 C).
#[doc(hidden)]
pub mod __private {
    pub use crate::class::{ExportedMethods, HostType, TypeBuilder};
    pub use crate::convert::{FromColibri, IntoColibri, Value};
    pub use crate::ctx::Cx;
    pub use crate::error::Error;
    pub use crate::types::{TypeDesc, TypeRegistry};

    use crate::{HostFn, Registration, Vm};

    /// The `i`th field of a struct value, or the error that says what was wrong.
    pub fn field(value: &Value, i: usize, type_name: &str) -> Result<Value, Error> {
        value.field(i).ok_or_else(|| {
            Error::Convert(format!(
                "expected a `{type_name}` struct with a field at position {i}"
            ))
        })
    }

    /// Registers an exported function with its parameter names.
    pub fn register_exported<'a, M, F: HostFn<M>, const N: usize>(
        vm: &'a mut Vm,
        name: &str,
        params: [&str; N],
        f: F,
    ) -> Result<Registration<'a>, Error> {
        vm.register_fn_named(name, params, f)
    }
}

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
    check_sources_with(&[(name, source)], config)
}

/// Checks a program given as text without running it: `(path, text)` pairs as in
/// [`Vm::load_sources`] (the first is the entry, and its folder the root of `import`s).
pub fn check_sources_with(files: &[(&str, &str)], config: &CheckConfig) -> Report {
    let Some((entry, _)) = files.first() else {
        return Report {
            sources: SourceMap::new(),
            diagnostics: Vec::new(),
        };
    };
    let mut fs = MemoryFs::new();
    for (path, text) in files {
        fs.insert(path, *text);
    }
    check_entry(&fs, Entry::File(Path::new(entry)), config).unwrap_or_else(|e| {
        // Unreachable for files in memory except an unusable entry name; say so in a report.
        let mut sources = SourceMap::new();
        let file = sources.add_file(entry.to_string(), String::new());
        Report {
            sources,
            diagnostics: vec![Diagnostic::error(e.to_string(), Span::new(file, 0, 0))],
        }
    })
}

fn check_entry(fs: &dyn SourceFs, entry: Entry<'_>, config: &CheckConfig) -> Result<Report, Error> {
    let mut env = colibri_check::HostEnv::new();
    for interface in &config.interfaces {
        env.merge(interface.env.clone());
    }
    for cap in config.caps.iter() {
        env.grant(cap.name());
    }
    let mut sources = SourceMap::new();
    let loaded = colibri_check::package::load(fs, &mut sources, entry, &env.named_modules())
        .map_err(|e| Error::Io {
            path: e.path,
            message: e.message,
        })?;
    let mut diagnostics = loaded.diagnostics.clone();
    // A tree with syntax errors would only produce follow-on errors.
    if !loaded.has_errors() {
        diagnostics = colibri_check::check_modules(
            &loaded.inputs(),
            &loaded.packages,
            &env,
            config.stack_budget,
        )
        .diagnostics;
    }
    Ok(Report {
        sources,
        diagnostics,
    })
}

/// Like [`check_source`], reading the source from a file. The file's folder is its implicit
/// package: what it `import`s is read from there and checked with it (see [`Vm::load_file`]).
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
    check_entry(&DiskFs, Entry::File(path.as_ref()), config)
}

/// Checks the package in folder `dir` (its `colibri.toml` and everything its `main.colibri`
/// imports) without running it; see [`Vm::load_package`].
pub fn check_package_with(dir: impl AsRef<Path>, config: &CheckConfig) -> Result<Report, Error> {
    check_entry(&DiskFs, Entry::Package(dir.as_ref()), config)
}

/// [`check_package_with`] for a folder, [`check_file_with`] for anything else: what `colibri
/// check <path>` does.
pub fn check_path_with(path: impl AsRef<Path>, config: &CheckConfig) -> Result<Report, Error> {
    let path = path.as_ref();
    if path.is_dir() {
        check_package_with(path, config)
    } else {
        check_file_with(path, config)
    }
}
