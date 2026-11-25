//! The bytecode VM as the runtime's engine: programs, panics with their stacks, a refusal that
//! stops cleanly, and REPL sessions.

use colibri_runtime as colibri;
use colibri_runtime::{Error, Vm};

fn message(error: &Error) -> String {
    error.render(false)
}

fn run(source: &str) -> (Result<(), Error>, String) {
    let out = colibri::SharedBuffer::new();
    let mut vm = Vm::builder().stdout(Box::new(out.clone())).build();
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
    let (result, out) = run(src);
    result.expect("the program runs on the VM");
    assert_eq!(out, "610\n");
}

#[test]
fn the_bytecode_engine_reports_a_panic_with_its_stack() {
    let (result, out) = run(
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
fn vm_with_nap(out: &colibri::SharedBuffer) -> Vm {
    let mut vm = Vm::builder().stdout(Box::new(out.clone())).build();
    vm.register_fn("nap", |cx: &mut colibri::Ctx| -> () { cx.raise(MISSING) })
        .unwrap();
    vm
}

#[test]
fn what_the_bytecode_engine_lacks_is_a_clean_panic_naming_it() {
    let out = colibri::SharedBuffer::new();
    let mut vm = vm_with_nap(&out);
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
    let mut vm = Vm::builder().stdout(Box::new(out.clone())).build();
    vm.session_start().expect("a session starts on the VM");
    let check = vm.session_check("let x = 20 + 1\nprint(x)\n").unwrap();
    vm.session_commit(check, &[], &[]).unwrap();
    assert_eq!(out.contents(), "21\n");
    let (text, ty) = vm.session_display("x").expect("x is a module value");
    assert_eq!((text.as_str(), ty.as_str()), ("21", "int"));
}

#[test]
fn a_repl_session_shows_an_optional_object() {
    let show = || {
        let mut vm = Vm::builder().build();
        vm.session_start().expect("a session starts");
        let src = "class N { v: int }\nlet n = N { v: 7 }\nlet s: N? = some(n)\n";
        let check = vm.session_check(src).unwrap();
        vm.session_commit(check, &[], &[]).unwrap();
        vm.session_display("s").expect("s is a module value")
    };
    let got = show();
    assert_eq!(got, ("some(N { v: 7 })".to_string(), "N?".to_string()));
}

/// What a commit without a running session says, on an engine.
fn commit_without_a_session(load: bool) -> String {
    let mut vm = Vm::builder().build();
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
fn a_session_commit_without_a_session_is_a_conflict() {
    let got = commit_without_a_session(false);
    assert_eq!(got, "no session is running");
}

/// What a commit after an ordinary load does and prints.
fn commit_after_a_load() -> (String, String) {
    let out = colibri::SharedBuffer::new();
    let mut vm = Vm::builder().stdout(Box::new(out.clone())).build();
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

/// A program loaded the ordinary way is extended by a commit.
#[test]
fn a_commit_after_an_ordinary_load_works() {
    let got = commit_after_a_load();
    assert_eq!(got, ("ok".to_string(), "ran 1\n".to_string()));
}

#[test]
fn a_session_display_after_an_ordinary_load_works() {
    let mut vm = Vm::builder().build();
    vm.load_source("a.colibri", "let x = 1\n").unwrap();
    assert_eq!(
        (vm.session_display("x"), vm.session_display("nobody")),
        (Some(("1".to_string(), "int".to_string())), None)
    );
}

#[test]
fn a_small_program_runs() {
    let src = "fn main() {\n    let mut s = 0\n    for i in 0..10 { s += i * i }\n    print(s)\n}";
    let (result, out) = run(src);
    result.unwrap();
    assert_eq!(out, "285\n");
}

#[test]
fn a_refusal_does_not_silence_the_module_values_of_later_tests() {
    // One Vm, three tests: the middle one meets a refusal beside a coroutine. The refusal
    // leaks what its own run holds, but the test after it must still see the module value's
    // `Drop` hook run.
    let src = "class G {\n    name: str\n}\n\nimpl Drop for G {\n    fn drop(self) {\n        print(\"drop {self.name}\")\n    }\n}\n\nlet g = G { name: \"module\" }\n\nfn sleeper() {\n    wait(100.0)\n}\n\ntest \"plain\" {\n    print(\"plain body\")\n}\n\ntest \"refused\" {\n    let c = go sleeper()\n    nap()\n    print(1)\n}\n\ntest \"plain again\" {\n    print(\"again body\")\n}\n";
    let run = || {
        let out = colibri::SharedBuffer::new();
        let mut vm = vm_with_nap(&out);
        vm.load_source("t.colibri", src).unwrap();
        let mut seen = Vec::new();
        for i in 0..3 {
            let result = vm.run_test(i);
            seen.push((result.is_ok(), out.take()));
        }
        seen
    };
    let got = run();
    // The refusal fails the test and prints nothing.
    assert_eq!(
        got,
        [
            (true, "drop module\nplain body\ndrop module\n".to_string()),
            (false, String::new()),
            (true, "again body\ndrop module\n".to_string()),
        ]
    );
}

/// A session entry that panics after creating a module-level value: the value is released when
/// the entry is rolled back (its `Drop` hook runs then), and the name is free again.
#[test]
fn a_failed_session_entry_releases_the_values_it_made() {
    let run = || {
        let out = colibri::SharedBuffer::new();
        let mut vm = Vm::builder().stdout(Box::new(out.clone())).build();
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
    let got = run();
    assert_eq!(got, (true, "drop made\n".to_string(), true));
}

/// Runs `entries` in a session, each a whole program; `Err` text for one that fails.
fn session(entries: &[&str], between: impl Fn(&mut Vm, usize)) -> (Vec<String>, String, usize) {
    let out = colibri::SharedBuffer::new();
    let mut vm = Vm::builder()
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

/// What a session entry that panics with `boom` at its line `line` reports.
fn boom_at(line: u32) -> String {
    format!(
        "Failed(Panic {{ message: \"boom\", location: Some(Location {{ file: \"<repl>\", line: {line}, column: 1 }}), stack: [StackFrame {{ function: \"<script>\", location: Location {{ file: \"<repl>\", line: {line}, column: 1 }} }}], notes: [] }})"
    )
}

/// What a call of a closure whose code is gone reports, at `line`, `column` of the entry.
fn code_is_gone_at(line: u32, column: u32) -> String {
    format!(
        "Failed(Panic {{ message: \"this closure was made by a REPL entry that failed and was rolled back, or in a function that a REPL entry replaced; its code is gone\", location: Some(Location {{ file: \"<repl>\", line: {line}, column: {column} }}), stack: [StackFrame {{ function: \"<script>\", location: Location {{ file: \"<repl>\", line: {line}, column: {column} }} }}], notes: [] }})"
    )
}

/// A coroutine that a failed entry started runs code that goes away with the entry: it ends at
/// the rollback (its `defer`s and drops run), and the next tick has nothing to resume. It used to
/// stay and fail with `internal error: unknown function`.
#[test]
fn a_failed_entry_ends_the_coroutines_it_started() {
    let entry = "let a = 1\nclass Box2 {\n    n: int\n}\nimpl Drop for Box2 {\n    fn drop(self) {\n        print(\"drop {self.n}\")\n    }\n}\nfn helper(n: int) -> int => n * 2\nfn worker() {\n    let b = Box2 { n: 3 }\n    wait(0.1)\n    print(\"helper {helper(b.n)}\")\n}\ngo worker()\npanic(\"boom\")\n";
    let ticks = std::cell::RefCell::new(Vec::new());
    let run = || {
        ticks.borrow_mut().clear();
        let r = session(
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
    let got = run();
    assert_eq!(
        got,
        (
            (
                vec!["ok".to_string(), boom_at(17), "ok".to_string()],
                "drop 3\nnext 1\n".to_string(),
                0
            ),
            vec!["Ok(())".to_string()]
        )
    );
}

/// A closure that a failed entry stored in an older value, made by a function that survives,
/// still finds its body when a later entry calls it (the entry's new function shifted the ids of
/// the lifted closure bodies). It used to run another body, or none (`unknown closure body`).
#[test]
fn a_closure_a_failed_entry_stored_from_surviving_code_still_runs() {
    let first = "let mut keep: fn() -> int = fn() => 0\nfn mk() -> fn() -> int {\n    return fn() => 5\n}\nfn other() -> fn() -> int {\n    return fn() => 9\n}\n";
    let second = format!("{first}fn extra() -> int => 1\nkeep = mk()\npanic(\"boom\")\n");
    let third = format!("{first}print(keep())\nprint(other()())\n");
    let run = || session(&[first, &second, &third], |_, _| {});
    let got = run();
    assert_eq!(
        got,
        (
            vec!["ok".to_string(), boom_at(10), "ok".to_string()],
            "5\n9\n".to_string(),
            0
        )
    );
}

/// A closure that only the failed entry's code made has no body to run: calling it later is a
/// panic and no other closure answers in its place.
#[test]
fn a_closure_from_a_failed_entrys_own_code_is_refused_not_mistaken() {
    let first = "let mut keep: fn() -> int = fn() => 0\nfn other() -> fn() -> int {\n    return fn() => 9\n}\n";
    let second =
        format!("{first}fn helper() -> int => 7\nkeep = fn() => helper()\npanic(\"boom\")\n");
    let third = format!("{first}print(keep())\n");
    let run = || session(&[first, &second, &third], |_, _| {});
    let got = run();
    // Nothing is printed, and the call is refused, not mistaken for another closure's.
    assert_eq!(
        got,
        (
            vec!["ok".to_string(), boom_at(7), code_is_gone_at(5, 7)],
            String::new(),
            0
        )
    );
}

/// The shift of the lifted ids a failed entry makes (it defines a function before the closures'
/// makers), with the list built-ins running the closure it stored: the closure finds its own body
/// there once the entry is rolled back, not a neighbour's.
#[test]
fn a_leaf_closure_a_failed_entry_stored_still_runs_its_own_body_in_the_list_built_ins() {
    let first = "let mut keep: fn(int) -> int = fn(n) => 0\nfn mk() -> fn(int) -> int {\n    return fn(n) => n * 5\n}\nfn other() -> fn(int) -> int {\n    return fn(n) => n + 9\n}\n";
    let second = format!("{first}fn extra() -> int => 1\nkeep = mk()\npanic(\"boom\")\n");
    let third = format!("{first}print([1, 2, 3].map(keep))\nprint([1, 2, 3].map(other()))\n");
    let run = || session(&[first, &second, &third], |_, _| {});
    let got = run();
    assert_eq!(
        got,
        (
            vec!["ok".to_string(), boom_at(10), "ok".to_string()],
            "[5, 10, 15]\n[10, 11, 12]\n".to_string(),
            0
        )
    );
}

/// The list built-ins run a closure that only computes in a native loop; one whose code is gone
/// fails there as a direct call of it does, with the same panic.
#[test]
fn a_leaf_closure_whose_code_is_gone_fails_in_the_list_built_ins_as_a_call_does() {
    let first = "let mut keep: fn(int) -> int = fn(n) => n\nfn other() -> fn() -> int {\n    return fn() => 9\n}\n";
    let second = format!(
        "{first}fn helper() -> int => 7\nkeep = fn(n) => n + helper() - 7\npanic(\"boom\")\n"
    );
    let direct = format!("{first}print(keep(5))\n");
    let mapped = format!("{first}print([1, 2, 3].map(keep).len())\n");
    let run = |third: &str| session(&[first, &second, third], |_, _| {});
    let d = run(&direct);
    assert_eq!(
        d,
        (
            vec!["ok".to_string(), boom_at(7), code_is_gone_at(5, 7)],
            String::new(),
            0
        )
    );
    let m = run(&mapped);
    assert_eq!(m, d, "mapped, then direct");
}

/// A closure whose lifted function the failed entry has no counterpart for (it redefined `mk`
/// with one closure where the old one had two) still runs after the rollback: the old program is
/// back, and so is its body.
#[test]
fn a_closure_whose_function_the_failed_entry_redefined_still_runs() {
    let first = "let mut keep: fn() -> int = fn() => 0\nfn mk(b: bool) -> fn() -> int {\n    if b {\n        return fn() => 5\n    }\n    return fn() => 8\n}\nkeep = mk(false)\n";
    let old_program = "let mut keep: fn() -> int = fn() => 0\nfn mk(b: bool) -> fn() -> int {\n    if b {\n        return fn() => 5\n    }\n    return fn() => 8\n}\n";
    let second = "let mut keep: fn() -> int = fn() => 0\nfn mk(b: bool) -> fn() -> int {\n    return fn() => 1\n}\npanic(\"boom\")\n";
    let third = format!("{old_program}print(keep())\n");
    let run = || session(&[first, second, &third], |_, _| {});
    let got = run();
    assert_eq!(
        got,
        (
            vec!["ok".to_string(), boom_at(5), "ok".to_string()],
            "8\n".to_string(),
            0
        )
    );
}

/// A `defer` that starts a coroutine while the rollback cancels another one: the new one runs
/// code of the failed entry too, so it is cancelled as well.
#[test]
fn a_coroutine_started_by_a_defer_during_the_rollback_is_cancelled_too() {
    let entry = "let a = 1\nfn helper() -> int => 4\nfn late() {\n    wait(0.1)\n    print(\"late {helper()}\")\n}\nfn start() {\n    go late()\n}\nfn spawner() {\n    defer start()\n    wait(1.0)\n}\ngo spawner()\nwait(0.05)\npanic(\"boom\")\n";
    let ticks = std::cell::RefCell::new(Vec::new());
    let run = || {
        ticks.borrow_mut().clear();
        let r = session(&["let a = 1\n", entry], |vm, i| {
            if i == 1 {
                ticks
                    .borrow_mut()
                    .push(format!("{:?}", vm.tick(0.5).map_err(|e| e.to_string())));
            }
        });
        (r, ticks.borrow().clone())
    };
    let got = run();

    assert_eq!(
        got,
        (
            (vec!["ok".to_string(), boom_at(16)], String::new(), 0),
            vec!["Ok(())".to_string()]
        )
    );
}
