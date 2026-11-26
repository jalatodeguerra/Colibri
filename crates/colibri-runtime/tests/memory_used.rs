//! `Vm::memory_used` with the counting allocator installed (phase 3 L13, ARCHITECTURE §35). This
//! binary is the only one that installs it, so the other test binaries run on the plain allocator.

use std::alloc::System;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Mutex;

use colibri_runtime as colibri;

#[global_allocator]
static ALLOC: colibri::CountingAlloc<System> = colibri::CountingAlloc(System);

/// Elements the tests grow lists by. A list slot is at least 8 bytes, so this many hold at least
/// `GROWN` bytes.
const N: i64 = 100_000;
const GROWN: u64 = N as u64 * 8;

/// What a `Vm`'s own bookkeeping may keep between two readings of an idle `Vm`.
const SLACK: u64 = 32 * 1024;

const PROGRAM: &str = r#"
fn build(n: int) {
    let mut xs: List[int] = []
    for i in 0..n {
        xs.push(i)
    }
    sink(xs)
}

let mut kept: List[int] = []

fn keep(n: int) {
    for i in 0..n {
        kept.push(i)
    }
}

fn len() -> int => kept.len()
"#;

thread_local! {
    /// The list `sink` was given: held by the host, so that it outlives the entry.
    static SUNK: RefCell<Option<colibri::Value>> = const { RefCell::new(None) };
}

/// Grows a list in an entry of `vm` and returns it to the host through `sink`.
fn build_list(vm: &mut colibri::Vm, n: i64) -> colibri::Value {
    let build = vm.get_fn::<(i64,), ()>("build").unwrap();
    build.call(vm, (n,)).unwrap();
    SUNK.with(|s| s.borrow_mut().take())
        .expect("the script sank a list")
}

fn vm() -> colibri::Vm {
    let mut vm = colibri::Vm::builder().build();
    vm.register_fn("sink", |v: colibri::Value| {
        SUNK.with(|s| *s.borrow_mut() = Some(v));
    })
    .unwrap();
    vm.load_source("mem.colibri", PROGRAM).unwrap();
    vm
}

fn used(vm: &colibri::Vm) -> u64 {
    vm.memory_used().expect("the allocator is installed")
}

#[test]
fn the_allocator_is_installed_here() {
    assert!(colibri_core::mem::installed());
    let vm = colibri::Vm::builder().build();
    assert!(vm.memory_used().is_some());
}

#[test]
fn a_list_grown_in_an_entry_raises_memory_used() {
    let mut vm = vm();
    let base = used(&vm);
    let list = build_list(&mut vm, N);
    let grown = used(&vm);
    eprintln!("base {base}, grown {grown}");
    assert!(grown >= base + GROWN, "base {base}, grown {grown}");
    drop(list);
}

#[test]
fn dropping_the_list_after_the_entry_returns_to_the_baseline() {
    let mut vm = vm();
    let base = used(&vm);
    let list = build_list(&mut vm, N);
    let grown = used(&vm);
    assert!(grown >= base + GROWN, "base {base}, grown {grown}");
    // The list dies here, outside any entry: its allocations are credited to the `Vm` that made
    // them all the same.
    drop(list);
    let after = used(&vm);
    eprintln!("base {base}, grown {grown}, after {after}");
    assert!(after <= base + SLACK, "base {base}, after {after}");
}

#[test]
fn two_vms_on_one_thread_are_accounted_separately() {
    let mut a = vm();
    let mut b = vm();
    let (base_a, base_b) = (used(&a), used(&b));
    let keep_a = a.get_fn::<(i64,), ()>("keep").unwrap();
    let keep_b = b.get_fn::<(i64,), ()>("keep").unwrap();
    keep_a.call(&mut a, (N,)).unwrap();
    assert!(used(&a) >= base_a + GROWN);
    assert!(used(&b) <= base_b + SLACK, "b was charged for a's list");
    keep_b.call(&mut b, (N / 20,)).unwrap();
    let (grown_a, grown_b) = (used(&a), used(&b));
    assert!(grown_b >= base_b + GROWN / 20);
    assert!(grown_b < base_b + GROWN / 2, "b was charged for a's list");
    assert!(grown_a >= base_a + GROWN);
    // Dropping one gives nothing back to the other.
    drop(b);
    assert_eq!(used(&a), grown_a);
}

