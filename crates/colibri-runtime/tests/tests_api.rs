//! The test API: `Vm::load_tests`, `Vm::tests`, `Vm::run_test` (ARCHITECTURE §15).

mod common;
use common::vm_pending;

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
    vm_pending("run_test_reports_success_and_each_kind_of_failure", || {
        let out = colibri::SharedBuffer::new();
        let mut vm = vm_with(&out);
        vm.set_instruction_budget(Some(100_000));
        vm.load_source("game.colibri", SOURCE).unwrap();
        out.take();

        vm.run_test(0).expect("the test passes");
        assert_eq!(out.take(), "in passes\n");

        // Module state is fresh for every test: `hits` is 0 again.
        vm.run_test(1).expect("the test passes");

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
        vm.run_test(6).expect("the test passes");
        vm.run_test(0).expect("the test passes");
    })
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
    vm_pending(
        "load_tests_runs_nothing_and_finds_modules_nobody_imports",
        || {
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
        },
    )
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

#[test]
fn a_tests_script_values_are_initialised_in_source_order_without_the_statements() {
    let out = colibri::SharedBuffer::new();
    let mut vm = vm_with(&out);
    let src = "fn noisy(s: str) -> int {\n    print(s)\n    return 1\n}\nprint(\"statement\")\nlet a = noisy(\"a\")\nlet b = noisy(\"b\") + a\ntest \"sees both\" {\n    assert(b == 2)\n}\n";
    vm.load_source("script.colibri", src).unwrap();
    assert_eq!(out.take(), "statement\na\nb\n");
    assert!(vm.run_test(0).is_ok());
    assert_eq!(out.take(), "a\nb\n");
}

// ---- the per-test host hook (EMBEDDING §9) ----

/// The host's own state: a counter its function bumps. Tests that run after another one see
/// what the earlier one left unless the host resets it.
#[derive(Default)]
struct Calls(i64);

const HOST_STATE_SOURCE: &str = r#"
test "first" {
    assert_eq(call_count(), 1)
}

test "second" {
    assert_eq(call_count(), 1)
}

test "third" {
    assert_eq(call_count(), 1)
}
"#;

fn vm_with_counter() -> colibri::Vm {
    let mut vm = colibri::Vm::builder().build();
    vm.insert_state(Calls::default());
    vm.register_fn("call_count", |cx: &mut colibri::Ctx| {
        let calls = cx.state_mut::<Calls>().unwrap();
        calls.0 += 1;
        calls.0
    })
    .unwrap();
    vm.load_source("state.colibri", HOST_STATE_SOURCE).unwrap();
    vm
}

#[test]
fn host_state_leaks_from_one_test_to_the_next_without_the_hook() {
    vm_pending(
        "host_state_leaks_from_one_test_to_the_next_without_the_hook",
        || {
            let mut vm = vm_with_counter();
            let results: Vec<_> = (0..3).map(|id| vm.run_test(id)).collect();
            results[0].as_ref().expect("the first test passes");
            let results: Vec<bool> = results.iter().map(|r| r.is_ok()).collect();
            assert_eq!(results, [true, false, false]);
            assert_eq!(vm.state_mut::<Calls>().unwrap().0, 3);
        },
    )
}

#[test]
fn on_test_start_lets_the_host_reset_its_state_before_each_test() {
    vm_pending(
        "on_test_start_lets_the_host_reset_its_state_before_each_test",
        || {
            let mut vm = vm_with_counter();
            let seen = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
            let log = seen.clone();
            vm.on_test_start(move |vm, info| {
                log.borrow_mut().push((info.id, info.name.clone()));
                vm.state_mut::<Calls>().unwrap().0 = 0;
            });
            for id in 0..3 {
                vm.run_test(id).unwrap();
            }
            assert_eq!(
                *seen.borrow(),
                [
                    (0, "first".to_string()),
                    (1, "second".to_string()),
                    (2, "third".to_string())
                ]
            );
        },
    )
}

