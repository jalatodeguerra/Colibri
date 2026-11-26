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
/// corrupt the stream. On Windows the process's standard output is used as it is.
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

#[cfg(not(unix))]
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
