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

use super::command;
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
    /// Copy the lock cargo resolved back to `colibri.rust.lock` (`colibri bindings --check` does
    /// not: it reports instead).
    pub write_lock: bool,
    /// After a build, write the `<key>.cbi` that differ (never into a git checkout), and when the
    /// runner was cached, say which differ without writing: what `run` and `test` do.
    pub write_interfaces: bool,
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
    /// The resolved lock differs from `colibri.rust.lock` (or there is none): it was written back
    /// if `Options::write_lock`, else the project's lock is out of date.
    pub lock_changed: bool,
    /// Interfaces written after this build (`Options::write_interfaces`).
    pub written: Vec<PathBuf>,
    /// Interfaces that differ from the runner's and were not written because the runner was
    /// cached (`Options::write_interfaces`).
    pub stale: Vec<PathBuf>,
    /// Those of a git dependency (the dependency's key and the file): never written.
    pub git_stale: Vec<(String, PathBuf)>,
    /// Why the interfaces could not be compared or written (the run goes on without them).
    pub interface_problem: Option<String>,
}

/// The package that holds `path` (a folder, or a script below one) and its Rust bindings.
pub fn project(path: &Path) -> Result<(PathBuf, Vec<ProjectBinding>), String> {
    // As the loader finds a package: a folder is the package, a script's package is its own folder.
    let folder = if path.is_dir() {
        path.to_path_buf()
    } else {
        path.parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
    };
    let root = canonical(&folder)?;
    if !root.join(colibri_runtime::MANIFEST_FILE).is_file() {
        return Err(format!(
            "`{}` has no `colibri.toml`: `colibri bindings` takes a package folder",
            root.display()
        ));
    }
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
    let dir = env.runners.join(format!(
        "{:016x}",
        fnv(r#gen::display_path(root).as_bytes())
    ));
    let exe = if cfg!(windows) { ".exe" } else { "" };
    let runner_in = |stamp: &str| dir.join(format!("runner-{stamp}{exe}"));
    // What the stamp is made of that can change under us: the lock and the files.
    let state = || {
        let (lock_in, lock_present) = match std::fs::read_to_string(&lock_path) {
            Ok(text) => (normalize(&text), true),
            Err(_) => (normalize(&generated.seed_lock), false),
        };
        (lock_in, lock_present, tree_identity(&crates, &engine))
    };
    let stamp_of = |lock: &str, tree: &str| {
        stamp(
            &generated.cargo_toml,
            &generated.main_rs,
            lock,
            tree,
            options.profile,
        )
    };
    let (lock_in, _, tree) = state();
    let first = runner_in(&stamp_of(&lock_in, &tree));
    if first.is_file() {
        return cached_runner(first, bindings, options);
    }

    let cargo = find_cargo(env.cargo.as_deref())?;
    // One build at a time on this machine: two projects share the target folder (and its binary's
    // path), and two builds of one project share its workspace and its lock. What the build
    // before ours made is looked at again once we hold the lock.
    let _guard = BuildLock::acquire(&env.runners, &mut |m| eprintln!("{m}"))?;
    let (lock_in, lock_present, tree) = state();
    let first = runner_in(&stamp_of(&lock_in, &tree));
    if first.is_file() {
        return cached_runner(first, bindings, options);
    }
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
    let path = runner_in(&stamp_of(&lock_out, &tree));
    // What an earlier runner of this name left (the file was deleted, the folder was not) is not
    // what this one says.
    let _ = std::fs::remove_dir_all(interfaces_dir(&path));
    install(&produced, &path)?;
    let lock_changed = lock_out != lock_in || !lock_present;
    if lock_changed && options.write_lock {
        write_if_changed(&lock_path, &lock_out)?;
    }
    prune(&dir, &path);
    let mut runner = Runner {
        path,
        built: true,
        lock_changed,
        written: Vec::new(),
        stale: Vec::new(),
        git_stale: Vec::new(),
        interface_problem: None,
    };
    // Only now: the build is over, installed and its lock copied back. A build that failed anywhere
    // before leaves the interfaces as they were.
    if options.write_interfaces {
        match command::write_interfaces(&runner, bindings, false) {
            Ok(report) => {
                runner.written = report.changed;
                runner.git_stale = named(bindings, report.git_stale);
            }
            Err(e) => runner.interface_problem = Some(e),
        }
    }
    Ok(runner)
}

/// A runner that was there already. With `Options::write_interfaces` the project's interfaces are
/// compared with the runner's (kept next to it, so this reads files and does not start it) and the
/// ones that differ are named, not written.
fn cached_runner(
    path: PathBuf,
    bindings: &[ProjectBinding],
    options: Options,
) -> Result<Runner, String> {
    let mut runner = Runner {
        path,
        built: false,
        lock_changed: false,
        written: Vec::new(),
        stale: Vec::new(),
        git_stale: Vec::new(),
        interface_problem: None,
    };
    if options.write_interfaces {
        match command::write_interfaces(&runner, bindings, true) {
            Ok(report) => {
                runner.stale = report.changed;
                runner.git_stale = named(bindings, report.git_stale);
            }
            Err(e) => runner.interface_problem = Some(e),
        }
    }
    Ok(runner)
}

/// The key of the binding each interface file belongs to.
fn named(bindings: &[ProjectBinding], files: Vec<PathBuf>) -> Vec<(String, PathBuf)> {
    files
        .into_iter()
        .map(|f| {
            let key = bindings
                .iter()
                .find(|b| b.interface == f)
                .map_or_else(String::new, |b| b.dep.name.clone());
            (key, f)
        })
        .collect()
}

/// The folder next to a runner that holds the interfaces it wrote (`<key>.cbi`): a runner never
/// changes, so what it says of its bindings is asked once.
pub fn interfaces_dir(runner: &Path) -> PathBuf {
    let mut name = runner.file_name().unwrap_or_default().to_os_string();
    name.push(".interfaces");
    runner.with_file_name(name)
}

/// The marker (`\x01colibri-bindings=<hex>\x02`) a runner of exactly these bindings holds, and
/// their keys.
pub fn expected_marker(bindings: &[ProjectBinding]) -> Result<(String, Vec<String>), String> {
    let crates = crates_of(bindings)?;
    let refs: Vec<&BindingCrate> = crates.iter().collect();
    Ok((
        r#gen::bindings_marker(&refs),
        crates.iter().map(|c| c.key.clone()).collect(),
    ))
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

/// The machine-wide build lock: held from writing a workspace until the runner is installed and
/// the lock is copied back, and dropped (so released) when the build is over or fails. `None` in
/// it: the file system cannot lock, so the build goes on without (and said so).
struct BuildLock(Option<std::fs::File>);

impl BuildLock {
    /// Takes the lock without waiting if it is free; else says (through `say`) that another build
    /// holds it, and waits.
    fn acquire(runners: &Path, say: &mut dyn FnMut(&str)) -> Result<BuildLock, String> {
        use std::fs::TryLockError;
        std::fs::create_dir_all(runners)
            .map_err(|e| format!("cannot create `{}`: {e}", runners.display()))?;
        let path = runners.join("build.lock");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(|e| format!("cannot open `{}`: {e}", path.display()))?;
        match file.try_lock() {
            Ok(()) => Ok(BuildLock(Some(file))),
            Err(TryLockError::WouldBlock) => {
                say(&format!(
                    "waiting for another Colibri build ({})",
                    path.display()
                ));
                file.lock()
                    .map_err(|e| format!("cannot lock `{}`: {e}", path.display()))?;
                Ok(BuildLock(Some(file)))
            }
            Err(TryLockError::Error(e)) if e.kind() == std::io::ErrorKind::Unsupported => {
                say(&format!(
                    "warning: the file system of `{}` cannot lock files: building without the lock, so two builds at once may disturb each other",
                    path.display()
                ));
                Ok(BuildLock(None))
            }
            Err(TryLockError::Error(e)) => Err(format!("cannot lock `{}`: {e}", path.display())),
        }
    }
}

impl Drop for BuildLock {
    fn drop(&mut self) {
        if let Some(file) = &self.0 {
            let _ = file.unlock();
        }
    }
}

/// `\r\n` is `\n`: a lock checked out on Windows is the same lock.
fn normalize(text: &str) -> String {
    text.replace("\r\n", "\n")
}

/// The part of the cache key that comes from files: each path binding crate and, transitively,
/// every crate it names with `path =` (a binding is usually a thin crate over a core crate next
/// to it), and the engine source. Each folder is walked once, whatever reaches it.
fn tree_identity(crates: &[BindingCrate], engine: &Engine) -> String {
    let mut text = String::new();
    let mut seen: std::collections::BTreeSet<PathBuf> = std::collections::BTreeSet::new();
    let mut todo: Vec<PathBuf> = crates
        .iter()
        .filter_map(|c| match &c.source {
            CrateSource::Path { dir, .. } => Some(dir.clone()),
            _ => None,
        })
        .collect();
    while let Some(dir) = todo.pop() {
        let dir = std::fs::canonicalize(&dir).unwrap_or(dir);
        if !seen.insert(dir.clone()) {
            continue;
        }
        if let Ok(manifest) = std::fs::read_to_string(dir.join("Cargo.toml")) {
            todo.extend(r#gen::path_dependencies(&manifest, &dir));
        }
    }
    for dir in &seen {
        let _ = writeln!(
            text,
            "path {} {:016x}",
            r#gen::display_path(dir),
            walk_hash(dir, &["target", ".git"])
        );
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
    let entries: Vec<_> = read.filter_map(Result::ok).collect();
    let mut runners: Vec<(std::time::SystemTime, PathBuf)> = entries
        .iter()
        .filter(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.starts_with("runner-") && !name.contains(".tmp") && !name.ends_with(".interfaces")
        })
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    runners.sort_by(|a, b| b.cmp(a));
    for (_, path) in runners.into_iter().skip(KEEP) {
        if path != keep {
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_dir_all(interfaces_dir(&path));
        }
    }
    // Leftovers: a kept folder whose runner is gone, and the scratch folder of an interrupted
    // `__write-interfaces` (only old ones: a running process may be using a young one).
    let old = std::time::Duration::from_secs(600);
    for e in &entries {
        let name = e.file_name().to_string_lossy().into_owned();
        let orphan = name.starts_with("runner-")
            && name.ends_with(".interfaces")
            && !dir.join(name.trim_end_matches(".interfaces")).exists();
        let abandoned = name.starts_with("interfaces-")
            && e.metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age > old);
        if orphan || abandoned {
            let _ = std::fs::remove_dir_all(e.path());
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

#[cfg(test)]
mod lock_tests {
    use super::*;

    #[test]
    fn a_second_build_says_it_waits_and_then_gets_the_lock() {
        let dir = std::env::temp_dir().join(format!("colibri-lock-{}", std::process::id()));
        let first = BuildLock::acquire(&dir, &mut |m| panic!("the free lock said `{m}`")).unwrap();
        let mut said = Vec::new();
        let release = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(300));
            drop(first);
        });
        let second = BuildLock::acquire(&dir, &mut |m| said.push(m.to_string())).unwrap();
        release.join().unwrap();
        assert!(second.0.is_some());
        assert_eq!(said.len(), 1, "{said:?}");
        assert!(
            said[0].starts_with("waiting for another Colibri build (")
                && said[0].contains("build.lock"),
            "{said:?}"
        );
        drop(second);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod prune_tests {
    use super::*;

    #[test]
    fn a_kept_folder_without_its_runner_is_removed_and_one_with_it_stays() {
        let dir = std::env::temp_dir().join(format!("colibri-prune-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("runner-b.interfaces")).unwrap();
        std::fs::create_dir_all(dir.join("runner-a.interfaces")).unwrap();
        std::fs::create_dir_all(dir.join("interfaces-1")).unwrap();
        std::fs::write(dir.join("runner-a"), "r").unwrap();
        prune(&dir, &dir.join("runner-a"));
        assert!(dir.join("runner-a.interfaces").is_dir());
        assert!(!dir.join("runner-b.interfaces").exists());
        // A scratch folder of a process that may still run is left (it is young).
        assert!(dir.join("interfaces-1").is_dir());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