#[test]
fn the_hook_does_not_run_for_an_unknown_test_or_a_plain_load() {
    vm_pending(
        "the_hook_does_not_run_for_an_unknown_test_or_a_plain_load",
        || {
            let mut vm = vm_with_counter();
            let count = std::rc::Rc::new(std::cell::Cell::new(0));
            let c = count.clone();
            vm.on_test_start(move |_, _| c.set(c.get() + 1));
            assert!(vm.run_test(99).is_err());
            assert_eq!(count.get(), 0);
            vm.run_test(0).unwrap();
            assert_eq!(count.get(), 1);
        },
    )
}

#[test]
fn a_hook_set_inside_the_hook_replaces_it() {
    vm_pending("a_hook_set_inside_the_hook_replaces_it", || {
        let mut vm = vm_with_counter();
        let hits = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let first = hits.clone();
        let second = hits.clone();
        vm.on_test_start(move |vm, _| {
            first.borrow_mut().push("first");
            let second = second.clone();
            vm.on_test_start(move |_, _| second.borrow_mut().push("second"));
        });
        vm.run_test(0).unwrap();
        let _ = vm.run_test(1);
        assert_eq!(*hits.borrow(), ["first", "second"]);
    })
}

const HOOK_SOURCE: &str = r#"
var items = [1, 2, 3]
fn peek() -> int { return items.len() }
test "one" { assert_eq(items.len(), 3) }
test "two" { items.push(4); assert_eq(items.len(), 4) }
"#;

