//! The Colibri standard library (DESIGN §13.1, ARCHITECTURE §13).
//!
//! Every module but `vec` is a *named host module* implemented in Rust through the same machinery as
//! `vm.module(..)`: [`install`] registers `io`, `fs`, `path`, `os`, `math`, `rand`, `time`, `str`,
//! `json`, `regex`, `http`, `args`, `process`, `net`, `datetime`, `base64`, `hex`, `hash`, `csv`, `toml` and `assets` into a [`Vm`], under the
//! interface names `std.io`, `std.fs`, ... The checker sees
//! them exactly as it sees an embedder's own host modules: from interface (`.cbi`) text generated
//! from the registrations ([`interfaces`] gives it to a checker that has no `Vm`). A script
//! writes `import math` (or `import std.math`, which always means the standard library).
//! `vec` is written in Colibri itself (`src/colibri/vec.colibri`) and loaded like a module of
//! the program, see [`sources`].
//!
//! Modules that reach outside the program are tagged with a capability (`fs` needs
//! [`Cap::Fs`]; `http` and `net` need [`Cap::Net`]; the environment, `exit`, `sleep`, reading stdin and the wall clock need
//! [`Cap::Os`]); a host that did not grant it gets a compile error at the use.
//!
//! Everything that depends on the world (the clock, the environment, the program's arguments,
//! standard input, the temporary folder, the default random seed) goes through [`StdConfig`],
//! so a host or a test can make a run hermetic.
//!
//! ```
//! use colibri_runtime as colibri;
//!
//! # fn main() -> Result<(), colibri::Error> {
//! let out = colibri::SharedBuffer::new();
//! let mut vm = colibri::Vm::builder().stdout(Box::new(out.clone())).build();
//! colibri_std::install(&mut vm)?;
//! vm.load_source("a.colibri", "import math\nprint(math.floor(math.PI * 100.0))\n")?;
//! assert_eq!(out.contents(), "314.0\n");
//! # Ok(())
//! # }
//! ```

mod args;
mod assets;
mod base64;
mod bytes;
mod config;
mod csv;
mod datetime;
mod errors;
#[cfg(not(target_family = "wasm"))]
mod fs;
#[cfg(not(target_family = "wasm"))]
mod fsfile;
mod hash;
mod hex;
#[cfg(not(target_family = "wasm"))]
mod http;
#[cfg(not(target_family = "wasm"))]
mod io;
mod json;
mod math;
#[cfg(not(target_family = "wasm"))]
mod net;
#[cfg(not(target_family = "wasm"))]
mod os;
mod path;
#[cfg(not(target_family = "wasm"))]
mod process;
mod rand;
mod re;
mod strs;
mod time;
mod tomls;
mod values;

use colibri_runtime::{Cap, Error, Interface, Registration, Vm};

pub use assets::{
    AssetError, AssetSource, NOT_A_FOLDER, NOT_AN_ASSET, PackAssets, check_dir, children, for_pack,
    io_reason,
};
#[cfg(target_family = "wasm")]
pub use config::seed_table_hash;
pub use config::{Clock, EnvSource, Factory, ManualClock, StdConfig, SystemClock, WorkerStd};
pub use rand::Rng;

/// The modules of the standard library built so far, in the order they are registered.
#[cfg(not(target_family = "wasm"))]
pub const MODULES: &[&str] = &[
    "io", "fs", "path", "os", "math", "rand", "time", "str", "json", "regex", "http", "args",
    "process", "net", "datetime", "base64", "hex", "hash", "csv", "toml", "assets",
];

/// The modules of a web build (`os == "web"`, and the only ones the web runner is built with):
/// the rest reach the machine, and importing one is the compile error "`fs` is not available in a
/// web build" (`colibri_runtime::WEB_UNAVAILABLE_STD`). `vec` is Colibri source, in [`sources`].
pub const WEB_MODULES: &[&str] = &[
    "path", "math", "rand", "time", "str", "json", "regex", "args", "datetime", "base64", "hex",
    "hash", "csv", "toml", "assets",
];

