//! Hot reload (DESIGN §16.3, ARCHITECTURE §17): noticing that source files changed, comparing
//! the old and the new program, and deciding between patching function bodies and restarting.
//! The `Vm` methods that use these pieces are in `vm.rs`.

use std::cell::RefCell;
use std::collections::hash_map::DefaultHasher;
use std::fmt::Write as _;
use std::hash::Hasher;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use colibri_check::hir::Program;
use colibri_check::package::{Entry, SourceFs};

use crate::error::Report;

/// What [`crate::Vm::reload_changed`] did.
#[derive(Debug)]
pub enum ReloadOutcome {
    /// No source file changed, or the changes did not touch the program's meaning (comments,
    /// blank lines, formatting).
    Unchanged,
    /// Only function bodies changed: the new bodies are in place and the next call of each
    /// uses them. Module-level values and their initialisers were not touched; a coroutine
    /// suspended inside an old body keeps running it.
    Patched {
        /// How many functions got a new body.
        functions: usize,
    },
    /// Something other than a function body changed, so the program was rebuilt in place:
    /// the coroutines were cancelled, the module-level values released, the new program
    /// loaded and its initialisers and top-level statements run. `main` was **not** called:
    /// that is the host's decision (the `colibri run --watch` tool calls it again).
    Restarted {
        /// What forced the restart, in a sentence (`a struct or enum changed`).
        reason: String,
        /// Whether the old program's `save_state()` text was handed to the new program's
        /// `restore_state(s)`.
        state_carried: bool,
    },
    /// The changed sources do not check. Nothing changed: the old program keeps running. The
    /// report holds the diagnostics (and the sources they point into); it is reported once,
    /// and the next call returns [`ReloadOutcome::Unchanged`] until a file changes again.
    CheckFailed(Report),
}

/// How [`crate::Vm::reload_changed_with`] decides between patching and restarting.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReloadOptions {
    /// Restart even when only function bodies changed (what `colibri run --watch` does after a
    /// program ended or panicked: a re-run should start from fresh module state).
    pub force_restart: bool,
}

/// A source file the loaded program was built from.
#[derive(Debug)]
struct WatchedFile {
    path: PathBuf,
    /// What the loader read, as a hash of the bytes.
    hash: u64,
    mtime: Option<SystemTime>,
    len: u64,
    /// Whether `mtime` can be believed. A file written within [`RACY_WINDOW`] of being read
    /// could change again without its modification time moving (the clock is coarse), so
    /// such a file is compared by content on every poll until it has aged.
    trusted: bool,
}

/// Files whose modification time is this recent when read are compared by content.
const RACY_WINDOW: Duration = Duration::from_secs(2);

/// Where a program came from, and the files that went into it.
#[derive(Debug)]
pub(crate) struct ReloadState {
    pub entry: OwnedEntry,
    files: Vec<WatchedFile>,
    /// The comparison data of the running program; computed on the first reload.
    pub fingerprint: Option<Fingerprint>,
}

#[derive(Clone, Debug)]
pub(crate) enum OwnedEntry {
    File(PathBuf),
    Package(PathBuf),
}

impl OwnedEntry {
    pub fn as_entry(&self) -> Entry<'_> {
        match self {
            OwnedEntry::File(p) => Entry::File(p),
            OwnedEntry::Package(p) => Entry::Package(p),
        }
    }
}

impl ReloadState {
    pub fn new(entry: OwnedEntry, reads: Vec<(PathBuf, u64)>) -> Self {
        let mut state = ReloadState {
            entry,
            files: Vec::new(),
            fingerprint: None,
        };
        state.set_files(reads);
        state
    }

