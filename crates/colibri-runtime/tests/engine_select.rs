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

/// A `Vm` with the host function `nap()`, which waits for a background operation: on the VM, a
/// wait while a coroutine is alive is host async (vm-plan step 12), which it does not have yet.
fn vm_with_nap(kind: EngineKind, out: &colibri::SharedBuffer) -> Vm {
    let mut vm = Vm::builder()
        .engine(kind)
        .stdout(Box::new(out.clone()))
        .build();
    vm.register_fn("nap", |cx: &mut colibri::Ctx| cx.run_async(|| ()))
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
    let text = message(&result.expect_err("a host wait beside a coroutine is not on the VM yet"));
    assert!(text.contains("the bytecode VM does not support"), "{text}");
    assert_eq!(out.contents(), "");
}

#[test]
fn a_repl_session_is_refused_on_the_bytecode_engine() {
    let mut vm = Vm::builder().engine(EngineKind::Bytecode).build();
    let error = vm.session_start().expect_err("no session on the VM yet");
    assert!(
        message(&error).contains("does not support REPL sessions"),
        "{}",
        message(&error)
    );
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
    // A program loaded the ordinary way is no session on the VM either.
    assert_eq!(commit_without_a_session(EngineKind::Bytecode, true), tree);
}

#[test]
fn a_session_display_after_an_ordinary_load_does_not_panic_on_the_bytecode_engine() {
    let mut vm = Vm::builder().engine(EngineKind::Bytecode).build();
    vm.load_source("a.colibri", "let x = 1\n").unwrap();
    assert!(vm.session_display("x").is_none());
    assert!(vm.session_display("nobody").is_none());
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
    // One Vm, three tests: the middle one waits in a host function beside a coroutine, which the
    // VM refuses. The refusal
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
    assert!(!vm[1].0, "the host wait is refused: {vm:?}");
    assert_eq!(vm[2], tree[2], "{vm:?}");
}
