//! The `colibri` command line tool.

use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use colibri_lang::check_syntax;

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

/// Parses `file`, printing diagnostics to stderr. `Err` carries the exit code
/// to stop with when the file cannot be read or has syntax errors.
fn parse(file: &Path) -> Result<(), ExitCode> {
    let color = io::stderr().is_terminal();
    match check_syntax(file, color, &mut io::stderr()) {
        Ok(false) => Ok(()),
        Ok(true) => Err(ExitCode::from(1)),
        Err(e) => {
            eprintln!("colibri: cannot read `{}`: {e}", file.display());
            Err(ExitCode::from(1))
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Check { file } => match parse(&file) {
            // Only the syntax is checked so far.
            Ok(()) => ExitCode::SUCCESS,
            Err(code) => code,
        },
        Command::Run { file } => match parse(&file) {
            Ok(()) => {
                eprintln!("colibri run: not implemented yet");
                ExitCode::from(2)
            }
            Err(code) => code,
        },
    }
}
