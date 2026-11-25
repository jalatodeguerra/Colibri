//! Choosing the engine: the tree-walker is the default; the bytecode VM runs the programs it
//! has the features for and refuses the rest cleanly.

use colibri_runtime as colibri;
use colibri_runtime::{EngineKind, Error, Vm};

fn message(error: &Error) -> String {
    error.render(false)
}

fn run_on(kind: EngineKind, source: &str) -> (Result<(), Error>, String) {
    let out = colibri::SharedBuffer::new();
    let mut vm = Vm::builder()
        .engine(kind)
        .stdout(Box::new(out.clone()))
        .build();
    let result = vm
        .load_source("a.colibri", source)
        .and_then(|()| vm.run_main().map(|_| ()));
    (result, out.contents())
}

#[test]
fn the_bytecode_engine_runs_a_program() {
    let src = "fn fib(n: int) -> int {
    if n < 2 { return n }
    return fib(n - 1) + fib(n - 2)
}
fn main() { print(fib(15)) }";
    let (result, out) = run_on(EngineKind::Bytecode, src);
    result.expect("the program runs on the VM");
    assert_eq!(out, "610\n");
}

#[test]
fn the_bytecode_engine_reports_a_panic_with_its_stack() {
    let (result, out) = run_on(
        EngineKind::Bytecode,
        "fn boom(n: int) -> int => 10 / n\nfn main() {\n    print(\"before\")\n    print(boom(0))\n}",
    );
    assert_eq!(out, "before\n");
    let text = message(&result.expect_err("a division by zero"));
    assert!(text.contains("division by zero"), "{text}");
    assert!(text.contains("boom"), "{text}");
}

/// The text of a refusal, which every program a checker accepts runs without now (the VM has all of
/// the language). The host function below stands in for a feature the VM lacks by failing with it.
const MISSING: &str = "the bytecode VM does not support a feature that is not there yet";

/// A `Vm` with the host function `nap()`, which fails with the text of a refusal.
fn vm_with_nap(kind: EngineKind, out: &colibri::SharedBuffer) -> Vm {
    let mut vm = Vm::builder()
        .engine(kind)
        .stdout(Box::new(out.clone()))
        .build();
    vm.register_fn("nap", |cx: &mut colibri::Ctx| -> () { cx.raise(MISSING) })
        .unwrap();
    vm
}

#[test]
fn what_the_bytecode_engine_lacks_is_a_clean_panic_naming_it() {
    let out = colibri::SharedBuffer::new();
    let mut vm = vm_with_nap(EngineKind::Bytecode, &out);
    vm.load_source(
        "a.colibri",
        "fn sleeper() {\n    wait(100.0)\n}\nfn main() {\n    let c = go sleeper()\n    nap()\n    print(1)\n}",
    )
    .unwrap();
    let result = vm.run_main();
    let text = message(&result.expect_err("the stand-in feature is refused"));
    assert!(text.contains(MISSING), "{text}");
    assert_eq!(out.contents(), "");
}

#[test]
fn a_repl_session_runs_on_the_bytecode_engine() {
    let out = colibri::SharedBuffer::new();
    let mut vm = Vm::builder()
        .engine(EngineKind::Bytecode)
        .stdout(Box::new(out.clone()))
        .build();
    vm.session_start().expect("a session starts on the VM");
    let check = vm.session_check("let x = 20 + 1\nprint(x)\n").unwrap();
    vm.session_commit(check, &[], &[]).unwrap();
    assert_eq!(out.contents(), "21\n");
    let (text, ty) = vm.session_display("x").expect("x is a module value");
    assert_eq!((text.as_str(), ty.as_str()), ("21", "int"));
}

#[test]
fn a_repl_session_shows_an_optional_object_the_same_on_both_engines() {
    let show = |kind: EngineKind| {
        let mut vm = Vm::builder().engine(kind).build();
        vm.session_start().expect("a session starts");
        let src = "class N { v: int }\nlet n = N { v: 7 }\nlet s: N? = some(n)\n";
        let check = vm.session_check(src).unwrap();
        vm.session_commit(check, &[], &[]).unwrap();
        vm.session_display("s").expect("s is a module value")
    };
    let tree = show(EngineKind::Tree);
    assert_eq!(tree.0, "some(N { v: 7 })");
    assert_eq!(show(EngineKind::Bytecode), tree);
}

