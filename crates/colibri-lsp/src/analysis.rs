//! Checking the package a document belongs to, with unsaved buffers laid over the files on
//! disk, and keeping what hover and definition need.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use colibri_check::HostEnv;
use colibri_check::insight::Insight;
use colibri_check::package::{
    DiskFs, Entry, LoadError, LoadOptions, Loaded, MAIN_FILE, SourceFs, is_hidden_or_cache,
    load_with, normalize, skipped_folder, try_canonical_path,
};
use colibri_syntax::{Diagnostic, FileId, SourceMap, Span};

/// The manifest's file name.
const MANIFEST: &str = "colibri.toml";

/// The path with links resolved, so two names of one file are equal. A path that is not on
/// disk (an unsaved buffer) is resolved through its nearest folder that is.
pub fn canon(path: &Path) -> PathBuf {
    let dot = |p: &Path| {
        if p.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            p.to_path_buf()
        }
    };
    if let Ok(p) = try_canonical_path(&dot(path)) {
        return p;
    }
    let norm = normalize(path);
    let mut rest: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = norm.as_path();
    while let Some(parent) = cur.parent() {
        let Some(name) = cur.file_name() else { break };
        rest.push(name.to_os_string());
        if let Ok(base) = try_canonical_path(&dot(parent)) {
            return rest.iter().rev().fold(base, |acc, n| acc.join(n));
        }
        cur = parent;
    }
    norm
}

/// Unsaved buffers, by canonical path, over the disk. Every path asked about is resolved once
/// (a link, or a name that differs in case on a case-insensitive file system, still reaches
/// its buffer); the folders the buffers are in are indexed, so a listing or a folder test does
/// not scan the buffers.
pub struct Overlay<'a> {
    buffers: &'a HashMap<PathBuf, String>,
    /// For each folder above a buffer: the names of its entries that hold or are a buffer.
    entries: HashMap<PathBuf, Vec<String>>,
    /// The canonical form of the paths asked about so far.
    resolved: RefCell<HashMap<PathBuf, PathBuf>>,
}

impl<'a> Overlay<'a> {
    /// `buffers` is keyed by canonical path.
    pub fn new(buffers: &'a HashMap<PathBuf, String>) -> Overlay<'a> {
        let mut entries: HashMap<PathBuf, Vec<String>> = HashMap::new();
        for path in buffers.keys() {
            let mut cur = path.as_path();
            while let (Some(parent), Some(name)) = (cur.parent(), cur.file_name()) {
                entries
                    .entry(parent.to_path_buf())
                    .or_default()
                    .push(name.to_string_lossy().into_owned());
                cur = parent;
            }
        }
        for names in entries.values_mut() {
            names.sort();
            names.dedup();
        }
        Overlay {
            buffers,
            entries,
            resolved: RefCell::new(HashMap::new()),
        }
    }

    /// The canonical path of `path`, worked out once.
    fn resolve(&self, path: &Path) -> PathBuf {
        if let Some(found) = self.resolved.borrow().get(path) {
            return found.clone();
        }
        let found = canon(path);
        self.resolved
            .borrow_mut()
            .insert(path.to_path_buf(), found.clone());
        found
    }

    /// The text of the buffer that is `path`, if there is one.
    fn buffer(&self, path: &Path) -> Option<&String> {
        if self.buffers.is_empty() {
            return None;
        }
        self.buffers.get(&self.resolve(path))
    }
}

impl SourceFs for Overlay<'_> {
    fn read(&self, path: &Path) -> Result<String, String> {
        COUNTERS.with(|c| c.files_read.set(c.files_read.get() + 1));
        match self.buffer(path) {
            Some(text) => Ok(text.clone()),
            None => DiskFs.read(path),
        }
    }

    fn read_prefix(&self, path: &Path, n: usize) -> Option<Vec<u8>> {
        match self.buffer(path) {
            Some(text) => Some(text.as_bytes()[..n.min(text.len())].to_vec()),
            None => DiskFs.read_prefix(path, n),
        }
    }

    fn is_file(&self, path: &Path) -> bool {
        self.buffer(path).is_some() || DiskFs.is_file(path)
    }

