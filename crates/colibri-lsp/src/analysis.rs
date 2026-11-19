//! Checking the package a document belongs to, with unsaved buffers laid over the files on
//! disk, and keeping what hover and definition need.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use colibri_check::HostEnv;
use colibri_check::insight::Insight;
use colibri_check::package::{DiskFs, Entry, MAIN_FILE, SourceFs, load, normalize};
use colibri_syntax::{Diagnostic, FileId, SourceMap, Span};

/// The manifest's file name.
const MANIFEST: &str = "colibri.toml";

/// Unsaved buffers, by normalised path, over the disk.
pub struct Overlay<'a> {
    pub buffers: &'a HashMap<PathBuf, String>,
}

impl SourceFs for Overlay<'_> {
    fn read(&self, path: &Path) -> Result<String, String> {
        match self.buffers.get(&normalize(path)) {
            Some(text) => Ok(text.clone()),
            None => DiskFs.read(path),
        }
    }

    fn is_file(&self, path: &Path) -> bool {
        self.buffers.contains_key(&normalize(path)) || DiskFs.is_file(path)
    }

    fn is_dir(&self, path: &Path) -> bool {
        let dir = normalize(path);
        self.buffers
            .keys()
            .any(|f| f.parent().is_some_and(|p| p.starts_with(&dir)))
            || DiskFs.is_dir(path)
    }

    fn list(&self, dir: &Path) -> Vec<String> {
        let dir_norm = normalize(dir);
        let mut names = DiskFs.list(dir);
        names.extend(self.buffers.keys().filter_map(|f| {
            let rest = f.strip_prefix(&dir_norm).ok()?;
            Some(
                rest.components()
                    .next()?
                    .as_os_str()
                    .to_string_lossy()
                    .into_owned(),
            )
        }));
        names.sort();
        names.dedup();
        names
    }
}

/// The result of checking a program: everything the server answers from.
pub struct Analysis {
    pub sources: SourceMap,
    pub diagnostics: Vec<Diagnostic>,
    pub insight: Insight,
}

impl Analysis {
    /// Every file of the program, with the path it was read from.
    pub fn files(&self) -> Vec<(FileId, PathBuf)> {
        (0..)
            .map(FileId)
            .take_while(|f| self.sources.contains(*f))
            .map(|f| (f, normalize(Path::new(self.sources.name(f)))))
            .collect()
    }

    pub fn file_of(&self, path: &Path) -> Option<FileId> {
        let path = normalize(path);
        self.files()
            .into_iter()
            .find(|(_, p)| *p == path)
            .map(|(f, _)| f)
    }
}

/// How the program around a document is found and checked.
pub struct Settings {
    pub stack_budget: usize,
    /// Interface (`.cbi`) files named by the client.
    pub interfaces: Vec<PathBuf>,
    /// The folder the client opened, if any.
    pub root: Option<PathBuf>,
}

/// What the host interfaces gave: the environment, and what could not be read.
pub struct Hosts {
    pub env: HostEnv,
    pub problems: Vec<(PathBuf, String)>,
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

/// Checks the program `doc` belongs to: the package of the nearest `colibri.toml`, or the
/// `main.colibri` of its folder, if they reach `doc`; else `doc` as a script of its own.
pub fn analyze(doc: &Path, fs: &dyn SourceFs, settings: &Settings) -> (Analysis, Hosts) {
    let doc = normalize(doc);
    let package_dir = manifest_dir(fs, &doc);
    let hosts = host_env(settings, fs, package_dir.as_deref());
    let folder_main = doc
        .parent()
        .map(|d| d.join(MAIN_FILE))
        .filter(|m| *m != doc && fs.is_file(m));
    let mut entries: Vec<Entry<'_>> = Vec::new();
    if let Some(dir) = &package_dir
        && fs.is_file(&dir.join(MAIN_FILE))
        && *dir != doc
    {
        entries.push(Entry::Package(dir));
    }
    if let Some(main) = &folder_main {
        entries.push(Entry::File(main));
    }
    let modules = hosts.env.named_modules();
    let run = |entry: Entry<'_>| {
        let mut sources = SourceMap::new();
        load(fs, &mut sources, entry, &modules).map(|loaded| {
            let mut diagnostics = loaded.diagnostics.clone();
            let mut insight = Insight::default();
            if !loaded.has_errors() {
                let (checked, found) = colibri_check::check_modules_with_insight(
                    &loaded.inputs(),
                    &loaded.packages,
                    &hosts.env,
                    settings.stack_budget,
                );
                diagnostics = checked.diagnostics;
                insight = found;
            }
            Analysis {
                sources,
                diagnostics,
                insight,
            }
        })
    };
    let last = Entry::File(&doc);
    for entry in entries {
        if let Ok(analysis) = run(entry)
            && analysis.file_of(&doc).is_some()
        {
            return (analysis, hosts);
        }
    }
    let analysis = run(last).unwrap_or_else(|e| {
        // Nothing could be loaded (the document itself is unreadable): say so at its start.
        let mut sources = SourceMap::new();
        let file = sources.add_file(doc.to_string_lossy(), String::new());
        Analysis {
            sources,
            diagnostics: vec![Diagnostic::error(
                format!("cannot load `{}`: {}", e.path, e.message),
                Span::new(file, 0, 0),
            )],
            insight: Insight::default(),
        }
    });
    (analysis, hosts)
}
