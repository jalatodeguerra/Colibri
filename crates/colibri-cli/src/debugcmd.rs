//! `colibri debug`: the debug adapter (Debug Adapter Protocol on standard input and output).
//! The protocol is in `colibri-dap`; this builds the `Vm` of a `launch` as `colibri run` does.

use std::io::{self, BufReader, Write};
use std::process::ExitCode;

use colibri_dap::LaunchArgs;

use crate::depscmd::{self, DepFlags};

/// Runs the adapter until the client disconnects.
pub fn run() -> ExitCode {
    let code = colibri_dap::serve(
        BufReader::new(io::stdin()),
        protocol_output(),
        build_program_vm,
    );
    ExitCode::from(code as u8)
}

/// Where the protocol is written. On Unix the adapter keeps its own copy of the standard
/// output and points file descriptor 1 at standard error, so that whatever else writes there
/// (a program started with `process.status`, a C function called through `extern`) cannot
/// corrupt the stream. Windows does the same with its standard output handle.
#[cfg(unix)]
fn protocol_output() -> Box<dyn io::Write + Send> {
    use std::os::fd::FromRawFd;
    // SAFETY: `fcntl` with `F_DUPFD_CLOEXEC` gives a new descriptor (3 or above, closed in
    // every program this one starts) that this function owns from here on (it is checked);
    // `dup2` then makes descriptor 1 refer to what descriptor 2 refers to.
    unsafe {
        let own = libc::fcntl(1, libc::F_DUPFD_CLOEXEC, 3);
        if own < 0 || libc::dup2(2, 1) < 0 {
            if own >= 0 {
                libc::close(own);
            }
            return Box::new(io::stdout());
        }
        Box::new(std::fs::File::from_raw_fd(own))
    }
}

/// Windows: the adapter keeps a private (not inheritable) duplicate of the standard output
/// handle for the protocol, and makes the process's standard output handle the standard error
/// handle. A program started with `process.status` takes the process's standard output at the
/// moment it starts (std reads it then), so it writes to standard error as on Unix.
#[cfg(windows)]
fn protocol_output() -> Box<dyn io::Write + Send> {
    use std::os::windows::io::{AsHandle, AsRawHandle};

    unsafe extern "system" {
        /// `STD_OUTPUT_HANDLE` is `-11` as a `u32`; a non-zero result is success.
        fn SetStdHandle(which: u32, handle: *mut std::ffi::c_void) -> i32;
    }
    const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;

    let stdout = io::stdout();
    let raw = stdout.as_raw_handle();
    // No standard output (a null or invalid handle), or one that cannot be copied: the
    // protocol goes where it always went, and children keep the real standard output.
    if raw.is_null() || raw as isize == -1 {
        return Box::new(io::stdout());
    }
    let Ok(own) = stdout.as_handle().try_clone_to_owned() else {
        return Box::new(io::stdout());
    };
    let err = io::stderr().as_raw_handle();
    if err.is_null() || err as isize == -1 {
        return Box::new(io::stdout());
    }
    // SAFETY: `SetStdHandle` only stores a handle value in the process; `err` is the live
    // standard error handle, which is not closed here.
    if unsafe { SetStdHandle(STD_OUTPUT_HANDLE, err) } == 0 {
        return Box::new(io::stdout());
    }
    Box::new(std::fs::File::from(own))
}

#[cfg(not(any(unix, windows)))]
fn protocol_output() -> Box<dyn io::Write + Send> {
    Box::new(io::stdout())
}

/// The `Vm` for the program a `launch` names: every capability, the program's standard output
/// and error as the adapter gives them, and no standard input (the client's input is the
/// protocol).
fn build_program_vm(args: LaunchArgs) -> Result<colibri_runtime::Vm, String> {
    let mut cfg = colibri_runtime::BuildCfg::default();
    for name in &args.features {
        if let Some((message, hint)) = colibri_runtime::feature_name_problem(name) {
            return Err(format!("feature `{name}`: {message} ({hint})"));
        }
        cfg = cfg.with_feature(name);
    }
    cfg.debug = true;
    // The build configuration is set once for the process: a second launch with other features
    // would silently keep the first ones. The order of the features does not matter, nor does a
    // feature named twice.
    static FIRST: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    let mut features = args.features.clone();
    features.sort();
    features.dedup();
    if *FIRST.get_or_init(|| features.clone()) != features {
        return Err(format!(
            "the features of this session were fixed by the first launch ({}); start a new debug session to use others",
            FIRST.get().map_or(String::new(), |f| f.join(", "))
        ));
    }
    crate::set_build_cfg(cfg);
    let mut args = args;
    // What `colibri run` prints on standard error goes to the client as output.
    let notes = depscmd::prepare_quiet(&args.program, DepFlags::default())?;
    for note in notes {
        let _ = writeln!(args.stderr, "  {note}");
    }
    let config = colibri_std::StdConfig {
        args: args.args,
        assets: Some(crate::assets::for_entry(&args.program)),
        stdin: Some(Box::new(io::empty())),
        ..colibri_std::StdConfig::default()
    };
    let mut vm =
        crate::build_vm_with_std(crate::vm_builder().allow_all().stdout(args.stdout), config);
    colibri_std::set_stderr(&mut vm, args.stderr);
    Ok(vm)
}