#[test]
fn a_vm_inside_a_host_function_charges_the_inner_vm() {
    let inner = Rc::new(RefCell::new(vm()));
    let inner_base = used(&inner.borrow());
    let inner_keep = inner.borrow().get_fn::<(i64,), ()>("keep").unwrap();
    let mut outer = colibri::Vm::builder().build();
    let seen = Rc::new(Cell::new(0u64));
    {
        let inner = Rc::clone(&inner);
        let seen = Rc::clone(&seen);
        outer
            .register_fn("work_inside", move || {
                let mut v = inner.borrow_mut();
                inner_keep.call(&mut *v, (N,)).unwrap();
                seen.set(v.memory_used().unwrap());
            })
            .unwrap();
    }
    outer
        .load_source(
            "outer.colibri",
            "let mut mine: List[int] = []\nfn start(n: int) {\n    work_inside()\n    for i in 0..n {\n        mine.push(i)\n    }\n}\n",
        )
        .unwrap();
    let outer_base = used(&outer);
    let go = outer.get_fn::<(i64,), ()>("start").unwrap();
    go.call(&mut outer, (N / 20,)).unwrap();
    let outer_used = used(&outer);
    let inner_used = used(&inner.borrow());
    eprintln!("outer {outer_base} -> {outer_used}, inner {inner_base} -> {inner_used}");
    // The inner entry charged the inner `Vm` ...
    assert!(inner_used >= inner_base + GROWN);
    assert_eq!(seen.get(), inner_used);
    // ... not the outer one, which holds only what it allocated itself ...
    assert!(
        outer_used < outer_base + GROWN / 2,
        "the outer Vm paid for the inner list"
    );
    // ... and the outer entry went on being charged to the outer `Vm` after the inner one ended.
    assert!(outer_used >= outer_base + GROWN / 20);
}

static STASH: Mutex<Vec<Vec<u8>>> = Mutex::new(Vec::new());

#[test]
fn a_freed_vms_account_is_not_confused_with_a_new_ones() {
    // A host function keeps 10 MiB made during `a`'s entry; `a` goes away; `b` is made.
    let mut a = colibri::Vm::builder().build();
    a.register_fn("stash", || {
        STASH.lock().unwrap().push(vec![7u8; 10 << 20]);
    })
    .unwrap();
    a.load_source("a.colibri", "fn start() {\n    stash()\n}\n")
        .unwrap();
    let go = a.get_fn::<(), ()>("start").unwrap();
    go.call(&mut a, ()).unwrap();
    assert!(used(&a) >= 10 << 20);
    drop(a);
    let mut b = vm();
    let before = used(&b);
    assert!(before > 0, "b holds its program");
    // The stash dies after `a`: crediting it must not touch `b`.
    STASH.lock().unwrap().clear();
    assert_eq!(used(&b), before);
    let keep = b.get_fn::<(i64,), ()>("keep").unwrap();
    keep.call(&mut b, (1000,)).unwrap();
    assert!(used(&b) > before);
}

thread_local! {
    static HELD: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

#[test]
fn realloc_keeps_the_account_that_made_the_allocation() {
    let mut vm = colibri::Vm::builder().build();
    vm.register_fn("hold", || {
        HELD.with(|h| h.borrow_mut().extend_from_slice(&[1u8; 64]));
    })
    .unwrap();
    vm.load_source("hold.colibri", "fn start() {\n    hold()\n}\n")
        .unwrap();
    let go = vm.get_fn::<(), ()>("start").unwrap();
    go.call(&mut vm, ()).unwrap();
    let held = used(&vm);
    // Outside any entry the vector grows by about a MiB, which `realloc` moves: the growth
    // belongs to the account of the first allocation, not to the thread's current one (none).
    HELD.with(|h| h.borrow_mut().resize(1 << 20, 2));
    let grown = used(&vm);
    assert!(grown >= held + (1 << 20) - 64, "held {held}, grown {grown}");
    // Shrinking gives it back to the same account.
    HELD.with(|h| {
        let mut h = h.borrow_mut();
        h.truncate(16);
        h.shrink_to_fit();
    });
    let shrunk = used(&vm);
    assert!(shrunk < held + SLACK, "held {held}, shrunk {shrunk}");
    HELD.with(|h| *h.borrow_mut() = Vec::new());
    assert!(used(&vm) < held, "the last bytes were credited too");
}
