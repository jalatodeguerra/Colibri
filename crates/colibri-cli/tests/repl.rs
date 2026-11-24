//! `colibri repl` with piped input (no prompt, no line editor).

use std::io::Write;
use std::process::Stdio;

/// Runs the REPL on `input`; stdout and stderr.
fn repl(input: &str) -> (String, String) {
    let mut child = colibri_command()
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
    let (out, _) = repl("let x = 10\nlet mut n = 0\nprint(\"once\")\nn += 1\nn += 1\nn\nx * 2\n");
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
    let input = "fn sum(xs: List[int]) -> int {\n  let mut t = 0\n  for v in xs {\n    t += v\n  }\n  return t\n}\nsum([1, 2, 3])\nfor i in 0..3 {\n  print(i)\n}\n";
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
    let (out, err) = repl("let mut n = 1\nn = n / 0\nlet z = 1 / 0\nn\nz\n");
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
fn import_vec_loads_the_library_written_in_colibri() {
    let (out, err) = repl(
        "import vec\nlet a = vec.vec2(3, 4)\na.len()\na + vec.vec2(1, 1)\n2 * a\nimport vec.{ivec2}\nivec2(1, 2).to_vec()\n",
    );
    assert_eq!(
        out,
        "= 5.0 : f32\n= vec2(4.0, 5.0) : vec2\n= vec2(6.0, 8.0) : vec2\n= vec2(1.0, 2.0) : vec2\n"
    );
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
    let input = "let mut fs: List[fn(int) -> int] = []\nfs.push(fn(n: int) => n * 3)\nfn later() -> int { return 1 }\nfs[0](4)\nlet g = fn(a: int) => a + 100\nfn other() -> int { return 2 }\ng(1)\nfs[0](5)\n";
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
    let (out, err) = repl("let mut a: List[int] = []; a.push(1); a.push(2); let n = a.len(); n\n");
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

#[test]
fn load_refuses_a_manifest_and_prints_nothing_of_it() {
    let dir = std::env::temp_dir().join(format!("colibri-repl-manifest-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("dir");
    let text = "[package]\nname = \"x\"\nurl = \"https://user:SeCrEt@example.invalid/r\"\n";
    for name in ["colibri.toml", "colibri.lock"] {
        let file = dir.join(name);
        std::fs::write(&file, text).expect("file");
        let (out, err) = repl(&format!(":load {}\n1 + 1\n", file.display()));
        assert_eq!(out, "= 2 : int\n", "{err}");
        assert!(err.contains("not a script") && err.contains(name), "{err}");
        // `:load` reads one script: there is no folder to pass instead. The path in the message
        // can hold any word, so the message itself is what is checked.
        assert!(!err.contains("pass the folder"), "{err}");
        assert!(err.trim_end().ends_with("not a script"), "{err}");
        assert!(!err.contains("SeCrEt") && !out.contains("SeCrEt"), "{err}");
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn an_attribute_on_its_own_line_waits_for_the_declaration() {
    // The formatter writes the attribute above the item; the item must not run without it.
    let (out, err) =
        repl("@when(false)\nfn z() -> int => 2\nz()\n@when(true)\nfn y() -> int => 3\ny()\n");
    assert!(!err.contains("expected an item"), "{err}");
    assert!(err.contains("unknown name `z`"), "{err}");
    assert!(
        err.contains("`z` exists but is left out here by `@when(false)`"),
        "{err}"
    );
    assert!(out.contains("= 3 : int"), "{out}");
}

#[test]
fn a_deprecated_use_warns_in_the_repl() {
    let (out, err) = repl("@deprecated(\"use g\")\nfn f() -> int => 1\nf()\nf()\n");
    assert_eq!(out.matches("= 1 : int").count(), 2, "{out}");
    // Each use warns once, in its own entry; the definition does not warn.
    assert_eq!(err.matches("`f` is deprecated: use g").count(), 4, "{err}"); // message + label
    // An import that nothing uses yet is not reported.
    let (_, err) = repl("import math\n");
    assert!(!err.contains("unused import"), "{err}");
}

#[test]
fn two_when_variants_of_one_name_keep_both() {
    // The documented pattern: one name, two conditions. Typed one after the other, each stays.
    let (out, err) = repl(
        "@when(debug)\nfn mode() -> str => \"debug\"\n@when(!debug)\nfn mode() -> str => \"release\"\nmode()\n",
    );
    assert_eq!(err, "");
    assert!(
        out.contains("= \"debug\" : str") || out.contains("= \"release\" : str"),
        "{out}"
    );
    // Typed as one entry.
    let (out, err) = repl(
        "@when(debug)\nfn mode() -> str => \"debug\"\n@when(!debug)\nfn mode() -> str => \"release\"\n\nmode()\n",
    );
    assert_eq!(err, "");
    assert!(
        out.contains("= \"debug\" : str") || out.contains("= \"release\" : str"),
        "{out}"
    );
    // An ordinary function can still be redefined, and so can one variant.
    let (out, err) = repl(
        "fn g() -> int => 1\nfn g() -> int => 2\ng()\n@when(false)\nfn h() -> int => 1\n@when(true)\nfn h() -> int => 5\n@when(true)\nfn h() -> int => 6\nh()\n",
    );
    assert_eq!(err, "");
    assert!(
        out.contains("= 2 : int") && out.contains("= 6 : int"),
        "{out}"
    );
}

#[test]
fn loading_a_file_with_two_when_variants_keeps_both() {
    let dir = std::env::temp_dir().join(format!("colibri_repl_when_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("modes.colibri");
    std::fs::write(
        &file,
        "@when(debug)\nfn mode() -> str => \"debug\"\n@when(!debug)\nfn mode() -> str => \"release\"\n",
    )
    .unwrap();
    // The path is a command argument (`:load <file>` reads the rest of the line as a path), not
    // Colibri source, so the native spelling is right here, `\` on Windows included. The
    // folder's own name (`colibri_repl_when_<pid>`) has no spaces.
    let (out, err) = repl(&format!(":load {}\nmode()\n", file.display()));
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(err, "");
    assert!(
        out.contains("= \"debug\" : str") || out.contains("= \"release\" : str"),
        "{out}"
    );
}

#[test]
fn attributes_without_an_item_are_an_error_and_attach_to_nothing() {
    let (out, err) = repl("fn f() -> int => 1\n@deprecated(\"q\")\n\nf()\n");
    assert!(
        err.contains("expected an item after the attributes"),
        "{err}"
    );
    // `f` was not marked deprecated by the dangling attribute.
    assert!(!err.contains("is deprecated"), "{err}");
    assert!(out.contains("= 1 : int"), "{out}");
}

#[test]
fn a_redefinition_with_the_same_condition_replaces_whatever_the_spacing() {
    let (out, err) = repl(
        "@when(debug)\nfn mode() -> int => 1\n@when( debug )\nfn mode() -> int => 2\n@when((debug))\nfn mode() -> int => 3\nmode()\n",
    );
    assert_eq!(err, "");
    assert!(out.contains("= 3 : int"), "{out}");
}

#[test]
fn a_definition_without_when_replaces_every_variant_and_the_other_way_round() {
    // A plain function, then a conditional one: replaced, as before.
    let (out, err) = repl("fn mode() -> int => 1\n@when(debug)\nfn mode() -> int => 2\nmode()\n");
    assert_eq!(err, "");
    assert!(out.contains("= 2 : int"), "{out}");
    // Two variants, then a plain one: only the plain one is left.
    let (out, err) = repl(
        "@when(debug)\nfn mode() -> int => 1\n@when(!debug)\nfn mode() -> int => 2\nfn mode() -> int => 3\nmode()\n",
    );
    assert_eq!(err, "");
    assert!(out.contains("= 3 : int"), "{out}");
    // And a variant can be added again afterwards.
    let (out, err) = repl(
        "fn mode() -> int => 3\n@when(debug)\nfn mode() -> int => 4\nmode()\n@when(!debug)\nfn mode() -> int => 5\nmode()\n",
    );
    assert_eq!(err, "");
    assert!(out.contains("= 4 : int"), "{out}");
}

#[test]
fn redefining_the_left_out_variant_leaves_the_active_one_alone() {
    let (out, err) = repl(
        "@when(debug)\nlet mut counter = 0\n@when(!debug)\nlet mut counter = 100\ncounter += 5\ncounter\n@when(!debug)\nlet mut counter = 200\ncounter\n",
    );
    assert_eq!(err, "");
    assert_eq!(out, "= 5 : int\n= 5 : int\n", "{out}");
    // The same for a function.
    let (out, err) = repl(
        "@when(debug)\nfn f() -> int => 1\n@when(!debug)\nfn f() -> int => 2\n@when(!debug)\nfn f() -> int => 3\nf()\n",
    );
    assert_eq!(err, "");
    assert!(out.contains("= 1 : int"), "{out}");
}

#[test]
fn a_plain_redefinition_takes_the_place_of_the_variant_that_is_in_the_program() {
    // The variant that holds is the second of `a`: the new `a` goes there, behind `b`, so
    // nothing is renumbered.
    let (out, err) = repl(
        "@when(!debug)\nlet mut a = 0\nlet mut b = 9\n@when(debug)\nlet mut a = 1\nlet mut a = 2\na\nb\n",
    );
    assert_eq!(err, "");
    assert_eq!(out, "= 2 : int\n= 9 : int\n", "{out}");
    let (out, err) = repl(
        "@when(!debug)\nfn g() -> int => 0\nfn h() -> int => 9\n@when(debug)\nfn g() -> int => 1\nfn g() -> int => 2\ng()\nh()\n",
    );
    assert_eq!(err, "");
    assert_eq!(out, "= 2 : int\n= 9 : int\n", "{out}");
}

#[test]
fn a_definition_that_is_left_out_here_cannot_replace_the_one_that_is_in_the_program() {
    let (out, err) = repl("fn f() -> int => 1\n@when(!debug)\nfn f() -> int => 2\nf()\n");
    assert!(
        err.contains("`f` would no longer exist") && err.contains("the old definition is kept"),
        "{err}"
    );
    assert!(!err.contains("changed their order"), "{err}");
    assert_eq!(out, "= 1 : int\n", "{out}");
    let (out, err) = repl("let mut x = 1\n@when(!debug)\nlet mut x = 2\nx\n");
    assert!(err.contains("`x` would no longer exist"), "{err}");
    assert!(!err.contains("removes module-level"), "{err}");
    assert_eq!(out, "= 1 : int\n", "{out}");
    // Two variants with a condition each are independent of each other.
    let (out, err) =
        repl("@when(debug)\nfn f() -> int => 1\n@when(!debug)\nfn f() -> int => 2\nf()\n");
    assert_eq!(err, "");
    assert_eq!(out, "= 1 : int\n", "{out}");
}

/// Runs `colibri repl <args>` with `input` on its standard input.
fn repl_with(args: &[&str], input: &str) -> (bool, String, String) {
    let mut child = colibri_command()
        .arg("repl")
        .args(args)
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
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn the_repl_takes_the_build_flags_of_run() {
    let program = "@when(fast)\nfn speed() -> str => \"fast\"\n@when(!fast)\nfn speed() -> str => \"slow\"\n@when(debug)\nfn mode() -> str => \"debug\"\n@when(!debug)\nfn mode() -> str => \"release\"\nspeed()\nmode()\n";
    // Without the flags `fast` is not a name at all, and the build is a debug build.
    let (ok, out, err) = repl_with(&[], "@when(debug)\nfn mode() -> str => \"debug\"\nmode()\n");
    assert!(ok, "{err}");
    assert_eq!(out, "= \"debug\" : str\n", "{err}");
    let (ok, out, err) = repl_with(&["--feature", "fast", "--release"], program);
    assert!(ok, "{err}");
    assert_eq!(out, "= \"fast\" : str\n= \"release\" : str\n", "{err}");
    // More than once, and with the `=` form; no manifest, so any name is a feature.
    let (ok, out, err) = repl_with(
        &[
            "--feature=fast",
            "--feature",
            "other",
            "--no-default-features",
        ],
        "@when(other)\nfn o() -> int => 1\no()\n",
    );
    assert!(ok, "{err}");
    assert_eq!(out, "= 1 : int\n", "{err}");
}

#[test]
fn the_hint_for_a_missing_feature_works_in_the_repl() {
    let (_, _, err) = repl_with(&[], "@when(fast)\nfn f() -> int => 1\nf()\n");
    assert!(err.contains("pass `--feature fast`"), "{err}");
    let (_, out, err) = repl_with(
        &["--feature", "fast"],
        "@when(fast)\nfn f() -> int => 1\nf()\n",
    );
    assert_eq!(out, "= 1 : int\n", "{err}");
    // A name that means something already is refused as it is for `run`.
    let out = colibri_command()
        .args(["repl", "--feature", "debug"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
}

/// The binary under test, pinned to the tree-walker: these tests are about the command line,
/// not the engine, and `COLIBRI_ENGINE=vm` would otherwise reach every process they spawn.
fn colibri_command() -> std::process::Command {
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_colibri"));
    // vm-pin: 2c
    cmd.env_remove("COLIBRI_ENGINE");
    cmd
}
