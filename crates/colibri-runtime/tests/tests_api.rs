//! The test API: `Vm::load_tests`, `Vm::tests`, `Vm::run_test` (ARCHITECTURE §15).

use colibri_runtime as colibri;

const SOURCE: &str = r#"
var hits = 0

fn bump() -> int {
    hits += 1
    return hits
}

fn parse(s: str) -> Result[int] {
    return err("cannot parse {s}")
}

test "passes" {
    assert_eq(bump(), 1)
    print("in passes")
}

test "again" {
    assert_eq(bump(), 1)
}

test "fails an assertion" {
    assert_eq(bump(), 2)
}

test "panics" {
    let zero = 0
    print(1 / zero)
}

test "returns an error" {
    let n = parse("x")?
    assert_eq(n, 0)
}

test "spins" {
    while true { }
}

test "ok result" {
    return ok(())
}

fn main() {
    print("main")
}
print("top level")
"#;

fn vm_with(out: &colibri::SharedBuffer) -> colibri::Vm {
    colibri::Vm::builder().stdout(Box::new(out.clone())).build()
}

#[test]
fn tests_lists_the_blocks_in_source_order() {
    let out = colibri::SharedBuffer::new();
    let mut vm = vm_with(&out);
    assert!(vm.tests().is_empty(), "nothing loaded yet");
    vm.load_source("game.colibri", SOURCE).unwrap();
    let tests = vm.tests();
    let names: Vec<&str> = tests.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "passes",
            "again",
            "fails an assertion",
            "panics",
            "returns an error",
            "spins",
            "ok result"
        ]
    );
    assert!(tests.iter().all(|t| t.root && t.module == "game"));
    assert_eq!(
        tests.iter().map(|t| t.id).collect::<Vec<_>>(),
        (0..7).collect::<Vec<_>>()
    );
    assert_eq!(tests[0].file, "game.colibri");
    assert_eq!(tests[0].line, 13);
}

#[test]
fn loading_normally_never_runs_a_test() {
    let out = colibri::SharedBuffer::new();
    let mut vm = vm_with(&out);
    vm.load_source("game.colibri", SOURCE).unwrap();
    vm.run_main().unwrap();
    assert_eq!(out.contents(), "top level\nmain\n");
}

#[test]
fn run_test_reports_success_and_each_kind_of_failure() {
    let out = colibri::SharedBuffer::new();
    let mut vm = vm_with(&out);
    vm.set_instruction_budget(Some(100_000));
    vm.load_source("game.colibri", SOURCE).unwrap();
    out.take();

    assert!(vm.run_test(0).is_ok());
    assert_eq!(out.take(), "in passes\n");

    // Module state is fresh for every test: `hits` is 0 again.
    assert!(vm.run_test(1).is_ok());

    match vm.run_test(2) {
        Err(colibri::Error::Panic {
            message,
            location,
            stack,
            ..
        }) => {
            assert!(message.contains("assertion failed"), "{message}");
            assert!(message.contains("left: 1"), "{message}");
            assert!(message.contains("right: 2"), "{message}");
            assert_eq!(location.unwrap().line, 23);
            assert_eq!(stack[0].function, "test \"fails an assertion\"");
        }
        other => panic!("{other:?}"),
    }
    match vm.run_test(3) {
        Err(colibri::Error::Panic { message, .. }) => assert_eq!(message, "division by zero"),
        other => panic!("{other:?}"),
    }
    match vm.run_test(4) {
        Err(colibri::Error::TestReturnedErr { message }) => {
            assert!(message.contains("cannot parse x"), "{message}");
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        vm.run_test(5),
        Err(colibri::Error::BudgetExhausted { .. })
    ));
    // The Vm is still usable after every kind of failure.
    assert!(vm.run_test(6).is_ok());
    assert!(vm.run_test(0).is_ok());
}

#[test]
fn an_unknown_test_id_is_an_error_and_nothing_loaded_is_not_loaded() {
    let mut vm = colibri::Vm::new();
    assert!(matches!(vm.run_test(0), Err(colibri::Error::NotLoaded)));
    vm.load_source("t.colibri", "test \"a\" { assert(true) }\n")
        .unwrap();
    assert!(matches!(
        vm.run_test(1),
        Err(colibri::Error::FnNotFound { .. })
    ));
}

