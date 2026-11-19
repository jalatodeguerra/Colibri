//! Finding the modules of a program: files, folders, `colibri.toml` and `import` resolution
//! (ARCHITECTURE §11). Nothing here type-checks; the result is what [`crate::check_modules`]
//! takes.

mod fs;
mod loader;
mod manifest;
pub mod toml;

use std::collections::HashMap;

use colibri_syntax::Diagnostic;
use colibri_syntax::ast::{self, NodeId};

pub use fs::{DiskFs, MemoryFs, SourceFs, normalize};
pub use loader::{Entry, LoadError, load};
pub use manifest::{Dependency, Manifest};

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
    /// The dotted path of the module in its tree (`game.player`; a dependency's name comes
    /// first). The entry module's name is the file stem.
    pub name: String,
    /// The package (folder) the module belongs to: an index into [`Loaded::packages`].
    pub package: usize,
    pub entry: bool,
    /// Each `import` item of the file, by its node id.
    pub imports: HashMap<NodeId, ImportTarget>,
}

/// A folder of modules: the unit of visibility.
#[derive(Clone, Debug)]
pub struct PackageNode {
    /// How messages call it: `game`, `foo.physics` (a dependency's folder), the package name
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

/// The standard library's modules. None exist yet; the import resolver already asks, so an
/// `import std.math` that matches one will be a candidate next to a local file or a dependency
/// of the same name (ARCHITECTURE §11.2).
pub fn std_module(_path: &[&str]) -> Option<String> {
    None
}
