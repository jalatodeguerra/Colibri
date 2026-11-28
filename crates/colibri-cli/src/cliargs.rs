//! The command line that `colibri` and a project runner share: the flags of `run`, `test` and
//! `repl` are defined once, here, so a runner takes exactly what `colibri` takes.

use std::path::PathBuf;

use crate::depscmd::DepFlags;

/// The flags that decide `@when` conditions (DESIGN §3.6).
#[derive(clap::Args, Clone, Debug, Default)]
pub struct BuildFlags {
    /// Turn on a feature flag: `@when(name)` items are kept. With a `colibri.toml` the feature
    /// must be declared in its `[features]` table. May be given more than once
    #[arg(long = "feature", value_name = "NAME", value_parser = feature_name)]
    pub features: Vec<String>,
    /// Do not turn on the features of the `default` list in `[features]`
    #[arg(long)]
    pub no_default_features: bool,
    /// A release build: `@when(debug)` is false
    #[arg(long)]
    pub release: bool,
}

/// A feature name, checked as in `colibri.toml` (`--feature debug` would mean nothing).
pub fn feature_name(name: &str) -> Result<String, String> {
    match colibri_runtime::feature_name_problem(name) {
        Some((message, hint)) => Err(format!("{message} ({hint})")),
        None => Ok(name.to_string()),
    }
}

impl BuildFlags {
    pub fn cfg(&self) -> colibri_runtime::BuildCfg {
        let mut cfg = colibri_runtime::BuildCfg::default();
        for name in &self.features {
            cfg = cfg.with_feature(name);
        }
        cfg.no_default_features = self.no_default_features;
        cfg.debug = !self.release;
        cfg
    }
}

/// `--runner FILE` of `run`, `test` and `repl`. A project runner takes it too and ignores it, so
/// that the command line `colibri` was given can be handed over unchanged (what follows the
/// script is the program's, and only the parser knows which `--runner` is whose).
#[derive(clap::Args, Clone, Debug, Default)]
pub struct RunnerFlag {
    /// For a package with `[rust-dependencies]`: use this project runner (made by `colibri
    /// bindings -o FILE` here or elsewhere) instead of building one. It must be of this version
    /// and hold exactly the package's bindings, which is checked first (a project runner ignores it)
    #[arg(long, value_name = "FILE")]
    pub runner: Option<PathBuf>,
}

/// The arguments of `run`.
#[derive(clap::Args, Clone, Debug)]
pub struct RunArgs {
    pub file: Option<PathBuf>,
    /// Keep running and reload the program when its source files change: edits that only
    /// touch function bodies are patched into the running program, other edits restart
    /// it (`save_state() -> str` and `restore_state(s: str)` carry state across), and a
    /// panic or the end of the program waits for the next edit
    #[arg(long)]
    pub watch: bool,
    /// Arguments for the script, which it reads with `os.args()` (after `--` if they
    /// start with a dash)
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<String>,
    /// Run the leak detector when the program ends and report the reference cycles that
    /// were never freed to stderr; `--leaks=error` also exits with 1
    #[arg(long, value_enum, value_name = "MODE", num_args = 0..=1, require_equals = true, default_missing_value = "warn", conflicts_with = "watch")]
    pub leaks: Option<crate::leakcmd::LeakMode>,
    /// Sample the running program and write a flame graph (SVG) to FILE (`profile.svg` by
    /// default) and the folded stacks it was drawn from next to it (`profile.folded`; a FILE without an extension gets `.svg`), then
    /// print the heaviest functions and lines to stderr. A sample is taken about every
    /// 1,024 steps, so a long call into a host function or a built-in is charged to the
    /// line that made the call; stacks deeper than 200 frames are cut in the middle; time the program spends waiting is `[idle]`
    #[arg(long, value_name = "FILE", num_args = 0..=1, require_equals = true, default_missing_value = "profile.svg", conflicts_with = "watch")]
    pub profile: Option<PathBuf>,
    /// What a sample weighs with `--profile`: `time` (the default) is real time, so time in
    /// host functions counts; `steps` counts instructions, which gives the same profile on
    /// every run
    #[arg(
        long,
        value_enum,
        value_name = "WEIGHT",
        default_value = "time",
        requires = "profile"
    )]
    pub profile_weight: crate::profilecmd::Weight,
    /// Count the reference-count traffic of each line (references taken and given up by
    /// copying, reading and dropping strings, lists, structs and other values that have a
    /// reference count, and `x.clone()`) and print the lines with the most to stderr. It
    /// works on its own and does not need `--profile`. The program is compiled with a
    /// counter next to each such instruction, which makes it slower (10 to 30% for most
    /// programs, up to 2 times for one that mostly passes structs around, not at all for
    /// arithmetic) and changes nothing else. Not counted: what built-ins written in Rust do
    /// inside (`sort`, `join`, map lookups), host functions, field and element writes and
    /// the cleanup of a panic
    #[arg(long, conflicts_with = "watch")]
    pub profile_rc: bool,
    #[command(flatten)]
    pub build: BuildFlags,
    #[command(flatten)]
    pub deps: DepFlags,
}

