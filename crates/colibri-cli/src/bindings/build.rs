//! Building the project runner and caching it. A runner is a cargo workspace (see [`super::r#gen`])
//! under the cache folder; `ensure_runner` computes a stamp from everything the binary depends on
//! and calls cargo only when no `runner-<stamp>` is there yet. The binary is copied out under that
//! name and never overwritten (Windows cannot replace a running executable), the lock cargo
//! resolved is copied back to `colibri.rust.lock` after a success, and old runners are pruned.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use colibri_runtime::ProjectBinding;

use super::engine::{self, Engine};
use super::r#gen::{self, BindingCrate, CrateSource, RUNNER_BIN};

/// The project's lock of the Rust crates, next to `colibri.toml`.
pub const LOCK_FILE: &str = "colibri.rust.lock";

/// The line `colibri run` and friends print before the first build of a project.
pub const BUILDING: &str = "building the Rust bindings of this project (once, then cached)";

/// Runners kept in a workspace folder after a build (the newest ones: a project's `colibri-run`
/// and `release` runners, and the one before each).
const KEEP: usize = 4;

/// What the cargo profile of the runner is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Profile {
    /// `colibri run`: no LTO, many codegen units.
    Run,
    /// `colibri build`: as `colibri` itself is built.
    Release,
}

impl Profile {
    /// The profile name in the generated manifest, which is also its folder under `target`.
    pub fn name(self) -> &'static str {
        match self {
            Profile::Run => "colibri-run",
            Profile::Release => "release",
        }
    }
}

/// What the caller asks for.
#[derive(Clone, Copy, Debug)]
pub struct Options {
    pub profile: Profile,
    /// `--locked`: cargo fails instead of changing the lock.
    pub locked: bool,
    /// `--offline`: cargo never uses the network.
    pub offline: bool,
}

/// What a build depends on besides the project: the machine's facts, replaceable in a test.
pub struct Env {
    /// The folder of the runners (`<cache>/runners`).
    pub runners: PathBuf,
    /// The cargo to run; none means `CARGO`, else the one on `PATH`.
    pub cargo: Option<PathBuf>,
    /// Environment variables for the cargo process.
    pub cargo_env: Vec<(String, String)>,
    pub engine: engine::Inputs,
}

impl Env {
    /// This machine: the cache folder of `colibri`, `CARGO` or `PATH`, the engine of this binary.
    pub fn current() -> Result<Env, String> {
        let root = colibri_runtime::cache_root()?;
        Ok(Env {
            runners: root.join("runners"),
            cargo: None,
            cargo_env: Vec::new(),
            engine: engine::Inputs::current(),
        })
    }
}

/// A runner that is ready to run.
#[derive(Debug)]
pub struct Runner {
    pub path: PathBuf,
    /// Whether cargo ran for it (false: it was cached).
    pub built: bool,
}

/// The package that holds `path` (a folder, or a script below one) and its Rust bindings.
pub fn project(path: &Path) -> Result<(PathBuf, Vec<ProjectBinding>), String> {
    let start = if path.is_dir() {
        path.to_path_buf()
    } else {
        path.parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."))
    };
    let start = if start.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        start
    };
    let start = canonical(&start)?;
    let root = start
        .ancestors()
        .find(|d| d.join(colibri_runtime::MANIFEST_FILE).is_file())
        .map(Path::to_path_buf)
        .ok_or_else(|| {
            format!(
                "`{}` is not in a package (there is no `colibri.toml` above it)",
                path.display()
            )
        })?;
    let names: Vec<String> = colibri_std::MODULES
        .iter()
        .map(|m| format!("std.{m}"))
        .collect();
    let entry = colibri_runtime::Entry::Package(&root);
    let bindings = colibri_runtime::project_bindings(
        &colibri_runtime::DiskFs,
        entry,
        crate::build_cfg(),
        &names,
    )?;
    Ok((root, bindings))
}

