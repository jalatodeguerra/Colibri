//! The `colibri` command line tool.

use std::io::{self, IsTerminal};
use std::path::PathBuf;
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
    /// Run a Colibri program
    Run { file: PathBuf },
    /// Type-check a Colibri program without running it
    Check { file: PathBuf },
}

/// Checks `file`, printing every diagnostic to stderr. `Err` is the exit code to stop with.
fn check(file: &PathBuf) -> Result<(), ExitCode> {
    let color = io::stderr().is_terminal();
    match colibri::check_file_with_stack_budget(file, colibri_lang::NATIVE_STACK_BUDGET) {
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

fn run(file: &PathBuf) -> ExitCode {
    if let Err(code) = check(file) {
        return code;
    }
    let color = io::stderr().is_terminal();
    let mut vm = colibri_lang::vm_builder().build();
    match vm.load_file(file).and_then(|()| vm.run_main()) {
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
        Command::Check { file } => match check(&file) {
            Ok(()) => ExitCode::SUCCESS,
            Err(code) => code,
        },
        Command::Run { file } => run(&file),
    })
}