    fn is_dir(&self, path: &Path) -> bool {
        DiskFs.is_dir(path)
            || (!self.entries.is_empty() && self.entries.contains_key(&self.resolve(path)))
    }

    fn is_symlink(&self, path: &Path) -> bool {
        self.buffer(path).is_none() && DiskFs.is_symlink(path)
    }

    fn canonical(&self, path: &Path) -> PathBuf {
        self.resolve(path)
    }

    fn list(&self, dir: &Path) -> Vec<String> {
        let mut names = DiskFs.list(dir);
        if !self.entries.is_empty()
            && let Some(more) = self.entries.get(&self.resolve(dir))
        {
            names.extend(more.iter().cloned());
            names.sort();
            names.dedup();
        }
        names
    }
}

/// How a program is read: the CLI reads a source file in one of these ways, and what an
/// `import` means differs between them.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Spec {
    /// `colibri check <folder>`: the package in the folder, every module under it, imports
    /// rooted at the package folder.
    Package(PathBuf),
    /// `colibri run <file>` / `colibri check <file>`: the file alone as a script, imports
    /// rooted at the file's own folder.
    File(PathBuf),
}

impl Spec {
    /// The folder `import`s are rooted at.
    pub fn root(&self) -> PathBuf {
        match self {
            Spec::Package(d) => d.clone(),
            Spec::File(f) => f.parent().map(Path::to_path_buf).unwrap_or_default(),
        }
    }
}

/// The result of checking a program: everything the server answers from.
pub struct Analysis {
    pub sources: SourceMap,
    pub diagnostics: Vec<Diagnostic>,
    pub insight: Insight,
    /// The syntax trees of every module that was loaded, with how they import each other. It
    /// is there even when the program has syntax errors (the parser recovers) or does not
    /// check.
    pub program: Loaded,
    /// What the host provides (the standard library's modules and the client's interfaces).
    pub host: Rc<HostEnv>,
    /// How the program was read.
    pub spec: Spec,
    /// The canonical path of each file, by `FileId`.
    canon: Vec<PathBuf>,
    /// The file of each canonical path.
    by_path: HashMap<PathBuf, FileId>,
}

/// How much work the server on this thread has done: what the tests that bound the cost of a
/// request count, instead of the time it took. (Each server runs on a thread of its own, and
/// the counts are of that thread.)
#[derive(Default)]
struct Counters {
    /// Files read through the overlay (a buffer or the disk).
    files_read: std::cell::Cell<u64>,
    /// Programs checked.
    analyses: std::cell::Cell<u64>,
}

thread_local! {
    static COUNTERS: Counters = Counters::default();
}

/// `(files read, programs checked)` so far by the server of this thread.
pub fn counters() -> (u64, u64) {
    COUNTERS.with(|c| (c.files_read.get(), c.analyses.get()))
}

impl Analysis {
    fn new(
        sources: SourceMap,
        diagnostics: Vec<Diagnostic>,
        insight: Insight,
        program: Loaded,
        host: Rc<HostEnv>,
        spec: Spec,
    ) -> Analysis {
        COUNTERS.with(|c| c.analyses.set(c.analyses.get() + 1));
        let canon: Vec<PathBuf> = (0..)
            .map(FileId)
            .take_while(|f| sources.contains(*f))
            .map(|f| canon(Path::new(sources.name(f))))
            .collect();
        let mut by_path = HashMap::new();
        for (i, p) in canon.iter().enumerate() {
            by_path.entry(p.clone()).or_insert(FileId(i as u32));
        }
        Analysis {
            sources,
            diagnostics,
            insight,
            program,
            host,
            spec,
            canon,
            by_path,
        }
    }

    /// Every file of the program, with its canonical path.
    pub fn files(&self) -> Vec<(FileId, PathBuf)> {
        self.canon
            .iter()
            .enumerate()
            .map(|(i, p)| (FileId(i as u32), p.clone()))
            .collect()
    }

    /// The canonical path of `file`.
    pub fn path_of(&self, file: FileId) -> PathBuf {
        self.canon
            .get(file.0 as usize)
            .cloned()
            .unwrap_or_else(|| normalize(Path::new(self.sources.name(file))))
    }

    /// The index of `file` among the loaded modules.
    pub fn module_of(&self, file: FileId) -> Option<usize> {
        self.program.files.iter().position(|f| f.span.file == file)
    }

