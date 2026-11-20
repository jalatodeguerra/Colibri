//! Loading a program: the trees (a root folder and its dependencies), the modules reachable by
//! `import` from the entry, the packages they form and the order they initialise in.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

use colibri_syntax::ast::{self, ItemKind, NodeId, TopLevel};
use colibri_syntax::{Diagnostic, FileId, SourceMap, Span, parse_file};

use super::fs::{SourceFs, leaves_repository, normalize};
use super::git;
use super::lock::{self, LOCK_FILE, Lock};
use super::manifest::{self, GitSource, Manifest, Source};
use super::sync::inside;
use super::toml;
use super::{
    Dropped, DroppedKind, ImportTarget, LATER_STD_MODULES, LIB_FILE, Loaded, MAIN_FILE, ModuleMeta,
    PackageNode, SOURCE_EXT, std_module,
};

/// The manifest's file name.
pub const MANIFEST_FILE: &str = "colibri.toml";

/// Where a program starts.
#[derive(Clone, Copy, Debug)]
pub enum Entry<'a> {
    /// A script: its folder is the tree root (and the implicit package); a `colibri.toml` in
    /// that folder is read for its name and dependencies, but is not required.
    File(&'a Path),
    /// A package: a folder with a `colibri.toml`, started from its `main.colibri`.
    Package(&'a Path),
}

/// The program cannot be loaded at all (no diagnostic has a place to point to).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadError {
    pub path: String,
    pub message: String,
}

/// A root folder, its name and the trees of its dependencies.
struct Tree {
    root: PathBuf,
    name: String,
    /// A dependency of another tree: its modules are named after it.
    is_dep: bool,
    manifest_name: Option<String>,
    /// The manifest allows `extern` blocks (or there is none: a script).
    ffi: bool,
    /// The package comes from a git checkout (itself, or a path dependency inside one).
    from_git: bool,
    /// `allow-ffi` of the manifest: only the root's counts.
    allow_ffi: Vec<String>,
    /// Where each name of `allow_ffi` is written, with the manifest's file.
    allow_ffi_spans: Vec<Span>,
    deps: Vec<(String, usize)>,
}

struct Disc {
    file: ast::File,
    name: String,
    tree: usize,
    package: usize,
    entry: bool,
    imports: Vec<(NodeId, ImportTarget)>,
}

/// One way an import could be satisfied.
enum Candidate {
    Local {
        file: PathBuf,
        tree: usize,
        rel: PathBuf,
    },
    Dependency {
        name: String,
        file: PathBuf,
        tree: usize,
        rel: PathBuf,
    },
    Host(String),
    Std(String),
}

impl Candidate {
    fn describe(&self, fs: &dyn SourceFs) -> String {
        match self {
            Candidate::Local { file, .. } => format!("the file `{}`", fs.display_name(file)),
            Candidate::Dependency { name, file, .. } => {
                format!("the dependency `{name}` (`{}`)", fs.display_name(file))
            }
            Candidate::Host(name) => format!("the host module `{name}`"),
            Candidate::Std(name) => format!("the standard library module `{name}`"),
        }
    }
}

struct Loader<'a> {
    fs: &'a dyn SourceFs,
    sources: &'a mut SourceMap,
    host_modules: &'a [String],
    trees: Vec<Tree>,
    trees_by_dir: HashMap<PathBuf, usize>,
    discs: Vec<Option<Disc>>,
    by_path: HashMap<PathBuf, usize>,
    pending: VecDeque<usize>,
    packages: Vec<PackageNode>,
    package_keys: HashMap<(usize, PathBuf), usize>,
    diags: Vec<Diagnostic>,
    dropped: Vec<Dropped>,
    /// `colibri.lock` of the root package, read when the first git dependency needs it.
    lock: Option<Lock>,
    lock_read: bool,
    /// Pairs of trees already reported as one name twice.
    clashes: HashSet<(usize, usize)>,
    /// Trees already reported for `ffi = true` without the root's `allow-ffi`.
    ffi_reported: HashSet<usize>,
}

/// The file that marks a folder as a cache (the standard marker, written by Cargo's `target`
/// and by many other tools): what is in it is not source.
pub const CACHE_TAG_FILE: &str = "CACHEDIR.TAG";

/// What a `CACHEDIR.TAG` starts with (the Cache Directory Tagging Specification): a file of
/// that name that does not start with it is not a tag.
pub const CACHE_TAG_SIGNATURE: &str = "Signature: 8a477f597d28d172789f06886806bc55";

/// Whether a folder is no part of the source tree on its own: its name starts with `.`, or it
/// holds a `CACHEDIR.TAG` that starts with the standard signature (only its first 43 bytes are
/// read). `target` and `node_modules` are ordinary folders to Colibri.
pub fn is_hidden_or_cache(fs: &dyn SourceFs, name: &str, path: &Path) -> bool {
    name.starts_with('.') || has_cache_tag(fs, path)
}

fn has_cache_tag(fs: &dyn SourceFs, dir: &Path) -> bool {
    let tag = dir.join(CACHE_TAG_FILE);
    fs.is_file(&tag)
        && fs
            .read_prefix(&tag, CACHE_TAG_SIGNATURE.len())
            .is_some_and(|head| head == CACHE_TAG_SIGNATURE.as_bytes())
}

/// Whether the walk over the files of a package (every module of `colibri check <folder>`, and
/// what the language server reads) leaves a folder out: [`is_hidden_or_cache`], or a
/// `colibri.toml` of its own (another package). An explicit `import` does not look at this.
pub fn skipped_folder(fs: &dyn SourceFs, name: &str, path: &Path) -> bool {
    is_hidden_or_cache(fs, name, path) || fs.is_file(&path.join(MANIFEST_FILE))
}

/// What else `load` may do.
#[derive(Clone, Copy, Debug, Default)]
pub struct LoadOptions {
    /// For a package: load every `.colibri` file under its root, not only the modules the
    /// entry reaches by `import` (what `colibri test` needs to find every `test` block).
    /// Folders that [`skipped_folder`] names are not walked (an `import` still reaches into them).
    /// Ignored for a script.
    pub all_modules: bool,
}

