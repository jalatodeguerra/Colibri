//! The `colibri` command line tool.

use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use colibri_lang::on_big_stack;
use colibri_runtime as colibri;

/// Exit code of a program that panicked (the convention of Rust's own panics).
const EXIT_PANIC: u8 = 101;

#[derive(Parser)]
#[command(name = "colibri", version, about = "The Colibri programming language")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run a Colibri script, or the package in a folder (the current folder by default)
    Run { file: Option<PathBuf> },
    /// Type-check a Colibri script or package without running it
    Check {
        file: Option<PathBuf>,
        /// An interface file (`.cbi`) describing what the embedding program provides; may be
        /// given more than once
        #[arg(long = "interface", value_name = "FILE")]
        interfaces: Vec<PathBuf>,
    },
    /// Format Colibri source files in place (`-` formats stdin to stdout)
    Fmt {
        /// Files or directories; defaults to the current directory
        paths: Vec<PathBuf>,
        /// Change nothing; exit with 1 and list the files that would be reformatted
        #[arg(long)]
        check: bool,
    },
}

/// Checks `file`, printing every diagnostic to stderr. `Err` is the exit code to stop with.
fn check(file: &Path, interface_files: &[PathBuf]) -> Result<(), ExitCode> {
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
        stack_budget: colibri_lang::NATIVE_STACK_BUDGET,
        interfaces,
        caps: colibri::CapSet::all(),
    };
    match colibri::check_path_with(file, &config) {
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

fn run(file: &Path) -> ExitCode {
    if let Err(code) = check(file, &[]) {
        return code;
    }
    let color = io::stderr().is_terminal();
    let mut vm = colibri_lang::vm_builder().allow_all().build();
    match vm.load_path(file).and_then(|()| vm.run_main()) {
        Ok(()) => ExitCode::SUCCESS,
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

fn main() -> ExitCode {
    let cli = Cli::parse();
    on_big_stack(move || match cli.command {
        Command::Check { file, interfaces } => {
            match check(&file.unwrap_or_else(|| PathBuf::from(".")), &interfaces) {
                Ok(()) => ExitCode::SUCCESS,
                Err(code) => code,
            }
        }
        Command::Run { file } => run(&file.unwrap_or_else(|| PathBuf::from("."))),
        Command::Fmt { paths, check } => colibri_lang::fmt::run(&paths, check),
    })
}