/// What a commit without a running session says, on an engine.
fn commit_without_a_session(kind: EngineKind, load: bool) -> String {
    let mut vm = Vm::builder().engine(kind).build();
    if load {
        vm.load_source("a.colibri", "let x = 1\n").unwrap();
    }
    let check = vm.session_check("let x = 1\n").expect("checks");
    match vm.session_commit(check, &[], &[]) {
        Err(colibri::SessionError::Conflict(text)) => text,
        Err(other) => panic!("expected a conflict, got {other:?}"),
        Ok(()) => panic!("committed without a session"),
    }
}

#[test]
fn a_session_commit_without_a_session_is_the_same_error_on_both_engines() {
    let tree = commit_without_a_session(EngineKind::Tree, false);
    assert_eq!(commit_without_a_session(EngineKind::Bytecode, false), tree);
}

/// What a commit after an ordinary load does and prints.
fn commit_after_a_load(kind: EngineKind) -> (String, String) {
    let out = colibri::SharedBuffer::new();
    let mut vm = Vm::builder()
        .engine(kind)
        .stdout(Box::new(out.clone()))
        .build();
    vm.load_source("a.colibri", "let x = 1\n").unwrap();
    let check = vm
        .session_check("let x = 1\nprint(\"ran {x}\")\n")
        .expect("checks");
    let result = match vm.session_commit(check, &[], &[]) {
        Ok(()) => "ok".to_string(),
        Err(e) => format!("{e:?}"),
    };
    (result, out.contents())
}

/// A program loaded the ordinary way is extended by a commit, as on the tree-walker.
#[test]
fn a_commit_after_an_ordinary_load_is_the_same_on_both_engines() {
    let tree = commit_after_a_load(EngineKind::Tree);
    assert_eq!(tree, ("ok".to_string(), "ran 1\n".to_string()));
    assert_eq!(commit_after_a_load(EngineKind::Bytecode), tree);
}

#[test]
fn a_session_display_after_an_ordinary_load_is_the_same_on_both_engines() {
    let display = |kind: EngineKind| {
        let mut vm = Vm::builder().engine(kind).build();
        vm.load_source("a.colibri", "let x = 1\n").unwrap();
        (vm.session_display("x"), vm.session_display("nobody"))
    };
    assert_eq!(display(EngineKind::Bytecode), display(EngineKind::Tree));
}

#[test]
fn the_default_engine_is_the_vm() {
    assert_eq!(EngineKind::default(), EngineKind::Bytecode);
}

#[test]
fn the_tree_engine_runs_programs() {
    let (result, out) = run_on(EngineKind::Tree, "fn main() { print(\"tree\") }");
    result.unwrap();
    assert_eq!(out, "tree\n");
}

#[test]
fn both_engines_agree_on_a_small_program() {
    let src = "fn main() {\n    let mut s = 0\n    for i in 0..10 { s += i * i }\n    print(s)\n}";
    let tree = run_on(EngineKind::Tree, src);
    let vm = run_on(EngineKind::Bytecode, src);
    assert!(tree.0.is_ok() && vm.0.is_ok());
    assert_eq!(tree.1, vm.1);
}

