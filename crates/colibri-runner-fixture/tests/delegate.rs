//! `colibri run` in a package with `[rust-dependencies]` hands over to the project runner (here the
//! fixture runner, put where cargo would put it by a fake cargo), and `--runner FILE` uses a given
//! one. The real `main.rs` of `colibri` is the program under test (`colibri-under-test`).

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const UNDER_TEST: &str = env!("CARGO_BIN_EXE_colibri-under-test");
const RUNNER: &str = env!("CARGO_BIN_EXE_colibri-runner-fixture");
const FAKE_CARGO: &str = env!("CARGO_BIN_EXE_fake-cargo");

/// Arguments, standard input, exit codes: what a program shows of its host.
const PROGRAM: &str = "import os\nimport io\n\nfn main() {\n    print(os.args())\n    print(io.read_line())\n    io.eprint(\"to stderr\\n\")\n    let a = os.args()\n    if a.len() > 0 {\n        if a[0] == \"one\" {\n            os.exit(1)\n        }\n        if a[0] == \"panic\" {\n            let v = [1]\n            print(v[3])\n        }\n    }\n}\n";

const BOUND: &str = "[package]\nname = \"app\"\n\n[rust-dependencies]\nfixture = { binding = \"colibri-fixture\", version = \"0.1\" }\n";
const OTHER: &str = "[package]\nname = \"app\"\n\n[rust-dependencies]\nfixture = { binding = \"colibri-fixture\", version = \"0.2\" }\n";
const PLAIN: &str = "[package]\nname = \"app\"\n";

struct World {
    root: PathBuf,
}

struct Out {
    stdout: String,
    stderr: String,
    code: i32,
}

