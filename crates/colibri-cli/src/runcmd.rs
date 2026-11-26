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
    let mut vm = build_run_vm(file, args, leaks.is_some(), Box::new(io::stdout()));
    run_with(&mut vm, file, leaks, profile, rc)
}

/// The assets of a worker that could not open the executable again: none.
struct NoAssets;

impl colibri_std::AssetSource for NoAssets {
    fn read(&mut self, _path: &str) -> Result<Vec<u8>, colibri_std::AssetError> {
        Err(colibri_std::AssetError::NotFound(
            colibri_std::NOT_AN_ASSET.to_string(),
        ))
    }
    fn list(&mut self, _dir: &str) -> Result<Vec<String>, colibri_std::AssetError> {
        Err(colibri_std::AssetError::NotFound(
            colibri_std::NOT_AN_ASSET.to_string(),
        ))
    }
}

/// The `Vm` of `colibri run file`: every capability, the standard library, the program's assets,
/// and `out` for what `print` writes, shared with the workers of thread tasks (whole lines, in the
/// order they are finished). Their library is the main one's: the same arguments, assets and
/// environment.
#[doc(hidden)]
pub fn build_run_vm(
    file: &Path,
    args: Vec<String>,
    track_leaks: bool,
    out: Box<dyn io::Write + Send>,
) -> colibri::Vm {
    crate::build_vm_with_assets(
        crate::vm_builder()
            .allow_all()
            .track_leaks(track_leaks)
            .stdout_shared(out),
        args,
        Some(crate::assets::factory_for_entry(file)),
    )
}

/// Runs the program of a built executable (`colibri build`): the same as `colibri run` on the
/// files it was built from, with every capability granted, but read from the pack.
pub fn run_pack(pack: crate::pack::Pack, file: std::fs::File, args: Vec<String>) -> ExitCode {
    let (fs, start) = pack.file_system();
    // The first source reads from the file that was opened; a worker opens the executable again
    // (the pack's own file), so that no two threads share a file position.
    let pack = std::sync::Arc::new(pack);
    let first = std::sync::Mutex::new(Some(file));
    let assets: colibri_std::Factory<Box<dyn colibri_std::AssetSource>> = {
        let pack = std::sync::Arc::clone(&pack);
        std::sync::Arc::new(move || match first.lock().ok().and_then(|mut f| f.take()) {
            Some(file) => crate::assets::for_pack(&pack, file),
            None => match std::env::current_exe().and_then(std::fs::File::open) {
                Ok(file) => crate::assets::for_pack(&pack, file),
                Err(_) => Box::new(NoAssets),
            },
        })
    };
    let mut vm = crate::build_vm_with_assets(crate::vm_builder().allow_all(), args, Some(assets));
    let color = io::stderr().is_terminal();
    vm.on_warnings(move |report| eprint!("{}", report.render(color)));
    let loaded = match &start {
        crate::pack::Start::File(path) => vm.load_from(&fs, colibri::Entry::File(Path::new(path))),
        crate::pack::Start::Package(dir) => {
            vm.load_from(&fs, colibri::Entry::Package(Path::new(dir)))
        }
    };
    finish_run(&mut vm, loaded, None, None, false, color)
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
    finish_run(vm, loaded, leaks, profile, rc, color)
}

/// Runs a program that was loaded by `loaded`, as `colibri run` does.
fn finish_run(
    vm: &mut colibri::Vm,
    loaded: Result<(), colibri::Error>,
    leaks: Option<LeakMode>,
    profile: Option<&profilecmd::Options>,
    rc: bool,
    color: bool,
) -> ExitCode {
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
