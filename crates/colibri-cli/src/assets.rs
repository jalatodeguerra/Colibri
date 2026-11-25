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
//! pack (`collect`), and the built program reads them through [`PackAssets`]. The three answer
//! alike, so a program does not tell where it runs.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use colibri_runtime::{AssetPatterns, MANIFEST_FILE, asset_path_problem, parse_manifest};
use colibri_std::{AssetError, AssetSource};
use colibri_syntax::FileId;

use crate::pack::{Kind, Pack};

/// What every "no" says: the one thing a program can do about it.
const NOT_AN_ASSET: &str = "not an asset of this program: `[build] assets` in `colibri.toml` lists the files a program can read";

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
    Box::new(DiskAssets { root, patterns })
}

/// The assets of a built program, from its pack.
pub fn for_pack(pack: &mut Pack) -> Box<dyn AssetSource> {
    let mut files = BTreeMap::new();
    for file in &mut pack.files {
        if file.kind == Kind::Asset {
            files.insert(file.path.clone(), std::mem::take(&mut file.data));
        }
    }
    Box::new(PackAssets { files })
}

/// The names directly in the folder `dir` (`""` is the top) among `paths`, sorted; a folder's
/// name ends with `/`. `None` if nothing is below `dir`, which is no folder of assets.
fn children<'a>(paths: impl Iterator<Item = &'a str>, dir: &str) -> Option<Vec<String>> {
    let prefix = if dir.is_empty() {
        String::new()
    } else {
        format!("{dir}/")
    };
    let mut names = BTreeSet::new();
    let mut found = false;
    for path in paths {
        let Some(rest) = path.strip_prefix(prefix.as_str()) else {
            continue;
        };
        found = true;
        names.insert(match rest.split_once('/') {
            Some((folder, _)) => format!("{folder}/"),
            None => rest.to_string(),
        });
    }
    (found || dir.is_empty()).then(|| names.into_iter().collect())
}

/// The assets of a built program: the files the pack holds.
pub struct PackAssets {
    files: BTreeMap<String, Vec<u8>>,
}

impl AssetSource for PackAssets {
    fn read(&mut self, path: &str) -> Result<Vec<u8>, AssetError> {
        if let Some(why) = asset_path_problem(path) {
            return Err(AssetError::Invalid(why.to_string()));
        }
        self.files
            .get(path)
            .cloned()
            .ok_or_else(|| AssetError::NotFound(NOT_AN_ASSET.to_string()))
    }

    fn list(&mut self, dir: &str) -> Result<Vec<String>, AssetError> {
        check_dir(dir)?;
        children(self.files.keys().map(String::as_str), dir)
            .ok_or_else(|| AssetError::NotFound(NOT_A_FOLDER.to_string()))
    }
}

const NOT_A_FOLDER: &str = "no assets are in this folder: `[build] assets` in `colibri.toml` lists the files a program can read";

fn check_dir(dir: &str) -> Result<(), AssetError> {
    match (dir.is_empty(), asset_path_problem(dir)) {
        (false, Some(why)) => Err(AssetError::Invalid(why.to_string())),
        _ => Ok(()),
    }
}

/// The assets of a program that runs from its sources: the files of the package folder that
/// the manifest lists, as they are on disk now.
pub struct DiskAssets {
    root: PathBuf,
    patterns: AssetPatterns,
}

impl DiskAssets {
    pub fn new(root: PathBuf, patterns: AssetPatterns) -> DiskAssets {
        DiskAssets { root, patterns }
    }
}

fn other(what: &str, e: &std::io::Error) -> AssetError {
    AssetError::Other(format!("{what}: {}", io_reason(e)))
}

/// The same few words on every system for the common cases, so no message depends on the
/// wording of the OS.
fn io_reason(e: &std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::PermissionDenied => "permission denied".to_string(),
        std::io::ErrorKind::NotFound => "no such file".to_string(),
        _ => {
            let text = e.to_string();
            text.split(" (os error")
                .next()
                .unwrap_or(&text)
                .to_lowercase()
        }
    }
}

impl AssetSource for DiskAssets {
    fn read(&mut self, path: &str) -> Result<Vec<u8>, AssetError> {
        if let Some(why) = asset_path_problem(path) {
            return Err(AssetError::Invalid(why.to_string()));
        }
        if !self.patterns.selects(path) {
            return Err(AssetError::NotFound(NOT_AN_ASSET.to_string()));
        }
        // Every piece below the package folder is looked at without following links: a link
        // anywhere on the way could lead out of the folder, so none is followed.
        let mut at = self.root.clone();
        let pieces: Vec<&str> = path.split('/').collect();
        for (i, piece) in pieces.iter().enumerate() {
            at.push(piece);
            let meta = match fs::symlink_metadata(&at) {
                Ok(meta) => meta,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Err(AssetError::NotFound(NOT_AN_ASSET.to_string()));
                }
                // A file where a folder was expected.
                Err(e) if e.kind() == std::io::ErrorKind::NotADirectory => {
                    return Err(AssetError::NotFound(NOT_AN_ASSET.to_string()));
                }
                Err(e) => return Err(other("cannot look at it", &e)),
            };
            if meta.file_type().is_symlink() {
                return Err(AssetError::Invalid(
                    "a symbolic link is on the way: assets are plain files in the package folder"
                        .to_string(),
                ));
            }
            let last = i + 1 == pieces.len();
            if last != meta.is_file() {
                return Err(AssetError::NotFound(NOT_AN_ASSET.to_string()));
            }
        }
        fs::read(&at).map_err(|e| other("cannot read it", &e))
    }

    fn list(&mut self, dir: &str) -> Result<Vec<String>, AssetError> {
        check_dir(dir)?;
        let paths = walk(&self.root, &self.patterns, false).map_err(AssetError::Other)?;
        children(paths.iter().map(String::as_str), dir)
            .ok_or_else(|| AssetError::NotFound(NOT_A_FOLDER.to_string()))
    }
}