    /// Replaces the watched files with what the latest load read.
    pub fn set_files(&mut self, reads: Vec<(PathBuf, u64)>) {
        let mut reads = reads;
        // Even a load that failed before reading the entry should notice it appearing.
        let entry = match &self.entry {
            OwnedEntry::File(p) => p.clone(),
            OwnedEntry::Package(p) => p.join(colibri_check::package::MAIN_FILE),
        };
        if !reads.iter().any(|(p, _)| *p == entry) {
            reads.push((entry, 0));
        }
        let now = SystemTime::now();
        self.files = reads
            .into_iter()
            .map(|(path, hash)| {
                let meta = std::fs::metadata(&path).ok();
                let mtime = meta.as_ref().and_then(|m| m.modified().ok());
                let trusted =
                    mtime.is_some_and(|t| now.duration_since(t).is_ok_and(|age| age > RACY_WINDOW));
                WatchedFile {
                    hash,
                    mtime,
                    len: meta.map_or(0, |m| m.len()),
                    trusted,
                    path,
                }
            })
            .collect();
    }

    /// Whether any watched file differs from what was loaded: a cheap metadata comparison,
    /// and a read and hash only for files whose metadata moved (or cannot be believed).
    pub fn poll_changed(&mut self) -> bool {
        let now = SystemTime::now();
        let mut changed = false;
        for f in &mut self.files {
            let meta = std::fs::metadata(&f.path).ok();
            let mtime = meta.as_ref().and_then(|m| m.modified().ok());
            let len = meta.as_ref().map_or(0, |m| m.len());
            if meta.is_none() {
                // Gone (or being replaced by an editor): the next load reports it.
                changed |= f.mtime.is_some() || f.hash != 0;
                continue;
            }
            if f.trusted && mtime == f.mtime && len == f.len {
                continue;
            }
            match std::fs::read(&f.path) {
                Ok(bytes) => {
                    if hash_bytes(&bytes) != f.hash {
                        changed = true;
                    } else {
                        f.mtime = mtime;
                        f.len = len;
                        f.trusted = mtime
                            .is_some_and(|t| now.duration_since(t).is_ok_and(|a| a > RACY_WINDOW));
                    }
                }
                Err(_) => changed = true,
            }
        }
        changed
    }
}

pub(crate) fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut h = DefaultHasher::new();
    h.write(bytes);
    h.finish()
}

/// A [`SourceFs`] that remembers what was read through it.
pub(crate) struct RecordingFs<'a> {
    inner: &'a dyn SourceFs,
    reads: RefCell<Vec<(PathBuf, u64)>>,
}

impl<'a> RecordingFs<'a> {
    pub fn new(inner: &'a dyn SourceFs) -> Self {
        RecordingFs {
            inner,
            reads: RefCell::new(Vec::new()),
        }
    }

    /// The files read, as absolute paths, each once.
    pub fn into_reads(self) -> Vec<(PathBuf, u64)> {
        let mut out: Vec<(PathBuf, u64)> = Vec::new();
        for (path, hash) in self.reads.into_inner() {
            let path = std::path::absolute(&path).unwrap_or(path);
            match out.iter_mut().find(|(p, _)| *p == path) {
                Some(slot) => slot.1 = hash,
                None => out.push((path, hash)),
            }
        }
        out
    }
}

impl SourceFs for RecordingFs<'_> {
    fn read(&self, path: &Path) -> Result<String, String> {
        let text = self.inner.read(path)?;
        self.reads
            .borrow_mut()
            .push((path.to_path_buf(), hash_bytes(text.as_bytes())));
        Ok(text)
    }

    fn is_file(&self, path: &Path) -> bool {
        self.inner.is_file(path)
    }

    fn is_dir(&self, path: &Path) -> bool {
        self.inner.is_dir(path)
    }

    fn list(&self, dir: &Path) -> Vec<String> {
        self.inner.list(dir)
    }
}

