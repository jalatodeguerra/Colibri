//! Where source files come from: the real file system, or a set of files in memory (for hosts
//! and tests that hand over a whole program as text).

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

/// Reads source files and answers the questions import resolution asks about the tree.
pub trait SourceFs {
    /// The text of a file, or why it could not be read.
    fn read(&self, path: &Path) -> Result<String, String>;
    /// The first `n` bytes of a file (fewer if it is shorter), or `None` if it cannot be read.
    /// Reads no more than that where the system allows.
    fn read_prefix(&self, path: &Path, n: usize) -> Option<Vec<u8>> {
        let text = self.read(path).ok()?;
        Some(text.as_bytes()[..n.min(text.len())].to_vec())
    }
    fn is_file(&self, path: &Path) -> bool;
    fn is_dir(&self, path: &Path) -> bool;
    /// The names of the entries of a folder, sorted (empty if it is not one).
    fn list(&self, dir: &Path) -> Vec<String>;
    /// Whether the path is itself a symbolic link (a folder walk does not follow those).
    fn is_symlink(&self, _path: &Path) -> bool {
        false
    }
    /// The path with links resolved, so two names of one file compare equal. Where there are no
    /// links this is [`normalize`].
    fn canonical(&self, path: &Path) -> PathBuf {
        normalize(path)
    }
    /// How diagnostics and stack traces name the file at `path`. For files on disk it is the
    /// path the tool built, shown the system's way (`dir\\lib.colibri` on Windows). Files that
    /// a host named keep the name it gave them (see [`MemoryFs`]).
    fn display_name(&self, path: &Path) -> String {
        path.display().to_string()
    }
    /// The folder that holds the checkouts of git dependencies (`git/checkouts/..` below it),
    /// or why there is none. The cache of the system by default; a program held in memory
    /// brings its own (see [`MemoryFs::with_cache_root`]).
    fn cache_root(&self) -> Result<PathBuf, String> {
        super::git::cache_root()
    }
}

/// A [`SourceFs`] that remembers every file that was read through it, with its text: what a
/// load of a program reads is exactly the program (its modules, manifests, lock file and the
/// sources of its dependencies). Looking a file up (`is_file`, `list`) or reading its first
/// bytes (`read_prefix`) is not a read.
pub struct RecordingFs<'a> {
    inner: &'a dyn SourceFs,
    reads: std::cell::RefCell<Vec<Recorded>>,
}

/// A file a [`RecordingFs`] saw read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recorded {
    /// The path the reader asked for, as it asked.
    pub path: PathBuf,
    /// What the inner file system calls the file in messages ([`SourceFs::display_name`]).
    pub name: String,
    /// Its text when it was read.
    pub text: String,
}

impl<'a> RecordingFs<'a> {
    pub fn new(inner: &'a dyn SourceFs) -> Self {
        RecordingFs {
            inner,
            reads: std::cell::RefCell::new(Vec::new()),
        }
    }

    /// The files read, in the order they were first read, each once (a file read again keeps
    /// the text of its last read). Two spellings of one path (`a/./b`, `a/b`) are one file.
    pub fn into_files(self) -> Vec<Recorded> {
        let mut out: Vec<(PathBuf, Recorded)> = Vec::new();
        for read in self.reads.into_inner() {
            let key = normalize(&read.path);
            match out.iter_mut().find(|(k, _)| *k == key) {
                Some(slot) => slot.1 = read,
                None => out.push((key, read)),
            }
        }
        out.into_iter().map(|(_, r)| r).collect()
    }
}

impl SourceFs for RecordingFs<'_> {
    fn read(&self, path: &Path) -> Result<String, String> {
        let text = self.inner.read(path)?;
        self.reads.borrow_mut().push(Recorded {
            path: path.to_path_buf(),
            name: self.inner.display_name(path),
            text: text.clone(),
        });
        Ok(text)
    }

    /// Forwarded as it is: the start of a file (a `CACHEDIR.TAG`) is not a read of the program,
    /// and the default would read the whole file and record it.
    fn read_prefix(&self, path: &Path, n: usize) -> Option<Vec<u8>> {
        self.inner.read_prefix(path, n)
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

    fn is_symlink(&self, path: &Path) -> bool {
        self.inner.is_symlink(path)
    }

    fn canonical(&self, path: &Path) -> PathBuf {
        self.inner.canonical(path)
    }

    fn display_name(&self, path: &Path) -> String {
        self.inner.display_name(path)
    }

    fn cache_root(&self) -> Result<PathBuf, String> {
        self.inner.cache_root()
    }
}

