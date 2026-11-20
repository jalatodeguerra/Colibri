//! `colibri check`: type-check a script or package without running it.

use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use colibri_runtime as colibri;

use crate::depscmd::{self, DepFlags};

/// Checks `file`, printing every diagnostic to stderr. `Err` is the exit code to stop with.
/// With `all_modules`, a package's modules that its entry does not import are checked too
/// (`colibri check`).
pub fn check(
    file: &Path,
    interface_files: &[PathBuf],
    all_modules: bool,
    deps: DepFlags,
) -> Result<(), ExitCode> {
    check_with(
        file,
        interface_files,
        all_modules,
        deps,
        &mut colibri_std::interface,
    )
}

/// [`check`] with the source of the standard library's interfaces: it is asked for the
/// `std.*` modules the program imports, and only those.
pub fn check_with(
    file: &Path,
    interface_files: &[PathBuf],
    all_modules: bool,
    deps: DepFlags,
    std_interface: &mut dyn FnMut(&str) -> Option<colibri::Interface>,
) -> Result<(), ExitCode> {
    // Git dependencies are fetched and locked here, and nowhere else (the loader only reads).
    depscmd::prepare(file, deps)?;
    let color = io::stderr().is_terminal();
    let mut interfaces = Vec::new();
    for path in interface_files {
        match colibri::Interface::read(path) {
            Ok(interface) => interfaces.push(interface),
            Err(e) => {
                eprint!("{}", e.render(color));
                return Err(ExitCode::from(1));
            }
        }
    }
    // The tool trusts what it checks: every capability is granted (EMBEDDING §5).
    let config = colibri::CheckConfig {
        stack_budget: crate::NATIVE_STACK_BUDGET,
        interfaces,
        caps: colibri::CapSet::all(),
    };
    // The loader needs the names of the standard modules; their interfaces are built for the
    // ones the program imports.
    let names: Vec<String> = colibri_std::MODULES
        .iter()
        .map(|m| format!("std.{m}"))
        .collect();
    match colibri::check_path_lazy(file, &config, all_modules, &names, std_interface) {
        Ok(report) => {
            eprint!("{}", report.render(color));
            if report.has_errors() {
                Err(ExitCode::from(1))
            } else {
                Ok(())
            }
        }
        Err(e) => {
            eprint!("{}", e.render(color));
            Err(ExitCode::from(1))
        }
    }
}
