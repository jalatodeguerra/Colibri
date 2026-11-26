//! A thread task's worker runs under its parent's memory account and limit (phase 3 L17,
//! ARCHITECTURE §35.1, §36.3). This binary installs the counting allocator, as every host that
//! counts memory must.

use std::alloc::System;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

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

fn ints(n: int) -> List[int] {
    let mut xs: List[int] = []
    for i in 0..n {
        xs.push(i)
    }
    return xs
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
    assert_eq!(run(&mut vm, "small", vec![]).unwrap().value.clone_int(), 3);
    assert!(vm.memory_used().unwrap() < 4 * MIB);
}

#[test]
fn a_result_too_large_to_copy_back_is_a_memory_limit_failure() {
    let mut vm = colibri::Vm::builder()
        .worker_threads(1)
        .memory_limit(8 * MIB)
        .build();
    vm.load_source("mem.colibri", PROGRAM).unwrap();
    // 200,000 integers are 3 MiB in the worker (the task fits, and passes no safepoint with the
    // flag raised), and their portable form is counted at more than the limit: the copy is
    // refused, which is the memory limit's failure and not a failure to copy.
    let why = run(&mut vm, "ints", vec![Portable::Int(200_000)]).unwrap_err();
    assert_eq!(why, TaskFailure::MemoryLimit { limit: 8 * MIB });
    // A smaller result goes through.
    let small = run(&mut vm, "ints", vec![Portable::Int(1_000)]).unwrap();
    assert!(matches!(&small.value, Portable::List { items, .. } if items.len() == 1_000));
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

// ---- one count, a limit for each isolate ----

/// The two sides of a rendezvous between the worker (parked in a host function, holding what it
/// allocated) and the main thread (in an entry). No clock: both ends block on channels.
struct Gate {
    /// The worker says it is over its limit and parked.
    over_tx: Mutex<mpsc::Sender<()>>,
    over_rx: Mutex<mpsc::Receiver<()>>,
    /// The main thread lets the worker go.
    go_tx: Mutex<mpsc::Sender<()>>,
    go_rx: Mutex<mpsc::Receiver<()>>,
}

impl Gate {
    fn new() -> Arc<Gate> {
        let (over_tx, over_rx) = mpsc::channel();
        let (go_tx, go_rx) = mpsc::channel();
        Arc::new(Gate {
            over_tx: Mutex::new(over_tx),
            over_rx: Mutex::new(over_rx),
            go_tx: Mutex::new(go_tx),
            go_rx: Mutex::new(go_rx),
        })
    }
}

const GATED: &str = r#"
fn entry_alloc() -> int {
    wait_over()
    let mut xs: List[int] = []
    for i in 0..200000 {
        xs.push(i)
    }
    return xs.len()
}

fn hog(n: int) -> int {
    grab()
    let mut t = 0
    for i in 0..n {
        t += 1
    }
    return t
}

fn entry() -> int {
    wait_over()
    let mut t = 0
    for i in 0..60000 {
        t += i % 7
    }
    return t
}
"#;

fn gated_vm(gate: &Arc<Gate>, limit: u64) -> colibri::Vm {
    let mut vm = colibri::Vm::builder()
        .worker_threads(1)
        .memory_limit(limit)
        .worker_setup({
            let gate = Arc::clone(gate);
            move |vm| {
                let gate = Arc::clone(&gate);
                // The worker allocates past the limit inside one host call (no safepoint sees the
                // flag before it is parked), tells the main thread, and waits to be let go.
                vm.register_fn("grab", move || {
                    let held = vec![1u8; 9 * MIB as usize];
                    gate.over_tx.lock().unwrap().send(()).unwrap();
                    gate.go_rx.lock().unwrap().recv().unwrap();
                    drop(std::hint::black_box(held));
                })
                .unwrap();
            }
        })
        .build();
    vm.register_fn("grab", || {}).unwrap();
    let waiting = Arc::clone(gate);
    vm.register_fn("wait_over", move || {
        waiting.over_rx.lock().unwrap().recv().unwrap();
    })
    .unwrap();
    vm.load_source("gated.colibri", GATED).unwrap();
    vm
}

#[test]
fn a_workers_breach_does_not_fail_the_main_threads_entry() {
    let gate = Gate::new();
    let mut vm = gated_vm(&gate, 8 * MIB);
    let task = vm
        .submit_portable("hog", vec![Portable::Int(20_000)])
        .unwrap();
    // The main entry runs (and passes several safepoints) while the worker holds more than the
    // limit and is parked: the worker's flag is the worker's, and main allocates nothing.
    let entry = vm.get_fn::<(), i64>("entry").unwrap();
    let called = entry.call(&mut vm, ());
    // Let the worker go whatever happened (a failed entry must fail the test, not hang it).
    gate.go_tx.lock().unwrap().send(()).unwrap();
    let total = called.expect("the main entry is not the worker's failure");
    assert_eq!(total, (0..60000_i64).map(|i| i % 7).sum::<i64>());
    // The worker's own limit failed its task (its flag was raised by its own allocation), with
    // the limit of the Vm.
    assert_eq!(
        task.wait().unwrap_err(),
        TaskFailure::MemoryLimit { limit: 8 * MIB }
    );
    // And what the worker held is back: the count is the main thread's alone.
    assert!(vm.memory_used().unwrap() < 4 * MIB);
}

#[test]
fn main_allocates_freely_while_a_worker_is_over_the_limit() {
    let gate = Gate::new();
    let mut vm = gated_vm(&gate, 8 * MIB);
    let task = vm
        .submit_portable("hog", vec![Portable::Int(20_000)])
        .unwrap();
    // The worker holds 9 MiB (over the limit) and is parked; the main entry builds a list of
    // 3 MiB meanwhile, which is far under the limit for what the main thread itself holds.
    let entry = vm.get_fn::<(), i64>("entry_alloc").unwrap();
    let called = entry.call(&mut vm, ());
    gate.go_tx.lock().unwrap().send(()).unwrap();
    assert_eq!(
        called.expect("main is not failed by a worker's memory"),
        200_000
    );
    assert_eq!(
        task.wait().unwrap_err(),
        TaskFailure::MemoryLimit { limit: 8 * MIB }
    );
}

#[test]
fn main_still_fails_for_its_own_allocations_while_a_worker_is_quiet() {
    let mut vm = colibri::Vm::builder()
        .worker_threads(1)
        .memory_limit(8 * MIB)
        .build();
    vm.load_source(
        "own.colibri",
        "fn grow(n: int) -> int {\n    let mut xs: List[int] = []\n    for i in 0..n {\n        xs.push(i)\n    }\n    return xs.len()\n}\n",
    )
    .unwrap();
    // A task ran and ended; the account of its worker does not change who is to blame.
    drop(run(&mut vm, "grow", vec![Portable::Int(100)]).unwrap());
    let grow = vm.get_fn::<(i64,), i64>("grow").unwrap();
    let err = grow.call(&mut vm, (4_000_000,)).unwrap_err();
    assert!(
        matches!(err, colibri::Error::MemoryLimit { limit, .. } if limit == 8 * MIB),
        "{err:?}"
    );
    // The `Vm` is usable afterwards, for the main thread and for a task.
    assert_eq!(grow.call(&mut vm, (10,)).unwrap(), 10);
    assert_eq!(
        run(&mut vm, "grow", vec![Portable::Int(10)])
            .unwrap()
            .value
            .clone_int(),
        10
    );
}

/// A channel end that a host function of a worker may own.
fn shared<T>(t: T) -> Arc<Mutex<T>> {
    Arc::new(Mutex::new(t))
}

#[test]
fn a_task_that_starts_does_not_end_the_main_threads_grace() {
    let (a_running_tx, a_running_rx) = mpsc::channel::<()>();
    let (go_a_tx, go_a_rx) = mpsc::channel::<()>();
    let (b_ran_tx, b_ran_rx) = mpsc::channel::<()>();
    let (a_running_tx, go_a_rx, b_ran_tx) =
        (shared(a_running_tx), shared(go_a_rx), shared(b_ran_tx));
    let out = colibri::SyncBuffer::new();
    let mut vm = colibri::Vm::builder()
        .worker_threads(1)
        .memory_limit(8 * MIB)
        .stdout_shared(Box::new(out.clone()))
        .worker_setup(move |vm| {
            let (a_running_tx, go_a_rx) = (Arc::clone(&a_running_tx), Arc::clone(&go_a_rx));
            vm.register_fn("park_a", move || {
                a_running_tx.lock().unwrap().send(()).unwrap();
                go_a_rx.lock().unwrap().recv().unwrap();
            })
            .unwrap();
            let b_ran_tx = Arc::clone(&b_ran_tx);
            vm.register_fn("ping_b", move || {
                b_ran_tx.lock().unwrap().send(()).unwrap();
            })
            .unwrap();
        })
        .build();
    vm.register_fn("park_a", || {}).unwrap();
    vm.register_fn("ping_b", || {}).unwrap();
    let a_running_rx = shared(a_running_rx);
    vm.register_fn("wait_a", move || {
        a_running_rx.lock().unwrap().recv().unwrap();
    })
    .unwrap();
    let go_a_tx = shared(go_a_tx);
    vm.register_fn("release_a", move || {
        go_a_tx.lock().unwrap().send(()).unwrap();
    })
    .unwrap();
    let b_ran_rx = shared(b_ran_rx);
    vm.register_fn("wait_b", move || {
        b_ran_rx.lock().unwrap().recv().unwrap();
    })
    .unwrap();
    vm.load_source(
        "grace.colibri",
        r#"
fn task_a() {
    park_a()
}

fn task_b() {
    ping_b()
}

fn cleanup() {
    release_a()
    wait_b()
    let s = "x".pad_left(100000)
    print("cleanup ran {s.len()}")
}

fn entry() {
    wait_a()
    let mut xs: List[int] = []
    defer cleanup()
    for i in 0..4000000 {
        xs.push(i)
    }
}
"#,
    )
    .unwrap();
    let a = vm.submit_portable("task_a", vec![]).unwrap();
    let b = vm.submit_portable("task_b", vec![]).unwrap();
    // The main entry passes its limit, and in the grace that follows (its `defer` runs) a worker
    // finishes one task and starts another under the same `Vm`: the main thread's grace is its
    // own, so the `defer` goes on to its end.
    let entry = vm.get_fn::<(), ()>("entry").unwrap();
    let err = entry.call(&mut vm, ()).unwrap_err();
    assert!(
        matches!(err, colibri::Error::MemoryLimit { limit, .. } if limit == 8 * MIB),
        "{err:?}"
    );
    assert_eq!(out.contents(), "cleanup ran 100000\n");
    assert!(a.wait().is_ok());
    assert!(b.wait().is_ok());
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
