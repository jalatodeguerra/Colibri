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
fn check_is_silent_and_succeeds_on_valid_programs() {
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
fn run_prints_to_stdout_and_exits_zero() {
    let path = temp_file(
        "run_ok.colibri",
        "fn main() {\n    print(\"from main\")\n}\nprint(\"hello {1 + 2}\")\n",
    );
    let out = colibri(&["run", path.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hello 3\nfrom main\n");
    assert!(out.stderr.is_empty());
}

#[test]
fn run_reports_type_errors_with_exit_code_one() {
    let path = temp_file("type_error.colibri", "let x: int = \"s\"\nprint(x)\n");
    let out = colibri(&["run", path.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty(), "nothing runs when checking fails");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("expected `int`, found `str`"), "{stderr}");
    assert!(stderr.contains("type_error.colibri"), "{stderr}");
}

#[test]
fn run_reports_a_panic_with_a_stack_trace_and_exit_code_101() {
    let source = "fn inner(n: int) -> int => 10 / n\nfn outer(n: int) -> int => inner(n - 1)\nprint(\"before\")\nprint(outer(1))\nprint(\"after\")\n";
    let path = temp_file("panics.colibri", source);
    let out = colibri(&["run", path.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(101));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "before\n");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.starts_with("panic: division by zero\n"), "{stderr}");
    assert!(
        stderr.contains("at inner (") && stderr.contains("panics.colibri:1:"),
        "{stderr}"
    );
    assert!(
        stderr.contains("at outer (") && stderr.contains("panics.colibri:2:"),
        "{stderr}"
    );
    assert!(
        stderr.contains("at <script> (") && stderr.contains("panics.colibri:4:"),
        "{stderr}"
    );
}

#[test]
fn deep_recursion_is_a_stack_overflow_panic_not_a_crash() {
    let path = temp_file(
        "overflow.colibri",
        "fn f(n: int) -> int => f(n + 1) + 1\nprint(f(0))\n",
    );
    let out = colibri(&["run", path.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(101));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.starts_with("panic: stack overflow: more than 10000 nested calls"),
        "{stderr}"
    );
    assert!(
        stderr.contains("more calls ..."),
        "long traces are elided: {stderr}"
    );
    assert!(stderr.lines().count() < 40, "{stderr}");
}

#[test]
fn recursion_below_the_limit_works() {
    let path = temp_file(
        "deep.colibri",
        "fn depth(n: int) -> int => if n == 0 { 0 } else { 1 + depth(n - 1) }\nprint(depth(9000))\n",
    );
    let out = colibri(&["run", path.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "9000\n");
}

#[test]
fn run_prints_warnings_and_still_runs() {
    let path = temp_file(
        "warn_run.colibri",
        "fn main() {\n    let x = 1\n    let x = 2\n    print(x)\n}\n",
    );
    let out = colibri(&["run", path.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "2\n");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("shadows an earlier binding"), "{stderr}");
}

#[test]
fn check_type_checks_but_does_not_run() {
    let ok = temp_file("check_ok.colibri", "print(\"must not print\")\n");
    let out = colibri(&["check", ok.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0));
    assert!(out.stdout.is_empty());
    assert!(out.stderr.is_empty());

    let bad = temp_file("check_bad.colibri", "print(nope)\n");
    let out = colibri(&["check", bad.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown name `nope`"));
}

#[test]
fn check_prints_warnings_and_exits_zero() {
    let path = temp_file(
        "check_warn.colibri",
        "fn main() {\n    let x = 1\n    let x = 2\n    print(x)\n}\n",
    );
    let out = colibri(&["check", path.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0));
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).contains("Warning"));
}

#[test]
fn unsupported_features_are_clean_errors() {
    let path = temp_file("unsupported.colibri", "class P { x: int }\n");
    let out = colibri(&["run", path.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("is not supported yet"));
}

#[test]
fn a_missing_file_is_an_error() {
    let out = colibri(&["check", "/nonexistent/nope.colibri"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("cannot read"));
}
