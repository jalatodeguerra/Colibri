//! `colibri run`: load a script or package once, in the real `Vm`, and run it.

use std::io::{self, IsTerminal};
use std::path::Path;
use std::process::ExitCode;

use colibri_runtime as colibri;

use crate::depscmd::{self, DepFlags};
use crate::leakcmd::{self, LeakMode};
use crate::profilecmd;

/// Exit code of a program that panicked (the convention of Rust's own panics).
const EXIT_PANIC: u8 = 101;

/// Runs `file`: syncs the dependencies, builds the `Vm`, loads the program (the loader's
/// compile errors are the diagnostics) and runs it. Only the standard modules the program
/// imports are registered.
pub fn run(
    file: &Path,
    args: Vec<String>,
    leaks: Option<LeakMode>,
    deps: DepFlags,
    profile: Option<&profilecmd::Options>,
    rc: bool,
) -> ExitCode {
    if let Err(code) = depscmd::prepare(file, deps) {
        return code;
    }
    let mut vm = crate::build_vm(
        crate::vm_builder().allow_all().track_leaks(leaks.is_some()),
        args,
    );
    run_with(&mut vm, file, leaks, profile, rc)
}

/// [`run`] in a `Vm` that has the standard library installed.
pub fn run_in(vm: &mut colibri::Vm, file: &Path, leaks: Option<LeakMode>) -> ExitCode {
    run_with(vm, file, leaks, None, false)
}

/// [`run_in`], profiling the run when `profile` is given: the profile covers the load, `main`
/// and the coroutines that keep running after it, and is written before the leak report. `rc`
/// counts reference-count traffic by line (`--profile-rc`) and prints that report after it.
pub fn run_with(
    vm: &mut colibri::Vm,
    file: &Path,
    leaks: Option<LeakMode>,
    profile: Option<&profilecmd::Options>,
    rc: bool,
) -> ExitCode {
    let color = io::stderr().is_terminal();
    // Warnings are printed once the program has checked and before any of its code runs:
    // its top level may run for a long time.
    vm.on_warnings(move |report| eprint!("{}", report.render(color)));
    if rc {
        // Before the load: the counting is part of the code that gets compiled.
        profilecmd::start_rc(vm);
    }
    if let Some(options) = profile {
        profilecmd::start(vm, options);
    }
    let loaded = vm.load_path(file);
    // After `main`, coroutines keep running (timed by the real clock) until none is left.
    let outcome = loaded
        .and_then(|()| vm.run_main())
        .and_then(|()| vm.run_event_loop());
    if let Some(options) = profile {
        profilecmd::finish(vm, options);
    }
    if rc {
        profilecmd::finish_rc(vm);
    }
    let code = exit_code(vm, outcome, color);
    // The program is over: coroutines are cancelled and module-level values released, so a
    // cycle that is left is a leak.
    let Some(mode) = leaks else { return code };
    vm.shutdown();
    let found = vm.find_leaks();
    eprint!("{}", leakcmd::render(&found, mode, color));
    if mode == LeakMode::Error && !found.is_empty() && code == ExitCode::SUCCESS {
        return ExitCode::from(1);
    }
    code
}

/// The exit code of a finished run, printing its error.
fn exit_code(vm: &mut colibri::Vm, outcome: Result<(), colibri::Error>, color: bool) -> ExitCode {
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        // `os.exit(n)` ends the program as a panic does; the code is what it asked for.
        Err(colibri::Error::Panic { .. }) if colibri_std::exit_code(vm).is_some() => {
            ExitCode::from(colibri_std::exit_code(vm).unwrap_or(1) as u8)
        }
        Err(e @ colibri::Error::Panic { .. }) => {
            eprint!("{}", e.render(color));
            ExitCode::from(EXIT_PANIC)
        }
        Err(e) => {
            eprint!("{}", e.render(color));
            ExitCode::from(1)
        }
    }
}