fn canonical(path: &Path) -> Result<PathBuf, String> {
    std::fs::canonicalize(path)
        .map(|p| PathBuf::from(r#gen::strip_verbatim(&p.to_string_lossy())))
        .map_err(|e| format!("cannot read `{}`: {e}", path.display()))
}

/// The runner of the project in `root` with these bindings, built if no cached one fits.
pub fn ensure_runner(
    env: &Env,
    root: &Path,
    bindings: &[ProjectBinding],
    options: Options,
) -> Result<Runner, String> {
    if bindings.is_empty() {
        return Err(
            "this project declares no `[rust-dependencies]`, so it has no runner to build"
                .to_string(),
        );
    }
    let crates = crates_of(bindings)?;
    let engine = engine::resolve(&env.engine)?;
    let generated = r#gen::generate(&engine, &crates)?;
    let lock_path = root.join(LOCK_FILE);
    let (lock_in, lock_present) = match std::fs::read_to_string(&lock_path) {
        Ok(text) => (normalize(&text), true),
        Err(_) => (normalize(&generated.seed_lock), false),
    };
    let tree = tree_identity(&crates, &engine);
    let stamp_of = |lock: &str| {
        stamp(
            &generated.cargo_toml,
            &generated.main_rs,
            lock,
            &tree,
            options.profile,
        )
    };
    let dir = env.runners.join(format!(
        "{:016x}",
        fnv(r#gen::display_path(root).as_bytes())
    ));
    let exe = if cfg!(windows) { ".exe" } else { "" };
    let runner_in = |stamp: &str| dir.join(format!("runner-{stamp}{exe}"));
    let first = runner_in(&stamp_of(&lock_in));
    if first.is_file() {
        return Ok(Runner {
            path: first,
            built: false,
        });
    }

    let cargo = find_cargo(env.cargo.as_deref())?;
    write_if_changed(&dir.join("Cargo.toml"), &generated.cargo_toml)?;
    write_if_changed(&dir.join("src").join("main.rs"), &generated.main_rs)?;
    write_if_changed(&dir.join("Cargo.lock"), &lock_in)?;
    let target = env
        .runners
        .join(format!("target-{}", env!("CARGO_PKG_VERSION")));
    eprintln!("{BUILDING}");
    run_cargo(env, &cargo, &dir, &target, options)?;

    let produced = target
        .join(options.profile.name())
        .join(format!("{RUNNER_BIN}{exe}"));
    if !produced.is_file() {
        return Err(format!(
            "cargo finished but `{}` is not there",
            produced.display()
        ));
    }
    let lock_out = normalize(
        &std::fs::read_to_string(dir.join("Cargo.lock"))
            .map_err(|e| format!("cannot read the lock cargo wrote: {e}"))?,
    );
    check_one_runtime(&lock_out)?;
    // The runner is named for the lock it was built with, which is what the next call will see
    // once the lock is copied back.
    let path = runner_in(&stamp_of(&lock_out));
    install(&produced, &path)?;
    if lock_out != lock_in || !lock_present {
        write_if_changed(&lock_path, &lock_out)?;
    }
    prune(&dir, &path);
    Ok(Runner { path, built: true })
}

/// The binding crates of every package, each resolved against the folder that declares it.
fn crates_of(bindings: &[ProjectBinding]) -> Result<Vec<BindingCrate>, String> {
    let mut crates = Vec::new();
    let mut errors = Vec::new();
    for b in bindings {
        match r#gen::resolve_crates(std::slice::from_ref(&b.dep), &b.dir) {
            Ok(found) => crates.extend(found),
            Err(e) => errors.extend(e),
        }
    }
    if errors.is_empty() {
        crates.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(crates)
    } else {
        Err(errors.join("\n"))
    }
}

/// `\r\n` is `\n`: a lock checked out on Windows is the same lock.
fn normalize(text: &str) -> String {
    text.replace("\r\n", "\n")
}

/// The part of the cache key that comes from files: each path dependency and the engine source.
fn tree_identity(crates: &[BindingCrate], engine: &Engine) -> String {
    let mut text = String::new();
    for c in crates {
        if let CrateSource::Path { dir, .. } = &c.source {
            let _ = writeln!(
                text,
                "path {} {:016x}",
                c.key,
                walk_hash(dir, &["target", ".git"])
            );
        }
    }
    match engine {
        Engine::Git { url, rev } => {
            let _ = writeln!(text, "engine git {url} {rev}");
        }
        Engine::Path(root) => {
            let _ = writeln!(text, "engine path {:016x}", engine_hash(root));
        }
    }
    text
}

/// What of a Colibri tree a runner is built from: the workspace manifest and, for each crate,
/// its manifest, build script and `src/`. Tests, docs and examples do not change a runner.
/// A walk of (path, size, mtime) costs a few thousand `stat`s, far less than asking git, and
/// it sees uncommitted edits, which `HEAD` plus a dirty flag would only half see.
fn engine_hash(root: &Path) -> u64 {
    let mut h = Fnv::new();
    file_facts(&mut h, &root.join("Cargo.toml"), "Cargo.toml");
    let mut crates: Vec<PathBuf> = std::fs::read_dir(root.join("crates"))
        .map(|rd| rd.filter_map(Result::ok).map(|e| e.path()).collect())
        .unwrap_or_default();
    crates.sort();
    for c in crates {
        let name = c
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        h.write(name.as_bytes());
        file_facts(&mut h, &c.join("Cargo.toml"), "Cargo.toml");
        file_facts(&mut h, &c.join("build.rs"), "build.rs");
        let mut sub = Fnv::new();
        walk_into(&mut sub, &c.join("src"), Path::new(""), &["target", ".git"]);
        h.write(&sub.finish().to_le_bytes());
    }
    h.finish()
}

/// The hash of the files below `dir`: relative path, size and modification time of each,
/// skipping the folders named in `skip` (at any depth).
fn walk_hash(dir: &Path, skip: &[&str]) -> u64 {
    let mut h = Fnv::new();
    walk_into(&mut h, dir, Path::new(""), skip);
    h.finish()
}

fn walk_into(h: &mut Fnv, base: &Path, rel: &Path, skip: &[&str]) {
    let Ok(read) = std::fs::read_dir(base.join(rel)) else {
        return;
    };
    let mut entries: Vec<_> = read.filter_map(Result::ok).collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name();
        let here = rel.join(&name);
        let Ok(meta) = std::fs::symlink_metadata(e.path()) else {
            continue;
        };
        if meta.is_dir() {
            if skip.iter().any(|s| name == *s) {
                continue;
            }
            h.write(b"d");
            h.write(here.to_string_lossy().as_bytes());
            walk_into(h, base, &here, skip);
        } else {
            h.write(b"f");
            h.write(here.to_string_lossy().as_bytes());
            facts(h, &meta);
        }
    }
}

fn file_facts(h: &mut Fnv, path: &Path, label: &str) {
    h.write(label.as_bytes());
    if let Ok(meta) = std::fs::metadata(path) {
        facts(h, &meta);
    }
}

fn facts(h: &mut Fnv, meta: &std::fs::Metadata) {
    h.write(&meta.len().to_le_bytes());
    let nanos = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos());
    h.write(&nanos.to_le_bytes());
}