/// The operating system's files.
#[derive(Clone, Copy, Debug, Default)]
pub struct DiskFs;

impl SourceFs for DiskFs {
    fn read(&self, path: &Path) -> Result<String, String> {
        std::fs::read_to_string(path).map_err(|e| e.to_string())
    }

    fn read_prefix(&self, path: &Path, n: usize) -> Option<Vec<u8>> {
        use std::io::Read;
        let mut out = Vec::new();
        std::fs::File::open(path)
            .ok()?
            .take(n as u64)
            .read_to_end(&mut out)
            .ok()?;
        Some(out)
    }

    fn is_file(&self, path: &Path) -> bool {
        path.is_file()
    }

    fn is_dir(&self, path: &Path) -> bool {
        or_dot(path).is_dir()
    }

    fn is_symlink(&self, path: &Path) -> bool {
        std::fs::symlink_metadata(or_dot(path)).is_ok_and(|m| m.file_type().is_symlink())
    }

    fn canonical(&self, path: &Path) -> PathBuf {
        canonical_path(path)
    }

    fn list(&self, dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(or_dot(dir))
            .map(|entries| {
                entries
                    .flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }
}

/// The empty path is the current folder.
fn or_dot(path: &Path) -> &Path {
    if path.as_os_str().is_empty() {
        Path::new(".")
    } else {
        path
    }
}

/// Files held in memory, keyed by their normalised path.
#[derive(Clone, Debug, Default)]
pub struct MemoryFs {
    files: BTreeMap<PathBuf, MemoryFile>,
    cache_root: Option<PathBuf>,
}

/// A file held in memory: the name the host gave it and its text.
#[derive(Clone, Debug)]
struct MemoryFile {
    name: String,
    text: String,
}

impl MemoryFs {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a file (a later one with the same path replaces it). The file keeps the name given
    /// here for diagnostics, however an `import` reaches it (the resolver joins folders with
    /// the system's separator, which is not what the host wrote on Windows).
    pub fn insert(&mut self, path: impl AsRef<Path>, text: impl Into<String>) {
        let path = path.as_ref();
        let file = MemoryFile {
            name: path.to_string_lossy().into_owned(),
            text: text.into(),
        };
        self.files.insert(normalize(path), file);
    }

    /// [`MemoryFs::insert`] for a file that is known by another name in messages than the
    /// path it is found at: a program that was packed up keeps the names its files had.
    pub fn insert_named(
        &mut self,
        path: impl AsRef<Path>,
        name: impl Into<String>,
        text: impl Into<String>,
    ) {
        let file = MemoryFile {
            name: name.into(),
            text: text.into(),
        };
        self.files.insert(normalize(path.as_ref()), file);
    }

    /// Sets the folder the checkouts of git dependencies are looked for in, instead of the
    /// cache of the system: a packed program holds them itself.
    pub fn with_cache_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.cache_root = Some(root.into());
        self
    }
}

impl SourceFs for MemoryFs {
    fn read(&self, path: &Path) -> Result<String, String> {
        self.files
            .get(&normalize(path))
            .map(|f| f.text.clone())
            .ok_or_else(|| "no such file".to_string())
    }

    fn is_file(&self, path: &Path) -> bool {
        self.files.contains_key(&normalize(path))
    }

    fn is_dir(&self, path: &Path) -> bool {
        let dir = normalize(path);
        self.files
            .keys()
            .any(|f| f.parent().is_some_and(|p| p.starts_with(&dir)) && *f != dir)
    }

    fn cache_root(&self) -> Result<PathBuf, String> {
        match &self.cache_root {
            Some(root) => Ok(root.clone()),
            None => super::git::cache_root(),
        }
    }

    fn display_name(&self, path: &Path) -> String {
        match self.files.get(&normalize(path)) {
            Some(file) => file.name.clone(),
            // A folder, or a file that is not there (a "looked for" hint): in memory there is
            // no system's way, so `/`.
            None => path.to_string_lossy().replace('\\', "/"),
        }
    }

