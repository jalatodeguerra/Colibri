//! `COLIBRI_ENGINE` used to choose the engine. It is ignored now: the VM runs the program
//! whatever it says, and `tree` (the interpreter that was removed) gets a one-line warning.

use std::path::PathBuf;
use std::process::{Command, Output};

fn script(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("colibri-engine-env-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("main.colibri");
    std::fs::write(&file, "fn main() {\n    print(\"ran\")\n}\n").unwrap();
    file
}

fn run(file: &PathBuf, engine: Option<&str>) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_colibri"));
    cmd.arg("run").arg(file);
    cmd.env_remove("COLIBRI_ENGINE");
    if let Some(engine) = engine {
        cmd.env("COLIBRI_ENGINE", engine);
    }
    cmd.output().expect("the colibri binary runs")
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn the_variable_is_ignored_whatever_it_says() {
    let file = script("ignored");
    for engine in [None, Some("vm"), Some("tree"), Some("both"), Some("jit")] {
        let out = run(&file, engine);
        assert!(out.status.success(), "{engine:?}: {}", stderr(&out));
        assert_eq!(String::from_utf8_lossy(&out.stdout), "ran\n", "{engine:?}");
    }
}

#[test]
fn tree_gets_a_one_line_warning_and_nothing_else_does() {
    let file = script("warning");
    let out = run(&file, Some("tree"));
    assert_eq!(
        stderr(&out),
        "warning: COLIBRI_ENGINE=tree is ignored: the tree-walking interpreter was removed, and the VM runs the program\n"
    );
    for engine in [None, Some("vm"), Some("jit")] {
        let out = run(&file, engine);
        assert_eq!(stderr(&out), "", "{engine:?}");
    }
}
