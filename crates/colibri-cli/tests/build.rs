//! `colibri build`, end to end: a built executable runs like `colibri run`, with no source file
//! on disk, and a damaged one says so instead of crashing.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

struct Out {
    stdout: String,
    stderr: String,
    code: i32,
}

/// A fresh folder, removed at the end, with a private `COLIBRI_HOME` inside.
struct Dir(PathBuf);

impl Dir {
    fn new(name: &str) -> Dir {
        let dir = std::env::temp_dir().join(format!("colibri-build-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("home")).unwrap();
        // Git's global settings: an empty file, so that nothing of the machine's is used.
        fs::write(dir.join("gitconfig"), "").unwrap();
        Dir(dir)
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.0.join(rel)
    }

    fn write(&self, rel: &str, text: &str) {
        let full = self.0.join(rel);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, text).unwrap();
    }

    /// `colibri args..` in this folder.
    fn colibri(&self, args: &[&str]) -> Out {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_colibri"));
        cmd.args(args);
        self.exec(cmd)
    }

    /// A built executable of this folder, with `args`.
    fn built(&self, exe: &str, args: &[&str]) -> Out {
        let mut cmd = Command::new(self.path(exe));
        cmd.args(args);
        self.exec(cmd)
    }

    fn exec(&self, mut cmd: Command) -> Out {
        git_env(&mut cmd, &self.path("gitconfig"))
            .current_dir(&self.0)
            .env("COLIBRI_HOME", self.path("home"));
        let out = cmd.output().expect("the program runs");
        Out {
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            code: out.status.code().unwrap_or(-1),
        }
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The name of an executable that `-o name` writes.
fn exe(name: &str) -> String {
    format!("{name}{}", std::env::consts::EXE_SUFFIX)
}

/// Builds `target` into `name` and returns the output of the build.
fn build(dir: &Dir, target: &str, name: &str) -> Out {
    let out = dir.colibri(&["build", target, "-o", &exe(name)]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(dir.path(&exe(name)).is_file());
    out
}

fn same(run: &Out, built: &Out) {
    assert_eq!(run.stdout, built.stdout, "stdout");
    assert_eq!(run.stderr, built.stderr, "stderr");
    assert_eq!(run.code, built.code, "exit code");
}

const SCRIPT: &str = r#"import math
import os
import vec
import util.shout

fn main() {
    print("root {math.sqrt(16.0)}")
    print(shout.loud("hello"))
    let v = vec.vec2 { x: 3.0, y: 4.0 }
    print(v.len())
    print(os.args())
}
"#;

const SHOUT: &str = "pub fn loud(s: str) -> str => \"{s.to_upper()}!\"\n";

#[test]
fn a_built_script_runs_like_run_without_its_sources() {
    let dir = Dir::new("script");
    dir.write("prog.colibri", SCRIPT);
    dir.write("util/shout.colibri", SHOUT);
    let run = dir.colibri(&["run", "prog.colibri", "one", "--two", "-x"]);
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert!(run.stdout.contains("HELLO!"), "{}", run.stdout);
    let made = build(&dir, "prog.colibri", "prog_bin");
    assert!(made.stderr.contains("2 source files"), "{}", made.stderr);
    // Nothing of the program is left on disk: the built file holds it all.
    fs::remove_file(dir.path("prog.colibri")).unwrap();
    fs::remove_dir_all(dir.path("util")).unwrap();
    let built = dir.built(&exe("prog_bin"), &["one", "--two", "-x"]);
    same(&run, &built);
    // Every argument is the program's, the ones that `colibri` knows too.
    let built = dir.built(&exe("prog_bin"), &["--version"]);
    assert!(
        built.stdout.contains("[\"--version\"]"),
        "{}{}",
        built.stdout,
        built.stderr
    );
}

#[test]
fn a_built_panic_has_the_trace_and_the_exit_code_of_run() {
    let dir = Dir::new("panic");
    dir.write(
        "prog.colibri",
        "import deep.inner\n\nfn main() {\n    print(\"start\")\n    inner.boom(2)\n}\n",
    );
    dir.write(
        "deep/inner.colibri",
        "pub fn boom(n: int) {\n    if n == 0 {\n        let xs: List[int] = []\n        print(xs[1])\n    }\n    boom(n - 1)\n}\n",
    );
    let run = dir.colibri(&["run", "prog.colibri"]);
    assert_eq!(run.code, 101, "{}", run.stderr);
    assert!(run.stderr.contains("inner.colibri"), "{}", run.stderr);
    build(&dir, "prog.colibri", "p");
    fs::remove_file(dir.path("prog.colibri")).unwrap();
    fs::remove_dir_all(dir.path("deep")).unwrap();
    same(&run, &dir.built(&exe("p"), &[]));
}

#[test]
fn a_built_program_exits_with_the_code_it_asks_for() {
    let dir = Dir::new("exit");
    dir.write(
        "prog.colibri",
        "import os\n\nfn main() {\n    print(\"bye\")\n    os.exit(7)\n}\n",
    );
    let run = dir.colibri(&["run", "prog.colibri"]);
    assert_eq!(run.code, 7, "{}", run.stderr);
    build(&dir, "prog.colibri", "p");
    same(&run, &dir.built(&exe("p"), &[]));
}

#[test]
fn the_build_flags_travel_with_the_program() {
    let dir = Dir::new("flags");
    dir.write(
        "app/colibri.toml",
        "[package]\nname = \"app\"\n\n[features]\nextra = []\n",
    );
    dir.write(
        "app/main.colibri",
        r#"@when(debug)
fn mode() -> str => "debug"

@when(!debug)
fn mode() -> str => "release"

@when(extra)
fn more() -> str => "extra on"

@when(!extra)
fn more() -> str => "extra off"

fn main() {
    print("{mode()} {more()}")
}
"#,
    );
    let mut seen = Vec::new();
    for flags in [vec![], vec!["--release"], vec!["--feature", "extra"]] {
        let mut run_args = vec!["run", "app"];
        run_args.extend(&flags);
        let run = dir.colibri(&run_args);
        assert_eq!(run.code, 0, "{}", run.stderr);
        let out_name = exe("f_bin");
        let mut build_args = vec!["build", "app", "-o", out_name.as_str()];
        build_args.extend(&flags);
        let made = dir.colibri(&build_args);
        assert_eq!(made.code, 0, "{}", made.stderr);
        same(&run, &dir.built(&out_name, &[]));
        seen.push(run.stdout);
    }
    // And the three really differ.
    assert_eq!(
        seen,
        [
            "debug extra off\n",
            "release extra off\n",
            "debug extra on\n"
        ]
    );
}

#[test]
fn a_package_is_built_from_its_folder() {
    let dir = Dir::new("package");
    dir.write("app/colibri.toml", "[package]\nname = \"app\"\n");
    dir.write(
        "app/main.colibri",
        "import tools\n\nfn main() {\n    print(tools.twice(21))\n}\n",
    );
    dir.write(
        "app/tools.colibri",
        "pub fn twice(n: int) -> int => n * 2\n",
    );
    let run = dir.colibri(&["run", "app"]);
    assert_eq!(run.stdout, "42\n", "{}", run.stderr);
    // No `-o`: the name of the folder, in the current folder, which here is the folder
    // itself (a file next to `app` would be called `app`, which is the folder).
    let made = dir.colibri(&["build", "app"]);
    assert_eq!(made.code, 1);
    assert!(made.stderr.contains("is a folder"), "{}", made.stderr);
    let made = dir.colibri(&["build", "app", "-o", &exe("appbin")]);
    assert_eq!(made.code, 0, "{}", made.stderr);
    fs::remove_dir_all(dir.path("app")).unwrap();
    same(&run, &dir.built(&exe("appbin"), &[]));
}

#[test]
fn a_program_that_does_not_check_is_not_built() {
    let dir = Dir::new("broken");
    dir.write("prog.colibri", "fn main() {\n    print(nope)\n}\n");
    let out = dir.colibri(&["build", "prog.colibri", "-o", "x_bin"]);
    assert_eq!(out.code, 1);
    assert!(out.stderr.contains("nope"), "{}", out.stderr);
    assert!(!dir.path("x_bin").exists());
    // No half-written file either.
    let left: Vec<_> = fs::read_dir(&dir.0)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains("colibri-build"))
        .collect();
    assert!(left.is_empty(), "{left:?}");
}

#[test]
fn the_output_never_overwrites_a_source_file() {
    let dir = Dir::new("clobber");
    dir.write("prog.colibri", "fn main() {\n    print(1)\n}\n");
    dir.write("helper.colibri", "pub fn f() -> int => 1\n");
    let out = dir.colibri(&["build", "prog.colibri", "-o", "prog.colibri"]);
    assert_eq!(out.code, 1);
    assert!(out.stderr.contains("source file"), "{}", out.stderr);
    assert_eq!(
        fs::read_to_string(dir.path("prog.colibri")).unwrap(),
        "fn main() {\n    print(1)\n}\n"
    );
}

/// The executable `built` made, with a mistake `damage` makes in its bytes.
fn damaged(dir: &Dir, name: &str, damage: impl FnOnce(&mut Vec<u8>, usize)) -> Out {
    let good = fs::read(dir.path(&exe("good"))).unwrap();
    let payload_start = {
        let t = &good[good.len() - 32..];
        u64::from_le_bytes(t[12..20].try_into().unwrap()) as usize
    };
    let mut bytes = good;
    damage(&mut bytes, payload_start);
    let target = dir.path(&exe(name));
    fs::write(&target, &bytes).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
    }
    dir.built(&exe(name), &[])
}

fn clean_error(out: &Out) {
    assert_eq!(out.code, 1, "{}", out.stderr);
    assert!(out.stdout.is_empty(), "{}", out.stdout);
    assert!(
        out.stderr.starts_with("error: ") && out.stderr.contains("program's data"),
        "{}",
        out.stderr
    );
    // A crash would say so.
    assert!(!out.stderr.contains("panicked"), "{}", out.stderr);
    assert!(!out.stderr.contains("RUST_BACKTRACE"), "{}", out.stderr);
}

#[test]
fn a_damaged_or_cut_program_is_a_clean_error() {
    let dir = Dir::new("damaged");
    dir.write("prog.colibri", "fn main() {\n    print(\"fine\")\n}\n");
    build(&dir, "prog.colibri", "good");
    assert_eq!(dir.built(&exe("good"), &[]).stdout, "fine\n");
    // A flipped byte in the source text.
    clean_error(&damaged(&dir, "flip", |b, start| b[start + 40] ^= 0x20));
    // A flipped byte of the check sum.
    clean_error(&damaged(&dir, "sum", |b, _| {
        let at = b.len() - 32 - 3;
        b[at] ^= 1;
    }));
    // The trailer pointing somewhere else.
    clean_error(&damaged(&dir, "offset", |b, _| {
        let at = b.len() - 20;
        b[at] = b[at].wrapping_add(1);
    }));
    clean_error(&damaged(&dir, "length", |b, _| {
        let at = b.len() - 12;
        b[at] = b[at].wrapping_add(1);
    }));
    // A format version of the future.
    let out = damaged(&dir, "version", |b, _| {
        let at = b.len() - 4;
        b[at] = 99;
    });
    assert_eq!(out.code, 1);
    assert!(out.stderr.contains("format version 99"), "{}", out.stderr);
    // Part of the payload cut out, the trailer kept.
    clean_error(&damaged(&dir, "cut", |b, start| {
        let trailer: Vec<u8> = b[b.len() - 32..].to_vec();
        b.truncate(start + 20);
        b.extend(trailer);
    }));
    // The payload gone, the trailer kept.
    clean_error(&damaged(&dir, "gone", |b, start| {
        let trailer: Vec<u8> = b[b.len() - 32..].to_vec();
        b.truncate(start);
        b.extend(trailer);
    }));
}

#[test]
fn a_cut_executable_is_the_plain_tool_and_not_a_program() {
    // Without its trailer the file is a plain `colibri`: it does not run the program, it
    // answers to `--version` like the tool does.
    let dir = Dir::new("cut");
    dir.write("prog.colibri", "fn main() {\n    print(\"fine\")\n}\n");
    build(&dir, "prog.colibri", "good");
    let good = fs::read(dir.path(&exe("good"))).unwrap();
    assert!(
        !dir.built(&exe("good"), &["--version"])
            .stdout
            .starts_with("colibri ")
    );
    let out = {
        let target = dir.path(&exe("short"));
        fs::write(&target, &good[..good.len() - 5]).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
        }
        dir.built(&exe("short"), &["--version"])
    };
    assert_eq!(
        (out.code, out.stdout.starts_with("colibri ")),
        (0, true),
        "{}{}",
        out.stdout,
        out.stderr
    );
}

const LIB_TOML: &str = "[package]\nname = \"greet\"\n";
const LIB_SRC: &str = "pub fn version() -> str => \"from the repository\"\n";

fn git(dir: &Dir, repo: &Path, args: &[&str]) {
    let mut cmd = Command::new("git");
    git_env(&mut cmd, &dir.path("gitconfig"))
        .current_dir(repo)
        .args(args);
    let out = cmd
        .output()
        .expect("`git` must be installed to run these tests");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_env<'a>(cmd: &'a mut Command, global: &Path) -> &'a mut Command {
    cmd.env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .env("GIT_CONFIG_GLOBAL", global)
        .env("GIT_CONFIG_NOSYSTEM", "1")
}