/// Loads the program that starts at `entry`, reading through `fs` and adding every file it
/// reads to `sources` (so spans name their files). `host_modules` are the named host modules
/// `import` may resolve to.
pub fn load(
    fs: &dyn SourceFs,
    sources: &mut SourceMap,
    entry: Entry<'_>,
    host_modules: &[String],
) -> Result<Loaded, LoadError> {
    load_with(fs, sources, entry, host_modules, LoadOptions::default())
}

/// What to say when `path` names the manifest or the lock file of a package, which is never
/// Colibri source (its text would be parsed and printed as such): `None` for any other file.
/// The names are compared without regard to case, as the file systems that ignore it do.
pub fn not_a_script(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let (file, what) = if name.eq_ignore_ascii_case(MANIFEST_FILE) {
        (MANIFEST_FILE, "manifest")
    } else if name.eq_ignore_ascii_case(super::lock::LOCK_FILE) {
        (super::lock::LOCK_FILE, "lock file")
    } else {
        return None;
    };
    Some(format!(
        "`{file}` is the {what} of a package, not a script: pass the folder that holds it instead"
    ))
}

/// [`load`] with options.
pub fn load_with(
    fs: &dyn SourceFs,
    sources: &mut SourceMap,
    entry: Entry<'_>,
    host_modules: &[String],
    options: LoadOptions,
) -> Result<Loaded, LoadError> {
    let mut loader = Loader::new(fs, sources, host_modules);
    let (root, file, require_manifest) = match entry {
        Entry::File(path) => {
            if let Some(message) = not_a_script(path) {
                return Err(LoadError {
                    path: fs.display_name(path),
                    message,
                });
            }
            let root = path.parent().map(Path::to_path_buf).unwrap_or_default();
            let name = path
                .file_name()
                .map(PathBuf::from)
                .ok_or_else(|| LoadError {
                    path: fs.display_name(path),
                    message: "this is not a file".to_string(),
                })?;
            (root, name, false)
        }
        Entry::Package(dir) => (dir.to_path_buf(), PathBuf::from(MAIN_FILE), true),
    };
    if require_manifest && !fs.is_dir(&root) {
        return Err(LoadError {
            path: fs.display_name(&root),
            message: "no such folder".to_string(),
        });
    }
    let tree = loader
        .load_tree(&root, require_manifest, false, None)
        .map_err(|message| LoadError {
            path: fs.display_name(&root.join(MANIFEST_FILE)),
            message,
        })?;
    loader.warn_unknown_allow_ffi();
    let entry_path = match entry {
        Entry::File(path) => path.to_path_buf(),
        Entry::Package(_) => root.join(&file),
    };
    if matches!(entry, Entry::Package(_)) && !fs.is_file(&entry_path) {
        return Err(LoadError {
            path: fs.display_name(&entry_path),
            message: "the package has no `main.colibri` to start from".to_string(),
        });
    }
    let text = fs.read(&entry_path).map_err(|message| LoadError {
        path: fs.display_name(&entry_path),
        message,
    })?;
    let entry_id = loader.add_module(&entry_path, text, tree, &file, true);
    loader.pending.push_back(entry_id);
    while let Some(id) = loader.pending.pop_front() {
        loader.resolve_imports(id);
    }
    if options.all_modules && matches!(entry, Entry::Package(_)) {
        loader.add_tree_modules(tree, &root, Path::new(""));
        while let Some(id) = loader.pending.pop_front() {
            loader.resolve_imports(id);
        }
    }
    Ok(loader.finish(entry_id))
}

impl<'a> Loader<'a> {
    fn new(fs: &'a dyn SourceFs, sources: &'a mut SourceMap, host_modules: &'a [String]) -> Self {
        Loader {
            fs,
            sources,
            host_modules,
            trees: Vec::new(),
            trees_by_dir: HashMap::new(),
            discs: Vec::new(),
            by_path: HashMap::new(),
            pending: VecDeque::new(),
            packages: Vec::new(),
            package_keys: HashMap::new(),
            diags: Vec::new(),
            dropped: Vec::new(),
            lock: None,
            lock_read: false,
            clashes: HashSet::new(),
            ffi_reported: HashSet::new(),
        }
    }

    /// Reads the manifest of the tree rooted at `dir` (and, recursively, its dependencies).
    /// `Err` is a message about the folder itself.
    /// Reads a manifest or a lock file, `text`, through [`toml::read_config`] and puts what
    /// it says to show in the source map: every snippet and printed line of the file comes
    /// from that text, never from `text`.
    fn read_config_file(
        &mut self,
        path: &Path,
        text: &str,
        name: &str,
    ) -> (FileId, Result<Vec<toml::Table>, Vec<Diagnostic>>) {
        let file = self.sources.next_file_id();
        let config = toml::read_config(file, text, name);
        let added = self
            .sources
            .add_file(self.fs.display_name(path), config.display);
        debug_assert_eq!(added, file);
        (file, config.tables)
    }

