//! Memory and `task.run` (phase 3 L18, ARCHITECTURE §35.1, §36.4): which account pays for the
//! copies a task makes, and what the memory limit does to a task and to the program that joins
//! it. This binary installs the counting allocator, as every host that counts memory must.

use std::alloc::System;
use std::io::Write;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use colibri_runtime as colibri;

#[global_allocator]
static ALLOC: colibri::CountingAlloc<System> = colibri::CountingAlloc(System);

const MIB: u64 = 1 << 20;

#[derive(Clone, Default)]
struct Out(Arc<Mutex<Vec<u8>>>);

impl Out {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

impl Write for Out {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A gate that a task blocks on until the test opens it (`gate_wait` in the task, `gate_open` in
/// the program), giving up after half a minute so that a bug fails the test instead of hanging.
#[derive(Clone)]
struct Gate {
    tx: mpsc::Sender<()>,
    rx: Arc<Mutex<mpsc::Receiver<()>>>,
}

fn vm_with(limit: Option<u64>, source: &str) -> (colibri::Vm, Out) {
    let (tx, rx) = mpsc::channel();
    let gate = Gate {
        tx,
        rx: Arc::new(Mutex::new(rx)),
    };
    let out = Out::default();
    let mut builder = colibri::Vm::builder()
        .worker_threads(1)
        .stdout_shared(Box::new(out.clone()));
    if let Some(limit) = limit {
        builder = builder.memory_limit(limit);
    }
    let wait = gate.clone();
    let mut vm = builder
        .worker_setup(move |vm| {
            let rx = Arc::clone(&wait.rx);
            vm.register_fn("gate_wait", move || -> i64 {
                match rx.lock().unwrap().recv_timeout(Duration::from_secs(30)) {
                    Ok(()) => 1,
                    Err(_) => -1,
                }
            })
            .unwrap();
        })
        .build();
    let rx = Arc::clone(&gate.rx);
    vm.register_fn("gate_wait", move || -> i64 {
        match rx.lock().unwrap().recv_timeout(Duration::from_secs(30)) {
            Ok(()) => 1,
            Err(_) => -1,
        }
    })
    .unwrap();
    vm.register_fn("gate_open", move || gate.tx.send(()).is_ok())
        .unwrap();
    vm.load_source("task_memory.colibri", source).unwrap();
    (vm, out)
}

#[test]
fn a_closure_too_big_to_copy_is_the_senders_memory_limit_not_a_failure_to_send() {
    let (mut vm, _out) = vm_with(
        Some(8 * MIB),
        r#"
fn main() {
    let mut items: List[int] = []
    for i in 0..200_000 {
        items.push(i)
    }
    // The list is 3 MiB; its copy is counted at more than what is left of the limit.
    let t = task.run(fn() => items.len())
    print("not reached {t.done()}")
}
"#,
    );
    let err = vm.run_main().unwrap_err();
    assert!(
        matches!(err, colibri::Error::MemoryLimit { limit, .. } if limit == 8 * MIB),
        "{err:?}"
    );
}

#[test]
fn a_task_that_passes_the_memory_limit_is_an_err_to_the_joiner_and_main_goes_on() {
    let (mut vm, out) = vm_with(
        Some(8 * MIB),
        r#"
fn hog() -> int {
    let mut xs: List[int] = []
    for i in 0..4_000_000 {
        xs.push(i)
    }
    return xs.len()
}

fn main() {
    match task.run(hog).join() {
        ok(v) => print("value {v}")
        err(e) => print("err: {e}")
    }
    print("main goes on: {task.run(fn() => 3).join().expect("small")}")
}
"#,
    );
    vm.run_main().unwrap();
    assert_eq!(
        out.text(),
        "err: the task `hog` failed: memory limit of 8 MiB exceeded\nmain goes on: 3\n"
    );
    // What the failed task held was freed on its thread and credited to the same account.
    assert!(
        vm.memory_used().unwrap() < 4 * MIB,
        "{:?}",
        vm.memory_used()
    );
}

#[test]
fn a_result_that_does_not_fit_the_joiners_limit_is_the_joiners_memory_limit() {
    // The task runs under the limit of the moment it was started; the program lowers its own
    // before it joins, so the result that the worker could send does not fit when it is
    // received: the joiner's limit failed, and it is not a failure to send.
    let (mut vm, _out) = vm_with(
        Some(64 * MIB),
        r#"
let mut pending: Task[List[int]]? = none

fn ints() -> List[int] {
    let mut xs: List[int] = []
    for i in 0..200_000 {
        xs.push(i)
    }
    gate_wait()
    return xs
}

fn start() {
    pending = some(task.run(ints))
}

fn finish() -> int {
    let t = pending.expect("started")
    pending = none
    gate_open()
    return t.join().expect("the task").len()
}
"#,
    );
    let start = vm.get_fn::<(), ()>("start").unwrap();
    start.call(&mut vm, ()).unwrap();
    vm.set_memory_limit(Some(2 * MIB));
    let finish = vm.get_fn::<(), i64>("finish").unwrap();
    let err = finish.call(&mut vm, ()).unwrap_err();
    assert!(
        matches!(err, colibri::Error::MemoryLimit { limit, .. } if limit == 2 * MIB),
        "{err:?}"
    );
}

#[test]
fn what_a_task_receives_is_credited_to_the_senders_account_when_it_is_freed() {
    // The closure's portable form is made on this thread, freed on the worker's (the rebuild
    // takes it), and the rebuilt copy dies on the worker: both go back to the account that was
    // charged, and the program's count is what it was.
    let (mut vm, _out) = vm_with(
        None,
        r#"
let mut pending: Task[int]? = none

fn start(n: int) {
    let blob = "x".pad_left(n)
    pending = some(task.run(fn() -> int {
        let size = blob.len()
        gate_wait()
        return size
    }))
}

fn finish() -> int {
    let t = pending.expect("started")
    pending = none
    gate_open()
    return t.join().expect("the task")
}
"#,
    );
    let start = vm.get_fn::<(i64,), ()>("start").unwrap();
    let finish = vm.get_fn::<(), i64>("finish").unwrap();
    // Warm the worker up (its program and compiled code stay charged to the parent).
    start.call(&mut vm, (1,)).unwrap();
    assert_eq!(finish.call(&mut vm, ()).unwrap(), 1);
    let before = vm.memory_used().expect("the allocator is installed");
    start.call(&mut vm, (8 * MIB as i64,)).unwrap();
    // The task holds a copy of its own, and the program holds the string it copied from: the
    // portable form between them was freed as soon as the worker had rebuilt the closure.
    let during = vm.memory_used().unwrap();
    eprintln!("receiver memory: before {before}, during {during}");
    assert!(during >= before + 8 * MIB, "{before} -> {during}");
    assert_eq!(finish.call(&mut vm, ()).unwrap(), 8 * MIB as i64);
    let after = vm.memory_used().unwrap();
    assert!(after < before + MIB, "{before} -> {after}");
}
