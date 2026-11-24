//! End-to-end tests of the `colibri` binary.

mod common;
use common::vm_pending;

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
        stderr.contains("more calls (set COLIBRI_FULL_TRACE=1 to show them all) ..."),
        "long traces are elided: {stderr}"
    );
    assert!(stderr.lines().count() < 40, "{stderr}");
    let full = common::checked(
        colibri_command()
            .args(["run", path.to_str().unwrap()])
            .env("COLIBRI_FULL_TRACE", "1")
            .output()
            .unwrap(),
    );
    let full = String::from_utf8_lossy(&full.stderr);
    assert!(!full.contains("more calls"), "{full}");
    assert!(full.lines().count() > 5000, "{}", full.lines().count());
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
    let path = temp_file("unsupported.colibri", "fn id(x: [int; 3]) {}\n");
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

/// A host class of one module that a function of another returns.
struct Value(i64);
impl colibri_runtime::HostType for Value {
    const NAME: &'static str = "Value";
    fn describe(d: &mut colibri_runtime::TypeBuilder<'_, Self>) {
        d.method("int", [], |this: &Value| this.0);
    }
}

/// The interface files of two modules, the second naming the first's `Value`.
fn module_interfaces(prefix: &str) -> (PathBuf, PathBuf) {
    let mut vm = colibri_runtime::Vm::builder().build();
    let mut json = vm.module("jsonish");
    json.register_type::<Value>().unwrap();
    let json = temp_file(&format!("{prefix}_jsonish.cbi"), &json.interface_text());
    let mut toml = vm.module("tomlish");
    toml.uses("jsonish").unwrap();
    toml.register_fn_named("parse", ["text"], |t: String| Value(t.len() as i64))
        .unwrap();
    let toml = temp_file(&format!("{prefix}_tomlish.cbi"), &toml.interface_text());
    (json, toml)
}