    /// The file with this canonical path.
    pub fn file_of(&self, path: &Path) -> Option<FileId> {
        self.by_path.get(path).copied()
    }

    /// The canonical paths of the source modules this program loaded.
    pub fn module_paths(&self) -> impl Iterator<Item = PathBuf> + '_ {
        self.program.files.iter().map(|f| self.path_of(f.span.file))
    }
}

/// What the host interfaces gave: the environment, and what could not be read.
pub struct Hosts {
    pub env: HostEnv,
    pub problems: Vec<(PathBuf, String)>,
}

fn empty_program() -> Loaded {
    Loaded {
        files: Vec::new(),
        metas: Vec::new(),
        packages: Vec::new(),
        diagnostics: Vec::new(),
        dropped: Vec::new(),
    }
}

/// The canonical text of the standard library's interfaces, built once.
fn std_interface_texts() -> &'static [String] {
    static TEXTS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    TEXTS.get_or_init(|| {
        colibri_std::interfaces()
            .iter()
            .map(|i| i.canonical_text())
            .collect()
    })
}

/// The `*.cbi` files at the top of `dir`, if `dir` is a package (has a `colibri.toml`).
fn package_interfaces(fs: &dyn SourceFs, dir: &Path) -> Vec<PathBuf> {
    if !fs.is_file(&dir.join(MANIFEST)) {
        return Vec::new();
    }
    fs.list(dir)
        .into_iter()
        .filter(|n| n.ends_with(".cbi"))
        .map(|n| dir.join(n))
        .collect()
}

/// Reads the interface files: those the client configured, and the ones next to the manifests
/// of the workspace and of the document's package. The tool trusts what it checks, so every
/// capability is granted, as `colibri check` does.
pub fn host_env(settings: &Settings, fs: &dyn SourceFs, package_dir: Option<&Path>) -> Hosts {
    let mut paths: Vec<PathBuf> = settings.interfaces.clone();
    for dir in settings
        .root
        .iter()
        .map(PathBuf::as_path)
        .chain(package_dir)
    {
        for p in package_interfaces(fs, dir) {
            if !paths.contains(&p) {
                paths.push(p);
            }
        }
    }
    let mut env = HostEnv::new();
    let mut problems = Vec::new();
    // The standard library is always there, as for `colibri check`.
    for text in std_interface_texts() {
        match HostEnv::from_text(text) {
            Ok(one) => env.merge(one),
            Err(message) => problems.push((PathBuf::from("<std>"), message)),
        }
    }
    for path in paths {
        match fs.read(&path).and_then(|text| HostEnv::from_text(&text)) {
            Ok(one) => env.merge(one),
            Err(message) => problems.push((path, message)),
        }
    }
    env.grant_all();
    Hosts { env, problems }
}

/// The folder with a `colibri.toml` that holds `doc`, nearest first.
fn manifest_dir(fs: &dyn SourceFs, doc: &Path) -> Option<PathBuf> {
    doc.ancestors()
        .skip(1)
        .find(|dir| fs.is_file(&dir.join(MANIFEST)))
        .map(Path::to_path_buf)
}

/// A program that is loaded (its files read and parsed, its imports resolved) but not yet
/// checked: enough to know which files it holds and what they import.
pub struct Pending {
    pub spec: Spec,
    sources: SourceMap,
    loaded: Loaded,
    env: Rc<HostEnv>,
    /// The canonical path of each module, as in `loaded.files`.
    paths: Vec<PathBuf>,
}

impl Pending {
    fn new(spec: Spec, sources: SourceMap, loaded: Loaded, env: Rc<HostEnv>) -> Pending {
        let paths = loaded
            .files
            .iter()
            .map(|f| canon(Path::new(sources.name(f.span.file))))
            .collect();
        Pending {
            spec,
            sources,
            loaded,
            env,
            paths,
        }
    }

    /// The canonical paths of the modules.
    pub fn paths(&self) -> &[PathBuf] {
        &self.paths
    }

    pub fn holds(&self, path: &Path) -> bool {
        self.paths.iter().any(|p| p == path)
    }

    /// What was left out of the program while loading it.
    pub fn program(&self) -> (&SourceMap, &Loaded) {
        (&self.sources, &self.loaded)
    }

