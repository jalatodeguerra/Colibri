//! Where source files come from: the real file system, or a set of files in memory (for hosts
//! and tests that hand over a whole program as text).

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

/// Reads source files and answers the questions import resolution asks about the tree.
pub trait SourceFs {
    /// The text of a file, or why it could not be read.
    fn read(&self, path: &Path) -> Result<String, String>;
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
}

/// The operating system's files.
#[derive(Clone, Copy, Debug, Default)]
pub struct DiskFs;

impl SourceFs for DiskFs {
    fn read(&self, path: &Path) -> Result<String, String> {
        std::fs::read_to_string(path).map_err(|e| e.to_string())
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
        std::fs::canonicalize(or_dot(path)).unwrap_or_else(|_| normalize(path))
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

#[cfg(test)]
mod tests {
    use super::*;

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
