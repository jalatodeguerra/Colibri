//! `runner_main`: the command line of a project runner (phase 4). A generated runner is a binary
//! that calls it with the project's binding table; it takes the commands that run a program
//! (`run`, with `--leaks` and `--profile`, `test`, `repl`, `debug`) with the flags `colibri`
//! gives them (keep them in step with `main.rs`), and each `Vm` it builds has the bindings
//! registered. It has no `check`, `fmt`, `build` or language server: `colibri` does those.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use crate::bindings::{self, Binding};
use crate::depscmd::DepFlags;
use crate::on_big_stack;

#[derive(clap::Args, Clone, Debug, Default)]
struct BuildFlags {
    /// Turn on a feature flag (may be given more than once)
    #[arg(long = "feature", value_name = "NAME", value_parser = feature_name)]
    features: Vec<String>,
    /// Do not turn on the features of the `default` list in `[features]`
    #[arg(long)]
    no_default_features: bool,
    /// A release build: `@when(debug)` is false
    #[arg(long)]
    release: bool,
}

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
        file: Option<PathBuf>,
        /// Keep running and reload the program when its source files change
        #[arg(long)]
        watch: bool,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
        /// Report the reference cycles that were never freed (`--leaks=error` exits with 1)
        #[arg(long, value_enum, value_name = "MODE", num_args = 0..=1, require_equals = true, default_missing_value = "warn", conflicts_with = "watch")]
        leaks: Option<crate::leakcmd::LeakMode>,
        /// Write a flame graph (SVG) to FILE (`profile.svg` by default)
        #[arg(long, value_name = "FILE", num_args = 0..=1, require_equals = true, default_missing_value = "profile.svg", conflicts_with = "watch")]
        profile: Option<PathBuf>,
        /// What a sample weighs with `--profile`: `time` or `steps`
        #[arg(
            long,
            value_enum,
            value_name = "WEIGHT",
            default_value = "time",
            requires = "profile"
        )]
        profile_weight: crate::profilecmd::Weight,
        /// Count the reference-count traffic of each line
        #[arg(long, conflicts_with = "watch")]
        profile_rc: bool,
        #[command(flatten)]
        build: BuildFlags,
        #[command(flatten)]
        deps: DepFlags,
    },
    /// Run the `test "name" { .. }` blocks of a script or package
    Test {
        file: Option<PathBuf>,
        /// Run only the tests whose `module::name` contains this text
        #[arg(long, value_name = "TEXT")]
        filter: Option<String>,
        /// Steps one test may take before it fails (0 is unlimited)
        #[arg(long, value_name = "STEPS", default_value_t = crate::testcmd::DEFAULT_BUDGET)]
        budget: u64,
        /// Show what the tests print at once, not only for failing tests
        #[arg(long)]
        nocapture: bool,
        /// Report the reference cycles each test leaks
        #[arg(long, value_enum, value_name = "MODE", num_args = 0..=1, require_equals = true, default_missing_value = "warn")]
        leaks: Option<crate::leakcmd::LeakMode>,
        /// Write a flame graph (SVG) of all the tests to FILE (`profile.svg` by default)
        #[arg(long, value_name = "FILE", num_args = 0..=1, require_equals = true, default_missing_value = "profile.svg")]
        profile: Option<PathBuf>,
        /// What a sample weighs with `--profile`: `time` or `steps`
        #[arg(
            long,
            value_enum,
            value_name = "WEIGHT",
            default_value = "time",
            requires = "profile"
        )]
        profile_weight: crate::profilecmd::Weight,
        /// Count the reference-count traffic of each line
        #[arg(long)]
        profile_rc: bool,
        #[command(flatten)]
        build: BuildFlags,
        #[command(flatten)]
        deps: DepFlags,
    },
    /// Start an interactive session
    Repl {
        #[command(flatten)]
        build: BuildFlags,
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
    if let Err(e) = bindings::register_all(&mut colibri_runtime::Vm::builder().build(), table) {
        eprintln!("error: {e}");
        return ExitCode::from(1);
    }
    on_big_stack(move || match cli.command {
        Command::Run {
            file,
            watch,
            args,
            leaks,
            profile,
            profile_weight,
            profile_rc,
            build,
            deps,
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
        Command::Repl { build } => {
            crate::set_build_cfg(build.cfg());
            crate::repl::run()
        }
        Command::Debug => crate::debugcmd::run(),
        Command::WriteInterfaces { dir } => write_interfaces(&dir, table),
    })
}

/// Registers each binding into a bare `Vm` and writes its module's interface to `dir/<name>.cbi`.
fn write_interfaces(dir: &std::path::Path, table: &[Binding]) -> ExitCode {
    let mut vm = colibri_runtime::Vm::builder().build();
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