/// On the web (`target_family = "wasm"`) the modules that reach the machine are not built:
/// `io`, `fs`, `os`, `http`, `process` and `net`.
#[cfg(target_family = "wasm")]
pub const MODULES: &[&str] = WEB_MODULES;

/// The text of the `vec` module (`import vec`): Colibri source, not a host module.
const VEC: &str = include_str!("colibri/vec.colibri");

/// The text of the `httpd` module (`import httpd`): an HTTP/1.1 server written in Colibri.
const HTTPD: &str = include_str!("colibri/httpd.colibri");

/// The standard library modules that are written in Colibri and embedded in the toolchain, for
/// the tools that check a program without a [`Vm`] (`colibri check`, the language server). A
/// [`Vm`] gets them from [`install`]: `vec` and `httpd`.
pub fn sources() -> Vec<colibri_runtime::SourceModule> {
    vec![
        colibri_runtime::SourceModule {
            name: "vec",
            text: VEC,
        },
        colibri_runtime::SourceModule {
            name: "httpd",
            text: HTTPD,
        },
    ]
}

/// Gives scripts `import vec` in a [`Vm`] that does not use the rest of the standard library:
/// registers the Colibri source of the `vec` module, and declares `math` (which it imports, for
/// `from_angle` and `angle`). It must happen before a program is loaded. Do not call it after
/// [`install`], which does the same.
pub fn install_vec(vm: &mut Vm) -> Result<(), Error> {
    vm.std_source_module("vec", VEC);
    vm.lazy_std_module("math", math::register);
    vm.no_await_module("math", &[]);
    Ok(())
}

/// Registers the standard library into `vm` with the default [`StdConfig`]. It must happen
/// before a program is loaded; calling it twice is an error (a module cannot be registered
/// twice). The modules are registered lazily, see [`Vm::lazy_std_module`].
pub fn install(vm: &mut Vm) -> Result<(), Error> {
    install_with(vm, StdConfig::default())
}

/// Like [`install`], with a [`StdConfig`] that decides what the world looks like to scripts.
pub fn install_with(vm: &mut Vm, config: StdConfig) -> Result<(), Error> {
    if vm.state_mut::<config::State>().is_some() {
        return Err(Error::Registration {
            name: "std".to_string(),
            message: "the standard library is already installed".to_string(),
        });
    }
    vm.insert_state(config::State::new(config));
    for source in sources() {
        vm.std_source_module(source.name, source.text);
    }
    // Only the names are declared now: a module is registered (its interface generated,
    // parsed and checked) when a program imports it or something asks for it by name.
    type Register = fn(&mut Vm) -> Result<(), Error>;
    // The registration order is the order of `MODULES`; the web build leaves out the modules
    // that reach the machine.
    let lazy: Vec<(&str, Register)> = vec![
        #[cfg(not(target_family = "wasm"))]
        ("io", io::register),
        #[cfg(not(target_family = "wasm"))]
        ("fs", fs::register),
        ("path", path::register),
        #[cfg(not(target_family = "wasm"))]
        ("os", os::register),
        ("math", math::register),
        ("rand", rand::register),
        ("time", time::register),
        ("str", strs::register),
        ("json", json::register),
        ("regex", re::register),
        #[cfg(not(target_family = "wasm"))]
        ("http", http::register),
        ("args", args::register),
        #[cfg(not(target_family = "wasm"))]
        ("process", process::register),
        #[cfg(not(target_family = "wasm"))]
        ("net", net::register),
        ("datetime", datetime::register),
        ("base64", base64::register),
        ("hex", hex::register),
        ("hash", hash::register),
        ("csv", csv::register),
        ("toml", tomls::register),
        ("assets", assets::register),
    ];
    for (name, register) in lazy {
        vm.lazy_std_module(name, register);
    }
    declare_no_await(vm);
    Ok(())
}

