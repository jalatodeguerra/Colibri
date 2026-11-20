//! Git dependencies on disk (ARCHITECTURE §11.6b): the cache, and the `git` commands that fill it.
//!
//! ```text
//! <cache>/git/db/<name>-<hash>.git            a bare repository per url: what was fetched
//! <cache>/git/checkouts/<name>-<hash>/<commit>/  the files of one commit, written once and
//!                                                never changed afterwards
//! ```
//!
//! `<cache>` is `$COLIBRI_HOME`, else the platform's cache folder (`$XDG_CACHE_HOME/colibri` or
//! `~/.cache/colibri`; `~/Library/Caches/colibri` on macOS; `%LOCALAPPDATA%\colibri` on
//! Windows). `<hash>` is a fixed hash of the url as written, so two projects that name the same
//! url share their checkouts. Only [`Git`] runs `git`, and only the tool's own commands (not
//! the loader) call it.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::manifest::GitSpec;

/// The environment variable that overrides the cache folder.
pub const HOME_VAR: &str = "COLIBRI_HOME";

/// The cache folder, or why there is none.
pub fn cache_root() -> Result<PathBuf, String> {
    let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty());
    let dir = if let Some(home) = var(HOME_VAR) {
        PathBuf::from(home)
    } else if cfg!(windows) {
        var("LOCALAPPDATA")
            .map(|d| PathBuf::from(d).join("colibri"))
            .ok_or_else(no_cache)?
    } else if cfg!(target_os = "macos") {
        var("HOME")
            .map(|d| PathBuf::from(d).join("Library/Caches/colibri"))
            .ok_or_else(no_cache)?
    } else if let Some(xdg) = var("XDG_CACHE_HOME") {
        PathBuf::from(xdg).join("colibri")
    } else {
        var("HOME")
            .map(|d| PathBuf::from(d).join(".cache/colibri"))
            .ok_or_else(no_cache)?
    };
    Ok(std::path::absolute(&dir).unwrap_or(dir))
}

pub(super) fn no_cache() -> String {
    format!("there is no cache folder for git dependencies: set `{HOME_VAR}` to one")
}

/// A folder name for a repository url: its last path segment and a hash of the whole url.
fn repo_key(url: &str) -> String {
    let last: String = url
        .trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\', ':'])
        .next()
        .unwrap_or("")
        .trim_end_matches(".git")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(40)
        .collect();
    // FNV-1a: fixed by definition, unlike `DefaultHasher`, so the folder is the same everywhere.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in url.bytes() {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    let last = if last.is_empty() {
        "repo".to_string()
    } else {
        last
    };
    format!("{last}-{hash:016x}")
}

/// Where the files of `commit` of the repository at `url` are (once fetched).
pub fn checkout_dir(cache: &Path, url: &str, commit: &str) -> PathBuf {
    cache
        .join("git")
        .join("checkouts")
        .join(repo_key(url))
        .join(commit)
}

/// `std::fs::remove_dir_all`, and when that is refused, once more after the read-only marks
/// are taken off: Windows will not delete a read-only file, and git writes its pack files
/// read-only.
fn remove_tree(dir: &Path) -> std::io::Result<()> {
    fn writable(path: &Path) {
        let Ok(meta) = std::fs::symlink_metadata(path) else {
            return;
        };
        if meta.is_dir() {
            if let Ok(entries) = std::fs::read_dir(path) {
                for entry in entries.flatten() {
                    writable(&entry.path());
                }
            }
        } else if meta.permissions().readonly() {
            let mut perm = meta.permissions();
            #[allow(clippy::permissions_set_readonly_false)]
            perm.set_readonly(false);
            let _ = std::fs::set_permissions(path, perm);
        }
    }
    std::fs::remove_dir_all(dir).or_else(|first| {
        writable(dir);
        std::fs::remove_dir_all(dir).map_err(|_| first)
    })
}

/// The file inside a database folder that says which url it holds.
const URL_FILE: &str = "colibri-url";

/// Where the database of one url is, or would be made.
enum Slot {
    /// A folder that says it holds this url.
    Ours(PathBuf),
    /// The first folder name that is free (or holds nothing usable).
    Free(PathBuf),
}

/// Looks for the database of `url` among `<key>.git`, `<key>-1.git`, `<key>-2.git`, ...: the
/// folder whose [`URL_FILE`] is exactly `url`. Two urls with one key (the key is only a
/// 64-bit hash) must never share a database, because a fetch of one would overwrite the
/// branches and tags of the other: the second goes to the next name.
fn find_slot(dbs: &Path, key: &str, url: &str) -> Slot {
    for n in 0.. {
        let name = if n == 0 {
            format!("{key}.git")
        } else {
            format!("{key}-{n}.git")
        };
        let path = dbs.join(name);
        if !path.exists() {
            return Slot::Free(path);
        }
        match std::fs::read_to_string(path.join(URL_FILE)) {
            Ok(held) if held == url => return Slot::Ours(path),
            Ok(_) => {}
            // No url file: made by a run that did not finish, or damaged. Not trusted.
            Err(_) => return Slot::Free(path),
        }
    }
    unreachable!("the slots are endless")
}

fn db_dir(cache: &Path, url: &str) -> PathBuf {
    let dbs = cache.join("git").join("db");
    match find_slot(&dbs, &repo_key(url), url) {
        Slot::Ours(p) | Slot::Free(p) => p,
    }
}

/// Makes the database for `url` under the key `key` if there is no usable one (the lock of
/// `key` is held), and returns its folder.
///
/// It is made in a temporary folder, with its url file, and renamed into place, so a crash
/// cannot leave a half-made one under the real name; and a folder under the real name that is
/// not a repository (an older crash, a damaged disk) counts as absent and is made again.
fn claim_db(dbs: &Path, key: &str, url: &str) -> Result<PathBuf, String> {
    let db = match find_slot(dbs, key, url) {
        Slot::Ours(p) if is_valid_repo(&p) => return Ok(p),
        Slot::Ours(p) | Slot::Free(p) => p,
    };
    if db.exists() {
        remove_tree(&db)
            .map_err(|e| format!("cannot remove the damaged `{}`: {e}", db.display()))?;
    }
    let name = db.file_name().unwrap_or_default().to_string_lossy();
    let tmp = db.with_file_name(format!(".{name}.tmp{}", std::process::id()));
    let _ = remove_tree(&tmp);
    run(git_command().args(["init", "--bare", "--quiet"]).arg(&tmp))?;
    std::fs::write(tmp.join(URL_FILE), redact_url(url))
        .map_err(|e| format!("cannot write in `{}`: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &db).map_err(|e| {
        let _ = remove_tree(&tmp);
        format!("cannot create `{}`: {e}", db.display())
    })?;
    Ok(db)
}

/// Whether the folder looks like a bare repository (cheap: no `git` process).
fn looks_like_repo(db: &Path) -> bool {
    db.join("HEAD").is_file() && db.join("objects").is_dir()
}

/// Whether the folder is a bare repository that `git` itself accepts.
fn is_valid_repo(db: &Path) -> bool {
    db.is_dir()
        && run(git_command()
            .arg("--git-dir")
            .arg(db)
            .args(["rev-parse", "--git-dir"]))
        .is_ok()
}

/// Folders that a crashed run left behind are removed when they are older than this.
const STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// Removes the entries of `dir` that are temporary folders (a name that starts with `.` and
/// contains `.tmp`) and were last touched more than a day ago: the work of a run that died.
fn remove_stale_tmp(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !(name.starts_with('.') && name.contains(".tmp")) {
            continue;
        }
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > STALE_AFTER);
        if old {
            let path = entry.path();
            let _ = if path.is_dir() {
                remove_tree(&path)
            } else {
                std::fs::remove_file(&path)
            };
        }
    }
}

/// The advisory lock of one repository: held while its database is created or fetched into
/// and while one of its checkouts is written, so concurrent `colibri` processes (parallel
/// builds, an editor and a terminal) take turns. It is released when this is dropped, or when
/// the process dies, so a crash never leaves it held.
///
/// The lock is `std::fs::File::lock` (an `flock`/`LockFileEx` lock; Rust 1.89 and later), so
/// there is no extra dependency.
struct DbLock {
    _file: std::fs::File,
}

impl DbLock {
    /// Takes the lock of `url`, saying on stderr that it is waiting when another process
    /// holds it.
    fn take(cache: &Path, url: &str) -> Result<DbLock, String> {
        DbLock::take_telling(cache, url, &mut |line| eprintln!("{line}"))
    }

