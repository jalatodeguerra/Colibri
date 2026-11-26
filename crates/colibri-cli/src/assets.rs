//! The assets of a program, for `colibri run`, `colibri test` and `colibri build`
//! (`import assets`, `[build] assets` in `colibri.toml`).
//!
//! The package folder of a program is the folder of its `colibri.toml`: the folder itself for
//! `colibri run folder`, the folder a script is in for `colibri run script.colibri` (the loader
//! reads a manifest next to a script too). A script with no manifest beside it has no assets.
//! Its assets are the files below that folder which the patterns of `[build] assets` select
//! (see [`colibri_runtime::AssetPatterns`]), and nothing else: not a file the patterns do not
//! list, not one reached through a symbolic link, not one outside the folder.
//!
//! `run` and `test` read them from disk through [`DiskAssets`]; `build` copies them into the
//! pack (`collect`), and the built program reads them from its own file through [`PackAssets`],
//! one at a time when the program asks. The three answer alike, so a program does not tell where
//! it runs.
//!
//! **Reading from disk.** A path is never handed to the system as a whole. [`open_below`] takes it
//! a piece at a time: the piece must be in the listing of its folder under exactly that name (so
//! a short `PROGRA~1`, a name in another case and a trailing dot find nothing, as they find
//! nothing in a build that walked the folder), and is then opened from the folder it was found in
//! without following a link (`openat` with `O_NOFOLLOW` on Unix; on Windows a handle that opens
//! the link itself and is refused if it is a reparse point, with the folders on the way kept open
//! so that nobody can move them meanwhile). A link put in the place of a piece after the listing
//! is therefore refused as well.

use std::ffi::OsStr;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use colibri_runtime::{AssetPatterns, MANIFEST_FILE, asset_path_problem, parse_manifest};
use colibri_std::{AssetError, AssetSource, NOT_A_FOLDER, NOT_AN_ASSET, check_dir, children};
pub use colibri_std::{PackAssets, for_pack, io_reason};
use colibri_syntax::FileId;

use crate::pack::PackAsset;

/// The package folder of the program that starts at `entry` (a package folder or a script).
pub fn package_root(entry: &Path) -> PathBuf {
    let root = if entry.is_dir() {
        entry
    } else {
        entry.parent().unwrap_or(Path::new(""))
    };
    if root.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        root.to_path_buf()
    }
}

/// The patterns `[build] assets` of the manifest in `root`: none if there is no manifest or it
/// has none. `Err` is why the manifest cannot be read (which the loader reports in full, so a
/// run does not print it twice).
pub fn declared(root: &Path) -> Result<AssetPatterns, String> {
    let path = root.join(MANIFEST_FILE);
    if !path.is_file() {
        return Ok(AssetPatterns::default());
    }
    let text =
        fs::read_to_string(&path).map_err(|e| format!("cannot read `{MANIFEST_FILE}`: {e}"))?;
    let manifest =
        parse_manifest(FileId(0), &text).map_err(|_| format!("`{MANIFEST_FILE}` has errors"))?;
    let texts: Vec<String> = manifest.build.assets.into_iter().map(|(t, _)| t).collect();
    AssetPatterns::new(&texts)
}

/// The assets of the program at `entry`, read from disk as they are now.
pub fn for_entry(entry: &Path) -> Box<dyn AssetSource> {
    let root = package_root(entry);
    let patterns = declared(&root).unwrap_or_default();
    Box::new(DiskAssets::new(root, patterns))
}

/// The text of an [`AssetError`], without its kind.
pub fn error_text(e: &AssetError) -> &str {
    match e {
        AssetError::NotFound(t) | AssetError::Invalid(t) | AssetError::Other(t) => t,
    }
}

/// A function the tests give to see each piece of a path before it is opened.
#[cfg(test)]
type Hook = Box<dyn FnMut(&Path)>;

/// The assets of a program that runs from its sources: the files of the package folder that
/// the manifest lists, as they are on disk now.
pub struct DiskAssets {
    root: PathBuf,
    patterns: AssetPatterns,
    /// Called with the path of each piece after it was found in its folder and before it is
    /// opened: where a test puts a link in its place.
    #[cfg(test)]
    hook: Option<Hook>,
}

impl DiskAssets {
    pub fn new(root: PathBuf, patterns: AssetPatterns) -> DiskAssets {
        DiskAssets {
            root,
            patterns,
            #[cfg(test)]
            hook: None,
        }
    }
}