    fn list(&self, dir: &Path) -> Vec<String> {
        let dir = normalize(dir);
        let mut names: Vec<String> = self
            .files
            .keys()
            .filter_map(|f| {
                let rest = f.strip_prefix(&dir).ok()?;
                let first = rest.components().next()?;
                Some(first.as_os_str().to_string_lossy().into_owned())
            })
            .collect();
        names.sort();
        names.dedup();
        names
    }
}

/// Removes `.` and resolves `..` against the components before it, without touching the file
/// system. `.` alone becomes the empty path (so a file in it prints without a prefix).
pub fn normalize(path: &Path) -> PathBuf {
    let mut out: Vec<Component<'_>> = Vec::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => match out.last() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                Some(Component::RootDir) => {}
                _ => out.push(c),
            },
            other => out.push(other),
        }
    }
    out.iter().collect()
}

/// `path` with its links resolved where it exists, else only [`normalize`]d. On Windows
/// `canonicalize` gives the verbatim form (`\\?\C:\dir`), which would not compare equal to
/// the plain form of a path that is not there (yet), and which no message should show: the
/// plain one is returned.
pub fn canonical_path(path: &Path) -> PathBuf {
    try_canonical_path(path).unwrap_or_else(|_| normalize(path))
}

/// [`canonical_path`] for a path that must be on disk.
pub fn try_canonical_path(path: &Path) -> std::io::Result<PathBuf> {
    std::fs::canonicalize(or_dot(path)).map(|p| without_verbatim(&p))
}

/// `\\?\C:\dir` as `C:\dir` and `\\?\UNC\server\share` as `\\server\share`, when that is safe;
/// any other path, and any path whose plain form would name something else, as it is (see
/// [`plain_form`]).
fn without_verbatim(path: &Path) -> PathBuf {
    path.to_str()
        .and_then(plain_form)
        .map_or_else(|| path.to_path_buf(), PathBuf::from)
}

/// The plain form of a verbatim path, or `None` when it is not one or the plain form is not
/// the same place. The verbatim prefix switches off what Windows does to a name, so it may only
/// be taken off a path that would be read the same without it. A name that ends in `.` or a
/// space (the plain form drops that), is a device (`CON`, `PRN`, `AUX`, `NUL`, `COM1` to
/// `COM9`, `LPT1` to `LPT9`, with or without an extension, in any case) or a path of 260
/// UTF-16 units or more (past the limit of the plain form) keeps it. Works on the text, with `\`
/// between the pieces, so it is the same on every system.
fn plain_form(text: &str) -> Option<String> {
    let (plain_prefix, rest, skip) = if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        (r"\\", rest, 0)
    } else {
        let rest = text.strip_prefix(r"\\?\")?;
        let b = rest.as_bytes();
        if !(b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':') {
            return None;
        }
        ("", rest, 1)
    };
    for piece in rest.split('\\').skip(skip) {
        if piece.ends_with(['.', ' ']) && !piece.is_empty() || is_device_name(piece) {
            return None;
        }
    }
    let plain = format!("{plain_prefix}{rest}");
    (plain.encode_utf16().count() < 260).then_some(plain)
}