#[test]
fn a_refusal_does_not_silence_the_module_values_of_later_tests() {
    // One Vm, three tests: the middle one meets a refusal beside a coroutine. The refusal
    // leaks what its own run holds, but the test after it must still see the module value's
    // `Drop` hook run, as on the tree-walker.
    let src = "class G {\n    name: str\n}\n\nimpl Drop for G {\n    fn drop(self) {\n        print(\"drop {self.name}\")\n    }\n}\n\nlet g = G { name: \"module\" }\n\nfn sleeper() {\n    wait(100.0)\n}\n\ntest \"plain\" {\n    print(\"plain body\")\n}\n\ntest \"refused\" {\n    let c = go sleeper()\n    nap()\n    print(1)\n}\n\ntest \"plain again\" {\n    print(\"again body\")\n}\n";
    let run = |kind: EngineKind| {
        let out = colibri::SharedBuffer::new();
        let mut vm = vm_with_nap(kind, &out);
        vm.load_source("t.colibri", src).unwrap();
        let mut seen = Vec::new();
        for i in 0..3 {
            let result = vm.run_test(i);
            seen.push((result.is_ok(), out.take()));
        }
        seen
    };
    let tree = run(EngineKind::Tree);
    assert_eq!(
        tree.last().unwrap(),
        &(true, "again body\ndrop module\n".to_string()),
        "{tree:?}"
    );
    let vm = run(EngineKind::Bytecode);
    assert_eq!(vm[0], tree[0], "{vm:?}");
    assert!(!vm[1].0, "the refusal fails the test: {vm:?}");
    assert_eq!(vm[2], tree[2], "{vm:?}");
}

/// A session entry that panics after creating a module-level value: the value is released when
/// the entry is rolled back (its `Drop` hook runs then), and the name is free again.
#[test]
fn a_failed_session_entry_releases_the_values_it_made_on_both_engines() {
    let run = |kind: EngineKind| {
        let out = colibri::SharedBuffer::new();
        let mut vm = Vm::builder()
            .engine(kind)
            .stdout(Box::new(out.clone()))
            .build();
        vm.session_start().unwrap();
        let decl = "class G {\n    name: str\n}\nimpl Drop for G {\n    fn drop(self) {\n        print(\"drop {self.name}\")\n    }\n}\n";
        let check = vm.session_check(decl).unwrap();
        vm.session_commit(check, &[], &[]).unwrap();
        let failing = format!("{decl}let g = G {{ name: \"made\" }}\nlet boom = 1 / (2 - 2)\n");
        let check = vm.session_check(&failing).unwrap();
        let failed = vm.session_commit(check, &[], &[]).is_err();
        let after = out.contents();
        let shown = vm.session_display("g").is_none();
        (failed, after, shown)
    };
    let tree = run(EngineKind::Tree);
    assert_eq!(tree, (true, "drop made\n".to_string(), true));
    assert_eq!(run(EngineKind::Bytecode), tree);
}

/// Runs `entries` in a session, each a whole program; `Err` text for one that fails.
fn session(
    kind: EngineKind,
    entries: &[&str],
    between: impl Fn(&mut Vm, usize),
) -> (Vec<String>, String, usize) {
    let out = colibri::SharedBuffer::new();
    let mut vm = Vm::builder()
        .engine(kind)
        .stdout(Box::new(out.clone()))
        .track_leaks(true)
        .build();
    vm.session_start().unwrap();
    let mut results = Vec::new();
    for (i, entry) in entries.iter().enumerate() {
        let check = vm.session_check(entry).unwrap();
        results.push(match vm.session_commit(check, &[], &[]) {
            Ok(()) => "ok".to_string(),
            Err(e) => format!("{e:?}").lines().next().unwrap_or("").to_string(),
        });
        between(&mut vm, i);
    }
    (results, out.contents(), vm.find_leaks().len())
}

