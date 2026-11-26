//! `assets`: the files that belong to the program and that it reads at run time.
//!
//! - `assets.read(path) -> Result[str]` and `assets.read_bytes(path) -> Result[Bytes]` give the
//!   content of one asset, as `fs.read` and `fs.read_bytes` do for a file;
//! - `assets.list(dir) -> Result[List[str]]` gives the names in a folder of assets, sorted;
//!   a name that is a folder ends with `/`, and `""` is the package folder.
//!
//! What an asset is, and where its bytes come from, is up to the host (an [`AssetSource`]):
//! the `colibri` tool reads the files that `[build] assets` of `colibri.toml` lists, from the
//! package folder under `run` and `test`, and from the executable itself in a program made by
//! `colibri build`. The program sees no difference. Paths are relative to the package folder and
//! written with `/`; one that is empty, absolute, has a `.` or `..` piece, a `\` or a `:` is
//! `assets.Error.Invalid` and nothing is looked up. A `Vm` with no source (an embedder that did
//! not give one) has no assets: every read is `NotFound`.
//!
//! The module needs no capability: the data is read-only and part of the program.

use colibri_runtime::{Bytes, Ctx, Error, TypedError, Vm};

use crate::config::State;
use crate::errors::error_kind;
use crate::plain;

error_kind!(
    "assets",
    /// Why an `assets` call failed (`assets.Error.NotFound`).
    Kind {
        /// There is no such asset: the file is not listed as one, or is not there.
        NotFound,
        /// The path cannot name an asset (empty, absolute, `..`, a symbolic link on the way).
        Invalid,
        /// `read` on an asset that is not UTF-8 text.
        InvalidData,
        /// The asset could not be read.
        Other,
    }
);

/// Why a source could not give an asset. The text says why, without the path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AssetError {
    NotFound(String),
    Invalid(String),
    Other(String),
}

/// Where the assets of a program are kept (the host's side of the module). The paths a source
/// is given have passed [`colibri_runtime::asset_path_problem`]; a source that reads a file
/// system should check them again where it matters.
pub trait AssetSource {
    /// The bytes of the asset at `path`.
    fn read(&mut self, path: &str) -> Result<Vec<u8>, AssetError>;
    /// The names in the folder `dir` (`""` is the top folder), sorted; a folder's name ends
    /// with `/`.
    fn list(&mut self, dir: &str) -> Result<Vec<String>, AssetError>;
}

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Seek, SeekFrom};

/// What every "no" says: the one thing a program can do about it.
pub const NOT_AN_ASSET: &str = "not an asset of this program: `[build] assets` in `colibri.toml` lists the files a program can read";

/// The assets of a built program, from its pack: `source` is the file the pack was read from (on
/// the web, the pack's bytes in memory).
/// Nothing is read from it here; an asset is read, and checked against its check sum, when the
/// program asks for it.
pub fn for_pack<R: Read + Seek + 'static>(
    pack: &colibri_pack::Pack,
    source: R,
) -> Box<dyn AssetSource> {
    let mut index = BTreeMap::new();
    for asset in &pack.assets {
        if let colibri_pack::AssetData::Packed { offset } = asset.data {
            index.insert(
                asset.path.clone(),
                (pack.assets_at + offset, asset.len, asset.sum),
            );
        }
    }
    Box::new(PackAssets { source, index })
}

