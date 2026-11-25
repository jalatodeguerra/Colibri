//! `COLIBRI_ENGINE` and `colibri run --engine` choose the engine.

use std::path::PathBuf;
use std::process::{Command, Output};

fn script(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "colibri-engine-select-{}-{name}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("main.colibri");
    std::fs::write(&file, "fn main() {\n    print(\"ran\")\n}\n").unwrap();
    file
}

fn run(file: &PathBuf, flag: Option<&str>, env: Option<&str>) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_colibri"));
    cmd.arg("run");
    if let Some(flag) = flag {
        cmd.arg(flag);
    }
    cmd.arg(file);
    // The engine is what these tests are about: start from none, then set what a test names.
    cmd.env_remove("COLIBRI_ENGINE");
    if let Some(env) = env {
        cmd.env("COLIBRI_ENGINE", env);
    }
    cmd.output().expect("the colibri binary runs")
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn the_default_is_the_vm_and_the_environment_can_select_the_tree_walker() {
    let out = run(&script("default"), None, None);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "ran\n");
    // The engines are told apart by the steps of one test (engine-specific, EMBEDDING §5): this
    // test takes 47 on the VM and 48 on the tree-walker, so a budget of 47 lets only the VM
    // finish it.
    let file = script("steps");
    std::fs::write(file.with_file_name("walk.colibri"), STEPS).unwrap();
    let file = file.with_file_name("walk.colibri");
    let tests_with = |budget: &str, env: Option<&str>| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_colibri"));
        cmd.args(["test", "--budget", budget]).arg(&file);
        cmd.env_remove("COLIBRI_ENGINE");
        if let Some(env) = env {
            cmd.env("COLIBRI_ENGINE", env);
        }
        cmd.output().expect("the colibri binary runs")
    };
    let tests = |env: Option<&str>| tests_with("47", env);
    let default = tests(None);
    assert!(default.status.success(), "{}", stderr(&default));
    let vm = tests(Some("vm"));
    assert!(vm.status.success(), "{}", stderr(&vm));
    let tree = tests(Some("tree"));
    assert!(!tree.status.success(), "the tree-walker needs 48 steps");
    let report = format!("{}{}", String::from_utf8_lossy(&tree.stdout), stderr(&tree));
    assert!(
        report.contains("--budget <steps>"),
        "the tree-walker fails on the budget: {report}"
    );
    // One more step lets the tree-walker finish too, so the margin above is the engines' difference.
    let tree = tests_with("48", Some("tree"));
    assert!(tree.status.success(), "{}", stderr(&tree));
}

const STEPS: &str = r#"struct P { x: int, y: float }

test "walk" {
    let mut ps = [P { x: 0, y: 0.5 }, P { x: 1, y: 1.5 }, P { x: 2, y: 2.5 }]
    for round in 0..4 {
        for mut p in ps {
            p.x += round
            p.y *= 2.0
        }
    }
    print("{ps[0].x} {ps[1].x} {ps[2].x} {ps[2].y}")
}
"#;

#[test]
fn the_vm_runs_the_program_from_the_flag_and_from_the_environment() {
    let file = script("flag");
    let out = run(&file, Some("--engine=vm"), None);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "ran\n");
    let out = run(&file, None, Some("vm"));
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "ran\n");
}

#[test]
fn the_flag_wins_over_the_environment() {
    let out = run(&script("wins"), Some("--engine=tree"), Some("vm"));
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "ran\n");
}

#[test]
fn an_unknown_engine_name_is_refused() {
    let out = run(&script("unknown"), None, Some("jit"));
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(stderr(&out).contains("unknown engine `jit`"));
}
