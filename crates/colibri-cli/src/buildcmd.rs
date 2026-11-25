//! `colibri build`: write one executable that holds a program.
//!
//! The executable is a copy of the running `colibri` (the runner) with the program after its
//! code (see [`crate::pack`]). The program is checked first, through a recording file system:
//! the files the check reads are the program, dependencies included.
//!
//! `--target T --runner PATH` writes for another platform: the runner is then a `colibri` of that
//! platform, which this build never runs. It is refused unless it is the right kind of file
//! (ELF, PE or Mach-O, and the processor of `T`), has the version of this `colibri` (a marker
//! string in its data, see [`crate::platform`]) and holds no program already. The target also
//! decides `@when(os == ..)` and `@when(arch == ..)` while the program is checked, since that
//! is when the files to read are chosen. No runner is ever downloaded.
//!
//! A program for macOS has its `__LINKEDIT` segment extended over the pack ([`crate::macho`]), so
//! that `codesign -s -` accepts it: this build does that itself on a Mac, and elsewhere warns
//! that it must be done there.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use colibri_runtime::{self as colibri, Recorded, RecordingFs};

use crate::assets;
use crate::depscmd::{self, DepFlags};
use crate::macho;
use crate::pack::{self, Kind, Pack, PackFile, Start};
use crate::platform::{self, Target};

