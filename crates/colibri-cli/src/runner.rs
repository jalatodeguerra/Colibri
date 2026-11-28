//! `runner_main`: the command line of a project runner (phase 4). A generated runner is a binary
//! that calls it with the project's binding table; it takes the commands that run a program
//! (`run`, with `--leaks` and `--profile`, `test`, `repl`, `debug`) with the flags `colibri`
//! gives them (keep them in step with `main.rs`), and each `Vm` it builds has the bindings
//! registered. It has no `check`, `fmt`, `build` or language server: `colibri` does those.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use crate::bindings::{self, Binding};
use crate::cliargs::{ReplArgs, RunArgs, RunnerFlag, TestArgs};
use crate::on_big_stack;

#[derive(Parser)]
#[command(
    name = "colibri-project-runner",
    version,
    about = "A Colibri project runner"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run a script, or the package in a folder (the current folder by default)
    Run {
        #[command(flatten)]
        args: RunArgs,
        #[command(flatten)]
        runner: RunnerFlag,
    },
    /// Run the `test "name" { .. }` blocks of a script or package
    Test {
        #[command(flatten)]
        args: TestArgs,
        #[command(flatten)]
        runner: RunnerFlag,
    },
    /// Start an interactive session
    Repl {
        #[command(flatten)]
        args: ReplArgs,
        #[command(flatten)]
        runner: RunnerFlag,
    },
    /// Debug adapter on standard input and output
    Debug,
    /// Writes `<key>.cbi` for each binding into DIR (`colibri bindings` calls this)
    #[command(name = "__write-interfaces", hide = true)]
    WriteInterfaces { dir: PathBuf },
}

/// The `main` of a project runner: parses the command line and runs it with `table` registered
/// in every `Vm`, the main one and the isolates of thread tasks.
pub fn runner_main(table: &'static [Binding]) -> ExitCode {
    // Keeps the version marker `colibri build --runner` looks for in the binary.
    std::hint::black_box(crate::platform::own_version());
    bindings::install(table);
    let cli = Cli::parse();
    // A binding that cannot register (a clash, a panic) is said once, here, instead of being
    // found by whichever `Vm` is built first.
    if let Err(e) = bindings::register_all(&mut std_vm(), table) {
        eprintln!("error: {e}");
        return ExitCode::from(1);
    }
    on_big_stack(move || match cli.command {
        Command::Run {
            runner: _,
            args:
                RunArgs {
                    file,
                    watch,
                    args,
                    leaks,
                    profile,
                    profile_weight,
                    profile_rc,
                    build,
                    deps,
                },
        } => {
            crate::set_build_cfg(build.cfg());
            let file = file.unwrap_or_else(|| PathBuf::from("."));
            if watch {
                match crate::depscmd::prepare(&file, deps) {
                    Ok(()) => crate::watch::run(&file, args),
                    Err(code) => code,
                }
            } else {
                let profile = profile.map(|f| crate::profilecmd::Options::new(&f, profile_weight));
                crate::runcmd::run(&file, args, leaks, deps, profile.as_ref(), profile_rc)
            }
        }
        Command::Test {
            runner: _,
            args:
                TestArgs {
                    file,
                    filter,
                    budget,
                    nocapture,
                    leaks,
                    profile,
                    profile_weight,
                    profile_rc,
                    build,
                    deps,
                },
        } => {
            crate::set_build_cfg(build.cfg());
            crate::testcmd::run(&crate::testcmd::Options {
                path: file.unwrap_or_else(|| PathBuf::from(".")),
                filter,
                budget,
                nocapture,
                leaks,
                deps,
                profile: profile.map(|f| crate::profilecmd::Options::new(&f, profile_weight)),
                profile_rc,
            })
        }
        Command::Repl {
            args: ReplArgs { build },
            runner: _,
        } => {
            crate::set_build_cfg(build.cfg());
            crate::repl::run()
        }
        Command::Debug => crate::debugcmd::run(),
        Command::WriteInterfaces { dir } => write_interfaces(&dir, table),
    })
}

/// A `Vm` with the standard library, like the real ones: a binding may use its types.
fn std_vm() -> colibri_runtime::Vm {
    let mut vm = colibri_runtime::Vm::builder().build();
    colibri_std::install(&mut vm).expect("the standard library registers into a new Vm");
    vm
}

/// Registers each binding into a `Vm` with the standard library and writes its module's interface to `dir/<name>.cbi`.
fn write_interfaces(dir: &std::path::Path, table: &[Binding]) -> ExitCode {
    let mut vm = std_vm();
    if let Err(e) = bindings::register_all(&mut vm, table) {
        eprintln!("error: {e}");
        return ExitCode::from(1);
    }
    if let Err(e) = std::fs::create_dir_all(dir) {
        eprintln!("error: cannot create `{}`: {e}", dir.display());
        return ExitCode::from(1);
    }
    for binding in table {
        let path = dir.join(format!("{}.cbi", binding.name));
        if let Err(e) = vm.module(binding.name).write_interface(&path) {
            eprintln!("error: cannot write `{}`: {e}", path.display());
            return ExitCode::from(1);
        }
    }
    ExitCode::SUCCESS
}