    /// Checks the program.
    pub fn check(self, settings: &Settings) -> Analysis {
        let mut diagnostics = self.loaded.diagnostics.clone();
        let mut insight = Insight::default();
        if !self.loaded.has_errors() {
            let (checked, found) = colibri_check::check_modules_with_insight(
                &self.loaded.inputs(),
                &self.loaded.packages,
                &self.env,
                settings.stack_budget,
            );
            diagnostics.extend(checked.diagnostics);
            insight = found;
        }
        Analysis::new(
            self.sources,
            diagnostics,
            insight,
            self.loaded,
            self.env,
            self.spec,
        )
    }
}

/// A host environment with the names of its named modules.
type HostChoice = (Rc<HostEnv>, Vec<String>);

/// Loads programs through one file system, with the host environments built once.
pub struct Reader<'a> {
    pub fs: &'a dyn SourceFs,
    pub settings: &'a Settings,
    envs: RefCell<HashMap<Option<PathBuf>, HostChoice>>,
    problems: RefCell<Vec<(PathBuf, String)>>,
}

impl<'a> Reader<'a> {
    pub fn new(fs: &'a dyn SourceFs, settings: &'a Settings) -> Reader<'a> {
        Reader {
            fs,
            settings,
            envs: RefCell::new(HashMap::new()),
            problems: RefCell::new(Vec::new()),
        }
    }

    /// The interface files that could not be used so far.
    pub fn problems(&self) -> Vec<(PathBuf, String)> {
        self.problems.borrow().clone()
    }

    fn env_for(&self, package_dir: Option<&Path>) -> (Rc<HostEnv>, Vec<String>) {
        let key = package_dir.map(Path::to_path_buf);
        if let Some(found) = self.envs.borrow().get(&key) {
            return found.clone();
        }
        let Hosts { env, problems } = host_env(self.settings, self.fs, package_dir);
        self.problems.borrow_mut().extend(problems);
        let modules = env.named_modules();
        let made = (Rc::new(env), modules);
        self.envs.borrow_mut().insert(key, made.clone());
        made
    }

    /// Reads the program of `spec` without checking it.
    pub fn load(&self, spec: &Spec, all_modules: bool) -> Result<Pending, LoadError> {
        let package_dir = match spec {
            Spec::Package(d) => Some(d.clone()),
            Spec::File(f) => manifest_dir(self.fs, f),
        };
        let (env, modules) = self.env_for(package_dir.as_deref());
        let mut sources = SourceMap::new();
        let entry = match spec {
            Spec::Package(d) => Entry::Package(d),
            Spec::File(f) => Entry::File(f),
        };
        let loaded = load_with(
            self.fs,
            &mut sources,
            entry,
            &modules,
            LoadOptions { all_modules },
        )?;
        Ok(Pending::new(spec.clone(), sources, loaded, env))
    }

    /// Reads and checks the program of `spec`.
    pub fn analyze(&self, spec: &Spec, all_modules: bool) -> Result<Analysis, LoadError> {
        Ok(self.load(spec, all_modules)?.check(self.settings))
    }
}

/// How the program around a document is found and checked.
pub struct Settings {
    pub stack_budget: usize,
    /// Interface (`.cbi`) files named by the client.
    pub interfaces: Vec<PathBuf>,
    /// The folder the client opened, if any, as a canonical path.
    pub root: Option<PathBuf>,
    /// The same folder as the client wrote it (what URIs of files under it are built from).
    pub root_shown: Option<PathBuf>,
}

/// Checks the program `doc` belongs to: the package of the nearest `colibri.toml`, or the
/// `main.colibri` of its folder, if they reach `doc`; else `doc` as a script of its own.
pub fn analyze(
    doc: &Path,
    fs: &dyn SourceFs,
    settings: &Settings,
) -> (Analysis, Vec<(PathBuf, String)>) {
    analyze_with(doc, fs, settings, false)
}

