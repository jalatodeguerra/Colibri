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