/// What makes two programs "the same declarations" for patching, in labelled parts, plus one
/// hash per function body.
///
/// Every part is a hash of the `Debug` text of the HIR with source spans removed, so moving
/// code around in a file changes nothing. Ids are positions in the program's tables, so equal
/// parts mean every `FnId`, `GlobalId` and `TypeId` denotes the same thing in both programs.
#[derive(Debug, Clone)]
pub(crate) struct Fingerprint {
    /// `(what, hash)`; `what` finishes the sentence "... changed".
    pub parts: Vec<(&'static str, u64)>,
    /// One per entry of `Program::fns`.
    pub bodies: Vec<u64>,
}

impl Fingerprint {
    pub fn of(program: &Program) -> Fingerprint {
        let mut buf = String::new();
        let mut hash_debug = |value: &dyn std::fmt::Debug| -> u64 {
            buf.clear();
            let _ = write!(buf, "{value:?}");
            hash_without_spans(&buf)
        };
        let fns: Vec<(&str, &colibri_check::types::FnSig, u32, Option<_>)> = program
            .fns
            .iter()
            .map(|f| (f.name.as_str(), &f.sig, f.type_params, f.host))
            .collect();
        let globals: Vec<_> = program
            .globals
            .iter()
            .map(|g| (&g.name, &g.ty, g.mutable))
            .collect();
        let inits: Vec<u64> = program
            .init_order
            .iter()
            .map(|&g| hash_debug(&*program.globals[g.0 as usize].init))
            .collect();
        let tests: Vec<_> = program
            .tests
            .iter()
            .map(|t| (&t.name, &t.module, t.root, t.f))
            .collect();
        let parts = vec![
            (
                "the functions or their signatures",
                hash_debug(&(&fns, program.main)),
            ),
            (
                "a struct, class, enum or interface",
                hash_debug(&program.types),
            ),
            ("an interface implementation", hash_debug(&program.impls)),
            (
                "the module-level declarations",
                hash_debug(&(&globals, &program.init_order)),
            ),
            (
                "a module-level initialiser or top-level statement",
                hash_debug(&(&inits, &*program.script)),
            ),
            (
                "the host interface or the tests",
                hash_debug(&(&program.host_fns, program.host_types, &tests)),
            ),
        ];
        let bodies = program.fns.iter().map(|f| hash_debug(&*f.body)).collect();
        Fingerprint { parts, bodies }
    }

    /// The first part that differs, if any.
    pub fn declaration_difference(&self, other: &Fingerprint) -> Option<&'static str> {
        if self.bodies.len() != other.bodies.len() {
            return Some("the functions or their signatures");
        }
        self.parts
            .iter()
            .zip(&other.parts)
            .find(|(a, b)| a.1 != b.1)
            .map(|(a, _)| a.0)
    }

    /// How many function bodies differ (same length assumed).
    pub fn changed_bodies(&self, other: &Fingerprint) -> usize {
        self.bodies
            .iter()
            .zip(&other.bodies)
            .filter(|(a, b)| a != b)
            .count()
    }
}

/// Hashes `text` with every `Span { file: FileId(n), start: n, end: n }` left out.
fn hash_without_spans(text: &str) -> u64 {
    const SPAN: &str = "Span { file: FileId(";
    let mut h = DefaultHasher::new();
    let mut rest = text;
    while let Some(at) = rest.find(SPAN) {
        h.write(&rest.as_bytes()[..at]);
        let after = &rest[at + SPAN.len()..];
        match after.find('}') {
            Some(end) => rest = &after[end + 1..],
            None => {
                rest = "";
            }
        }
    }
    h.write(rest.as_bytes());
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spans_do_not_count() {
        let a = "Foo { x: 1, span: Span { file: FileId(0), start: 3, end: 9 } }";
        let b = "Foo { x: 1, span: Span { file: FileId(7), start: 30, end: 90 } }";
        let c = "Foo { x: 2, span: Span { file: FileId(0), start: 3, end: 9 } }";
        assert_eq!(hash_without_spans(a), hash_without_spans(b));
        assert_ne!(hash_without_spans(a), hash_without_spans(c));
    }
}