/// The paths (below `root`, written with `/`, sorted) of the files the patterns select. Links
/// are never followed, so a loop cannot trap the walk and nothing outside `root` is read. A
/// selected link is an error when `strict` (a build must not leave a file out in silence) and
/// is skipped otherwise, which is also what `read` does with it (it refuses).
fn walk(root: &Path, patterns: &AssetPatterns, strict: bool) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    if !patterns.is_empty() {
        walk_into(root, "", patterns, strict, &mut out)?;
    }
    out.sort();
    Ok(out)
}

fn walk_into(
    dir: &Path,
    rel: &str,
    patterns: &AssetPatterns,
    strict: bool,
    out: &mut Vec<String>,
) -> Result<(), String> {
    let shown = |p: &str| {
        if p.is_empty() {
            ".".to_string()
        } else {
            p.to_string()
        }
    };
    let entries = fs::read_dir(dir)
        .map_err(|e| format!("cannot list the folder `{}`: {}", shown(rel), io_reason(&e)))?;
    for entry in entries {
        let entry = entry
            .map_err(|e| format!("cannot list the folder `{}`: {}", shown(rel), io_reason(&e)))?;
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
            walk_into(&entry.path(), &child, patterns, strict, out)?;
        } else if kind.is_file() && patterns.selects(&child) {
            if name.to_str().is_none() {
                if strict {
                    return Err(format!(
                        "the name of the asset `{child}` is not text, and cannot be packed"
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

/// What `colibri build` packs: the assets of the package at `root`, with their bytes, in path
/// order. A pattern that selects no file is an error (it is almost always a typo), and so is a
/// selected link. `out` is where the executable is going to be written: a file the patterns
/// select must not be it.
pub fn collect(
    root: &Path,
    patterns: &AssetPatterns,
    out: &Path,
) -> Result<Vec<(String, Vec<u8>)>, String> {
    let paths = walk(root, patterns, true)?;
    let unmatched = patterns.unmatched(&paths);
    if let Some(first) = unmatched.first() {
        return Err(format!(
            "the asset pattern `{first}` of `[build] assets` selects no file in `{}`",
            root.display()
        ));
    }
    let out = fs::canonicalize(out).ok();
    let mut files = Vec::new();
    for path in paths {
        let full = root.join(&path);
        if out.is_some() && fs::canonicalize(&full).ok() == out {
            return Err(format!(
                "the output is the asset `{path}`: choose another file with `-o`, or write it outside the assets"
            ));
        }
        let data = fs::read(&full)
            .map_err(|e| format!("cannot read the asset `{path}`: {}", io_reason(&e)))?;
        files.push((path, data));
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn a_folder_listing_names_folders_with_a_slash() {
        let paths = ["a.txt", "d/x", "d/sub/y", "d/sub/z", "e/q"];
        let list = |dir: &str| children(paths.iter().copied(), dir);
        assert_eq!(list("").unwrap(), ["a.txt", "d/", "e/"]);
        assert_eq!(list("d").unwrap(), ["sub/", "x"]);
        assert_eq!(list("d/sub").unwrap(), ["y", "z"]);
        assert_eq!(list("none"), None);
        // A folder that only shares a prefix with another is not it.
        assert_eq!(list("e").unwrap(), ["q"]);
        assert_eq!(
            children(std::iter::empty(), "").unwrap(),
            Vec::<String>::new()
        );
        assert_eq!(children(["dd/x"].into_iter(), "d"), None);
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

    #[test]
    fn collect_reports_a_pattern_that_selects_nothing_and_refuses_the_output() {
        let dir = scratch("collect");
        put(&dir, "data/a.txt", b"A");
        let ok = collect(&dir, &patterns(&["data/"]), &dir.join("out")).unwrap();
        assert_eq!(ok, [("data/a.txt".to_string(), b"A".to_vec())]);
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
    fn a_pack_answers_like_the_disk() {
        let mut pack = PackAssets {
            files: BTreeMap::from([
                ("data/a.txt".to_string(), b"A".to_vec()),
                ("data/sub/b.txt".to_string(), b"B".to_vec()),
            ]),
        };
        assert_eq!(pack.read("data/a.txt").unwrap(), b"A");
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
}
