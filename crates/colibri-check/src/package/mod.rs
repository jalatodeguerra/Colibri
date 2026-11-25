//! Finding the modules of a program: files, folders, `colibri.toml` and `import` resolution
//! (ARCHITECTURE §11). Nothing here type-checks; the result is what [`crate::check_modules`]
//! takes.

pub mod assets;
mod cfg;
mod fs;
// On the web the disk half of `git` (the cache, the `git` commands) is compiled but never
// reached: `cache_root`, `sync` and `DiskFs` refuse first.
#[cfg_attr(target_family = "wasm", allow(dead_code))]
mod git;
mod loader;
mod lock;
mod manifest;
#[cfg_attr(target_family = "wasm", allow(dead_code))]
mod sync;
pub mod toml;

use std::collections::HashMap;

use colibri_syntax::Diagnostic;
use colibri_syntax::ast::{self, NodeId};

pub use assets::{AssetPatterns, path_problem as asset_path_problem};
pub use cfg::{BuildCfg, Excluded, KNOWN_ARCH, KNOWN_OS};
pub use fs::{
    DiskFs, MemoryFs, Recorded, RecordingFs, SourceFs, canonical_path, normalize,
    try_canonical_path,
};
pub use git::{COMPLETE_MARKER, HOME_VAR, cache_root, checkout_dir, redact, redact_url};
pub use loader::{
    CACHE_TAG_FILE, Entry, LoadError, LoadOptions, MANIFEST_FILE, is_hidden_or_cache, load,
    load_with, not_a_script, not_a_script_plainly, skipped_folder,
};
pub use lock::{LOCK_FILE, Lock, LockEntry};
pub use manifest::parse as parse_manifest;
pub use manifest::{
    Build, Dependency, Features, GitSource, GitSpec, Manifest, Source, feature_name_problem,
    is_name as is_package_name,
};
pub use sync::{SyncOptions, SyncReport, Update, sync};

/// The extension of source files.
pub const SOURCE_EXT: &str = "colibri";

/// The file of a package that `colibri run` starts from, and a dependency's root module.
pub const MAIN_FILE: &str = "main.colibri";
pub const LIB_FILE: &str = "lib.colibri";

/// What an `import` item names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImportTarget {
    /// A source module: an index into [`Loaded::files`].
    Module(usize),
    /// A named host module (`vm.module("physics")`).
    Host(String),
    /// It could not be resolved; the loader reported why.
    Failed,
}

/// What the checker needs to know about a module besides its syntax tree.
#[derive(Clone, Debug)]
pub struct ModuleMeta {
    /// The dotted path of the module in its tree (`jobs.worker`; a dependency's name comes
    /// first). The entry module's name is the file stem.
    pub name: String,
    /// The package (folder) the module belongs to: an index into [`Loaded::packages`].
    pub package: usize,
    pub entry: bool,
    /// Not the entry, and no module imports it: a file of the package that is run on its own
    /// (an example, a tool). It may have top-level statements; they are checked, never run.
    pub script: bool,
    /// The module belongs to a dependency of the program, not to the program's own tree.
    pub dependency: bool,
    /// The package may contain `extern` blocks (DESIGN §15.3): its `colibri.toml` says
    /// `ffi = true`, or it is a script without a manifest (the host's `Cap::Ffi` still decides).
    pub ffi: bool,
    /// The module is a standard library module written in Colibri (`std.vec`), embedded in the
    /// toolchain: its file is virtual (`<std>/vec.colibri`), and the checker reports no
    /// warnings in it.
    pub std: bool,
    /// Each `import` item of the file, by its node id.
    pub imports: HashMap<NodeId, ImportTarget>,
    /// The top-level names a false `@when` left out of the module, for "left out here" hints.
    pub excluded: Vec<Excluded>,
}

/// A folder of modules: the unit of visibility.
#[derive(Clone, Debug)]
pub struct PackageNode {
    /// How messages call it: `jobs`, `foo.physics` (a dependency's folder), the package name
    /// for a tree's root folder.
    pub name: String,
}

