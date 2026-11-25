//! The entry of a program as coroutines (`Vm::load_from_deferred`, `start_entry`, `entry_step`):
//! what a host that cannot block (the web runner) does instead of `load` and `run_main`.

use std::path::Path;

use colibri_runtime as colibri;

fn start(source: &str) -> (colibri::Vm, colibri::SharedBuffer) {
    let out = colibri::SharedBuffer::new();
    let mut vm = colibri::Vm::builder().stdout(Box::new(out.clone())).build();
    let mut fs = colibri::MemoryFs::new();
    fs.insert("main.colibri", source);
    vm.load_from_deferred(&fs, colibri::Entry::File(Path::new("main.colibri")))
        .expect("the program loads");
    (vm, out)
}

#[test]
fn a_wait_in_the_top_level_statements_suspends_instead_of_running_the_loop() {
    let (mut vm, out) = start("print(\"a\")\nwait(1.0)\nprint(\"b\")\n");
    // Loading ran the modules' initialisers, not the statements.
    assert_eq!(out.contents(), "");
    vm.start_entry().expect("starts");
    assert_eq!(out.contents(), "a\n", "it ran up to its wait");
    assert!(!vm.entry_step().unwrap(), "still running");
    vm.tick(0.5).unwrap();
    assert_eq!(out.contents(), "a\n");
    vm.tick(0.5).unwrap();
    assert_eq!(out.contents(), "a\nb\n");
    assert!(vm.entry_step().unwrap(), "over");
    assert_eq!(vm.coroutine_count(), 0);
}

#[test]
fn main_starts_after_the_top_level_statements_have_ended() {
    let (mut vm, out) = start(
        "print(\"top\")\nwait(0.5)\nprint(\"top woke\")\nfn main() {\n    print(\"main\")\n    wait(0.5)\n    print(\"main woke\")\n}\n",
    );
    vm.start_entry().unwrap();
    assert_eq!(out.contents(), "top\n");
    vm.tick(0.5).unwrap();
    assert!(!vm.entry_step().unwrap(), "main is running now");
    assert_eq!(out.contents(), "top\ntop woke\nmain\n");
    vm.tick(0.5).unwrap();
    assert!(vm.entry_step().unwrap());
    assert_eq!(out.contents(), "top\ntop woke\nmain\nmain woke\n");
}

#[test]
fn a_main_that_returns_err_after_a_wait_is_reported_by_the_step() {
    let (mut vm, _out) =
        start("fn main() -> Result[()] {\n    wait(0.1)\n    return err(\"nope\")\n}\n");
    vm.start_entry().unwrap();
    assert!(!vm.entry_step().unwrap());
    vm.tick(0.2).unwrap();
    match vm.entry_step() {
        Err(colibri::Error::MainReturnedErr { message }) => assert!(message.contains("nope")),
        other => panic!("expected the error of main, got {other:?}"),
    }
}

#[test]
fn a_panic_in_the_entry_is_reported_by_the_tick_or_by_the_start_and_never_twice() {
    // After a wait: the tick reports it.
    let (mut vm, _out) = start("fn main() {\n    wait(0.1)\n    panic(\"late\")\n}\n");
    vm.start_entry().unwrap();
    let error = vm.tick(0.2).expect_err("the panic");
    assert!(error.to_string().contains("late"), "{error}");
    assert!(vm.entry_step().is_ok(), "the step has nothing more to say");
    // Before the first wait: the start reports it, once.
    let (mut vm, _out) = start("fn main() {\n    panic(\"early\")\n}\n");
    let error = vm.start_entry().expect_err("the panic");
    assert!(error.to_string().contains("early"), "{error}");
    assert!(vm.entry_step().unwrap(), "and the entry is over");
    assert!(vm.tick(0.1).is_ok(), "nothing is left to report");
}

#[test]
fn a_program_with_no_main_and_no_statements_is_over_at_once() {
    let (mut vm, _out) = start("fn helper() {}\n");
    vm.start_entry().unwrap();
    assert!(vm.entry_step().unwrap());
}

#[test]
fn the_next_wake_says_when_a_tick_is_needed() {
    let (mut vm, _out) = start("fn main() {\n    wait(2.0)\n}\n");
    vm.start_entry().unwrap();
    let wake = vm.next_wake(0.0).expect("a timer");
    assert!((wake - 2.0).abs() < 1e-9, "{wake}");
    vm.tick(0.5).unwrap();
    let wake = vm.next_wake(0.0).expect("a timer");
    assert!((wake - 1.5).abs() < 1e-9, "{wake}");
}

#[test]
fn a_vm_dropped_in_the_middle_of_its_entry_releases_it() {
    // The handle of the entry's coroutine must be let go of before the engine shuts down (a class
    // object dropped without being released is a failed assertion in a debug build).
    let (mut vm, _out) = start("fn main() {\n    wait(5.0)\n}\n");
    vm.start_entry().unwrap();
    drop(vm);
}