fn other(what: &str, e: &std::io::Error) -> AssetError {
    AssetError::Other(format!("{what}: {}", io_reason(e)))
}

fn is_link_error() -> AssetError {
    AssetError::Invalid(
        "a symbolic link is on the way: assets are plain files in the package folder".to_string(),
    )
}

/// Whether `names` (the entries of a folder) has `wanted`, spelt exactly so.
fn has_exact(mut names: impl Iterator<Item = std::ffi::OsString>, wanted: &OsStr) -> bool {
    names.any(|n| n == wanted)
}

/// Whether the folder `dir` has an entry called exactly `name`.
fn exact_entry(dir: &Path, name: &OsStr) -> Result<bool, AssetError> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(false);
        }
        Err(e) => return Err(other("cannot list the folder", &e)),
    };
    let mut failed = None;
    let found = has_exact(
        entries.map_while(|entry| match entry {
            Ok(entry) => Some(entry.file_name()),
            Err(e) => {
                failed = Some(e);
                None
            }
        }),
        name,
    );
    match failed {
        Some(e) if !found => Err(other("cannot list the folder", &e)),
        _ => Ok(found),
    }
}

/// Opens the asset `path` (valid by [`asset_path_problem`]) of the package at `root`, for
/// reading, as the module's own description says: each piece is found by its exact name, then
/// opened without following a link, from the folder it was found in. `before` is called with
/// each piece's path between the two.
#[cfg(unix)]
pub fn open_below(
    root: &Path,
    path: &str,
    before: &mut dyn FnMut(&Path),
) -> Result<fs::File, AssetError> {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;

    let c_string = |bytes: &[u8]| {
        CString::new(bytes).map_err(|_| AssetError::Invalid("the path has a NUL character".into()))
    };
    let root_c = c_string(root.as_os_str().as_bytes())?;
    // SAFETY: `root_c` is a NUL-terminated string that outlives the call.
    let fd = unsafe {
        libc::open(
            root_c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(other(
            "cannot open the package folder",
            &std::io::Error::last_os_error(),
        ));
    }
    // SAFETY: `fd` is a new descriptor that nothing else owns.
    let mut dir = unsafe { OwnedFd::from_raw_fd(fd) };
    let pieces: Vec<&str> = path.split('/').collect();
    let mut at = root.to_path_buf();
    for (i, piece) in pieces.iter().enumerate() {
        let last = i + 1 == pieces.len();
        if !exact_entry(&at, OsStr::new(piece))? {
            return Err(AssetError::NotFound(NOT_AN_ASSET.to_string()));
        }
        at.push(piece);
        before(&at);
        let name = c_string(piece.as_bytes())?;
        let flags = libc::O_RDONLY
            | libc::O_NOFOLLOW
            | libc::O_CLOEXEC
            | if last {
                // A named pipe must not block the open; it is refused below.
                libc::O_NONBLOCK
            } else {
                libc::O_DIRECTORY
            };
        // SAFETY: `dir` is an open descriptor and `name` a NUL-terminated string.
        let fd = unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            let e = std::io::Error::last_os_error();
            return Err(match e.raw_os_error() {
                Some(libc::ENOENT) => AssetError::NotFound(NOT_AN_ASSET.to_string()),
                // A link (what `O_NOFOLLOW` refuses) or something that is not a folder.
                Some(libc::ELOOP | libc::ENOTDIR | libc::EMLINK) => {
                    // SAFETY: a zeroed `stat` is a valid buffer for `fstatat` to fill.
                    let mut st: libc::stat = unsafe { std::mem::zeroed() };
                    // SAFETY: as above, with `st` a valid buffer.
                    let ok = unsafe {
                        libc::fstatat(
                            dir.as_raw_fd(),
                            name.as_ptr(),
                            &mut st,
                            libc::AT_SYMLINK_NOFOLLOW,
                        )
                    } == 0;
                    if ok && st.st_mode & libc::S_IFMT == libc::S_IFLNK {
                        is_link_error()
                    } else {
                        AssetError::NotFound(NOT_AN_ASSET.to_string())
                    }
                }
                _ => other("cannot open it", &e),
            });
        }
        // SAFETY: `fd` is a new descriptor that nothing else owns.
        let opened = unsafe { OwnedFd::from_raw_fd(fd) };
        if last {
            // SAFETY: a zeroed `stat` is a valid buffer for `fstat` to fill.
            let mut st: libc::stat = unsafe { std::mem::zeroed() };
            // SAFETY: `opened` is an open descriptor and `st` a valid buffer.
            if unsafe { libc::fstat(opened.as_raw_fd(), &mut st) } != 0 {
                return Err(other("cannot look at it", &std::io::Error::last_os_error()));
            }
            if st.st_mode & libc::S_IFMT != libc::S_IFREG {
                return Err(AssetError::NotFound(NOT_AN_ASSET.to_string()));
            }
            return Ok(fs::File::from(opened));
        }
        dir = opened;
    }
    Err(AssetError::NotFound(NOT_AN_ASSET.to_string()))
}

