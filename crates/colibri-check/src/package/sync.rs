//! Resolving git dependencies and keeping `colibri.lock` in step with `colibri.toml`
//! (ARCHITECTURE §11.6b). This is the only place that may contact the network, and only the
//! command line tool calls it, before it loads a program: the loader and the language server
//! read the lock and the cache and never fetch.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use colibri_syntax::FileId;

use super::fs::{canonical_path, leaves_repository, normalize};
use super::git::{self, Git};
use super::loader::MANIFEST_FILE;
use super::lock::{self, LOCK_FILE, Lock, LockEntry};
use super::manifest::{self, GitSource, Source, UrlKind, classify_url};

/// Which locked dependencies to resolve again, as `colibri update` does.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Update {
    /// Only what is new or changed in the manifest.
    #[default]
    Nothing,
    All,
    Names(Vec<String>),
}

#[derive(Clone, Debug, Default)]
pub struct SyncOptions {
    pub update: Update,
    /// Fail rather than change `colibri.lock`.
    pub locked: bool,
    /// Never contact the network: use the cache.
    pub offline: bool,
}

/// What a sync did, for the tool to tell the user.
#[derive(Clone, Debug, Default)]
pub struct SyncReport {
    /// One line per dependency that was newly locked or moved.
    pub notes: Vec<String>,
    pub lock_written: bool,
}

/// Makes `colibri.lock` next to `root/colibri.toml` list the commit of every git dependency of
/// the package at `root` (direct and transitive), fetching what is not in the cache.
///
/// A folder without a manifest, and a manifest that does not parse, are not this function's
/// business: it does nothing and the loader reports them.
pub fn sync(root: &Path, options: &SyncOptions) -> Result<SyncReport, String> {
    sync_in(root, options, git::cache_root().ok())
}

fn sync_in(
    root: &Path,
    options: &SyncOptions,
    cache: Option<PathBuf>,
) -> Result<SyncReport, String> {
    let lock_path = root.join(LOCK_FILE);
    // A lock that is there but cannot be read or understood is an error, never "no lock":
    // the file would be overwritten, and with it the commits it pinned.
    let old_text = match std::fs::read(&lock_path) {
        Ok(bytes) => Some(String::from_utf8(bytes).map_err(|_| {
            format!(
                "`{}` is not valid UTF-8 ({})",
                lock_path.display(),
                lock::RECREATE_HINT
            )
        })?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            return Err(format!(
                "cannot read `{}`: {e} ({})",
                lock_path.display(),
                lock::RECREATE_HINT
            ));
        }
    };
    let old = match &old_text {
        Some(text) => Lock::parse(FileId(0), text).map_err(|d| {
            format!(
                "{}: {}",
                lock_path.display(),
                lock::describe_error(text, &d)
            )
        })?,
        None => Lock::default(),
    };
    let mut s = Sync {
        git: cache.map(|c| Git::new(c, options.offline)),
        options,
        old,
        new: Lock::default(),
        visited: HashSet::new(),
        report: SyncReport::default(),
        updated: HashSet::new(),
        asked: HashMap::new(),
    };
    if options.locked && options.update == Update::Nothing {
        // What the lock cannot vouch for is known from the manifests the user wrote, before
        // anything is fetched for the dependencies that come first.
        s.check_locked(root, &mut HashSet::new())?;
    }
    s.visit(root, None, None)?;
    if let Update::Names(names) = &options.update {
        for n in names {
            if !s.new.entries.contains_key(n) {
                return Err(format!(
                    "`{n}` is not a git dependency of this package (the git dependencies are: {})",
                    list(s.new.entries.keys().map(String::as_str))
                ));
            }
        }
    }
    let text = s.new.render();
    let unchanged = match &old_text {
        // A checkout that turned the lines into `\r\n` (git's `autocrlf` on Windows) is the
        // same file.
        Some(t) => t.replace("\r\n", "\n") == text,
        None => s.new.entries.is_empty(),
    };
    if !unchanged {
        if options.locked {
            return Err(format!(
                "`{LOCK_FILE}` needs to change, but `--locked` was given: run `colibri update` or `colibri check` without it"
            ));
        }
        std::fs::write(&lock_path, text)
            .map_err(|e| format!("cannot write `{}`: {e}", lock_path.display()))?;
        s.report.lock_written = true;
    }
    Ok(s.report)
}

