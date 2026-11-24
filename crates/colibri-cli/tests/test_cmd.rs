//! End-to-end tests of `colibri test`.

mod common;

use std::path::PathBuf;
use std::process::Output;

fn colibri(args: &[&str]) -> Output {
    common::checked(
        colibri_command()
            .args(args)
            .output()
            .expect("the colibri binary runs"),
    )
}

/// A fresh folder holding `files` (`(relative path, text)`).
fn folder(name: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("colibri-test-cmd-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for (path, text) in files {
        let full = dir.join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, text).unwrap();
    }
    dir
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Runs `colibri test` on one script.
fn test_script(name: &str, source: &str, extra: &[&str]) -> Output {
    let dir = folder(name, &[("t.colibri", source)]);
    let file = dir.join("t.colibri");
    let mut args = vec!["test", file.to_str().unwrap()];
    args.extend_from_slice(extra);
    colibri(&args)
}

const PACKAGE: &[(&str, &str)] = &[
    (
        "colibri.toml",
        "[package]\nname = \"demo\"\n\n[dependencies]\nlib = { path = \"lib\" }\n",
    ),
    (
        "main.colibri",
        "import util\nimport lib\n\nfn main() {\n    print(util.double(lib.one()))\n}\n\ntest \"main works\" {\n    assert_eq(util.double(2), 4)\n}\n",
    ),
    (
        "util.colibri",
        "pub fn double(n: int) -> int => n * 2\n\ntest \"double\" {\n    assert_eq(double(21), 42)\n}\n\ntest \"double zero\" {\n    assert_eq(double(0), 0)\n}\n",
    ),
    // Nobody imports it, but its tests run all the same.
    (
        "extra/more.colibri",
        "test \"in a folder nobody imports\" {\n    assert(1 < 2)\n}\n",
    ),
    ("lib/colibri.toml", "[package]\nname = \"lib\"\n"),
    (
        "lib/lib.colibri",
        "pub fn one() -> int => 1\n\ntest \"dependency test, never run\" {\n    assert(false)\n}\n",
    ),
];

#[test]
fn a_passing_package_reports_every_test_and_exits_zero() {
    let dir = folder("pass", PACKAGE);
    let out = colibri(&["test", dir.to_str().unwrap()]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}{}",
        stdout(&out),
        stderr(&out)
    );
    let text = stdout(&out);
    assert!(text.contains("running 4 tests"), "{text}");
    assert!(text.contains("test main::main works ... ok"), "{text}");
    assert!(text.contains("test util::double ... ok"), "{text}");
    assert!(text.contains("test util::double zero ... ok"), "{text}");
    assert!(
        text.contains("test extra.more::in a folder nobody imports ... ok"),
        "{text}"
    );
    assert!(!text.contains("dependency test"), "{text}");
    assert!(text.trim_end().ends_with("4 passed, 0 failed"), "{text}");
}

#[test]
fn a_package_without_a_path_uses_the_current_folder() {
    let dir = folder("cwd", PACKAGE);
    let out = common::checked(
        colibri_command()
            .arg("test")
            .current_dir(&dir)
            .output()
            .unwrap(),
    );
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));
    assert!(stdout(&out).contains("4 passed, 0 failed"));
}

#[test]
fn a_failing_assertion_fails_the_test_with_both_values_and_the_location() {
    let out = test_script(
        "assert_eq",
        "test \"adds\" {\n    assert_eq(1 + 1, 3)\n}\n",
        &[],
    );
    assert_eq!(out.status.code(), Some(1));
    let text = stdout(&out);
    assert!(text.contains("test t::adds ... FAILED"), "{text}");
    assert!(text.contains("---- t::adds ----"), "{text}");
    assert!(text.contains("assertion failed"), "{text}");
    assert!(text.contains("left: 2"), "{text}");
    assert!(text.contains("right: 3"), "{text}");
    assert!(text.contains("t.colibri:2:5"), "{text}");
    assert!(text.trim_end().ends_with("0 passed, 1 failed"), "{text}");
}