    /// [`DbLock::take`], with `tell` receiving the line to show if the lock has to be waited
    /// for.
    fn take_telling(cache: &Path, url: &str, tell: &mut dyn FnMut(&str)) -> Result<DbLock, String> {
        let dbs = cache.join("git").join("db");
        std::fs::create_dir_all(&dbs)
            .map_err(|e| format!("cannot create `{}`: {e}", dbs.display()))?;
        let path = dbs.join(format!("{}.lock", repo_key(url)));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| format!("cannot open `{}`: {e}", path.display()))?;
        match file.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                // A fetch can take long (or hang): a process that waits in silence looks
                // stuck, so say what it waits for.
                tell(&format!(
                    "waiting for another colibri process to finish with `{}` (lock file {})",
                    repo_key(url),
                    path.display()
                ));
                file.lock()
                    .map_err(|e| format!("cannot lock `{}`: {e}", path.display()))?;
            }
            Err(std::fs::TryLockError::Error(e)) => {
                return Err(format!("cannot lock `{}`: {e}", path.display()));
            }
        }
        // Whoever holds the lock is the only one working on this repository: this is the
        // moment to remove what a run that was killed left behind.
        remove_stale_tmp(&dbs);
        remove_stale_tmp(&cache.join("git").join("checkouts").join(repo_key(url)));
        Ok(DbLock { _file: file })
    }
}

/// Runs `git` for one tool command: what may touch the network is up to `offline`.
pub struct Git {
    cache: PathBuf,
    offline: bool,
    /// Urls fetched during this run (each is fetched once).
    fetched: HashSet<String>,
    /// `git` was found.
    git_found: bool,
}

impl Git {
    pub fn new(cache: PathBuf, offline: bool) -> Git {
        Git {
            cache,
            offline,
            fetched: HashSet::new(),
            git_found: false,
        }
    }

    /// Fails with "the `git` program was not found" if there is none, so that even `--offline`
    /// (which only reads the cache) says so rather than that something "is not in the cache".
    fn require_git(&mut self) -> Result<(), String> {
        if !self.git_found {
            run(git_command().arg("--version"))?;
            self.git_found = true;
        }
        Ok(())
    }

    /// The commit (full hash) that `spec` means in the repository at `url`.
    ///
    /// A `rev` already in the cache is answered from it, as it cannot change. Anything else
    /// (a branch, a tag, the default branch, or a `rev` not seen yet) fetches first, unless
    /// offline: then only what the cache holds can answer.
    pub fn resolve(&mut self, url: &str, spec: &GitSpec) -> Result<String, String> {
        if let Some(problem) = transport_problem(url) {
            return Err(problem);
        }
        if let Some(why) = userinfo_problem(url) {
            return Err(format!("the url `{}`: {why}", redact_url(url)));
        }
        self.require_git()?;
        if matches!(spec, GitSpec::Rev(_))
            && let Some(commit) = self.lookup(url, spec)?
        {
            return Ok(commit);
        }
        let shown = redact_url(url);
        if self.offline {
            return self.lookup(url, spec)?.ok_or_else(|| {
                format!(
                    "cannot resolve {} of `{shown}` offline: it is not in the cache ({})",
                    spec.describe(),
                    self.cache.display()
                )
            });
        }
        self.fetch(url)?;
        self.lookup(url, spec)?.ok_or_else(|| match spec {
            GitSpec::DefaultBranch => format!("the repository `{shown}` has no commits"),
            GitSpec::Rev(rev) => format!("the repository `{shown}` has no commit `{rev}`"),
            GitSpec::Tag(tag) => format!("the repository `{shown}` has no tag `{tag}`"),
            GitSpec::Branch(b) => format!("the repository `{shown}` has no branch `{b}`"),
        })
    }

    /// The commit `spec` names in the local copy, if the copy has it.
    fn lookup(&self, url: &str, spec: &GitSpec) -> Result<Option<String>, String> {
        let db = db_dir(&self.cache, url);
        let db = db.as_path();
        if !looks_like_repo(db) {
            return Ok(None);
        }
        let name = match spec {
            GitSpec::Rev(rev) => return rev_commit(db, url, rev),
            GitSpec::DefaultBranch => "refs/colibri/head".to_string(),
            GitSpec::Tag(tag) => format!("refs/tags/{tag}"),
            GitSpec::Branch(b) => format!("refs/heads/{b}"),
        };
        let out = run(git_command()
            .arg("--git-dir")
            .arg(db)
            .args(["rev-parse", "--verify", "--quiet"])
            .arg(format!("{name}^{{commit}}")));
        Ok(out.ok().filter(|c| c.len() == 40))
    }

    /// Brings the local copy of the repository at `url` up to date: all branches, all tags and
    /// the remote's default branch.
    fn fetch(&mut self, url: &str) -> Result<(), String> {
        if self.fetched.contains(url) {
            return Ok(());
        }
        let _lock = DbLock::take(&self.cache, url)?;
        self.fetch_locked(url)
    }

    /// [`Git::fetch`] for a caller that holds the lock of `url`.
    fn fetch_locked(&mut self, url: &str) -> Result<(), String> {
        if self.fetched.contains(url) {
            return Ok(());
        }
        let db = self.ensure_db(url)?;
        let fetched = run(git_command()
            // A transfer that stalls (under 1000 bytes a second for a minute) fails instead
            // of holding the lock of the repository for ever. Not for `ssh`, which asks for
            // pass phrases: that is the user's to answer.
            .args([
                "-c",
                "http.lowSpeedLimit=1000",
                "-c",
                "http.lowSpeedTime=60",
            ])
            .arg("--git-dir")
            .arg(&db)
            .args([
                "fetch",
                "--quiet",
                "--force",
                "--prune",
                "--no-tags",
                "--no-recurse-submodules",
                "--",
                url,
            ])
            .args([
                "+refs/heads/*:refs/heads/*",
                "+refs/tags/*:refs/tags/*",
                "+HEAD:refs/colibri/head",
            ]));
        if let Err(first) = fetched {
            // A repository without commits has no HEAD to fetch: not an error of the url.
            if first.contains("couldn't find remote ref HEAD") {
                return Err(format!(
                    "the repository `{}` has no commits",
                    redact_url(url)
                ));
            }
            let mut message = format!("cannot fetch `{}`: {first}", redact_url(url));
            if first.contains("too slow") {
                message.push_str(
                    " (the transfer stalled: less than 1000 bytes a second for 60 seconds; check the network and run again)",
                );
            }
            return Err(add_damage_hint(message, &first, &db));
        }
        self.fetched.insert(url.to_string());
        Ok(())
    }

    /// The bare repository for `url`, created if there is none (the lock of `url` is held).
    fn ensure_db(&self, url: &str) -> Result<PathBuf, String> {
        claim_db(&self.cache.join("git").join("db"), &repo_key(url), url)
    }

    /// Whether `path` (at the top of the repository) is a symbolic link at `commit`.
    pub fn is_symlink_at(&self, url: &str, commit: &str, path: &str) -> bool {
        let db = db_dir(&self.cache, url);
        looks_like_repo(&db)
            && run(git_command()
                .arg("--git-dir")
                .arg(&db)
                .args(["ls-tree", commit, "--", path]))
            .is_ok_and(|out| out.starts_with("120000 "))
    }

    /// The folder with the files of `commit`, written from the cache if it is not there yet.
    pub fn checkout(&mut self, url: &str, commit: &str) -> Result<PathBuf, String> {
        if let Some(problem) = transport_problem(url) {
            return Err(problem);
        }
        if let Some(why) = userinfo_problem(url) {
            return Err(format!("the url `{}`: {why}", redact_url(url)));
        }
        let dir = checkout_dir(&self.cache, url, commit);
        if is_complete(&dir) {
            return Ok(dir);
        }
        self.require_git()?;
        let cache = self.cache.clone();
        let have = || {
            let db = db_dir(&cache, url);
            looks_like_repo(&db)
                && run(git_command()
                    .arg("--git-dir")
                    .arg(&db)
                    .args(["cat-file", "-e"])
                    .arg(format!("{commit}^{{commit}}")))
                .is_ok()
        };
        if self.offline && !have() {
            return Err(format!(
                "the commit {commit} of `{}` is not in the cache ({}) and `--offline` was given",
                redact_url(url),
                self.cache.display()
            ));
        }
        let _lock = DbLock::take(&self.cache, url)?;
        // Another process may have written it while this one waited for the lock.
        if is_complete(&dir) {
            return Ok(dir);
        }
        if !have() {
            self.fetch_locked(url)?;
        }
        let db = db_dir(&self.cache, url);
        let parent = dir.parent().unwrap_or(&self.cache).to_path_buf();
        std::fs::create_dir_all(&parent)
            .map_err(|e| format!("cannot create `{}`: {e}", parent.display()))?;
        // A folder without the marker is what an interrupted run left: it is never trusted.
        if dir.exists() {
            remove_tree(&dir)
                .map_err(|e| format!("cannot remove the incomplete `{}`: {e}", dir.display()))?;
        }
        // Written beside the final place, marked complete, and moved into it.
        let tmp = parent.join(format!(".{commit}.tmp{}", std::process::id()));
        let _ = remove_tree(&tmp);
        std::fs::create_dir_all(&tmp)
            .map_err(|e| format!("cannot create `{}`: {e}", tmp.display()))?;
        let written = write_tree(&db, commit, &tmp).and_then(|()| {
            std::fs::write(tmp.join(COMPLETE_MARKER), commit)
                .map_err(|e| format!("cannot write in `{}`: {e}", tmp.display()))
        });
        if let Err(e) = written {
            let _ = remove_tree(&tmp);
            return Err(add_damage_hint(
                format!("cannot check out {commit} of `{}`: {e}", redact_url(url)),
                &e,
                &db,
            ));
        }
        if let Err(e) = std::fs::rename(&tmp, &dir) {
            let _ = remove_tree(&tmp);
            return Err(format!("cannot create `{}`: {e}", dir.display()));
        }
        Ok(dir)
    }
}

