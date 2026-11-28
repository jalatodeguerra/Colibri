//! The `colibri` command line tool.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use colibri_lang::depscmd::{self, DepFlags};
use colibri_lang::on_big_stack;

// The system allocator is a large share of the time of the allocation-heavy programs (`objs`, `strings`,
// `map`); mimalloc is faster. Only the binary chooses: a library cannot pick an allocator for its host.
#[cfg(not(target_family = "wasm"))]
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
        #[arg(long, value_enum, value_name = "MODE", num_args = 0..=1, require_equals = true, default_missing_value = "warn", conflicts_with = "watch")]
        leaks: Option<colibri_lang::leakcmd::LeakMode>,
        /// Sample the running program and write a flame graph (SVG) to FILE (`profile.svg` by
        /// default) and the folded stacks it was drawn from next to it (`profile.folded`; a FILE without an extension gets `.svg`), then
        /// print the heaviest functions and lines to stderr. A sample is taken about every
        /// 1,024 steps, so a long call into a host function or a built-in is charged to the
        /// line that made the call; stacks deeper than 200 frames are cut in the middle; time the program spends waiting is `[idle]`
        #[arg(long, value_name = "FILE", num_args = 0..=1, require_equals = true, default_missing_value = "profile.svg", conflicts_with = "watch")]
        profile: Option<PathBuf>,
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
        profile_weight: colibri_lang::profilecmd::Weight,
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
        profile_rc: bool,
        #[command(flatten)]
        build: BuildFlags,
        #[command(flatten)]
        deps: DepFlags,
    },
    /// Write one executable that holds a script or package, its dependencies and the standard
    /// library, and runs it when started: `colibri build app -o app-bin`. It is a copy of this
    /// `colibri` with the program's source files appended, so it needs no `colibri` and no
    /// source files where it runs, and it runs like `colibri run` (the same output, exit
    /// codes and `os.args()`, every capability granted). Arguments given to it go to the
    /// program, never to `colibri`. The program is checked first; git dependencies are
    /// resolved as for `run`. It runs on the platform of this `colibri`, unless `--target` and
    /// `--runner` name another. With `--web` it writes a folder for a static web server
    /// instead (`index.html`, `colibri.js`, `colibri.wasm`, `app.pack`): the program runs in
    /// the page, which shows what it prints
    Build {
        /// A script, or a package folder (the current folder by default)
        file: Option<PathBuf>,
        /// Where to write the executable (by default the name of the script, or of the
        /// package folder, in the current folder); with `--web`, the folder (`NAME-web` by
        /// default)
        #[arg(short, long, value_name = "FILE")]
        output: Option<PathBuf>,
        /// Build for a web page instead of an executable: the program is checked against the
        /// standard library a page has (`os` is `"web"`; `fs`, `os`, `io`, `process`, `net`,
        /// `http` and `task.run` are errors), and the output is a folder to serve
        #[arg(long, conflicts_with_all = ["target", "runner"])]
        web: bool,
        /// With `--web`: use this `colibri.wasm` (or its gzip) as the page's runner instead of
        /// the one inside this `colibri`; for a runner just built from `crates/colibri-web`
        #[arg(long, value_name = "FILE", requires = "web")]
        web_runner: Option<PathBuf>,
        /// Build for another platform (linux, macos or windows, then x86_64 or aarch64:
        /// `linux-aarch64`). Needs `--runner`, and `@when(os == ..)` and `@when(arch == ..)`
        /// are decided for this platform
        #[arg(long, value_name = "PLATFORM")]
        target: Option<colibri_lang::platform::Target>,
        /// A `colibri` of the target's platform and of this version, to copy as the runner
        /// (get it from the release of that version). Needs `--target`. It is never downloaded
        #[arg(long, value_name = "FILE")]
        runner: Option<PathBuf>,
        #[command(flatten)]
        build: BuildFlags,
        #[command(flatten)]
        deps: DepFlags,
    },
    /// Type-check a Colibri script or package without running it
    Check {
        file: Option<PathBuf>,
        /// Check for a web page, as `colibri build --web` does: `os` is `"web"` and only the
        /// standard library of a page is there
        #[arg(long)]
        web: bool,
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
        /// Sample the running program and write a flame graph (SVG) to FILE (`profile.svg` by
        /// default) and the folded stacks it was drawn from next to it (`profile.folded`; a FILE without an extension gets `.svg`), then
        /// print the heaviest functions and lines to stderr. A sample is taken about every
        /// 1,024 steps, so a long call into a host function or a built-in is charged to the
        /// line that made the call; stacks deeper than 200 frames are cut in the middle; time the program spends waiting is `[idle]`; all the tests make
        /// one profile
        #[arg(long, value_name = "FILE", num_args = 0..=1, require_equals = true, default_missing_value = "profile.svg")]
        profile: Option<PathBuf>,
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
        profile_weight: colibri_lang::profilecmd::Weight,
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
        profile_rc: bool,
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
    /// Build the project runner for the `[rust-dependencies]` of a package (once, then cached;
    /// this is the one command besides `run`, `test` and the like that calls cargo) and write
    /// each binding's interface, `<key>.cbi`, next to the `colibri.toml` that declares the key.
    /// With `--check`, write nothing and exit with 1 if an interface is out of date
    Bindings {
        /// The package folder
        path: Option<PathBuf>,
        /// Build the runner as `colibri build` does (fat LTO) instead of for `colibri run`
        #[arg(long)]
        release: bool,
        /// Also copy the runner to FILE
        #[arg(short, long, value_name = "FILE")]
        output: Option<PathBuf>,
        /// Change nothing; exit with 1 if an interface would change
        #[arg(long)]
        check: bool,
        #[command(flatten)]
        deps: DepFlags,
    },
    /// Run the language server (LSP) on stdin and stdout
    Lsp,
    /// Debug adapter: speaks the Debug Adapter Protocol on standard input and output, for an
    /// editor to debug a Colibri script or package (breakpoints, stepping, pause, variables).
    /// The program, its arguments and its working folder come from the editor's `launch`
    /// request
    Debug,
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

/// The platform a web page is: `@when(os == "web")` and `arch == "wasm32"`.
fn web_platform(cfg: &mut colibri_runtime::BuildCfg) {
    cfg.os = colibri_runtime::WEB_OS.to_string();
    cfg.arch = "wasm32".to_string();
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

/// `COLIBRI_ENGINE` used to choose between the VM and the tree-walking interpreter. The
/// variable is ignored now; a script that still asks for the interpreter is told once.
fn warn_if_the_removed_engine_is_asked_for() {
    if std::env::var("COLIBRI_ENGINE").is_ok_and(|v| v == "tree") {
        eprintln!(
            "warning: COLIBRI_ENGINE=tree is ignored: the tree-walking interpreter was removed, and the VM runs the program"
        );
    }
}

/// An executable made by `colibri build` is a program, not the tool: it runs what it holds and
/// every argument is the program's.
fn run_built(pack: colibri_lang::pack::Pack, file: std::fs::File) -> ExitCode {
    colibri_lang::set_build_cfg(pack.cfg.clone());
    let args: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    on_big_stack(move || colibri_lang::runcmd::run_pack(pack, file, args))
}

fn main() -> ExitCode {
    // One seek and one read of 32 bytes tell whether this executable holds a program.
    match colibri_lang::pack::read_own() {
        Ok(None) => {}
        Ok(Some((pack, file))) => return run_built(pack, file),
        Err(message) => {
            eprintln!("error: {message}");
            return ExitCode::from(1);
        }
    }
    let cli = Cli::parse();
    warn_if_the_removed_engine_is_asked_for();
    // Every command but `new` runs on the big stack: parsing and checking recurse on the Rust
    // stack for deeply nested code, and `update` walks the dependency graph recursively (one
    // level per dependency in a chain, which a git dependency someone else controls can make
    // long). `new` only checks a name and writes a template.
    let command = match cli.command {
        Command::New { name } => return colibri_lang::newcmd::run(&name),
        other => other,
    };
    on_big_stack(move || match command {
        Command::Check {
            file,
            web,
            interfaces,
            build,
            deps,
        } => {
            let mut cfg = build.cfg();
            if web {
                web_platform(&mut cfg);
            }
            colibri_lang::set_build_cfg(cfg);
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
        Command::Build {
            file,
            output,
            web,
            web_runner,
            target,
            runner,
            build,
            deps,
        } => {
            let mut cfg = build.cfg();
            if web {
                web_platform(&mut cfg);
                colibri_lang::set_build_cfg(cfg);
                return colibri_lang::webbuild::build(&colibri_lang::webbuild::Options {
                    path: &file.unwrap_or_else(|| PathBuf::from(".")),
                    out: output.as_deref(),
                    deps,
                    runner: web_runner.as_deref(),
                });
            }
            // The files a program reads are chosen by `@when(os == ..)` while it is checked.
            if let Some(target) = target {
                cfg.os = target.os_name().to_string();
                cfg.arch = target.arch_name().to_string();
            }
            colibri_lang::set_build_cfg(cfg);
            let file = file.unwrap_or_else(|| PathBuf::from("."));
            colibri_lang::buildcmd::build(&colibri_lang::buildcmd::Options {
                path: &file,
                out: output.as_deref(),
                deps,
                target,
                runner: runner.as_deref(),
            })
        }
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
            colibri_lang::set_build_cfg(build.cfg());
            let file = file.unwrap_or_else(|| PathBuf::from("."));
            if watch {
                match depscmd::prepare(&file, deps) {
                    Ok(()) => colibri_lang::watch::run(&file, args),
                    Err(code) => code,
                }
            } else {
                let profile =
                    profile.map(|f| colibri_lang::profilecmd::Options::new(&f, profile_weight));
                colibri_lang::runcmd::run(&file, args, leaks, deps, profile.as_ref(), profile_rc)
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
            colibri_lang::set_build_cfg(build.cfg());
            colibri_lang::testcmd::run(&colibri_lang::testcmd::Options {
                path: file.unwrap_or_else(|| PathBuf::from(".")),
                filter,
                budget,
                nocapture,
                leaks,
                deps,
                profile: profile
                    .map(|f| colibri_lang::profilecmd::Options::new(&f, profile_weight)),
                profile_rc,
            })
        }
        Command::Update { names, path, deps } => depscmd::update(&path, names, deps),
        Command::New { .. } => unreachable!("`new` runs before the big stack is made"),
        Command::Fmt { paths, check } => colibri_lang::fmt::run(&paths, check),
        Command::Repl { build } => {
            colibri_lang::set_build_cfg(build.cfg());
            colibri_lang::repl::run()
        }
        Command::Bindings {
            path,
            release,
            output,
            check,
            deps,
        } => colibri_lang::bindings::command::run(&colibri_lang::bindings::command::Args {
            path: &path.unwrap_or_else(|| PathBuf::from(".")),
            release,
            output: output.as_deref(),
            check,
            deps,
        }),
        Command::Lsp => lsp(),
        Command::Debug => colibri_lang::debugcmd::run(),
        Command::Doc { std, check } => colibri_lang::doccmd::run(&std, check),
    })
}