/// Tells the VM which functions never wait for anything outside the program, so that a
/// coroutine may call them (a function that may wait is refused beside running coroutines until
/// the VM can suspend one for a wait). Everything of `net`, `process` and `http` may wait, and
/// so may `time.sleep` and the file handles of `fs`; the rest is pure.
fn declare_no_await(vm: &mut Vm) {
    for module in [
        "io", "path", "os", "math", "rand", "str", "json", "regex", "args", "datetime", "base64",
        "hex", "hash", "csv", "toml", "assets",
    ] {
        vm.no_await_module(module, &[]);
    }
    vm.no_await_module("time", &["sleep"]);
    vm.no_await_module("fs", &["open", "create", "open_append", "File.*"]);
}

/// The interface files (`.cbi`) of the standard library, for `colibri check` and language
/// servers: pass them in `CheckConfig::interfaces` and a source that imports `math` checks
/// without a `Vm`. Generated from the same registrations as [`install`].
pub fn interfaces() -> Vec<Interface> {
    let mut vm = Vm::builder().stdout(Box::new(std::io::sink())).build();
    install(&mut vm).expect("the standard library registers into an empty Vm");
    MODULES
        .iter()
        .map(|name| {
            let module = vm.std_module(name);
            let text = module.interface_text();
            Interface::from_text(&format!("std.{name}"), &text)
                .expect("the standard library's own interface text parses")
        })
        .collect()
}

/// The interface of the standard module `name` (`std.math` or `math`), or `None` if there is
/// no such module. Only that module is registered and rendered: what [`interfaces`] does for
/// all of them.
pub fn interface(name: &str) -> Option<Interface> {
    interface_for(name, false)
}

/// [`interface`] for a web build: what a program for a page is checked against. A module a page
/// does not have is `None` (the loader says so before it asks), and `time` has no `sleep`.
pub fn web_interface(name: &str) -> Option<Interface> {
    interface_for(name, true)
}

fn interface_for(name: &str, web: bool) -> Option<Interface> {
    let short = name.strip_prefix("std.").unwrap_or(name);
    let short = *MODULES.iter().find(|m| **m == short)?;
    if web && !WEB_MODULES.contains(&short) {
        return None;
    }
    let mut vm = Vm::builder().stdout(Box::new(std::io::sink())).build();
    let config = StdConfig {
        web,
        ..StdConfig::default()
    };
    install_with(&mut vm, config).expect("the standard library registers into an empty Vm");
    let text = vm.std_module(short).interface_text();
    Some(
        Interface::from_text(&format!("std.{short}"), &text)
            .expect("the standard library's own interface text parses"),
    )
}

/// The code a script passed to `os.exit` in this `Vm`, if it did. A script that exits ends
/// with an [`Error::Panic`] whose message is "the program exited with code N" (its `defer`s
/// and `Drop` hooks have run); the `colibri` tool turns that into the process exit code.
pub fn exit_code(vm: &mut Vm) -> Option<i32> {
    vm.state_mut::<config::State>().and_then(|s| s.exit)
}

/// Makes `io.eprint` and `io.eprintln` write to `writer` instead of the process's standard
/// error (a debug adapter, whose own standard output is the protocol, turns them into events).
pub fn set_stderr(vm: &mut Vm, writer: Box<dyn std::io::Write>) {
    if let Some(state) = vm.state_mut::<config::State>() {
        state.stderr = Some(writer);
    }
}

