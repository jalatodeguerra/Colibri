//! `colibri build` of a package with `[rust-dependencies]` ships the project runner: the program
//! it writes is the runner (here the fixture runner, put where cargo would put it by a fake cargo)
//! with the pack after it, so it runs as `colibri run` does, and needs no cargo and no cache.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use common::*;

/// Calls the binding, shows the arguments and the exit code of the program.
const SHIPPED: &str = "import fixture\nimport os\n\nfn main() {\n    print(fixture.double(21))\n    print(os.args())\n    let a = os.args()\n    if a.len() > 0 {\n        if a[0] == \"one\" {\n            os.exit(3)\n        }\n    }\n}\n";

fn package(w: &World, name: &str, manifest: &str) -> PathBuf {
    let dir = w.package(name, manifest);
    fs::write(dir.join("main.colibri"), SHIPPED).unwrap();
    dir
}

fn exe(w: &World, name: &str) -> PathBuf {
    w.root
        .join(format!("{name}{}", std::env::consts::EXE_SUFFIX))
}

/// Runs a built program somewhere else, with no cargo to be found and no guard variable.
fn run_built(w: &World, exe: &Path, args: &[&str]) -> Out {
    let elsewhere = w.root.join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    let mut command = std::process::Command::new(exe);
    command
        .args(args)
        .current_dir(&elsewhere)
        .env("CARGO", "colibri-no-such-cargo")
        .env("COLIBRI_HOME", w.root.join("no-home"))
        .env_remove("COLIBRI_PROJECT_RUNNER");
    let out = command
        .stdin(Stdio::null())
        .output()
        .expect("the built program starts");
    Out {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        code: out.status.code().unwrap_or(-1),
    }
}

#[test]
fn a_built_program_is_what_run_shows() {
    let w = World::new("ship");
    let bound = package(&w, "bound", BOUND);
    let out = exe(&w, "app");
    let target = out.display().to_string();
    let built = w.colibri(&bound, &["build", "-o", &target], "");
    assert_eq!(built.code, 0, "{}", built.stderr);
    // One release build, and the runner's size is said.
    let log = w.cargo_log();
    assert_eq!(log.len(), 1, "{log:?}");
    assert!(log[0].contains("release"), "{log:?}");
    let size = fs::metadata(RUNNER).unwrap().len();
    assert!(
        built.stderr.contains(&format!(
            "({size} bytes, with the Rust bindings of `fixture`)"
        )),
        "{}",
        built.stderr
    );
    // A built program is a program: it runs with no cargo (the log keeps its one line).
    assert!(run_built(&w, &out, &[]).stdout.starts_with("42\n"));
    assert_eq!(w.cargo_log().len(), 1);
    for args in [&[][..], &["a", "--b", "c d"][..], &["one"][..]] {
        let mut run = vec!["run", "main.colibri"];
        run.extend_from_slice(args);
        let expected = w.colibri(&bound, &run, "");
        let got = run_built(&w, &out, args);
        assert_eq!(got.code, expected.code, "{args:?}: {}", got.stderr);
        assert_eq!(got.stdout, expected.stdout, "{args:?}");
        assert!(got.stdout.starts_with("42\n"), "{}", got.stdout);
    }
    // Exit codes came through.
    assert_eq!(run_built(&w, &out, &["one"]).code, 3);
}

#[test]
fn a_runner_given_by_hand_is_used_after_its_checks() {
    let w = World::new("ship-given");
    let bound = package(&w, "bound", BOUND);
    let other = package(&w, "other", OTHER);
    let host = colibri_lang::platform::Target::host()
        .expect("a tested platform")
        .name();
    let out = exe(&w, "app");
    let target = out.display().to_string();
    let args = |runner: &str| {
        vec![
            "build",
            "--target",
            Box::leak(host.clone().into_boxed_str()) as &str,
            "--runner",
            Box::leak(runner.to_string().into_boxed_str()) as &str,
            "-o",
            Box::leak(target.clone().into_boxed_str()) as &str,
        ]
    };
    // The runner of the package: used as it is, cargo never called.
    let built = w.colibri(&bound, &args(RUNNER), "");
    assert_eq!(built.code, 0, "{}", built.stderr);
    assert!(w.cargo_log().is_empty(), "{:?}", w.cargo_log());
    assert!(run_built(&w, &out, &[]).stdout.starts_with("42\n"));
    fs::remove_file(&out).unwrap();
    // The plain `colibri`.
    let refused = w.colibri(&bound, &args(UNDER_TEST), "");
    assert_eq!(refused.code, 1);
    assert!(
        refused.stderr.contains("is a plain `colibri`"),
        "{}",
        refused.stderr
    );
    // The runner of another project.
    let refused = w.colibri(&other, &args(RUNNER), "");
    assert_eq!(refused.code, 1);
    assert!(
        refused.stderr.contains("other Rust bindings"),
        "{}",
        refused.stderr
    );
    // A project runner for a package with no bindings.
    let plain = package(&w, "plain", PLAIN);
    let refused = w.colibri(&plain, &args(RUNNER), "");
    assert_eq!(refused.code, 1);
    assert!(
        refused.stderr.contains("declares none"),
        "{}",
        refused.stderr
    );
    assert!(!out.exists(), "a refused build wrote a file");
}

#[test]
fn another_platform_needs_a_runner_built_there() {
    let w = World::new("ship-target");
    let bound = package(&w, "bound", BOUND);
    let host = colibri_lang::platform::Target::host().expect("a tested platform");
    let foreign = colibri_lang::platform::TARGETS
        .iter()
        .map(|(name, _)| *name)
        .find(|name| *name != host.name())
        .unwrap();
    let out = exe(&w, "app");
    let refused = w.colibri(
        &bound,
        &[
            "build",
            "--target",
            foreign,
            "-o",
            &out.display().to_string(),
        ],
        "",
    );
    assert_eq!(refused.code, 1);
    assert!(
        refused
            .stderr
            .contains("a project runner built on that platform"),
        "{}",
        refused.stderr
    );
    assert!(w.cargo_log().is_empty(), "{:?}", w.cargo_log());
    assert!(!out.exists());
}

#[test]
fn a_web_build_refuses_rust_bindings() {
    let w = World::new("ship-web");
    let bound = package(&w, "bound", BOUND);
    let out = w.root.join("page");
    let refused = w.colibri(
        &bound,
        &["build", "--web", "-o", &out.display().to_string()],
        "",
    );
    assert_eq!(refused.code, 1);
    assert!(
        refused
            .stderr
            .contains("Rust bindings cannot run in a web build yet (`fixture`)"),
        "{}",
        refused.stderr
    );
    assert!(!out.exists());
    assert!(w.cargo_log().is_empty());
}

#[test]
fn a_package_without_bindings_is_built_as_before() {
    let w = World::new("ship-plain");
    let plain = w.package("plain", PLAIN);
    let out = exe(&w, "app");
    let mut command = w.command(
        UNDER_TEST,
        &plain,
        &["build", "-o", &out.display().to_string()],
    );
    // No cargo to be found: a project runner would fail.
    command
        .env("CARGO", "colibri-no-such-cargo")
        .env("PATH", "");
    let built = command.stdin(Stdio::null()).output().unwrap();
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    assert!(!String::from_utf8_lossy(&built.stderr).contains("runner"));
    assert!(w.cargo_log().is_empty());
}
