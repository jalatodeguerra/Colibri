//! The interface files follow the runner: written after a build that succeeded, compared (not
//! rewritten) when the runner was cached, never changed by a build that failed.

mod common;

use std::fs;
use std::path::PathBuf;
use std::process::Stdio;

use common::*;

/// The package: its program calls `fixture.double(21)` (one argument, as the first runner has it).
const PROGRAM_V1: &str = "import fixture\n\nfn main() {\n    print(fixture.double(21))\n}\n";
/// The manifest after the binding's version moved (so that the runner is built again).
const BOUND_NEXT: &str = "[package]\nname = \"app\"\n\n[rust-dependencies]\nfixture = { binding = \"colibri-fixture\", version = \"0.1.1\" }\n";

struct App {
    w: World,
    dir: PathBuf,
}

impl App {
    fn new(name: &str) -> App {
        let w = World::new(name);
        let dir = w.package("app", BOUND);
        fs::write(dir.join("main.colibri"), PROGRAM_V1).unwrap();
        App { w, dir }
    }

    fn cbi(&self) -> String {
        fs::read_to_string(self.dir.join("fixture.cbi")).unwrap()
    }

    /// `colibri <args>` here, with the runner that fake cargo "builds" and extra variables.
    fn colibri(&self, args: &[&str], runner: &str, extra: &[(&str, &str)]) -> Out {
        let mut c = self.w.command(UNDER_TEST, &self.dir, args);
        c.env("FAKE_CARGO_RUNNER", runner);
        for (k, v) in extra {
            c.env(k, v);
        }
        let out = c.stdin(Stdio::null()).output().unwrap();
        Out {
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            code: out.status.code().unwrap_or(-1),
        }
    }

    fn manifest(&self, text: &str) {
        fs::write(self.dir.join("colibri.toml"), text).unwrap();
    }
}

fn runners_of(w: &World) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(projects) = fs::read_dir(w.root.join("home").join("runners")) else {
        return found;
    };
    for project in projects.flatten() {
        if project.path().is_dir() {
            for e in fs::read_dir(project.path()).unwrap().flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if name.starts_with("runner-") && !name.ends_with(".interfaces") {
                    found.push(e.path());
                }
            }
        }
    }
    found
}

fn count(text: &str, what: &str) -> usize {
    text.matches(what).count()
}

#[test]
fn a_rebuild_with_a_new_signature_updates_the_interface_and_check_sees_it() {
    let app = App::new("rebuild");
    let v1 = app.cbi();
    assert!(app.colibri(&["check", "."], RUNNER, &[]).code == 0);
    let first = app.colibri(&["run", "main.colibri"], RUNNER, &[]);
    assert_eq!(first.code, 0, "{}", first.stderr);
    assert_eq!(first.stdout.trim(), "42");
    assert!(!first.stderr.contains("warning"), "{}", first.stderr);
    assert_eq!(app.cbi(), v1, "nothing changed, nothing is written");

    // The binding's signature changes: `double` takes two numbers. The next run builds again.
    app.manifest(BOUND_NEXT);
    let next = app.colibri(&["run", "main.colibri"], RUNNER_V2, &[]);
    assert_eq!(app.w.cargo_log().len(), 2, "{:?}", app.w.cargo_log());
    assert!(next.stderr.contains("updated "), "{}", next.stderr);
    assert!(next.stderr.contains("fixture.cbi"), "{}", next.stderr);
    let v2 = app.cbi();
    assert_ne!(v2, v1);
    assert!(v2.contains("times"), "{v2}");
    // The script still calls it with one: `check` says so, without cargo or a runner.
    let mut c = app.w.command(UNDER_TEST, &app.dir, &["check", "."]);
    c.env("CARGO", "colibri-no-such-cargo").env("PATH", "");
    let out = c.stdin(Stdio::null()).output().unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(err.contains("double"), "{err}");
    // And the run in the new runner stops at the same place.
    let run = app.colibri(&["run", "main.colibri"], RUNNER_V2, &[]);
    assert_eq!(run.code, 1, "{}", run.stderr);
    assert!(run.stderr.contains("double"), "{}", run.stderr);
    assert_eq!(app.w.cargo_log().len(), 2);
}

