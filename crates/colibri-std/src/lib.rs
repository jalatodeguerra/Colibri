//! The Colibri standard library (DESIGN §13.1, ARCHITECTURE §13).
//!
//! Every module but `vec` is a *named host module* implemented in Rust through the same machinery as
//! `vm.module(..)`: [`install`] registers `io`, `fs`, `path`, `os`, `math`, `rand`, `time`, `str`,
//! `json`, `regex`, `http`, `args`, `process`, `net`, `datetime`, `base64`, `hex`, `hash` and `csv` into a [`Vm`], under the
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
//! # // Host functions run only on the tree engine so far (`COLIBRI_ENGINE=vm`).
//! # if std::env::var("COLIBRI_ENGINE").is_ok_and(|v| v == "vm") { return Ok(()); }
//! let out = colibri::SharedBuffer::new();
//! let mut vm = colibri::Vm::builder().stdout(Box::new(out.clone())).build();
//! colibri_std::install(&mut vm)?;
//! vm.load_source("a.colibri", "import math\nprint(math.floor(math.PI * 100.0))\n")?;
//! assert_eq!(out.contents(), "314.0\n");
//! # Ok(())
//! # }
//! ```

mod args;
mod base64;
mod bytes;
mod config;
mod csv;
mod datetime;
mod errors;
mod fs;
mod fsfile;
mod hash;
mod hex;
mod http;
mod io;
mod json;
mod math;
mod net;
mod os;
mod path;
mod process;
mod rand;
mod re;
mod strs;
mod time;
mod tomls;
mod values;

use colibri_runtime::{Cap, Error, Interface, Registration, Vm};

pub use config::{Clock, EnvSource, ManualClock, StdConfig, SystemClock};
pub use rand::Rng;

/// The modules of the standard library built so far, in the order they are registered.
pub const MODULES: &[&str] = &[
    "io", "fs", "path", "os", "math", "rand", "time", "str", "json", "regex", "http", "args",
    "process", "net", "datetime", "base64", "hex", "hash", "csv", "toml",
];

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
    let lazy: [(&str, Register); 20] = [
        ("io", io::register),
        ("fs", fs::register),
        ("path", path::register),
        ("os", os::register),
        ("math", math::register),
        ("rand", rand::register),
        ("time", time::register),
        ("str", strs::register),
        ("json", json::register),
        ("regex", re::register),
        ("http", http::register),
        ("args", args::register),
        ("process", process::register),
        ("net", net::register),
        ("datetime", datetime::register),
        ("base64", base64::register),
        ("hex", hex::register),
        ("hash", hash::register),
        ("csv", csv::register),
        ("toml", tomls::register),
    ];
    for (name, register) in lazy {
        vm.lazy_std_module(name, register);
    }
    Ok(())
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
    let short = name.strip_prefix("std.").unwrap_or(name);
    let short = *MODULES.iter().find(|m| **m == short)?;
    let mut vm = Vm::builder().stdout(Box::new(std::io::sink())).build();
    install(&mut vm).expect("the standard library registers into an empty Vm");
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
