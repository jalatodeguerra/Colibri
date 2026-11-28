//! Rust bindings in the project runner (phase 4): the table a runner registers, and the workspace
//! `colibri` generates to build one.
//!
//! A binding is a crate with a `#[colibri::module]` function. A generated runner lists each one in
//! a [`Binding`] table and passes it to [`crate::runner_main`]; every `Vm` it makes (the main one
//! and the isolates of thread tasks) registers the table. Nothing here calls cargo.

pub mod build;
pub mod command;
pub mod engine;
pub mod r#gen;

use std::sync::OnceLock;

use colibri_runtime as colibri;

/// One binding crate: the module name (the `colibri.toml` key, what `import` says) and the
/// `__colibri_binding::register` function `#[colibri::module]` wrote.
#[derive(Clone, Copy)]
pub struct Binding {
    pub name: &'static str,
    pub register: fn(&mut colibri::Vm, &str) -> Result<(), colibri::Error>,
}

/// Registers every binding of `table` into `vm`, each under its own name.
pub fn register_all(vm: &mut colibri::Vm, table: &[Binding]) -> Result<(), colibri::Error> {
    for binding in table {
        (binding.register)(vm, binding.name)?;
    }
    Ok(())
}

static INSTALLED: OnceLock<&'static [Binding]> = OnceLock::new();

/// Sets the table the `Vm`s of this process register (`runner_main` does, once, before anything is
/// built); a plain `colibri` never does.
pub fn install(table: &'static [Binding]) {
    let _ = INSTALLED.set(table);
}

/// The table of this process: empty unless a runner installed one.
pub fn installed() -> &'static [Binding] {
    INSTALLED.get().copied().unwrap_or(&[])
}