/// [`analyze`] over the whole package when `all_modules` is set: every `.colibri` file under
/// the package's root is loaded, not only the modules the entry imports (the ones nothing
/// imports are scripts of their own), as `colibri check <folder>` does.
pub fn analyze_with(
    doc: &Path,
    fs: &dyn SourceFs,
    settings: &Settings,
    all_modules: bool,
) -> (Analysis, Vec<(PathBuf, String)>) {
    let reader = Reader::new(fs, settings);
    let analysis = analyze_in(&reader, doc, all_modules);
    (analysis, reader.problems())
}

/// [`analyze_with`] in a reader.
pub fn analyze_in(reader: &Reader<'_>, doc: &Path, all_modules: bool) -> Analysis {
    let fs = reader.fs;
    let doc = canon(doc);
    let package_dir = manifest_dir(fs, &doc);
    let mut specs: Vec<Spec> = Vec::new();
    if let Some(dir) = &package_dir
        && fs.is_file(&dir.join(MANIFEST))
        && fs.is_file(&dir.join(MAIN_FILE))
        && *dir != doc
    {
        specs.push(Spec::Package(dir.clone()));
    }
    if let Some(main) = doc
        .parent()
        .map(|d| d.join(MAIN_FILE))
        .filter(|m| *m != doc && fs.is_file(m))
    {
        specs.push(Spec::File(main));
    }
    for spec in specs {
        if let Ok(pending) = reader.load(&spec, all_modules)
            && pending.holds(&doc)
        {
            return pending.check(reader.settings);
        }
    }
    let own = Spec::File(doc.clone());
    match reader.analyze(&own, all_modules) {
        Ok(a) => a,
        Err(e) => {
            // Nothing could be loaded (the document itself is unreadable): say so at its start.
            let mut sources = SourceMap::new();
            let file = sources.add_file(doc.to_string_lossy(), String::new());
            let (env, _) = reader.env_for(package_dir.as_deref());
            Analysis::new(
                sources,
                vec![Diagnostic::error(
                    format!("cannot load `{}`: {}", e.path, e.message),
                    Span::new(file, 0, 0),
                )],
                Insight::default(),
                empty_program(),
                env,
                own,
            )
        }
    }
}

/// How many files of the tree are looked at before giving up.
const FILE_CAP: usize = 2000;

/// How many folders of the workspace are looked at for packages that depend on the one
/// being edited.
const FOLDER_CAP: usize = 5000;

/// What a walk for source files found.
#[derive(Default)]
struct Walk {
    /// The canonical path of each file, in the order found.
    files: Vec<PathBuf>,
    seen_files: HashSet<PathBuf>,
    /// The folders already walked, by canonical path.
    seen_dirs: HashSet<PathBuf>,
}

/// Adds the `.colibri` files under `dir` (itself a folder with a manifest, or one without):
/// no skipped folders, no folders with a manifest of their own, and no links to folders (the
/// walk of `colibri check <folder>` does not follow them either; a file behind one is reached
/// where it is, or by an import). `Err` past `FILE_CAP`.
fn source_files(fs: &dyn SourceFs, dir: &Path, walk: &mut Walk) -> Result<(), ()> {
    // `dir` is a canonical path: only a link needs resolving to get one for what is under it.
    if !walk.seen_dirs.insert(dir.to_path_buf()) {
        return Ok(());
    }
    for name in fs.list(dir) {
        let path = dir.join(&name);
        if fs.is_dir(&path) {
            if !fs.is_symlink(&path) && !skipped_folder(fs, &name, &path) {
                source_files(fs, &path, walk)?;
            }
        } else if name.ends_with(".colibri") {
            let c = if fs.is_symlink(&path) {
                canon(&path)
            } else {
                path
            };
            if walk.seen_files.insert(c.clone()) {
                walk.files.push(c);
            }
        }
        if walk.files.len() > FILE_CAP {
            return Err(());
        }
    }
    Ok(())
}

/// The folders under `dir` (itself included) that hold a `colibri.toml`; no skipped folders,
/// no links. `Err` after `FOLDER_CAP` folders.
fn manifest_dirs(
    fs: &dyn SourceFs,
    dir: &Path,
    seen: &mut usize,
    out: &mut Vec<PathBuf>,
) -> Result<(), ()> {
    *seen += 1;
    if *seen > FOLDER_CAP {
        return Err(());
    }
    if fs.is_file(&dir.join(MANIFEST)) {
        out.push(dir.to_path_buf());
    }
    for name in fs.list(dir) {
        let path = dir.join(&name);
        if fs.is_dir(&path) && !fs.is_symlink(&path) && !is_hidden_or_cache(fs, &name, &path) {
            manifest_dirs(fs, &path, seen, out)?;
        }
    }
    Ok(())
}