fn list<'a>(names: impl Iterator<Item = &'a str>) -> String {
    let names: Vec<String> = names.map(|n| format!("`{n}`")).collect();
    if names.is_empty() {
        "none".to_string()
    } else {
        names.join(", ")
    }
}

/// Where the package being looked at was fetched from, when it was fetched.
#[derive(Clone)]
struct Origin {
    url: String,
    kind: UrlKind,
}

struct Sync<'a> {
    git: Option<Git>,
    options: &'a SyncOptions,
    old: Lock,
    new: Lock,
    visited: HashSet<PathBuf>,
    report: SyncReport,
    updated: HashSet<String>,
    /// The package that first asked for each git dependency of this run.
    asked: HashMap<String, String>,
}

impl Sync<'_> {
    /// For `--locked`: fails if a git dependency named by the manifest in `dir` (or by one of
    /// the path dependencies of the same tree) is not in the lock, or is locked for another
    /// url or another revision. Nothing is fetched.
    fn check_locked(&self, dir: &Path, seen: &mut HashSet<PathBuf>) -> Result<(), String> {
        if !seen.insert(normalize(dir)) {
            return Ok(());
        }
        let Ok(text) = std::fs::read_to_string(dir.join(MANIFEST_FILE)) else {
            return Ok(());
        };
        refuse_credentials(dir, &text)?;
        let Ok(manifest) = manifest::parse(FileId(0), &text) else {
            return Ok(());
        };
        for dep in &manifest.dependencies {
            match &dep.source {
                Source::Path { path, .. } => {
                    self.check_locked(&normalize(&dir.join(path)), seen)?
                }
                Source::Git(source) => {
                    let vouched = self
                        .old
                        .entries
                        .get(&dep.name)
                        .is_some_and(|e| e.url == source.url && e.asks(&source.spec));
                    if !vouched {
                        return Err(format!(
                            "the dependency `{}` is not in `{LOCK_FILE}` (or differs from it), but `--locked` was given: run `colibri check` or `colibri update` without it",
                            dep.name
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    /// Looks at the manifest in `dir` and follows its dependencies. `git_root` is the checkout
    /// that `dir` is inside, if any: path dependencies may not leave it.
    fn visit(
        &mut self,
        dir: &Path,
        git_root: Option<&Path>,
        origin: Option<&Origin>,
    ) -> Result<(), String> {
        let key = canonical_path(dir);
        if !self.visited.insert(key) {
            return Ok(());
        }
        let Ok(text) = std::fs::read_to_string(dir.join(MANIFEST_FILE)) else {
            return Ok(());
        };
        refuse_credentials(dir, &text)?;
        let manifest = match manifest::parse(FileId(0), &text) {
            Ok(m) => m,
            // `run` and `check` leave the report to the loader, which shows the source line;
            // `update` would otherwise write a lock without the dependencies it could not read.
            Err(errors) if self.options.update != Update::Nothing => {
                return Err(manifest_errors(dir, &text, &errors));
            }
            Err(_) => return Ok(()),
        };
        for dep in &manifest.dependencies {
            match &dep.source {
                Source::Path { path, .. } => {
                    let target = normalize(&dir.join(path));
                    // By the text first: `inside` resolves links, which looks at the disk.
                    if let Some(root) = git_root
                        && (leaves_repository(root, dir, path) || !inside(&target, root))
                    {
                        continue; // the loader says why
                    }
                    self.visit(&target, git_root, origin)?;
                }
                Source::Git(source) => {
                    let kind = classify_url(&source.url).map_err(|e| {
                        format!("the dependency `{}` of `{}`: {e}", dep.name, manifest.name)
                    })?;
                    // What was fetched from the network may not make the tool fetch from the
                    // user's own disk: a transitive manifest could name `file:///home/me/private`.
                    if let Some(from) = origin
                        && from.kind == UrlKind::Remote
                        && kind == UrlKind::Local
                    {
                        return Err(format!(
                            "the package `{}`, fetched from `{}`, depends on `{}` at the local repository `{}`: a package from a remote url may not use a local path or a `file://` url",
                            manifest.name,
                            git::redact_url(&from.url),
                            dep.name,
                            git::redact_url(&source.url)
                        ));
                    }
                    let checkout = self.git_dependency(&manifest.name, &dep.name, source)?;
                    if !checkout.join(MANIFEST_FILE).is_file() {
                        let commit = &self.new.entries[&dep.name].commit;
                        if self
                            .git
                            .as_ref()
                            .is_some_and(|g| g.is_symlink_at(&source.url, commit, MANIFEST_FILE))
                        {
                            return Err(format!(
                                "the repository `{}` (the dependency `{}`) has a `{MANIFEST_FILE}` that is a symbolic link at its commit {}: symbolic links in dependencies are ignored, so the package has no manifest; the repository must hold the file itself",
                                git::redact_url(&source.url),
                                dep.name,
                                short(commit)
                            ));
                        }
                        return Err(format!(
                            "the repository `{}` (the dependency `{}`) has no `{MANIFEST_FILE}` at its commit {}",
                            git::redact_url(&source.url),
                            dep.name,
                            short(&self.new.entries[&dep.name].commit)
                        ));
                    }
                    let next = Origin {
                        url: source.url.clone(),
                        kind,
                    };
                    self.visit(&checkout, Some(&checkout), Some(&next))?;
                }
            }
        }
        Ok(())
    }

    /// Pins the git dependency `name` and makes sure its files are in the cache.
    ///
    /// `dependent` is the package whose manifest lists it. A package that several dependents
    /// need is one package when they all mean the same repository url and commit, however they
    /// spell it (`tag = "v1"` here, `branch = "main"` there).
    fn git_dependency(
        &mut self,
        dependent: &str,
        name: &str,
        source: &GitSource,
    ) -> Result<PathBuf, String> {
        let context = |e: String| format!("the dependency `{name}`: {e}");
        let wants_update = match &self.options.update {
            Update::Nothing => false,
            Update::All => true,
            Update::Names(names) => names.iter().any(|n| n == name),
        };
        if let Some(prev) = self.new.entries.get(name).cloned() {
            let git = self.git.as_mut().ok_or_else(|| context(git::no_cache()))?;
            // The lock already vouches for this spec meaning the commit: no need to ask git.
            let vouched = !wants_update
                && self.old.entries.get(name).is_some_and(|o| {
                    o.url == source.url
                        && o.commit == prev.commit
                        && o.asks(&source.spec)
                        && o.commit_fits(&source.spec)
                });
            let commit = if (prev.url == source.url
                && prev.asks(&source.spec)
                && prev.commit_fits(&source.spec))
                || vouched
            {
                prev.commit.clone()
            } else if self.options.locked && !wants_update {
                // Another package asks for it differently than the lock records: the lock
                // would have to change, and asking git cannot help.
                return Err(context(format!(
                    "another package asks for it as {}, which `{LOCK_FILE}` does not record, but `--locked` was given: run `colibri update` or `colibri check` without it",
                    source.spec.describe()
                )));
            } else {
                git.resolve(&source.url, &source.spec).map_err(context)?
            };
            if prev.url != source.url || prev.commit != commit {
                let first = self.asked.get(name).cloned().unwrap_or_default();
                return Err(format!(
                    "two different versions of the package `{name}` are needed: `{first}` asks for `{}` at {} ({}), and `{dependent}` asks for `{}` at {} ({})",
                    git::redact_url(&prev.url),
                    short(&prev.commit),
                    prev.spec.describe(),
                    git::redact_url(&source.url),
                    short(&commit),
                    source.spec.describe()
                ));
            }
            if let Some(entry) = self.new.entries.get_mut(name) {
                entry.add_also(source.spec.clone());
            }
            return git.checkout(&source.url, &commit).map_err(context);
        }
        let kept = self
            .old
            .entries
            .get(name)
            .filter(|e| e.matches(source) && !wants_update)
            .cloned();
        if kept.is_none()
            && !wants_update
            && let Some(e) = self.old.entries.get(name)
            && e.url == source.url
            && e.asks(&source.spec)
            && !e.commit_fits(&source.spec)
        {
            // The text of the lock was edited to point a `rev` somewhere else.
            if self.options.locked {
                return Err(context(format!(
                    "its entry in `{LOCK_FILE}` is the commit {}, which does not start with the rev `{}`, but `--locked` was given: run `colibri update` or `colibri check` without it",
                    short(&e.commit),
                    source.spec.key_value().map_or("", |(_, v)| v)
                )));
            }
            self.report.notes.push(format!(
                "the locked commit {} of {name} does not match its {}: resolving it again",
                short(&e.commit),
                source.spec.describe()
            ));
        }
        let git = self.git.as_mut().ok_or_else(|| context(git::no_cache()))?;
        let commit = match &kept {
            Some(e) => e.commit.clone(),
            None => {
                if self.options.locked && !wants_update {
                    return Err(format!(
                        "the dependency `{name}` is not in `{LOCK_FILE}` (or differs from it), but `--locked` was given: run `colibri check` or `colibri update` without it"
                    ));
                }
                git.resolve(&source.url, &source.spec).map_err(context)?
            }
        };
        let dir = git.checkout(&source.url, &commit).map_err(context)?;
        let entry = LockEntry {
            url: source.url.clone(),
            spec: source.spec.clone(),
            also: Vec::new(),
            commit,
        };
        if self.updated.insert(name.to_string()) {
            match self.old.entries.get(name) {
                None => self.report.notes.push(format!(
                    "locking {name} to {} ({})",
                    short(&entry.commit),
                    source.spec.describe()
                )),
                Some(prev) if prev.commit != entry.commit => self.report.notes.push(format!(
                    "updating {name}: {} -> {} ({})",
                    short(&prev.commit),
                    short(&entry.commit),
                    source.spec.describe()
                )),
                Some(_) => {}
            }
        }
        self.asked.insert(name.to_string(), dependent.to_string());
        self.new.entries.insert(name.to_string(), entry);
        Ok(dir)
    }
}

/// A manifest with a credential written in it is refused as a whole, whatever the command:
/// nothing is fetched and no lock is written from it. The message gives the lines, never the
/// text.
fn refuse_credentials(dir: &Path, text: &str) -> Result<(), String> {
    let (_, credentials) = git::credential_ranges(text);
    let Some(&(first, _)) = credentials.first() else {
        return Ok(());
    };
    let line = text.as_bytes()[..first.min(text.len())]
        .iter()
        .filter(|&&b| b == b'\n')
        .count()
        + 1;
    Err(format!(
        "{}:{line}: a credential is written in `{MANIFEST_FILE}` ({} place{}): the file is refused, nothing was fetched and `{LOCK_FILE}` was not written; a url may not hold a user name and password, use a git credential helper or an ssh key",
        dir.join(MANIFEST_FILE).display(),
        credentials.len(),
        if credentials.len() == 1 { "" } else { "s" }
    ))
}

/// The errors of the manifest in `dir` as text: each with its line, and no credentials.
fn manifest_errors(dir: &Path, text: &str, errors: &[colibri_syntax::Diagnostic]) -> String {
    let text = git::mask_source_text(text);
    let file = dir.join(MANIFEST_FILE);
    let lines: Vec<String> = errors
        .iter()
        .map(|d| {
            let start = (d.span.start as usize).min(text.len());
            let line = text.as_bytes()[..start]
                .iter()
                .filter(|&&b| b == b'\n')
                .count()
                + 1;
            let hint = d
                .hint
                .as_ref()
                .map(|h| format!(" ({h})"))
                .unwrap_or_default();
            format!(
                "{}:{line}: {}{hint}",
                file.display(),
                git::redact(&d.message)
            )
        })
        .collect();
    format!(
        "cannot read the manifest, so no dependency was resolved and `{LOCK_FILE}` was not written:\n  {}",
        lines.join("\n  ")
    )
}

/// The first digits of a commit hash, as people write them.
fn short(commit: &str) -> &str {
    &commit[..commit.len().min(10)]
}

/// Whether `path` is inside `root`, links resolved where they exist.
pub(super) fn inside(path: &Path, root: &Path) -> bool {
    canonical_path(path).starts_with(canonical_path(root))
}