/// What a host's `worker_setup` calls on the `Vm` of each worker thread: [`install_with`] with the
/// configuration `worker` makes for it, and its `io.eprint` target when the host set one.
pub fn install_worker(vm: &mut Vm, worker: &WorkerStd) -> Result<(), Error> {
    install_with(vm, worker.config())?;
    if let Some(stderr) = worker.stderr() {
        set_stderr(vm, stderr);
    }
    // A task has a world of its own, made from its serial number and the configuration, as on a
    // thread of its own: a generator seeded from the serial (its numbers do not depend on which
    // worker runs it or on what that worker ran before) and a clock made afresh. What was there
    // comes back when the task ends (a task that runs inline in a join leaves its joiner's).
    let main = worker.seed();
    let make_clock = worker.clock_factory();
    vm.task_scope::<config::State>(move |state, serial| {
        let clock = std::mem::replace(&mut state.clock, make_clock());
        let rng = main.map(|main| {
            std::mem::replace(
                &mut state.rng,
                rand::Rng::from_seed(config::task_seed(main, serial)),
            )
        });
        Box::new(move |state| {
            state.clock = clock;
            if let Some(rng) = rng {
                state.rng = rng;
            }
        })
    });
    Ok(())
}

/// Tags a registration with a capability.
pub(crate) fn needs(cap: Cap, r: Result<Registration<'_>, Error>) -> Result<(), Error> {
    r?.requires(cap);
    Ok(())
}

/// Registers a function with no capability.
pub(crate) fn plain(r: Result<Registration<'_>, Error>) -> Result<(), Error> {
    r.map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every standard module is either in the web build or refused there by name (the loader's
    /// list is `WEB_UNAVAILABLE_STD`, which `colibri-check` cannot derive from this crate).
    #[test]
    fn every_standard_module_is_in_the_web_build_or_refused_there() {
        let refused = colibri_runtime::WEB_UNAVAILABLE_STD;
        let everything = MODULES
            .iter()
            .copied()
            .chain(sources().into_iter().map(|s| s.name));
        for name in everything {
            let in_web = WEB_MODULES.contains(&name) || name == "vec";
            assert!(
                in_web != refused.contains(&name),
                "`{name}` must be in exactly one of the web build's modules and WEB_UNAVAILABLE_STD"
            );
        }
        for name in WEB_MODULES.iter().chain(refused) {
            assert!(
                MODULES.contains(name) || sources().iter().any(|s| s.name == *name),
                "`{name}` is no standard module"
            );
        }
    }

    /// Each embedded source module is checked as an ordinary program here, because a std source
    /// module has its warnings dropped: it must have none to drop.
    #[test]
    fn every_source_module_checks_with_no_warnings() {
        let config = colibri_runtime::CheckConfig {
            interfaces: interfaces(),
            // A source module may use any capability; its program asks for the ones it needs.
            caps: colibri_runtime::CapSet::all(),
            ..colibri_runtime::CheckConfig::default()
        };
        for source in sources() {
            let file = format!("{}.colibri", source.name);
            let report = colibri_runtime::check_source_with(&file, source.text, &config);
            assert!(
                report.diagnostics.is_empty(),
                "{file}:\n{}",
                colibri_runtime::render_diagnostics(&report.diagnostics, &report.sources, false)
            );
        }
    }

    #[test]
    fn every_source_module_is_formatted() {
        for source in sources() {
            let file = format!("{}.colibri", source.name);
            let formatted = colibri_syntax::fmt::format_source(&file, source.text).unwrap();
            assert_eq!(formatted, source.text, "run `colibri fmt` on {file}");
        }
    }

    #[test]
    fn every_source_module_is_named_without_the_prefix_and_imports_resolve() {
        for source in sources() {
            assert!(!source.name.contains('.'), "{}", source.name);
            assert!(!MODULES.contains(&source.name), "{}", source.name);
            // An import inside the library names the standard library only, so no module of
            // the embedder or of the program's folder can take part.
            for line in source.text.lines().filter(|l| l.starts_with("import ")) {
                assert!(
                    line.starts_with("import std."),
                    "{}: `{line}` must name `std.<module>`",
                    source.name
                );
                // ... and a module that exists: a host module, or another source module.
                let named = line["import std.".len()..]
                    .split([' ', '.'])
                    .next()
                    .unwrap_or_default();
                assert!(
                    MODULES.contains(&named) || sources().iter().any(|s| s.name == named),
                    "{}: `{line}` names a module the standard library does not have",
                    source.name
                );
            }
        }
    }
}