    fn load_tree(
        &mut self,
        dir: &Path,
        require_manifest: bool,
        is_dep: bool,
        git_root: Option<PathBuf>,
    ) -> Result<usize, String> {
        let key = normalize(dir);
        if let Some(&t) = self.trees_by_dir.get(&key) {
            return Ok(t);
        }
        let manifest_path = dir.join(MANIFEST_FILE);
        let mut manifest: Option<(Manifest, FileId)> = None;
        if self.fs.is_file(&manifest_path) {
            let text = self
                .fs
                .read(&manifest_path)
                .map_err(|e| format!("cannot read it: {e}"))?;
            let (file, tables) = self.read_config_file(&manifest_path, &text, MANIFEST_FILE);
            match manifest::from_tables(file, tables) {
                Ok(m) => manifest = Some((m, file)),
                Err(diags) => self.diags.extend(diags),
            }
        } else if require_manifest {
            return Err(format!(
                "there is no `{MANIFEST_FILE}` in `{}`",
                display_dir(self.fs, dir)
            ));
        }
        let folder_name = key
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| "main".to_string());
        let tree = self.trees.len();
        self.trees.push(Tree {
            root: dir.to_path_buf(),
            name: manifest
                .as_ref()
                .map_or(folder_name, |(m, _)| m.name.clone()),
            is_dep,
            manifest_name: manifest.as_ref().map(|(m, _)| m.name.clone()),
            // A script with no manifest is the program itself: its host decides.
            ffi: manifest.as_ref().is_none_or(|(m, _)| m.ffi),
            from_git: git_root.is_some(),
            allow_ffi: manifest
                .as_ref()
                .map(|(m, _)| m.allow_ffi.clone())
                .unwrap_or_default(),
            allow_ffi_spans: manifest
                .as_ref()
                .map(|(m, _)| m.allow_ffi_spans.clone())
                .unwrap_or_default(),
            deps: Vec::new(),
        });
        self.trees_by_dir.insert(key, tree);
        if let Some((m, _)) = &manifest {
            for dep in &m.dependencies {
                let (dep_dir, dep_root, span) = match &dep.source {
                    Source::Path { path, path_span } => {
                        let dep_dir = normalize(&dir.join(path));
                        // A package from git is judged by the text of its path first: no
                        // folder outside the repository is looked at (not even to see
                        // whether it is there) when its manifest names one.
                        if let Some(root) = &git_root
                            && leaves_repository(root, dir, path)
                        {
                            self.diags.push(leaves_repository_diagnostic(
                                self.fs, &dep.name, &dep_dir, *path_span,
                            ));
                            continue;
                        }
                        if !self.fs.is_dir(&dep_dir) {
                            self.diags.push(
                                Diagnostic::error(
                                    format!(
                                        "the dependency `{}` is not at `{}`: no such folder",
                                        dep.name,
                                        display_dir(self.fs, &dep_dir)
                                    ),
                                    *path_span,
                                )
                                .with_hint(
                                    "`path` is relative to the folder of this `colibri.toml`",
                                ),
                            );
                            continue;
                        }
                        if let Some(root) = &git_root
                            && !inside(&dep_dir, root)
                        {
                            self.diags.push(leaves_repository_diagnostic(
                                self.fs, &dep.name, &dep_dir, *path_span,
                            ));
                            continue;
                        }
                        (dep_dir, git_root.clone(), *path_span)
                    }
                    Source::Git(source) => match self.git_checkout(&dep.name, source) {
                        Ok(dep_dir) => (dep_dir.clone(), Some(dep_dir), source.url_span),
                        Err(diag) => {
                            self.diags.push(diag);
                            continue;
                        }
                    },
                };
                match self.load_tree(&dep_dir, true, true, dep_root) {
                    Ok(t) => {
                        if self.trees[t]
                            .manifest_name
                            .as_deref()
                            .is_some_and(|n| n != dep.name)
                        {
                            let found = self.trees[t].manifest_name.clone().unwrap_or_default();
                            self.diags.push(
                                Diagnostic::error(
                                    format!(
                                        "the dependency `{}` is called `{found}` in its own `{MANIFEST_FILE}`",
                                        dep.name
                                    ),
                                    dep.name_span,
                                )
                                .with_hint("the name before `=` and the package `name` must agree"),
                            );
                        }
                        self.check_ffi_allowed(tree, t, dep.name_span);
                        self.report_clash(t, dep.name_span);
                        self.trees[tree].deps.push((dep.name.clone(), t));
                    }
                    Err(message) => self.diags.push(Diagnostic::error(
                        format!("the dependency `{}`: {message}", dep.name),
                        span,
                    )),
                }
            }
        }
        Ok(tree)
    }

    /// A package from git that declares `ffi = true` must be listed in the root package's
    /// `allow-ffi` (DESIGN §15.3): FFI is a capability, and a dependency the user never read
    /// (a transitive one) must not be able to link C code just by saying so itself.
    fn check_ffi_allowed(&mut self, dependent: usize, t: usize, span: Span) {
        if !self.trees[t].from_git
            || !self.trees[t].ffi
            || self.trees[t].manifest_name.is_none()
            || !self.ffi_reported.insert(t)
        {
            return;
        }
        let name = self.trees[t].name.clone();
        if self.trees[0].allow_ffi.contains(&name) {
            return;
        }
        let root = &self.trees[0];
        let root_manifest = root.root.join(MANIFEST_FILE);
        self.diags.push(
            Diagnostic::error(
                format!(
                    "the git dependency `{name}` declares `ffi = true` (it may link C code), and the package `{}` has not allowed it; it is needed by `{}`",
                    root.name, self.trees[dependent].name
                ),
                span,
            )
            .with_hint(format!(
                "to allow it, add `allow-ffi = [\"{name}\"]` to the `[package]` table of `{}`",
                self.fs.display_name(&root_manifest)
            )),
        );
    }

    /// A name in the root package's `allow-ffi` that is no dependency (direct or transitive)
    /// allows nothing: most likely a misspelling, which would leave the real one refused.
    fn warn_unknown_allow_ffi(&mut self) {
        let Some(root) = self.trees.first() else {
            return;
        };
        let unknown: Vec<(String, Span)> = root
            .allow_ffi
            .iter()
            .zip(&root.allow_ffi_spans)
            .filter(|(name, _)| !self.trees.iter().any(|t| t.is_dep && t.name == **name))
            .map(|(n, s)| (n.clone(), *s))
            .collect();
        for (name, span) in unknown {
            self.diags.push(
                Diagnostic::warning(
                    format!(
                        "`allow-ffi` lists `{name}`, which is not a dependency of this package"
                    ),
                    span,
                )
                .with_hint(
                    "check the spelling: the name is the one before `=` in `[dependencies]`",
                ),
            );
        }
    }

    /// Two different packages with one name cannot both be imported as that name: say so.
    fn report_clash(&mut self, t: usize, span: Span) {
        let name = self.trees[t].name.clone();
        let Some(other) = (0..t).find(|&o| {
            self.trees[o].is_dep
                && self.trees[o].name == name
                && self.fs.canonical(&self.trees[o].root) != self.fs.canonical(&self.trees[t].root)
        }) else {
            return;
        };
        if !self.clashes.insert((other, t)) {
            return;
        }
        self.diags.push(
            Diagnostic::error(
                format!(
                    "two different packages are both called `{name}`: `{}` and `{}`",
                    display_dir(self.fs, &self.trees[other].root),
                    display_dir(self.fs, &self.trees[t].root)
                ),
                span,
            )
            .with_hint(
                "a build can have one version of a package; make the dependencies agree on one",
            ),
        );
    }

    /// The folder of the pinned checkout of a git dependency; never fetches.
    fn git_checkout(&mut self, name: &str, source: &GitSource) -> Result<PathBuf, Diagnostic> {
        let span = source.url_span;
        let fetch_hint = "run `colibri check` or `colibri update` in a terminal to fetch it (editors and the language server never use the network)";
        if !self.lock_read {
            self.lock_read = true;
            let root = self
                .trees
                .first()
                .map(|t| t.root.clone())
                .unwrap_or_default();
            let path = root.join(LOCK_FILE);
            if self.fs.is_file(&path) {
                match self.fs.read(&path) {
                    Ok(text) => {
                        let (_, tables) = self.read_config_file(&path, &text, LOCK_FILE);
                        match Lock::from_tables(tables) {
                            Ok(lock) => self.lock = Some(lock),
                            Err(d) => {
                                let hint = match &d.hint {
                                    Some(h) => format!("{h}; {}", lock::RECREATE_HINT),
                                    None => lock::RECREATE_HINT.to_string(),
                                };
                                self.diags.push(d.with_hint(hint));
                            }
                        }
                    }
                    Err(e) => self.diags.push(
                        Diagnostic::error(
                            format!("cannot read `{}`: {e}", self.fs.display_name(&path)),
                            span,
                        )
                        .with_hint(lock::RECREATE_HINT),
                    ),
                }
            }
        }
        let entry = match self.lock.as_ref().and_then(|l| l.entries.get(name)) {
            Some(e) if e.matches(source) => e.clone(),
            Some(e) => {
                let asked = source.spec.describe();
                return Err(Diagnostic::error(
                    format!(
                        "`{LOCK_FILE}` is out of date for the dependency `{name}`: it was locked for `{}` ({}), and `colibri.toml` now says `{}` ({asked})",
                        git::redact_url(&e.url),
                        e.spec.describe(),
                        git::redact_url(&source.url)
                    ),
                    span,
                )
                .with_hint("run `colibri check` or `colibri update` to lock it again"));
            }
            None => {
                return Err(Diagnostic::error(
                    format!("the git dependency `{name}` is not in `{LOCK_FILE}` yet"),
                    span,
                )
                .with_hint(fetch_hint));
            }
        };
        let cache = git::cache_root().map_err(|m| Diagnostic::error(m, span))?;
        let dir = git::checkout_dir(&cache, &entry.url, &entry.commit);
        if !self.fs.is_file(&dir.join(git::COMPLETE_MARKER)) {
            return Err(Diagnostic::error(
                format!(
                    "the git dependency `{name}` ({}) is not in the cache",
                    entry.commit.get(..10).unwrap_or(&entry.commit)
                ),
                span,
            )
            .with_hint(fetch_hint));
        }
        Ok(dir)
    }

    /// Adds every source file under `dir` (`rel` is its path under the tree root) that is not a
    /// module yet, depth first in name order.
    fn add_tree_modules(&mut self, tree: usize, dir: &Path, rel: &Path) {
        for name in self.fs.list(dir) {
            let path = dir.join(&name);
            let rel_path = rel.join(&name);
            if self.fs.is_dir(&path) {
                // A link to a folder is not followed: its files are reached where they are
                // (and a link back up the tree would never end).
                if self.fs.is_symlink(&path) {
                    continue;
                }
                if !skipped_folder(self.fs, &name, &path) {
                    self.add_tree_modules(tree, &path, &rel_path);
                }
            } else if name.ends_with(&format!(".{SOURCE_EXT}"))
                && !self.by_path.contains_key(&self.fs.canonical(&path))
                && let Ok(text) = self.fs.read(&path)
            {
                let id = self.add_module(&path, text, tree, &rel_path, false);
                self.pending.push_back(id);
            }
        }
    }

    /// Registers and parses a file as a module of `tree`; `rel` is its path under the root.
    fn add_module(
        &mut self,
        path: &Path,
        text: String,
        tree: usize,
        rel: &Path,
        entry: bool,
    ) -> usize {
        let file_id = self
            .sources
            .add_file(self.fs.display_name(path), text.clone());
        let (file, diags) = parse_file(file_id, &text);
        for d in diags.iter().filter(|d| d.is_error()) {
            self.dropped.push(Dropped {
                kind: DroppedKind::Syntax,
                span: d.span,
                message: d.message.clone(),
            });
        }
        self.diags.extend(diags);
        let package = self.package_of(tree, rel);
        let name = self.module_name(tree, rel, entry);
        let id = self.discs.len();
        self.discs.push(Some(Disc {
            file,
            name,
            tree,
            package,
            entry,
            imports: Vec::new(),
        }));
        self.by_path.insert(self.fs.canonical(path), id);
        id
    }

    fn module_name(&self, tree: usize, rel: &Path, entry: bool) -> String {
        let stem = rel.with_extension("");
        let dotted = dotted(&stem);
        if entry {
            return stem
                .file_name()
                .map_or(dotted, |n| n.to_string_lossy().into_owned());
        }
        let t = &self.trees[tree];
        if t.is_dep {
            if dotted == "lib" {
                t.name.clone()
            } else {
                format!("{}.{dotted}", t.name)
            }
        } else {
            dotted
        }
    }

    fn package_of(&mut self, tree: usize, rel: &Path) -> usize {
        let dir = rel.parent().map(Path::to_path_buf).unwrap_or_default();
        if let Some(&p) = self.package_keys.get(&(tree, dir.clone())) {
            return p;
        }
        let t = &self.trees[tree];
        let name = match (dir.as_os_str().is_empty(), t.is_dep) {
            (true, _) => t.name.clone(),
            (false, true) => format!("{}.{}", t.name, dotted(&dir)),
            (false, false) => dotted(&dir),
        };
        self.packages.push(PackageNode { name });
        let p = self.packages.len() - 1;
        self.package_keys.insert((tree, dir), p);
        p
    }

    fn resolve_imports(&mut self, id: usize) {
        let tree = self.discs[id].as_ref().map_or(0, |d| d.tree);
        let imports: Vec<(NodeId, ast::Import, Span)> = match &self.discs[id] {
            Some(disc) => disc
                .file
                .items
                .iter()
                .filter_map(|top| match top {
                    TopLevel::Item(item) => match &item.kind {
                        ItemKind::Import(imp) => Some((item.id, imp.clone(), item.span)),
                        _ => None,
                    },
                    TopLevel::Stmt(_) => None,
                })
                .collect(),
            None => return,
        };
        let mut resolved = Vec::new();
        for (node, imp, item_span) in imports {
            let target = self.resolve_import(tree, &imp, item_span);
            resolved.push((node, target));
        }
        if let Some(disc) = &mut self.discs[id] {
            disc.imports = resolved;
        }
    }

    /// The candidates an import path can mean, in a fixed order.
    fn candidates(&self, tree: usize, path: &[&str]) -> Vec<Candidate> {
        let mut out = Vec::new();
        let t = &self.trees[tree];
        // `std` is reserved for the standard library: `import std.math` is never a file, a
        // dependency or a host module (ARCHITECTURE §13).
        if path[0] == "std" {
            if let Some(name) = std_module(path, self.host_modules) {
                out.push(Candidate::Std(name));
            }
            return out;
        }
        // 1. A file of this tree.
        let mut rel: PathBuf = path.iter().collect();
        rel.set_extension(SOURCE_EXT);
        let file = t.root.join(&rel);
        if self.fs.is_file(&file) {
            out.push(Candidate::Local { file, tree, rel });
        }
        // 2. A dependency of this tree.
        if let Some(&(_, dep)) = t.deps.iter().find(|(n, _)| n == path[0]) {
            let rel: PathBuf = if path.len() == 1 {
                PathBuf::from(LIB_FILE)
            } else {
                let mut r: PathBuf = path[1..].iter().collect();
                r.set_extension(SOURCE_EXT);
                r
            };
            let file = self.trees[dep].root.join(&rel);
            if self.fs.is_file(&file) {
                out.push(Candidate::Dependency {
                    name: path[0].to_string(),
                    file,
                    tree: dep,
                    rel,
                });
            }
        }
        // 3. A named host module.
        if path.len() == 1 && self.host_modules.iter().any(|m| m == path[0]) {
            out.push(Candidate::Host(path[0].to_string()));
        }
        // 4. The standard library.
        if let Some(name) = std_module(path, self.host_modules) {
            out.push(Candidate::Std(name));
        }
        out
    }

    fn resolve_import(&mut self, tree: usize, imp: &ast::Import, item_span: Span) -> ImportTarget {
        let names: Vec<&str> = imp.path.iter().map(|i| i.name.as_str()).collect();
        let span = match (imp.path.first(), imp.path.last()) {
            (Some(a), Some(b)) => a.span.to(b.span),
            _ => item_span,
        };
        let dotted_path = names.join(".");
        let mut candidates = self.candidates(tree, &names);
        match candidates.len() {
            0 => {
                self.report_missing(tree, &names, span);
                ImportTarget::Failed
            }
            1 => match candidates.remove(0) {
                // The standard library is a host module (ARCHITECTURE §13).
                Candidate::Host(name) | Candidate::Std(name) => ImportTarget::Host(name),
                Candidate::Local { file, tree, rel }
                | Candidate::Dependency {
                    file, tree, rel, ..
                } => {
                    let key = self.fs.canonical(&file);
                    if let Some(&id) = self.by_path.get(&key) {
                        return ImportTarget::Module(id);
                    }
                    match self.fs.read(&file) {
                        Ok(text) => {
                            let id = self.add_module(&file, text, tree, &rel, false);
                            self.pending.push_back(id);
                            ImportTarget::Module(id)
                        }
                        Err(e) => {
                            let message =
                                format!("cannot read `{}`: {e}", self.fs.display_name(&file));
                            self.dropped.push(Dropped {
                                kind: DroppedKind::Unreadable,
                                span,
                                message: message.clone(),
                            });
                            self.diags.push(Diagnostic::error(message, span));
                            ImportTarget::Failed
                        }
                    }
                }
            },
            _ => {
                let list: Vec<String> = candidates.iter().map(|c| c.describe(self.fs)).collect();
                let hint = if candidates.iter().any(|c| matches!(c, Candidate::Std(_))) {
                    format!(
                        "rename the file or the dependency, or write `import std.{dotted_path}` for the standard library"
                    )
                } else {
                    "rename the file or the dependency so that only one matches".to_string()
                };
                let message = format!(
                    "`import {dotted_path}` is ambiguous: it could be {}",
                    list.join(", or ")
                );
                self.dropped.push(Dropped {
                    kind: DroppedKind::Ambiguous,
                    span,
                    message: message.clone(),
                });
                self.diags
                    .push(Diagnostic::error(message, span).with_hint(hint));
                ImportTarget::Failed
            }
        }
    }

    fn report_missing(&mut self, tree: usize, names: &[&str], span: Span) {
        let dotted_path = names.join(".");
        let t = &self.trees[tree];
        let mut diag = Diagnostic::error(format!("no module `{dotted_path}`"), span);
        if names[0] == "host" {
            diag = diag.with_hint(
                "the default `host` module is always in scope; its names need no import",
            );
            self.diags.push(diag);
            return;
        }
        let std_name = match names {
            ["std", name] => Some(*name),
            ["std"] => Some(""),
            [name] => Some(*name),
            _ => None,
        };
        if let Some(name) = std_name
            && let Some((_, why)) = LATER_STD_MODULES.iter().find(|(n, _)| *n == name)
        {
            let diag =
                Diagnostic::error(format!("the standard library module `{name}` {why}"), span);
            self.diags.push(diag);
            return;
        }
        if names[0] == "std" {
            let available: Vec<&str> = self
                .host_modules
                .iter()
                .filter_map(|m| m.strip_prefix(crate::host::STD_PREFIX))
                .collect();
            let hint = if available.is_empty() {
                "this program was checked without the standard library".to_string()
            } else {
                format!("the standard library modules are {}", available.join(", "))
            };
            let message = match names {
                ["std", name] => format!("the standard library has no module `{name}`"),
                _ => format!("no module `{dotted_path}`"),
            };
            self.diags
                .push(Diagnostic::error(message, span).with_hint(hint));
            return;
        }
        // `import a.b.X`: a module followed by a name.
        for cut in (1..names.len()).rev() {
            let prefix = &names[..cut];
            if !self.candidates(tree, prefix).is_empty() {
                let rest = names[cut..].join(", ");
                diag = diag.with_hint(format!(
                    "`{}` is a module; to bring in a name from it write `import {}.{{{rest}}}`",
                    prefix.join("."),
                    prefix.join(".")
                ));
                self.diags.push(diag);
                return;
            }
        }
        let mut rel: PathBuf = names.iter().collect();
        let folder = t.root.join(&rel);
        rel.set_extension(SOURCE_EXT);
        let mut hint = format!("looked for `{}`", self.fs.display_name(&t.root.join(&rel)));
        if let Some((_, dep)) = t.deps.iter().find(|(n, _)| n == names[0]) {
            hint.push_str(&format!(
                " and in the dependency `{}` (`{}`)",
                names[0],
                self.fs.display_name(&self.trees[*dep].root)
            ));
        }
        if self.fs.is_dir(&folder) {
            let modules: Vec<String> = self
                .fs
                .list(&folder)
                .into_iter()
                .filter_map(|f| {
                    f.strip_suffix(&format!(".{SOURCE_EXT}"))
                        .map(str::to_string)
                })
                .map(|stem| format!("{dotted_path}.{stem}"))
                .collect();
            if !modules.is_empty() {
                hint = format!(
                    "`{dotted_path}` is a folder (a package), not a module; import one of its modules: {}",
                    modules.join(", ")
                );
            }
        }
        self.diags.push(diag.with_hint(hint));
    }

    /// Orders the modules, finds cycles between packages and assembles the result.
    fn finish(mut self, entry: usize) -> Loaded {
        // Post-order of the import graph from the entry: imports first.
        let n = self.discs.len();
        let edges: Vec<Vec<usize>> = (0..n)
            .map(|i| {
                self.discs[i]
                    .as_ref()
                    .map(|d| {
                        d.imports
                            .iter()
                            .filter_map(|(_, t)| match t {
                                ImportTarget::Module(m) => Some(*m),
                                _ => None,
                            })
                            .collect()
                    })
                    .unwrap_or_default()
            })
            .collect();
        // A module nothing imports (loaded by `all_modules`) is a script of its own.
        let mut imported = vec![false; n];
        for (from, targets) in edges.iter().enumerate() {
            for &to in targets {
                if to != from {
                    imported[to] = true;
                }
            }
        }
        let mut order: Vec<usize> = Vec::with_capacity(n);
        let mut seen = vec![false; n];
        seen[entry] = true;
        // Modules nobody imports (loaded on request, see `LoadOptions::all_modules`) come
        // first, in discovery order; the entry is always last.
        let roots: Vec<usize> = (0..n)
            .filter(|&i| i != entry && self.discs[i].is_some())
            .chain([entry])
            .collect();
        for root in roots {
            if root != entry && seen[root] {
                continue;
            }
            seen[root] = true;
            let mut stack: Vec<(usize, usize)> = vec![(root, 0)];
            while let Some(&(node, next)) = stack.last() {
                match edges[node].get(next) {
                    Some(&to) => {
                        if let Some(top) = stack.last_mut() {
                            top.1 += 1;
                        }
                        if !seen[to] {
                            seen[to] = true;
                            stack.push((to, 0));
                        }
                    }
                    None => {
                        order.push(node);
                        stack.pop();
                    }
                }
            }
        }
        self.report_package_cycles();
        let mut new_index = vec![usize::MAX; n];
        for (new, &old) in order.iter().enumerate() {
            new_index[old] = new;
        }
        let mut files = Vec::with_capacity(n);
        let mut metas = Vec::with_capacity(n);
        for &old in &order {
            let Some(disc) = self.discs[old].take() else {
                continue;
            };
            let imports = disc
                .imports
                .into_iter()
                .map(|(node, target)| {
                    let target = match target {
                        ImportTarget::Module(m) => ImportTarget::Module(new_index[m]),
                        other => other,
                    };
                    (node, target)
                })
                .collect();
            metas.push(ModuleMeta {
                name: disc.name,
                package: disc.package,
                entry: disc.entry,
                script: !disc.entry && !imported[old],
                dependency: self.trees[disc.tree].is_dep,
                ffi: self.trees[disc.tree].ffi,
                imports,
            });
            files.push(disc.file);
        }
        Loaded {
            files,
            metas,
            packages: self.packages,
            diagnostics: self.diags,
            dropped: self.dropped,
        }
    }

    /// Import cycles between modules of different packages are an error (DESIGN §13).
    fn report_package_cycles(&mut self) {
        // (from package, to package, the import's path span)
        let mut pkg_edges: Vec<(usize, usize, Span)> = Vec::new();
        for d in self.discs.iter().flatten() {
            for (node, target) in &d.imports {
                let ImportTarget::Module(to) = target else {
                    continue;
                };
                let Some(to_pkg) = self.discs[*to].as_ref().map(|t| t.package) else {
                    continue;
                };
                if to_pkg == d.package {
                    continue;
                }
                let span = d
                    .file
                    .items
                    .iter()
                    .find_map(|top| match top {
                        TopLevel::Item(item) if item.id == *node => match &item.kind {
                            ItemKind::Import(imp) => match (imp.path.first(), imp.path.last()) {
                                (Some(a), Some(b)) => Some(a.span.to(b.span)),
                                _ => Some(item.span),
                            },
                            _ => None,
                        },
                        _ => None,
                    })
                    .unwrap_or(d.file.span);
                pkg_edges.push((d.package, to_pkg, span));
            }
        }
        let count = self.packages.len();
        let mut adj: Vec<Vec<usize>> = vec![Vec::new(); count];
        for (idx, (from, _, _)) in pkg_edges.iter().enumerate() {
            adj[*from].push(idx);
        }
        #[derive(Clone, Copy, PartialEq)]
        enum Color {
            White,
            Grey,
            Black,
        }
        let mut color = vec![Color::White; count];
        let mut reported: HashSet<Vec<usize>> = HashSet::new();
        for start in 0..count {
            if color[start] != Color::White {
                continue;
            }
            // (package, next edge to try); `path` holds the edges taken to get here.
            let mut stack: Vec<(usize, usize)> = vec![(start, 0)];
            let mut path: Vec<usize> = Vec::new();
            color[start] = Color::Grey;
            while let Some(&(node, next)) = stack.last() {
                match adj[node].get(next) {
                    None => {
                        color[node] = Color::Black;
                        stack.pop();
                        path.pop();
                    }
                    Some(&edge) => {
                        if let Some(top) = stack.last_mut() {
                            top.1 += 1;
                        }
                        let to = pkg_edges[edge].1;
                        match color[to] {
                            Color::White => {
                                color[to] = Color::Grey;
                                stack.push((to, 0));
                                path.push(edge);
                            }
                            Color::Black => {}
                            Color::Grey => {
                                let from = stack.iter().position(|&(p, _)| p == to).unwrap_or(0);
                                let mut cycle: Vec<usize> = path[from..].to_vec();
                                cycle.push(edge);
                                let mut key: Vec<usize> =
                                    cycle.iter().map(|&e| pkg_edges[e].0).collect();
                                key.sort_unstable();
                                if reported.insert(key) {
                                    self.report_cycle(&cycle, &pkg_edges);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    fn report_cycle(&mut self, cycle: &[usize], pkg_edges: &[(usize, usize, Span)]) {
        let name = |p: usize| self.packages[p].name.clone();
        let mut chain: Vec<String> = cycle
            .iter()
            .map(|&e| format!("`{}`", name(pkg_edges[e].0)))
            .collect();
        chain.push(format!("`{}`", name(pkg_edges[cycle[0]].0)));
        let first = pkg_edges[cycle[0]];
        let mut diag = Diagnostic::error(
            format!("import cycle between packages: {}", chain.join(" -> ")),
            first.2,
        )
        .with_hint(
            "files of one package may import each other, but packages may not import in a circle; move what they share into a third package",
        );
        for &e in &cycle[1..] {
            let (from, to, span) = pkg_edges[e];
            diag = diag.with_label(
                span,
                format!("`{}` imports `{}` here", name(from), name(to)),
            );
        }
        self.diags.push(diag);
    }
}

/// `a/b` as `a.b`.
fn dotted(path: &Path) -> String {
    path.components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(".")
}

/// The error for a path dependency of a package from git that is not inside its repository.
fn leaves_repository_diagnostic(
    fs: &dyn SourceFs,
    name: &str,
    dep_dir: &Path,
    span: Span,
) -> Diagnostic {
    Diagnostic::error(
        format!(
            "the path dependency `{name}` leaves the git repository it is in (`{}`)",
            display_dir(fs, dep_dir)
        ),
        span,
    )
    .with_hint("a package from git may only use folders of its own repository")
}

fn display_dir(fs: &dyn SourceFs, dir: &Path) -> String {
    if dir.as_os_str().is_empty() {
        ".".to_string()
    } else {
        fs.display_name(dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::package::fs::MemoryFs;

    fn load_mem(files: &[(&str, &str)], host: &[&str]) -> (Loaded, SourceMap) {
        let mut fs = MemoryFs::new();
        for (p, t) in files {
            fs.insert(p, *t);
        }
        let mut sources = SourceMap::new();
        let host: Vec<String> = host.iter().map(|s| s.to_string()).collect();
        let loaded =
            load(&fs, &mut sources, Entry::File(Path::new(files[0].0)), &host).expect("loads");
        (loaded, sources)
    }

    #[test]
    fn a_manifest_or_a_lock_is_not_an_entry_file() {
        for name in ["colibri.toml", "app/colibri.lock", "app/Colibri.TOML"] {
            let mut fs = MemoryFs::new();
            fs.insert(name, "x = \"https://u:SeCrEt@h\"\n");
            let mut sources = SourceMap::new();
            let err =
                load(&fs, &mut sources, Entry::File(Path::new(name)), &[]).expect_err("refused");
            assert!(err.message.contains("not a script"), "{name}: {err:?}");
            assert!(!err.message.contains("SeCrEt"));
        }
    }

    fn all_module_names(files: &[(&str, &str)]) -> Vec<String> {
        let mut fs = MemoryFs::new();
        for (p, t) in files {
            fs.insert(p, *t);
        }
        let mut sources = SourceMap::new();
        let loaded = load_with(
            &fs,
            &mut sources,
            Entry::Package(Path::new("p")),
            &[],
            LoadOptions { all_modules: true },
        )
        .expect("loads");
        let mut names: Vec<String> = loaded.metas.iter().map(|m| m.name.clone()).collect();
        names.sort();
        names
    }

    /// Files in memory that fail the test when asked about a path with `FORBIDDEN` in it: what
    /// the loader must not look at.
    struct Watching(MemoryFs);

    const FORBIDDEN: &str = "FORBIDDEN";

    impl Watching {
        fn guard(&self, path: &Path) {
            assert!(
                !path.to_string_lossy().contains(FORBIDDEN),
                "the loader looked at {path:?}"
            );
        }
    }

    impl SourceFs for Watching {
        fn read(&self, path: &Path) -> Result<String, String> {
            self.guard(path);
            self.0.read(path)
        }
        fn read_prefix(&self, path: &Path, n: usize) -> Option<Vec<u8>> {
            self.guard(path);
            self.0.read_prefix(path, n)
        }
        fn is_file(&self, path: &Path) -> bool {
            self.guard(path);
            self.0.is_file(path)
        }
        fn is_dir(&self, path: &Path) -> bool {
            self.guard(path);
            self.0.is_dir(path)
        }
        fn list(&self, dir: &Path) -> Vec<String> {
            self.guard(dir);
            self.0.list(dir)
        }
        fn is_symlink(&self, path: &Path) -> bool {
            self.guard(path);
            self.0.is_symlink(path)
        }
        fn canonical(&self, path: &Path) -> PathBuf {
            self.guard(path);
            self.0.canonical(path)
        }
        fn display_name(&self, path: &Path) -> String {
            self.0.display_name(path)
        }
    }

    #[test]
    fn a_path_of_a_git_package_that_leaves_its_repository_is_refused_before_any_folder_is_looked_at()
     {
        for rel in [
            "/FORBIDDEN/x",
            "\\FORBIDDEN\\x",
            "//FORBIDDEN/share/x",
            "\\\\FORBIDDEN\\share\\x",
            "\\\\?\\C:\\FORBIDDEN",
            "\\\\?\\UNC\\FORBIDDEN\\share",
            "C:/FORBIDDEN",
            "C:\\FORBIDDEN",
            "c:FORBIDDEN",
            "../../FORBIDDEN",
            "..\\..\\FORBIDDEN",
            "x/../../../FORBIDDEN",
            "x\\..\\..\\..\\FORBIDDEN",
            "./../../FORBIDDEN/./y",
        ] {
            let mut mem = MemoryFs::new();
            mem.insert(
                "/cache/repo/pkg/colibri.toml",
                format!("[package]\nname = \"pkg\"\n\n[dependencies]\nd = {{ path = '{rel}' }}\n"),
            );
            let fs = Watching(mem);
            let mut sources = SourceMap::new();
            let mut loader = Loader::new(&fs, &mut sources, &[]);
            let root = PathBuf::from("/cache/repo");
            loader
                .load_tree(Path::new("/cache/repo/pkg"), true, true, Some(root))
                .unwrap();
            let said: Vec<String> = loader.diags.iter().map(|d| d.message.clone()).collect();
            assert_eq!(said.len(), 1, "{rel}: {said:?}");
            assert!(
                said[0].contains("leaves the git repository"),
                "{rel}: {said:?}"
            );
        }
    }

    #[test]
    fn a_cachedir_tag_without_the_signature_does_not_make_a_cache() {
        let names = all_module_names(&[
            ("p/colibri.toml", "[package]\nname = \"p\"\n"),
            ("p/main.colibri", ""),
            ("p/empty/CACHEDIR.TAG", ""),
            ("p/empty/e.colibri", ""),
            (
                "p/words/CACHEDIR.TAG",
                "this is not a signature, only a file",
            ),
            ("p/words/w.colibri", ""),
            (
                "p/short/CACHEDIR.TAG",
                "Signature: 8a477f597d28d172789f06886806bc5",
            ),
            ("p/short/s.colibri", ""),
            (
                "p/other/CACHEDIR.TAG",
                "Signature: 00000000000000000000000000000000",
            ),
            ("p/other/o.colibri", ""),
            // The signature at the start, whatever follows it.
            (
                "p/real/CACHEDIR.TAG",
                "Signature: 8a477f597d28d172789f06886806bc55\n# a comment\n",
            ),
            ("p/real/r.colibri", ""),
        ]);
        for walked in ["empty.e", "words.w", "short.s", "other.o"] {
            assert!(names.contains(&walked.to_string()), "{walked}: {names:?}");
        }
        assert!(!names.contains(&"real.r".to_string()), "{names:?}");
    }

    #[test]
    fn the_all_modules_walk_skips_cache_folders_but_not_target_or_node_modules() {
        let names = all_module_names(&[
            ("p/colibri.toml", "[package]\nname = \"p\"\n"),
            ("p/main.colibri", "import cached.c\n"),
            ("p/target/a.colibri", ""),
            ("p/node_modules/n.colibri", ""),
            (
                "p/build/CACHEDIR.TAG",
                "Signature: 8a477f597d28d172789f06886806bc55",
            ),
            ("p/build/b.colibri", ""),
            (
                "p/cached/CACHEDIR.TAG",
                "Signature: 8a477f597d28d172789f06886806bc55",
            ),
            ("p/cached/c.colibri", ""),
            ("p/.hidden/h.colibri", ""),
        ]);
        // `build` is walked past; `cached` is only there because `main` imports it by name.
        assert!(names.contains(&"target.a".to_string()), "{names:?}");
        assert!(names.contains(&"node_modules.n".to_string()), "{names:?}");
        assert!(!names.contains(&"build.b".to_string()), "{names:?}");
        assert!(names.contains(&"cached.c".to_string()), "{names:?}");
        assert!(!names.iter().any(|n| n.contains("hidden")), "{names:?}");
    }

    fn messages(l: &Loaded) -> Vec<String> {
        l.diagnostics.iter().map(|d| d.message.clone()).collect()
    }

    #[test]
    fn modules_come_in_initialisation_order_with_the_entry_last() {
        let (l, _) = load_mem(
            &[
                ("m.colibri", "import a\nimport b\n"),
                ("a.colibri", "import b\n"),
                ("b.colibri", ""),
            ],
            &[],
        );
        assert!(!l.has_errors(), "{:?}", messages(&l));
        let names: Vec<_> = l.metas.iter().map(|m| m.name.clone()).collect();
        assert_eq!(names, ["b", "a", "m"].map(String::from));
        assert!(l.metas.last().unwrap().entry);
    }

    #[test]
    fn a_missing_module_is_reported_at_the_import() {
        let (l, _) = load_mem(&[("m.colibri", "import nothing\n")], &[]);
        assert!(
            messages(&l)
                .iter()
                .any(|m| m.contains("no module `nothing`"))
        );
    }

    #[test]
    fn a_local_file_and_a_host_module_are_ambiguous() {
        let (l, _) = load_mem(
            &[("m.colibri", "import physics\n"), ("physics.colibri", "")],
            &["physics"],
        );
        let msgs = messages(&l);
        assert!(
            msgs.iter()
                .any(|m| m.contains("physics") && m.contains("ambiguous")),
            "{msgs:?}"
        );
    }

    #[test]
    fn a_host_module_resolves_without_a_file() {
        let (l, _) = load_mem(&[("m.colibri", "import physics\n")], &["physics"]);
        assert!(!l.has_errors(), "{:?}", messages(&l));
        assert!(
            l.metas[0]
                .imports
                .values()
                .any(|t| matches!(t, ImportTarget::Host(n) if n == "physics"))
        );
    }

    #[test]
    fn cycles_between_packages_are_listed() {
        let (l, _) = load_mem(
            &[
                ("m.colibri", "import a.x\n"),
                ("a/x.colibri", "import b.y\n"),
                ("b/y.colibri", "import a.x\n"),
            ],
            &[],
        );
        let msgs = messages(&l);
        assert!(
            msgs.iter()
                .any(|m| m.contains("cycle") && m.contains("`a`") && m.contains("`b`")),
            "{msgs:?}"
        );
    }

    #[test]
    fn cycles_inside_a_package_are_fine() {
        let (l, _) = load_mem(
            &[
                ("m.colibri", "import a\n"),
                ("a.colibri", "import b\n"),
                ("b.colibri", "import a\n"),
            ],
            &[],
        );
        assert!(!l.has_errors(), "{:?}", messages(&l));
    }
}