/// Whether the checkout folder `dir` was written completely.
pub fn is_complete(dir: &Path) -> bool {
    dir.join(COMPLETE_MARKER).is_file()
}

/// Whether what `git` said looks like a damaged repository (an empty or truncated object, a
/// bad pack) rather than a problem of the url or the network.
fn looks_damaged(said: &str) -> bool {
    let said = said.to_ascii_lowercase();
    [
        "object file",
        "is empty",
        "corrupt",
        "bad object",
        "bad tree object",
        "inflate:",
        "unable to read",
        "unable to unpack",
        "loose object",
        "packfile",
        "index-pack failed",
        "missing blob",
        "missing tree",
        "invalid object",
        "did not receive expected object",
        "not a valid object",
        "not a git repository",
        "fatal: bad",
    ]
    .iter()
    .any(|p| said.contains(p))
}

/// `message`, with the advice to delete the database `db` when `said` (what `git` said) looks
/// like damage to it.
fn add_damage_hint(message: String, said: &str, db: &Path) -> String {
    if looks_damaged(said) {
        format!(
            "{message} (the cached copy may be damaged; delete `{}` and run again)",
            db.display()
        )
    } else {
        message
    }
}

/// The commit that the object name `rev` (hexadecimal, as written in the manifest) means in
/// the repository `db`, if it has one.
///
/// A `rev` is an object name and nothing else: `git rev-parse <rev>` would prefer a branch or
/// tag of the same name (a repository can add a branch called `73bb2bc`), so the objects whose
/// names start with `rev` are listed and the commits among them taken.
fn rev_commit(db: &Path, url: &str, rev: &str) -> Result<Option<String>, String> {
    let rev = rev.to_ascii_lowercase();
    let candidates: Vec<String> = if rev.len() == 40 {
        vec![rev.clone()]
    } else {
        run(git_command()
            .arg("--git-dir")
            .arg(db)
            .arg("rev-parse")
            .arg(format!("--disambiguate={rev}")))?
        .lines()
        .map(str::to_string)
        .collect()
    };
    let commits = candidates
        .into_iter()
        .filter(|id| {
            run(git_command()
                .arg("--git-dir")
                .arg(db)
                .args(["cat-file", "-t"])
                .arg(id))
            .is_ok_and(|kind| kind == "commit")
        })
        .collect();
    pick_commit(&rev, commits).map_err(|e| format!("the repository `{}`: {e}", redact_url(url)))
}