/// A coroutine that a failed entry started runs code that goes away with the entry: it ends at
/// the rollback (its `defer`s and drops run), and the next tick has nothing to resume. It used to
/// stay and fail with `internal error: unknown function`.
#[test]
fn a_failed_entry_ends_the_coroutines_it_started_on_both_engines() {
    let entry = "let a = 1\nclass Box2 {\n    n: int\n}\nimpl Drop for Box2 {\n    fn drop(self) {\n        print(\"drop {self.n}\")\n    }\n}\nfn helper(n: int) -> int => n * 2\nfn worker() {\n    let b = Box2 { n: 3 }\n    wait(0.1)\n    print(\"helper {helper(b.n)}\")\n}\ngo worker()\npanic(\"boom\")\n";
    let ticks = std::cell::RefCell::new(Vec::new());
    let run = |kind: EngineKind| {
        ticks.borrow_mut().clear();
        let r = session(
            kind,
            &["let a = 1\n", entry, "let a = 1\nprint(\"next {a}\")\n"],
            |vm, i| {
                if i == 1 {
                    ticks
                        .borrow_mut()
                        .push(format!("{:?}", vm.tick(0.5).map_err(|e| e.to_string())));
                }
            },
        );
        (r, ticks.borrow().clone())
    };
    let tree = run(EngineKind::Tree);
    let ((results, out, leaks), tick) = &tree;
    assert!(results[1].contains("boom"), "{results:?}");
    assert_eq!(tick, &["Ok(())"]);
    assert_eq!(out, "drop 3\nnext 1\n");
    assert_eq!(*leaks, 0);
    assert_eq!(run(EngineKind::Bytecode), tree);
}

/// A closure that a failed entry stored in an older value, made by a function that survives,
/// still finds its body when a later entry calls it (the entry's new function shifted the ids of
/// the lifted closure bodies). It used to run another body, or none (`unknown closure body`).
#[test]
fn a_closure_a_failed_entry_stored_from_surviving_code_still_runs_on_both_engines() {
    let first = "let mut keep: fn() -> int = fn() => 0\nfn mk() -> fn() -> int {\n    return fn() => 5\n}\nfn other() -> fn() -> int {\n    return fn() => 9\n}\n";
    let second = format!("{first}fn extra() -> int => 1\nkeep = mk()\npanic(\"boom\")\n");
    let third = format!("{first}print(keep())\nprint(other()())\n");
    let run = |kind: EngineKind| session(kind, &[first, &second, &third], |_, _| {});
    let tree = run(EngineKind::Tree);
    assert_eq!(tree.1, "5\n9\n", "{tree:?}");
    assert_eq!(run(EngineKind::Bytecode), tree);
}

/// A closure that only the failed entry's code made has no body to run: calling it later is a
/// panic on both engines, and no other closure answers in its place.
#[test]
fn a_closure_from_a_failed_entrys_own_code_is_refused_not_mistaken_on_both_engines() {
    let first = "let mut keep: fn() -> int = fn() => 0\nfn other() -> fn() -> int {\n    return fn() => 9\n}\n";
    let second =
        format!("{first}fn helper() -> int => 7\nkeep = fn() => helper()\npanic(\"boom\")\n");
    let third = format!("{first}print(keep())\n");
    let run = |kind: EngineKind| session(kind, &[first, &second, &third], |_, _| {});
    let tree = run(EngineKind::Tree);
    assert_eq!(tree.1, "", "nothing printed: {tree:?}");
    assert!(
        tree.0[2].contains("this closure was made by a REPL entry that failed"),
        "{tree:?}"
    );
    assert!(!tree.0[2].contains("internal error"), "{tree:?}");
    assert_eq!(run(EngineKind::Bytecode), tree);
}

/// The shift of the lifted ids a failed entry makes (it defines a function before the closures'
/// makers), with the list built-ins running the closure it stored: the closure finds its own body
/// there once the entry is rolled back, not a neighbour's.
#[test]
fn a_leaf_closure_a_failed_entry_stored_still_runs_its_own_body_in_the_list_built_ins_on_both_engines()
 {
    let first = "let mut keep: fn(int) -> int = fn(n) => 0\nfn mk() -> fn(int) -> int {\n    return fn(n) => n * 5\n}\nfn other() -> fn(int) -> int {\n    return fn(n) => n + 9\n}\n";
    let second = format!("{first}fn extra() -> int => 1\nkeep = mk()\npanic(\"boom\")\n");
    let third = format!("{first}print([1, 2, 3].map(keep))\nprint([1, 2, 3].map(other()))\n");
    let run = |kind: EngineKind| session(kind, &[first, &second, &third], |_, _| {});
    let tree = run(EngineKind::Tree);
    assert_eq!(tree.1, "[5, 10, 15]\n[10, 11, 12]\n", "{tree:?}");
    assert_eq!(run(EngineKind::Bytecode), tree);
}

