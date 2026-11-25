//! `colibri build`: write one executable that holds a program.
//!
//! The executable is a copy of the running `colibri` (the runner) with the program after its
//! code (see [`crate::pack`]). The program is checked first, through a recording file system:
//! the files the check reads are the program, dependencies included.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use colibri_runtime::{self as colibri, Recorded, RecordingFs};

use crate::depscmd::{self, DepFlags};
use crate::pack::{self, Kind, Pack, PackFile, Start};

/// What `colibri build` was asked for.
pub struct Options<'a> {
    /// A script, or a package folder.
    pub path: &'a Path,
    /// Where to write the executable; by default the name of the program in the current
    /// folder.
    pub out: Option<&'a Path>,
    pub deps: DepFlags,
}

/// Builds the executable, printing problems to stderr.
pub fn build(options: &Options<'_>) -> ExitCode {
    match try_build(options) {
        Ok(()) => ExitCode::SUCCESS,
        Err(code) => code,
    }
}

fn fail(message: impl std::fmt::Display) -> ExitCode {
    eprintln!("error: {message}");
    ExitCode::from(1)
}

fn try_build(options: &Options<'_>) -> Result<(), ExitCode> {
    let path = options.path;
    // As `colibri run` does: the dependencies are fetched and locked before anything is read.
    depscmd::prepare(path, options.deps)?;
    let recorder = RecordingFs::new(&colibri::DiskFs);
    crate::checkcmd::check_on(&recorder, path, &[], false, &mut colibri_std::interface)?;
    let reads = recorder.into_files();
    let pack = make_pack(path, reads)?;
    let out = match options.out {
        Some(out) => out.to_path_buf(),
        None => default_out(path),
    };
    if let Some(name) = clobbers(&out, &pack, path) {
        return Err(fail(format!(
            "the output `{}` is the source file `{name}`: choose another with `-o`",
            out.display()
        )));
    }
    let runner = std::env::current_exe()
        .map_err(|e| fail(format!("cannot find the running `colibri` to copy: {e}")))?;
    write_executable(&runner, &out, &pack).map_err(fail)?;
    let sources = pack.files.iter().filter(|f| f.kind != Kind::Asset).count();
    let size = fs::metadata(&out).map_or(0, |m| m.len());
    eprintln!(
        "built {} ({sources} source file{}, {size} bytes)",
        out.display(),
        if sources == 1 { "" } else { "s" }
    );
    Ok(())
}

/// The pack of the program at `path`, from the files its check read.
fn make_pack(path: &Path, reads: Vec<Recorded>) -> Result<Pack, ExitCode> {
    let cache = colibri::cache_root().ok();
    let mut files = Vec::new();
    let mut checkouts: Vec<String> = Vec::new();
    for read in reads {
        // A file of a git dependency is kept below the cache folder: where that is differs
        // from one machine to the next.
        let below_cache = cache
            .as_ref()
            .and_then(|c| read.path.strip_prefix(c).ok().map(Path::to_path_buf));
        let (kind, shown) = match &below_cache {
            Some(rel) => (Kind::Dependency, rel.as_path()),
            None => (Kind::Source, read.path.as_path()),
        };
        let Some(text_path) = pack::portable(shown) else {
            return Err(fail(format!(
                "the path `{}` is not text, and cannot be packed",
                read.path.display()
            )));
        };
        if kind == Kind::Dependency
            && let Some(dir) = checkout_of(&text_path)
            && !checkouts.contains(&dir)
        {
            checkouts.push(dir);
        }
        files.push(PackFile {
            kind,
            path: text_path,
            name: read.name,
            data: read.text.into_bytes(),
        });
    }
    // The loader takes a checkout for complete when it has this file, which it looks up and
    // never reads.
    for dir in checkouts {
        files.push(PackFile {
            kind: Kind::Dependency,
            path: format!("{dir}/{}", colibri::COMPLETE_MARKER),
            name: format!("{dir}/{}", colibri::COMPLETE_MARKER),
            data: Vec::new(),
        });
    }
    let Some(start) = pack::portable(path) else {
        return Err(fail(format!(
            "the path `{}` is not text, and cannot be packed",
            path.display()
        )));
    };
    Ok(Pack {
        name: program_name(path),
        start: if path.is_dir() {
            Start::Package(start)
        } else {
            Start::File(start)
        },
        cfg: crate::build_cfg(),
        files,
    })
}