/// The cache key: 16 hex digits over every input of the build.
fn stamp(cargo_toml: &str, main_rs: &str, lock: &str, tree: &str, profile: Profile) -> String {
    let mut h = Fnv::new();
    for part in [
        env!("CARGO_PKG_VERSION"),
        env!("COLIBRI_HOST_TARGET"),
        profile.name(),
        cargo_toml,
        main_rs,
        lock,
        tree,
    ] {
        h.write(&(part.len() as u64).to_le_bytes());
        h.write(part.as_bytes());
    }
    format!("{:016x}", h.finish())
}

struct Fnv(u64);

impl Fnv {
    fn new() -> Fnv {
        Fnv(0xcbf2_9ce4_8422_2325)
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 ^= u64::from(b);
            self.0 = self.0.wrapping_mul(0x0100_0000_01b3);
        }
    }
    fn finish(&self) -> u64 {
        self.0
    }
}

fn fnv(bytes: &[u8]) -> u64 {
    let mut h = Fnv::new();
    h.write(bytes);
    h.finish()
}

/// Why there is no cargo to run.
pub const NO_CARGO: &str = "cannot build the Rust bindings of this project: cargo was not found (looked at the CARGO variable, then PATH)\n  install Rust from https://rustup.rs, or use a project runner built elsewhere with `--runner PATH`";