#[test]
fn a_stale_committed_interface_is_one_warning_and_bindings_check_fails_until_it_is_fresh() {
    let app = App::new("stale");
    let fresh = app.cbi();
    // (`--check` also wants the project's lock committed: write it first.)
    app.colibri(&["bindings", "."], RUNNER, &[]);
    let ok = app.colibri(&["bindings", "--check", "."], RUNNER, &[]);
    assert_eq!(ok.code, 0, "{}", ok.stderr);

    fs::write(app.dir.join("fixture.cbi"), "stale\n").unwrap();
    let check = app.colibri(&["bindings", "--check", "."], RUNNER, &[]);
    assert_eq!(check.code, 1, "{}", check.stderr);
    assert!(check.stderr.contains("out of date"), "{}", check.stderr);
    assert_eq!(app.cbi(), "stale\n", "--check writes nothing");

    // `run` with the cached runner: it runs (the runner knows the live interface) and says once.
    let run = app.colibri(&["run", "main.colibri"], RUNNER, &[]);
    assert_eq!(count(&run.stderr, "warning"), 1, "{}", run.stderr);
    assert_eq!(count(&run.stderr, "colibri bindings"), 1, "{}", run.stderr);
    assert_eq!(app.cbi(), "stale\n", "a cached run does not write");

    let fix = app.colibri(&["bindings", "."], RUNNER, &[]);
    assert_eq!(fix.code, 0, "{}", fix.stderr);
    assert_eq!(app.cbi(), fresh);
    let ok = app.colibri(&["bindings", "--check", "."], RUNNER, &[]);
    assert_eq!(ok.code, 0, "{}", ok.stderr);
    let run = app.colibri(&["run", "main.colibri"], RUNNER, &[]);
    assert!(!run.stderr.contains("warning"), "{}", run.stderr);
}

#[test]
fn a_failed_build_leaves_the_old_interface() {
    let app = App::new("failed");
    let v1 = app.cbi();
    app.colibri(&["run", "main.colibri"], RUNNER, &[]);
    app.manifest(BOUND_NEXT);
    // Cargo fails; or cargo succeeds and the lock it made is refused (two runtimes).
    for extra in [
        ("FAKE_CARGO_FAIL", "build"),
        ("FAKE_CARGO_DUP_RUNTIME", "1"),
    ] {
        let out = app.colibri(&["run", "main.colibri"], RUNNER_V2, &[extra]);
        assert_eq!(out.code, 1, "{extra:?}: {}", out.stderr);
        assert_eq!(app.cbi(), v1, "{extra:?}: the interface changed");
        assert!(!out.stderr.contains("updated"), "{}", out.stderr);
    }
}

#[test]
fn the_runner_is_asked_for_its_interfaces_once() {
    let app = App::new("once");
    app.colibri(&["run", "main.colibri"], RUNNER, &[]);
    let runners = runners_of(&app.w);
    assert_eq!(runners.len(), 1, "{runners:?}");
    let kept = colibri_lang::bindings::build::interfaces_dir(&runners[0]);
    assert!(kept.join("fixture.cbi").is_file(), "{kept:?}");
    // Later runs read that folder: they pass without a runner that could answer.
    let mut text = fs::read_to_string(kept.join("fixture.cbi")).unwrap();
    text.push_str("// kept\n");
    fs::write(kept.join("fixture.cbi"), &text).unwrap();
    let run = app.colibri(&["run", "main.colibri"], RUNNER, &[]);
    assert_eq!(
        count(&run.stderr, "warning"),
        1,
        "the kept copy is what is compared: {}",
        run.stderr
    );
}