/// [`open_below`] on Windows: every piece is opened as itself (`FILE_FLAG_OPEN_REPARSE_POINT`
/// opens a link and not what it points to) and refused if its handle says it is a reparse
/// point. The folders stay open, without the right to delete or rename them, until the file is
/// open, so none can be replaced on the way.
#[cfg(windows)]
pub fn open_below(
    root: &Path,
    path: &str,
    before: &mut dyn FnMut(&Path),
) -> Result<fs::File, AssetError> {
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    const FILE_SHARE_READ: u32 = 1;
    const FILE_SHARE_WRITE: u32 = 2;
    const FILE_LIST_DIRECTORY: u32 = 1;
    const FILE_READ_ATTRIBUTES: u32 = 0x80;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

    let pieces: Vec<&str> = path.split('/').collect();
    let mut at = root.to_path_buf();
    let mut held: Vec<fs::File> = Vec::new();
    for (i, piece) in pieces.iter().enumerate() {
        let last = i + 1 == pieces.len();
        if !exact_entry(&at, OsStr::new(piece))? {
            return Err(AssetError::NotFound(NOT_AN_ASSET.to_string()));
        }
        at.push(piece);
        before(&at);
        let mut options = fs::OpenOptions::new();
        options
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE);
        if last {
            // Backup semantics so that a folder (or a link to one) opens too and is then told
            // apart below: without it Windows answers "access denied" for a folder.
            options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS);
        } else {
            options
                .access_mode(FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES)
                .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS);
        }
        let file = match options.open(&at) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(AssetError::NotFound(NOT_AN_ASSET.to_string()));
            }
            Err(e) => return Err(other("cannot open it", &e)),
        };
        let meta = file
            .metadata()
            .map_err(|e| other("cannot look at it", &e))?;
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(is_link_error());
        }
        if last {
            if !meta.is_file() {
                return Err(AssetError::NotFound(NOT_AN_ASSET.to_string()));
            }
            return Ok(file);
        }
        if !meta.is_dir() {
            return Err(AssetError::NotFound(NOT_AN_ASSET.to_string()));
        }
        held.push(file);
    }
    Err(AssetError::NotFound(NOT_AN_ASSET.to_string()))
}

/// [`open_below`] where there is neither `openat` nor a way to open a link itself: each piece is
/// looked at without following links, then the file is opened. A link swapped in between is not
/// caught here.
#[cfg(not(any(unix, windows)))]
pub fn open_below(
    root: &Path,
    path: &str,
    before: &mut dyn FnMut(&Path),
) -> Result<fs::File, AssetError> {
    let pieces: Vec<&str> = path.split('/').collect();
    let mut at = root.to_path_buf();
    for (i, piece) in pieces.iter().enumerate() {
        let last = i + 1 == pieces.len();
        if !exact_entry(&at, OsStr::new(piece))? {
            return Err(AssetError::NotFound(NOT_AN_ASSET.to_string()));
        }
        at.push(piece);
        before(&at);
        let meta = fs::symlink_metadata(&at).map_err(|e| other("cannot look at it", &e))?;
        if meta.file_type().is_symlink() {
            return Err(is_link_error());
        }
        if last != meta.is_file() {
            return Err(AssetError::NotFound(NOT_AN_ASSET.to_string()));
        }
    }
    fs::File::open(&at).map_err(|e| other("cannot open it", &e))
}