#[test]
fn the_instruction_budget_is_per_test() {
    let out = colibri::SharedBuffer::new();
    let mut vm = vm_with(&out);
    let source = "test \"a\" {\n    var n = 0\n    for i in 0..1000 {\n        n += i\n    }\n}\n\ntest \"b\" {\n    var n = 0\n    for i in 0..1000 {\n        n += i\n    }\n}\n";
    vm.load_source("t.colibri", source).unwrap();
    // One test fits, so a shared allotment would run out during the second one.
    vm.set_instruction_budget(Some(6_000));
    for round in 0..3 {
        for id in 0..2 {
            vm.run_test(id)
                .unwrap_or_else(|e| panic!("round {round} test {id}: {e}"));
        }
    }
    vm.set_instruction_budget(Some(100));
    assert!(matches!(
        vm.run_test(0),
        Err(colibri::Error::BudgetExhausted { .. })
    ));
}

/// A fresh folder holding `files`.
fn folder(name: &str, files: &[(&str, &str)]) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("colibri-rt-tests-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for (path, text) in files {
        let full = dir.join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, text).unwrap();
    }
    dir
}

#[test]
fn load_tests_runs_nothing_and_finds_modules_nobody_imports() {
    let dir = folder(
        "package",
        &[
            (
                "colibri.toml",
                "[package]\nname = \"demo\"\n\n[dependencies]\nlib = { path = \"lib\" }\n",
            ),
            (
                "main.colibri",
                "import lib\nprint(\"top level\")\nlet shared = [lib.one()]\n\ntest \"in main\" {\n    assert_eq(shared.len(), 1)\n}\n",
            ),
            (
                "sub/extra.colibri",
                "test \"in extra\" {\n    assert(true)\n}\n",
            ),
            ("lib/colibri.toml", "[package]\nname = \"lib\"\n"),
            (
                "lib/lib.colibri",
                "pub fn one() -> int => 1\n\ntest \"in lib\" {\n    assert(true)\n}\n",
            ),
        ],
    );
    let out = colibri::SharedBuffer::new();
    let mut vm = vm_with(&out);
    vm.load_tests(&dir).unwrap();
    assert_eq!(
        out.contents(),
        "",
        "neither initialisers nor top-level code ran"
    );
    let tests = vm.tests();
    let listed: Vec<(String, String, bool)> = tests
        .iter()
        .map(|t| (t.module.clone(), t.name.clone(), t.root))
        .collect();
    assert!(
        listed.contains(&("main".into(), "in main".into(), true)),
        "{listed:?}"
    );
    assert!(
        listed.contains(&("sub.extra".into(), "in extra".into(), true)),
        "{listed:?}"
    );
    assert!(
        listed.contains(&("lib".into(), "in lib".into(), false)),
        "{listed:?}"
    );
    assert_eq!(listed.len(), 3, "{listed:?}");
    for t in tests.iter().filter(|t| t.root) {
        vm.run_test(t.id).unwrap();
    }
    // The initialiser ran for the test that needed it, the top-level print never.
    assert_eq!(out.contents(), "");
}

#[test]
fn load_tests_reports_compile_errors() {
    let dir = folder(
        "broken",
        &[("a.colibri", "test \"t\" {\n    let x: int = \"s\"\n}\n")],
    );
    let mut vm = colibri::Vm::new();
    match vm.load_tests(dir.join("a.colibri")) {
        Err(colibri::Error::Compile(report)) => {
            assert!(report.render(false).contains("expected `int`, found `str`"));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_panicking_initialiser_fails_the_test_that_needs_it() {
    let mut vm = colibri::Vm::new();
    let source = "let zero = 0\nlet boom = 1 / zero\n\ntest \"t\" {\n    assert(true)\n}\n";
    vm.load_source("t.colibri", source)
        .expect_err("the load itself initialises the module and panics");
    let dir = folder("init_panic", &[("t.colibri", source)]);
    let mut vm = colibri::Vm::new();
    vm.load_tests(dir.join("t.colibri")).unwrap();
    match vm.run_test(0) {
        Err(colibri::Error::Panic { message, .. }) => assert_eq!(message, "division by zero"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn drop_hooks_of_module_values_run_after_each_test() {
    let out = colibri::SharedBuffer::new();
    let mut vm = vm_with(&out);
    let source = "class G {\n    name: str\n}\n\nimpl Drop for G {\n    fn drop(self) {\n        print(\"drop {self.name}\")\n    }\n}\n\nlet g = G { name: \"module\" }\n\ntest \"t\" {\n    print(\"body\")\n}\n";
    let dir = folder("drops", &[("t.colibri", source)]);
    vm.load_tests(dir.join("t.colibri")).unwrap();
    vm.run_test(0).unwrap();
    assert_eq!(out.take(), "body\ndrop module\n");
    vm.run_test(0).unwrap();
    assert_eq!(out.take(), "body\ndrop module\n");
}
