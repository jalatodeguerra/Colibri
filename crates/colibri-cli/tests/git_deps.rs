//! Git dependencies and `colibri.lock`, end to end: the repositories are made here, in
//! temporary folders, and used by path or `file://` url, so nothing needs the network. A
//! missing `git` program fails these tests loudly (they never skip).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// What a `colibri` run left.
struct Out {
    stdout: String,
    stderr: String,
    code: i32,
}

/// A fresh folder with a private `COLIBRI_HOME` and room for repositories and apps.
struct Sandbox {
    root: PathBuf,
}

/// The file that reads as empty: git is told to use it for its global settings.
const NULL_DEVICE: &str = if cfg!(windows) { "NUL" } else { "/dev/null" };

/// `path` written with `/` on every system: Windows takes it as it takes `\`, it is valid
/// inside a TOML basic string and a Colibri string (`\` is not), and it is what a git url
/// or a manifest path should say. Never paste `path.display()` into a manifest.
fn slashes(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn git_env(cmd: &mut Command) -> &mut Command {
    cmd.env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .env("GIT_CONFIG_GLOBAL", NULL_DEVICE)
        .env("GIT_CONFIG_NOSYSTEM", "1")
}

fn git(dir: &Path, args: &[&str]) -> String {
    let mut cmd = Command::new("git");
    git_env(&mut cmd).current_dir(dir).args(args);
    let out = cmd
        .output()
        .expect("`git` must be installed to run these tests");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

impl Sandbox {
    fn new(name: &str) -> Sandbox {
        let root = std::env::temp_dir().join(format!("colibri-git-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("home")).unwrap();
        Sandbox { root }
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    fn write(&self, rel: &str, text: &str) {
        let full = self.root.join(rel);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, text).unwrap();
    }

    /// An empty repository on branch `main`.
    fn repo(&self, name: &str) -> Repo {
        let dir = self.root.join("repos").join(name);
        fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "--quiet", "-b", "main"]);
        Repo { dir }
    }

    /// A repository with a package `name` whose `lib.colibri` answers `version()` with `v`.
    fn lib_repo(&self, name: &str, v: &str) -> (Repo, String) {
        let repo = self.repo(name);
        let sha = repo.commit(&lib_files(name, v, ""), "first");
        (repo, sha)
    }

    /// An app package `app` whose `main` prints `<dep>.version()` for each of `deps`.
    fn app(&self, folder: &str, deps_toml: &str, uses: &[&str]) -> PathBuf {
        let dir = format!("apps/{folder}");
        self.write(
            &format!("{dir}/colibri.toml"),
            &format!("[package]\nname = \"app\"\n\n[dependencies]\n{deps_toml}\n"),
        );
        let imports: String = uses.iter().map(|u| format!("import {u}\n")).collect();
        let calls: String = uses
            .iter()
            .map(|u| format!("    print({u}.version())\n"))
            .collect();
        self.write(
            &format!("{dir}/main.colibri"),
            &format!("{imports}fn main() {{\n{calls}}}\n"),
        );
        self.path(&dir)
    }

    fn colibri(&self, args: &[&str]) -> Out {
        self.colibri_env(args, &[])
    }

    fn colibri_env(&self, args: &[&str], env: &[(&str, &str)]) -> Out {
        let mut cmd = colibri_command();
        git_env(&mut cmd)
            .env("COLIBRI_HOME", self.home())
            .args(args);
        for (k, v) in env {
            cmd.env(k, v);
        }
        let out = cmd.output().expect("the colibri binary runs");
        Out {
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            code: out.status.code().unwrap_or(-1),
        }
    }

    fn run(&self, dir: &Path) -> Out {
        self.colibri(&["run", dir.to_str().unwrap()])
    }

    fn lock(&self, dir: &Path) -> String {
        fs::read_to_string(dir.join("colibri.lock")).expect("a colibri.lock")
    }
}

struct Repo {
    dir: PathBuf,
}

impl Repo {
    /// The url of this repository as a plain path (`/` separated, `C:/..` on Windows).
    fn url(&self) -> String {
        slashes(&self.dir)
    }

    /// The same as a `file://` url: `file:///tmp/..`, `file:///C:/..` on Windows.
    fn file_url(&self) -> String {
        let path = self.url();
        if path.starts_with('/') {
            format!("file://{path}")
        } else {
            format!("file:///{path}")
        }
    }

    /// Writes `files`, commits all and returns the commit.
    fn commit(&self, files: &[(String, String)], message: &str) -> String {
        for (path, text) in files {
            let full = self.dir.join(path);
            fs::create_dir_all(full.parent().unwrap()).unwrap();
            fs::write(full, text).unwrap();
        }
        git(&self.dir, &["add", "--all"]);
        git(&self.dir, &["commit", "--quiet", "-m", message]);
        git(&self.dir, &["rev-parse", "HEAD"])
    }

    /// Adds `path` to the index with the content `text` (a regular file) without touching the
    /// working folder: Windows cannot write every name git can store.
    fn stage_file(&self, path: &str, text: &str) {
        self.stage(path, text, "100644");
    }

    /// Adds a symbolic link `path` pointing at `target` to the index, written with git's own
    /// commands: no link is made on disk, so this works where links need a privilege.
    fn stage_link(&self, path: &str, target: &str) {
        self.stage(path, target, "120000");
    }

    fn stage(&self, path: &str, content: &str, mode: &str) {
        let blob = self.dir.join(".colibri-test-blob");
        fs::write(&blob, content).unwrap();
        let oid = git(&self.dir, &["hash-object", "-w", &slashes(&blob)]);
        fs::remove_file(&blob).unwrap();
        git(
            &self.dir,
            // `core.protectNTFS=false`: Git for Windows refuses to stage a name such as
            // `docs/aux.md` otherwise, and the tests that stage one are about what the tool
            // does with such a repository, not about what git lets a Windows user add.
            &[
                "-c",
                "core.protectNTFS=false",
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("{mode},{oid},{path}"),
            ],
        );
    }

    fn tag(&self, tag: &str) {
        git(&self.dir, &["tag", tag]);
    }

    fn branch(&self, branch: &str) {
        git(&self.dir, &["branch", branch]);
    }

    fn checkout(&self, branch: &str) {
        git(&self.dir, &["checkout", "--quiet", branch]);
    }
}

/// The files of a package `name` with `version()` answering `v`; `extra` is more manifest text.
fn lib_files(name: &str, v: &str, extra: &str) -> Vec<(String, String)> {
    vec![
        (
            "colibri.toml".into(),
            format!("[package]\nname = \"{name}\"\n{extra}"),
        ),
        (
            "lib.colibri".into(),
            format!("pub fn version() -> str => \"{v}\"\n"),
        ),
    ]
}

fn new_version(repo: &Repo, name: &str, v: &str) -> String {
    repo.commit(&lib_files(name, v, ""), &format!("version {v}"))
}

#[test]
fn a_rev_dependency_is_checked_out_at_that_commit_and_locked() {
    let sb = Sandbox::new("rev");
    let (repo, c1) = sb.lib_repo("greet", "one");
    new_version(&repo, "greet", "two");
    let app = sb.app(
        "app",
        &format!("greet = {{ git = \"{}\", rev = \"{c1}\" }}", repo.url()),
        &["greet"],
    );
    let out = sb.run(&app);
    assert_eq!(
        (out.code, out.stdout.as_str()),
        (0, "one\n"),
        "{}",
        out.stderr
    );
    assert!(out.stderr.contains("locking greet"), "{}", out.stderr);
    let lock = sb.lock(&app);
    assert!(lock.contains("[git.greet]"), "{lock}");
    assert!(lock.contains(&format!("rev = \"{c1}\"")), "{lock}");
    assert!(lock.contains(&format!("commit = \"{c1}\"")), "{lock}");
    // The checkout is in the cache, under its url and its commit.
    let checkouts = sb.home().join("git/checkouts");
    let repo_dir = fs::read_dir(&checkouts)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert!(
        repo_dir
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("greet-")
    );
    assert!(repo_dir.join(&c1).join("lib.colibri").is_file());
    assert!(!repo_dir.join(&c1).join(".git").exists());
}

#[test]
fn an_abbreviated_rev_resolves_to_the_full_commit() {
    let sb = Sandbox::new("short_rev");
    let (repo, c1) = sb.lib_repo("greet", "one");
    new_version(&repo, "greet", "two");
    let app = sb.app(
        "app",
        &format!(
            "greet = {{ git = \"{}\", rev = \"{}\" }}",
            repo.url(),
            &c1[..8]
        ),
        &["greet"],
    );
    let out = sb.run(&app);
    assert_eq!(
        (out.code, out.stdout.as_str()),
        (0, "one\n"),
        "{}",
        out.stderr
    );
    assert!(sb.lock(&app).contains(&format!("commit = \"{c1}\"")));
}

#[test]
fn tag_branch_and_default_branch_dependencies() {
    let sb = Sandbox::new("refs");
    let (repo, c1) = sb.lib_repo("greet", "one");
    repo.tag("v1");
    repo.branch("old");
    let c2 = new_version(&repo, "greet", "two");
    repo.checkout("old");
    let c3 = new_version(&repo, "greet", "three");
    repo.checkout("main");
    let url = repo.url();
    for (spec, want, commit) in [
        ("tag = \"v1\"", "one\n", &c1),
        ("branch = \"old\"", "three\n", &c3),
        ("", "two\n", &c2),
    ] {
        let comma = if spec.is_empty() { "" } else { ", " };
        let app = sb.app(
            &format!("app{}", spec.len()),
            &format!("greet = {{ git = \"{url}\"{comma}{spec} }}"),
            &["greet"],
        );
        let out = sb.run(&app);
        assert_eq!(
            (out.code, out.stdout.as_str()),
            (0, want),
            "{spec}: {}",
            out.stderr
        );
        let lock = sb.lock(&app);
        assert!(
            lock.contains(&format!("commit = \"{commit}\"")),
            "{spec}: {lock}"
        );
        if !spec.is_empty() {
            assert!(lock.contains(spec), "{lock}");
        }
    }
}

#[test]
fn a_file_url_works_like_a_path() {
    let sb = Sandbox::new("file_url");
    let (repo, c1) = sb.lib_repo("greet", "one");
    let app = sb.app(
        "app",
        &format!("greet = {{ git = \"{}\" }}", repo.file_url()),
        &["greet"],
    );
    let out = sb.run(&app);
    assert_eq!(
        (out.code, out.stdout.as_str()),
        (0, "one\n"),
        "{}",
        out.stderr
    );
    assert!(sb.lock(&app).contains(&c1));
}

#[test]
fn the_lock_is_stable_and_used_without_the_network() {
    let sb = Sandbox::new("reuse");
    let (repo, _) = sb.lib_repo("greet", "one");
    let app = sb.app(
        "app",
        &format!("greet = {{ git = \"{}\" }}", repo.url()),
        &["greet"],
    );
    assert_eq!(sb.run(&app).code, 0);
    let first = sb.lock(&app);
    // The branch moves on, but the lock still says the old commit.
    new_version(&repo, "greet", "two");
    let out = sb.run(&app);
    assert_eq!(
        (out.code, out.stdout.as_str()),
        (0, "one\n"),
        "{}",
        out.stderr
    );
    assert!(out.stderr.is_empty(), "{}", out.stderr);
    assert_eq!(sb.lock(&app), first);
    // And with the repository gone, nothing is fetched at all.
    fs::remove_dir_all(&repo.dir).unwrap();
    for cmd in ["run", "check", "test"] {
        let out = sb.colibri(&[cmd, app.to_str().unwrap()]);
        assert_eq!(out.code, 0, "{cmd}: {}", out.stderr);
    }
    assert_eq!(sb.lock(&app), first);
}

#[test]
fn two_projects_share_one_checkout() {
    let sb = Sandbox::new("share");
    let (repo, _) = sb.lib_repo("greet", "one");
    let dep = format!("greet = {{ git = \"{}\" }}", repo.url());
    let a = sb.app("a", &dep, &["greet"]);
    assert_eq!(sb.run(&a).code, 0);
    // The second project locks from the cache: no repository, no network.
    fs::remove_dir_all(&repo.dir).unwrap();
    let b = sb.app("b", &dep, &["greet"]);
    let out = sb.colibri(&["run", "--offline", b.to_str().unwrap()]);
    assert_eq!(
        (out.code, out.stdout.as_str()),
        (0, "one\n"),
        "{}",
        out.stderr
    );
    let checkouts: Vec<_> = fs::read_dir(sb.home().join("git/checkouts"))
        .unwrap()
        .collect();
    assert_eq!(checkouts.len(), 1);
}

#[test]
fn a_changed_manifest_re_resolves_only_the_changed_entries() {
    let sb = Sandbox::new("stale");
    let (ra, ca) = sb.lib_repo("alpha", "a1");
    ra.tag("v1");
    let (rb, cb1) = sb.lib_repo("beta", "b1");
    rb.branch("dev");
    rb.checkout("dev");
    let cb2 = new_version(&rb, "beta", "b2");
    rb.checkout("main");
    let dep_a = format!("alpha = {{ git = \"{}\", tag = \"v1\" }}", ra.url());
    let app = sb.app(
        "app",
        &format!("{dep_a}\nbeta = {{ git = \"{}\" }}", rb.url()),
        &["alpha", "beta"],
    );
    let out = sb.run(&app);
    assert_eq!(
        (out.code, out.stdout.as_str()),
        (0, "a1\nb1\n"),
        "{}",
        out.stderr
    );
    assert!(sb.lock(&app).contains(&cb1));
    // `alpha` could not be resolved again now; it need not be, as it did not change.
    fs::remove_dir_all(&ra.dir).unwrap();
    let app = sb.app(
        "app",
        &format!(
            "{dep_a}\nbeta = {{ git = \"{}\", branch = \"dev\" }}",
            rb.url()
        ),
        &["alpha", "beta"],
    );
    let out = sb.run(&app);
    assert_eq!(
        (out.code, out.stdout.as_str()),
        (0, "a1\nb2\n"),
        "{}",
        out.stderr
    );
    let lock = sb.lock(&app);
    assert!(
        lock.contains(&ca) && lock.contains(&cb2) && !lock.contains(&cb1),
        "{lock}"
    );
    assert!(out.stderr.contains("updating beta") && !out.stderr.contains("alpha"));
    // A dependency that is removed leaves the lock.
    let app = sb.app(
        "app",
        &format!("beta = {{ git = \"{}\" }}", rb.url()),
        &["beta"],
    );
    assert_eq!(sb.run(&app).code, 0);
    assert!(!sb.lock(&app).contains("alpha"));
}

#[test]
fn update_moves_branches_and_tags_and_only_the_named_one() {
    let sb = Sandbox::new("update");
    let (ra, ca1) = sb.lib_repo("alpha", "a1");
    let (rb, cb1) = sb.lib_repo("beta", "b1");
    let app = sb.app(
        "app",
        &format!(
            "alpha = {{ git = \"{}\" }}\nbeta = {{ git = \"{}\" }}",
            ra.url(),
            rb.url()
        ),
        &["alpha", "beta"],
    );
    assert_eq!(sb.run(&app).code, 0);
    let ca2 = new_version(&ra, "alpha", "a2");
    let cb2 = new_version(&rb, "beta", "b2");
    // `run` does not move anything.
    assert_eq!(sb.run(&app).stdout, "a1\nb1\n");
    let path = app.to_str().unwrap();
    let out = sb.colibri(&["update", "alpha", "--path", path]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(out.stderr.contains("updating alpha"), "{}", out.stderr);
    let lock = sb.lock(&app);
    assert!(
        lock.contains(&ca2) && !lock.contains(&ca1) && lock.contains(&cb1),
        "{lock}"
    );
    assert_eq!(sb.run(&app).stdout, "a2\nb1\n");
    let out = sb.colibri(&["update", "--path", path]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(sb.lock(&app).contains(&cb2));
    assert_eq!(sb.run(&app).stdout, "a2\nb2\n");
    // Nothing moved: nothing is said, and the file is the same.
    let before = sb.lock(&app);
    let out = sb.colibri(&["update", "--path", path]);
    assert_eq!((out.code, out.stderr.as_str()), (0, ""));
    assert_eq!(sb.lock(&app), before);
    let out = sb.colibri(&["update", "nope", "--path", path]);
    assert_eq!(out.code, 1);
    assert!(
        out.stderr.contains("`nope` is not a git dependency"),
        "{}",
        out.stderr
    );
    assert!(out.stderr.contains("`alpha`, `beta`"), "{}", out.stderr);
}

#[test]
fn a_moved_tag_stays_put_until_update() {
    let sb = Sandbox::new("moved_tag");
    let (repo, c1) = sb.lib_repo("greet", "one");
    repo.tag("stable");
    let app = sb.app(
        "app",
        &format!("greet = {{ git = \"{}\", tag = \"stable\" }}", repo.url()),
        &["greet"],
    );
    assert_eq!(sb.run(&app).code, 0);
    let c2 = new_version(&repo, "greet", "two");
    git(&repo.dir, &["tag", "--force", "stable"]);
    assert!(sb.lock(&app).contains(&c1));
    assert_eq!(sb.run(&app).stdout, "one\n");
    assert_eq!(
        sb.colibri(&["update", "--path", app.to_str().unwrap()])
            .code,
        0
    );
    assert!(sb.lock(&app).contains(&c2));
    assert_eq!(sb.run(&app).stdout, "two\n");
}

#[test]
fn transitive_git_and_path_dependencies_are_locked_and_work() {
    let sb = Sandbox::new("transitive");
    let (rc, cc) = sb.lib_repo("deep", "deep");
    // `mid` uses `deep` from git and `helper` from a folder of its own repository.
    let mid = sb.repo("mid");
    let mut files = lib_files(
        "mid",
        "mid",
        &format!(
            "\n[dependencies]\ndeep = {{ git = \"{}\" }}\nhelper = {{ path = \"helper\" }}\n",
            rc.url()
        ),
    );
    files[1].1 = "import deep\nimport helper\npub fn version() -> str => \"mid+{deep.version()}+{helper.version()}\"\n".into();
    files.extend(
        lib_files("helper", "help", "")
            .into_iter()
            .map(|(p, t)| (format!("helper/{p}"), t)),
    );
    let cm = mid.commit(&files, "mid");
    let app = sb.app(
        "app",
        &format!("mid = {{ git = \"{}\" }}", mid.url()),
        &["mid"],
    );
    let out = sb.run(&app);
    assert_eq!(
        (out.code, out.stdout.as_str()),
        (0, "mid+deep+help\n"),
        "{}",
        out.stderr
    );
    let lock = sb.lock(&app);
    assert!(
        lock.find("[git.deep]").unwrap() < lock.find("[git.mid]").unwrap(),
        "{lock}"
    );
    assert!(lock.contains(&cc) && lock.contains(&cm), "{lock}");
    assert!(
        !lock.contains("helper"),
        "path dependencies are not locked: {lock}"
    );
    // Both repositories can go: the lock and the cache are enough.
    fs::remove_dir_all(&rc.dir).unwrap();
    fs::remove_dir_all(&mid.dir).unwrap();
    let out = sb.run(&app);
    assert_eq!(
        (out.code, out.stdout.as_str()),
        (0, "mid+deep+help\n"),
        "{}",
        out.stderr
    );
}

#[test]
fn a_path_of_a_git_dependency_is_refused_by_its_text_with_either_separator() {
    // Whatever system it is on: a manifest from git that names a folder outside its repository
    // is refused on what it says, `/` or `\`, absolute, a drive, a UNC or verbatim prefix, or
    // a climb out of the repository.
    let sb = Sandbox::new("escape_text");
    for (i, rel) in [
        r"..\..\..\outside",
        "../../../outside",
        r"\\?\C:\outside",
        r"\\host\share\outside",
        r"C:\outside",
        "/outside",
    ]
    .iter()
    .enumerate()
    {
        let repo = sb.repo(&format!("evil{i}"));
        repo.commit(
            &[
                (
                    "colibri.toml".to_string(),
                    format!(
                        "[package]\nname = \"evil\"\n[dependencies]\noutside = {{ path = '{rel}' }}\n"
                    ),
                ),
                (
                    "lib.colibri".to_string(),
                    "pub fn version() -> str => \"e\"\n".to_string(),
                ),
            ],
            "evil",
        );
        let app = sb.app(
            &format!("app_text{i}"),
            &format!("evil = {{ git = \"{}\" }}", repo.url()),
            &["evil"],
        );
        let out = sb.run(&app);
        assert_eq!(out.code, 1, "{rel}: {}", out.stderr);
        assert!(
            out.stderr.contains("leaves the git repository"),
            "{rel}: {}",
            out.stderr
        );
    }
}

#[test]
fn a_path_dependency_of_a_git_dependency_cannot_leave_its_repository() {
    let sb = Sandbox::new("escape");
    sb.write("outside/colibri.toml", "[package]\nname = \"outside\"\n");
    sb.write("outside/lib.colibri", "pub fn version() -> str => \"x\"\n");
    let repo = sb.repo("evil");
    let outside = sb.path("outside");
    let files = vec![
        (
            "colibri.toml".to_string(),
            format!(
                "[package]\nname = \"evil\"\n[dependencies]\noutside = {{ path = \"{}\" }}\n",
                slashes(&outside)
            ),
        ),
        (
            "lib.colibri".to_string(),
            "pub fn version() -> str => \"e\"\n".to_string(),
        ),
    ];
    repo.commit(&files, "evil");
    let app = sb.app(
        "app",
        &format!("evil = {{ git = \"{}\" }}", repo.url()),
        &["evil"],
    );
    let out = sb.run(&app);
    assert_eq!(out.code, 1);
    assert!(
        out.stderr.contains("leaves the git repository"),
        "{}",
        out.stderr
    );
}

#[test]
fn two_versions_of_one_package_are_an_error_naming_both() {
    let sb = Sandbox::new("dup");
    let (rc, c1) = sb.lib_repo("shared", "s1");
    let c2 = new_version(&rc, "shared", "s2");
    let mk = |name: &str, rev: &str| {
        let r = sb.repo(name);
        let mut files = lib_files(
            name,
            name,
            &format!(
                "\n[dependencies]\nshared = {{ git = \"{}\", rev = \"{rev}\" }}\n",
                rc.url()
            ),
        );
        files[1].1 = format!("pub fn version() -> str => \"{name}\"\n");
        r.commit(&files, name);
        r
    };
    let ra = mk("alpha", &c1);
    let rb = mk("beta", &c2);
    let app = sb.app(
        "app",
        &format!(
            "alpha = {{ git = \"{}\" }}\nbeta = {{ git = \"{}\" }}",
            ra.url(),
            rb.url()
        ),
        &["alpha", "beta"],
    );
    let out = sb.run(&app);
    assert_eq!(out.code, 1);
    assert!(
        out.stderr
            .contains("two different versions of the package `shared`"),
        "{}",
        out.stderr
    );
    assert!(
        out.stderr.contains(&c1[..10]) && out.stderr.contains(&c2[..10]),
        "{}",
        out.stderr
    );
    assert!(
        out.stderr.contains("`alpha` asks for") && out.stderr.contains("`beta` asks for"),
        "{}",
        out.stderr
    );
    assert!(!app.join("colibri.lock").exists());
    // The same commit twice is one package.
    let rb = mk("beta", &c1);
    let app = sb.app(
        "app2",
        &format!(
            "alpha = {{ git = \"{}\" }}\nbeta = {{ git = \"{}\" }}",
            ra.url(),
            rb.url()
        ),
        &["alpha", "beta"],
    );
    let out = sb.run(&app);
    assert_eq!(out.code, 0, "{}", out.stderr);
}

#[test]
fn a_path_and_a_git_package_of_one_name_are_an_error_naming_both() {
    let sb = Sandbox::new("dup_path");
    let (rc, _) = sb.lib_repo("shared", "git");
    sb.write(
        "local/shared/colibri.toml",
        "[package]\nname = \"shared\"\n",
    );
    sb.write(
        "local/shared/lib.colibri",
        "pub fn version() -> str => \"path\"\n",
    );
    let repo = sb.repo("holder");
    let mut files = lib_files(
        "holder",
        "h",
        &format!("\n[dependencies]\nshared = {{ git = \"{}\" }}\n", rc.url()),
    );
    files[1].1 = "pub fn version() -> str => \"h\"\n".into();
    repo.commit(&files, "holder");
    let shared_path = sb.path("local/shared");
    let app = sb.app(
        "app",
        &format!(
            "holder = {{ git = \"{}\" }}\nshared = {{ path = \"{}\" }}",
            repo.url(),
            slashes(&shared_path)
        ),
        &["holder"],
    );
    let out = sb.run(&app);
    assert_eq!(out.code, 1);
    assert!(
        out.stderr
            .contains("two different packages are both called `shared`"),
        "{}",
        out.stderr
    );
    assert!(
        out.stderr.contains(&native(&shared_path))
            && out
                .stderr
                .contains(&native(&sb.home().join("git/checkouts"))),
        "{}",
        out.stderr
    );
}

#[test]
fn locked_fails_instead_of_writing_the_lock() {
    let sb = Sandbox::new("locked");
    let (repo, _) = sb.lib_repo("greet", "one");
    let dep = format!("greet = {{ git = \"{}\" }}", repo.url());
    let app = sb.app("app", &dep, &["greet"]);
    let path = app.to_str().unwrap();
    let out = sb.colibri(&["check", "--locked", path]);
    assert_eq!(out.code, 1);
    assert!(out.stderr.contains("`--locked`"), "{}", out.stderr);
    assert!(!app.join("colibri.lock").exists());
    assert_eq!(sb.colibri(&["check", path]).code, 0);
    let lock = sb.lock(&app);
    for cmd in ["check", "run", "test"] {
        assert_eq!(sb.colibri(&[cmd, "--locked", path]).code, 0, "{cmd}");
    }
    // A manifest that the lock no longer matches.
    repo.branch("dev");
    let app = sb.app(
        "app",
        &format!("greet = {{ git = \"{}\", branch = \"dev\" }}", repo.url()),
        &["greet"],
    );
    let out = sb.colibri(&["run", "--locked", path]);
    assert_eq!(out.code, 1);
    assert!(out.stderr.contains("`--locked`"), "{}", out.stderr);
    assert_eq!(sb.lock(&app), lock);
}

#[test]
fn offline_uses_the_cache_and_never_the_repository() {
    let sb = Sandbox::new("offline");
    let (repo, _) = sb.lib_repo("greet", "one");
    let app = sb.app(
        "app",
        &format!("greet = {{ git = \"{}\" }}", repo.url()),
        &["greet"],
    );
    let path = app.to_str().unwrap();
    let out = sb.colibri(&["check", "--offline", path]);
    assert_eq!(out.code, 1);
    assert!(out.stderr.contains("offline"), "{}", out.stderr);
    assert!(!app.join("colibri.lock").exists());
    assert_eq!(sb.colibri(&["check", path]).code, 0);
    // Even a url that does not exist any more is not contacted.
    new_version(&repo, "greet", "two");
    fs::remove_dir_all(&repo.dir).unwrap();
    assert_eq!(sb.colibri(&["run", "--offline", path]).stdout, "one\n");
    // `update --offline` can only answer from what the cache already has.
    let out = sb.colibri(&["update", "--offline", "--path", path]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(sb.colibri(&["run", "--offline", path]).stdout, "one\n");
}

#[test]
fn each_failure_to_fetch_has_its_own_message() {
    let sb = Sandbox::new("errors");
    let (repo, _) = sb.lib_repo("greet", "one");
    repo.tag("v1");
    let url = repo.url();
    let check = |name: &str, spec: &str| -> Out {
        let app = sb.app(
            name,
            &format!("greet = {{ git = \"{url}\"{spec} }}"),
            &["greet"],
        );
        sb.colibri(&["check", app.to_str().unwrap()])
    };
    let out = check(
        "unknown_rev",
        ", rev = \"0123456789abcdef0123456789abcdef01234567\"",
    );
    assert_eq!(out.code, 1);
    assert!(
        out.stderr.contains("has no commit `0123456789abcdef"),
        "{}",
        out.stderr
    );
    assert!(
        out.stderr.contains("the dependency `greet`"),
        "{}",
        out.stderr
    );
    let out = check("unknown_tag", ", tag = \"v9\"");
    assert!(out.stderr.contains("has no tag `v9`"), "{}", out.stderr);
    let out = check("unknown_branch", ", branch = \"nope\"");
    assert!(
        out.stderr.contains("has no branch `nope`"),
        "{}",
        out.stderr
    );

    // The url does not lead anywhere.
    let app = sb.app(
        "unreachable",
        &format!(
            "greet = {{ git = \"{}\" }}",
            slashes(&sb.path("repos/missing"))
        ),
        &["greet"],
    );
    let out = sb.colibri(&["check", app.to_str().unwrap()]);
    assert_eq!(out.code, 1);
    assert!(out.stderr.contains("cannot fetch"), "{}", out.stderr);
    assert!(out.stderr.contains("repos/missing"), "{}", out.stderr);

    // A repository with no commits at all.
    let empty = sb.repo("empty");
    let app = sb.app(
        "empty_app",
        &format!("greet = {{ git = \"{}\" }}", empty.url()),
        &["greet"],
    );
    let out = sb.colibri(&["check", app.to_str().unwrap()]);
    assert!(out.stderr.contains("has no commits"), "{}", out.stderr);

    // A repository that is not a Colibri package.
    let plain = sb.repo("plain");
    let sha = plain.commit(&[("README".to_string(), "hi\n".to_string())], "readme");
    let app = sb.app(
        "plain_app",
        &format!("greet = {{ git = \"{}\" }}", plain.url()),
        &["greet"],
    );
    let out = sb.colibri(&["check", app.to_str().unwrap()]);
    assert_eq!(out.code, 1);
    assert!(
        out.stderr.contains("has no `colibri.toml`"),
        "{}",
        out.stderr
    );
    assert!(out.stderr.contains(&sha[..10]), "{}", out.stderr);
    assert!(!app.join("colibri.lock").exists());
}

#[test]
fn a_missing_git_program_is_said_plainly() {
    let sb = Sandbox::new("no_git");
    let (repo, _) = sb.lib_repo("greet", "one");
    let app = sb.app(
        "app",
        &format!("greet = {{ git = \"{}\" }}", repo.url()),
        &["greet"],
    );
    let empty = sb.path("empty_path");
    fs::create_dir_all(&empty).unwrap();
    let out = sb.colibri_env(
        &["check", app.to_str().unwrap()],
        &[("PATH", empty.to_str().unwrap())],
    );
    assert_eq!(out.code, 1);
    assert!(
        out.stderr.contains("the `git` program was not found"),
        "{}",
        out.stderr
    );
}

#[test]
fn manifest_mistakes_in_a_git_dependency_are_reported() {
    let sb = Sandbox::new("manifest");
    for (spec, want) in [
        (", rev = \"abc1234\", tag = \"v1\"", "both `rev` and `tag`"),
        (", rev = \"main\"", "not a commit hash"),
        (", path = \"x\"", "both `path` and `git`"),
        (
            ", version = \"1\"",
            "registry dependencies are not supported yet",
        ),
    ] {
        let app = sb.app(
            "app",
            &format!("greet = {{ git = \"/x\"{spec} }}"),
            &["greet"],
        );
        let out = sb.colibri(&["check", app.to_str().unwrap()]);
        assert_eq!(out.code, 1, "{spec}");
        assert!(out.stderr.contains(want), "{spec}: {}", out.stderr);
    }
}

#[test]
fn a_git_dependency_with_ffi_must_be_allowed_by_the_root() {
    let sb = Sandbox::new("ffi");
    let extern_lib = "@link(\"c\")\nextern \"C\" { pub fn abs(x: i32) -> i32 }\npub fn version() -> str => \"{abs(-1)}\"\n";
    let make = |name: &str, extra: &str| {
        let repo = sb.repo(name);
        let mut files = lib_files(name, "", extra);
        files[1].1 = extern_lib.into();
        repo.commit(&files, "ffi");
        repo
    };
    // An app manifest with the given `[package]` extras and dependencies.
    let app_with = |folder: &str, package_extra: &str, deps: &str, uses: &[&str]| {
        let app = sb.app(folder, deps, uses);
        fs::write(
            app.join("colibri.toml"),
            format!("[package]\nname = \"app\"\n{package_extra}[dependencies]\n{deps}\n"),
        )
        .unwrap();
        app
    };
    // The app declaring `ffi = true` does not grant it to what it depends on.
    let quiet = make("quiet", "");
    let dep = format!("quiet = {{ git = \"{}\" }}", quiet.url());
    let app = app_with("app", "ffi = true\n", &dep, &["quiet"]);
    let out = sb.colibri(&["check", app.to_str().unwrap()]);
    assert_eq!(out.code, 1);
    assert!(
        out.stderr
            .contains("the package `quiet` cannot contain `extern` blocks"),
        "{}",
        out.stderr
    );
    // The dependency says so in its own manifest, but the root has not allowed it: the error
    // names the package, who needs it, and the line to add.
    let open = make("open", "ffi = true\n");
    let dep = format!("open = {{ git = \"{}\" }}", open.url());
    let app = app_with("app2", "", &dep, &["open"]);
    let out = sb.colibri(&["check", app.to_str().unwrap()]);
    assert_eq!(out.code, 1, "{}", out.stderr);
    for want in [
        "the git dependency `open` declares `ffi = true`",
        "needed by `app`",
        "allow-ffi = [\"open\"]",
        "colibri.toml",
    ] {
        assert!(out.stderr.contains(want), "{want}: {}", out.stderr);
    }
    // Another name in the list does not help; the right one does.
    let app = app_with("app3", "allow-ffi = [\"other\"]\n", &dep, &["open"]);
    assert_eq!(sb.colibri(&["check", app.to_str().unwrap()]).code, 1);
    let app = app_with(
        "app4",
        "allow-ffi = [\"other\", \"open\"]\n",
        &dep,
        &["open"],
    );
    let out = sb.colibri(&["check", app.to_str().unwrap()]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(sb.run(&app).stdout, "1\n");
    // A transitive one, which the user never read, needs the root's say-so as well.
    let holder = sb.repo("holder");
    let mut files = lib_files(
        "holder",
        "h",
        &format!("\n[dependencies]\nopen = {{ git = \"{}\" }}\n", open.url()),
    );
    files[1].1 = "import open\npub fn version() -> str => open.version()\n".into();
    holder.commit(&files, "holder");
    let dep = format!("holder = {{ git = \"{}\" }}", holder.url());
    let app = app_with("app5", "", &dep, &["holder"]);
    let out = sb.colibri(&["check", app.to_str().unwrap()]);
    assert_eq!(out.code, 1, "{}", out.stderr);
    assert!(
        out.stderr.contains("`open` declares `ffi = true`")
            && out.stderr.contains("needed by `holder`"),
        "{}",
        out.stderr
    );
    let app = app_with("app6", "allow-ffi = [\"open\"]\n", &dep, &["holder"]);
    let out = sb.colibri(&["check", app.to_str().unwrap()]);
    assert_eq!(out.code, 0, "{}", out.stderr);
}

#[test]
fn a_git_dependency_with_rust_dependencies_must_be_allowed_by_the_root() {
    let sb = Sandbox::new("allow_rust");
    let repo = sb.repo("bound");
    let mut vm = colibri_runtime::Vm::builder().build();
    vm.module("native")
        .register_fn("add", |a: i64, b: i64| a + b)
        .unwrap();
    let mut files = lib_files(
        "bound",
        "b",
        "\n[rust-dependencies]\nnative = { path = \"native/\" }\n",
    );
    files.push(("native.cbi".into(), vm.module("native").interface_text()));
    repo.commit(&files, "bound");
    // A second package declares the table but did not commit the interface.
    let bare = sb.repo("bare");
    bare.commit(
        &lib_files(
            "bare",
            "b",
            "\n[rust-dependencies]\nother = { path = \"other/\" }\n",
        ),
        "bare",
    );
    let dep = format!("bound = {{ git = \"{}\" }}", repo.url());
    let app_with = |folder: &str, package_extra: &str| {
        let app = sb.app(folder, &dep, &["bound"]);
        fs::write(
            app.join("colibri.toml"),
            format!("[package]\nname = \"app\"\n{package_extra}[dependencies]\n{dep}\n"),
        )
        .unwrap();
        app
    };
    let app = app_with("app", "");
    let out = sb.colibri(&["check", app.to_str().unwrap()]);
    assert_eq!(out.code, 1, "{}", out.stderr);
    for want in [
        "the git dependency `bound` declares `[rust-dependencies]`",
        "needed by `app`",
        "allow-rust = [\"bound\"]",
    ] {
        assert!(out.stderr.contains(want), "{want}: {}", out.stderr);
    }
    // Its author must commit the interface: the hint says so.
    let bare_dep = format!("bare = {{ git = \"{}\" }}", bare.url());
    let app = sb.app("app_bare", &bare_dep, &["bare"]);
    fs::write(
        app.join("colibri.toml"),
        format!("[package]\nname = \"app\"\nallow-rust = [\"bare\"]\n[dependencies]\n{bare_dep}\n"),
    )
    .unwrap();
    let out = sb.colibri(&["check", app.to_str().unwrap()]);
    assert_eq!(out.code, 1, "{}", out.stderr);
    for want in [
        "`other` is a Rust binding",
        "the author of this git package must generate it and commit it",
    ] {
        assert!(out.stderr.contains(want), "{want}: {}", out.stderr);
    }
    // `allow-ffi` is another capability: it does not allow this one.
    let app = app_with("app2", "allow-ffi = [\"bound\"]\n");
    assert_eq!(sb.colibri(&["check", app.to_str().unwrap()]).code, 1);
    let app = app_with("app3", "allow-rust = [\"other\", \"bound\"]\n");
    let out = sb.colibri(&["check", app.to_str().unwrap()]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    // A name that matches no dependency is a warning, as for `allow-ffi`.
    assert!(
        out.stderr.contains("`allow-rust` lists `other`") && !out.stderr.contains("lists `bound`"),
        "{}",
        out.stderr
    );
}

#[test]
fn loading_never_fetches_it_says_to_run_the_tool() {
    // This is what the language server does: it loads through the library, which has no
    // network code, so a dependency that is not in the cache is a diagnostic.
    let sb = Sandbox::new("no_fetch");
    let (repo, _) = sb.lib_repo("greet", "one");
    let app = sb.app(
        "app",
        &format!("greet = {{ git = \"{}\" }}", repo.url()),
        &["greet"],
    );
    let config = colibri_runtime::CheckConfig::default();
    let report = colibri_runtime::check_package_with(&app, &config)
        .unwrap()
        .render(false);
    assert!(report.contains("is not in `colibri.lock` yet"), "{report}");
    assert!(
        report.contains("run `colibri check` or `colibri update`"),
        "{report}"
    );
    assert!(!app.join("colibri.lock").exists());
    // With a lock, but a cache that does not have the checkout (this process's cache, not the
    // sandbox's).
    let lock = format!(
        "[git.greet]\nurl = \"{}\"\ncommit = \"{}\"\n",
        repo.url(),
        "0123456789abcdef0123456789abcdef01234567"
    );
    fs::write(app.join("colibri.lock"), lock).unwrap();
    let report = colibri_runtime::check_package_with(&app, &config)
        .unwrap()
        .render(false);
    assert!(report.contains("is not in the cache"), "{report}");
    assert!(
        report.contains("run `colibri check` or `colibri update`"),
        "{report}"
    );
    // A lock for something else than the manifest says is out of date.
    fs::write(
        app.join("colibri.lock"),
        "[git.greet]\nurl = \"/elsewhere\"\ncommit = \"0123456789abcdef0123456789abcdef01234567\"\n",
    )
    .unwrap();
    let report = colibri_runtime::check_package_with(&app, &config)
        .unwrap()
        .render(false);
    assert!(
        report.contains("is out of date for the dependency `greet`"),
        "{report}"
    );
}

#[test]
fn symlinks_in_a_dependency_are_ignored_and_never_followed() {
    // The links are put into the repository with git's plumbing, so no link is made on disk.
    let sb = Sandbox::new("symlinks");
    let secret = sb.path("outside/secret");
    sb.write("outside/secret", "TOP-SECRET-LINE\nmore\n");
    sb.write(
        "outside/sneaky.colibri",
        "pub fn version() -> str => \"OUTSIDE\"\n",
    );
    let repo = sb.repo("linky");
    repo.commit(&lib_files("linky", "ok", ""), "lib");
    repo.stage_link("leak.colibri", &slashes(&secret));
    repo.stage_link(
        "sneaky.colibri",
        &slashes(&sb.path("outside/sneaky.colibri")),
    );
    git(&repo.dir, &["commit", "--quiet", "-m", "links"]);
    let mode = git(&repo.dir, &["ls-files", "-s", "leak.colibri"]);
    assert!(mode.starts_with("120000"), "{mode}");
    for module in ["leak", "sneaky"] {
        let app = sb.app(
            &format!("app_{module}"),
            &format!("linky = {{ git = \"{}\" }}", repo.url()),
            &[],
        );
        sb.write(
            &format!("apps/app_{module}/main.colibri"),
            &format!("import linky.{module}\nfn main() {{\n    print({module}.version())\n}}\n"),
        );
        let out = sb.run(&app);
        assert_eq!(out.code, 1, "{module}: {}", out.stderr);
        assert!(
            !out.stderr.contains("TOP-SECRET") && !out.stdout.contains("OUTSIDE"),
            "{module}: {}{}",
            out.stdout,
            out.stderr
        );
        assert!(
            out.stderr.contains("not found") || out.stderr.contains("no module"),
            "{module}: {}",
            out.stderr
        );
    }
    // Nothing of the links is in the checkout at all.
    let mut stack = vec![sb.home().join("git/checkouts")];
    let mut files = Vec::new();
    while let Some(dir) = stack.pop() {
        for e in fs::read_dir(dir).unwrap().flatten() {
            if e.file_type().unwrap().is_dir() {
                stack.push(e.path());
            } else {
                files.push(e.file_name().to_string_lossy().into_owned());
            }
        }
    }
    assert!(
        !files
            .iter()
            .any(|f| f == "leak.colibri" || f == "sneaky.colibri"),
        "{files:?}"
    );
}

#[test]
fn gitattributes_filters_and_ident_do_not_run_on_a_dependency() {
    let sb = Sandbox::new("filters");
    let repo = sb.repo("attr");
    let mut files = lib_files("attr", "x", "");
    files[1].1 = "pub fn version() -> str => \"$Id$\"\n".into();
    files.push((".gitattributes".into(), "* filter=demo ident\n".into()));
    repo.commit(&files, "attributes");
    let ran = sb.path("filter-ran");
    let smudge = format!("echo ran >> {} && cat", slashes(&ran));
    let app = sb.app(
        "app",
        &format!("attr = {{ git = \"{}\" }}", repo.url()),
        &["attr"],
    );
    let out = sb.colibri_env(
        &["run", app.to_str().unwrap()],
        &[
            ("GIT_CONFIG_COUNT", "2"),
            ("GIT_CONFIG_KEY_0", "filter.demo.smudge"),
            ("GIT_CONFIG_VALUE_0", &smudge),
            ("GIT_CONFIG_KEY_1", "core.autocrlf"),
            ("GIT_CONFIG_VALUE_1", "true"),
        ],
    );
    assert_eq!(
        (out.code, out.stdout.as_str()),
        (0, "$Id$\n"),
        "{}",
        out.stderr
    );
    assert!(!ran.exists(), "the user's filter program ran");
}

#[test]
fn a_rev_is_an_object_name_never_a_branch_or_tag_of_the_same_name() {
    let sb = Sandbox::new("rev_vs_ref");
    let (repo, c1) = sb.lib_repo("greet", "one");
    let c2 = new_version(&repo, "greet", "two");
    let short = &c1[..7];
    // Upstream adds a branch and a tag called like the abbreviated rev, pointing elsewhere.
    git(&repo.dir, &["branch", short, &c2]);
    git(&repo.dir, &["tag", &c1[..9], &c2]);
    let app = sb.app(
        "app",
        &format!("greet = {{ git = \"{}\", rev = \"{short}\" }}", repo.url()),
        &["greet"],
    );
    let out = sb.run(&app);
    assert_eq!(
        (out.code, out.stdout.as_str()),
        (0, "one\n"),
        "{}",
        out.stderr
    );
    assert!(sb.lock(&app).contains(&format!("commit = \"{c1}\"")));
    // The rev may be written in capitals.
    let app = sb.app(
        "app_upper",
        &format!(
            "greet = {{ git = \"{}\", rev = \"{}\" }}",
            repo.url(),
            c1[..9].to_uppercase()
        ),
        &["greet"],
    );
    let out = sb.run(&app);
    assert_eq!(
        (out.code, out.stdout.as_str()),
        (0, "one\n"),
        "{}",
        out.stderr
    );
}

#[test]
fn a_lock_edited_to_point_a_rev_elsewhere_is_not_trusted() {
    let sb = Sandbox::new("rev_lock_edit");
    let (repo, c1) = sb.lib_repo("greet", "one");
    let c2 = new_version(&repo, "greet", "two");
    let app = sb.app(
        "app",
        &format!(
            "greet = {{ git = \"{}\", rev = \"{}\" }}",
            repo.url(),
            &c1[..8]
        ),
        &["greet"],
    );
    let path = app.to_str().unwrap();
    assert_eq!(sb.run(&app).code, 0);
    let good = sb.lock(&app);
    let evil = good.replace(&c1, &c2);
    assert_ne!(good, evil);
    fs::write(app.join("colibri.lock"), &evil).unwrap();
    let out = sb.colibri(&["run", "--locked", path]);
    assert_eq!(out.code, 1, "{}", out.stdout);
    assert!(
        out.stderr.contains("does not start with the rev"),
        "{}",
        out.stderr
    );
    assert_eq!(sb.lock(&app), evil, "--locked leaves the file alone");
    let out = sb.colibri(&["run", path]);
    assert_eq!(
        (out.code, out.stdout.as_str()),
        (0, "one\n"),
        "{}",
        out.stderr
    );
    assert!(
        out.stderr.contains("does not match its rev"),
        "{}",
        out.stderr
    );
    assert_eq!(sb.lock(&app), good);
}

/// Starts one `colibri` per app at once on the sandbox's cache and waits for all of them.
fn run_together(sb: &Sandbox, command: &str, apps: &[PathBuf]) -> Vec<Out> {
    let children: Vec<_> = apps
        .iter()
        .map(|app| {
            let mut cmd = colibri_command();
            git_env(&mut cmd)
                .env("COLIBRI_HOME", sb.home())
                .args([command, app.to_str().unwrap()])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            cmd.spawn().expect("the colibri binary runs")
        })
        .collect();
    children
        .into_iter()
        .map(|c| {
            let out = c.wait_with_output().unwrap();
            Out {
                stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
                code: out.status.code().unwrap_or(-1),
            }
        })
        .collect()
}

#[test]
fn parallel_processes_on_a_fresh_cache_all_succeed() {
    for round in 0..3 {
        let sb = Sandbox::new(&format!("parallel{round}"));
        let (r1, _) = sb.lib_repo("one", "1");
        let (r2, _) = sb.lib_repo("two", "2");
        let apps: Vec<PathBuf> = (0..8)
            .map(|i| {
                sb.app(
                    &format!("app{i}"),
                    &format!(
                        "one = {{ git = \"{}\" }}\ntwo = {{ git = \"{}\" }}",
                        r1.url(),
                        r2.url()
                    ),
                    &["one", "two"],
                )
            })
            .collect();
        for (i, out) in run_together(&sb, "run", &apps).iter().enumerate() {
            assert_eq!(
                (out.code, out.stdout.as_str()),
                (0, "1\n2\n"),
                "round {round}, process {i}: {}",
                out.stderr
            );
        }
        // And again on the warm cache.
        for out in run_together(&sb, "check", &apps) {
            assert_eq!(out.code, 0, "{}", out.stderr);
        }
    }
}

/// Sets the modification time of a folder. Windows refuses to open a folder as a file
/// unless asked for backup semantics, and needs the right to write attributes (not data).
fn set_modified(dir: &Path, when: std::time::SystemTime) {
    #[cfg(windows)]
    let file = {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_WRITE_ATTRIBUTES: u32 = 0x100;
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        fs::OpenOptions::new()
            .access_mode(FILE_WRITE_ATTRIBUTES)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(dir)
            .unwrap()
    };
    #[cfg(not(windows))]
    let file = fs::File::open(dir).unwrap();
    file.set_modified(when).unwrap();
}

/// `path` with the separators of the system, as the tool prints a path it built from the
/// pieces (`home\git\db\x.git` on Windows) whatever way the test joined it
/// (`home\git/db\x.git`).
fn native(path: &Path) -> String {
    path.components().collect::<PathBuf>().display().to_string()
}

/// The single folder under `dir` whose name ends in `suffix`.
fn only_entry(dir: &Path, suffix: &str) -> PathBuf {
    let found: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.to_string_lossy().ends_with(suffix))
        .collect();
    assert_eq!(found.len(), 1, "{dir:?}: {found:?}");
    found[0].clone()
}

#[test]
fn a_damaged_or_interrupted_cache_is_made_again() {
    let sb = Sandbox::new("recover");
    let (repo, _) = sb.lib_repo("greet", "one");
    let app = sb.app(
        "app",
        &format!("greet = {{ git = \"{}\" }}", repo.url()),
        &["greet"],
    );
    assert_eq!(sb.run(&app).stdout, "one\n");
    let db = only_entry(&sb.home().join("git/db"), ".git");
    // A crash between `mkdir` and `git init` left an empty folder, and no checkout.
    fs::remove_dir_all(&db).unwrap();
    fs::create_dir(&db).unwrap();
    fs::remove_dir_all(sb.home().join("git/checkouts")).unwrap();
    let out = sb.run(&app);
    assert_eq!(
        (out.code, out.stdout.as_str()),
        (0, "one\n"),
        "{}",
        out.stderr
    );

    // A checkout folder without its completion marker is not trusted, and is written again.
    let key = only_entry(&sb.home().join("git/checkouts"), "");
    let commit_dir = fs::read_dir(&key).unwrap().flatten().next().unwrap().path();
    fs::remove_file(commit_dir.join(".colibri-complete")).unwrap();
    fs::remove_file(commit_dir.join("lib.colibri")).unwrap();
    let out = sb.colibri(&["run", "--offline", app.to_str().unwrap()]);
    assert_eq!(
        (out.code, out.stdout.as_str()),
        (0, "one\n"),
        "{}",
        out.stderr
    );
    assert!(commit_dir.join(".colibri-complete").is_file());

    // Temporary folders of a run that died: old ones go, a fresh one stays.
    let old = key.join(".dead.tmp1");
    let fresh = key.join(".live.tmp2");
    fs::create_dir_all(old.join("x")).unwrap();
    fs::create_dir_all(&fresh).unwrap();
    let two_days = std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 86400);
    set_modified(&old, two_days);
    new_version(&repo, "greet", "two");
    let out = sb.colibri(&["update", "--path", app.to_str().unwrap()]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(!old.exists() && fresh.exists());
}

#[test]
fn a_failed_fetch_keeps_only_the_end_of_gits_complaint() {
    let sb = Sandbox::new("short_errors");
    let app = sb.app(
        "app",
        &format!(
            "greet = {{ git = \"{}\" }}",
            slashes(&sb.path("repos/missing"))
        ),
        &["greet"],
    );
    let out = sb.colibri(&["check", app.to_str().unwrap()]);
    assert_eq!(out.code, 1);
    assert!(out.stderr.len() < 2000, "{}", out.stderr.len());
}

#[test]
fn remote_helpers_and_other_transports_are_refused_not_run() {
    let sb = Sandbox::new("transports");
    let proof = sb.path("helper-ran");
    for (name, url, want) in [
        (
            "ext",
            format!("ext::sh -c touch$IFS{}", slashes(&proof)),
            "remote helper",
        ),
        ("fd", "fd::17".to_string(), "remote helper"),
        (
            "http",
            "http://example.com/r.git".to_string(),
            "transport `http`",
        ),
        (
            "ftp",
            "ftp://example.com/r.git".to_string(),
            "transport `ftp`",
        ),
    ] {
        let app = sb.app(name, &format!("greet = {{ git = \"{url}\" }}"), &["greet"]);
        let out = sb.colibri(&["check", app.to_str().unwrap()]);
        assert_eq!(out.code, 1, "{name}: {}", out.stderr);
        assert!(out.stderr.contains(want), "{name}: {}", out.stderr);
        assert!(out.stderr.contains("not allowed"), "{name}: {}", out.stderr);
    }
    assert!(!proof.exists());
}

#[test]
fn git_variables_from_a_hook_do_not_redirect_the_cache() {
    // Started from a git hook, `colibri` inherits `GIT_DIR` and friends for the user's own
    // repository; the objects of a dependency must still land in the cache.
    let sb = Sandbox::new("hook_env");
    let (repo, _) = sb.lib_repo("greet", "one");
    let decoy = sb.path("decoy.git");
    fs::create_dir_all(&decoy).unwrap();
    git(&decoy, &["init", "--bare", "--quiet"]);
    let count = |dir: &Path| {
        let mut n = 0;
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in fs::read_dir(d).unwrap().flatten() {
                if e.file_type().unwrap().is_dir() {
                    stack.push(e.path());
                } else {
                    n += 1;
                }
            }
        }
        n
    };
    let before = count(&decoy);
    let app = sb.app(
        "app",
        &format!("greet = {{ git = \"{}\" }}", repo.url()),
        &["greet"],
    );
    let decoy_str = decoy.to_str().unwrap().to_string();
    let objects = decoy.join("objects").to_str().unwrap().to_string();
    let work = sb.path("decoy-work");
    fs::create_dir_all(&work).unwrap();
    let out = sb.colibri_env(
        &["run", app.to_str().unwrap()],
        &[
            ("GIT_DIR", &decoy_str),
            ("GIT_OBJECT_DIRECTORY", &objects),
            ("GIT_WORK_TREE", work.to_str().unwrap()),
            ("GIT_INDEX_FILE", sb.path("decoy-index").to_str().unwrap()),
            ("GIT_NAMESPACE", "decoy"),
            ("GIT_COMMON_DIR", &decoy_str),
        ],
    );
    assert_eq!(
        (out.code, out.stdout.as_str()),
        (0, "one\n"),
        "{}",
        out.stderr
    );
    assert_eq!(count(&decoy), before, "the decoy repository was written to");
    assert_eq!(fs::read_dir(&work).unwrap().count(), 0);
    assert!(!sb.path("decoy-index").exists());
}

#[test]
fn only_urls_scp_addresses_and_absolute_paths_are_git_urls() {
    let sb = Sandbox::new("url_forms");
    for url in ["r4", "sub/repo", ".git", "..\\\\x", "host:path", "./x"] {
        let app = sb.app("app", &format!("greet = {{ git = \"{url}\" }}"), &["greet"]);
        let out = sb.colibri(&["check", app.to_str().unwrap()]);
        assert_eq!(out.code, 1, "{url}");
        assert!(
            out.stderr.contains("not a git url") || out.stderr.contains("relative path"),
            "{url}: {}",
            out.stderr
        );
        assert!(!sb.path("apps/app/colibri.lock").exists());
    }
}

#[test]
fn a_remote_package_cannot_name_a_local_repository() {
    let sb = Sandbox::new("remote_to_local");
    // The user's private repository, and a package that was published at an https url
    // (here the url is rewritten to a local folder, as `url.<base>.insteadOf` does).
    let (private, _) = sb.lib_repo("private", "SECRET");
    let outer = sb.repo("outer");
    let mut files = lib_files(
        "outer",
        "o",
        &format!(
            "\n[dependencies]\nprivate = {{ git = \"{}\" }}\n",
            private.file_url()
        ),
    );
    files[1].1 = "pub fn version() -> str => \"o\"\n".into();
    outer.commit(&files, "outer");
    let key = format!("url.{}.insteadOf", outer.url());
    let rewrite = [
        ("GIT_CONFIG_COUNT", "1"),
        ("GIT_CONFIG_KEY_0", key.as_str()),
        ("GIT_CONFIG_VALUE_0", "https://example.test/outer.git"),
    ];
    let remote = sb.app(
        "remote",
        "outer = { git = \"https://example.test/outer.git\" }",
        &["outer"],
    );
    let out = sb.colibri_env(&["run", remote.to_str().unwrap()], &rewrite);
    assert_eq!(out.code, 1, "{}{}", out.stdout, out.stderr);
    for want in [
        "`outer`",
        "https://example.test/outer.git",
        "`private`",
        "local",
    ] {
        assert!(out.stderr.contains(want), "{want}: {}", out.stderr);
    }
    assert!(!remote.join("colibri.lock").exists());
    // The same package fetched from a local url may use local repositories.
    let local = sb.app(
        "local",
        &format!("outer = {{ git = \"{}\" }}", outer.url()),
        &["outer"],
    );
    let out = sb.run(&local);
    assert_eq!(
        (out.code, out.stdout.as_str()),
        (0, "o\n"),
        "{}",
        out.stderr
    );
}

#[test]
fn a_branch_or_tag_deleted_upstream_is_gone_from_the_cache_too() {
    let sb = Sandbox::new("prune");
    let (repo, _) = sb.lib_repo("greet", "one");
    repo.branch("dev");
    repo.branch("feature");
    repo.tag("v1");
    let by_branch = sb.app(
        "by_branch",
        &format!("greet = {{ git = \"{}\", branch = \"dev\" }}", repo.url()),
        &["greet"],
    );
    let by_tag = sb.app(
        "by_tag",
        &format!("greet = {{ git = \"{}\", tag = \"v1\" }}", repo.url()),
        &["greet"],
    );
    assert_eq!(sb.run(&by_branch).code, 0);
    assert_eq!(sb.run(&by_tag).code, 0);
    git(&repo.dir, &["branch", "-D", "dev"]);
    git(&repo.dir, &["tag", "-d", "v1"]);
    // A branch `feature` becoming `feature/x` is a ref conflict that only pruning resolves.
    git(&repo.dir, &["branch", "-D", "feature"]);
    git(&repo.dir, &["branch", "feature/x"]);
    let out = sb.colibri(&["update", "--path", by_branch.to_str().unwrap()]);
    assert_eq!(out.code, 1, "{}", out.stderr);
    assert!(out.stderr.contains("has no branch `dev`"), "{}", out.stderr);
    let out = sb.colibri(&["update", "--path", by_tag.to_str().unwrap()]);
    assert_eq!(out.code, 1, "{}", out.stderr);
    assert!(out.stderr.contains("has no tag `v1`"), "{}", out.stderr);
    // The new branch is there.
    let by_new = sb.app(
        "by_new",
        &format!(
            "greet = {{ git = \"{}\", branch = \"feature/x\" }}",
            repo.url()
        ),
        &["greet"],
    );
    assert_eq!(sb.run(&by_new).code, 0);
}

#[test]
fn a_database_holding_another_url_is_never_shared() {
    // Two urls with one cache key are simulated by a database folder that says it holds
    // another url: the tool must not fetch into it, but use the next name.
    let sb = Sandbox::new("collision");
    let (repo, _) = sb.lib_repo("greet", "one");
    let app = sb.app(
        "app",
        &format!("greet = {{ git = \"{}\" }}", repo.url()),
        &["greet"],
    );
    assert_eq!(sb.run(&app).code, 0);
    let dbs = sb.home().join("git/db");
    let first = only_entry(&dbs, ".git");
    assert_eq!(
        fs::read_to_string(first.join("colibri-url")).unwrap(),
        repo.url()
    );
    fs::write(first.join("colibri-url"), "https://other.example/greet.git").unwrap();
    let refs_before = git(&first, &["for-each-ref"]);
    new_version(&repo, "greet", "two");
    let out = sb.colibri(&["update", "--path", app.to_str().unwrap()]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(sb.run(&app).stdout, "two\n");
    assert_eq!(git(&first, &["for-each-ref"]), refs_before);
    let second = dbs.join(format!(
        "{}-1.git",
        first
            .file_name()
            .unwrap()
            .to_string_lossy()
            .trim_end_matches(".git")
    ));
    assert_eq!(
        fs::read_to_string(second.join("colibri-url")).unwrap(),
        repo.url()
    );
}

#[test]
fn one_commit_asked_for_by_a_tag_and_by_a_branch_is_one_package() {
    let sb = Sandbox::new("diamond_specs");
    let (shared, c1) = sb.lib_repo("shared", "s");
    shared.tag("v1");
    let mk = |name: &str, spec: &str| {
        let r = sb.repo(name);
        let mut files = lib_files(
            name,
            name,
            &format!(
                "\n[dependencies]\nshared = {{ git = \"{}\", {spec} }}\n",
                shared.url()
            ),
        );
        files[1].1 = format!("pub fn version() -> str => \"{name}\"\n");
        r.commit(&files, name);
        r
    };
    let ra = mk("alpha", "tag = \"v1\"");
    let rb = mk("beta", "branch = \"main\"");
    let app = sb.app(
        "app",
        &format!(
            "alpha = {{ git = \"{}\" }}\nbeta = {{ git = \"{}\" }}",
            ra.url(),
            rb.url()
        ),
        &["alpha", "beta"],
    );
    let path = app.to_str().unwrap();
    let out = sb.run(&app);
    assert_eq!(
        (out.code, out.stdout.as_str()),
        (0, "alpha\nbeta\n"),
        "{}",
        out.stderr
    );
    assert!(sb.lock(&app).contains(&c1));
    assert_eq!(sb.lock(&app).matches("[git.shared]").count(), 1);
    for cmd in ["check", "run"] {
        assert_eq!(sb.colibri(&[cmd, "--locked", path]).code, 0, "{cmd}");
    }
    // Once the branch moves on, the two really differ, and the message says who asked for what.
    new_version(&shared, "shared", "s2");
    let out = sb.colibri(&["update", "--path", path]);
    assert_eq!(out.code, 1, "{}", out.stderr);
    assert!(
        out.stderr.contains("`alpha` asks for") && out.stderr.contains("tag `v1`"),
        "{}",
        out.stderr
    );
    assert!(
        out.stderr.contains("`beta` asks for") && out.stderr.contains("branch `main`"),
        "{}",
        out.stderr
    );
}

#[test]
fn a_lock_that_cannot_be_read_is_an_error_not_an_absent_lock() {
    let sb = Sandbox::new("bad_lock");
    let (repo, _) = sb.lib_repo("greet", "one");
    let app = sb.app(
        "app",
        &format!("greet = {{ git = \"{}\" }}", repo.url()),
        &["greet"],
    );
    let path = app.to_str().unwrap();
    let lock = app.join("colibri.lock");
    for command in [vec!["check", path], vec!["update", "--path", path]] {
        // A syntax error: the line and the way out.
        let text = "[git.greet]\nurl = \"x\"\nbogus\n";
        fs::write(&lock, text).unwrap();
        let out = sb.colibri(&command);
        assert_eq!(out.code, 1, "{command:?}: {}", out.stderr);
        assert!(out.stderr.contains("line 3"), "{}", out.stderr);
        assert!(
            out.stderr
                .contains("delete `colibri.lock` and run `colibri update`"),
            "{}",
            out.stderr
        );
        assert_eq!(fs::read_to_string(&lock).unwrap(), text, "left alone");
        // Not UTF-8.
        fs::write(&lock, [0xff, 0xfe, b'x']).unwrap();
        let out = sb.colibri(&command);
        assert_eq!(out.code, 1, "{command:?}: {}", out.stderr);
        assert!(out.stderr.contains("not valid UTF-8"), "{}", out.stderr);
        assert_eq!(fs::read(&lock).unwrap(), [0xff, 0xfe, b'x']);
    }
    // The loader (the editor's side) gives the same way out.
    fs::write(&lock, "[git.greet]\nurl = \"x\"\nbogus\n").unwrap();
    let report =
        colibri_runtime::check_package_with(&app, &colibri_runtime::CheckConfig::default())
            .unwrap()
            .render(false);
    assert!(
        report.contains("delete `colibri.lock` and run `colibri update`"),
        "{report}"
    );
    // Unreadable (here: a folder where the file should be).
    fs::remove_file(&lock).unwrap();
    fs::create_dir(&lock).unwrap();
    let out = sb.colibri(&["check", path]);
    assert_eq!(out.code, 1);
    assert!(out.stderr.contains("cannot read"), "{}", out.stderr);
    assert!(lock.is_dir());
    fs::remove_dir(&lock).unwrap();
    assert_eq!(sb.colibri(&["check", path]).code, 0);
}

#[test]
fn offline_without_git_says_git_was_not_found() {
    let sb = Sandbox::new("offline_no_git");
    let (repo, _) = sb.lib_repo("greet", "one");
    let app = sb.app(
        "app",
        &format!("greet = {{ git = \"{}\" }}", repo.url()),
        &["greet"],
    );
    let empty = sb.path("empty_path");
    fs::create_dir_all(&empty).unwrap();
    let out = sb.colibri_env(
        &["check", "--offline", app.to_str().unwrap()],
        &[("PATH", empty.to_str().unwrap())],
    );
    assert_eq!(out.code, 1);
    assert!(
        out.stderr.contains("the `git` program was not found"),
        "{}",
        out.stderr
    );
    assert!(!out.stderr.contains("not in the cache"), "{}", out.stderr);
}

#[test]
fn credentials_in_a_url_are_refused_and_never_printed_or_written() {
    let sb = Sandbox::new("credentials");
    for (n, url) in [
        "https://user:s3cret@example.invalid:9/r.git",
        "https://s3cret@example.invalid:9/r.git",
        "HTTPS://user:s3cret@example.invalid:9/r.git",
        "ssh://git:s3cret@example.invalid:9/r.git",
    ]
    .iter()
    .enumerate()
    {
        let app = sb.app(
            &format!("app{n}"),
            &format!("greet = {{ git = \"{url}\" }}"),
            &["greet"],
        );
        for cmd in ["check", "run"] {
            let out = sb.colibri(&[cmd, app.to_str().unwrap()]);
            assert_eq!(out.code, 1, "{url}: {cmd}");
            let all = format!("{}{}", out.stdout, out.stderr);
            assert!(!all.contains("s3cret"), "{url}: {cmd}: {all}");
            assert!(
                all.contains("read as a user name and password")
                    && all.contains("credential helper"),
                "{url}: {cmd}: {all}"
            );
            assert!(!app.join("colibri.lock").exists(), "{url}: {cmd}");
        }
        // `update` reads no url it cannot use: nothing is fetched, locked or printed.
        let out = sb.colibri(&["update", "--path", app.to_str().unwrap()]);
        assert!(
            !format!("{}{}", out.stdout, out.stderr).contains("s3cret"),
            "{url}: update"
        );
        assert!(!app.join("colibri.lock").exists(), "{url}: update");
    }
    // The cache was never touched: nothing was fetched for them.
    assert!(!sb.home().join("git").exists());
    // A user name for ssh is fine (it fails later, at the connection, never on the url).
    let app = sb.app(
        "ssh_user",
        "greet = { git = \"ssh://git@example.invalid:9/r.git\" }",
        &["greet"],
    );
    // `--offline`: no connection is made, whatever is listening at the address.
    let out = sb.colibri(&["check", "--offline", app.to_str().unwrap()]);
    assert!(
        !out.stderr.contains("credential helper") && !out.stderr.contains("not allowed"),
        "{}",
        out.stderr
    );
}

/// Every file under `dir`, as text, for a search that must find nothing.
fn tree_text(dir: &Path, out: &mut String) {
    // The checkouts hold the files of a repository as they are, a manifest with its own text.
    if dir.file_name().is_some_and(|n| n == "checkouts") {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let path = e.path();
        out.push_str(&path.display().to_string());
        out.push('\n');
        match e.file_type() {
            Ok(t) if t.is_dir() => tree_text(&path, out),
            Ok(t) if t.is_file() => {
                out.push_str(&String::from_utf8_lossy(
                    &fs::read(&path).unwrap_or_default(),
                ));
                out.push('\n');
            }
            _ => {}
        }
    }
}

#[test]
fn no_way_of_writing_a_secret_in_a_url_gets_it_printed_or_stored() {
    let sb = Sandbox::new("secret_forms");
    for (n, url) in [
        "https://u:p#s3cret@example.invalid:9/r.git",
        "https://u:p?s3cret@example.invalid:9/r.git",
        "https://u:pa/s3cret@example.invalid:9/r.git",
        "https://u:p@s3cret@example.invalid:9/r.git",
        "https://u:p%40s3cret@example.invalid:9/r.git",
        "https://u%3As3cret@example.invalid:9/r.git",
        "HTTPS://u:s3cret@example.invalid:9/r.git",
        "Git://u:s3cret@example.invalid:9/r.git",
        "file://u:s3cret@example.invalid/r.git",
        "ssh://u:p#s3cret@example.invalid:9/r.git",
        "ssh://u:p@s3cret@example.invalid:9/r.git",
        "u:s3cret@example.invalid:r.git",
        "u:p@s3cret@example.invalid:r.git",
    ]
    .iter()
    .enumerate()
    {
        let app = sb.app(
            &format!("form{n}"),
            &format!("greet = {{ git = \"{url}\" }}"),
            &["greet"],
        );
        for args in [
            &["check"][..],
            &["run"][..],
            &["update", "--path"][..],
            &["check", "--offline"][..],
            &["check", "--locked"][..],
        ] {
            let mut full = args.to_vec();
            full.push(app.to_str().unwrap());
            let out = sb.colibri(&full);
            let all = format!("{}{}", out.stdout, out.stderr);
            assert_eq!(out.code, 1, "{url}: {args:?}: {all}");
            assert!(!all.contains("s3cret"), "{url}: {args:?}: {all}");
            assert!(all.contains("credential helper"), "{url}: {args:?}: {all}");
            assert!(!app.join("colibri.lock").exists(), "{url}: {args:?}");
        }
    }
    let mut files = String::new();
    tree_text(&sb.home(), &mut files);
    assert!(!files.contains("s3cret"), "{files}");
}

#[test]
fn a_secret_in_a_dependencys_own_manifest_is_refused_and_never_stored() {
    let sb = Sandbox::new("secret_in_dependency");
    let repo = sb.repo("lib");
    repo.commit(
        &lib_files(
            "lib",
            "one",
            "\n[dependencies]\nbad = { git = \"https://u:p#s3cret@example.invalid:9/b.git\" }\n",
        ),
        "first",
    );
    let app = sb.app(
        "app",
        &format!("lib = {{ git = \"{}\" }}", repo.url()),
        &["lib"],
    );
    for args in [&["run"][..], &["check"][..], &["update"][..]] {
        let mut full = args.to_vec();
        if args[0] == "update" {
            full.push("--path");
        }
        full.push(app.to_str().unwrap());
        let out = sb.colibri(&full);
        let all = format!("{}{}", out.stdout, out.stderr);
        assert_eq!(out.code, 1, "{args:?}: {all}");
        assert!(!all.contains("s3cret"), "{args:?}: {all}");
        assert!(all.contains("credential helper"), "{args:?}: {all}");
        assert!(
            all.contains("read as a user name and password"),
            "{args:?}: {all}"
        );
    }
    // `update` refused the dependency instead of writing a lock that leaves it out.
    let _ = fs::remove_file(app.join("colibri.lock"));
    let out = sb.colibri(&["update", "--path", app.to_str().unwrap()]);
    assert_eq!(out.code, 1, "{}", out.stderr);
    assert!(!app.join("colibri.lock").exists());
    let mut files = String::new();
    tree_text(&sb.home(), &mut files);
    tree_text(&app, &mut files);
    assert!(!files.contains("s3cret"), "{files}");
}

/// Ways of writing a url with a secret (`s3cret`) in it that break a split at spaces and
/// quotes: each is the text of a TOML value, as written after `git = ` or `url = `.
fn quoted_secret_literals() -> Vec<String> {
    let tail = "example.invalid:9/r.git";
    vec![
        format!("\"https://u:ab'cds3cret@{tail}\""),
        format!("\"https://u:x\\\"ys3cret@{tail}\""),
        format!("\"https://u:pa ss3cret@{tail}\""),
        "'https://u:p\"qs3cret@example.invalid:99/r.git'".to_string(),
        format!("\"https://u:s3cret\\u0040{tail}\""),
        format!("\"https://u:s3cret\\U00000040{tail}\""),
        format!("\"https://u:p%40s3cret@{tail}\""),
        format!("\"https://u:pa\tss3cret@{tail}\""),
        format!("\"https://u:pa\\tss3cret@{tail}\""),
        format!("\"https://u:p@s3cret@{tail}\""),
        format!("\"https://u:ab`s3cret@{tail}\""),
        "\"u:a'bs3cret@example.invalid:r.git\"".to_string(),
        format!("\"ssh://u:a b'c\\\"ds3cret@{tail}\""),
    ]
}

/// What the commands that read a project printed, and what they wrote under `COLIBRI_HOME`,
/// holds no secret.
fn assert_nothing_leaks(sb: &Sandbox, app: &Path, what: &str, skip_lock: bool) {
    for args in [
        &["check"][..],
        &["run"][..],
        &["check", "--locked"][..],
        &["check", "--offline"][..],
        &["update", "--path"][..],
    ] {
        let mut full = args.to_vec();
        full.push(app.to_str().unwrap());
        let out = sb.colibri(&full);
        let all = format!("{}{}", out.stdout, out.stderr);
        assert!(!all.contains("s3cret"), "{what}: {args:?}: {all}");
    }
    let mut files = String::new();
    tree_text(&sb.home(), &mut files);
    if !skip_lock && let Ok(lock) = fs::read_to_string(app.join("colibri.lock")) {
        files.push_str(&lock);
    }
    assert!(!files.contains("s3cret"), "{what}: {files}");
}

#[test]
fn a_quoted_manifest_line_never_shows_a_password_with_a_quote_or_a_space() {
    let sb = Sandbox::new("quoted_secret_manifest");
    for (n, lit) in quoted_secret_literals().iter().enumerate() {
        // The value parses (or the parser rejects an escape in it) ...
        let app = sb.app(&format!("m{n}"), &format!("g = {{ git = {lit} }}"), &[]);
        let out = sb.colibri(&["check", app.to_str().unwrap()]);
        let all = format!("{}{}", out.stdout, out.stderr);
        assert_eq!(out.code, 1, "{lit}: {all}");
        assert!(
            all.contains("credential") || all.contains("escape"),
            "{lit}: {all}"
        );
        assert_nothing_leaks(&sb, &app, lit, false);
        // ... or the manifest is cut short, so it does not parse at all.
        let app = sb.app(&format!("b{n}"), &format!("g = {{ git = {lit} "), &[]);
        let out = sb.colibri(&["check", app.to_str().unwrap()]);
        assert_eq!(out.code, 1, "{lit}: {}", out.stderr);
        assert_nothing_leaks(&sb, &app, lit, false);
        // The same in a table of its own, after a line that is wrong.
        let app = sb.app(
            &format!("t{n}"),
            &format!("g = {{ git = {lit} }}\nx = 5"),
            &[],
        );
        assert_nothing_leaks(&sb, &app, lit, false);
    }
}

#[test]
fn a_dependencys_manifest_line_never_shows_a_password_with_a_quote_or_a_space() {
    let sb = Sandbox::new("quoted_secret_dependency");
    for (n, lit) in quoted_secret_literals().iter().enumerate() {
        for (k, broken) in [false, true].into_iter().enumerate() {
            let repo = sb.repo(&format!("lib{n}_{k}"));
            let line = if broken {
                format!("bad = {{ git = {lit} ")
            } else {
                format!("bad = {{ git = {lit} }}")
            };
            repo.commit(
                &lib_files("lib", "one", &format!("\n[dependencies]\n{line}\n")),
                "first",
            );
            let app = sb.app(
                &format!("d{n}_{k}"),
                &format!("lib = {{ git = \"{}\" }}", repo.url()),
                &["lib"],
            );
            let out = sb.colibri(&["check", app.to_str().unwrap()]);
            assert_eq!(out.code, 1, "{lit}: {}{}", out.stdout, out.stderr);
            assert_nothing_leaks(&sb, &app, lit, false);
            // The fetched copy of the repository holds the manifest as it is written: that
            // is the repository's own text, not something the tool stored.
        }
    }
}

#[test]
fn a_hand_edited_lock_never_shows_a_password_with_a_quote_or_a_space() {
    let sb = Sandbox::new("quoted_secret_lock");
    let commit = "5f2a9c1e0b7d4a6c8e3f1a2b4c5d6e7f8091a2b3";
    for (n, lit) in quoted_secret_literals().iter().enumerate() {
        for (k, lock) in [
            format!("[git.greet]\nurl = {lit}\ncommit = \"{commit}\"\n"),
            format!("[git.greet]\nurl = {lit} junk\ncommit = \"{commit}\"\n"),
            format!("[git.greet]\ncommit = \"{commit}\"\nurl = {lit}"),
        ]
        .iter()
        .enumerate()
        {
            let app = sb.app(
                &format!("l{n}_{k}"),
                "greet = { git = \"file:///colibri-no-such-repository/r.git\" }",
                &["greet"],
            );
            fs::write(app.join("colibri.lock"), lock).unwrap();
            for args in [
                &["check", "--locked"][..],
                &["check", "--offline"][..],
                &["check"][..],
                &["run"][..],
            ] {
                let mut full = args.to_vec();
                full.push(app.to_str().unwrap());
                let out = sb.colibri(&full);
                let all = format!("{}{}", out.stdout, out.stderr);
                assert!(!all.contains("s3cret"), "{lit}: {args:?}: {all}");
            }
            let mut files = String::new();
            tree_text(&sb.home(), &mut files);
            assert!(!files.contains("s3cret"), "{lit}: {files}");
            // `update` writes a new lock from the manifest, never from the old text (a lock
            // it refuses to read stays as the person wrote it).
            let out = sb.colibri(&["update", "--path", app.to_str().unwrap()]);
            let all = format!("{}{}", out.stdout, out.stderr);
            assert!(!all.contains("s3cret"), "{lit}: update: {all}");
            if let Ok(new) = fs::read_to_string(app.join("colibri.lock"))
                && new != *lock
            {
                assert!(!new.contains("s3cret"), "{lit}: {new}");
            }
        }
    }
}

#[test]
fn an_ssh_or_scp_url_that_looks_like_an_option_is_refused_before_git_runs() {
    let sb = Sandbox::new("option_urls");
    for (n, url) in [
        "ssh://-oProxyCommand=x/r",
        "u@h:-oProxyCommand=x",
        "ssh://git@-oProxyCommand=x/r",
    ]
    .iter()
    .enumerate()
    {
        let app = sb.app(&format!("o{n}"), &format!("g = {{ git = \"{url}\" }}"), &[]);
        let out = sb.colibri(&["check", app.to_str().unwrap()]);
        let all = format!("{}{}", out.stdout, out.stderr);
        assert_eq!(out.code, 1, "{url}: {all}");
        assert!(all.contains("starts with `-`"), "{url}: {all}");
    }
}

#[test]
fn update_says_when_a_manifest_does_not_parse_and_writes_no_lock() {
    let sb = Sandbox::new("update_bad_manifest");
    let (repo, _) = sb.lib_repo("greet", "one");
    let app = sb.app(
        "app",
        &format!("greet = {{ git = \"{}\" }}", repo.url()),
        &["greet"],
    );
    sb.write("apps/app/colibri.toml", "[package\nname = \"app\"\n");
    let out = sb.colibri(&["update", "--path", app.to_str().unwrap()]);
    assert_eq!(out.code, 1, "{}{}", out.stdout, out.stderr);
    assert!(
        out.stderr.contains("colibri.toml") && out.stderr.contains("error"),
        "{}",
        out.stderr
    );
    assert!(!app.join("colibri.lock").exists());
    // The manifest of a dependency that does not parse stops the update as well.
    let bad = sb.repo("bad");
    let mut files = lib_files("bad", "b", "");
    files[0].1 = "[package\nname = \"bad\"\n".into();
    bad.commit(&files, "broken");
    sb.app(
        "app",
        &format!("bad = {{ git = \"{}\" }}", bad.url()),
        &["bad"],
    );
    let out = sb.colibri(&["update", "--path", app.to_str().unwrap()]);
    assert_eq!(out.code, 1, "{}{}", out.stdout, out.stderr);
    assert!(out.stderr.contains("colibri.toml"), "{}", out.stderr);
    assert!(!app.join("colibri.lock").exists());
}

#[test]
fn names_only_windows_dislikes_stop_a_checkout_there_and_nowhere_else() {
    let sb = Sandbox::new("windows_names");
    let repo = sb.repo("greet");
    repo.commit(&lib_files("greet", "one", ""), "first");
    // Git stores these names everywhere; Windows cannot write them. They are put in the index
    // directly, since the working folder of this test may be on Windows too.
    for name in [
        "docs/aux.md",
        "src/con.c",
        "notes/2021-01-01T10:00.md",
        "a.b.",
    ] {
        repo.stage_file(name, "x\n");
    }
    git(&repo.dir, &["commit", "--quiet", "-m", "names"]);
    let app = sb.app(
        "app",
        &format!("greet = {{ git = \"{}\" }}", repo.url()),
        &["greet"],
    );
    let out = sb.run(&app);
    if cfg!(windows) {
        assert_eq!(out.code, 1, "{}{}", out.stdout, out.stderr);
        assert!(
            out.stderr.contains("cannot be checked out on Windows"),
            "{}",
            out.stderr
        );
    } else {
        assert_eq!(
            (out.code, out.stdout.as_str()),
            (0, "one\n"),
            "{}",
            out.stderr
        );
    }
}

#[test]
fn an_old_lock_with_a_secret_in_it_is_not_printed() {
    let sb = Sandbox::new("old_lock_secret");
    let app = sb.app(
        "app",
        "greet = { git = \"file:///colibri-no-such-repository/r.git\" }",
        &["greet"],
    );
    fs::write(
        app.join("colibri.lock"),
        "[git.greet]\nurl = \"https://user:s3cret@example.invalid:9/r.git\"\ncommit = \"5f2a9c1e0b7d4a6c8e3f1a2b4c5d6e7f8091a2b3\"\n",
    )
    .unwrap();
    for args in [
        &["check", "--locked"][..],
        &["check", "--offline"][..],
        &["check"][..],
    ] {
        let mut full = args.to_vec();
        full.push(app.to_str().unwrap());
        let out = sb.colibri(&full);
        let all = format!("{}{}", out.stdout, out.stderr);
        assert!(!all.contains("s3cret"), "{args:?}: {all}");
    }
}

#[test]
fn an_upper_case_scheme_is_normalised_in_the_lock_and_works() {
    let sb = Sandbox::new("upper_scheme");
    let (repo, c1) = sb.lib_repo("greet", "one");
    let app = sb.app(
        "app",
        &format!(
            "greet = {{ git = \"FILE{}\" }}",
            &repo.file_url()["file".len()..]
        ),
        &["greet"],
    );
    let out = sb.run(&app);
    assert_eq!(
        (out.code, out.stdout.as_str()),
        (0, "one\n"),
        "{}",
        out.stderr
    );
    let lock = sb.lock(&app);
    assert!(lock.contains(&c1));
    assert!(
        lock.contains(&format!("url = \"{}\"", repo.file_url())),
        "{lock}"
    );
    assert!(!lock.contains("FILE://"), "{lock}");
    // The lower-case spelling of the same url is the same dependency.
    sb.app(
        "app",
        &format!("greet = {{ git = \"{}\" }}", repo.file_url()),
        &["greet"],
    );
    let out = sb.colibri(&["check", "--locked", app.to_str().unwrap()]);
    assert_eq!(out.code, 0, "{}", out.stderr);
}

#[test]
fn a_second_process_says_it_is_waiting_for_the_lock() {
    use std::io::Read;
    use std::process::Stdio;
    let sb = Sandbox::new("lock_wait");
    let (repo, _) = sb.lib_repo("greet", "one");
    let app = sb.app(
        "app",
        &format!("greet = {{ git = \"{}\" }}", repo.url()),
        &["greet"],
    );
    assert_eq!(sb.run(&app).stdout, "one\n");
    // Make the next run need the lock: no checkout any more.
    fs::remove_dir_all(sb.home().join("git/checkouts")).unwrap();
    let lock_path = only_entry(&sb.home().join("git/db"), ".lock");
    let held = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock_path)
        .unwrap();
    held.lock().unwrap();
    let mut cmd = colibri_command();
    git_env(&mut cmd)
        .env("COLIBRI_HOME", sb.home())
        .args(["run", app.to_str().unwrap()])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1500));
    assert!(child.try_wait().unwrap().is_none(), "it should be waiting");
    held.unlock().unwrap();
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    let mut stdout = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut stdout)
        .unwrap();
    assert!(child.wait().unwrap().success(), "{stderr}");
    assert_eq!(stdout, "one\n");
    assert!(
        stderr.contains("waiting for another colibri process to finish with `greet-")
            && stderr.contains(&native(&lock_path)),
        "{stderr}"
    );
}

#[test]
fn a_damaged_database_says_where_to_delete_it() {
    let sb = Sandbox::new("damaged_db");
    let (repo, _) = sb.lib_repo("greet", "one");
    let app = sb.app(
        "app",
        &format!("greet = {{ git = \"{}\" }}", repo.url()),
        &["greet"],
    );
    assert_eq!(sb.run(&app).stdout, "one\n");
    let db = only_entry(&sb.home().join("git/db"), ".git");
    // Every object file emptied, as after a full disk.
    let mut stack = vec![db.join("objects")];
    while let Some(dir) = stack.pop() {
        for e in fs::read_dir(dir).unwrap().flatten() {
            let path = e.path();
            if path.is_dir() {
                stack.push(path);
            } else if path
                .parent()
                .is_some_and(|p| p.file_name().is_some_and(|n| n.len() == 2))
            {
                let mut perms = fs::metadata(&path).unwrap().permissions();
                #[allow(clippy::permissions_set_readonly_false)]
                perms.set_readonly(false);
                fs::set_permissions(&path, perms).unwrap();
                fs::write(&path, b"").unwrap();
            }
        }
    }
    fs::remove_dir_all(sb.home().join("git/checkouts")).unwrap();
    let out = sb.run(&app);
    assert_eq!(out.code, 1, "{}{}", out.stdout, out.stderr);
    assert!(
        out.stderr.contains("the cached copy may be damaged")
            && out.stderr.contains(&format!("delete `{}`", native(&db))),
        "{}",
        out.stderr
    );
    // Doing what it says works.
    fs::remove_dir_all(&db).unwrap();
    let out = sb.run(&app);
    assert_eq!(
        (out.code, out.stdout.as_str()),
        (0, "one\n"),
        "{}",
        out.stderr
    );
}

/// Runs every command that reads the package in `app` and checks that each fails and that
/// nothing it printed has the secret in it.
fn refused_without_the_secret(sb: &Sandbox, app: &Path, what: &str) {
    for args in [
        vec!["check"],
        vec!["run"],
        vec!["update"],
        vec!["check", "--offline"],
        vec!["test"],
    ] {
        let mut args = args;
        let target = app.to_str().unwrap().to_string();
        // `update` takes the folder as `--path`; as a plain argument it names a dependency,
        // and the manifest would never be read.
        if args[0] == "update" {
            args.push("--path");
        }
        args.push(&target);
        let out = sb.colibri(&args);
        assert_eq!(out.code, 1, "{what} {args:?}: {}", out.stderr);
        assert!(
            !out.stderr.contains("there is no `colibri.toml`"),
            "{what} {args:?} never reached the manifest: {}",
            out.stderr
        );
        assert!(
            !format!("{}{}", out.stdout, out.stderr).contains("SeCrEt"),
            "{what} {args:?}: {}{}",
            out.stdout,
            out.stderr
        );
    }
}

/// The `[features]` table of a manifest that also has a url with a password: the file is
/// refused for the credential like any other, whatever is wrong or right in the table, and
/// nothing printed has the password in it.
#[test]
fn a_feature_error_next_to_a_url_with_a_password_has_no_password_in_it() {
    let sb = Sandbox::new("secret_features");
    let cases = [
        "[features]\n\"a b\" = []\n",
        "[features]\nfast = [\"missing\"]\n",
        "[features]\nfast = \"x\"\n",
        "[features]\nfast = []\n",
        "[features]\ndefault = [\"https://u:SeCrEt@h\"]\n",
        "[features]\n\"https://u:SeCrEt@h\" = []\n",
    ];
    for (i, features) in cases.iter().enumerate() {
        let dir = format!("apps/feat{i}");
        sb.write(
            &format!("{dir}/colibri.toml"),
            &format!(
                "[package]\nname = \"app\"\n\n[dependencies]\nx = {{ git = \"https://u:SeCrEt@h.invalid/r\" }}\n\n{features}"
            ),
        );
        sb.write(&format!("{dir}/main.colibri"), "fn main() {}\n");
        refused_without_the_secret(&sb, &sb.path(&dir), features);
    }
}

/// What a manifest can hold in a value that is not a url: a credential in it is in no message
/// about it, whatever the key and whatever the command, and the file is refused.
#[test]
fn a_credential_in_any_value_of_a_manifest_is_in_no_message_about_it() {
    let sb = Sandbox::new("secret_messages");
    let cases = [
        "x = { path = \"https://u:SeCrEt@h.invalid/r\" }",
        "x = { path = \"git:SeCrEt@h.invalid:r\" }",
        "x = { git = \"https://h.invalid/r\", branch = \"https://u:SeCrEt@h\" }",
        "x = { git = \"https://h.invalid/r\", tag = \"https://u:SeCrEt@h\" }",
        "x = { git = \"https://h.invalid/r\", rev = \"https://u:SeCrEt@h\" }",
        "x = { version = \"https://u:SeCrEt@h\" }",
        "\"https://u:SeCrEt@h\" = { path = \"p\" }",
        "x = { git = \"https://h.invalid/r\", features = [\"https://u:SeCrEt@h\"] }",
        // What a message makes of a path: dots and slashes taken out of it.
        "x = { path = \"https://u:a/./SeCrEt@h/r\" }",
        "x = { path = \"https://u:a/b/../SeCrEt@h/r\" }",
        "x = { path = \"https://u:a///SeCrEt@h/r\" }",
        "x = { path = \"https://u:a\\\\SeCrEt@h/r\" }",
        // Two on a line, in a file that does not parse (a key defined twice).
        "x = { \"https://v:SeCrEt@h\" = \"1\", \"https://v:SeCrEt@h\" = \"2\" }",
        "x = { \"https://v:SeCrEt@h\" = \"1\", \"https://w:SeCrEt@h\" = \"2\"",
        "a = \"git:SeCrEt@h:r\", b = \"https://u:SeCrEt@h/r\", c = \"u:SeCrEt@h\"",
        // A file that does not parse because of an escape.
        "lib = { git = \"https\\u003a//ghp_SeCrEtToKeN@github.com/me/r\" }",
        "lib = { git = \"https\\x3a//ghp_SeCrEtToKeN@github.com/me/r\" }",
        "lib = { git = \"ghp\\u003aSeCrEt@github.com\" }",
        // Round 4: the features of a dependency, next to a url with a password.
        "x = { git = \"https://u:SeCrEt@h.invalid/r\", features = [\"a b\"] }",
        "x = { git = \"https://u:SeCrEt@h.invalid/r\", features = [\"os\"] }",
        "x = { git = \"https://u:SeCrEt@h.invalid/r\", features = [\"fast\"], default-features = \"no\" }",
        "x = { path = \"https://u:SeCrEt@h/r\", features = [3] }",
    ];
    for (i, dep) in cases.iter().enumerate() {
        let app = sb.app(&format!("secret{i}"), dep, &[]);
        refused_without_the_secret(&sb, &app, dep);
    }
    // The package name is a value too.
    sb.write(
        "apps/named/colibri.toml",
        "[package]\nname = \"https://u:SeCrEt@h\"\n",
    );
    sb.write("apps/named/main.colibri", "fn main() {}\n");
    refused_without_the_secret(&sb, &sb.path("apps/named"), "name");
    // Many on one line.
    let many: String = (0..200)
        .map(|i| format!("\"https://u{i}:SeCrEt@h{i}\" = \"{i}\", "))
        .collect();
    let app = sb.app("many", &format!("x = {{ {many}"), &[]);
    refused_without_the_secret(&sb, &app, "many");
}

#[test]
fn a_credential_in_the_manifest_of_a_dependency_or_in_the_lock_is_refused_through_the_root() {
    let sb = Sandbox::new("secret_dependencies");
    // A path dependency.
    sb.write(
        "libs/inner/colibri.toml",
        "[package]\nname = \"inner\"\n\n[dependencies]\ny = { path = \"https://u:a/./SeCrEt@h/r\" }\n",
    );
    sb.write(
        "libs/inner/lib.colibri",
        "pub fn version() -> str => \"i\"\n",
    );
    let app = sb.app(
        "dep_path",
        &format!(
            "inner = {{ path = \"{}\" }}",
            slashes(&sb.path("libs/inner"))
        ),
        &["inner"],
    );
    refused_without_the_secret(&sb, &app, "path dependency");
    // A git dependency from a local repository.
    let repo = sb.repo("holder");
    repo.commit(
        &[
            (
                "colibri.toml".to_string(),
                "[package]\nname = \"holder\"\n\n[dependencies]\nz = { git = \"https://h.invalid/r\", branch = \"https://u:SeCrEt@h\" }\n"
                    .to_string(),
            ),
            (
                "lib.colibri".to_string(),
                "pub fn version() -> str => \"h\"\n".to_string(),
            ),
        ],
        "holder",
    );
    let app = sb.app(
        "dep_git",
        &format!("holder = {{ git = \"{}\" }}", repo.url()),
        &["holder"],
    );
    refused_without_the_secret(&sb, &app, "git dependency");
    // A lock file.
    let (lib, _) = sb.lib_repo("greet", "one");
    let app = sb.app(
        "lock",
        &format!("greet = {{ git = \"{}\" }}", lib.url()),
        &["greet"],
    );
    assert_eq!(sb.run(&app).stdout, "one\n");
    let lock = fs::read_to_string(app.join("colibri.lock")).unwrap();
    let broken = lock.replacen("url = \"", "url = \"https://u:SeCrEt@h.invalid/", 1);
    assert_ne!(broken, lock);
    fs::write(app.join("colibri.lock"), &broken).unwrap();
    refused_without_the_secret(&sb, &app, "lock");
    assert_eq!(
        fs::read_to_string(app.join("colibri.lock")).unwrap(),
        broken,
        "a lock with a credential is not rewritten"
    );
    // Many on a line of a lock that does not parse.
    let many: String = (0..50)
        .map(|i| format!("\"https://u{i}:SeCrEt@h{i}\" = \"{i}\", "))
        .collect();
    fs::write(
        app.join("colibri.lock"),
        format!("[git.greet]\nurl = {{ {many}\n"),
    )
    .unwrap();
    refused_without_the_secret(&sb, &app, "lock, many");
}

/// A package whose `colibri.toml` is `manifest` and whose `main` does nothing.
fn package_with(sb: &Sandbox, name: &str, manifest: &str) -> PathBuf {
    sb.write(&format!("apps/{name}/colibri.toml"), manifest);
    sb.write(&format!("apps/{name}/main.colibri"), "fn main() {}\n");
    sb.path(&format!("apps/{name}"))
}

#[test]
fn an_at_sign_in_a_comment_or_a_path_never_refuses_a_manifest() {
    let sb = Sandbox::new("at_in_comments");
    sb.write(
        "apps/@scope/lib/colibri.toml",
        "[package]\nname = \"lib\"\n",
    );
    sb.write(
        "apps/@scope/lib/lib.colibri",
        "pub fn version() -> str => \"l\"\n",
    );
    for (i, text) in [
        "# docs: https://medium.com/@me/post\n[package]\nname = \"app\"\n",
        "# mirror of https://www.npmjs.com/package/@scope/pkg\n[package]\nname = \"app\"\n",
        "[package]\nname = \"app\" # contact jane.doe+x@example.com: thanks\n",
        "# see http://example.com/~user@host\n[package]\nname = \"app\"\n",
        "# TODO(jane@example.com): pin a tag\n[package]\nname = \"app\"\n",
        "# Maintainer: Jane Doe <jane@example.com> (license: MIT)\n[package]\nname = \"app\"\n",
        "# clone git@github.com:me/x.git\n[package]\nname = \"app\"\n",
        "[package]\nname = \"app\"\n[dependencies]\nlib = { path = \"../@scope/lib\" } # https://u:SeCrEt@h/r\n",
    ]
    .iter()
    .enumerate()
    {
        let app = package_with(&sb, &format!("ok{i}"), text);
        let out = sb.colibri(&["check", "--offline", app.to_str().unwrap()]);
        assert_eq!(out.code, 0, "{text}: {}", out.stderr);
        assert!(!out.stderr.contains("SeCrEt"), "{text}: {}", out.stderr);
    }
}

#[test]
fn odd_text_around_an_at_sign_is_an_error_not_a_panic() {
    let sb = Sandbox::new("at_no_panic");
    for (i, text) in [
        "[package]\nname = \"app\"\nffi = 1\n# owner: me@example.com = Jane\n",
        "v = 1\n# me@x = y\n",
        "[package]\nname = \"app\"\n\n\nu:SeCrEt@host:r = 1\n",
        "[package]\nname = \"p1\"\nx = \"\\é\"\n",
        "[package]\nname = \"p1\"\nx = \"\\😀\" # u:SeCrEt@h\n",
        "[package]\nname = \"p1\"\nx = \"\\",
    ]
    .iter()
    .enumerate()
    {
        let app = package_with(&sb, &format!("odd{i}"), text);
        for args in [vec!["check"], vec!["run"], vec!["update"]] {
            let mut args = args;
            if args[0] == "update" {
                args.push("--path");
            }
            args.push(app.to_str().unwrap());
            let out = sb.colibri(&args);
            assert_eq!(out.code, 1, "{text} {args:?}: {}", out.stderr);
            assert!(
                !out.stderr.contains("there is no `colibri.toml`"),
                "{text} {args:?} never reached the manifest: {}",
                out.stderr
            );
            assert!(!out.stderr.contains("panicked"), "{text}: {}", out.stderr);
            assert!(!out.stderr.contains("SeCrEt"), "{text}: {}", out.stderr);
        }
    }
}

#[test]
fn a_file_that_does_not_parse_reports_its_true_error_and_shows_no_line_with_an_at_sign() {
    let sb = Sandbox::new("unparsed");
    // The error is on line 3, which has no `@`; line 5 has a credential.
    let app = package_with(
        &sb,
        "true_error",
        "[package]\nname = \"app\"\nffi = 1\n[dependencies]\nlib = { git = \"https://u:SeCrEt@h/r\" }\n",
    );
    let out = sb.colibri(&["check", app.to_str().unwrap()]);
    assert_eq!(out.code, 1, "{}", out.stderr);
    assert!(
        out.stderr.contains("numbers are not supported"),
        "{}",
        out.stderr
    );
    assert!(out.stderr.contains("colibri.toml:3:"), "{}", out.stderr);
    assert!(out.stderr.contains("ffi = 1"), "{}", out.stderr);
    assert!(!out.stderr.contains("SeCrEt"), "{}", out.stderr);
    // The error is on a line with a legitimate `@`: a fixed message, the place, no text.
    let app = package_with(
        &sb,
        "fixed_text",
        "[package]\nname = \"app\"\n[dependencies]\nlib = { git = \"git@github.com:me/x.git\" \n",
    );
    let out = sb.colibri(&["check", "--offline", app.to_str().unwrap()]);
    assert_eq!(out.code, 1, "{}", out.stderr);
    assert!(
        out.stderr.contains("the text of this line is not shown"),
        "{}",
        out.stderr
    );
    assert!(out.stderr.contains("colibri.toml:4:"), "{}", out.stderr);
    assert!(!out.stderr.contains("github"), "{}", out.stderr);
    // A credential in a comment only, on a line with an unrelated manifest error.
    let app = package_with(
        &sb,
        "comment_only",
        "[package]\nname = \"app\"\nbogus = \"x\" # was https://u:SeCrEt@h/r\n",
    );
    let out = sb.colibri(&["check", "--offline", app.to_str().unwrap()]);
    assert_eq!(out.code, 1, "{}", out.stderr);
    assert!(out.stderr.contains("unknown key `bogus`"), "{}", out.stderr);
    assert!(!out.stderr.contains("SeCrEt"), "{}", out.stderr);
    assert!(
        !out.stderr.contains("read as a user name and password"),
        "{}",
        out.stderr
    );
}

#[test]
fn a_plain_ssh_user_is_no_credential_and_no_other_message_is_changed() {
    let sb = Sandbox::new("secret_innocent");
    // The url is judged by git later (the host is not there); the manifest, the lock step and
    // the message are not refused for the user name.
    let app = sb.app(
        "ssh_plain",
        "greet = { git = \"ssh://git@example.invalid:9/r.git\" }\nother = { git = \"git@example.invalid:r.git\" }",
        &[],
    );
    let out = sb.colibri(&["check", "--offline", app.to_str().unwrap()]);
    assert_eq!(out.code, 1, "{}", out.stderr);
    assert!(
        !out.stderr.contains("read as a user name and password"),
        "{}",
        out.stderr
    );
    // A path or a name with an `@` in it: its message says it as written.
    let app = sb.app("at_path", "x = { path = \"data@host\" }", &[]);
    let out = sb.colibri(&["check", app.to_str().unwrap()]);
    assert_eq!(out.code, 1);
    assert!(out.stderr.contains("data@host"), "{}", out.stderr);
    assert!(
        !out.stderr.contains("read as a user name and password"),
        "{}",
        out.stderr
    );
}

#[test]
fn refusing_the_credentials_of_a_big_manifest_costs_about_what_the_same_manifest_without_costs() {
    let sb = Sandbox::new("secret_cost");
    let n = 2000;
    let with: String = (0..n)
        .map(|i| format!("d{i} = {{ path = \"https://u{i}:SeCrEt@h{i}/r\" }}\n"))
        .collect();
    let without: String = (0..n)
        .map(|i| format!("d{i} = {{ path = \"libs/d{i}\" }}\n"))
        .collect();
    let time = |name: &str, deps: &str| {
        let app = sb.app(name, deps, &[]);
        let t = std::time::Instant::now();
        let out = sb.colibri(&["check", "--offline", app.to_str().unwrap()]);
        assert_eq!(out.code, 1, "{}", &out.stderr[..out.stderr.len().min(300)]);
        t.elapsed().as_secs_f64()
    };
    let (a, b) = (time("cost_without", &without), time("cost_with", &with));
    // A generous ratio: it was minutes against seconds when each message was searched for
    // each secret.
    assert!(b < 20.0 * a + 5.0, "{b} s with credentials, {a} s without");
}

#[test]
fn locked_fails_before_fetching_anything_the_lock_does_not_vouch_for() {
    let sb = Sandbox::new("locked_early");
    let (repo, _) = sb.lib_repo("first", "one");
    let first = format!("first = {{ git = \"{}\" }}", repo.url());
    let app = sb.app("app", &first, &["first"]);
    assert_eq!(sb.colibri(&["check", app.to_str().unwrap()]).code, 0);
    // `first` is locked but no longer in the cache; `second` is new.
    fs::remove_dir_all(sb.home().join("git")).unwrap();
    sb.app(
        "app",
        &format!("{first}\nsecond = {{ git = \"file:///colibri-no-such-repository/second.git\" }}"),
        &["first"],
    );
    let out = sb.colibri(&["check", "--locked", app.to_str().unwrap()]);
    assert_eq!(out.code, 1);
    assert!(
        out.stderr.contains("`second`") && out.stderr.contains("`--locked`"),
        "{}",
        out.stderr
    );
    // `first` comes before `second` and could have been fetched again: it was not.
    assert!(!sb.home().join("git").exists(), "the cache was touched");
}

#[test]
fn an_allow_ffi_name_that_matches_no_dependency_is_a_warning() {
    let sb = Sandbox::new("allow_ffi_unknown");
    let (repo, _) = sb.lib_repo("greet", "one");
    let app = sb.app(
        "app",
        &format!("greet = {{ git = \"{}\" }}", repo.url()),
        &["greet"],
    );
    sb.write(
        "apps/app/colibri.toml",
        &format!(
            "[package]\nname = \"app\"\nallow-ffi = [\"gfx\", \"greet\"]\n\n[dependencies]\ngreet = {{ git = \"{}\" }}\n",
            repo.url()
        ),
    );
    let out = sb.colibri(&["check", app.to_str().unwrap()]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    let all = format!("{}{}", out.stdout, out.stderr);
    assert!(
        all.contains("`allow-ffi` lists `gfx`") && !all.contains("lists `greet`"),
        "{all}"
    );
}

#[test]
fn a_manifest_that_is_a_symbolic_link_is_named_as_one() {
    let sb = Sandbox::new("manifest_link");
    let repo = sb.repo("linked");
    repo.commit(
        &[
            ("real.toml".into(), "[package]\nname = \"linked\"\n".into()),
            (
                "lib.colibri".into(),
                "pub fn version() -> str => \"x\"\n".into(),
            ),
        ],
        "files",
    );
    repo.stage_link("colibri.toml", "real.toml");
    git(&repo.dir, &["commit", "--quiet", "-m", "link"]);
    let app = sb.app(
        "app",
        &format!("linked = {{ git = \"{}\" }}", repo.url()),
        &["linked"],
    );
    let out = sb.run(&app);
    assert_eq!(out.code, 1, "{}", out.stdout);
    assert!(
        out.stderr
            .contains("`colibri.toml` that is a symbolic link")
            && out
                .stderr
                .contains("symbolic links in dependencies are ignored"),
        "{}",
        out.stderr
    );
}

#[test]
fn a_manifest_or_a_lock_is_never_taken_for_a_script() {
    let sb = Sandbox::new("manifest_as_script");
    let manifests = [
        "[package]\nname = \"p4\"\nx = { git = \"https://user:SeCrEt@example.invalid/r\" \n",
        "# was https://user:SeCrEt@example.invalid/r\n[package]\nname = \"p4\"\n",
    ];
    for (i, text) in manifests.iter().enumerate() {
        let app = package_with(&sb, &format!("script{i}"), text);
        sb.write(&format!("apps/script{i}/colibri.lock"), text);
        for file in ["colibri.toml", "colibri.lock"] {
            let target = app.join(file);
            let target = target.to_str().unwrap();
            for args in [
                vec!["check", target],
                vec!["run", target],
                vec!["run", "--watch", target],
                vec!["test", target],
                vec!["fmt", target],
                vec!["fmt", "--check", target],
            ] {
                let out = sb.colibri(&args);
                let all = format!("{}{}", out.stdout, out.stderr);
                assert_eq!(out.code, 1, "{args:?}: {all}");
                assert!(!all.contains("SeCrEt"), "{args:?}: {all}");
                assert!(
                    all.contains(file) && all.contains("not a script") && all.contains("folder"),
                    "{args:?}: {all}"
                );
                assert_eq!(&fs::read_to_string(target).unwrap(), text, "{args:?}");
            }
        }
        // A folder walk takes `.colibri` files only: the manifest is left alone.
        let out = sb.colibri(&["fmt", app.to_str().unwrap()]);
        assert_eq!(out.code, 0, "{}", out.stderr);
        assert_eq!(&fs::read_to_string(app.join("colibri.toml")).unwrap(), text);
    }
}

#[test]
fn a_manifest_that_cannot_be_read_never_empties_or_rewrites_the_lock() {
    let sb = Sandbox::new("broken_manifest_lock");
    let lock = "# colibri.lock\n\n[git.lib]\nurl = \"https://example.invalid/lib.git\"\ntag = \"v1\"\ncommit = \"0123456789abcdef0123456789abcdef01234567\"\n";
    let broken_git = "[package]\nname = \"p5\"\n[dependencies]\nlib = { git = \"https://example.invalid/lib.git\", tag = \"v1\" }\nbad = 1\n";
    // The manifest of the package itself, then of a path dependency that holds the git one.
    let app = package_with(&sb, "p5", broken_git);
    sb.write("apps/p5/colibri.lock", lock);
    let top = app.clone();
    sb.write(
        "apps/p6/colibri.toml",
        "[package]\nname = \"p6\"\n[dependencies]\nsub = { path = \"../sub6\" }\n",
    );
    sb.write("apps/p6/main.colibri", "fn main() {}\n");
    sb.write("apps/p6/colibri.lock", lock);
    sb.write("apps/sub6/colibri.toml", broken_git);
    sb.write("apps/sub6/lib.colibri", "pub fn f() {}\n");
    // A path dependency whose manifest fails validation (an unknown key) is the same.
    sb.write(
        "apps/p7/colibri.toml",
        "[package]\nname = \"p7\"\n[dependencies]\nsub = { path = \"../sub7\" }\n",
    );
    sb.write("apps/p7/main.colibri", "fn main() {}\n");
    sb.write("apps/p7/colibri.lock", lock);
    sb.write("apps/sub7/colibri.toml", "[package]\nname = \"sub7\"\nbogus = \"x\"\n[dependencies]\nlib = { git = \"https://example.invalid/lib.git\", tag = \"v1\" }\n");
    sb.write("apps/sub7/lib.colibri", "pub fn f() {}\n");
    for dir in [top, sb.path("apps/p6"), sb.path("apps/p7")] {
        let lock_file = dir.join("colibri.lock");
        for args in [
            vec!["check"],
            vec!["check", "--offline"],
            vec!["run"],
            vec!["test"],
            vec!["update"],
        ] {
            let mut args = args;
            if args[0] == "update" {
                args.push("--path");
            }
            args.push(dir.to_str().unwrap());
            let out = sb.colibri(&args);
            let all = format!("{}{}", out.stdout, out.stderr);
            assert_eq!(out.code, 1, "{args:?}: {all}");
            assert!(!all.contains("panicked"), "{args:?}: {all}");
            assert_eq!(
                fs::read_to_string(&lock_file).unwrap(),
                lock,
                "{args:?}: {all}"
            );
        }
    }
}

#[test]
fn update_with_a_manifest_that_cannot_be_read_fails_and_leaves_the_lock() {
    let sb = Sandbox::new("update_unreadable");
    let lock = "# colibri.lock\n\n[git.lib]\nurl = \"https://example.invalid/lib.git\"\ntag = \"v1\"\ncommit = \"0123456789abcdef0123456789abcdef01234567\"\n";
    // An invalid UTF-8 byte: the file is there and cannot be read as text.
    let bad: &[u8] = b"[package]\nname = \"p\"\n# \xff\n";
    // The package's own manifest, then a path dependency's.
    sb.write("apps/q1/main.colibri", "fn main() {}\n");
    fs::write(sb.path("apps/q1/colibri.toml"), bad).unwrap();
    sb.write("apps/q2/main.colibri", "fn main() {}\n");
    sb.write(
        "apps/q2/colibri.toml",
        "[package]\nname = \"q2\"\n[dependencies]\nsub = { path = \"../sub2\" }\n",
    );
    sb.write("apps/sub2/lib.colibri", "pub fn f() {}\n");
    fs::write(sb.path("apps/sub2/colibri.toml"), bad).unwrap();
    for (dir, file) in [("apps/q1", "q1"), ("apps/q2", "sub2")] {
        let dir = sb.path(dir);
        fs::write(dir.join("colibri.lock"), lock).unwrap();
        for names in [vec![], vec!["lib"]] {
            let mut args = vec!["update", "--path", dir.to_str().unwrap()];
            args.extend(names.iter());
            let out = sb.colibri(&args);
            assert_ne!(out.code, 0, "{args:?}: {}", out.stderr);
            assert!(
                out.stderr.contains("cannot read") && out.stderr.contains("colibri.toml"),
                "{file}: {args:?}: {}",
                out.stderr
            );
            assert_eq!(fs::read_to_string(dir.join("colibri.lock")).unwrap(), lock);
        }
    }
}

/// The names of what is in the cache's git folder, at any depth.
fn cache_names(sb: &Sandbox) -> Vec<String> {
    fn walk(dir: &Path, out: &mut Vec<String>) {
        if let Ok(read) = fs::read_dir(dir) {
            for e in read.flatten() {
                out.push(e.file_name().to_string_lossy().into_owned());
                walk(&e.path(), out);
            }
        }
    }
    let mut out = Vec::new();
    walk(&sb.home().join("git"), &mut out);
    out
}

#[test]
fn nothing_is_fetched_or_announced_after_a_manifest_that_cannot_be_read() {
    let sb = Sandbox::new("incomplete_walk");
    let (repo, _) = sb.lib_repo("lib", "one");
    let git_dep = format!("lib = {{ git = \"{}\" }}\n", repo.file_url());
    sb.write(
        "apps/sub/colibri.toml",
        "[package]\nname = \"sub\"\nbogus = \"x\"\n",
    );
    sb.write("apps/sub/lib.colibri", "pub fn f() {}\n");
    // The broken path dependency comes first (`asub` before `lib`), then after (`zsub`).
    for (order, deps) in [
        (
            "first",
            format!("asub = {{ path = \"../sub\" }}\n{git_dep}"),
        ),
        ("last", format!("{git_dep}zsub = {{ path = \"../sub\" }}\n")),
    ] {
        let app = sb.app(&format!("walk_{order}"), &deps, &[]);
        for args in [vec!["check"], vec!["run"], vec!["test"]] {
            let mut args = args;
            args.push(app.to_str().unwrap());
            let out = sb.colibri(&args);
            let all = format!("{}{}", out.stdout, out.stderr);
            assert_eq!(out.code, 1, "{order} {args:?}: {all}");
            assert!(
                !all.contains("locking") && !all.contains("updating"),
                "{order} {args:?}: {all}"
            );
            assert!(!app.join("colibri.lock").exists(), "{order} {args:?}");
            if order == "first" {
                assert!(
                    !cache_names(&sb).iter().any(|n| n.starts_with("lib-")),
                    "{order} {args:?}: {:?}",
                    cache_names(&sb)
                );
            }
        }
    }
}

#[test]
fn the_note_that_the_lock_was_left_is_only_said_when_there_is_a_lock() {
    let sb = Sandbox::new("left_note");
    let broken = "[package]\nname = \"p\"\nbad = 1\n";
    let app = package_with(&sb, "noted", broken);
    let out = sb.colibri(&["check", app.to_str().unwrap()]);
    assert_eq!(out.code, 1, "{}", out.stderr);
    assert!(!out.stderr.contains("was left as it is"), "{}", out.stderr);
    assert!(!app.join("colibri.lock").exists());
    fs::write(app.join("colibri.lock"), "# colibri.lock\n").unwrap();
    let out = sb.colibri(&["check", app.to_str().unwrap()]);
    assert_eq!(out.code, 1, "{}", out.stderr);
    assert!(out.stderr.contains("was left as it is"), "{}", out.stderr);
}

#[test]
fn a_git_dependency_takes_features_and_the_lock_does_not_know_them() {
    let sb = Sandbox::new("features");
    let repo = sb.repo("speedy");
    repo.commit(
        &[
            (
                "colibri.toml".to_string(),
                "[package]\nname = \"speedy\"\n\n[features]\ndefault = []\nfast = []\n".to_string(),
            ),
            (
                "lib.colibri".to_string(),
                "@when(fast)\npub fn version() -> str => \"fast\"\n@when(!fast)\npub fn version() -> str => \"slow\"\n".to_string(),
            ),
        ],
        "first",
    );
    let url = repo.url();
    // No features asked: the dependency's own default (nothing).
    let app = sb.app(
        "app",
        &format!("speedy = {{ git = \"{url}\" }}"),
        &["speedy"],
    );
    let out = sb.run(&app);
    assert_eq!(
        (out.code, out.stdout.as_str()),
        (0, "slow\n"),
        "{}",
        out.stderr
    );
    let lock = sb.lock(&app);
    // Asking for the feature changes the program, not the lock, so `--locked` still works.
    let app = sb.app(
        "app",
        &format!("speedy = {{ git = \"{url}\", features = [\"fast\"] }}"),
        &["speedy"],
    );
    let out = sb.colibri(&["run", "--locked", app.to_str().unwrap()]);
    assert_eq!(
        (out.code, out.stdout.as_str()),
        (0, "fast\n"),
        "{}",
        out.stderr
    );
    assert_eq!(sb.lock(&app), lock);
    // A feature the repository does not declare names both manifests.
    let app = sb.app(
        "app",
        &format!("speedy = {{ git = \"{url}\", features = [\"turbo\"] }}"),
        &["speedy"],
    );
    let out = sb.colibri(&["check", "--locked", app.to_str().unwrap()]);
    assert_eq!(out.code, 1, "{}", out.stderr);
    assert!(
        out.stderr
            .contains("asks for the feature `turbo` of `speedy`, which `speedy` does not declare"),
        "{}",
        out.stderr
    );
    assert!(out.stderr.contains("colibri.toml"), "{}", out.stderr);
}

/// The binary under test.
fn colibri_command() -> std::process::Command {
    std::process::Command::new(env!("CARGO_BIN_EXE_colibri"))
}