impl AssetSource for DiskAssets {
    fn read(&mut self, path: &str) -> Result<Vec<u8>, AssetError> {
        if let Some(why) = asset_path_problem(path) {
            return Err(AssetError::Invalid(why.to_string()));
        }
        if !self.patterns.selects(path) {
            return Err(AssetError::NotFound(NOT_AN_ASSET.to_string()));
        }
        let mut nothing = |_: &Path| {};
        #[cfg(test)]
        let before: &mut dyn FnMut(&Path) = match self.hook.as_mut() {
            Some(hook) => &mut **hook,
            None => &mut nothing,
        };
        #[cfg(not(test))]
        let before: &mut dyn FnMut(&Path) = &mut nothing;
        let mut file = open_below(&self.root, path, before)?;
        let mut data = Vec::new();
        file.read_to_end(&mut data)
            .map_err(|e| other("cannot read it", &e))?;
        Ok(data)
    }

    fn list(&mut self, dir: &str) -> Result<Vec<String>, AssetError> {
        check_dir(dir)?;
        let paths = walk(&self.root, &self.patterns, false).map_err(AssetError::Other)?;
        children(paths.iter().map(String::as_str), dir)
            .ok_or_else(|| AssetError::NotFound(NOT_A_FOLDER.to_string()))
    }
}

/// The paths (below `root`, written with `/`, sorted) of the files the patterns select. Links
/// are never followed, so a loop cannot trap the walk and nothing outside `root` is read, and a
/// folder is entered only where a pattern can select something below it (so `.git` or `target`
/// are not walked, nor looked at, unless a pattern asks for them). A selected link is an error
/// when `strict` (a build must not leave a file out in silence) and is skipped otherwise, which
/// is also what `read` does with it (it refuses).
fn walk(root: &Path, patterns: &AssetPatterns, strict: bool) -> Result<Vec<String>, String> {
    walk_visiting(root, patterns, strict, &mut |_| {})
}

/// [`walk`], telling `visit` each folder it enters (`""` is the top).
fn walk_visiting(
    root: &Path,
    patterns: &AssetPatterns,
    strict: bool,
    visit: &mut dyn FnMut(&str),
) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    if !patterns.is_empty() {
        walk_into(root, "", patterns, strict, visit, &mut out)?;
    }
    out.sort();
    Ok(out)
}

fn walk_into(
    dir: &Path,
    rel: &str,
    patterns: &AssetPatterns,
    strict: bool,
    visit: &mut dyn FnMut(&str),
    out: &mut Vec<String>,
) -> Result<(), String> {
    visit(rel);
    let shown = |p: &str| {
        if p.is_empty() {
            ".".to_string()
        } else {
            p.to_string()
        }
    };
    let cannot =
        |e: &std::io::Error| format!("cannot list the folder `{}`: {}", shown(rel), io_reason(e));
    let entries = fs::read_dir(dir).map_err(|e| cannot(&e))?;
    for entry in entries {
        let entry = entry.map_err(|e| cannot(&e))?;
        let name = entry.file_name();
        let text = name.to_string_lossy();
        let child = if rel.is_empty() {
            text.to_string()
        } else {
            format!("{rel}/{text}")
        };
        let kind = entry
            .file_type()
            .map_err(|e| format!("cannot look at `{child}`: {}", io_reason(&e)))?;
        if kind.is_symlink() {
            // A link to a folder stands for what is below it, so the probe name stands in.
            let selected =
                patterns.selects(&child) || patterns.selects(&format!("{child}/{PROBE}"));
            if strict && selected {
                return Err(format!(
                    "`{child}` is a symbolic link: assets are plain files in the package folder, and links are not followed"
                ));
            }
        } else if kind.is_dir() {
            // A name that no asset path can have holds no asset either.
            if asset_path_problem(&child).is_none() && patterns.may_select_below(&child) {
                walk_into(&entry.path(), &child, patterns, strict, visit, out)?;
            }
        } else if kind.is_file() && patterns.selects(&child) {
            if name.to_str().is_none() || asset_path_problem(&child).is_some() {
                if strict {
                    return Err(format!(
                        "the asset `{child}` has a name that cannot be packed (it is not text, or Windows would change it)"
                    ));
                }
                continue;
            }
            out.push(child);
        }
    }
    Ok(())
}

/// A name that no pattern is likely to single out, for asking whether a link to a folder is
/// selected.
const PROBE: &str = "_";