/// The list built-ins run a closure that only computes in a native loop; one whose code is gone
/// fails there as a direct call of it does, with the same panic, on both engines.
#[test]
fn a_leaf_closure_whose_code_is_gone_fails_in_the_list_built_ins_as_a_call_does_on_both_engines() {
    let first = "let mut keep: fn(int) -> int = fn(n) => n\nfn other() -> fn() -> int {\n    return fn() => 9\n}\n";
    let second = format!(
        "{first}fn helper() -> int => 7\nkeep = fn(n) => n + helper() - 7\npanic(\"boom\")\n"
    );
    let direct = format!("{first}print(keep(5))\n");
    let mapped = format!("{first}print([1, 2, 3].map(keep).len())\n");
    let run = |kind: EngineKind, third: &str| session(kind, &[first, &second, third], |_, _| {});
    for kind in [EngineKind::Tree, EngineKind::Bytecode] {
        let d = run(kind, &direct);
        assert!(d.0[2].contains("its code is gone"), "{d:?}");
        let m = run(kind, &mapped);
        assert_eq!(m, d, "mapped, then direct");
    }
    assert_eq!(
        run(EngineKind::Bytecode, &mapped),
        run(EngineKind::Tree, &mapped)
    );
}

/// A closure whose lifted function the failed entry has no counterpart for (it redefined `mk`
/// with one closure where the old one had two) still runs after the rollback: the old program is
/// back, and so is its body.
#[test]
fn a_closure_whose_function_the_failed_entry_redefined_still_runs_on_both_engines() {
    let first = "let mut keep: fn() -> int = fn() => 0\nfn mk(b: bool) -> fn() -> int {\n    if b {\n        return fn() => 5\n    }\n    return fn() => 8\n}\nkeep = mk(false)\n";
    let old_program = "let mut keep: fn() -> int = fn() => 0\nfn mk(b: bool) -> fn() -> int {\n    if b {\n        return fn() => 5\n    }\n    return fn() => 8\n}\n";
    let second = "let mut keep: fn() -> int = fn() => 0\nfn mk(b: bool) -> fn() -> int {\n    return fn() => 1\n}\npanic(\"boom\")\n";
    let third = format!("{old_program}print(keep())\n");
    let run = |kind: EngineKind| session(kind, &[first, second, &third], |_, _| {});
    let tree = run(EngineKind::Tree);
    assert_eq!(tree.1, "8\n", "{tree:?}");
    assert_eq!(tree.0[2], "ok");
    assert_eq!(run(EngineKind::Bytecode), tree);
}

/// A `defer` that starts a coroutine while the rollback cancels another one: the new one runs
/// code of the failed entry too, so it is cancelled as well.
#[test]
fn a_coroutine_started_by_a_defer_during_the_rollback_is_cancelled_too_on_both_engines() {
    let entry = "let a = 1\nfn helper() -> int => 4\nfn late() {\n    wait(0.1)\n    print(\"late {helper()}\")\n}\nfn start() {\n    go late()\n}\nfn spawner() {\n    defer start()\n    wait(1.0)\n}\ngo spawner()\nwait(0.05)\npanic(\"boom\")\n";
    let ticks = std::cell::RefCell::new(Vec::new());
    let run = |kind: EngineKind| {
        ticks.borrow_mut().clear();
        let r = session(kind, &["let a = 1\n", entry], |vm, i| {
            if i == 1 {
                ticks
                    .borrow_mut()
                    .push(format!("{:?}", vm.tick(0.5).map_err(|e| e.to_string())));
            }
        });
        (r, ticks.borrow().clone())
    };
    let tree = run(EngineKind::Tree);

    assert_eq!(tree.1, ["Ok(())"], "{tree:?}");
    assert_eq!((tree.0).1, "", "{tree:?}");
    assert_eq!(run(EngineKind::Bytecode), tree);
}