#[test]
fn assert_with_a_message_shows_the_message() {
    let out = test_script(
        "assert_msg",
        "test \"msg\" {\n    assert(false, \"the lights are off\")\n}\n",
        &[],
    );
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stdout(&out).contains("panic: the lights are off"),
        "{}",
        stdout(&out)
    );
}

#[test]
fn a_panicking_test_does_not_stop_the_others() {
    let out = test_script(
        "panic",
        "test \"first\" {\n    assert(true)\n}\n\ntest \"boom\" {\n    let zero = 0\n    print(1 / zero)\n}\n\ntest \"last\" {\n    assert(true)\n}\n",
        &[],
    );
    assert_eq!(out.status.code(), Some(1));
    let text = stdout(&out);
    assert!(text.contains("test t::first ... ok"), "{text}");
    assert!(text.contains("test t::boom ... FAILED"), "{text}");
    assert!(text.contains("test t::last ... ok"), "{text}");
    assert!(text.contains("panic: division by zero"), "{text}");
    assert!(text.trim_end().ends_with("2 passed, 1 failed"), "{text}");
}

#[test]
fn a_panic_in_a_defer_fails_the_test_cleanly() {
    let out = test_script(
        "defer",
        "test \"defer panics\" {\n    defer panic(\"from defer\")\n    assert(true)\n}\n\ntest \"after\" {\n    assert(true)\n}\n",
        &[],
    );
    assert_eq!(out.status.code(), Some(1));
    let text = stdout(&out);
    assert!(text.contains("test t::defer panics ... FAILED"), "{text}");
    assert!(text.contains("from defer"), "{text}");
    assert!(text.contains("test t::after ... ok"), "{text}");
}

#[test]
fn a_test_that_returns_err_fails_with_the_error_chain() {
    let source = "fn load() -> Result[int] {\n    return err(\"disk on fire\")\n}\n\ntest \"question mark\" {\n    let n = load().context(\"loading the save\")?\n    assert_eq(n, 1)\n}\n\ntest \"question mark ok\" {\n    let r: Result[int] = ok(1)\n    let n = r?\n    assert_eq(n, 1)\n}\n";
    let out = test_script("err", source, &[]);
    assert_eq!(out.status.code(), Some(1));
    let text = stdout(&out);
    assert!(text.contains("test t::question mark ... FAILED"), "{text}");
    assert!(text.contains("test t::question mark ok ... ok"), "{text}");
    assert!(text.contains("loading the save"), "{text}");
    assert!(text.contains("disk on fire"), "{text}");
}

#[test]
fn a_bare_return_ends_a_test_successfully() {
    let out = test_script(
        "return",
        "test \"early\" {\n    if true {\n        return\n    }\n    assert(false)\n}\n",
        &[],
    );
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));
}

#[test]
fn filter_selects_by_substring_of_the_full_name() {
    let source = "test \"alpha one\" {\n    assert(true)\n}\n\ntest \"alpha two\" {\n    assert(false)\n}\n\ntest \"beta\" {\n    assert(false)\n}\n";
    let out = test_script("filter", source, &["--filter", "alpha one"]);
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));
    let text = stdout(&out);
    assert!(text.contains("running 1 test\n"), "{text}");
    assert!(text.contains("test t::alpha one ... ok"), "{text}");
    assert!(!text.contains("beta"), "{text}");
    assert!(
        text.contains("1 passed, 0 failed (2 filtered out)"),
        "{text}"
    );
    // The module name counts too.
    let out = test_script("filter_mod", source, &["--filter", "t::be"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stdout(&out).contains("test t::beta ... FAILED"));
    // Nothing matches: nothing runs, nothing fails.
    let out = test_script("filter_none", source, &["--filter", "gamma"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(stdout(&out).contains("0 passed, 0 failed (3 filtered out)"));
}

#[test]
fn an_endless_loop_fails_the_test_when_the_budget_runs_out() {
    let source = "test \"spins\" {\n    let mut n = 0\n    while true {\n        n += 1\n    }\n}\n\ntest \"fine\" {\n    assert(true)\n}\n";
    let out = test_script("budget", source, &["--budget", "50000"]);
    assert_eq!(out.status.code(), Some(1));
    let text = stdout(&out);
    assert!(text.contains("test t::spins ... FAILED"), "{text}");
    assert!(text.contains("instruction budget is exhausted"), "{text}");
    assert!(text.contains("--budget"), "{text}");
    assert!(text.contains("test t::fine ... ok"), "{text}");
}

#[test]
fn the_default_budget_is_large_enough_for_ordinary_work() {
    let source = "test \"busy\" {\n    let mut sum = 0\n    for i in 0..200000 {\n        sum += i\n    }\n    assert_eq(sum, 19999900000)\n}\n";
    let out = test_script("busy", source, &[]);
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));
}