/// The folder a script `doc` (no `colibri.toml` above it) is analysed from: the workspace
/// folder if it holds `doc`, else the topmost folder of an unbroken chain of folders that
/// hold scripts, going up from `doc`'s own (a file higher up that imports `doc` can only
/// be found there).
fn script_root(fs: &dyn SourceFs, doc: &Path, settings: &Settings) -> PathBuf {
    if let Some(root) = &settings.root
        && doc.starts_with(root)
    {
        return root.clone();
    }
    let mut top = doc.parent().map(Path::to_path_buf).unwrap_or_default();
    while let Some(up) = top.parent() {
        let holds_scripts = fs.list(up).iter().any(|n| n.ends_with(".colibri"));
        if !holds_scripts || fs.is_file(&up.join(MANIFEST)) {
            break;
        }
        top = up.to_path_buf();
    }
    top
}

/// What a request about a name works from: every way the files that can refer to it are read.
pub struct Coverage {
    /// Readings that are loaded but not checked; the first analysis is not among them.
    pub pending: Vec<Pending>,
    /// The folders of packages (inside the workspace folder, or nested in the package) whose files depend on the package of
    /// the document by `path`: their analyses see its items as a dependency's, and a rename
    /// may still edit them.
    pub editable: Vec<PathBuf>,
    /// Why not every file that can use the name was found, if so: an item that other
    /// files could use then cannot be renamed.
    pub incomplete: Option<String>,
}

/// What a loaded reading tells about covering another: the folder its imports are rooted
/// at, the files it holds, and which of them import nothing.
struct Cover {
    root: PathBuf,
    held: HashSet<PathBuf>,
    leaves: HashSet<PathBuf>,
    clean: bool,
}

impl Cover {
    fn of(spec: &Spec, paths: &[PathBuf], loaded: &Loaded) -> Cover {
        Cover {
            root: spec.root(),
            held: paths.iter().cloned().collect(),
            leaves: paths
                .iter()
                .zip(&loaded.metas)
                .filter(|(_, m)| m.imports.is_empty())
                .map(|(p, _)| p.clone())
                .collect(),
            clean: !loaded.has_errors(),
        }
    }
}

