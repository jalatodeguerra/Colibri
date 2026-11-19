//! The `colibri` command line tool.

use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use colibri_lang::leakcmd::{self, LeakMode};
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
    Run {
        file: Option<PathBuf>,
        /// Keep running and reload the program when its source files change: edits that only
        /// touch function bodies are patched into the running program, other edits restart
        /// it (`save_state() -> str` and `restore_state(s: str)` carry state across), and a
        /// panic or the end of the program waits for the next edit
        #[arg(long)]
        watch: bool,
        /// Arguments for the script, which it reads with `os.args()` (after `--` if they
        /// start with a dash)
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
        /// Run the leak detector when the program ends and report the reference cycles that
        /// were never freed to stderr; `--leaks=error` also exits with 1 (ARCHITECTURE §18)
        #[arg(long, value_enum, value_name = "MODE", num_args = 0..=1, require_equals = true, default_missing_value = "warn")]
        leaks: Option<colibri_lang::leakcmd::LeakMode>,
    },
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
    /// Run the `test "name" { .. }` blocks of a script or package (the current folder by
    /// default); a failing test does not stop the others
    Test {
        file: Option<PathBuf>,
        /// Run only the tests whose `module::name` contains this text
        #[arg(long, value_name = "TEXT")]
        filter: Option<String>,
        /// Steps one test may take before it fails (0 is unlimited)
        #[arg(long, value_name = "STEPS", default_value_t = colibri_lang::testcmd::DEFAULT_BUDGET)]
        budget: u64,
        /// Show what the tests print at once, not only for failing tests
        #[arg(long)]
        nocapture: bool,
        /// Report the reference cycles each test leaks; `--leaks=error` fails the test
        #[arg(long, value_enum, value_name = "MODE", num_args = 0..=1, require_equals = true, default_missing_value = "warn")]
        leaks: Option<colibri_lang::leakcmd::LeakMode>,
    },
    /// Create a new package in a folder named after it: a `colibri.toml`, a `main.colibri`
    /// that prints a greeting, and a test
    New {
        /// The package name, which is also the folder (an existing folder must be empty)
        name: PathBuf,
    },
    /// Start an interactive session: type statements, expressions and declarations; values
    /// and declarations persist (`:help` lists the commands; input piped to stdin is read
    /// line by line without a prompt)
    Repl,
    /// Run the language server (LSP) on stdin and stdout
    Lsp,
}

/// Checks `file`, printing every diagnostic to stderr. `Err` is the exit code to stop with.
/// With `all_modules`, a package's modules that its entry does not import are checked too
/// (`colibri check`); `run` needs only what the program uses.
fn check(file: &Path, interface_files: &[PathBuf], all_modules: bool) -> Result<(), ExitCode> {
    let color = io::stderr().is_terminal();
    let mut interfaces = colibri_std::interfaces();
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
    let checked = if all_modules && file.is_dir() {
        colibri::check_package_all_with(file, &config)
    } else {
        colibri::check_path_with(file, &config)
    };
    match checked {
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

fn run(file: &Path, args: Vec<String>, leaks: Option<LeakMode>) -> ExitCode {
    if let Err(code) = check(file, &[], false) {
        return code;
    }
    let color = io::stderr().is_terminal();
    let mut vm = colibri_lang::build_vm(
        colibri_lang::vm_builder()
            .allow_all()
            .track_leaks(leaks.is_some()),
        args,
    );
    // After `main`, coroutines keep running (timed by the real clock) until none is left.
    let outcome = vm
        .load_path(file)
        .and_then(|()| vm.run_main())
        .and_then(|()| vm.run_event_loop());
    let code = exit_code(&mut vm, outcome, color);
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

/// Serves the language server protocol on stdin and stdout.
fn lsp() -> ExitCode {
    let config = colibri_lsp::Config {
        stack_budget: colibri_lang::NATIVE_STACK_BUDGET,
    };
    match colibri_lsp::run_stdio(config) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(1)
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    on_big_stack(move || match cli.command {
        Command::Check { file, interfaces } => {
            match check(&file.unwrap_or_else(|| PathBuf::from(".")), &interfaces, true) {
                Ok(()) => ExitCode::SUCCESS,
                Err(code) => code,
            }
        }
        Command::Run {
            file,
            watch,
            args,
            leaks,
        } => {
            let file = file.unwrap_or_else(|| PathBuf::from("."));
            if watch {
                colibri_lang::watch::run(&file, args)
            } else {
                run(&file, args, leaks)
            }
        }
        Command::Test {
            file,
            filter,
            budget,
            nocapture,
            leaks,
        } => colibri_lang::testcmd::run(&colibri_lang::testcmd::Options {
            path: file.unwrap_or_else(|| PathBuf::from(".")),
            filter,
            budget,
            nocapture,
            leaks,
        }),
        Command::Fmt { paths, check } => colibri_lang::fmt::run(&paths, check),
        Command::New { name } => colibri_lang::newcmd::run(&name),
        Command::Repl => colibri_lang::repl::run(),
        Command::Lsp => lsp(),
    })
}