#[test]
fn check_reads_interfaces_whose_types_name_another_module() {
    let (json, toml) = module_interfaces("both");
    let src = temp_file(
        "uses_modules.colibri",
        "import tomlish\nimport jsonish\nfn main() {\n    let v: jsonish.Value = tomlish.parse(\"abc\")\n    print(v.int())\n}\n",
    );
    let out = colibri(&[
        "check",
        "--interface",
        json.to_str().unwrap(),
        "--interface",
        toml.to_str().unwrap(),
        src.to_str().unwrap(),
    ]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // The type is the module's: the message says `jsonish.Value`.
    let bad = temp_file(
        "misuses_modules.colibri",
        "import tomlish\nfn main() {\n    let v: int = tomlish.parse(\"abc\")\n}\n",
    );
    let out = colibri(&[
        "check",
        "--interface",
        json.to_str().unwrap(),
        "--interface",
        toml.to_str().unwrap(),
        bad.to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("jsonish.Value"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn check_says_which_interface_file_is_missing() {
    let (_, toml) = module_interfaces("alone");
    let src = temp_file("alone.colibri", "import tomlish\nfn main() {}\n");
    let out = colibri(&[
        "check",
        "--interface",
        toml.to_str().unwrap(),
        src.to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("tomlish") && stderr.contains("jsonish"),
        "{stderr}"
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

/// A fresh folder holding `files` (`(relative path, text)`).
fn folder(name: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("colibri-cli-dir-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for (path, text) in files {
        let full = dir.join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, text).unwrap();
    }
    dir
}

const SCRIPT: &str = "import helper\nfn main() {\n    print(helper.greet(\"you\"))\n}\n";
const HELPER: &str = "pub fn greet(who: str) -> str {\n    return \"hello {who}\"\n}\n";

#[test]
fn run_a_script_loads_its_folder_as_an_implicit_package() {
    let dir = folder(
        "script",
        &[("script.colibri", SCRIPT), ("helper.colibri", HELPER)],
    );
    let out = colibri(&["run", dir.join("script.colibri").to_str().unwrap()]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hello you\n");
}

#[test]
fn run_a_folder_runs_the_package_in_it() {
    let dir = folder(
        "pkg",
        &[
            ("colibri.toml", "[package]\nname = \"demo\"\n"),
            ("main.colibri", SCRIPT),
            ("helper.colibri", HELPER),
        ],
    );
    let out = colibri(&["run", dir.to_str().unwrap()]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hello you\n");
}

#[test]
fn run_and_check_without_a_file_use_the_current_folder() {
    let dir = folder(
        "cwd",
        &[
            ("colibri.toml", "[package]\nname = \"demo\"\n"),
            ("main.colibri", SCRIPT),
            ("helper.colibri", HELPER),
        ],
    );
    let run = |arg: &str| {
        common::checked(
            colibri_command()
                .arg(arg)
                .current_dir(&dir)
                .output()
                .expect("the colibri binary runs"),
        )
    };
    let out = run("run");
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hello you\n");
    let out = run("check");
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.stdout.is_empty());
}

#[test]
fn an_error_in_an_imported_file_names_that_file() {
    let dir = folder(
        "err",
        &[
            ("colibri.toml", "[package]\nname = \"demo\"\n"),
            ("main.colibri", SCRIPT),
            (
                "helper.colibri",
                "pub fn greet(who: str) -> str {\n    return 1\n}\n",
            ),
        ],
    );
    let out = colibri(&["check", dir.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("helper.colibri"));
}

#[test]
fn check_interface_accepts_a_named_module() {
    let mut vm = colibri_runtime::Vm::builder().build();
    vm.module("physics")
        .register_fn("gravity", || 10i64)
        .unwrap();
    let cbi = temp_file("physics.cbi", &vm.module("physics").interface_text());
    let src = temp_file(
        "uses_physics.colibri",
        "import physics\nfn main() {\n    print(physics.gravity())\n}\n",
    );
    let out = colibri(&[
        "check",
        "--interface",
        cbi.to_str().unwrap(),
        src.to_str().unwrap(),
    ]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Without it `import physics` finds nothing.
    let out = colibri(&["check", src.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("physics"));
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
    let out = common::checked(
        colibri_command()
            .arg("fmt")
            .current_dir(&dir)
            .output()
            .expect("the colibri binary runs"),
    );
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
    let mut child = colibri_command()
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

#[test]
fn run_passes_the_arguments_after_the_file_to_os_args() {
    let path = temp_file(
        "args.colibri",
        "import os\n\nfn main() {\n    for a in os.args() {\n        print(a)\n    }\n    print(\"{os.args().len()}\")\n}\n",
    );
    let out = colibri(&["run", path.to_str().unwrap(), "a", "--flag", "b c"]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "a\n--flag\nb c\n3\n");
    let out = colibri(&["run", path.to_str().unwrap(), "--", "-x"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "-x\n1\n");
    let out = colibri(&["run", path.to_str().unwrap()]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "0\n");
}

#[test]
fn run_keeps_running_coroutines_after_main_with_real_time() {
    vm_pending(
        "run_keeps_running_coroutines_after_main_with_real_time",
        || {
            let path = temp_file(
                "coro_loop.colibri",
                "fn w() {\n    wait(0.05)\n    print(\"late\")\n}\nfn main() {\n    go w()\n    print(\"main\")\n}\n",
            );
            let started = std::time::Instant::now();
            let out = colibri(&["run", path.to_str().unwrap()]);
            assert!(out.status.success());
            assert_eq!(String::from_utf8_lossy(&out.stdout), "main\nlate\n");
            assert!(started.elapsed().as_millis() >= 50);
        },
    )
}

#[test]
fn a_panic_in_a_coroutine_after_main_exits_101() {
    vm_pending("a_panic_in_a_coroutine_after_main_exits_101", || {
        let path = temp_file(
            "coro_panic.colibri",
            "fn w() {\n    wait(0.01)\n    panic(\"in coroutine\")\n}\nfn main() {\n    go w()\n}\n",
        );
        let out = colibri(&["run", path.to_str().unwrap()]);
        assert_eq!(out.status.code(), Some(101));
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains("in coroutine") && err.contains("w"), "{err}");
    })
}

const CYCLE: &str = "class Node {\n    name: str\n    next: Node? = none\n}\nfn main() {\n    let a = Node { name: \"a\" }\n    a.next = some(a)\n    print(\"ran\")\n}\n";

#[test]
fn run_without_leaks_flag_says_nothing_about_cycles() {
    let path = temp_file("leak_off.colibri", CYCLE);
    let out = colibri(&["run", path.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0));
    assert!(out.stderr.is_empty());
}

#[test]
fn run_leaks_prints_a_warning_and_keeps_the_exit_code() {
    let path = temp_file("leak_warn.colibri", CYCLE);
    let out = colibri(&["run", "--leaks", path.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "ran\n");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("warning: 1 leaked reference cycle"), "{err}");
    assert!(
        err.contains(r#"Node(name: "a") -> next -> Node(name: "a")"#),
        "{err}"
    );
    assert!(err.contains("leak_warn.colibri:6"), "{err}");
}

#[test]
fn run_leaks_error_exits_one() {
    let path = temp_file("leak_error.colibri", CYCLE);
    let out = colibri(&["run", "--leaks=error", path.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("error: 1 leaked reference cycle"));
}

#[test]
fn run_leaks_is_quiet_when_nothing_leaks() {
    let path = temp_file(
        "leak_none.colibri",
        "fn main() {\n    print(\"clean\")\n}\n",
    );
    let out = colibri(&["run", "--leaks=error", path.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0));
    assert!(out.stderr.is_empty());
}

/// A package whose `stray.colibri` has a type error and is imported by nobody.
fn package_with_stray_module(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("colibri-cli-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    std::fs::write(dir.join("colibri.toml"), "[package]\nname = \"demo\"\n").unwrap();
    std::fs::write(dir.join("main.colibri"), "fn main() {\n    print(1)\n}\n").unwrap();
    std::fs::write(
        dir.join("sub/stray.colibri"),
        "pub fn bad() -> int => \"text\"\n",
    )
    .unwrap();
    dir
}

#[test]
fn check_looks_at_package_modules_the_entry_does_not_import() {
    let dir = package_with_stray_module("check_stray");
    let out = colibri(&["check", dir.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("stray.colibri"));
    // `run` only needs what the program uses.
    let out = colibri(&["run", dir.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "1\n");
}

#[test]
fn check_of_a_package_with_a_script_beside_it_is_unchanged() {
    let dir = package_with_stray_module("check_script");
    // A single file keeps checking only what it imports.
    let out = colibri(&["check", dir.join("main.colibri").to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
}

#[test]
fn unused_code_is_a_warning_that_changes_no_exit_code() {
    let path = temp_file(
        "unused_code.colibri",
        "import math\n\nfn main() {\n    let left_over = 1\n    print(\"ran\")\n}\n",
    );
    let file = path.to_str().unwrap();
    let out = colibri(&["check", file]);
    assert_eq!(out.status.code(), Some(0));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("Warning"), "{stderr}");
    assert!(stderr.contains("unused variable `left_over`"), "{stderr}");
    assert!(stderr.contains("unused import `math`"), "{stderr}");
    let out = colibri(&["run", file]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "ran\n");
}

#[test]
fn new_creates_a_package_that_checks_runs_and_passes_its_tests() {
    let root = std::env::temp_dir().join(format!("colibri-cli-new-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let dir = root.join("hello_app");
    let d = dir.to_str().unwrap();
    let out = colibri(&["new", d]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert!(dir.join("colibri.toml").is_file());
    assert!(dir.join("main.colibri").is_file());
    let manifest = std::fs::read_to_string(dir.join("colibri.toml")).unwrap();
    assert!(manifest.contains("name = \"hello_app\""), "{manifest}");

    let out = colibri(&["check", d]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert!(
        out.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = colibri(&["run", d]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "Hello, world!\n");
    let out = colibri(&["test", d]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("1 passed"),
        "{out:?}"
    );

    // A folder with something in it is left alone.
    let out = colibri(&["new", d]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("not an empty folder"));
    // An empty existing folder is fine; an invalid name is refused and creates nothing.
    std::fs::create_dir_all(root.join("empty_one")).unwrap();
    let out = colibri(&["new", root.join("empty_one").to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let bad = root.join("my-app");
    let out = colibri(&["new", bad.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("not a valid package name"));
    assert!(!bad.exists());
    // A keyword cannot be imported, so it is no package name either.
    let kw = root.join("fn");
    let out = colibri(&["new", kw.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("not a valid package name"));
    assert!(!kw.exists());
    let _ = std::fs::remove_dir_all(&root);
}

/// A value of `std.json`'s `Value`, passed through by a host function of another module.
struct JsonValue(colibri_runtime::Value);
impl colibri_runtime::FromColibri for JsonValue {
    fn ty(reg: &mut colibri_runtime::TypeRegistry) -> colibri_runtime::TypeDesc {
        reg.foreign_type("std.json", "Value")
    }
    fn from_colibri(
        v: &colibri_runtime::Value,
        _cx: &mut colibri_runtime::Cx<'_>,
    ) -> Result<Self, colibri_runtime::Error> {
        Ok(JsonValue(v.clone()))
    }
}
impl colibri_runtime::IntoColibri for JsonValue {
    fn ty(reg: &mut colibri_runtime::TypeRegistry) -> colibri_runtime::TypeDesc {
        reg.foreign_type("std.json", "Value")
    }
    fn into_colibri(self, _cx: &mut colibri_runtime::Cx<'_>) -> colibri_runtime::Value {
        self.0
    }
}

#[test]
fn check_interface_of_a_module_that_uses_a_standard_module() {
    let mut vm = colibri_runtime::Vm::builder().build();
    colibri_std::install(&mut vm).unwrap();
    let mut cfg = vm.module("cfg");
    cfg.uses("std.json").unwrap();
    cfg.register_fn("same", |j: JsonValue| j).unwrap();
    let text = cfg.interface_text();
    assert!(text.contains("uses = [\"std.json\"]"), "{text}");
    let cbi = temp_file("cfg.cbi", &text);
    let src = temp_file(
        "ucfg.colibri",
        "import cfg\nimport json\nfn main() {\n    print(cfg.same(json.Value.null()))\n}\n",
    );
    let out = colibri(&[
        "check",
        "--interface",
        cbi.to_str().unwrap(),
        src.to_str().unwrap(),
    ]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // A program that imports only `cfg` needs `std.json` too, and gets it.
    let alone = temp_file("ucfg_alone.colibri", "import cfg\nfn main() {}\n");
    let out = colibri(&[
        "check",
        "--interface",
        cbi.to_str().unwrap(),
        alone.to_str().unwrap(),
    ]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn a_manifest_is_not_an_interface_file() {
    let dir = std::env::temp_dir().join(format!("colibri-cli-iface-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("dir");
    let src = temp_file("iface_user.colibri", "fn main() {}\n");
    for name in ["colibri.toml", "colibri.lock"] {
        let file = dir.join(name);
        std::fs::write(
            &file,
            "[package]\nname = \"x\"\nurl = \"https://user:SeCrEt@example.invalid/r\" \n",
        )
        .expect("file");
        let out = colibri(&[
            "check",
            "--interface",
            file.to_str().unwrap(),
            src.to_str().unwrap(),
        ]);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(1), "{stderr}");
        assert!(
            stderr.contains("not a script") && stderr.contains(name),
            "{stderr}"
        );
        assert!(!stderr.contains("SeCrEt"), "{stderr}");
    }
    let _ = std::fs::remove_dir_all(dir);
}

const WHEN_PROGRAM: &str = "@when(debug)\nfn mode() -> str => \"debug\"\n@when(!debug)\nfn mode() -> str => \"release\"\n@when(fast)\nfn speed() -> str => \"fast\"\n@when(!fast)\nfn speed() -> str => \"slow\"\nfn main() {\n    print(\"{mode()} {speed()}\")\n}\n";

#[test]
fn when_follows_release_and_feature_flags() {
    let dir = std::env::temp_dir().join(format!("colibri-cli-when-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("colibri.toml"),
        "[package]\nname = \"w\"\n\n[features]\ndefault = []\nfast = []\n",
    )
    .unwrap();
    std::fs::write(dir.join("main.colibri"), WHEN_PROGRAM).unwrap();
    let d = dir.to_str().unwrap();
    let run = |extra: &[&str]| -> (Option<i32>, String, String) {
        let mut args = vec!["run", d];
        args.extend_from_slice(extra);
        let out = colibri(&args);
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };
    assert_eq!(run(&[]).1, "debug slow\n");
    assert_eq!(run(&["--release"]).1, "release slow\n");
    assert_eq!(run(&["--feature", "fast"]).1, "debug fast\n");
    assert_eq!(run(&["--release", "--feature=fast"]).1, "release fast\n");
    // A feature the manifest does not declare is refused at the manifest.
    let (code, _, stderr) = run(&["--feature", "turbo"]);
    assert_eq!(code, Some(1));
    assert!(
        stderr.contains("the feature `turbo` is not declared in `colibri.toml`"),
        "{stderr}"
    );
    // `check` and `test` take the same flags.
    let check = colibri(&["check", d, "--feature", "fast", "--release"]);
    assert_eq!(check.status.code(), Some(0));
    // A script has no manifest: any flag is a feature.
    let script = temp_file("when_script.colibri", WHEN_PROGRAM);
    let out = colibri(&["run", script.to_str().unwrap(), "--feature", "fast"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "debug fast\n");
}

#[test]
fn the_hint_for_an_unknown_when_name_in_a_package_can_be_followed() {
    let dir = std::env::temp_dir().join(format!("colibri-cli-when-hint-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("colibri.toml"), "[package]\nname = \"w\"\n").unwrap();
    std::fs::write(
        dir.join("main.colibri"),
        "@when(fast)\nfn speed() -> int => 1\nfn main() {\n    print(1)\n}\n",
    )
    .unwrap();
    let d = dir.to_str().unwrap();
    let out = colibri(&["check", d]);
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("unknown name `fast` in `@when`"),
        "{stderr}"
    );
    assert!(
        stderr.contains("declare the feature in `colibri.toml`")
            && stderr.contains("then turn it on with `--feature fast`"),
        "{stderr}"
    );
    // The flag alone is refused, as the hint says it must come after the declaration.
    let out = colibri(&["check", d, "--feature", "fast"]);
    assert_eq!(out.status.code(), Some(1));
    // Following the hint: declare it, then turn it on.
    std::fs::write(
        dir.join("colibri.toml"),
        "[package]\nname = \"w\"\n\n[features]\nfast = []\n",
    )
    .unwrap();
    let out = colibri(&["check", d, "--feature", "fast"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stderr}");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn update_given_the_manifest_itself_asks_for_the_folder_and_interface_asks_for_a_file() {
    let dir = std::env::temp_dir().join(format!("colibri-cli-manifest-arg-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let manifest = dir.join("colibri.toml");
    std::fs::write(&manifest, "[package]\nname = \"w\"\n").unwrap();
    let out = colibri(&["update", "--path", manifest.to_str().unwrap()]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains("is the manifest of a package, not a script")
            && stderr.contains("pass the folder that holds it instead"),
        "{stderr}"
    );
    assert!(!stderr.contains("there is no `colibri.toml`"), "{stderr}");
    // An interface is a file, so the advice to pass a folder does not fit there.
    let script = dir.join("s.colibri");
    std::fs::write(&script, "fn main() {}\n").unwrap();
    let out = colibri(&[
        "check",
        "--interface",
        manifest.to_str().unwrap(),
        script.to_str().unwrap(),
    ]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("is the manifest of a package, not a script"),
        "{stderr}"
    );
    assert!(!stderr.contains("pass the folder"), "{stderr}");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn feature_names_on_the_command_line_follow_the_manifest_rules() {
    let script = temp_file("feature_names.colibri", "fn main() { print(1) }\n");
    let s = script.to_str().unwrap();
    for (name, why) in [
        ("debug", "meaning of its own"),
        ("os", "meaning of its own"),
        ("true", "meaning of its own"),
        ("", "cannot be empty"),
        ("a b", "is not a valid feature name"),
    ] {
        let out = colibri(&["run", s, "--feature", name]);
        assert_eq!(out.status.code(), Some(2), "[{name}]");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains(why), "[{name}] {stderr}");
    }
    let out = colibri(&["run", s, "--feature", "fast_2"]);
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn import_vec_checks_runs_and_leaves_the_library_alone() {
    let source =
        "import vec\n\nfn main() {\n    print(vec.vec3(1, 2, 3).cross(vec.vec3(0, 1, 0)))\n}\n";
    let path = temp_file("uses_vec.colibri", source);
    let out = colibri(&["check", path.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0));
    assert!(
        out.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = colibri(&["run", path.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "vec3(-3.0, 0.0, 1.0)\n"
    );
    // The library is not a file of the program: the formatter has nothing of it to touch.
    let out = colibri(&["fmt", "--check", path.to_str().unwrap()]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn a_panic_inside_vec_names_the_library_in_the_trace() {
    let source = "import vec\n\nfn main() {\n    print(vec.ivec2(1, 1) / vec.ivec2(1, 0))\n}\n";
    let path = temp_file("vec_panics.colibri", source);
    let out = colibri(&["run", path.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(101));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("division by zero"), "{stderr}");
    assert!(stderr.contains("<std>/vec.colibri"), "{stderr}");
}

/// The binary under test. It inherits `COLIBRI_ENGINE`, so these tests also run the programs they
/// spawn on the VM (see `vm_pending.txt` for the ones that cannot pass there yet).
fn colibri_command() -> std::process::Command {
    std::process::Command::new(env!("CARGO_BIN_EXE_colibri"))
}
