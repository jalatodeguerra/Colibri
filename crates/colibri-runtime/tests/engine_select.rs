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

#[test]
fn what_the_bytecode_engine_lacks_is_a_clean_panic_naming_it() {
    let (result, _) = run_on(
        EngineKind::Bytecode,
        "fn main() {\n    let xs = [1, 2, 3]\n    print(xs)\n}",
    );
    let text = message(&result.expect_err("lists are not on the VM yet"));
    assert!(text.contains("the bytecode VM does not support"), "{text}");
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

#[test]
fn the_tree_engine_runs_programs() {
    let (result, out) = run_on(EngineKind::Tree, "fn main() { print(\"tree\") }");
    result.unwrap();
    assert_eq!(out, "tree\n");
}

#[test]
fn both_engines_agree_on_a_small_program() {
    let src = "fn main() {\n    var s = 0\n    for i in 0..10 { s += i * i }\n    print(s)\n}";
    let tree = run_on(EngineKind::Tree, src);
    let vm = run_on(EngineKind::Bytecode, src);
    assert!(tree.0.is_ok() && vm.0.is_ok());
    assert_eq!(tree.1, vm.1);
}