impl World {
    fn new(name: &str) -> World {
        let root =
            std::env::temp_dir().join(format!("colibri-delegate-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        // (macOS and Windows reach a temp folder through a link or a short name.)
        let root = fs::canonicalize(&root).unwrap();
        let root = PathBuf::from(
            colibri_lang::bindings::r#gen::strip_verbatim(&root.to_string_lossy()).to_string(),
        );
        World { root }
    }

    /// A package folder with `manifest` and the test program.
    fn package(&self, name: &str, manifest: &str) -> PathBuf {
        let dir = self.root.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("colibri.toml"), manifest).unwrap();
        fs::write(dir.join("main.colibri"), PROGRAM).unwrap();
        // The interface the checker reads, from the runner itself, as `colibri bindings` does.
        if manifest.contains("rust-dependencies") {
            let out = Command::new(RUNNER)
                .arg("__write-interfaces")
                .arg(&dir)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        dir
    }

    fn cargo_log(&self) -> Vec<String> {
        fs::read_to_string(self.root.join("cargo.log"))
            .map(|t| t.lines().map(str::to_string).collect())
            .unwrap_or_default()
    }

    fn command(&self, exe: &str, dir: &Path, args: &[&str]) -> Command {
        let mut c = Command::new(exe);
        c.args(args)
            .current_dir(dir)
            .env("COLIBRI_HOME", self.root.join("home"))
            .env("CARGO", FAKE_CARGO)
            .env("FAKE_CARGO_LOG", self.root.join("cargo.log"))
            .env("FAKE_CARGO_RUNNER", RUNNER)
            .env_remove("COLIBRI_PROJECT_RUNNER");
        c
    }

    fn run_with(&self, exe: &str, dir: &Path, args: &[&str], stdin: &str) -> Out {
        let mut child = self
            .command(exe, dir, args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        Out {
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            code: out.status.code().unwrap_or(-1),
        }
    }

    /// `colibri <args>` as the user types it, in `dir`.
    fn colibri(&self, dir: &Path, args: &[&str], stdin: &str) -> Out {
        self.run_with(UNDER_TEST, dir, args, stdin)
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn the_fixture_marker_is_the_one_of_its_package() {
    let w = World::new("marker");
    let dir = w.package("bound", BOUND);
    let (_, found) = colibri_lang::bindings::build::project(&dir).unwrap();
    let (marker, keys) = colibri_lang::bindings::build::expected_marker(&found).unwrap();
    assert_eq!(keys, ["fixture"]);
    let bytes = fs::read(RUNNER).unwrap();
    assert!(
        bytes.windows(marker.len()).any(|w| w == marker.as_bytes()),
        "the fixture should hold {marker:?}: put it in BINDINGS_MARKER of src/main.rs"
    );
}

/// A program shows the same of its host in `colibri run` without bindings and handed over to a
/// project runner: output, `os.args()`, standard input, and the exit code (0, 1 and 101).
#[test]
fn a_delegated_run_is_the_plain_run() {
    let w = World::new("parity");
    let plain = w.package("plain", PLAIN);
    let bound = w.package("bound", BOUND);
    let cases: [(&[&str], &str, i32); 4] = [
        (&["run", "main.colibri"], "line one\nline two\n", 0),
        (&["run", "main.colibri", "x", "--y", "z w"], "in\n", 0),
        (&["run", "main.colibri", "one"], "", 1),
        (&["run", "main.colibri", "panic"], "", 101),
    ];
    for (args, stdin, code) in cases {
        let a = w.colibri(&plain, args, stdin);
        let b = w.colibri(&bound, args, stdin);
        assert_eq!(a.code, code, "plain {args:?}: {}", a.stderr);
        assert_eq!(b.code, code, "delegated {args:?}: {}", b.stderr);
        assert_eq!(a.stdout, b.stdout, "{args:?}");
        // The first delegated run says it builds; what the program prints is the same.
        let strip = |s: &str| {
            s.lines()
                .filter(|l| !l.starts_with("building the Rust bindings"))
                .collect::<Vec<_>>()
                .join("\n")
        };
        assert_eq!(strip(&a.stderr), strip(&b.stderr), "{args:?}");
    }
    // The runner was built once, for all four runs, and only the first said so.
    assert_eq!(w.cargo_log().len(), 1, "{:?}", w.cargo_log());
    let again = w.colibri(&bound, &["run", "main.colibri"], "");
    assert!(
        !again.stderr.contains("building the Rust bindings"),
        "{}",
        again.stderr
    );
}

#[test]
fn test_and_a_folder_argument_hand_over_too() {
    let w = World::new("folder");
    let bound = w.package("bound", BOUND);
    let out = w.colibri(&bound, &["run", "."], "x\n");
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(out.stdout.contains("ok(some(\"x\"))"), "{}", out.stdout);
    assert_eq!(w.cargo_log().len(), 1);
    // `test` is the runner's too: no tests in the program, and no second build.
    let t = w.colibri(&bound, &["test"], "");
    assert_eq!(t.code, 0, "{}{}", t.stdout, t.stderr);
    assert_eq!(w.cargo_log().len(), 1);
}

#[test]
fn a_script_without_a_manifest_and_a_package_without_the_table_do_not_delegate() {
    let w = World::new("none");
    let plain = w.package("plain", PLAIN);
    let loose = w.root.join("loose");
    fs::create_dir_all(&loose).unwrap();
    fs::write(loose.join("main.colibri"), PROGRAM).unwrap();
    for dir in [&plain, &loose] {
        let mut c = w.command(UNDER_TEST, dir, &["run", "main.colibri"]);
        // No cargo to be found: a delegation would fail.
        c.env("CARGO", "colibri-no-such-cargo").env("PATH", "");
        let out = c.stdin(Stdio::null()).output().unwrap();
        assert_eq!(
            out.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    assert!(w.cargo_log().is_empty());
    assert!(!w.root.join("home").exists(), "the cache was touched");
}

#[test]
fn run_runner_uses_a_given_runner_after_checking_it() {
    let w = World::new("given");
    let bound = w.package("bound", BOUND);
    let other = w.package("other", OTHER);
    // A runner with these bindings: used as it is, cargo never called.
    let out = w.colibri(
        &bound,
        &[
            "run",
            "--runner",
            RUNNER,
            "main.colibri",
            "a",
            "--runner",
            "kept",
        ],
        "hi\n",
    );
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(
        out.stdout.starts_with("[\"a\", \"--runner\", \"kept\"]"),
        "{}",
        out.stdout
    );
    let out = w.colibri(&bound, &["run", "--runner", RUNNER, "main.colibri"], "");
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(w.cargo_log().is_empty(), "cargo ran: {:?}", w.cargo_log());

    // The plain `colibri`: right version, no bindings. Refused, naming the bindings.
    let out = w.colibri(&bound, &["run", "--runner", UNDER_TEST, "main.colibri"], "");
    assert_eq!(out.code, 1);
    assert!(
        out.stderr.contains("is a plain `colibri`") && out.stderr.contains("fixture"),
        "{}",
        out.stderr
    );
    assert!(out.stdout.is_empty(), "{}", out.stdout);

    // A runner of other bindings than the package needs.
    let out = w.colibri(&other, &["run", "--runner", RUNNER, "main.colibri"], "");
    assert_eq!(out.code, 1);
    assert!(out.stderr.contains("other Rust bindings"), "{}", out.stderr);

    // Not a file; and a package that has no table has no use for one.
    let missing = w.root.join("missing-runner").display().to_string();
    let out = w.colibri(&bound, &["run", "--runner", &missing, "main.colibri"], "");
    assert_eq!(out.code, 1);
    assert!(
        out.stderr.contains("cannot read the runner"),
        "{}",
        out.stderr
    );
    let plain = w.package("plain", PLAIN);
    let out = w.colibri(&plain, &["run", "--runner", RUNNER, "main.colibri"], "");
    assert_eq!(out.code, 1);
    assert!(
        out.stderr.contains("no `[rust-dependencies]`"),
        "{}",
        out.stderr
    );
}

/// A runner that is itself delegating would start itself again and again: the guard variable stops
/// it. Here the "runner" cargo makes is the plain `colibri` of a package with the table.
#[test]
fn a_runner_never_hands_over_again() {
    let w = World::new("guard");
    let bound = w.package("bound", BOUND);
    let mut command = w.command(UNDER_TEST, &bound, &["run", "main.colibri"]);
    command.env("FAKE_CARGO_RUNNER", UNDER_TEST);
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // A loop is found by the clock, not waited for.
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("colibri handed over to itself without end: the guard is gone");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let out = child.wait_with_output().unwrap();
    assert!(status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(w.cargo_log().len(), 1);
}

/// What handing over costs: the same run, in the runner directly and through `colibri`, the best
/// of ten each (a run that is already cached, as every run but the first is).
#[test]
fn handing_over_costs_little() {
    let w = World::new("overhead");
    let bound = w.package("bound", BOUND);
    let warm = w.colibri(&bound, &["run", "main.colibri"], "");
    assert_eq!(warm.code, 0, "{}", warm.stderr);
    let best = |exe: &str| {
        (0..10)
            .map(|_| {
                let start = Instant::now();
                let out = w.run_with(exe, &bound, &["run", "main.colibri"], "");
                assert_eq!(out.code, 0, "{}", out.stderr);
                start.elapsed()
            })
            .min()
            .unwrap()
    };
    let direct = best(RUNNER);
    let through = best(UNDER_TEST);
    let extra = through.saturating_sub(direct);
    eprintln!(
        "delegation overhead: {:.1} ms (runner alone {:.1} ms, through colibri {:.1} ms, best of 10)",
        extra.as_secs_f64() * 1000.0,
        direct.as_secs_f64() * 1000.0,
        through.as_secs_f64() * 1000.0
    );
    // 10 ms in a release build on Unix, where `colibri` becomes the runner. A debug build is several
    // times slower at everything, and on Windows the runner is a child process (about 20 ms more).
    let limit = match (cfg!(debug_assertions), cfg!(windows)) {
        (false, false) => 10,
        (true, false) => 60,
        (_, true) => 250,
    };
    assert!(extra < Duration::from_millis(limit), "{extra:?}");
}

/// A package with a path dependency that has no bindings, and many modules: `colibri run` reads the
/// manifests to learn that there is nothing to hand over, and starts as fast as with the guard.
#[test]
fn a_package_with_only_plain_dependencies_starts_as_fast_as_ever() {
    let w = World::new("plaindeps");
    let dir = w.root.join("big");
    fs::create_dir_all(dir.join("dep")).unwrap();
    fs::write(
        dir.join("colibri.toml"),
        "[package]\nname = \"app\"\n\n[dependencies]\ndep = { path = \"dep\" }\n",
    )
    .unwrap();
    fs::write(dir.join("dep/colibri.toml"), "[package]\nname = \"dep\"\n").unwrap();
    fs::write(
        dir.join("dep/lib.colibri"),
        "pub fn one() -> int {\n    return 1\n}\n",
    )
    .unwrap();
    let mut main = String::from("import dep\n");
    let mut body = String::from("    let mut total = dep.one()\n");
    for i in 0..300 {
        fs::write(
            dir.join(format!("m{i}.colibri")),
            format!("pub fn f() -> int {{\n    let a = {i}\n    let b = a * 2 + 1\n    return b - a\n}}\n"),
        )
        .unwrap();
        main.push_str(&format!("import m{i}\n"));
        body.push_str(&format!("    total = total + m{i}.f()\n"));
    }
    main.push_str(&format!("\nfn main() {{\n{body}    print(total)\n}}\n"));
    fs::write(dir.join("main.colibri"), main).unwrap();

    let time = |guard: bool| {
        (0..10)
            .map(|_| {
                let mut c = w.command(UNDER_TEST, &dir, &["run", "main.colibri"]);
                // No cargo to be found: a hand-over would fail.
                c.env("CARGO", "colibri-no-such-cargo").env("PATH", "");
                if guard {
                    c.env("COLIBRI_PROJECT_RUNNER", "1");
                }
                let start = Instant::now();
                let out = c.stdin(Stdio::null()).output().unwrap();
                let took = start.elapsed();
                assert_eq!(
                    out.status.code(),
                    Some(0),
                    "{}",
                    String::from_utf8_lossy(&out.stderr)
                );
                assert!(String::from_utf8_lossy(&out.stdout).starts_with("45151"));
                took
            })
            .min()
            .unwrap()
    };
    let plain = time(false);
    let guarded = time(true);
    let extra = plain.saturating_sub(guarded);
    eprintln!(
        "start-up of a package with a plain dependency: {:.1} ms, with the guard {:.1} ms (extra {:.1} ms, best of 10)",
        plain.as_secs_f64() * 1000.0,
        guarded.as_secs_f64() * 1000.0,
        extra.as_secs_f64() * 1000.0
    );
    // Within noise: a package load (what the check used to cost) is a good part of the time.
    assert!(
        extra < guarded / 10 + Duration::from_millis(3),
        "extra {extra:?} on {guarded:?}"
    );
    assert!(w.cargo_log().is_empty());
}

/// Without cargo a `run` says what `colibri bindings` says.
#[test]
fn run_without_cargo_says_what_bindings_says() {
    let w = World::new("nocargo");
    let bound = w.package("bound", BOUND);
    let message = |args: &[&str]| {
        let mut c = w.command(UNDER_TEST, &bound, args);
        c.env("CARGO", "colibri-no-such-cargo").env("PATH", "");
        let out = c.stdin(Stdio::null()).output().unwrap();
        assert_eq!(out.status.code(), Some(1));
        String::from_utf8_lossy(&out.stderr).into_owned()
    };
    let run = message(&["run", "main.colibri"]);
    let bindings = message(&["bindings", "."]);
    assert!(
        run.contains("rustup.rs") && run.contains("--runner PATH"),
        "{run}"
    );
    assert_eq!(run, bindings);
}