/// The arguments of `test`.
#[derive(clap::Args, Clone, Debug)]
pub struct TestArgs {
    pub file: Option<PathBuf>,
    /// Run only the tests whose `module::name` contains this text
    #[arg(long, value_name = "TEXT")]
    pub filter: Option<String>,
    /// Steps one test may take before it fails (0 is unlimited)
    #[arg(long, value_name = "STEPS", default_value_t = crate::testcmd::DEFAULT_BUDGET)]
    pub budget: u64,
    /// Show what the tests print at once, not only for failing tests
    #[arg(long)]
    pub nocapture: bool,
    /// Report the reference cycles each test leaks; `--leaks=error` fails the test
    #[arg(long, value_enum, value_name = "MODE", num_args = 0..=1, require_equals = true, default_missing_value = "warn")]
    pub leaks: Option<crate::leakcmd::LeakMode>,
    /// Sample the running program and write a flame graph (SVG) to FILE (`profile.svg` by
    /// default) and the folded stacks it was drawn from next to it (`profile.folded`; a FILE without an extension gets `.svg`), then
    /// print the heaviest functions and lines to stderr. A sample is taken about every
    /// 1,024 steps, so a long call into a host function or a built-in is charged to the
    /// line that made the call; stacks deeper than 200 frames are cut in the middle; time the program spends waiting is `[idle]`; all the tests make
    /// one profile
    #[arg(long, value_name = "FILE", num_args = 0..=1, require_equals = true, default_missing_value = "profile.svg")]
    pub profile: Option<PathBuf>,
    /// What a sample weighs with `--profile`: `time` (the default) is real time, so time in
    /// host functions counts; `steps` counts instructions, which gives the same profile on
    /// every run
    #[arg(
        long,
        value_enum,
        value_name = "WEIGHT",
        default_value = "time",
        requires = "profile"
    )]
    pub profile_weight: crate::profilecmd::Weight,
    /// Count the reference-count traffic of each line (references taken and given up by
    /// copying, reading and dropping strings, lists, structs and other values that have a
    /// reference count, and `x.clone()`) and print the lines with the most to stderr (one
    /// report for all the tests). It works on its own and does not need `--profile`. The program is compiled with a
    /// counter next to each such instruction, which makes it slower (10 to 30% for most
    /// programs, up to 2 times for one that mostly passes structs around, not at all for
    /// arithmetic) and changes nothing else. Not counted: what built-ins written in Rust do
    /// inside (`sort`, `join`, map lookups), host functions, field and element writes and
    /// the cleanup of a panic
    #[arg(long)]
    pub profile_rc: bool,
    #[command(flatten)]
    pub build: BuildFlags,
    #[command(flatten)]
    pub deps: DepFlags,
}

/// The arguments of `repl`.
#[derive(clap::Args, Clone, Debug)]
pub struct ReplArgs {
    #[command(flatten)]
    pub build: BuildFlags,
}
