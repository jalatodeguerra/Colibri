//! Memory and `Shared[T]` (phase 3 L20, ARCHITECTURE §35.1, §36.6): the frozen graph is charged to
//! the isolate that made the `Shared`, each isolate's copy to the isolate that keeps it, a second
//! `get` costs nothing, and the copies go when the last handle does. This binary installs the
//! counting allocator, as every host that counts memory must.

use std::alloc::System;
use std::io::Write;
use std::sync::{Arc, Mutex};

use colibri_runtime as colibri;

#[global_allocator]
static ALLOC: colibri::CountingAlloc<System> = colibri::CountingAlloc(System);

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

/// The list every program shares: 100,000 integers.
const BIG: &str = r#"
fn big() -> List[int] {
    let mut items: List[int] = []
    for i in 0..100_000 {
        items.push(i)
    }
    return items
}
"#;

fn vm_with(workers: usize, limit: Option<u64>, source: &str) -> (colibri::Vm, Out) {
    let out = Out::default();
    let mut builder = colibri::Vm::builder()
        .worker_threads(workers)
        .stdout_shared(Box::new(out.clone()));
    if let Some(limit) = limit {
        builder = builder.memory_limit(limit);
    }
    let mut vm = builder.build();
    vm.load_source("shared_memory.colibri", source).unwrap();
    (vm, out)
}

fn used(vm: &colibri::Vm) -> u64 {
    vm.memory_used().expect("the allocator is installed")
}

fn call0(vm: &mut colibri::Vm, name: &str) -> Result<(), colibri::Error> {
    vm.get_fn::<(), ()>(name).unwrap().call(vm, ())
}

fn call_int(vm: &mut colibri::Vm, name: &str) -> Result<i64, colibri::Error> {
    vm.get_fn::<(), i64>(name).unwrap().call(vm, ())
}

/// What a list of 100,000 integers costs as an ordinary list (`list`), as the frozen graph of a
/// `Shared` (`graph`) and as one isolate's copy of it (`copy`), measured in this process.
struct Sizes {
    base: u64,
    list: u64,
    graph: u64,
    copy: u64,
}

fn sizes() -> Sizes {
    let (base, _) = vm_with(1, None, BIG);
    let base = used(&base);
    let (plain, _) = vm_with(1, None, &format!("{BIG}\nlet plain = big()\n"));
    let list = used(&plain) - base;
    let source =
        format!("{BIG}\nlet s = Shared(big())\nfn get1() -> int {{ return s.get().len() }}\n");
    let (mut shared, _) = vm_with(1, None, &source);
    let graph = used(&shared) - base;
    call_int(&mut shared, "get1").unwrap();
    let copy = used(&shared) - base - graph;
    Sizes {
        base,
        list,
        graph,
        copy,
    }
}

#[test]
fn the_frozen_graph_and_the_copy_are_charged_and_a_second_get_costs_nothing() {
    let z = sizes();
    // The copy is a list of the same 100,000 integers, at least as compact as one that was
    // pushed to; the graph is bigger than the list it was made from (its nodes carry ids).
    assert!(
        z.copy > z.list / 2 && z.copy <= z.list,
        "{} {}",
        z.copy,
        z.list
    );
    assert!(z.graph > z.copy, "{} {}", z.graph, z.copy);
    let source =
        format!("{BIG}\nlet s = Shared(big())\nfn get1() -> int {{ return s.get().len() }}\n");
    let (mut vm, _) = vm_with(1, None, &source);
    let before = used(&vm);
    call_int(&mut vm, "get1").unwrap();
    let once = used(&vm);
    for _ in 0..3 {
        call_int(&mut vm, "get1").unwrap();
    }
    // The first `get` built the copy; the others are a pointer copy.
    assert!(once - before >= z.copy * 9 / 10, "{}", once - before);
    assert!(used(&vm).abs_diff(once) < 4096, "{} {}", used(&vm), once);
}

#[test]
fn the_graph_and_the_copy_are_freed_when_the_last_handle_in_the_isolate_dies() {
    let source = format!(
        r#"{BIG}
let mut keep: List[Shared[List[int]]] = []
fn add() -> int {{
    keep.push(Shared(big()))
    return keep[keep.len() - 1].get().len()
}}
fn clear() {{
    keep.clear()
}}
fn scoped() -> int {{
    let s = Shared(big())
    return s.get().len()
}}
"#
    );
    let (mut vm, _) = vm_with(1, None, &source);
    let base = used(&vm);
    let z = sizes();
    // A `Shared` that lives in a table holds its graph and its copy.
    assert_eq!(call_int(&mut vm, "add").unwrap(), 100_000);
    let held = used(&vm);
    assert!(held - base >= z.graph + z.copy * 9 / 10, "{}", held - base);
    // The last handle dying frees both.
    call0(&mut vm, "clear").unwrap();
    assert!(used(&vm).abs_diff(base) < 16 * 1024, "{} {base}", used(&vm));
    // So does a `Shared` that goes out of scope with its function.
    for _ in 0..3 {
        assert_eq!(call_int(&mut vm, "scoped").unwrap(), 100_000);
        assert!(used(&vm).abs_diff(base) < 16 * 1024, "{} {base}", used(&vm));
    }
}

