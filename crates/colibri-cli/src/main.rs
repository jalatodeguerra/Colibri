//! The `colibri` command line tool.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use colibri_lang::depscmd::{self, DepFlags};
use colibri_lang::on_big_stack;

// The system allocator is a large share of the time of the allocation-heavy programs (`objs`, `strings`,
// `map`); mimalloc is faster. Only the binary chooses: a library cannot pick an allocator for its host.
#[cfg(not(target_arch = "wasm32"))]
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// The flags that decide `@when` conditions (DESIGN §3.6).
#[derive(clap::Args, Clone, Debug, Default)]
struct BuildFlags {
    /// Turn on a feature flag: `@when(name)` items are kept. With a `colibri.toml` the feature
    /// must be declared in its `[features]` table. May be given more than once
    #[arg(long = "feature", value_name = "NAME", value_parser = feature_name)]
    features: Vec<String>,
    /// Do not turn on the features of the `default` list in `[features]`
    #[arg(long)]
    no_default_features: bool,
    /// A release build: `@when(debug)` is false
    #[arg(long)]
    release: bool,
}

/// A feature name, checked as in `colibri.toml` (`--feature debug` would mean nothing).
fn feature_name(name: &str) -> Result<String, String> {
    match colibri_runtime::feature_name_problem(name) {
        Some((message, hint)) => Err(format!("{message} ({hint})")),
        None => Ok(name.to_string()),
    }
}

impl BuildFlags {
    fn cfg(&self) -> colibri_runtime::BuildCfg {
        let mut cfg = colibri_runtime::BuildCfg::default();
        for name in &self.features {
            cfg = cfg.with_feature(name);
        }
        cfg.no_default_features = self.no_default_features;
        cfg.debug = !self.release;
        cfg
    }
}

/// The engines `colibri run --engine` names.
#[derive(clap::ValueEnum, Clone, Copy, Debug)]
enum EngineArg {
    Tree,
    Vm,
}

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
        /// were never freed to stderr; `--leaks=error` also exits with 1
        #[arg(long, value_enum, value_name = "MODE", num_args = 0..=1, require_equals = true, default_missing_value = "warn")]
        leaks: Option<colibri_lang::leakcmd::LeakMode>,
        /// The engine that runs the program (default: `COLIBRI_ENGINE`, else `tree`)
        #[arg(long, value_enum, value_name = "ENGINE", hide = true)]
        engine: Option<EngineArg>,
        #[command(flatten)]
        build: BuildFlags,
        #[command(flatten)]
        deps: DepFlags,
    },
    /// Type-check a Colibri script or package without running it
    Check {
        file: Option<PathBuf>,
        /// An interface file (`.cbi`) describing what the embedding program provides; may be
        /// given more than once
        #[arg(long = "interface", value_name = "FILE")]
        interfaces: Vec<PathBuf>,
        #[command(flatten)]
        build: BuildFlags,
        #[command(flatten)]
        deps: DepFlags,
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
        #[command(flatten)]
        build: BuildFlags,
        #[command(flatten)]
        deps: DepFlags,
    },
    /// Resolve the git dependencies of a package again (branches, tags and default branches
    /// move) and write the new commits to `colibri.lock`
    Update {
        /// Only these dependencies (by name, transitive ones too); default: all of them
        names: Vec<String>,
        /// The package folder
        #[arg(long, value_name = "DIR", default_value = ".")]
        path: PathBuf,
        #[command(flatten)]
        deps: DepFlags,
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
    Repl {
        #[command(flatten)]
        build: BuildFlags,
    },
    /// Run the language server (LSP) on stdin and stdout
    Lsp,
    /// Write the standard library reference as Markdown (one page for each module and an index),
    /// generated from the interfaces of the modules; with `--check`, only report whether the
    /// pages already in the folder are current. For the maintainers of the toolchain: the
    /// result is `guide/std` in its repository
    #[command(hide = true)]
    Doc {
        /// The folder to write the pages into
        #[arg(long, value_name = "DIR")]
        std: PathBuf,
        /// Change nothing; exit with 1 and list the pages that are out of date
        #[arg(long)]
        check: bool,
    },
}

/// Serves the language server protocol on stdin and stdout.
fn lsp() -> ExitCode {
    let config = colibri_lsp::Config {
        stack_budget: colibri_lang::NATIVE_STACK_BUDGET,
        ..colibri_lsp::Config::default()
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
        Command::Check {
            file,
            interfaces,
            build,
            deps,
        } => {
            colibri_lang::set_build_cfg(build.cfg());
            match colibri_lang::checkcmd::check(
                &file.unwrap_or_else(|| PathBuf::from(".")),
                &interfaces,
                true,
                deps,
            ) {
                Ok(()) => ExitCode::SUCCESS,
                Err(code) => code,
            }
        }
        Command::Run {
            file,
            watch,
            args,
            leaks,
            engine,
            build,
            deps,
        } => {
            colibri_lang::set_build_cfg(build.cfg());
            if let Some(engine) = engine {
                colibri_lang::set_engine(match engine {
                    EngineArg::Tree => colibri_runtime::EngineKind::Tree,
                    EngineArg::Vm => colibri_runtime::EngineKind::Bytecode,
                });
            }
            let file = file.unwrap_or_else(|| PathBuf::from("."));
            if watch {
                match depscmd::prepare(&file, deps) {
                    Ok(()) => colibri_lang::watch::run(&file, args),
                    Err(code) => code,
                }
            } else {
                colibri_lang::runcmd::run(&file, args, leaks, deps)
            }
        }
        Command::Test {
            file,
            filter,
            budget,
            nocapture,
            leaks,
            build,
            deps,
        } => {
            colibri_lang::set_build_cfg(build.cfg());
            colibri_lang::testcmd::run(&colibri_lang::testcmd::Options {
                path: file.unwrap_or_else(|| PathBuf::from(".")),
                filter,
                budget,
                nocapture,
                leaks,
                deps,
            })
        }
        Command::Update { names, path, deps } => depscmd::update(&path, names, deps),
        Command::Fmt { paths, check } => colibri_lang::fmt::run(&paths, check),
        Command::New { name } => colibri_lang::newcmd::run(&name),
        Command::Repl { build } => {
            colibri_lang::set_build_cfg(build.cfg());
            colibri_lang::repl::run()
        }
        Command::Lsp => lsp(),
        Command::Doc { std, check } => colibri_lang::doccmd::run(&std, check),
    })
}