/// The one commit among `commits` (full hashes) whose hash starts with `rev`; `None` if there
/// is none, an error if there are several.
pub(super) fn pick_commit(rev: &str, commits: Vec<String>) -> Result<Option<String>, String> {
    let rev = rev.to_ascii_lowercase();
    let mut matching: Vec<String> = commits
        .into_iter()
        .map(|c| c.to_ascii_lowercase())
        .filter(|c| c.len() == 40 && c.starts_with(&rev))
        .collect();
    matching.sort();
    matching.dedup();
    match matching.len() {
        0 => Ok(None),
        1 => Ok(matching.pop()),
        _ => Err(format!(
            "`{rev}` is ambiguous: it starts the commits {}; write more digits of the hash",
            matching
                .iter()
                .map(|c| format!("`{}`", &c[..12]))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// What a tree entry is, as far as a checkout is concerned.
#[derive(Debug, PartialEq, Eq)]
struct TreeFile {
    oid: String,
    path: String,
    executable: bool,
}

/// Reads the output of `git ls-tree -r -z`: the regular files, in tree order. Symbolic links
/// (mode 120000) and submodules (160000) are not files of the checkout and are left out, and
/// every path that is not safe to write is an error.
fn parse_tree(raw: &[u8]) -> Result<Vec<TreeFile>, String> {
    let mut files = Vec::new();
    for entry in raw.split(|&b| b == 0).filter(|e| !e.is_empty()) {
        let text = std::str::from_utf8(entry)
            .map_err(|_| "the repository has a file whose name is not valid UTF-8".to_string())?;
        let (meta, path) = text
            .split_once('\t')
            .ok_or_else(|| format!("unexpected `git ls-tree` output `{text}`"))?;
        let mut parts = meta.split(' ');
        let (mode, kind, oid) = (parts.next(), parts.next(), parts.next());
        let (Some(mode), Some(kind), Some(oid)) = (mode, kind, oid) else {
            return Err(format!("unexpected `git ls-tree` output `{text}`"));
        };
        // Only regular files: a link would let the repository point the loader at any file of
        // the user's disk, and a submodule is another repository.
        if kind != "blob" || !matches!(mode, "100644" | "100755") {
            continue;
        }
        check_path(path)?;
        files.push(TreeFile {
            oid: oid.to_string(),
            path: path.to_string(),
            executable: mode == "100755",
        });
    }
    Ok(files)
}

/// The name of the file that says a checkout folder was written completely.
pub const COMPLETE_MARKER: &str = ".colibri-complete";

/// Refuses a path of a repository that must not be written below the checkout folder: one
/// that leaves it, one that a file system would read as `.git`, and, on Windows, one that
/// Windows cannot write as it is named.
fn check_path(path: &str) -> Result<(), String> {
    check_path_for(path, cfg!(windows))
}

/// [`check_path`] for a platform: `windows` adds the Windows file name rules (a device name,
/// a `:` in a component, a name ending in a dot or a space). Git checks such files out fine
/// on Linux and macOS, so there they are no reason to refuse a whole dependency; the
/// lookalikes of `.git` and the reserved marker are refused everywhere, since they are a way
/// into the repository's own folder on some file system or the cache's own file.
fn check_path_for(path: &str, windows: bool) -> Result<(), String> {
    const ON_WINDOWS: &str = "the repository cannot be checked out on Windows";
    let bad = |why: &str| {
        Err(format!(
            "the repository has a file `{}` ({why})",
            path.escape_debug()
        ))
    };
    if path.is_empty() {
        return bad("empty path");
    }
    if path.contains('\0') {
        return bad("NUL in its name");
    }
    if path.contains('\\') {
        return bad("a backslash in its name");
    }
    if path.starts_with('/') {
        return bad("an absolute path");
    }
    for (i, component) in path.split('/').enumerate() {
        if component.is_empty() {
            return bad("an empty path component");
        }
        if component == "." || component == ".." {
            return bad(&format!("a `{component}` component"));
        }
        if is_dot_git(component) {
            return bad("a `.git` component, or a name the file system reads as one");
        }
        if windows {
            // A `:` is a drive or a data stream on Windows (`C:x`, `a.txt:stream`).
            if component.contains(':') {
                return bad(&format!("a component that contains `:`; {ON_WINDOWS}"));
            }
            if component.ends_with('.') || component.ends_with(' ') {
                return bad(&format!("a name ending in a dot or a space; {ON_WINDOWS}"));
            }
            if is_device_name(component) {
                return bad(&format!(
                    "a name Windows reserves for a device; {ON_WINDOWS}"
                ));
            }
        }
        if i == 0 && component.eq_ignore_ascii_case(COMPLETE_MARKER) {
            return bad("the name is reserved for the cache");
        }
    }
    Ok(())
}

/// The code points that HFS+ and APFS ignore when they compare names (git's
/// `is_hfs_dotgit`): `.g\u{200c}it` is `.git` there.
fn is_hfs_ignorable(c: char) -> bool {
    matches!(c,
        '\u{200c}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{206a}'..='\u{206f}' | '\u{feff}')
}

/// Whether a file system would take `component` for the `.git` folder: any letter case, the
/// code points HFS+ ignores mixed in, the NTFS short name (`git~1`, `git~2`, ...), and on NTFS
/// trailing dots and spaces or a data stream (`.git.`, `.git `, `.git::$INDEX_ALLOCATION`).
fn is_dot_git(component: &str) -> bool {
    let plain: String = component
        .chars()
        .filter(|c| !is_hfs_ignorable(*c))
        .collect::<String>()
        .to_lowercase();
    let name = plain.split(':').next().unwrap_or("");
    let name = name.trim_end_matches(['.', ' ']);
    name == ".git"
        || name
            .strip_prefix("git~")
            .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
}

/// Whether `component` is a device name on Windows (`con`, `aux.txt`, `COM1`, ...): the name
/// before the first dot decides, and trailing spaces do not count.
fn is_device_name(component: &str) -> bool {
    let stem = component
        .split('.')
        .next()
        .unwrap_or(component)
        .trim_end_matches(' ')
        .to_ascii_lowercase();
    matches!(stem.as_str(), "con" | "prn" | "aux" | "nul")
        || ["com", "lpt"].iter().any(|p| {
            stem.strip_prefix(p)
                .is_some_and(|n| matches!(n.as_bytes(), [b'1'..=b'9']))
        })
}

/// Writes the regular files of `commit` below `dest`, byte for byte as stored.
///
/// This deliberately does not use `git checkout` or `checkout-index`: those apply the
/// repository's `.gitattributes` (running the user's configured filter programs, which may be
/// `git-lfs` talking to a server the repository chose, expanding `$Id$`, changing line
/// endings) and create symbolic links. Here the blobs are copied out as they are.
fn write_tree(db: &Path, commit: &str, dest: &Path) -> Result<(), String> {
    use std::io::{BufRead, BufReader, Read, Write};

    let listing = run_bytes(
        git_command()
            .arg("--git-dir")
            .arg(db)
            .args(["ls-tree", "-r", "-z", "--full-tree"])
            .arg(commit),
    )?;
    let files = parse_tree(&listing)?;
    if files.is_empty() {
        return Ok(());
    }
    let mut child = git_command()
        .arg("--git-dir")
        .arg(db)
        .args(["cat-file", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(spawn_error)?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
    let oids: Vec<String> = files.iter().map(|f| f.oid.clone()).collect();
    // Feeding and reading at once: neither pipe may fill up while the other side waits.
    let feeder = std::thread::spawn(move || {
        for oid in oids {
            if writeln!(stdin, "{oid}").is_err() {
                break;
            }
        }
    });
    let result = (|| {
        for file in &files {
            let mut header = String::new();
            stdout
                .read_line(&mut header)
                .map_err(|e| format!("cannot read from `git cat-file`: {e}"))?;
            let mut parts = header.split_whitespace();
            let size = match (parts.next(), parts.next(), parts.next()) {
                (Some(_), Some("blob"), Some(size)) => size.parse::<u64>().ok(),
                _ => None,
            }
            .ok_or_else(|| {
                format!(
                    "the object of `{}` is not in the repository (`git cat-file`: `{}`)",
                    file.path,
                    header.trim()
                )
            })?;
            let target = dest.join(&file.path);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("cannot create `{}`: {e}", parent.display()))?;
            }
            // `create_new`: a path that is already there (two names that are one on a
            // case-insensitive disk) is an error, never an overwrite.
            let mut out = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&target)
                .map_err(|e| format!("cannot write `{}`: {e}", target.display()))?;
            let copied = std::io::copy(&mut (&mut stdout).take(size), &mut out)
                .map_err(|e| format!("cannot write `{}`: {e}", target.display()))?;
            if copied != size {
                return Err(format!("`git cat-file` ended early in `{}`", file.path));
            }
            drop(out);
            #[cfg(unix)]
            if file.executable {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755))
                    .map_err(|e| format!("cannot write `{}`: {e}", target.display()))?;
            }
            let mut newline = [0u8; 1];
            stdout
                .read_exact(&mut newline)
                .map_err(|e| format!("cannot read from `git cat-file`: {e}"))?;
        }
        Ok(())
    })();
    drop(stdout);
    let _ = feeder.join();
    let _ = child.kill();
    let _ = child.wait();
    result
}

/// The protocols a repository url may use (`GIT_ALLOW_PROTOCOL`, and the manifest reader):
/// `ext::` and `fd::` run programs, and other remote helpers are whatever the user has
/// installed. A plain path is the `file` protocol.
pub(super) const ALLOWED_SCHEMES: [&str; 4] = ["https", "ssh", "git", "file"];

/// Why `url` names a transport that is not allowed, if it does.
pub(super) fn transport_problem(url: &str) -> Option<String> {
    let allowed = || ALLOWED_SCHEMES.map(|s| format!("`{s}`")).join(", ");
    if let Some((scheme, _)) = url.split_once("://") {
        let lower = scheme.to_ascii_lowercase();
        if !ALLOWED_SCHEMES.contains(&lower.as_str()) {
            return Some(format!(
                "the transport `{scheme}` is not allowed in a git url (allowed: {})",
                allowed()
            ));
        }
        return None;
    }
    // `transport::address` hands the address to a remote helper program (`ext::` runs any
    // command, `fd::` reads a file descriptor of the tool).
    if let Some((helper, _)) = url.split_once("::")
        && !helper.is_empty()
        && helper
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'))
    {
        return Some(format!(
            "the remote helper `{helper}::` is not allowed in a git url (allowed transports: {})",
            allowed()
        ));
    }
    None
}

/// The scheme written right before the `://` that ends `head`, lower-cased.
fn scheme_before(head: &str) -> String {
    scheme_before_raw(head).to_ascii_lowercase()
}

/// A plain user name, the only thing allowed before the `@` of an `ssh` url or an scp-like
/// `user@host:path`.
fn is_plain_user(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

fn is_text_break(c: char) -> bool {
    c.is_whitespace() || matches!(c, '\'' | '"' | '`')
}

/// Rewrites what comes before the last `@` of every url in `text`: `shown` gets that text and
/// whether a plain user name may stay (`ssh` urls and the scp form), and returns what to write
/// in its place. A url runs to the next space or quote, so a password with a `/`, `?` or `#` in
/// it is inside the part that is rewritten.
fn rewrite_userinfo(text: &str, shown: &dyn Fn(&str, bool) -> String) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while !rest.is_empty() {
        let skip = rest.find(|c: char| !is_text_break(c)).unwrap_or(rest.len());
        out.push_str(&rest[..skip]);
        rest = &rest[skip..];
        let end = rest.find(is_text_break).unwrap_or(rest.len());
        rewrite_token(&rest[..end], shown, &mut out);
        rest = &rest[end..];
    }
    out
}

fn rewrite_token(token: &str, shown: &dyn Fn(&str, bool) -> String, out: &mut String) {
    let Some(first) = token.find("://") else {
        // The scp form `user@host:path`, from the start of the word.
        if !token.starts_with('/')
            && let Some(at) = token.rfind('@')
            && token[at..].contains(':')
        {
            out.push_str(&shown(&token[..at], true));
            out.push_str(&token[at..]);
        } else {
            out.push_str(token);
        }
        return;
    };
    let mut rest = token;
    let mut scheme_at = first;
    loop {
        let (head, tail) = rest.split_at(scheme_at + 3);
        out.push_str(head);
        let scheme = scheme_before(&head[..scheme_at]);
        // The next url in the same word starts at its scheme.
        let seg_end = match tail.find("://") {
            Some(j) => scheme_start(&tail[..j]),
            None => tail.len(),
        };
        let seg = &tail[..seg_end];
        match seg.rfind('@') {
            Some(at) => {
                out.push_str(&shown(&seg[..at], scheme == "ssh"));
                out.push_str(&seg[at..]);
            }
            None => out.push_str(seg),
        }
        rest = &tail[seg_end..];
        match rest.find("://") {
            Some(i) => scheme_at = i,
            None => {
                out.push_str(rest);
                return;
            }
        }
    }
}

/// Where the scheme written at the end of `head` starts.
fn scheme_start(head: &str) -> usize {
    head.len() - scheme_before_raw(head).len()
}

fn scheme_before_raw(head: &str) -> &str {
    let n = head
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'))
        .map(char::len_utf8)
        .sum::<usize>();
    &head[head.len() - n..]
}

/// `text` with the credentials of every url in it hidden: everything between the scheme (or
/// the start of an scp-like `user@host:path`) and the last `@` is replaced by `***` unless it
/// is a plain user name in an `ssh` url or the scp form (`ssh://git@host`, `git@host:path`).
/// Every message that shows a url, or what `git` said about one, goes through this, so a
/// secret that reaches it some other way (an old `colibri.lock`, the output of `git`) is not
/// printed; a password written with a `/`, `?` or `#` in it is hidden too.
pub fn redact(text: &str) -> String {
    rewrite_userinfo(text, &|userinfo, may_stay| {
        if may_stay && is_plain_user(userinfo) {
            userinfo.to_string()
        } else {
            "***".to_string()
        }
    })
}

/// `text` with each character replaced by a mask character of the same length in UTF-8 and in
/// UTF-16 (`*`, or a stand-in of two, three or four bytes), so that the spans into the text, and
/// the columns an editor computes from them, stay right whatever the credential held.
pub(super) fn mask_str(text: &str) -> String {
    text.chars()
        .map(|c| match c.len_utf8() {
            1 => '*',
            2 => '\u{a4}',
            3 => '\u{2022}',
            _ => '\u{1f512}',
        })
        .collect()
}

/// [`redact`] with every hidden byte replaced by a `*`, so the text keeps its length and the
/// spans into it stay right: what the loader hands to the error printer in place of a
/// manifest or lock file, which show the line an error is on.
pub fn mask_credentials(text: &str) -> String {
    rewrite_userinfo(text, &|userinfo, may_stay| {
        if may_stay && is_plain_user(userinfo) {
            userinfo.to_string()
        } else {
            mask_str(userinfo)
        }
    })
}

/// Where an `@` is written in `line`, as the byte range of the last one: a plain `@`, or a TOML
/// escape for it (`\u0040`, `\U00000040`, `\x40`), which a parser may reject and a reader
/// would still see as the end of the user information.
fn last_at_like(line: &str) -> Option<(usize, usize)> {
    let b = line.as_bytes();
    let mut last = None;
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'@' {
            last = Some((i, i + 1));
        } else if b[i] == b'\\' {
            // Compared in place: a copy of the rest of the line per backslash would make a
            // long line cost the square of its length.
            let at = |s: &[u8]| {
                b.get(i + 1..i + 1 + s.len())
                    .is_some_and(|x| x.eq_ignore_ascii_case(s))
            };
            let len = if at(b"u0040") {
                Some(6)
            } else if at(b"u00000040") {
                Some(10)
            } else if at(b"x40") {
                Some(4)
            } else {
                None
            };
            if let Some(len) = len {
                last = Some((i, i + len));
            }
        }
        i += 1;
    }
    last
}

