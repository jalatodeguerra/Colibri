//! Runs the example with `scripts/jobs.colibri` and prints its log.

use std::path::PathBuf;
use std::process::ExitCode;

use colibri_example_host_jobs::{DEFAULT_TICKS, JobHost, scripts_dir};

fn main() -> ExitCode {
    let ticks = match std::env::args().nth(1) {
        Some(arg) => match arg.parse() {
            Ok(n) => n,
            Err(_) => {
                eprintln!("usage: host_jobs [ticks]  (default {DEFAULT_TICKS})");
                return ExitCode::from(2);
            }
        },
        None => DEFAULT_TICKS,
    };
    let dir: PathBuf = scripts_dir();
    let mut host = match JobHost::new() {
        Ok(host) => host,
        Err(e) => {
            eprintln!("{}", e.render(false));
            return ExitCode::FAILURE;
        }
    };
    // The editors and `colibri check --interface` read this file; keep it current.
    if let Err(e) = host.write_interface(dir.join("host.cbi")) {
        eprintln!("warning: could not write host.cbi: {}", e.render(false));
    }
    if let Err(e) = host.load_file(dir.join("jobs.colibri")) {
        eprintln!("{}", e.render(false));
        return ExitCode::FAILURE;
    }
    for line in host.run(ticks) {
        println!("{line}");
    }
    ExitCode::SUCCESS
}