fn find_cargo(given: Option<&Path>) -> Result<PathBuf, String> {
    if let Some(p) = given {
        return Ok(p.to_path_buf());
    }
    if let Some(var) = std::env::var_os("CARGO").filter(|v| !v.is_empty()) {
        let path = PathBuf::from(&var);
        // A name without a folder is looked up on `PATH` like any command.
        if path.components().count() > 1 {
            return if path.is_file() {
                Ok(path)
            } else {
                Err(format!(
                    "the CARGO variable names `{}`, which is not a file\n  unset it to use the cargo on PATH, or {}",
                    path.display(),
                    NO_CARGO.split_once("\n  ").map_or("", |(_, tail)| tail)
                ))
            };
        }
        if let Some(found) = on_path(&var) {
            return Ok(found);
        }
        return Err(NO_CARGO.to_string());
    }
    on_path(OsString::from("cargo").as_os_str()).ok_or_else(|| NO_CARGO.to_string())
}

fn on_path(name: &std::ffi::OsStr) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&paths) {
        let plain = dir.join(name);
        if plain.is_file() {
            return Some(plain);
        }
        if cfg!(windows) {
            let exe = dir.join(format!("{}.exe", name.to_string_lossy()));
            if exe.is_file() {
                return Some(exe);
            }
        }
    }
    None
}

/// The text of what cargo printed that says it could not use the network, if it did.
fn network_trouble(tail: &str, offline: bool) -> Option<&'static str> {
    if offline
        && (tail.contains("--offline was specified")
            || tail.contains("offline mode")
            || tail.contains("no matching package"))
    {
        return Some(
            "cargo is offline and a crate it needs is not in its cache\n  run the build once without `--offline`, or `cargo fetch` the crates first",
        );
    }
    let needles = [
        "failed to download",
        "failed to fetch",
        "failed to get",
        "network failure",
        "spurious network error",
        "Could not resolve host",
        "couldn't resolve host",
        "failed to resolve address",
        "Unable to update registry",
        "unable to update",
        "failed to query replaced source",
    ];
    if needles.iter().any(|n| tail.contains(n)) {
        return Some(
            "cargo could not reach the network\n  fetch the crates once with `cargo fetch` (in the generated workspace shown above), then build again with `--offline`",
        );
    }
    None
}

fn run_cargo(
    env: &Env,
    cargo: &Path,
    dir: &Path,
    target: &Path,
    options: Options,
) -> Result<(), String> {
    let mut cmd = Command::new(cargo);
    cmd.arg("build")
        .arg("--profile")
        .arg(options.profile.name())
        .arg("--manifest-path")
        .arg(dir.join("Cargo.toml"))
        .arg("--target-dir")
        .arg(target)
        .arg("--bin")
        .arg(RUNNER_BIN);
    if options.locked {
        cmd.arg("--locked");
    }
    if options.offline {
        cmd.arg("--offline");
    }
    for (k, v) in &env.cargo_env {
        cmd.env(k, v);
    }
    // Everything cargo prints goes to stderr, as it would in a terminal; its standard error is
    // read here too, to recognise a network failure.
    cmd.stdin(Stdio::null())
        .stdout(Stdio::from(std::io::stderr()))
        .stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("cannot run cargo (`{}`): {e}", cargo.display()))?;
    let mut tail: Vec<u8> = Vec::new();
    if let Some(mut err) = child.stderr.take() {
        let mut buf = [0u8; 4096];
        let mut out = std::io::stderr();
        while let Ok(n) = err.read(&mut buf) {
            if n == 0 {
                break;
            }
            let _ = out.write_all(&buf[..n]);
            let _ = out.flush();
            tail.extend_from_slice(&buf[..n]);
            if tail.len() > 64 * 1024 {
                tail.drain(..tail.len() - 32 * 1024);
            }
        }
    }
    let status = child
        .wait()
        .map_err(|e| format!("cannot wait for cargo: {e}"))?;
    if status.success() {
        return Ok(());
    }
    let tail = String::from_utf8_lossy(&tail);
    let why = network_trouble(&tail, options.offline);
    let mut message = format!(
        "the Rust bindings of this project could not be built (cargo {}); the generated workspace is `{}`",
        status
            .code()
            .map_or("was stopped".to_string(), |c| format!("exited with {c}")),
        dir.display()
    );
    if let Some(why) = why {
        message.push_str("\n  ");
        message.push_str(why);
    }
    if options.locked && tail.contains("lock file") {
        message.push_str("\n  `--locked` needs a `colibri.rust.lock` that is up to date: build once without it and commit the lock");
    }
    Err(message)
}