/// Every way the files around `doc` are read, besides `first` (the analysis of `doc` itself).
///
/// A `.colibri` file can be read as a module of its package (imports rooted at the package
/// folder), as a script of its own (`colibri run file`: imports rooted at its folder), and as
/// a module of a package that depends on its package. The same `import util` means different
/// files in each. So every file under the tree gets its script reading, and every package
/// with a `main.colibri` its package reading. A script reading is left out only when another
/// reading that loaded without errors rooted its imports at the same folder and holds the
/// file, or when the file imports nothing and such a reading holds it (then there is nothing
/// the root could change).
pub fn coverage(first: &Analysis, doc: &Path, reader: &Reader<'_>) -> Result<Coverage, String> {
    let (fs, settings) = (reader.fs, reader.settings);
    let doc = canon(doc);
    let dir = manifest_dir(fs, &doc);
    let mut editable = Vec::new();
    let mut incomplete = None;
    // Folders to read, whose failure to be read is an error (`hard`) or only a gap.
    let mut tops: Vec<(PathBuf, bool)> = vec![(
        dir.clone()
            .unwrap_or_else(|| script_root(fs, &doc, settings)),
        true,
    )];
    if let Some(package) = &dir {
        // The world of a rename: the workspace folder, and the package's own folder (which
        // holds the packages nested in it, even when the workspace folder is not above it).
        let mut worlds: Vec<&Path> = Vec::new();
        worlds.extend(settings.root.as_deref());
        if settings
            .root
            .as_deref()
            .is_none_or(|root| !package.starts_with(root))
        {
            worlds.push(package);
        }
        let mut found = Vec::new();
        let mut seen = 0;
        let walked = worlds
            .iter()
            .try_for_each(|w| manifest_dirs(fs, w, &mut seen, &mut found));
        found.sort();
        found.dedup();
        if walked.is_err() {
            incomplete = Some(format!(
                "the workspace has more than {FOLDER_CAP} folders, too many to find the packages that depend on this one"
            ));
        } else {
            for dependent in dependents(fs, package, &found, &mut incomplete) {
                tops.push((dependent, true));
            }
            editable.push(package.clone());
        }
    }
    if let Some(root) = &settings.root
        && !tops.iter().any(|(t, _)| t == root)
    {
        tops.push((root.clone(), false));
    }
    let mut walk = Walk::default();
    let mut specs: Vec<Spec> = Vec::new();
    for (top, hard) in &tops {
        if fs.is_file(&top.join(MANIFEST)) && fs.is_file(&top.join(MAIN_FILE)) {
            specs.push(Spec::Package(top.clone()));
        }
        if source_files(fs, top, &mut walk).is_err() {
            if *hard {
                return Err(format!(
                    "the package has more than {FILE_CAP} source files, too many to know every use of a name"
                ));
            }
            incomplete.get_or_insert_with(|| {
                format!(
                    "the workspace has more than {FILE_CAP} source files, too many to know every use of a name"
                )
            });
            // What was found of the workspace is still read.
            walk.files.truncate(FILE_CAP);
        }
    }
    specs.extend(walk.files.iter().cloned().map(Spec::File));
    let mut covers: Vec<Cover> = vec![Cover::of(
        &first.spec,
        &first.module_paths().collect::<Vec<_>>(),
        &first.program,
    )];
    let mut pending: Vec<Pending> = Vec::new();
    for spec in specs {
        if spec == first.spec {
            continue;
        }
        if let Spec::File(file) = &spec
            && covers.iter().any(|c| {
                c.clean
                    && c.held.contains(file)
                    && (c.leaves.contains(file) || Some(c.root.as_path()) == file.parent())
            })
        {
            continue;
        }
        let p = match reader.load(&spec, true) {
            Ok(p) => p,
            Err(e) => {
                return Err(format!(
                    "`{}` cannot be read ({}), so its uses of a name are not known",
                    e.path, e.message
                ));
            }
        };
        covers.push(Cover::of(&spec, p.paths(), &p.loaded));
        pending.push(p);
    }
    Ok(Coverage {
        pending,
        editable,
        incomplete,
    })
}

/// The packages among `found` (folders with a manifest) that depend on `package` by `path`,
/// directly or through another one.
fn dependents(
    fs: &dyn SourceFs,
    package: &Path,
    found: &[PathBuf],
    incomplete: &mut Option<String>,
) -> Vec<PathBuf> {
    let mut wants: Vec<(PathBuf, Vec<PathBuf>)> = Vec::new();
    for dir in found.iter().filter(|d| d.as_path() != package) {
        let path = dir.join(MANIFEST);
        let parsed = fs.read(&path).map_err(|e| e.to_string()).and_then(|text| {
            colibri_check::package::parse_manifest(FileId(0), &text)
                .map_err(|d| d.first().map_or_else(String::new, |d| d.message.clone()))
        });
        match parsed {
            Ok(manifest) => {
                let paths = manifest
                    .dependencies
                    .iter()
                    .filter_map(|d| match &d.source {
                        colibri_check::package::Source::Path { path, .. } => {
                            Some(canon(&dir.join(path)))
                        }
                        _ => None,
                    })
                    .collect();
                wants.push((dir.clone(), paths));
            }
            Err(message) => {
                incomplete.get_or_insert_with(|| {
                    format!(
                        "`{}` cannot be read ({message}), so it is not known whether that package depends on this one",
                        path.display()
                    )
                });
            }
        }
    }
    let mut out: Vec<PathBuf> = Vec::new();
    let mut reach: Vec<&Path> = vec![package];
    loop {
        let more: Vec<&(PathBuf, Vec<PathBuf>)> = wants
            .iter()
            .filter(|(dir, paths)| {
                !out.contains(dir) && paths.iter().any(|p| reach.contains(&p.as_path()))
            })
            .collect();
        if more.is_empty() {
            break;
        }
        for (dir, _) in more {
            out.push(dir.clone());
            reach.push(dir);
        }
    }
    out
}
