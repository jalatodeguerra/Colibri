//! `colibri run` in a package with `[rust-dependencies]` hands over to the project runner (here the
//! fixture runner, put where cargo would put it by a fake cargo), and `--runner FILE` uses a given
//! one. The real `main.rs` of `colibri` is the program under test (`colibri-under-test`).

mod common;

use std::fs;
use std::process::Stdio;
use std::time::{Duration, Instant};

use common::*;

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

/// The bindings are declared by a dependency, not by the app: `run` hands over all the same.
#[test]
fn a_dependency_that_declares_bindings_makes_run_hand_over() {
    let w = World::new("depbinds");
    let plain = w.package("plain", PLAIN);
    let app = w.root.join("app");
    fs::create_dir_all(&app).unwrap();
    fs::write(
        app.join("colibri.toml"),
        "[package]\nname = \"app\"\n\n[dependencies]\ndep = { path = \"dep\" }\n",
    )
    .unwrap();
    fs::write(app.join("main.colibri"), PROGRAM).unwrap();
    let dep = app.join("dep");
    fs::create_dir_all(&dep).unwrap();
    fs::write(
        dep.join("colibri.toml"),
        BOUND.replace("name = \"app\"", "name = \"dep\""),
    )
    .unwrap();
    fs::write(
        dep.join("lib.colibri"),
        "pub fn one() -> int {\n    return 1\n}\n",
    )
    .unwrap();
    // The interface the checker reads, from the runner itself.
    let out = std::process::Command::new(RUNNER)
        .arg("__write-interfaces")
        .arg(&dep)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let args: &[&str] = &["run", "main.colibri", "x"];
    let want = w.colibri(&plain, args, "hi\n");
    let got = w.colibri(&app, args, "hi\n");
    assert_eq!(got.code, 0, "{}", got.stderr);
    assert_eq!(got.stdout, want.stdout);
    assert!(
        got.stderr.contains("building the Rust bindings"),
        "{}",
        got.stderr
    );
    assert_eq!(w.cargo_log().len(), 1, "{:?}", w.cargo_log());
}

/// The package is a folder of the binding crate (`path = ".."`): editing a Colibri file does not
/// make the next run build again, and neither does the lock the first build wrote into the crate.
#[test]
fn a_package_inside_the_crate_folder_is_not_rebuilt_for_a_colibri_edit() {
    let w = World::new("inside");
    let krate = w.root.join("crate");
    let app = krate.join("app");
    fs::create_dir_all(krate.join("src")).unwrap();
    fs::create_dir_all(&app).unwrap();
    fs::write(
        krate.join("Cargo.toml"),
        "[package]\nname = \"colibri-fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\ncolibri-runtime = \"0.1\"\n",
    )
    .unwrap();
    fs::write(krate.join("src/lib.rs"), "// the binding\n").unwrap();
    fs::write(
        app.join("colibri.toml"),
        "[package]\nname = \"app\"\n\n[rust-dependencies]\nfixture = { path = \"..\" }\n",
    )
    .unwrap();
    fs::write(app.join("main.colibri"), PROGRAM).unwrap();
    let out = std::process::Command::new(RUNNER)
        .arg("__write-interfaces")
        .arg(&app)
        .output()
        .unwrap();
    assert!(out.status.success());
    let first = w.colibri(&app, &["run", "main.colibri"], "a\n");
    assert_eq!(first.code, 0, "{}", first.stderr);
    assert_eq!(w.cargo_log().len(), 1);
    fs::write(app.join("main.colibri"), format!("{PROGRAM}\n// an edit\n")).unwrap();
    let second = w.colibri(&app, &["run", "main.colibri"], "a\n");
    assert_eq!(second.code, 0, "{}", second.stderr);
    assert_eq!(w.cargo_log().len(), 1, "a Colibri edit called cargo");
    assert!(!second.stderr.contains("building"), "{}", second.stderr);
    // A Rust edit still does.
    fs::write(krate.join("src/lib.rs"), "// another binding\n").unwrap();
    w.colibri(&app, &["run", "main.colibri"], "a\n");
    assert_eq!(w.cargo_log().len(), 2);
}

/// What a runner was started with is not for the programs it runs.
#[test]
fn a_runner_does_not_pass_its_guard_to_its_program() {
    let w = World::new("guardenv");
    let dir = w.package("bound", BOUND);
    fs::write(
        dir.join("main.colibri"),
        "import os\n\nfn main() {\n    print(os.env(\"COLIBRI_PROJECT_RUNNER\"))\n}\n",
    )
    .unwrap();
    let out = w.colibri(&dir, &["run", "main.colibri"], "");
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(out.stdout.trim(), "none", "{}{}", out.stdout, out.stderr);
}