/// A program found on disk (or in memory): every module reachable from the entry.
#[derive(Debug)]
pub struct Loaded {
    /// The syntax trees, in initialisation order: a module comes after the modules it imports
    /// (cycles broken at the back edge) and the entry is last.
    pub files: Vec<ast::File>,
    /// Parallel to `files`.
    pub metas: Vec<ModuleMeta>,
    pub packages: Vec<PackageNode>,
    /// Syntax errors, unresolved imports, cycles between packages and manifest problems.
    pub diagnostics: Vec<Diagnostic>,
    /// The problems among `diagnostics` that mean code or a module was left out of the
    /// program (a tool that must know every use of a name cannot trust it then).
    pub dropped: Vec<Dropped>,
}

/// Why part of a program is missing from a [`Loaded`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DroppedKind {
    /// A file has a syntax error: the parser dropped what it could not read.
    Syntax,
    /// An `import` matches more than one module, so it is bound to none.
    Ambiguous,
    /// A file an `import` names exists but cannot be read.
    Unreadable,
}

/// One problem that left code out of a [`Loaded`], with where it is reported.
#[derive(Clone, Debug)]
pub struct Dropped {
    pub kind: DroppedKind,
    pub span: colibri_syntax::Span,
    pub message: String,
}

impl Loaded {
    /// Whether anything went wrong that makes checking pointless.
    pub fn has_errors(&self) -> bool {
        self.diagnostics.iter().any(Diagnostic::is_error)
    }

    /// The modules as the checker takes them.
    pub fn inputs(&self) -> Vec<crate::ModuleInput<'_>> {
        self.files
            .iter()
            .zip(&self.metas)
            .map(|(file, meta)| crate::ModuleInput { file, meta })
            .collect()
    }
}

/// Standard library modules that do not exist yet, with the end of the sentence "the standard
/// library module `x` ..." that says why (DESIGN §13.1). Importing one says so instead of
/// a bare "no module". (`import test` cannot be written at all: `test` is a keyword.)
pub const LATER_STD_MODULES: &[(&str, &str)] = &[("task", "does not exist yet")];

/// Names of modules people expect from other languages, with what to do instead. `import
/// collections` is "no module", and the hint says why there is none.
pub const MISSING_STD_HINTS: &[(&str, &str)] = &[(
    "collections",
    "`List`, `Map` and `Set` are built in: use them without an import",
)];

/// The standard library module an import path names: `import math` and `import std.math` both
/// mean the module `std.math`, if the host provides it (`provided` are the interface names of
/// its modules; the `colibri-std` crate registers the standard ones). `None` when the path is
/// not one (ARCHITECTURE §13).
pub fn std_module(path: &[&str], provided: &[String]) -> Option<String> {
    let name = match path {
        ["std", name] => *name,
        [name] => *name,
        _ => return None,
    };
    let full = format!("{}{name}", crate::host::STD_PREFIX);
    provided.contains(&full).then_some(full)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn std_modules_are_found_by_short_name_or_with_the_prefix() {
        let provided = vec!["std.math".to_string(), "std.fs".to_string()];
        assert_eq!(
            std_module(&["math"], &provided).as_deref(),
            Some("std.math")
        );
        assert_eq!(
            std_module(&["std", "fs"], &provided).as_deref(),
            Some("std.fs")
        );
        assert_eq!(std_module(&["json"], &provided), None);
        assert_eq!(std_module(&["std", "json"], &provided), None);
        assert_eq!(std_module(&["std"], &provided), None);
        assert_eq!(std_module(&["a", "math"], &provided), None);
        assert_eq!(std_module(&["math"], &[]), None);
    }

    #[test]
    fn later_modules_are_not_also_provided_ones() {
        let provided = ["std.io", "std.fs", "std.path", "std.os", "std.math"];
        for (name, _) in LATER_STD_MODULES {
            assert!(
                !provided.contains(&format!("std.{name}").as_str()),
                "{name}"
            );
        }
    }
}