#[test]
fn a_git_dependency_is_inside_the_built_program() {
    let dir = Dir::new("gitdep");
    let repo = dir.path("repo");
    fs::create_dir_all(&repo).unwrap();
    git(&dir, &repo, &["init", "--quiet", "-b", "main"]);
    fs::write(repo.join("colibri.toml"), LIB_TOML).unwrap();
    fs::write(repo.join("lib.colibri"), LIB_SRC).unwrap();
    git(&dir, &repo, &["add", "--all"]);
    git(&dir, &repo, &["commit", "--quiet", "-m", "first"]);
    let url = repo.to_string_lossy().replace('\\', "/");
    dir.write(
        "app/colibri.toml",
        &format!("[package]\nname = \"app\"\n\n[dependencies]\ngreet = {{ git = \"{url}\" }}\n"),
    );
    dir.write(
        "app/main.colibri",
        "import greet\n\nfn main() {\n    print(greet.version())\n    let xs: List[int] = []\n    print(xs[0])\n}\n",
    );
    // The first run fetches and locks; the second is the one the program is compared with.
    let first = dir.colibri(&["run", "app"]);
    assert!(first.stderr.contains("locking greet"), "{}", first.stderr);
    let run = dir.colibri(&["run", "app"]);
    assert_eq!(run.code, 101, "{}", run.stderr);
    assert!(run.stdout.contains("from the repository"));
    let made = dir.colibri(&["build", "app", "-o", &exe("gapp")]);
    assert_eq!(made.code, 0, "{}", made.stderr);
    // The repository, the source, the lock and the cache are all gone; the program's cache
    // folder is another one that does not exist.
    fs::remove_dir_all(dir.path("repo")).unwrap();
    fs::remove_dir_all(dir.path("app")).unwrap();
    fs::remove_dir_all(dir.path("home")).unwrap();
    let mut cmd = Command::new(dir.path(&exe("gapp")));
    cmd.env_remove("COLIBRI_HOME")
        .env("HOME", dir.path("nowhere"))
        .env("XDG_CACHE_HOME", dir.path("nowhere"))
        .env("LOCALAPPDATA", dir.path("nowhere"))
        .current_dir(&dir.0);
    let out = cmd.output().unwrap();
    let built = Out {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        code: out.status.code().unwrap_or(-1),
    };
    same(&run, &built);
    assert!(!dir.path("nowhere").exists());
}

/// How long a built copy of a program takes to start and finish, printed for the CI log.
#[test]
fn start_up_time_of_a_built_server_example_is_printed() {
    let dir = Dir::new("startup");
    let hello = include_str!("../../../tests/golden/std/httpd_hello.colibri");
    dir.write("httpd_hello.colibri", hello);
    build(&dir, "httpd_hello.colibri", "hello");
    fs::remove_file(dir.path("httpd_hello.colibri")).unwrap();
    let mut best = std::time::Duration::MAX;
    for _ in 0..5 {
        let started = std::time::Instant::now();
        let out = dir.built(&exe("hello"), &[]);
        assert_eq!(out.code, 0, "{}", out.stderr);
        assert!(
            out.stdout.contains("hello, GET /greeting"),
            "{}",
            out.stdout
        );
        best = best.min(started.elapsed());
    }
    let size = fs::metadata(dir.path(&exe("hello"))).unwrap().len();
    eprintln!("built httpd_hello: {size} bytes, start and run {best:?} (best of 5)");
}