/// The bytes of `line` that hold what stands before its last `@`: from the start of the first
/// url scheme (`xxx://`) before it, or from the opening quote of the value when there is no
/// scheme. Quotes, spaces and tabs in between are ignored, so no password can end it early.
fn credential_range(line: &str, bare_url: bool) -> Option<(usize, usize)> {
    let (at, _) = last_at_like(line)?;
    let start = match line[..at].find("://") {
        Some(i) => scheme_start(&line[..i]),
        None if bare_url => 0,
        None => {
            let from = line.find('=').map_or(0, |e| e + 1);
            line[from..]
                .find(['"', '\''])
                .map_or_else(|| from, |q| from + q)
        }
    };
    (start < at).then_some((start, at))
}

/// `line` with everything before its last `@` hidden by `*`, if it has one.
fn mask_line_before_at(line: &str) -> String {
    match credential_range(line, false) {
        Some((start, end)) => format!(
            "{}{}{}",
            &line[..start],
            mask_str(&line[start..end]),
            &line[end..]
        ),
        None => line.to_string(),
    }
}

/// `line` with the comment at its end (outside any string) masked like an unparsed line.
fn mask_comment(line: &str) -> String {
    let mut quote = None;
    let mut escaped = false;
    for (i, c) in line.char_indices() {
        match quote {
            Some(q) => {
                if escaped {
                    escaped = false;
                } else if c == '\\' && q == '"' {
                    escaped = true;
                } else if c == q {
                    quote = None;
                }
            }
            None if c == '"' || c == '\'' => quote = Some(c),
            None if c == '#' => {
                return format!("{}{}", &line[..i], mask_line_before_at(&line[i..]));
            }
            None => {}
        }
    }
    line.to_string()
}

/// `url` as it may be shown: when it has credentials in it (or anything else wrong before the
/// `@`), everything between the scheme and the last `@` becomes `***`, whatever characters
/// the password has. A url without a problem goes through [`redact`] as usual.
pub fn redact_url(url: &str) -> String {
    if userinfo_problem(url).is_none() {
        return redact(url);
    }
    match credential_range(url, true) {
        Some((start, end)) => {
            // Keep the scheme (`https://***@host`).
            let start = url[start..end].find("://").map_or(start, |i| start + i + 3);
            format!("{}***{}", &url[..start], &url[end..])
        }
        None => redact(url),
    }
}

/// The url values (`git` in a manifest, `url` in a lock file) of `tables`, as the spans of
/// their literals, when the value has a problem before its `@`.
fn spans_with_credentials(entries: &[super::toml::Entry], out: &mut Vec<(usize, usize)>) {
    for e in entries {
        value_spans(&e.value, matches!(e.key.as_str(), "git" | "url"), out);
    }
}

/// [`spans_with_credentials`] for one value. `url` is whether the value stands under a `git` or
/// `url` key: a wrong value there (`git = ["https://u:pass@h/r"]`, `git = { u = "..." }`) is
/// quoted in the error about its type, so every string in it is masked, at any depth.
fn value_spans(value: &super::toml::Value, url: bool, out: &mut Vec<(usize, usize)>) {
    use super::toml::ValueKind;
    match &value.kind {
        ValueKind::Str(s) if url => {
            if userinfo_problem(s).is_some() {
                out.push((value.span.start as usize, value.span.end as usize));
            }
        }
        ValueKind::Array(items) => {
            for item in items {
                value_spans(item, url, out);
            }
        }
        ValueKind::Table(inner) => {
            for e in inner {
                value_spans(
                    &e.value,
                    url || matches!(e.key.as_str(), "git" | "url"),
                    out,
                );
            }
        }
        _ => {}
    }
}

/// `text` (a `colibri.toml` or `colibri.lock`) as it may be put in the source map, and so in
/// any message that quotes a line of it: the same length, with the credentials hidden by `*`.
///
/// When the text parses, the whole literal of every `git` or `url` value that has credentials
/// in it is overwritten, by its span, whatever the password holds. When it does not parse
/// (it is still quoted in the error), every line with an `@` in it (or an escape for one) is
/// masked from the url scheme, or the opening quote of the value, to the last `@`, ignoring
/// quotes and spaces in between: it may hide too much, and never shows a secret.
pub fn mask_source_text(text: &str) -> String {
    use colibri_syntax::FileId;
    let masked = match super::toml::parse(FileId(0), text) {
        Ok(tables) => {
            let mut spans = Vec::new();
            for t in &tables {
                spans_with_credentials(&t.entries, &mut spans);
            }
            let mut masked = String::with_capacity(text.len());
            let mut done = 0;
            spans.sort_unstable();
            for (s, e) in spans {
                let e = e.min(text.len());
                if s < done || !text.is_char_boundary(s) || !text.is_char_boundary(e) {
                    continue;
                }
                masked.push_str(&text[done..s]);
                masked.push_str(&mask_str(&text[s..e]));
                done = e;
            }
            masked.push_str(&text[done..]);
            // A comment may hold a url too.
            masked
                .split_inclusive('\n')
                .map(mask_comment)
                .collect::<String>()
        }
        Err(_) => text
            .split_inclusive('\n')
            .map(mask_line_before_at)
            .collect(),
    };
    mask_credentials(&masked)
}

/// What is wrong with the user information of `url`, if anything, without repeating it. An
/// `http`, `https`, `git` or `file` url may have no `@` at all (what stands before it would be
/// written to `colibri.lock` and printed in messages; a git credential helper keeps it out of
/// both). An `ssh` url or the scp form `user@host:path` may have exactly one `@`, with a plain
/// user name (`[A-Za-z0-9._-]+`) before it.
pub(super) fn userinfo_problem(url: &str) -> Option<&'static str> {
    const NO_CREDENTIALS: &str =
        "credentials and `@` are not allowed in this url; use a git credential helper";
    const ONLY_USER: &str = "only a plain user name (letters, digits, `.`, `_`, `-`) and one `@` are allowed before the host; use a git credential helper or an ssh key for anything else";
    if !url.contains('@') {
        return None;
    }
    let ssh_form = |rest: &str| {
        let (user, _) = rest.split_once('@')?;
        let ok = is_plain_user(user) && rest.matches('@').count() == 1;
        (!ok).then_some(ONLY_USER)
    };
    if let Some((scheme, rest)) = url.split_once("://") {
        return if scheme.eq_ignore_ascii_case("ssh") {
            ssh_form(rest)
        } else {
            Some(NO_CREDENTIALS)
        };
    }
    if super::manifest::is_absolute_path(url) {
        return None;
    }
    ssh_form(url)
}

/// `url` with the scheme written in lower case (`HTTPS://x` is `https://x`): git refuses an
/// upper-case transport.
pub(super) fn lower_scheme(url: &str) -> String {
    match url.split_once("://") {
        Some((scheme, rest)) => format!("{}://{rest}", scheme.to_ascii_lowercase()),
        None => url.to_string(),
    }
}