#[test]
fn output_is_shown_only_for_failing_tests_unless_nocapture() {
    let source = "test \"quiet\" {\n    print(\"from the passing test\")\n}\n\ntest \"loud\" {\n    print(\"from the failing test\")\n    assert(false)\n}\n";
    let out = test_script("capture", source, &[]);
    let text = stdout(&out);
    assert!(!text.contains("from the passing test"), "{text}");
    let at_failure = text.find("---- t::loud ----").expect(&text);
    let printed = text.find("from the failing test").expect(&text);
    assert!(printed > at_failure, "{text}");

    let out = test_script("nocapture", source, &["--nocapture"]);
    let text = stdout(&out);
    assert!(text.contains("from the passing test"), "{text}");
    assert!(text.contains("from the failing test"), "{text}");
    // Live output comes before the result line of its test.
    assert!(
        text.find("from the passing test") < text.find("test t::quiet ... ok"),
        "{text}"
    );
    // And is not repeated in the failure report.
    assert_eq!(text.matches("from the failing test").count(), 1, "{text}");
}

#[test]
fn every_test_starts_from_fresh_module_state() {
    let source = "let mut counter = 0\nlet names = [\"a\"]\n\nfn bump() -> int {\n    counter += 1\n    return counter\n}\n\ntest \"first\" {\n    assert_eq(bump(), 1)\n    assert_eq(bump(), 2)\n}\n\ntest \"second\" {\n    assert_eq(counter, 0)\n    assert_eq(bump(), 1)\n}\n\ntest \"third\" {\n    assert_eq(counter, 0)\n}\n";
    let out = test_script("fresh", source, &[]);
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));
    assert!(stdout(&out).contains("3 passed, 0 failed"));
}

