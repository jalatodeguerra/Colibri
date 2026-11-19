//! `colibri repl` with piped input (no prompt, no line editor).

use std::io::Write;
use std::process::{Command, Stdio};

/// Runs the REPL on `input`; stdout and stderr.
fn repl(input: &str) -> (String, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_colibri"))
        .arg("repl")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("colibri starts");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(input.as_bytes())
        .expect("input");
    let out = child.wait_with_output().expect("colibri ends");
    assert!(out.status.success());
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn expressions_print_value_and_type() {
    let (out, err) = repl("1 + 2\n\"hi\"\n[1, 2]\n2.5\ntrue\nprint(\"x\")\n");
    assert_eq!(
        out,
        "= 3 : int\n= \"hi\" : str\n= [1, 2] : List[int]\n= 2.5 : float\n= true : bool\nx\n"
    );
    assert_eq!(err, "");
}

#[test]
fn bindings_persist_and_are_not_rerun() {
    let (out, _) = repl("let x = 10\nvar n = 0\nprint(\"once\")\nn += 1\nn += 1\nn\nx * 2\n");
    assert_eq!(out, "once\n= 2 : int\n= 20 : int\n");
    let (out, _) =
        repl("fn init() -> int {\n  print(\"init\")\n  return 4\n}\nlet x = init()\nx\nx\n");
    assert_eq!(out, "init\n= 4 : int\n= 4 : int\n");
}

#[test]
fn rebinding_replaces_the_value() {
    let (out, _) = repl("let x = 1\nlet x = 2\nx\n");
    assert_eq!(out, "= 2 : int\n");
}

#[test]
fn functions_are_defined_then_called() {
    let (out, err) = repl("fn double(n: int) -> int { return n * 2 }\ndouble(21)\n");
    assert_eq!(out, "= 42 : int\n");
    assert_eq!(err, "");
}

#[test]
fn multi_line_function_and_loop() {
    let input = "fn sum(xs: List[int]) -> int {\n  var t = 0\n  for v in xs {\n    t += v\n  }\n  return t\n}\nsum([1, 2, 3])\nfor i in 0..3 {\n  print(i)\n}\n";
    let (out, err) = repl(input);
    assert_eq!(out, "= 6 : int\n0\n1\n2\n");
    assert_eq!(err, "");
}

#[test]
fn redefining_a_function_replaces_it() {
    let (out, _) = repl("fn f() -> int { return 1 }\nf()\nfn f() -> int { return 2 }\nf()\n");
    assert_eq!(out, "= 1 : int\n= 2 : int\n");
}

#[test]
fn a_check_error_leaves_the_session_intact() {
    let (out, err) = repl("let x = 5\nx + \"a\"\nundefined_name\nlet y: str = 1\nx\n");
    assert_eq!(out, "= 5 : int\n");
    assert!(err.contains("<repl>:1"), "{err}");
    assert_eq!(err.matches("Error").count(), 3, "{err}");
    // The failed `let y` did not define `y`.
    let (_, err) = repl("let y: str = 1\ny\n");
    assert!(err.contains("unknown name `y`"), "{err}");
}

#[test]
fn a_panic_leaves_the_session_intact() {
    let (out, err) = repl("var n = 1\nn = n / 0\nlet z = 1 / 0\nn\nz\n");
    assert_eq!(out, "= 1 : int\n");
    assert!(err.contains("panic: division by zero"), "{err}");
    assert!(err.contains("unknown name `z`"), "{err}");
}

#[test]
fn type_command_shows_the_type_without_running() {
    let (out, _) =
        repl("fn f(a: int) -> int { return a }\n:type f\n:type print(\"never\")\n:type [1.5]\n");
    assert_eq!(out, "fn(int) -> int\n()\nList[float]\n");
}

#[test]
fn reset_forgets_everything() {
    let (out, err) = repl("let x = 1\n:reset\nlet x = \"again\"\nx\nfoo\n");
    assert_eq!(out, "session reset\n= \"again\" : str\n");
    assert!(err.contains("unknown name `foo`"), "{err}");
}

#[test]
fn std_modules_import() {
    let (out, err) = repl("import math\nmath.sqrt(16.0)\nimport math\nmath.floor(2.7)\n");
    assert_eq!(out, "= 4.0 : float\n= 2.0 : float\n");
    assert_eq!(err, "");
}

#[test]
fn types_declare_and_persist() {
    let input = "struct P { a: int }\nlet p = P { a: 7 }\np.a\nenum Dir { Up, Down }\nDir.Up\nstruct P { b: int }\n";
    let (out, err) = repl(input);
    assert_eq!(out, "= 7 : int\n= Up : Dir\n");
    assert!(err.contains("already defined"), "{err}");
}

#[test]
fn closures_survive_later_declarations() {
    let input = "var fs: List[fn(int) -> int] = []\nfs.push(fn(n: int) => n * 3)\nfn later() -> int { return 1 }\nfs[0](4)\nlet g = fn(a: int) => a + 100\nfn other() -> int { return 2 }\ng(1)\nfs[0](5)\n";
    let (out, err) = repl(input);
    assert_eq!(out, "= 12 : int\n= 101 : int\n= 15 : int\n");
    assert_eq!(err, "");
}

#[test]
fn coroutines_run_when_due() {
    let input = "fn work() {\n  print(\"a\")\n  wait(0.0)\n  print(\"b\")\n}\nlet c = go work()\nprint(\"after\")\n";
    let (out, err) = repl(input);
    assert_eq!(out, "a\nb\nafter\n", "{err}");
}

#[test]
fn load_reads_declarations_from_a_file() {
    let dir = std::env::temp_dir().join(format!("colibri-repl-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("dir");
    let file = dir.join("lib.colibri");
    std::fs::write(
        &file,
        "fn triple(n: int) -> int { return n * 3 }\nlet base = 7\n",
    )
    .expect("file");
    let (out, _) = repl(&format!(":load {}\ntriple(base)\n", file.display()));
    assert_eq!(out, "= 21 : int\n");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn unknown_commands_and_quit() {
    let (out, err) = repl(":bogus\n1\n:quit\n2\n");
    assert_eq!(out, "= 1 : int\n");
    assert!(err.contains("unknown command"), "{err}");
}

#[test]
fn an_entry_runs_top_to_bottom() {
    // One entry (one line): the declaration after the statements sees what they did.
    let (out, err) = repl("var a: List[int] = []; a.push(1); a.push(2); let n = a.len(); n\n");
    assert_eq!(out, "= 2 : int\n");
    assert_eq!(err, "");
}

#[test]
fn only_the_last_piece_of_an_entry_shows_its_value() {
    let (out, err) = repl("let y = 1; y + 1; let z = y; print(f()); fn f() -> int => z\n");
    assert_eq!(out, "1\n");
    assert_eq!(err, "");
}

#[test]
fn the_repl_does_not_show_unused_code_warnings() {
    let (out, err) = repl("import math\nfn f() {\n  let unused = 1\n}\nf()\n");
    assert_eq!(out, "", "{err}");
    assert_eq!(err, "");
}