/// A `git` command with the tool's settings: no prompt, English messages, only the allowed
/// transports, and nothing inherited that could send git to another repository (the tool may
/// be started by a git hook, which sets `GIT_DIR` and friends for its own repository).
fn git_command() -> Command {
    let mut cmd = Command::new("git");
    for var in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_NAMESPACE",
        "GIT_CEILING_DIRECTORIES",
        "GIT_COMMON_DIR",
        "GIT_PREFIX",
        "GIT_QUARANTINE_PATH",
    ] {
        cmd.env_remove(var);
    }
    cmd.env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .env("GIT_ALLOW_PROTOCOL", ALLOWED_SCHEMES.join(":"))
        // The user's other settings stay (credential helpers, `url.<base>.insteadOf`, ssh):
        // these are the ones that would make a fetch into the cache do something else.
        .args(["-c", "gc.auto=0", "-c", "maintenance.auto=false"])
        .args([
            "-c",
            if cfg!(windows) {
                "core.hooksPath=NUL"
            } else {
                "core.hooksPath=/dev/null"
            },
        ]);
    cmd
}

fn spawn_error(e: std::io::Error) -> String {
    if e.kind() == std::io::ErrorKind::NotFound {
        "the `git` program was not found: install git (and put it on the PATH) to use git dependencies"
            .to_string()
    } else {
        format!("cannot run `git`: {e}")
    }
}

/// Runs a command with a prompt-free, English-speaking `git`; its trimmed stdout, or what it
/// said on stderr.
fn run(cmd: &mut Command) -> Result<String, String> {
    run_bytes(cmd).map(|out| String::from_utf8_lossy(&out).trim().to_string())
}

/// [`run`], with the output as it is.
fn run_bytes(cmd: &mut Command) -> Result<Vec<u8>, String> {
    let output = cmd.stdin(Stdio::null()).output().map_err(spawn_error)?;
    if output.status.success() {
        return Ok(output.stdout);
    }
    Err(redact(
        &stderr_tail(&String::from_utf8_lossy(&output.stderr))
            .unwrap_or_else(|| format!("git exited with {}", output.status)),
    ))
}

/// How many lines of git's stderr an error message keeps, and how long each may be.
const STDERR_LINES: usize = 4;
const STDERR_LINE_CHARS: usize = 300;

