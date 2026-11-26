//! A thread task's worker runs under its parent's memory account and limit (phase 3 L17,
//! ARCHITECTURE §35.1, §36.3). This binary installs the counting allocator, as every host that
//! counts memory must.

use std::alloc::System;

use colibri::tasks::{Portable, TaskDone, TaskFailure};
use colibri_runtime as colibri;

#[global_allocator]
static ALLOC: colibri::CountingAlloc<System> = colibri::CountingAlloc(System);

const MIB: u64 = 1 << 20;

const PROGRAM: &str = r#"
fn blob(n: int) -> str {
    return "x".pad_left(n)
}

fn hog(n: int) -> int {
    let mut xs: List[int] = []
    for i in 0..n {
        xs.push(i)
    }
    return xs.len()
}

fn small() -> int {
    return 3
}
"#;

fn run(vm: &mut colibri::Vm, f: &str, args: Vec<Portable>) -> Result<TaskDone, TaskFailure> {
    vm.submit_portable(f, args).unwrap().wait()
}

#[test]
fn what_a_task_allocates_is_charged_to_the_parent_until_it_is_freed() {
    let mut vm = colibri::Vm::builder().worker_threads(1).build();
    vm.load_source("mem.colibri", PROGRAM).unwrap();
    // The worker's own set-up (its program, its compiled code) is the worker's, and is counted
    // once: warm it up first so that what follows is the task's result alone.
    drop(run(&mut vm, "small", vec![]).unwrap());
    let before = vm.memory_used().expect("the allocator is installed");
    let done = run(&mut vm, "blob", vec![Portable::Int(8 * MIB as i64)]).unwrap();
    let Portable::Str(text) = &done.value else {
        panic!("{:?}", done.value);
    };
    assert_eq!(text.len() as u64, 8 * MIB);
    // The 8 MiB string was copied out under the parent's account, and the parent holds it now.
    let held = vm.memory_used().unwrap();
    assert!(held >= before + 8 * MIB, "{before} -> {held}");
    // Freeing it on this thread credits the same account.
    drop(done);
    let after = vm.memory_used().unwrap();
    assert!(after < before + MIB, "{before} -> {after}");
}

#[test]
fn a_limit_breach_in_a_worker_is_a_memory_limit_failure_and_the_worker_goes_on() {
    let mut vm = colibri::Vm::builder()
        .worker_threads(1)
        .memory_limit(8 * MIB)
        .build();
    vm.load_source("mem.colibri", PROGRAM).unwrap();
    assert_eq!(
        run(&mut vm, "hog", vec![Portable::Int(1_000)])
            .map(|d| d.value)
            .unwrap()
            .clone_int(),
        1_000
    );
    // Four million 16-byte slots are far past 8 MiB.
    let why = run(&mut vm, "hog", vec![Portable::Int(4_000_000)]).unwrap_err();
    assert_eq!(why, TaskFailure::MemoryLimit { limit: 8 * MIB });
    // What the failed task held was freed: the worker is usable, and the parent's count is down.
    assert_eq!(
        run(&mut vm, "small", vec![]).unwrap().value.clone_int(),
        3
    );
    assert!(vm.memory_used().unwrap() < 4 * MIB);
}

#[test]
fn a_vm_without_a_limit_lets_the_same_task_finish() {
    let mut vm = colibri::Vm::builder().worker_threads(1).build();
    vm.load_source("mem.colibri", PROGRAM).unwrap();
    assert_eq!(
        run(&mut vm, "hog", vec![Portable::Int(300_000)])
            .unwrap()
            .value
            .clone_int(),
        300_000
    );
}

trait CloneInt {
    fn clone_int(&self) -> i64;
}

impl CloneInt for Portable {
    fn clone_int(&self) -> i64 {
        match self {
            Portable::Int(n) => *n,
            other => panic!("not an int: {other:?}"),
        }
    }
}