/// The names directly in the folder `dir` (`""` is the top) among `paths`, sorted; a folder's
/// name ends with `/`. `None` if nothing is below `dir`, which is no folder of assets.
pub fn children<'a>(paths: impl Iterator<Item = &'a str>, dir: &str) -> Option<Vec<String>> {
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

/// The assets of a built program: the files the pack holds, read from `source` on demand.
pub struct PackAssets<R> {
    source: R,
    /// Path to where the bytes are in `source`, how long they are and their check sum.
    index: BTreeMap<String, (u64, u64, u64)>,
}

impl<R: Read + Seek> AssetSource for PackAssets<R> {
    fn read(&mut self, path: &str) -> Result<Vec<u8>, AssetError> {
        if let Some(why) = colibri_runtime::asset_path_problem(path) {
            return Err(AssetError::Invalid(why.to_string()));
        }
        let Some(&(at, len, sum)) = self.index.get(path) else {
            return Err(AssetError::NotFound(NOT_AN_ASSET.to_string()));
        };
        let cannot = |e: std::io::Error| {
            AssetError::Other(format!("cannot read the program's data: {}", io_reason(&e)))
        };
        // `len` was checked against the size of the file when the pack was read.
        let mut data = vec![
            0u8;
            usize::try_from(len).map_err(|_| {
                AssetError::Other("the asset is too large for this machine".to_string())
            })?
        ];
        self.source.seek(SeekFrom::Start(at)).map_err(cannot)?;
        self.source.read_exact(&mut data).map_err(cannot)?;
        let mut check = colibri_pack::Fnv::new();
        check.update(&data);
        if check.finish() != sum {
            return Err(AssetError::Other(
                "the program's data is damaged (the check sum of this asset does not match)"
                    .to_string(),
            ));
        }
        Ok(data)
    }

    fn list(&mut self, dir: &str) -> Result<Vec<String>, AssetError> {
        check_dir(dir)?;
        children(self.index.keys().map(String::as_str), dir)
            .ok_or_else(|| AssetError::NotFound(NOT_A_FOLDER.to_string()))
    }
}

pub const NOT_A_FOLDER: &str = "no assets are in this folder: `[build] assets` in `colibri.toml` lists the files a program can read";

pub fn check_dir(dir: &str) -> Result<(), AssetError> {
    match (dir.is_empty(), colibri_runtime::asset_path_problem(dir)) {
        (false, Some(why)) => Err(AssetError::Invalid(why.to_string())),
        _ => Ok(()),
    }
}

/// The same few words on every system for the common cases, so no message depends on the
/// wording of the OS.
pub fn io_reason(e: &std::io::Error) -> String {
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

fn failed(what: &str, path: &str, e: AssetError) -> TypedError {
    let (kind, why) = match e {
        AssetError::NotFound(why) => (Kind::NotFound, why),
        AssetError::Invalid(why) => (Kind::Invalid, why),
        AssetError::Other(why) => (Kind::Other, why),
    };
    kind.error(format!("cannot {what} `{path}`: {why}"))
}

fn invalid(what: &str, path: &str, why: &str) -> TypedError {
    Kind::Invalid.error(format!("cannot {what} `{path}`: {why}"))
}

fn no_source(what: &str, path: &str) -> TypedError {
    Kind::NotFound.error(format!(
        "cannot {what} `{path}`: this program has no assets"
    ))
}

fn read_bytes(cx: &mut Ctx, path: &str) -> Result<Vec<u8>, TypedError> {
    if let Some(why) = colibri_runtime::asset_path_problem(path) {
        return Err(invalid("read asset", path, why));
    }
    match cx.state_mut::<State>().and_then(|s| s.assets.as_mut()) {
        Some(source) => source.read(path).map_err(|e| failed("read asset", path, e)),
        None => Err(no_source("read asset", path)),
    }
}

fn list_names(cx: &mut Ctx, dir: &str) -> Result<Vec<String>, TypedError> {
    let trimmed = dir.strip_suffix('/').unwrap_or(dir);
    if !trimmed.is_empty()
        && let Some(why) = colibri_runtime::asset_path_problem(trimmed)
    {
        return Err(invalid("list assets in", dir, why));
    }
    match cx.state_mut::<State>().and_then(|s| s.assets.as_mut()) {
        Some(source) => source
            .list(trimmed)
            .map_err(|e| failed("list assets in", dir, e)),
        None if trimmed.is_empty() => Ok(Vec::new()),
        None => Err(no_source("list assets in", dir)),
    }
}

pub(crate) fn register(vm: &mut Vm) -> Result<(), Error> {
    Kind::declare(vm)?;
    let mut m = vm.std_module("assets");
    plain(
        m.register_fn_named("read", ["path"], |cx: &mut Ctx, path: String| {
            let bytes = read_bytes(cx, &path)?;
            String::from_utf8(bytes).map_err(|_| {
                Kind::InvalidData.error(format!(
                    "cannot read asset `{path}`: the data is not valid UTF-8 text"
                ))
            })
        }),
    )?;
    plain(
        m.register_fn_named("read_bytes", ["path"], |cx: &mut Ctx, path: String| {
            read_bytes(cx, &path).map(Bytes)
        }),
    )?;
    plain(
        m.register_fn_named("list", ["dir"], |cx: &mut Ctx, dir: String| {
            list_names(cx, &dir)
        }),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
