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

use crate::assets;
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
    let out = match options.out {
        Some(out) => out.to_path_buf(),
        None => default_out(path),
    };
    let pack = make_pack(path, reads, &out)?;
    if out.is_dir() {
        return Err(fail(format!(
            "the output `{}` is a folder: choose a file with `-o`",
            out.display()
        )));
    }
    if let Some(name) = clobbers(&out, &pack, path) {
        return Err(fail(format!(
            "the output `{}` is the source file `{name}`: choose another with `-o`",
            out.display()
        )));
    }
    let runner = std::env::current_exe()
        .map_err(|e| fail(format!("cannot find the running `colibri` to copy: {e}")))?;
    if fs::canonicalize(&out).is_ok_and(|o| fs::canonicalize(&runner).is_ok_and(|r| r == o)) {
        return Err(fail(format!(
            "the output `{}` is the running `colibri` itself: choose another with `-o`",
            out.display()
        )));
    }
    write_executable(&runner, &out, &pack).map_err(fail)?;
    let sources = pack
        .files
        .iter()
        .filter(|f| f.path.ends_with(".colibri"))
        .count();
    let size = fs::metadata(&out).map_or(0, |m| m.len());
    let assets = &pack.assets;
    for asset in assets.iter().take(MAX_LISTED_ASSETS) {
        eprintln!("  asset {} ({} bytes)", asset.path, asset.len);
    }
    if assets.len() > MAX_LISTED_ASSETS {
        eprintln!("  ... and {} more assets", assets.len() - MAX_LISTED_ASSETS);
    }
    let with_assets = match assets.len() {
        0 => String::new(),
        1 => ", 1 asset".to_string(),
        n => format!(", {n} assets"),
    };
    eprintln!(
        "built {} ({sources} source file{}{with_assets}, {size} bytes)",
        out.display(),
        if sources == 1 { "" } else { "s" }
    );
    Ok(())
}

/// How many assets the build lists one by one; a package with more gets the rest as a count.
const MAX_LISTED_ASSETS: usize = 50;

/// The pack of the program at `path`, from the files its check read and the assets its manifest
/// lists. `out` is the file the executable will be written to.
fn make_pack(path: &Path, reads: Vec<Recorded>, out: &Path) -> Result<Pack, ExitCode> {
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
    let root = assets::package_root(path);
    let patterns = assets::declared(&root).map_err(fail)?;
    let assets = assets::collect(&root, &patterns, out).map_err(fail)?;
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
        assets,
        assets_at: 0,
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

/// A name for the temporary file next to `out`: the process, a counter and the clock, so that
/// two builds never share one and nobody can guess it.
fn temp_name(name: &str, attempt: u32) -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    format!(
        ".{name}.{}-{}-{nanos:x}-{attempt}.colibri-build",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// Creates a new, empty file next to `out` under a name that is not taken. `create_new` never
/// opens an existing file and never follows a link that is already there.
fn create_temp(out: &Path) -> std::io::Result<(fs::File, PathBuf)> {
    let name = out
        .file_name()
        .map_or_else(|| "out".into(), |n| n.to_string_lossy().into_owned());
    let mut last = None;
    for attempt in 0..16 {
        let tmp = out.with_file_name(temp_name(&name, attempt));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
        {
            Ok(file) => return Ok((file, tmp)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => last = Some(e),
            Err(e) => return Err(e),
        }
    }
    Err(last.expect("at least one attempt was made"))
}

/// Copies `runner` to `out` and appends the pack. The file is made new under another name,
/// with the permissions of the runner, and moved into place, so a failure leaves nothing
/// half-written (and a running copy of `out` is not written into).
fn write_executable(runner: &Path, out: &Path, pack: &Pack) -> Result<(), String> {
    let cannot = |e: std::io::Error| format!("cannot write `{}`: {e}", out.display());
    let mut source = fs::File::open(runner)
        .map_err(|e| format!("cannot read the runner `{}`: {e}", runner.display()))?;
    if pack::has_pack(&mut source)? {
        return Err(format!(
            "the runner `{}` is a built program already",
            runner.display()
        ));
    }
    let permissions = source.metadata().map_err(cannot)?.permissions();
    let (mut file, tmp) = create_temp(out).map_err(cannot)?;
    let result = (|| {
        use std::io::Seek;
        source.rewind()?;
        let runner_len = std::io::copy(&mut source, &mut file)?;
        {
            let mut buffered = std::io::BufWriter::new(&mut file);
            pack.write_to(&mut buffered, runner_len)
                .map_err(std::io::Error::other)?;
            buffered.flush()?;
        }
        file.set_permissions(permissions)?;
        file.sync_all()?;
        fs::rename(&tmp, out)
    })()
    .map_err(cannot);
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("colibri-bc-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn tiny_pack() -> Pack {
        Pack {
            name: "p".to_string(),
            start: Start::File("p.colibri".to_string()),
            cfg: colibri::BuildCfg::default(),
            files: Vec::new(),
            assets: Vec::new(),
            assets_at: 0,
        }
    }

    #[test]
    fn a_failed_write_leaves_no_temporary_file() {
        let dir = scratch("failed");
        fs::write(dir.join("runner"), b"runner code").unwrap();
        // The output is a folder with something in it: the move into place cannot work, after
        // the file has been made and filled.
        fs::create_dir_all(dir.join("out")).unwrap();
        fs::write(dir.join("out").join("keep"), b"x").unwrap();
        let err = write_executable(&dir.join("runner"), &dir.join("out"), &tiny_pack());
        assert!(err.is_err());
        let mut names: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["out", "runner"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn temporary_names_are_never_the_same_twice() {
        let a = temp_name("out", 0);
        let b = temp_name("out", 0);
        assert_ne!(a, b);
        assert!(a.starts_with(".out.") && a.ends_with(".colibri-build"));
    }

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