/// Refuses a lock with two `colibri-runtime` packages: values of one runtime are not those of
/// the other, so a binding would never meet the engine.
fn check_one_runtime(lock: &str) -> Result<(), String> {
    let count = lock
        .lines()
        .filter(|l| l.trim() == "name = \"colibri-runtime\"")
        .count();
    if count > 1 {
        return Err(
            "the Rust bindings of this project build two copies of `colibri-runtime`, so they could never meet the engine\n  a binding crate must name `colibri-runtime = \"0.1\"` (not a path, a git url or another version); the runner patches it to its own"
                .to_string(),
        );
    }
    Ok(())
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_name(path: &Path) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".tmp{}-{n}", std::process::id()));
    path.with_file_name(name)
}

/// Writes `text` to `path` through a temporary file in the same folder, unless it holds that
/// already: a reader never sees half a file, and an untouched file keeps its time.
pub fn write_if_changed(path: &Path, text: &str) -> Result<bool, String> {
    if std::fs::read_to_string(path).is_ok_and(|old| old == text) {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create `{}`: {e}", parent.display()))?;
    }
    let tmp = temp_name(path);
    std::fs::write(&tmp, text).map_err(|e| format!("cannot write `{}`: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("cannot write `{}`: {e}", path.display())
    })?;
    Ok(true)
}

#[doc(hidden)]
/// Puts the binary at `to`, which is never replaced: a link that fails because `to` exists
/// means another build made it first, and that one is as good (and may be running).
pub fn install(from: &Path, to: &Path) -> Result<(), String> {
    if to.exists() {
        return Ok(());
    }
    let tmp = temp_name(to);
    std::fs::copy(from, &tmp)
        .map_err(|e| format!("cannot copy the runner to `{}`: {e}", tmp.display()))?;
    let linked = std::fs::hard_link(&tmp, to);
    let result = match linked {
        Ok(()) => Ok(()),
        Err(_) if to.exists() => Ok(()),
        // A file system without links: a rename that cannot replace a file is as good.
        Err(_) => std::fs::rename(&tmp, to)
            .map_err(|e| format!("cannot install the runner at `{}`: {e}", to.display())),
    };
    let _ = std::fs::remove_file(&tmp);
    result
}