/// `git/checkouts/<repository>/<commit>`, the folder a file of a dependency is in.
fn checkout_of(rel: &str) -> Option<String> {
    let pieces: Vec<&str> = rel.split('/').collect();
    (pieces.len() > 4 && pieces[0] == "git" && pieces[1] == "checkouts")
        .then(|| pieces[..4].join("/"))
}

/// What the program is called: the folder of a package, the file name of a script without its
/// extension.
fn program_name(path: &Path) -> String {
    let named = if path.is_dir() {
        fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
    } else {
        path.to_path_buf()
    };
    let name = if path.is_dir() {
        named.file_name()
    } else {
        named.file_stem()
    };
    name.map_or_else(
        || "program".to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

fn default_out(path: &Path) -> PathBuf {
    PathBuf::from(format!(
        "{}{}",
        program_name(path),
        std::env::consts::EXE_SUFFIX
    ))
}

/// The name of a source file of the program that `out` would overwrite, if there is one.
fn clobbers(out: &Path, pack: &Pack, entry: &Path) -> Option<String> {
    let out = fs::canonicalize(out).ok()?;
    let same = |p: &Path| fs::canonicalize(p).is_ok_and(|p| p == out);
    if same(entry) {
        return Some(entry.display().to_string());
    }
    pack.files
        .iter()
        .filter(|f| f.kind == Kind::Source)
        .find(|f| same(Path::new(&f.path)))
        .map(|f| f.name.clone())
}

/// Copies `runner` to `out` and appends the pack. The file is made under another name and
/// moved into place, so a failure leaves nothing half-written (and a running copy of `out`
/// is not written into).
fn write_executable(runner: &Path, out: &Path, pack: &Pack) -> Result<(), String> {
    let mut check = fs::File::open(runner)
        .map_err(|e| format!("cannot read the runner `{}`: {e}", runner.display()))?;
    if pack::has_pack(&mut check)? {
        return Err(format!(
            "the runner `{}` is a built program already",
            runner.display()
        ));
    }
    drop(check);
    let name = out
        .file_name()
        .map_or_else(|| "out".into(), |n| n.to_string_lossy().into_owned());
    let tmp = out.with_file_name(format!(".{name}.colibri-build"));
    let result = (|| {
        let runner_len =
            fs::copy(runner, &tmp).map_err(|e| format!("cannot write `{}`: {e}", out.display()))?;
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(&tmp)
            .map_err(|e| format!("cannot write `{}`: {e}", out.display()))?;
        file.write_all(&pack.append_to(runner_len))
            .and_then(|()| file.flush())
            .map_err(|e| format!("cannot write `{}`: {e}", out.display()))?;
        drop(file);
        sign(&tmp);
        fs::rename(&tmp, out).map_err(|e| format!("cannot write `{}`: {e}", out.display()))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// On macOS the appended data invalidates the signature of the runner, and arm64 refuses to
/// run unsigned code: sign it again, ad hoc.
#[cfg(target_os = "macos")]
fn sign(file: &Path) {
    let signed = std::process::Command::new("codesign")
        .args(["-s", "-", "-f"])
        .arg(file)
        .output()
        .is_ok_and(|o| o.status.success());
    if !signed {
        eprintln!(
            "warning: `codesign -s - {}` failed: the program may not start until it is run",
            file.display()
        );
    }
}

#[cfg(not(target_os = "macos"))]
fn sign(_file: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dependency_file_names_its_checkout() {
        assert_eq!(
            checkout_of("git/checkouts/lib-1a2b/abc123/src/x.colibri").as_deref(),
            Some("git/checkouts/lib-1a2b/abc123")
        );
        assert_eq!(checkout_of("git/checkouts/lib/abc"), None);
        assert_eq!(checkout_of("other/checkouts/lib/abc/x"), None);
    }
}