/// What `colibri build` was asked for.
pub struct Options<'a> {
    /// A script, or a package folder.
    pub path: &'a Path,
    /// Where to write the executable; by default the name of the program in the current
    /// folder.
    pub out: Option<&'a Path>,
    pub deps: DepFlags,
    /// The platform to build for; `runner` is then needed unless it is this one.
    pub target: Option<Target>,
    /// A `colibri` of the target's platform, to copy instead of this one.
    pub runner: Option<&'a Path>,
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
    let runner = choose_runner(options).map_err(fail)?;
    // As `colibri run` does: the dependencies are fetched and locked before anything is read.
    depscmd::prepare(path, options.deps)?;
    let recorder = RecordingFs::new(&colibri::DiskFs);
    crate::checkcmd::check_on(&recorder, path, &[], false, &mut colibri_std::interface)?;
    let reads = recorder.into_files();
    let out = match options.out {
        Some(out) => out.to_path_buf(),
        None => default_out(path, options.target),
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
    if fs::canonicalize(&out).is_ok_and(|o| fs::canonicalize(&runner).is_ok_and(|r| r == o)) {
        return Err(fail(format!(
            "the output `{}` is the runner itself: choose another with `-o`",
            out.display()
        )));
    }
    let mac = crate::build_cfg().os == "macos";
    write_executable(&runner, &out, &pack, mac).map_err(fail)?;
    if mac {
        sign_or_warn(&out, &runner).map_err(|e| {
            let _ = fs::remove_file(&out);
            fail(e)
        })?;
    }
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

fn default_out(path: &Path, target: Option<Target>) -> PathBuf {
    let suffix = target.map_or(std::env::consts::EXE_SUFFIX, Target::exe_suffix);
    PathBuf::from(format!("{}{suffix}", program_name(path)))
}

/// The file to copy as the runner: this `colibri`, or `--runner` after its checks.
fn choose_runner(options: &Options<'_>) -> Result<PathBuf, String> {
    let list = || {
        platform::TARGETS
            .iter()
            .map(|(n, _)| *n)
            .collect::<Vec<_>>()
            .join(", ")
    };
    let Some(given) = options.runner else {
        if let Some(target) = options.target
            && Some(target) != Target::host()
        {
            return Err(format!(
                "building for {} needs `--runner PATH`, a `colibri` of that platform (version {}); \
                 `colibri build` never downloads one",
                target.name(),
                platform::own_version()
            ));
        }
        return std::env::current_exe()
            .map_err(|e| format!("cannot find the running `colibri` to copy: {e}"));
    };
    let Some(target) = options.target else {
        return Err(format!(
            "`--runner` needs `--target`, the platform the runner is for (one of: {})",
            list()
        ));
    };
    check_runner(given, target)?;
    Ok(given.to_path_buf())
}

/// Refuses a runner that is not a `colibri` of `target`'s platform and of this version.
fn check_runner(runner: &Path, target: Target) -> Result<(), String> {
    let shown = runner.display();
    let mut file =
        fs::File::open(runner).map_err(|e| format!("cannot read the runner `{shown}`: {e}"))?;
    if !file.metadata().is_ok_and(|m| m.is_file()) {
        return Err(format!("the runner `{shown}` is not a file"));
    }
    platform::check_format(&mut file, target).map_err(|e| format!("`{shown}`: {e}"))?;
    if pack::has_pack(&mut file)? {
        return Err(format!("the runner `{shown}` is a built program already"));
    }
    use std::io::Seek;
    file.rewind()
        .map_err(|e| format!("cannot read the runner `{shown}`: {e}"))?;
    match platform::runner_version(&mut file)
        .map_err(|e| format!("cannot read the runner `{shown}`: {e}"))?
    {
        Some(v) if v == platform::own_version() => Ok(()),
        Some(v) => Err(format!(
            "the runner `{shown}` is `colibri` {v}, and this is {}: use a runner of the same version",
            platform::own_version()
        )),
        None => Err(format!(
            "the runner `{shown}` does not say which `colibri` version it is: it is not a `colibri` of this version"
        )),
    }
}

/// Signs the program on a Mac, and elsewhere says that it must be signed there.
fn sign_or_warn(out: &Path, runner: &Path) -> Result<(), String> {
    let thin = fs::File::open(runner).is_ok_and(|mut f| {
        let mut b = [0u8; 4];
        std::io::Read::read_exact(&mut f, &mut b).is_ok() && macho::is_thin(&b)
    });
    if !thin {
        eprintln!(
            "warning: the runner is a universal Mach-O file, which cannot be prepared for signing: \
             take one slice with `lipo -thin`, or the program stays unsigned"
        );
        return Ok(());
    }
    if cfg!(target_os = "macos") {
        let result = std::process::Command::new("codesign")
            .args(["-s", "-"])
            .arg(out)
            .output()
            .map_err(|e| format!("cannot run `codesign` to sign `{}`: {e}", out.display()))?;
        if !result.status.success() {
            return Err(format!(
                "`codesign -s -` failed on `{}`: {}",
                out.display(),
                String::from_utf8_lossy(&result.stderr).trim()
            ));
        }
    } else {
        eprintln!(
            "warning: the program for macOS is not signed, and arm64 Macs refuse unsigned programs: \
             on a Mac, run `codesign -s - {}` (or sign it with your identity)",
            out.display()
        );
    }
    Ok(())
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
///
/// With `mac`, a Mach-O runner has its signature taken off and `__LINKEDIT` extended over the
/// pack (see [`crate::macho`]).
fn write_executable(runner: &Path, out: &Path, pack: &Pack, mac: bool) -> Result<(), String> {
    let cannot = |e: std::io::Error| format!("cannot write `{}`: {e}", out.display());
    let mut source = fs::File::open(runner)
        .map_err(|e| format!("cannot read the runner `{}`: {e}", runner.display()))?;
    if pack::has_pack(&mut source)? {
        return Err(format!(
            "the runner `{}` is a built program already",
            runner.display()
        ));
    }
    let metadata = source.metadata().map_err(cannot)?;
    let permissions = metadata.permissions();
    let prepared = if mac {
        mac_head(&mut source, metadata.len())?
    } else {
        None
    };
    let (mut file, tmp) = create_temp(out).map_err(cannot)?;
    let result = (|| {
        use std::io::{Read, Seek, SeekFrom};
        source.rewind()?;
        let keep = prepared.as_ref().map_or(u64::MAX, |p| p.keep);
        let runner_len = std::io::copy(&mut (&mut source).take(keep), &mut file)?;
        {
            let mut buffered = std::io::BufWriter::new(&mut file);
            pack.write_to(&mut buffered, runner_len)
                .map_err(std::io::Error::other)?;
            buffered.flush()?;
        }
        if let Some(p) = &prepared {
            let total = file.seek(SeekFrom::End(0))?;
            file.seek(SeekFrom::Start(0))?;
            file.write_all(&p.head_with(total))?;
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

/// The runner's header and load commands made ready for a pack, if it is a thin Mach-O file.
fn mac_head(source: &mut fs::File, len: u64) -> Result<Option<macho::Prepared>, String> {
    use std::io::{Read, Seek};
    let mut start = [0u8; 32];
    source
        .rewind()
        .map_err(|e| format!("cannot read the runner: {e}"))?;
    if source.read_exact(&mut start).is_err() || !macho::is_thin(&start) {
        return Ok(None);
    }
    let Some(size) = macho::head_len(&start) else {
        return Ok(None);
    };
    let mut head = start.to_vec();
    head.resize(size, 0);
    source
        .read_exact(&mut head[32..])
        .map_err(|e| format!("cannot read the runner: {e}"))?;
    macho::prepare(&head, len).map(Some)
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
        let err = write_executable(&dir.join("runner"), &dir.join("out"), &tiny_pack(), false);
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

    /// What `codesign -s -` does to a prepared file, as far as this reader can tell: the blob
    /// goes to the next multiple of 16 after `__LINKEDIT`'s end, a command for it is added in
    /// the free room after the others, and `__LINKEDIT` grows over it.
    fn fake_codesign(mut file: Vec<u8>, blob: usize) -> Vec<u8> {
        let at = file.len().next_multiple_of(16);
        file.resize(at, 0);
        file.resize(at + blob, 0xEE);
        let ncmds = u32::from_le_bytes(file[16..20].try_into().unwrap());
        let size = u32::from_le_bytes(file[20..24].try_into().unwrap()) as usize;
        let mut cmd = Vec::new();
        cmd.extend(0x1du32.to_le_bytes());
        cmd.extend(16u32.to_le_bytes());
        cmd.extend((at as u32).to_le_bytes());
        cmd.extend((blob as u32).to_le_bytes());
        file[32 + size..32 + size + 16].copy_from_slice(&cmd);
        file[16..20].copy_from_slice(&(ncmds + 1).to_le_bytes());
        file[20..24].copy_from_slice(&((size + 16) as u32).to_le_bytes());
        file
    }

    #[test]
    fn a_program_for_macos_is_signable_and_still_found_after_signing() {
        let dir = scratch("mac");
        let runner = crate::macho::tests::image(0x8000, 0x100, false);
        fs::write(dir.join("runner"), &runner).unwrap();
        let mut pack = tiny_pack();
        pack.assets = vec![crate::pack::PackAsset::held("a.txt", vec![5; 1000])];
        let out = dir.join("out");
        write_executable(&dir.join("runner"), &out, &pack, true).unwrap();
        let built = fs::read(&out).unwrap();
        // The runner's own signature is gone, and what is left of it is untouched.
        assert!(built.len() > 0x7f00 + 1000);
        assert_eq!(&built[0x4000 + 200..0x7f00], &runner[0x4000 + 200..0x7f00]);
        // __LINKEDIT reaches the end of the file, so that codesign accepts it.
        let linkedit = crate::macho::tests::linkedit_of(&built[..32 + 144]);
        assert_eq!(0x4000 + linkedit.1, built.len() as u64);
        // Unsigned, the program is found at the end of the file; signed, before the signature.
        let mut plain = std::io::Cursor::new(built.clone());
        assert!(pack::read_from(&mut plain).unwrap().is_some());
        for blob in [16, 17, 4000] {
            let signed = fake_codesign(built.clone(), blob);
            let mut file = std::io::Cursor::new(signed);
            let found = pack::read_from(&mut file).unwrap().expect("the pack");
            assert_eq!(found.name, "p");
            assert_eq!(found.assets.len(), 1, "blob {blob}");
            assert!(pack::has_pack(&mut file).unwrap());
        }
        // Without `mac`, the runner is copied as it is.
        write_executable(&dir.join("runner"), &out, &pack, false).unwrap();
        assert_eq!(&fs::read(&out).unwrap()[..0x8000], &runner[..]);
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
