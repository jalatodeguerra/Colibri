//! The Colibri standard library (DESIGN §13.1, ARCHITECTURE §13).
//!
//! Every module is a *named host module* implemented in Rust through the same machinery as
//! `vm.module(..)`: [`install`] registers `io`, `fs`, `path`, `os`, `math`, `rand`, `time`, `str`,
//! `json`, `regex`, `http`, `args`, `process`, `datetime` and `vec` into a [`Vm`], under the
//! interface names `std.io`, `std.fs`, ... The checker sees
//! them exactly as it sees an embedder's own host modules: from interface (`.cbi`) text generated
//! from the registrations ([`interfaces`] gives it to a checker that has no `Vm`). A script
//! writes `import math` (or `import std.math`, which always means the standard library).
//!
//! Modules that reach outside the program are tagged with a capability (`fs` needs
//! [`Cap::Fs`]; `http` needs [`Cap::Net`]; the environment, `exit`, `sleep`, reading stdin and the wall clock need
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
mod config;
mod datetime;
mod fs;
mod http;
mod io;
mod json;
mod math;
mod os;
mod path;
mod process;
mod rand;
mod re;
mod strs;
mod time;
mod values;

use colibri_runtime::{Cap, Error, Interface, Registration, Vm};

pub use config::{Clock, EnvSource, ManualClock, StdConfig, SystemClock};
pub use rand::Rng;

/// The modules of the standard library built so far, in the order they are registered.
pub const MODULES: &[&str] = &[
    "io", "fs", "path", "os", "math", "rand", "time", "str", "json", "regex", "http", "args",
    "process", "datetime", "vec",
];

/// Registers the standard library into `vm` with the default [`StdConfig`]. It must happen
/// before a program is loaded; calling it twice is an error (a module cannot be registered
/// twice). The modules are registered lazily, see [`Vm::lazy_std_module`].
pub fn install(vm: &mut Vm) -> Result<(), Error> {
    install_with(vm, StdConfig::default())
}

/// Like [`install`], with a [`StdConfig`] that decides what the world looks like to scripts.
pub fn install_with(vm: &mut Vm, config: StdConfig) -> Result<(), Error> {
    vm.insert_state(config::State::new(config));
    // Only the names are declared now: a module is registered (its interface generated,
    // parsed and checked) when a program imports it or something asks for it by name.
    type Register = fn(&mut Vm) -> Result<(), Error>;
    let lazy: [(&str, Register); 14] = [
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
        ("datetime", datetime::register),
    ];
    for (name, register) in lazy {
        vm.lazy_std_module(name, register);
    }
    colibri_runtime::install_vec(vm)?;
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