#[test]
fn top_level_statements_of_a_script_do_not_run() {
    let source = "print(\"top level\")\n\nfn main() {\n    print(\"main\")\n}\n\ntest \"t\" {\n    assert(true)\n}\n";
    let out = test_script("toplevel", source, &["--nocapture"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(!stdout(&out).contains("top level"), "{}", stdout(&out));
    assert!(!stdout(&out).contains("main\n"), "{}", stdout(&out));
}

#[test]
fn drop_hooks_run_in_the_test_that_caused_them() {
    let source = "class Guard {\n    name: str\n}\n\nimpl Drop for Guard {\n    fn drop(self) {\n        print(\"dropped {self.name}\")\n    }\n}\n\nlet shared = Guard { name: \"module\" }\n\ntest \"fails\" {\n    let local = Guard { name: \"local\" }\n    assert(false)\n}\n";
    let out = test_script("drops", source, &[]);
    assert_eq!(out.status.code(), Some(1));
    let text = stdout(&out);
    let report = &text[text.find("---- t::fails ----").expect(&text)..];
    assert!(report.contains("dropped local"), "{text}");
    assert!(report.contains("dropped module"), "{text}");
}

#[test]
fn a_program_with_no_tests_passes() {
    let out = test_script("none", "fn main() {\n    print(\"hi\")\n}\n", &[]);
    assert_eq!(out.status.code(), Some(0));
    let text = stdout(&out);
    assert!(text.contains("running 0 tests"), "{text}");
    assert!(text.contains("0 passed, 0 failed"), "{text}");
}

#[test]
fn a_type_error_inside_a_test_stops_before_running_anything() {
    let out = test_script(
        "typeerr",
        "test \"fine\" {\n    assert(true)\n}\n\ntest \"bad\" {\n    let x: int = \"s\"\n}\n",
        &[],
    );
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("expected `int`, found `str`"),
        "{}",
        stderr(&out)
    );
    assert!(!stdout(&out).contains("running"), "{}", stdout(&out));
}

#[test]
fn check_finds_errors_in_tests_and_run_ignores_the_tests() {
    let dir = folder(
        "check_run",
        &[(
            "s.colibri",
            "test \"bad\" {\n    let x: int = \"s\"\n}\n\nfn main() {\n    print(\"ran\")\n}\n",
        )],
    );
    let file = dir.join("s.colibri");
    let out = colibri(&["check", file.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("expected `int`, found `str`"));
    // `run` type-checks them too.
    let out = colibri(&["run", file.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stdout(&out).is_empty());

    let dir = folder(
        "check_run_ok",
        &[(
            "s.colibri",
            "test \"boom\" {\n    panic(\"never\")\n}\n\nfn main() {\n    print(\"ran\")\n}\n",
        )],
    );
    let file = dir.join("s.colibri");
    let out = colibri(&["check", file.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0));
    let out = colibri(&["run", file.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(stdout(&out), "ran\n");
}

#[test]
fn tests_of_a_script_include_imported_modules_but_not_unrelated_files() {
    let dir = folder(
        "script_imports",
        &[
            (
                "s.colibri",
                "import helper\n\ntest \"own\" {\n    assert_eq(helper.one(), 1)\n}\n",
            ),
            (
                "helper.colibri",
                "pub fn one() -> int => 1\n\ntest \"helper's\" {\n    assert(true)\n}\n",
            ),
            ("stray.colibri", "test \"stray\" {\n    assert(false)\n}\n"),
        ],
    );
    let out = colibri(&["test", dir.join("s.colibri").to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));
    let text = stdout(&out);
    assert!(text.contains("test s::own ... ok"), "{text}");
    assert!(text.contains("test helper::helper's ... ok"), "{text}");
    assert!(!text.contains("stray"), "{text}");
}

#[test]
fn a_missing_path_is_an_error() {
    let out = colibri(&["test", "/no/such/place.colibri"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(!stderr(&out).is_empty());
}

#[test]
fn coroutines_do_not_leak_into_the_next_test() {
    let out = test_script(
        "coroutines",
        "let mut hits = 0\n\nfn w() {\n    wait(10.0)\n    hits += 1\n}\n\ntest \"a starts one\" {\n    go w()\n    task.tick(1.0)\n}\n\ntest \"b sees nothing\" {\n    assert_eq(hits, 0)\n    task.tick(100.0)\n    assert_eq(hits, 0)\n}\n",
        &[],
    );
    assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));
    assert!(stdout(&out).contains("2 passed"), "{}", stdout(&out));
}

const LEAKY_TESTS: &str = "class Node {\n    next: Node? = none\n}\ntest \"clean\" {\n    let _a = Node {}\n}\ntest \"leaky\" {\n    let a = Node {}\n    a.next = some(a)\n}\ntest \"also clean\" {\n    let _a = Node {}\n}\n";

#[test]
fn leaks_flag_reports_the_test_that_leaked() {
    let out = test_script("leaks_warn", LEAKY_TESTS, &["--leaks"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(stdout(&out).contains("3 passed, 0 failed"));
    let err = stderr(&out);
    assert_eq!(err.matches("leaked reference cycle").count(), 1, "{err}");
}

#[test]
fn leaks_error_fails_the_leaking_test_only() {
    let out = test_script("leaks_error", LEAKY_TESTS, &["--leaks=error"]);
    assert_eq!(out.status.code(), Some(1));
    let text = stdout(&out);
    assert!(text.contains("leaky ... FAILED"), "{text}");
    assert!(text.contains("clean ... ok"), "{text}");
    assert!(text.contains("2 passed, 1 failed"), "{text}");
}

#[test]
fn without_the_flag_leaks_are_not_looked_for() {
    let out = test_script("leaks_off", LEAKY_TESTS, &[]);
    assert_eq!(out.status.code(), Some(0));
    assert!(stderr(&out).is_empty());
}

/// The contents of a Colibri string literal for a path: `\` starts an escape in Colibri
/// (`C:\Users` is "unknown escape `\U`") and `{` an interpolation, so a native path pasted
/// into Colibri source (or JSON or TOML text) has to be escaped. Every test here that puts a
/// path into source text goes through this; never `format!("\"{}\"", path.display())`.
fn string_contents(path: &std::path::Path) -> String {
    path.to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('{', "\\{")
        .replace('}', "\\}")
}

/// A package folder with a script module (nothing imports it, it has top-level statements) whose
/// initialiser and statements would leave `marker` behind if anything of it ran.
fn script_package(name: &str, marker: &std::path::Path) -> PathBuf {
    let script = format!(
        "import fs\nlet made = fs.write(\"{}\", \"x\") or |e| {{ panic(e.message()) }}\nprint(\"gen\")\n\ntest \"in the script\" {{\n    assert(false)\n}}\n",
        string_contents(marker)
    );
    folder(
        name,
        &[
            ("colibri.toml", "[package]\nname = \"demo\"\n"),
            (
                "main.colibri",
                "fn main() {}\n\ntest \"main\" {\n    assert(true)\n}\n",
            ),
            ("tools/gen.colibri", &script),
            (
                "plain.colibri",
                "let n = 1\n\ntest \"plain\" {\n    assert_eq(n, 1)\n}\n",
            ),
        ],
    )
}

#[test]
fn a_script_module_of_a_folder_runs_nothing_under_test() {
    let marker = std::env::temp_dir().join(format!("colibri-script-marker-{}", std::process::id()));
    let _ = std::fs::remove_file(&marker);
    let dir = script_package("script_module_inert", &marker);
    let out = colibri(&["test", dir.to_str().unwrap()]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}{}",
        stdout(&out),
        stderr(&out)
    );
    assert!(!marker.exists(), "the script's initialiser ran");
    let text = stdout(&out);
    assert!(text.contains("2 passed, 0 failed"), "{text}");
    assert!(
        !text.contains("gen") && !text.contains("in the script"),
        "{text}"
    );
    let err = stderr(&out);
    assert!(err.contains("skipped 1 test in "), "{err}");
    assert!(
        err.contains("gen.colibri: a script's tests run with"),
        "{err}"
    );
    // A module with no statements stays an ordinary one: its tests ran (`plain`, above).
    assert!(text.contains("plain::plain ... ok"), "{text}");
}

#[test]
fn check_still_finds_errors_in_a_script_modules_statements_and_initialisers() {
    let dir = folder(
        "script_module_errors",
        &[
            ("colibri.toml", "[package]\nname = \"demo\"\n"),
            ("main.colibri", "fn main() {}\n"),
            (
                "tools/gen.colibri",
                "let a: int = \"initialiser\"\nlet b: str = 1 + \"statement\"\nprint(b)\n",
            ),
        ],
    );
    let out = colibri(&["check", dir.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(err.contains("gen.colibri:1"), "{err}");
    assert!(err.contains("gen.colibri:2"), "{err}");
}

/// The binary under test. It inherits `COLIBRI_ENGINE`, so these tests also run the programs they
/// spawn on the VM (see `vm_pending.txt` for the ones that cannot pass there yet).
fn colibri_command() -> std::process::Command {
    std::process::Command::new(env!("CARGO_BIN_EXE_colibri"))
}
