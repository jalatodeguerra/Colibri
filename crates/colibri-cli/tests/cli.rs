//! End-to-end tests of the `colibri` binary.

use std::path::PathBuf;
use std::process::{Command, Output};

fn colibri(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_colibri"))
        .args(args)
        .output()
        .expect("the colibri binary runs")
}

/// Writes `source` to a unique temporary file.
fn temp_file(name: &str, source: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("colibri-cli-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join(name);
    std::fs::write(&path, source).expect("temp file");
    path
}

#[test]
fn check_is_silent_and_succeeds_on_valid_syntax() {
    let path = temp_file("ok.colibri", "fn main() {\n    print(\"hi\")\n}\n");
    let out = colibri(&["check", path.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0));
    assert!(out.stdout.is_empty());
    assert!(out.stderr.is_empty());
}

#[test]
fn check_reports_syntax_errors_and_fails() {
    let path = temp_file("bad.colibri", "if x = 5 { }\n");
    let out = colibri(&["check", path.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("found assignment `=`"), "{stderr}");
    assert!(stderr.contains("bad.colibri"), "{stderr}");
}

#[test]
fn run_stops_on_syntax_errors() {
    let path = temp_file("bad_run.colibri", "let x = (1 2)\n");
    let out = colibri(&["run", path.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("expected `,` or `)`"));
}

#[test]
fn run_of_valid_syntax_is_not_implemented_yet() {
    let path = temp_file("run_ok.colibri", "print(\"hi\")\n");
    let out = colibri(&["run", path.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("not implemented yet"));
}

#[test]
fn a_missing_file_is_an_error() {
    let out = colibri(&["check", "/nonexistent/nope.colibri"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("cannot read"));
}