/// Removes all but the newest [`KEEP`] runners of `dir` (never `keep`), ignoring any error (a
/// runner that is running cannot be removed on Windows, and that is fine).
fn prune(dir: &Path, keep: &Path) {
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    let mut runners: Vec<(std::time::SystemTime, PathBuf)> = read
        .filter_map(Result::ok)
        .filter(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.starts_with("runner-") && !name.contains(".tmp")
        })
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    runners.sort_by(|a, b| b.cmp(a));
    for (_, path) in runners.into_iter().skip(KEEP) {
        if path != keep {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stamp_follows_every_input() {
        let base = |c: &str, m: &str, l: &str, t: &str, p| stamp(c, m, l, t, p);
        let s = base("c", "m", "l", "t", Profile::Run);
        assert_eq!(s, base("c", "m", "l", "t", Profile::Run));
        for other in [
            base("c2", "m", "l", "t", Profile::Run),
            base("c", "m2", "l", "t", Profile::Run),
            base("c", "m", "l2", "t", Profile::Run),
            base("c", "m", "l", "t2", Profile::Run),
            base("c", "m", "l", "t", Profile::Release),
        ] {
            assert_ne!(s, other);
        }
        // Parts do not run into each other.
        assert_ne!(
            base("ab", "c", "l", "t", Profile::Run),
            base("a", "bc", "l", "t", Profile::Run)
        );
    }

    /// A Colibri-like tree in a folder of its own, removed at the end.
    struct Tree(PathBuf);

    impl Tree {
        fn new(name: &str) -> Tree {
            let dir =
                std::env::temp_dir().join(format!("colibri-bstamp-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            for (rel, text) in [
                ("Cargo.toml", "[workspace]\n"),
                ("crates/a/Cargo.toml", "[package]\n"),
                ("crates/a/src/lib.rs", "fn a() {}\n"),
                ("crates/a/tests/t.rs", "fn t() {}\n"),
                ("crates/b/Cargo.toml", "[package]\n"),
                ("crates/b/src/lib.rs", "fn b() {}\n"),
            ] {
                let full = dir.join(rel);
                std::fs::create_dir_all(full.parent().unwrap()).unwrap();
                std::fs::write(full, text).unwrap();
            }
            Tree(dir)
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn the_engine_identity_follows_the_crates_sources_only() {
        let t = Tree::new("engine");
        let before = engine_hash(&t.0);
        assert_eq!(before, engine_hash(&t.0));
        // Tests, docs and the like are no part of a runner.
        std::fs::write(t.0.join("crates/a/tests/t.rs"), "changed, longer\n").unwrap();
        std::fs::write(t.0.join("README.md"), "x").unwrap();
        assert_eq!(before, engine_hash(&t.0));
        // A source file, a new one, a manifest, the workspace manifest.
        for (rel, text) in [
            ("crates/b/src/lib.rs", "fn b() { 1 }\n"),
            ("crates/b/src/new.rs", "\n"),
            ("crates/a/Cargo.toml", "[package]\nname = \"a\"\n"),
            ("Cargo.toml", "[workspace]\nresolver = \"3\"\n"),
        ] {
            let was = engine_hash(&t.0);
            std::fs::write(t.0.join(rel), text).unwrap();
            assert_ne!(was, engine_hash(&t.0), "{rel}");
        }
        // The same size, another time.
        let was = engine_hash(&t.0);
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(t.0.join("crates/a/src/lib.rs"))
            .unwrap();
        let later = f.metadata().unwrap().modified().unwrap() + std::time::Duration::from_secs(9);
        f.set_modified(later).unwrap();
        assert_ne!(was, engine_hash(&t.0));
    }

    #[test]
    fn crlf_is_lf() {
        assert_eq!(normalize("a\r\nb\n"), "a\nb\n");
    }

    #[test]
    fn two_runtimes_in_a_lock_are_refused() {
        let one = "[[package]]\nname = \"colibri-runtime\"\nversion = \"0.1.0\"\n";
        assert!(check_one_runtime(one).is_ok());
        let two = format!("{one}\n{one}");
        let message = check_one_runtime(&two).unwrap_err();
        assert!(
            message.contains("two copies of `colibri-runtime`"),
            "{message}"
        );
        assert!(message.contains("colibri-runtime = \"0.1\""), "{message}");
    }

    #[test]
    fn network_failures_are_recognised() {
        let online = network_trouble("error: failed to download `x v1`\n", false).unwrap();
        assert!(
            online.contains("cargo fetch") && online.contains("--offline"),
            "{online}"
        );
        let offline = network_trouble(
            "attempting to make an HTTP request, but --offline was specified",
            true,
        )
        .unwrap();
        assert!(offline.contains("without `--offline`"), "{offline}");
        assert!(network_trouble("error[E0425]: cannot find value", false).is_none());
    }
}