fn hook_dir(name: &str, text: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("colibri-hook-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("main.colibri"), text).unwrap();
    dir
}

#[test]
fn a_hook_that_reloads_the_program_runs_the_test_of_the_new_program() {
    let dir = hook_dir(
        "reload",
        "test \"one\" { print(\"old one\") }\ntest \"two\" { print(\"old two\") }\n",
    );
    let path = dir.join("main.colibri");
    let out = colibri::SharedBuffer::new();
    let mut vm = colibri::Vm::builder().stdout(Box::new(out.clone())).build();
    vm.load_file(&path).unwrap();
    let rewrite = path.clone();
    vm.on_test_start(move |vm, _| {
        // More functions than before: the old index of `two` would be out of range in the new
        // program, and the old function id would name another function.
        std::fs::write(
            &rewrite,
            "fn helper() { print(\"helper ran\") }\nfn h2() { panic(\"h2 ran\") }\ntest \"one\" { print(\"new one\") }\ntest \"two\" { print(\"new two\") }\n",
        )
        .unwrap();
        vm.reload_changed().unwrap();
    });
    vm.run_test(1).unwrap();
    assert_eq!(out.contents(), "new two\n");
}

#[test]
fn a_hook_that_reloads_with_a_test_inserted_earlier_runs_the_same_test() {
    let dir = hook_dir(
        "inserted",
        "test \"one\" { print(\"old one\") }\ntest \"two\" { print(\"old two\") }\n",
    );
    let path = dir.join("main.colibri");
    let out = colibri::SharedBuffer::new();
    let mut vm = colibri::Vm::builder().stdout(Box::new(out.clone())).build();
    vm.load_file(&path).unwrap();
    let rewrite = path.clone();
    vm.on_test_start(move |vm, _| {
        // A new first test: by position, id 1 is now `one`.
        std::fs::write(
            &rewrite,
            "test \"zero\" { print(\"new zero\") }\ntest \"one\" { print(\"new one\") }\ntest \"two\" { print(\"new two\") }\n",
        )
        .unwrap();
        vm.reload_changed().unwrap();
    });
    vm.run_test(1).unwrap();
    assert_eq!(out.contents(), "new two\n");
}

#[test]
fn going_by_name_runs_every_test_once_when_the_hook_reloads() {
    let dir = hook_dir(
        "byname",
        "test \"a\" { print(\"a\") }\ntest \"b\" { print(\"b\") }\n",
    );
    let path = dir.join("main.colibri");
    let out = colibri::SharedBuffer::new();
    let mut vm = colibri::Vm::builder().stdout(Box::new(out.clone())).build();
    vm.load_file(&path).unwrap();
    let names: Vec<String> = vm.tests().into_iter().map(|t| t.name).collect();
    let rewrite = path.clone();
    let done = std::cell::Cell::new(false);
    let done = std::rc::Rc::new(done);
    let once = done.clone();
    vm.on_test_start(move |vm, _| {
        if !once.replace(true) {
            // A new first test: the ids taken before are stale from here on.
            std::fs::write(
                &rewrite,
                "test \"z\" { print(\"z\") }\ntest \"a\" { print(\"a\") }\ntest \"b\" { print(\"b\") }\n",
            )
            .unwrap();
            vm.reload_changed().unwrap();
        }
    });
    for name in names {
        let id = vm.tests().iter().find(|t| t.name == name).map(|t| t.id);
        vm.run_test(id.expect("the test is still there")).unwrap();
    }
    assert_eq!(out.contents(), "a\nb\n");
}

#[test]
fn a_hook_that_reloads_to_fewer_tests_makes_run_test_fail_clearly() {
    let dir = hook_dir(
        "fewer",
        "test \"one\" { print(\"old one\") }\ntest \"two\" { print(\"old two\") }\n",
    );
    let path = dir.join("main.colibri");
    let mut vm = colibri::Vm::builder().build();
    vm.load_file(&path).unwrap();
    let rewrite = path.clone();
    vm.on_test_start(move |vm, _| {
        std::fs::write(&rewrite, "test \"only\" { print(\"only\") }\n").unwrap();
        vm.reload_changed().unwrap();
    });
    let err = vm.run_test(1).unwrap_err();
    assert!(
        matches!(&err, colibri::Error::FnNotFound { name } if name.contains("test #1") && name.contains("reloaded")),
        "{err:?}"
    );
}

#[test]
fn a_script_function_called_from_the_hook_that_reads_a_module_value_panics_clearly() {
    vm_pending(
        "a_script_function_called_from_the_hook_that_reads_a_module_value_panics_clearly",
        || {
            let mut vm = colibri::Vm::builder().build();
            vm.load_source("s.colibri", HOOK_SOURCE).unwrap();
            let seen = std::rc::Rc::new(std::cell::RefCell::new(None));
            let s = seen.clone();
            vm.on_test_start(move |vm, _| {
                let peek = vm.get_fn::<(), i64>("peek").unwrap();
                *s.borrow_mut() = Some(peek.call(vm, ()));
            });
            vm.run_test(0).unwrap();
            let result = seen.borrow_mut().take().unwrap();
            match result {
                Err(colibri::Error::Panic { message, .. }) => {
                    assert!(
                        message.contains("`items` is used before it is initialised"),
                        "{message}"
                    );
                    assert!(message.contains("on_test_start"), "{message}");
                }
                other => panic!("expected a panic, got {other:?}"),
            }
            // The test itself still sees its module values.
            vm.run_test(1).unwrap();
        },
    )
}

#[test]
fn a_panicking_hook_stays_registered() {
    vm_pending("a_panicking_hook_stays_registered", || {
        let mut vm = colibri::Vm::builder().build();
        vm.load_source("s.colibri", HOOK_SOURCE).unwrap();
        let calls = std::rc::Rc::new(std::cell::Cell::new(0));
        let c = calls.clone();
        vm.on_test_start(move |_, _| {
            c.set(c.get() + 1);
            if c.get() == 1 {
                panic!("boom");
            }
        });
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| vm.run_test(0)));
        assert!(caught.is_err());
        vm.run_test(1).unwrap();
        assert_eq!(calls.get(), 2);
    })
}

#[test]
fn a_test_run_from_inside_the_hook_runs_without_the_hook() {
    vm_pending(
        "a_test_run_from_inside_the_hook_runs_without_the_hook",
        || {
            let mut vm = colibri::Vm::builder().build();
            vm.load_source("s.colibri", HOOK_SOURCE).unwrap();
            let seen = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
            let s = seen.clone();
            vm.on_test_start(move |vm, info| {
                s.borrow_mut().push(info.id);
                if info.id == 1 {
                    vm.run_test(0).unwrap();
                }
            });
            vm.run_test(1).unwrap();
            assert_eq!(*seen.borrow(), [1]);
            // The hook is back for the next call.
            vm.run_test(0).unwrap();
            assert_eq!(*seen.borrow(), [1, 0]);
        },
    )
}
