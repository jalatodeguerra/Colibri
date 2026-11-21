//! Choosing the engine: the tree-walker is the default and the only one that runs programs
//! so far; the bytecode VM is refused, cleanly, at the first load.

use colibri_runtime as colibri;
use colibri_runtime::{EngineKind, Error, Vm};

const NOT_BUILT: &str = "the bytecode VM is not built yet";

fn message(error: &Error) -> String {
    error.render(false)
}

#[test]
fn the_bytecode_engine_is_refused_at_load() {
    let mut vm = Vm::builder().engine(EngineKind::Bytecode).build();
    let error = vm
        .load_source("a.colibri", "fn main() {}")
        .expect_err("the VM is not built yet");
    assert!(message(&error).contains(NOT_BUILT), "{}", message(&error));
    // Nothing was loaded: the failure leaves the Vm as it was.
    assert!(matches!(vm.run_main(), Err(Error::NotLoaded)));
}

#[test]
fn the_bytecode_engine_is_refused_for_every_way_in() {
    let mut vm = Vm::builder().engine(EngineKind::Bytecode).build();
    let error = vm.session_start().expect_err("no session on the VM yet");
    assert!(message(&error).contains(NOT_BUILT), "{}", message(&error));
    let error = vm
        .load_sources(&[("a.colibri", "fn main() {}")])
        .expect_err("no load on the VM yet");
    assert!(message(&error).contains(NOT_BUILT), "{}", message(&error));
}

#[test]
fn the_tree_engine_runs_programs() {
    let out = colibri::SharedBuffer::new();
    let mut vm = Vm::builder()
        .engine(EngineKind::Tree)
        .stdout(Box::new(out.clone()))
        .build();
    vm.load_source("a.colibri", "fn main() { print(\"tree\") }")
        .unwrap();
    vm.run_main().unwrap();
    assert_eq!(out.contents(), "tree\n");
}