/// Whether Windows takes the file name `name` for a device, whatever its extension.
pub(super) fn is_device_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or("").trim_end_matches(' ');
    let stem = stem.to_ascii_uppercase();
    matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || ["COM", "LPT"].iter().any(|d| {
        stem.strip_prefix(d).is_some_and(|n| {
            matches!(n, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
                || matches!(n, "\u{b9}" | "\u{b2}" | "\u{b3}")
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_verbatim_form_of_a_windows_path_is_made_plain() {
        let plain = |p: &str| {
            without_verbatim(Path::new(p))
                .to_string_lossy()
                .into_owned()
        };
        assert_eq!(plain(r"\\?\C:\repos\x"), r"C:\repos\x");
        assert_eq!(plain(r"\\?\UNC\server\share\x"), r"\\server\share\x");
        assert_eq!(plain(r"\\?\Volume{1}\x"), r"\\?\Volume{1}\x");
        assert_eq!(plain("/home/me/x"), "/home/me/x");
    }

    #[test]
    fn the_verbatim_prefix_stays_where_the_plain_form_would_be_another_place() {
        for kept in [
            r"\\?\C:\x\name.",
            r"\\?\C:\x \y",
            r"\\?\C:\x\y ",
            r"\\?\C:\x\con",
            r"\\?\C:\x\CON.txt",
            r"\\?\C:\x\Aux.tar.gz",
            r"\\?\C:\nul",
            r"\\?\C:\x\prn.",
            r"\\?\C:\x\com1",
            r"\\?\C:\x\LPT9.log",
            "\\\\?\\C:\\x\\com\u{b9}",
            r"\\?\UNC\server\share\aux",
            r"\\?\UNC\server.\share\x",
        ] {
            assert_eq!(without_verbatim(Path::new(kept)), Path::new(kept), "{kept}");
        }
        // Names that only look like those.
        for plain in [
            r"\\?\C:\x\console",
            r"\\?\C:\x\com10",
            r"\\?\C:\x\com0",
            r"\\?\C:\x\lpt",
            r"\\?\C:\x\a.b",
            r"\\?\C:\x\.hidden",
            r"\\?\C:\x\auxx.txt",
        ] {
            assert_ne!(
                without_verbatim(Path::new(plain)),
                Path::new(plain),
                "{plain}"
            );
        }
        // The limit is in UTF-16 units: 130 characters outside the BMP are 260 units.
        let wide = format!(r"\\?\C:\{}", "\u{1f600}".repeat(130));
        assert_eq!(without_verbatim(Path::new(&wide)), Path::new(&wide));
        let ok = format!(r"\\?\C:\{}", "\u{1f600}".repeat(100));
        assert_ne!(without_verbatim(Path::new(&ok)), Path::new(&ok));
        // The limit of the plain form: 259 characters are fine, 260 are not.
        let long = |n: usize| format!(r"\\?\C:\{}", "a".repeat(n - 3));
        assert_eq!(
            without_verbatim(Path::new(&long(259)))
                .to_string_lossy()
                .len(),
            259
        );
        assert_eq!(
            without_verbatim(Path::new(&long(260 + 4))),
            Path::new(&long(260 + 4))
        );
    }

    #[test]
    fn normalising_paths() {
        assert_eq!(normalize(Path::new("./a/./b")), Path::new("a/b"));
        assert_eq!(normalize(Path::new("a/b/../c")), Path::new("a/c"));
        assert_eq!(normalize(Path::new("../x")), Path::new("../x"));
        assert_eq!(normalize(Path::new("a/../../x")), Path::new("../x"));
        assert_eq!(normalize(Path::new(".")), Path::new(""));
        assert_eq!(normalize(Path::new("/a/../..")), Path::new("/"));
    }

    #[test]
    fn the_memory_fs_answers_like_a_disk() {
        let mut fs = MemoryFs::new();
        fs.insert("app/main.colibri", "a");
        fs.insert("./app/game/player.colibri", "b");
        assert!(fs.is_file(Path::new("app/game/player.colibri")));
        assert!(!fs.is_file(Path::new("app/game")));
        assert!(fs.is_dir(Path::new("app/game")));
        assert!(fs.is_dir(Path::new("app")));
        assert!(!fs.is_dir(Path::new("other")));
        assert_eq!(fs.list(Path::new("app")), ["game", "main.colibri"]);
        assert_eq!(fs.read(Path::new("app/main.colibri")).unwrap(), "a");
        assert!(fs.read(Path::new("nope")).is_err());
    }

    #[test]
    fn a_recording_fs_records_reads_and_nothing_else() {
        let mut mem = MemoryFs::new();
        mem.insert_named("a/one.colibri", "shown as one", "1");
        mem.insert("a/two.colibri", "2");
        mem.insert("a/three.colibri", "3");
        let rec = RecordingFs::new(&mem);
        // Looking at files, folders and the start of a file is not reading.
        assert!(rec.is_file(Path::new("a/one.colibri")));
        assert!(rec.is_dir(Path::new("a")));
        assert_eq!(rec.list(Path::new("a")).len(), 3);
        assert_eq!(
            rec.read_prefix(Path::new("a/three.colibri"), 1),
            Some(b"3".to_vec())
        );
        assert!(rec.read(Path::new("a/missing.colibri")).is_err());
        // Reads are, in order, once each, whichever way the path is spelled.
        assert_eq!(rec.read(Path::new("a/two.colibri")).unwrap(), "2");
        assert_eq!(rec.read(Path::new("a/one.colibri")).unwrap(), "1");
        assert_eq!(rec.read(Path::new("./a/two.colibri")).unwrap(), "2");
        let files = rec.into_files();
        let seen: Vec<(PathBuf, &str, &str)> = files
            .iter()
            .map(|f| (f.path.clone(), f.name.as_str(), f.text.as_str()))
            .collect();
        assert_eq!(
            seen,
            [
                (PathBuf::from("./a/two.colibri"), "a/two.colibri", "2"),
                (PathBuf::from("a/one.colibri"), "shown as one", "1"),
            ]
        );
    }

    #[test]
    fn a_memory_file_keeps_the_name_it_was_given() {
        let mut fs = MemoryFs::new();
        fs.insert("app/game/player.colibri", "b");
        // However the resolver spells the path (with the system's separator on Windows).
        let joined: PathBuf = ["app", "game", "player.colibri"].iter().collect();
        assert_eq!(fs.display_name(&joined), "app/game/player.colibri");
        // Not there: shown with `/`.
        assert_eq!(
            fs.display_name(Path::new("app/none.colibri")),
            "app/none.colibri"
        );
    }
}

/// Whether the dependency path `rel`, written in the manifest of the folder `dir`, certainly
/// leaves the repository whose root is `root`, judged from the text of the paths alone: no
/// folder is looked at, so nothing outside the repository is touched to find out. A path from
/// a package that came from git is judged this way before anything else is done with it.
///
/// It leaves when it is absolute in any system's way (a rooted `/x` or `\x`, a drive `C:` with
/// or without a slash, a UNC `\\host\share` or verbatim `\\?\` prefix, whatever system the
/// tool runs on), or when its `..` climb above `root`. `/` and `\` both separate its pieces.
/// Where `dir` is not under `root` as written, only the first rule applies: [`inside`] (which
/// resolves links) still has the last word.
///
/// [`inside`]: super::sync::inside
pub(super) fn leaves_repository(root: &Path, dir: &Path, rel: &str) -> bool {
    let b = rel.as_bytes();
    if rel.starts_with(['/', '\\'])
        || (b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':')
        || Path::new(rel).has_root()
        || Path::new(rel)
            .components()
            .next()
            .is_some_and(|c| matches!(c, std::path::Component::Prefix(_)))
    {
        return true;
    }
    let Ok(below) = normalize(dir)
        .strip_prefix(normalize(root))
        .map(Path::to_path_buf)
    else {
        return false;
    };
    let mut depth = below.components().count() as i64;
    for piece in rel.split(['/', '\\']) {
        match piece {
            "" | "." => {}
            ".." => {
                depth -= 1;
                if depth < 0 {
                    return true;
                }
            }
            _ => depth += 1,
        }
    }
    false
}

#[cfg(test)]
mod leaves_repository_tests {
    use super::*;

    #[test]
    fn a_path_leaves_a_repository_by_its_text() {
        let root = Path::new("/c/repo");
        let dir = Path::new("/c/repo/pkg/inner");
        for rel in [
            "/etc",
            "\\etc",
            "\\\\host\\share\\x",
            "//host/share/x",
            "\\\\?\\C:\\x",
            "C:/x",
            "C:\\x",
            "c:x",
            "../../..",
            "..\\..\\..",
            "../../../x",
            "a/../../../../x",
            "a\\..\\..\\..\\..\\x",
            "./../../../y",
        ] {
            assert!(leaves_repository(root, dir, rel), "{rel}");
        }
        for rel in [
            ".",
            "..",
            "../..",
            "../../pkg",
            "x/../y",
            "a/b/c",
            "..\\..\\pkg\\x",
            "./z",
        ] {
            assert!(!leaves_repository(root, dir, rel), "{rel}");
        }
        // A folder that is not under the root as written is for the resolving check.
        assert!(!leaves_repository(root, Path::new("/d/x"), "../.."));
        assert!(leaves_repository(root, Path::new("/d/x"), "/etc"));
    }
}