#[test]
fn a_second_handle_keeps_the_copy_alive_until_it_dies_too() {
    let source = format!(
        r#"{BIG}
let mut keep: List[Shared[List[int]]] = []
fn add() -> int {{
    let s = Shared(big())
    keep.push(s)
    keep.push(s)
    return s.get().len()
}}
fn drop_one() {{
    keep.pop()
}}
fn drop_both() {{
    keep.clear()
}}
"#
    );
    let (mut vm, _) = vm_with(1, None, &source);
    let base = used(&vm);
    call_int(&mut vm, "add").unwrap();
    let both = used(&vm);
    assert!(both - base > 1 << 20, "{}", both - base);
    call0(&mut vm, "drop_one").unwrap();
    // One handle is left, and the graph and the copy with it.
    assert!(used(&vm).abs_diff(both) < 1024, "{} {both}", used(&vm));
    call0(&mut vm, "drop_both").unwrap();
    assert!(used(&vm).abs_diff(base) < 16 * 1024, "{} {base}", used(&vm));
}

#[test]
fn a_worker_charges_its_copy_to_the_vm_and_frees_it_when_the_last_handle_dies() {
    let z = sizes();
    let source = format!(
        r#"{BIG}
let mut keep: List[Shared[List[int]]] = []
fn add() {{
    keep.push(Shared(big()))
}}
fn read() -> int {{
    let h = keep[0]
    return task.run(fn() => h.get().len()).join().expect("task")
}}
fn clear() {{
    keep.clear()
}}
fn nothing() -> int {{
    return task.run(fn() => 1).join().expect("task")
}}
"#
    );
    // One worker: every task is on it, and a task is never run by the joiner on the main thread.
    let (mut vm, _) = vm_with(1, None, &source);
    let base = used(&vm);
    call0(&mut vm, "add").unwrap();
    let graph = used(&vm);
    assert_eq!(call_int(&mut vm, "read").unwrap(), 100_000);
    let read = used(&vm);
    // The worker built a copy of its own: the `Vm` pays for it.
    assert!(read - graph >= z.copy * 9 / 10, "{}", read - graph);
    // A second task on the same worker finds the copy it made.
    assert_eq!(call_int(&mut vm, "read").unwrap(), 100_000);
    assert!(used(&vm).abs_diff(read) < 16 * 1024, "{} {read}", used(&vm));
    // Every handle is gone. The graph goes with the last one, and the idle worker is woken to
    // free its copy, without another task: the memory returns to what it was. (The worker frees it
    // on its own thread a moment after `clear` returns, so the test waits for it, with no limit on
    // how long it may take other than the minute that fails the test.)
    call0(&mut vm, "clear").unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while used(&vm).abs_diff(base) >= 32 * 1024 {
        assert!(
            std::time::Instant::now() < deadline,
            "the idle worker kept its copy: {} {base}",
            used(&vm)
        );
        std::thread::yield_now();
    }
    // The worker is still there and runs the next task as before.
    assert_eq!(call_int(&mut vm, "nothing").unwrap(), 1);
    assert!(used(&vm).abs_diff(base) < 32 * 1024, "{} {base}", used(&vm));
}

/// The program of the limit tests: a `Shared` (made while the list it copies is alive too), then
/// a list that stays, then `get`.
fn limit_program(task: bool) -> String {
    let get = if task {
        r#"let h = s
    return task.run(fn() => h.get().len()).join().expect("task")"#
    } else {
        "return s.get().len()"
    };
    format!(
        r#"{BIG}
let s = Shared(big())
let junk = big()
fn get1() -> int {{
    {get}
}}
"#
    )
}

#[test]
fn a_copy_that_does_not_fit_is_the_memory_limit_of_the_isolate_that_makes_it() {
    let z = sizes();
    // Made, and the list beside it: the peak is the list and the graph. The copy comes on top
    // of the graph and the second list, a copy (2.4 MB) more than that peak.
    let fits = z.base + z.list + z.graph;
    let does_not = z.base + z.graph + z.list + z.copy;
    let limit = (fits + does_not) / 2;
    assert!(limit > fits + z.copy / 4 && limit + z.copy / 4 < does_not);
    for task in [false, true] {
        let (mut vm, out) = vm_with(1, Some(limit), &limit_program(task));
        let outcome = call_int(&mut vm, "get1");
        if task {
            // A worker that cannot rebuild it fails the task, which the joiner sees as an `err`
            // that says why (the program `expect`s the join, so it panics with it).
            let err = outcome.unwrap_err();
            assert!(err.to_string().contains("memory limit"), "{err}");
        } else {
            let err = outcome.unwrap_err();
            assert!(
                matches!(err, colibri::Error::MemoryLimit { limit: l, .. } if l == limit),
                "{err:?}"
            );
        }
        assert_eq!(out.text(), "");
        drop(vm);
    }
    // The same program with room for the copy runs.
    let room = does_not + z.copy;
    for task in [false, true] {
        let (mut vm, _) = vm_with(1, Some(room), &limit_program(task));
        assert_eq!(call_int(&mut vm, "get1").unwrap(), 100_000);
    }
}

#[test]
fn a_shared_that_does_not_fit_when_it_is_made_is_the_memory_limit_of_the_program_that_makes_it() {
    let z = sizes();
    // The list and its graph do not fit together in less than their sum.
    let limit = z.base + z.list + z.graph / 2;
    let source =
        format!("{BIG}\nfn main() {{\n    let s = Shared(big())\n    print(\"made\")\n}}\n");
    let (mut vm, out) = vm_with(1, Some(limit), &source);
    let err = vm.run_main().unwrap_err();
    assert!(
        matches!(err, colibri::Error::MemoryLimit { limit: l, .. } if l == limit),
        "{err:?}"
    );
    assert_eq!(out.text(), "");
}