/// The last few lines of what a command said, joined with `; `: git can be very chatty (a
/// failed fetch of a big repository prints a line for every ref), and the end is where the
/// reason is.
fn stderr_tail(stderr: &str) -> Option<String> {
    let lines: Vec<&str> = stderr
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if lines.is_empty() {
        return None;
    }
    let skipped = lines.len().saturating_sub(STDERR_LINES);
    let mut out: Vec<String> = Vec::new();
    if skipped > 0 {
        out.push(format!("({skipped} earlier lines left out)"));
    }
    for line in &lines[skipped..] {
        out.push(match line.char_indices().nth(STDERR_LINE_CHARS) {
            Some((end, _)) => format!("{}...", &line[..end]),
            None => (*line).to_string(),
        });
    }
    Some(out.join("; "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_repository_folder_is_named_after_the_repository_and_the_whole_url() {
        let a = repo_key("https://example.com/me/physics.git");
        assert!(a.starts_with("physics-"), "{a}");
        assert_eq!(a, repo_key("https://example.com/me/physics.git"));
        assert_ne!(a, repo_key("https://example.org/me/physics.git"));
        assert_eq!(repo_key("git@host:me/p.git").split('-').next(), Some("p"));
        assert!(repo_key("/").starts_with("repo-"));
        // Fixed forever: a changed hash would orphan every user's cache.
        assert_eq!(repo_key("x"), "x-af63f54c86021707");
    }

    #[test]
    fn checkouts_are_keyed_by_url_and_commit() {
        let c = checkout_dir(Path::new("/c"), "file:///r/p", "abc");
        assert!(c.starts_with("/c/git/checkouts"));
        assert!(c.ends_with("abc"));
    }

    #[test]
    fn only_regular_files_are_taken_from_a_tree() {
        let raw = b"100644 blob aaaa\ta.colibri\x00100755 blob bbbb\tbin/run\x00120000 blob cccc\tlink.colibri\x00160000 commit dddd\tsub\x00";
        let files = parse_tree(raw).unwrap();
        let names: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(names, ["a.colibri", "bin/run"]);
        assert!(!files[0].executable && files[1].executable);
    }

    #[test]
    fn paths_that_could_leave_the_checkout_are_refused() {
        for windows in [false, true] {
            for bad in [
                "/etc/passwd",
                "../x",
                "a/../x",
                "a/./x",
                ".git/config",
                "a/.GIT/x",
                "a//b",
                "a/",
                "a\\b",
                ".colibri-complete",
                ".git.",
                ".git ",
                ".git. . ",
                "a/.git./x",
                ".git:stream",
                ".git::$INDEX_ALLOCATION",
                "git~1.",
                "GIT~2 /x",
            ] {
                assert!(
                    check_path_for(bad, windows).is_err(),
                    "{bad:?} (windows: {windows}) should be refused"
                );
            }
            let reason = |p: &str| check_path_for(p, windows).unwrap_err();
            for hidden in [
                ".g\u{200c}it/x",
                ".\u{200d}git",
                "a/.git\u{200e}",
                "\u{202a}.gi\u{202e}t/config",
                "\u{206a}.git\u{206f}",
                ".git\u{feff}",
                ".GiT",
                ".git.",
                ".git::$INDEX_ALLOCATION",
            ] {
                assert!(reason(hidden).contains(".git"), "{hidden:?}");
            }
            for short in ["git~1", "GIT~2/x", "a/git~12", "git~9999", "git~1 "] {
                assert!(reason(short).contains(".git"), "{short:?}");
            }
            for marker in [
                ".Colibri-Complete",
                ".COLIBRI-COMPLETE",
                ".colibri-complete",
            ] {
                assert!(reason(marker).contains("reserved"), "{marker:?}");
            }
            for good in [
                "a.colibri",
                "src/b.colibri",
                ".hidden/x",
                "x.git/y",
                "a b/c",
                "git~x",
                "git~",
                "console",
                "com10",
                "com0",
                "lpt",
                "auxiliary.colibri",
                "sub/.colibri-complete",
                "gitx",
            ] {
                assert!(
                    check_path_for(good, windows).is_ok(),
                    "{good:?} (windows: {windows}) should be fine"
                );
            }
        }
        let raw = b"100644 blob aaaa\t../x\0";
        assert!(parse_tree(raw).unwrap_err().contains("`..`"));
    }

    #[test]
    fn windows_file_name_rules_apply_only_on_windows() {
        let only_windows = [
            "docs/aux.md",
            "src/con.c",
            "notes/2021-01-01T10:00.md",
            "a.b.",
            "trailing.",
            "dir /x",
            "C:/x",
            "a:b",
            "x/C:/y",
            "con",
            "CON",
            "prn",
            "aux",
            "nul",
            "com1",
            "COM9",
            "lpt1",
            "LPT9",
            "con.txt",
            "a/aux.colibri",
            "nul.tar.gz",
            "Com5.x",
            "con ",
            "a.",
            "a ",
            "d/b. ",
            "d/b..",
        ];
        for p in only_windows {
            assert!(
                check_path_for(p, false).is_ok(),
                "{p:?} is fine off Windows"
            );
            let why = check_path_for(p, true).unwrap_err();
            assert!(
                why.contains(p) && why.contains("cannot be checked out on Windows"),
                "{p:?}: {why}"
            );
        }
        let reason = |p: &str| check_path_for(p, true).unwrap_err();
        assert!(reason("a:b").contains("contains `:`"));
        assert!(reason("con").contains("device"));
        for end in ["a.", "a ", "d/b. ", "d/b.."] {
            assert!(reason(end).contains("dot or a space"), "{end:?}");
        }
        // The platform the tool runs on decides for `check_path`.
        assert_eq!(check_path("docs/aux.md").is_err(), cfg!(windows));
    }

    #[test]
    fn credentials_in_a_url_are_hidden_in_any_text() {
        assert_eq!(
            redact("cannot fetch `https://user:s3cret@host/r.git`: x"),
            "cannot fetch `https://***@host/r.git`: x"
        );
        assert_eq!(redact("https://tok@h/r"), "https://***@h/r");
        assert_eq!(redact("ssh://git@h/r"), "ssh://git@h/r");
        assert_eq!(redact("ssh://git:pw@h/r"), "ssh://***@h/r");
        assert_eq!(redact("git@host:me/r.git"), "git@host:me/r.git");
        assert_eq!(redact("no url here"), "no url here");
        assert_eq!(
            redact("a https://u:p@h/x and ftp://v:q@k"),
            "a https://***@h/x and ftp://***@k"
        );
        assert!(!redact("fatal: unable to access 'HTTPS://u:s3cret@h/r.git/'").contains("s3cret"));
    }

    #[test]
    fn every_kind_of_password_is_hidden() {
        for secret in [
            "u:p#s3cret@",
            "u:p?s3cret@",
            "u:pa/s3cret@",
            "u:p@s3cret@",
            "u:p%40s3cret@",
        ] {
            for scheme in ["https", "HTTPS", "http", "git", "file", "ssh", "Ssh"] {
                let text = format!("cannot fetch {scheme}://{secret}host:9/r.git: x");
                assert!(!redact(&text).contains("s3cret"), "{text}");
                assert!(!mask_credentials(&text).contains("s3cret"), "{text}");
                assert_eq!(mask_credentials(&text).len(), text.len());
                let line = format!("g = {{ git = \"{scheme}://{secret}host/r.git\" }}");
                assert!(!redact(&line).contains("s3cret"), "{line}");
            }
        }
        for scp in [
            "u:s3cret@host:r.git",
            "u:p@s3cret@host:r.git",
            "a/s3cret@host:r",
        ] {
            let text = format!("`{scp}` is not a git url");
            assert!(!redact(&text).contains("s3cret"), "{text}");
            assert!(!mask_credentials(&text).contains("s3cret"), "{text}");
        }
        assert_eq!(
            redact("fatal: could not read from git@github.com: no"),
            "fatal: could not read from git@github.com: no"
        );
        assert_eq!(
            redact("ssh://git@h/r https://a:b@c"),
            "ssh://git@h/r https://***@c"
        );
    }

    #[test]
    fn masked_text_keeps_its_length() {
        for text in [
            "g = { git = \"https://user:s3cret@host/r.git\" }",
            "ssh://git:pw@h/x",
            "ssh://git@h/x",
            "https://tökén@h/x and https://a@b",
        ] {
            let masked = mask_credentials(text);
            assert_eq!(masked.len(), text.len(), "{masked}");
            assert!(
                !masked.contains("s3cret") && !masked.contains("pw@"),
                "{masked}"
            );
        }
        assert_eq!(mask_credentials("ssh://git:pw@h/x"), "ssh://******@h/x");
        assert_eq!(mask_credentials("ssh://git@h/x"), "ssh://git@h/x");
    }

    #[test]
    fn source_text_hides_a_url_with_credentials_whatever_the_password_holds() {
        for lit in [
            "\"https://u:ab'cds3cret@h/r.git\"",
            "\"https://u:x\\\"ys3cret@h/r.git\"",
            "\"https://u:pa ss3cret@h/r.git\"",
            "'https://u:p\"qs3cret@h:99/r.git'",
            "\"https://u:pa\tss3cret@h/r.git\"",
            "\"https://u:p@s3cret@h/r.git\"",
            "\"https://u:ab`s3cret@h/r.git\"",
            "\"u:a'bs3cret@h:r.git\"",
        ] {
            // Parsed: the literal is overwritten by its span.
            let text = format!("[dependencies]\ng = {{ git = {lit} }}\nh = true\n");
            let masked = mask_source_text(&text);
            assert_eq!(masked.len(), text.len(), "{lit}");
            assert!(!masked.contains("s3cret"), "{lit}: {masked}");
            assert!(masked.contains("g = { git = ***"), "{lit}: {masked}");
            let lock = format!("[git.g]\nurl = {lit}\ncommit = \"abc\"\n");
            assert!(!mask_source_text(&lock).contains("s3cret"), "{lit}");
            // Not parsed: every line with an `@` is masked up to the last one.
            for broken in [
                format!("[dependencies]\ng = {{ git = {lit} \n"),
                format!("url = {lit} junk\n"),
                format!("# {lit}\n"),
            ] {
                let masked = mask_source_text(&broken);
                assert_eq!(masked.len(), broken.len(), "{broken}");
                assert!(!masked.contains("s3cret"), "{broken}: {masked}");
            }
        }
        for broken in [
            "g = { git = \"https://u:s3cret\\u0040h/r.git\" }\n",
            "g = { git = \"https://u:s3cret\\U00000040h/r.git\" }\n",
            "g = { git = \"https://u:s3cret\\x40h/r.git\" }\n",
        ] {
            let masked = mask_source_text(broken);
            assert_eq!(masked.len(), broken.len());
            assert!(!masked.contains("s3cret"), "{masked}");
        }
    }

    #[test]
    fn a_wrong_value_of_a_git_key_is_masked_at_any_depth() {
        for text in [
            "[dependencies]\nj = { git = [\"https://u:pa ss3cret@h/r\"] }\n",
            "[dependencies]\nj = { git = { u = \"https://u:pa ss3cret@h/r\" } }\n",
            "[dependencies]\nj = { git = [[\"x\"], [\"https://u:pa ss3cret@h/r\"]] }\n",
            "[git.j]\nurl = [\"https://u:pa ss3cret@h/r\"]\ncommit = \"abc\"\n",
        ] {
            let masked = mask_source_text(text);
            assert_eq!(masked.len(), text.len(), "{text}");
            assert!(!masked.contains("s3cret"), "{text}: {masked}");
        }
        // A string that is not under a url key is not touched.
        let other = "[package]\nname = \"x\"\nnote = \"me@home\"\n";
        assert_eq!(mask_source_text(other), other);
    }

    #[test]
    fn masks_have_the_length_of_what_they_hide_in_utf8_and_utf16() {
        for text in ["abc", "éé", "日本", "😀😀", "a é 日 😀"] {
            let masked = mask_str(text);
            assert_eq!(masked.len(), text.len(), "{text}");
            assert_eq!(
                masked.encode_utf16().count(),
                text.encode_utf16().count(),
                "{text}"
            );
            assert_eq!(masked.chars().count(), text.chars().count());
            assert!(!masked.contains(|c: char| text.contains(c) && c != '*'));
            // Masking what is masked changes nothing.
            assert_eq!(mask_str(&masked), masked);
        }
        let text = "[dependencies]\nj = { git = \"https://éé😀:p@h/r\" }\n";
        let masked = mask_source_text(text);
        assert_eq!(masked.len(), text.len());
        assert_eq!(masked.encode_utf16().count(), text.encode_utf16().count());
        assert!(!masked.contains("éé"), "{masked}");
    }

    #[test]
    fn source_text_without_credentials_is_left_alone() {
        let text = "[dependencies]\na = { git = \"ssh://git@host/r.git\" }\nb = { git = \"git@host:r.git\" }\nc = { git = \"https://host/r.git\" }\n";
        assert_eq!(mask_source_text(text), text);
        assert_eq!(redact_url("https://u:pa ss3cret@h/x"), "https://***@h/x");
        assert_eq!(redact_url("ssh://git@h/x"), "ssh://git@h/x");
        assert_eq!(redact_url("u:a'bs3cret@h:r"), "***@h:r");
    }

    #[test]
    fn user_information_is_judged_by_scheme() {
        for bad in [
            "https://user@host/r.git",
            "https://user:pw@host/r.git",
            "HTTPS://user:pw@host/r.git",
            "http://user@host/r.git",
            "ssh://git:pw@host/r.git",
            "git://u:p@host/r.git",
            "git://user@host/r.git",
            "file://user@host/r.git",
            "https://u:p#s3cret@host/r.git",
            "https://u:p?s3cret@host/r.git",
            "https://u:pa/ss@host/r.git",
            "https://u:p@ss@host/r.git",
            "https://u:p%40ss@host/r.git",
            "https://host/r@v1.git",
            "ssh://git@host/r@v1.git",
            "ssh://u:p@ss@host/r.git",
            "ssh://a/b@host/r.git",
            "u:s3cret@host:r.git",
            "u:p@ss@host:r.git",
            "a/b@host:r.git",
        ] {
            assert!(userinfo_problem(bad).is_some(), "{bad}");
        }
        for good in [
            "https://host/r.git",
            "ssh://git@host/r.git",
            "git@host:me/r.git",
            "git://host/r.git",
            "file:///tmp/r",
            "/tmp/r",
            "/tmp/r@x",
            "ssh://a.b_c-d@host/r.git",
        ] {
            assert!(userinfo_problem(good).is_none(), "{good}");
        }
    }

    #[test]
    fn masking_a_very_long_line_of_backslashes_is_linear() {
        // A comment of a million backslashes and an `@`: each backslash used to copy the rest
        // of the line. The bound is generous; the quadratic scan took minutes.
        let line = format!("# {}@x\n", "\\".repeat(1_000_000));
        let text = format!("[package]\nname = \"x\"\n{line}");
        let start = std::time::Instant::now();
        let masked = mask_source_text(&text);
        assert!(start.elapsed().as_secs() < 30, "{:?}", start.elapsed());
        assert_eq!(masked.len(), text.len());
        assert!(
            masked.starts_with("[package]\nname = \"x\"\n"),
            "{}",
            &masked[..30]
        );
        // And the same line where the text does not parse.
        let broken = format!("= = {line}");
        let start = std::time::Instant::now();
        let masked = mask_source_text(&broken);
        assert!(start.elapsed().as_secs() < 30, "{:?}", start.elapsed());
        assert_eq!(masked.len(), broken.len());
    }

    #[test]
    fn the_scheme_is_written_in_lower_case() {
        assert_eq!(lower_scheme("HTTPS://Host/Repo"), "https://Host/Repo");
        assert_eq!(lower_scheme("File:///X"), "file:///X");
        assert_eq!(lower_scheme("git@host:me/R.git"), "git@host:me/R.git");
        assert_eq!(lower_scheme("/tmp/R"), "/tmp/R");
    }

    #[test]
    fn a_lock_that_is_held_is_waited_for_and_said_so() {
        let cache = std::env::temp_dir().join(format!("colibri-lock-wait-{}", std::process::id()));
        let _ = remove_tree(&cache);
        let url = "https://example.com/me/wait.git";
        let mut said = Vec::new();
        // Free: no message.
        let held = DbLock::take_telling(&cache, url, &mut |l| said.push(l.to_string())).unwrap();
        assert!(said.is_empty());
        let waiter = {
            let cache = cache.clone();
            std::thread::spawn(move || {
                let mut said = Vec::new();
                let lock = DbLock::take_telling(&cache, url, &mut |l| said.push(l.to_string()));
                (lock.is_ok(), said)
            })
        };
        std::thread::sleep(std::time::Duration::from_millis(300));
        drop(held);
        let (got, said) = waiter.join().unwrap();
        assert!(got);
        assert_eq!(said.len(), 1, "{said:?}");
        assert!(
            said[0].starts_with("waiting for another colibri process to finish with `wait-"),
            "{}",
            said[0]
        );
        assert!(said[0].contains(".lock"), "{}", said[0]);
        let _ = remove_tree(&cache);
    }

    #[test]
    fn what_looks_like_a_damaged_repository_says_where_to_delete() {
        let db = Path::new("/c/git/db/x-1.git");
        for said in [
            "error: object file /c/x/objects/ab/cd is empty; fatal: loose object abcd is corrupt",
            "fatal: bad object HEAD",
            "error: inflate: data stream error",
        ] {
            let m = add_damage_hint("cannot fetch `u`: x".into(), said, db);
            assert!(
                m.ends_with(
                    "(the cached copy may be damaged; delete `/c/git/db/x-1.git` and run again)"
                ),
                "{m}"
            );
        }
        let m = add_damage_hint("cannot fetch `u`: x".into(), "could not resolve host", db);
        assert_eq!(m, "cannot fetch `u`: x");
    }

    #[test]
    fn a_rev_names_one_commit_by_its_hash_prefix() {
        let a = "73bb2bca46000000000000000000000000000000".to_string();
        let b = "73bb2bc999000000000000000000000000000000".to_string();
        let c = "0123456789abcdef0123456789abcdef01234567".to_string();
        assert_eq!(
            pick_commit("73BB2BCA", vec![a.clone(), c.clone()]),
            Ok(Some(a.clone()))
        );
        assert_eq!(pick_commit("73bb2bc", vec![c.clone()]), Ok(None));
        // A commit that merely exists is not the one: it must start with the rev.
        assert_eq!(pick_commit("73bb2bc", vec![]), Ok(None));
        let e = pick_commit("73bb2bc", vec![a, b, c]).unwrap_err();
        assert!(
            e.contains("is ambiguous") && e.contains("73bb2bca4600"),
            "{e}"
        );
    }

    #[test]
    fn a_long_stderr_is_cut_to_its_last_lines() {
        let many: String = (0..2000).map(|i| format!("ref {i} rejected\n")).collect();
        let tail = stderr_tail(&format!("{many}fatal: the reason\n")).unwrap();
        assert!(tail.len() < 400, "{} bytes", tail.len());
        assert!(tail.starts_with("(1997 earlier lines left out)"), "{tail}");
        assert!(tail.ends_with("fatal: the reason"), "{tail}");
        let long = "x".repeat(5000);
        assert!(stderr_tail(&long).unwrap().len() < 400);
        assert_eq!(stderr_tail("\n  \n"), None);
        assert_eq!(stderr_tail("a\nb\n").as_deref(), Some("a; b"));
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("colibri-gitrs-{}-{name}", std::process::id()));
        let _ = remove_tree(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Sets the modification time of a folder (Windows opens a folder only with backup
    /// semantics, and for writing only the attributes).
    fn set_modified(dir: &Path, time: std::time::SystemTime) {
        #[cfg(windows)]
        let file = {
            use std::os::windows::fs::OpenOptionsExt;
            const FILE_WRITE_ATTRIBUTES: u32 = 0x0100;
            const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
            std::fs::OpenOptions::new()
                .access_mode(FILE_WRITE_ATTRIBUTES)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
                .open(dir)
                .unwrap()
        };
        #[cfg(not(windows))]
        let file = std::fs::File::open(dir).unwrap();
        file.set_modified(time).unwrap();
    }

    #[test]
    fn old_temporary_folders_are_removed_and_new_ones_kept() {
        let dir = scratch("stale");
        let old = dir.join(".abc.tmp1");
        let new = dir.join(".def.tmp2");
        let real = dir.join("abc");
        for d in [&old, &new, &real] {
            std::fs::create_dir_all(d).unwrap();
        }
        let two_days = std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 86400);
        set_modified(&old, two_days);
        set_modified(&real, two_days);
        remove_stale_tmp(&dir);
        assert!(!old.exists() && new.exists() && real.exists());
    }

    #[test]
    fn an_empty_folder_is_not_a_repository_and_is_made_again() {
        let dir = scratch("emptydb");
        let git = Git::new(dir.clone(), false);
        let url = "/nowhere";
        let db = db_dir(&dir, url);
        std::fs::create_dir_all(&db).unwrap();
        assert!(!is_valid_repo(&db));
        let _lock = DbLock::take(&dir, url).unwrap();
        let made = git.ensure_db(url).unwrap();
        assert_eq!(made, db);
        assert!(is_valid_repo(&db));
        // A second call keeps it.
        std::fs::write(db.join("marker"), "x").unwrap();
        git.ensure_db(url).unwrap();
        assert!(db.join("marker").exists());
    }

    #[test]
    fn only_the_allowed_transports_are_accepted() {
        for ok in [
            "https://h/r.git",
            "HTTPS://h/r",
            "ssh://h/r",
            "git://h/r",
            "file:///r",
            "/abs/path",
            "user@host:r.git",
        ] {
            assert_eq!(transport_problem(ok), None, "{ok}");
        }
        for bad in [
            "ext::sh -c whoami",
            "fd::17",
            "http://h/r",
            "ftp://h/r",
            "hg::https://x",
            "ext::",
        ] {
            assert!(transport_problem(bad).is_some(), "{bad}");
        }
        assert!(
            transport_problem("ext::x")
                .unwrap()
                .contains("remote helper")
        );
    }

    #[test]
    fn git_refuses_a_remote_helper_before_running_anything() {
        let dir = scratch("helper");
        let mut git = Git::new(dir.clone(), false);
        for url in ["ext::sh -c touch$IFS/tmp/x", "fd::3"] {
            let e = git.resolve(url, &GitSpec::DefaultBranch).unwrap_err();
            assert!(e.contains("not allowed"), "{e}");
            let e = git.checkout(url, &"a".repeat(40)).unwrap_err();
            assert!(e.contains("not allowed"), "{e}");
        }
        assert!(!dir.join("git").exists());
    }

    #[test]
    fn two_urls_with_one_key_get_two_databases() {
        let dbs = scratch("collide");
        let a = claim_db(&dbs, "same-key", "https://a.example/r.git").unwrap();
        let b = claim_db(&dbs, "same-key", "https://b.example/r.git").unwrap();
        let c = claim_db(&dbs, "same-key", "https://c.example/r.git").unwrap();
        assert_eq!(a, dbs.join("same-key.git"));
        assert_eq!(b, dbs.join("same-key-1.git"));
        assert_eq!(c, dbs.join("same-key-2.git"));
        // Each is found again, and nothing is shared or overwritten.
        std::fs::write(a.join("marker"), "a").unwrap();
        assert_eq!(
            claim_db(&dbs, "same-key", "https://b.example/r.git").unwrap(),
            b
        );
        assert_eq!(
            claim_db(&dbs, "same-key", "https://a.example/r.git").unwrap(),
            a
        );
        assert!(a.join("marker").exists() && !b.join("marker").exists());
        match find_slot(&dbs, "same-key", "https://c.example/r.git") {
            Slot::Ours(p) => assert_eq!(p, c),
            Slot::Free(p) => panic!("not found: {}", p.display()),
        }
        match find_slot(&dbs, "same-key", "https://d.example/r.git") {
            Slot::Free(p) => assert_eq!(p, dbs.join("same-key-3.git")),
            Slot::Ours(p) => panic!("wrong match: {}", p.display()),
        }
        // A damaged first folder is made again, the others stay.
        std::fs::remove_file(a.join(URL_FILE)).unwrap();
        let again = claim_db(&dbs, "same-key", "https://a.example/r.git").unwrap();
        assert_eq!(again, a);
        assert!(!a.join("marker").exists());
        assert_eq!(
            std::fs::read_to_string(a.join(URL_FILE)).unwrap(),
            "https://a.example/r.git"
        );
    }
}
