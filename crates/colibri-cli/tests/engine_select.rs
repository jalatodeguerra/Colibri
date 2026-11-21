//! `COLIBRI_ENGINE` and `colibri run --engine` choose the engine; the bytecode VM is not built
//! yet, so asking for it fails with a clear message and the tree-walker still runs.

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
fn the_default_is_the_tree_walker() {
    let out = run(&script("default"), None, None);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "ran\n");
}

#[test]
fn the_vm_is_refused_with_a_clear_message() {
    let file = script("flag");
    let out = run(&file, Some("--engine=vm"), None);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(stderr(&out).contains("the bytecode VM is not built yet"));
    let out = run(&file, None, Some("vm"));
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(stderr(&out).contains("the bytecode VM is not built yet"));
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
