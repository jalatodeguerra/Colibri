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
    let path = temp_file("unsupported.colibri", "fn id(x: vec2) -> vec2 => x\n");
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

#[test]
fn main_returning_err_reports_on_stderr_and_exits_one() {
    let path = temp_file(
        "main_err.colibri",
        "fn main() -> Result[()] {\n    print(\"start\")\n    return err(\"it broke\")\n}\n",
    );
    let out = colibri(&["run", path.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "start\n");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.starts_with("error: it broke"), "{stderr}");
}

#[test]
fn main_returning_ok_exits_zero() {
    let path = temp_file(
        "main_ok.colibri",
        "fn main() -> Result[()] {\n    print(\"fine\")\n}\n",
    );
    let out = colibri(&["run", path.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0));
}

/// Builds an interface file with the host-side library and returns its path.
fn interface_file(name: &str, with_sub: bool) -> PathBuf {
    let mut vm = colibri_runtime::Vm::builder().build();
    vm.register_fn("engine_add", |a: i64, b: i64| a + b)
        .unwrap();
    if with_sub {
        vm.register_fn("engine_sub", |a: i64, b: i64| a - b)
            .unwrap();
    }
    temp_file(name, &vm.interface_text())
}

#[test]
fn check_reads_interface_files() {
    let cbi = interface_file("engine.cbi", false);
    let ok = temp_file(
        "uses_engine.colibri",
        "fn main() {\n    print(engine_add(1, 2))\n}\n",
    );
    let out = colibri(&[
        "check",
        "--interface",
        cbi.to_str().unwrap(),
        ok.to_str().unwrap(),
    ]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let bad = temp_file(
        "misuses_engine.colibri",
        "fn main() {\n    print(engine_add(1))\n}\n",
    );
    let out = colibri(&[
        "check",
        "--interface",
        cbi.to_str().unwrap(),
        bad.to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("engine_add"));

    // Without the interface the host function is unknown.
    let out = colibri(&["check", ok.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn check_takes_several_interface_files() {
    let a = interface_file("a.cbi", false);
    let mut vm = colibri_runtime::Vm::builder().build();
    vm.register_fn("other_fn", || 1i64).unwrap();
    let b = temp_file("b.cbi", &vm.interface_text());
    let src = temp_file(
        "uses_both.colibri",
        "fn main() {\n    print(engine_add(1, 2))\n    print(other_fn())\n}\n",
    );
    let out = colibri(&[
        "check",
        "--interface",
        a.to_str().unwrap(),
        "--interface",
        b.to_str().unwrap(),
        src.to_str().unwrap(),
    ]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn check_refuses_an_edited_interface_file() {
    let cbi = interface_file("edited.cbi", false);
    let text = std::fs::read_to_string(&cbi)
        .unwrap()
        .replace("engine_add", "engine_plus");
    std::fs::write(&cbi, text).unwrap();
    let src = temp_file("uses_edited.colibri", "fn main() {}\n");
    let out = colibri(&[
        "check",
        "--interface",
        cbi.to_str().unwrap(),
        src.to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("edited.cbi"));
}

// ---- colibri fmt ----

use std::io::Write;
use std::process::Stdio;

/// A fresh empty directory for one test.
fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("colibri-fmt-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn write(dir: &std::path::Path, name: &str, text: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, text).unwrap();
    path
}

const MESSY: &str = "fn main() {\n  print(1);  print(2)\n}\n";
const TIDY: &str = "fn main() {\n    print(1)\n    print(2)\n}\n";

#[test]
fn fmt_rewrites_files_in_place() {
    let dir = temp_dir("rewrite");
    let path = write(&dir, "a.colibri", MESSY);
    let out = colibri(&["fmt", path.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), TIDY);
    // A second run changes nothing.
    let out = colibri(&["fmt", "--check", path.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn fmt_check_lists_files_that_would_change_and_changes_nothing() {
    let dir = temp_dir("check");
    let messy = write(&dir, "messy.colibri", MESSY);
    let tidy = write(&dir, "tidy.colibri", TIDY);
    let out = colibri(&[
        "fmt",
        "--check",
        messy.to_str().unwrap(),
        tidy.to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("messy.colibri"), "{stderr}");
    assert!(!stderr.contains("tidy.colibri"), "{stderr}");
    assert_eq!(std::fs::read_to_string(&messy).unwrap(), MESSY);
}

#[test]
fn fmt_walks_directories_but_skips_target_and_hidden_ones() {
    let dir = temp_dir("walk");
    let a = write(&dir, "src/a.colibri", MESSY);
    let b = write(&dir, "b.colibri", MESSY);
    let skipped = [
        write(&dir, "target/t.colibri", MESSY),
        write(&dir, ".hidden/h.colibri", MESSY),
        write(&dir, "notes.txt", MESSY),
    ];
    let out = colibri(&["fmt", dir.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(std::fs::read_to_string(a).unwrap(), TIDY);
    assert_eq!(std::fs::read_to_string(b).unwrap(), TIDY);
    for path in skipped {
        assert_eq!(std::fs::read_to_string(path).unwrap(), MESSY);
    }
}

#[test]
fn fmt_without_a_path_formats_the_current_directory() {
    let dir = temp_dir("cwd");
    let path = write(&dir, "x/a.colibri", MESSY);
    let out = Command::new(env!("CARGO_BIN_EXE_colibri"))
        .arg("fmt")
        .current_dir(&dir)
        .output()
        .expect("the colibri binary runs");
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(std::fs::read_to_string(path).unwrap(), TIDY);
}

#[test]
fn fmt_refuses_files_with_syntax_errors_and_leaves_them_alone() {
    let dir = temp_dir("refuse");
    let bad = write(&dir, "bad.colibri", "let x = (1 2)\n");
    let good = write(&dir, "good.colibri", MESSY);
    let out = colibri(&["fmt", bad.to_str().unwrap(), good.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("expected `,` or `)`"));
    assert_eq!(std::fs::read_to_string(&bad).unwrap(), "let x = (1 2)\n");
    // The other file is still formatted.
    assert_eq!(std::fs::read_to_string(&good).unwrap(), TIDY);
}

#[test]
fn fmt_dash_formats_stdin_to_stdout() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_colibri"))
        .args(["fmt", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the colibri binary runs");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(MESSY.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&out.stdout), TIDY);
}