/// What `colibri build` packs: the assets of the package at `root`, in path order, to be read
/// when the pack is written. A pattern that selects no file is an error (it is almost always a
/// typo), and so is a selected link. `out` is where the executable is going to be written: a
/// file the patterns select must not be it.
pub fn collect(
    root: &Path,
    patterns: &AssetPatterns,
    out: &Path,
) -> Result<Vec<PackAsset>, String> {
    let paths = walk(root, patterns, true)?;
    let unmatched = patterns.unmatched(&paths);
    if let Some(first) = unmatched.first() {
        return Err(format!(
            "the asset pattern `{first}` of `[build] assets` selects no file in `{}`",
            root.display()
        ));
    }
    let out = fs::canonicalize(out).ok();
    let mut assets = Vec::new();
    for path in paths {
        if out.is_some() && fs::canonicalize(root.join(&path)).ok() == out {
            return Err(format!(
                "the output is the asset `{path}`: choose another file with `-o`, or write it outside the assets"
            ));
        }
        assets.push(crate::pack::asset_from_disk(root, &path)?);
    }
    Ok(assets)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pack::{AssetData, Pack, PackDisk, Start};
    use std::cell::{Cell, RefCell};
    use std::io::{Cursor, Seek, SeekFrom};
    use std::rc::Rc;

    fn patterns(texts: &[&str]) -> AssetPatterns {
        AssetPatterns::new(&texts.iter().map(|t| t.to_string()).collect::<Vec<_>>()).unwrap()
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("colibri-as-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn put(dir: &Path, rel: &str, data: &[u8]) {
        let full = dir.join(rel);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, data).unwrap();
    }

    #[test]
    fn the_walk_finds_the_selected_files_in_order() {
        let dir = scratch("walk");
        put(&dir, "data/b.txt", b"b");
        put(&dir, "data/a/c.txt", b"c");
        put(&dir, "other/d.txt", b"d");
        put(&dir, "top.png", b"t");
        let found = walk(&dir, &patterns(&["data/", "*.png"]), true).unwrap();
        assert_eq!(found, ["data/a/c.txt", "data/b.txt", "top.png"]);
        assert!(walk(&dir, &patterns(&[]), true).unwrap().is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_walk_enters_only_folders_a_pattern_can_reach() {
        let dir = scratch("prune");
        put(&dir, "data/a.txt", b"a");
        put(&dir, "data/deep/b.txt", b"b");
        put(&dir, ".git/objects/x", b"x");
        put(&dir, "target/debug/y", b"y");
        put(&dir, "img/logo.png", b"l");
        put(&dir, "img/sub/skip.txt", b"s");
        let entered = |texts: &[&str]| {
            let seen = RefCell::new(Vec::new());
            let found = walk_visiting(&dir, &patterns(texts), true, &mut |d| {
                seen.borrow_mut().push(d.to_string());
            })
            .unwrap();
            let mut seen = seen.into_inner();
            seen.sort();
            (seen, found)
        };
        let (seen, found) = entered(&["data/"]);
        assert_eq!(seen, ["", "data", "data/deep"]);
        assert_eq!(found, ["data/a.txt", "data/deep/b.txt"]);
        let (seen, _) = entered(&["img/*.png", "data/a.txt"]);
        assert_eq!(seen, ["", "data", "img"]);
        // `**` may reach anywhere.
        let (seen, _) = entered(&["**/*.png"]);
        assert!(seen.contains(&".git".to_string()) && seen.contains(&"target/debug".to_string()));
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_folder_nobody_can_read_matters_only_where_a_pattern_goes() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("unreadable");
        put(&dir, "data/a.txt", b"a");
        put(&dir, "private/secret", b"s");
        fs::set_permissions(dir.join("private"), fs::Permissions::from_mode(0o000)).unwrap();
        // Unrelated to the patterns: neither the walk of a build nor `list` notices it.
        let found = collect(&dir, &patterns(&["data/"]), &dir.join("out"));
        let mut disk = DiskAssets::new(dir.clone(), patterns(&["data/"]));
        let listed = disk.list("");
        // Put back first, so that the folder can be removed whatever the result.
        fs::set_permissions(dir.join("private"), fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(found.unwrap().len(), 1);
        assert_eq!(listed.unwrap(), ["data/"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_name_is_found_only_spelt_exactly() {
        let names = || {
            ["A.TXT", "Data", "PROGRA~1"]
                .map(std::ffi::OsString::from)
                .into_iter()
        };
        assert!(has_exact(names(), OsStr::new("A.TXT")));
        assert!(has_exact(names(), OsStr::new("PROGRA~1")));
        // Another case, a short name of a long one, a trailing dot, a space.
        for wrong in [
            "a.txt",
            "data",
            "A.txt",
            "Program Files",
            "A.TXT.",
            "A.TXT ",
        ] {
            assert!(!has_exact(names(), OsStr::new(wrong)), "{wrong}");
        }
        // On disk too, whatever the file system does with a name that is spelt differently.
        let dir = scratch("exact");
        put(&dir, "A.TXT", b"x");
        assert!(exact_entry(&dir, OsStr::new("A.TXT")).unwrap());
        assert!(!exact_entry(&dir, OsStr::new("a.txt")).unwrap());
        assert!(!exact_entry(&dir.join("nope"), OsStr::new("A.TXT")).unwrap());
        assert!(!exact_entry(&dir.join("A.TXT"), OsStr::new("x")).unwrap());
        let mut disk = DiskAssets::new(dir.clone(), patterns(&["*.TXT", "*.txt"]));
        assert_eq!(disk.read("A.TXT").unwrap(), b"x");
        assert!(matches!(disk.read("a.txt"), Err(AssetError::NotFound(_))));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn disk_reads_only_listed_plain_files_below_the_folder() {
        let dir = scratch("disk");
        put(&dir, "data/a.txt", b"A");
        put(&dir, "secret.txt", b"S");
        let mut disk = DiskAssets::new(dir.clone(), patterns(&["data/"]));
        assert_eq!(disk.read("data/a.txt").unwrap(), b"A");
        // A file the patterns do not list is not an asset, though it is there.
        assert!(matches!(
            disk.read("secret.txt"),
            Err(AssetError::NotFound(_))
        ));
        assert!(matches!(
            disk.read("data/none"),
            Err(AssetError::NotFound(_))
        ));
        assert!(matches!(disk.read("data"), Err(AssetError::NotFound(_))));
        assert!(matches!(
            disk.read("data/a.txt/x"),
            Err(AssetError::NotFound(_))
        ));
        for bad in [
            "../secret.txt",
            "data/../secret.txt",
            "/etc/passwd",
            "",
            "data//a.txt",
            "data\\a.txt",
            "data/CON",
            "data/a.txt.",
        ] {
            assert!(
                matches!(disk.read(bad), Err(AssetError::Invalid(_))),
                "{bad}"
            );
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_link_is_never_followed_out_of_the_folder() {
        use std::os::unix::fs::symlink;
        let dir = scratch("links");
        let outside = scratch("links-outside");
        put(&outside, "stolen.txt", b"X");
        put(&dir, "data/ok.txt", b"ok");
        symlink(outside.join("stolen.txt"), dir.join("data/file_link")).unwrap();
        symlink(&outside, dir.join("data/dir_link")).unwrap();
        let mut disk = DiskAssets::new(dir.clone(), patterns(&["data/"]));
        assert_eq!(disk.read("data/ok.txt").unwrap(), b"ok");
        assert!(matches!(
            disk.read("data/file_link"),
            Err(AssetError::Invalid(_))
        ));
        assert!(matches!(
            disk.read("data/dir_link/stolen.txt"),
            Err(AssetError::Invalid(_))
        ));
        assert_eq!(disk.list("data").unwrap(), ["ok.txt"]);
        // A build refuses what it would have to leave out or follow.
        let err = collect(&dir, &patterns(&["data/"]), &dir.join("out")).unwrap_err();
        assert!(err.contains("symbolic link"), "{err}");
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&outside);
    }

    /// A link put where `piece` is, after the piece was found in its folder and before it is
    /// opened: what a program that unpacks someone's files could have raced against.
    #[cfg(unix)]
    fn swapped_for_a_link(piece: PathBuf, target: PathBuf, hits: Rc<Cell<u32>>) -> Hook {
        Box::new(move |at: &Path| {
            if at == piece {
                hits.set(hits.get() + 1);
                let _ = fs::remove_dir_all(at).or_else(|_| fs::remove_file(at));
                std::os::unix::fs::symlink(&target, at).unwrap();
            }
        })
    }

    #[cfg(unix)]
    #[test]
    fn a_link_swapped_in_after_the_listing_is_refused_at_the_file() {
        let dir = scratch("race-file");
        let outside = scratch("race-file-outside");
        put(&outside, "x.txt", b"OUTSIDE");
        put(&dir, "data/x.txt", b"inside");
        let hits = Rc::new(Cell::new(0));
        let mut disk = DiskAssets::new(dir.clone(), patterns(&["data/"]));
        disk.hook = Some(swapped_for_a_link(
            dir.join("data/x.txt"),
            outside.join("x.txt"),
            hits.clone(),
        ));
        let got = disk.read("data/x.txt");
        assert_eq!(
            hits.get(),
            1,
            "the swap happened between the listing and the open"
        );
        assert!(matches!(got, Err(AssetError::Invalid(_))), "{got:?}");
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&outside);
    }

    #[cfg(unix)]
    #[test]
    fn a_link_swapped_in_after_the_listing_is_refused_at_a_folder() {
        let dir = scratch("race-folder");
        let outside = scratch("race-folder-outside");
        put(&outside, "x.txt", b"OUTSIDE");
        put(&dir, "data/sub/x.txt", b"inside");
        let hits = Rc::new(Cell::new(0));
        let mut disk = DiskAssets::new(dir.clone(), patterns(&["data/"]));
        disk.hook = Some(swapped_for_a_link(
            dir.join("data/sub"),
            outside.clone(),
            hits.clone(),
        ));
        let got = disk.read("data/sub/x.txt");
        assert_eq!(hits.get(), 1);
        assert!(matches!(got, Err(AssetError::Invalid(_))), "{got:?}");
        // The same swap one folder higher.
        put(&dir, "data/sub2/x.txt", b"inside");
        let mut disk = DiskAssets::new(dir.clone(), patterns(&["data/"]));
        disk.hook = Some(swapped_for_a_link(
            dir.join("data"),
            outside.clone(),
            Rc::new(Cell::new(0)),
        ));
        let got = disk.read("data/x.txt");
        assert!(matches!(got, Err(AssetError::Invalid(_))), "{got:?}");
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&outside);
    }

    #[test]
    fn collect_reports_a_pattern_that_selects_nothing_and_refuses_the_output() {
        let dir = scratch("collect");
        put(&dir, "data/a.txt", b"A");
        let ok = collect(&dir, &patterns(&["data/"]), &dir.join("out")).unwrap();
        assert_eq!(ok.len(), 1);
        assert_eq!((ok[0].path.as_str(), ok[0].len), ("data/a.txt", 1));
        assert_eq!(ok[0].sum, crate::pack::checksum(b"A"));
        let err = collect(&dir, &patterns(&["data/", "typo/"]), &dir.join("out")).unwrap_err();
        assert!(
            err.contains("`typo/`") && err.contains("selects no file"),
            "{err}"
        );
        let err = collect(&dir, &patterns(&["data/"]), &dir.join("data/a.txt")).unwrap_err();
        assert!(
            err.contains("the output is the asset `data/a.txt`"),
            "{err}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_asset_that_changes_while_the_pack_is_written_stops_the_build() {
        let dir = scratch("changes");
        put(&dir, "data/a.txt", b"first");
        let assets = collect(&dir, &patterns(&["data/"]), &dir.join("out")).unwrap();
        let mut pack = pack_of(assets);
        let mut out = Vec::new();
        pack.write_to(&mut out, 0).unwrap();
        // The same length, other bytes; and another length.
        put(&dir, "data/a.txt", b"other");
        let err = pack.write_to(&mut Vec::new(), 0).unwrap_err();
        assert!(err.contains("changed while it was being packed"), "{err}");
        put(&dir, "data/a.txt", b"longer than before");
        let err = pack.write_to(&mut Vec::new(), 0).unwrap_err();
        assert!(err.contains("changed while it was being packed"), "{err}");
        put(&dir, "data/a.txt", b"x");
        assert!(pack.write_to(&mut Vec::new(), 0).is_err());
        pack.assets.clear();
        let _ = fs::remove_dir_all(&dir);
    }

    fn pack_of(assets: Vec<PackAsset>) -> Pack {
        Pack {
            name: "p".to_string(),
            start: Start::File("p.colibri".to_string()),
            cfg: colibri_runtime::BuildCfg::default(),
            files: Vec::new(),
            assets,
            assets_at: 0,
        }
    }

    /// The assets of a pack written and read back, over `Cursor`s the test can look into.
    fn packed(assets: Vec<PackAsset>) -> (Box<dyn AssetSource>, Vec<u8>) {
        let mut file = b"runner".to_vec();
        file.extend(pack_of(assets).append_to(6).unwrap());
        let pack = crate::pack::read_from(&mut Cursor::new(file.clone()))
            .unwrap()
            .unwrap();
        (for_pack(&pack, Cursor::new(file.clone())), file)
    }

    #[test]
    fn a_pack_answers_like_the_disk() {
        let (mut pack, _) = packed(vec![
            PackAsset::held("data/a.txt", b"A".to_vec()),
            PackAsset::held("data/sub/b.txt", b"B".to_vec()),
        ]);
        assert_eq!(pack.read("data/a.txt").unwrap(), b"A");
        assert_eq!(pack.read("data/sub/b.txt").unwrap(), b"B");
        assert!(matches!(
            pack.read("data/c.txt"),
            Err(AssetError::NotFound(_))
        ));
        assert!(matches!(pack.read("../x"), Err(AssetError::Invalid(_))));
        assert_eq!(pack.list("").unwrap(), ["data/"]);
        assert_eq!(pack.list("data").unwrap(), ["a.txt", "sub/"]);
        assert!(matches!(pack.list("nope"), Err(AssetError::NotFound(_))));
        assert!(matches!(pack.list(".."), Err(AssetError::Invalid(_))));
    }

    #[test]
    fn an_asset_with_a_flipped_byte_is_an_error_of_its_own_read_only() {
        let (_, file) = packed(vec![
            PackAsset::held("good.bin", b"GOOD-BYTES".to_vec()),
            PackAsset::held("bad.bin", b"BAD-BYTES".to_vec()),
        ]);
        let at = file.windows(9).position(|w| w == b"BAD-BYTES").unwrap();
        let mut bytes = file.clone();
        bytes[at + 2] ^= 0x20;
        let pack = crate::pack::read_from(&mut Cursor::new(bytes.clone()))
            .unwrap()
            .unwrap();
        let mut assets = for_pack(&pack, Cursor::new(bytes));
        assert_eq!(assets.read("good.bin").unwrap(), b"GOOD-BYTES");
        let err = assets.read("bad.bin").unwrap_err();
        assert!(
            matches!(&err, AssetError::Other(t) if t.contains("check sum of this asset")),
            "{err:?}"
        );
        // The file cut short in the middle of an asset.
        let mut cut = file.clone();
        cut.truncate(at + 3);
        let pack = crate::pack::read_from(&mut Cursor::new(file.clone()))
            .unwrap()
            .unwrap();
        let mut assets = for_pack(&pack, Cursor::new(cut));
        assert!(matches!(assets.read("bad.bin"), Err(AssetError::Other(_))));
    }

    #[test]
    fn an_asset_is_read_when_asked_for_and_not_before() {
        struct Counting {
            inner: Cursor<Vec<u8>>,
            read: Rc<Cell<u64>>,
        }
        impl Read for Counting {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let n = self.inner.read(buf)?;
                self.read.set(self.read.get() + n as u64);
                Ok(n)
            }
        }
        impl Seek for Counting {
            fn seek(&mut self, to: SeekFrom) -> std::io::Result<u64> {
                self.inner.seek(to)
            }
        }
        const BIG: usize = 5 * 1024 * 1024;
        let mut file = b"runner".to_vec();
        file.extend(
            pack_of(vec![
                PackAsset::held("big.bin", vec![9u8; BIG]),
                PackAsset::held("small.txt", b"small".to_vec()),
            ])
            .append_to(6)
            .unwrap(),
        );
        let pack = crate::pack::read_from(&mut Cursor::new(file.clone()))
            .unwrap()
            .unwrap();
        assert!(matches!(pack.assets[0].data, AssetData::Packed { .. }));
        let read = Rc::new(Cell::new(0));
        let mut assets = for_pack(
            &pack,
            Counting {
                inner: Cursor::new(file),
                read: read.clone(),
            },
        );
        // Setting the assets up, listing them and reading a small one: no megabyte read.
        assert_eq!(assets.list("").unwrap(), ["big.bin", "small.txt"]);
        assert_eq!(read.get(), 0);
        assert_eq!(assets.read("small.txt").unwrap(), b"small");
        assert_eq!(read.get(), 5);
        assert_eq!(assets.read("big.bin").unwrap().len(), BIG);
        assert_eq!(read.get(), 5 + BIG as u64);
    }
}
