//! `memory_limit` (phase 3 L14, ARCHITECTURE §35.1, EMBEDDING §5): a `Vm` whose entries go past
//! its limit unwinds like a panic, with a grace, and the host sees `Error::MemoryLimit`. This
//! binary installs the counting allocator, as every host that sets a limit must.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::{Cell, RefCell};

use colibri_runtime as colibri;

/// The system allocator that also remembers the largest request this thread made, so that a test
/// can say that a built-in refused *before* it allocated.
struct Biggest;

thread_local! {
    static BIGGEST: Cell<usize> = const { Cell::new(0) };
}

fn note(size: usize) {
    let _ = BIGGEST.try_with(|b| b.set(b.get().max(size)));
}

// SAFETY: every method forwards to `System` unchanged.
unsafe impl GlobalAlloc for Biggest {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note(layout.size());
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        note(layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note(new_size);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOC: colibri::CountingAlloc<Biggest> = colibri::CountingAlloc(Biggest);

const MIB: u64 = 1 << 20;

/// What a `Vm`'s own bookkeeping may keep between two readings of an idle `Vm`.
const SLACK: u64 = 64 * 1024;

const PROGRAM: &str = r#"
fn small(n: int) -> int {
    let mut xs: List[int] = []
    for i in 0..n {
        xs.push(i)
    }
    return xs.len()
}

fn grow(n: int) -> int {
    let mut xs: List[int] = []
    for i in 0..n {
        xs.push(i)
    }
    return xs.len()
}

fn double(times: int) -> int {
    let mut s = "xxxxxxxx"
    let mut i = 0
    while i < times {
        s = s + s
        i += 1
    }
    return s.len()
}

fn pad(n: int) -> int {
    let s = "x".pad_left(n)
    return s.len()
}

fn with_defer() {
    let mut xs: List[int] = []
    defer {
        let mut tmp: List[int] = []
        for i in 0..20000 {
            tmp.push(i)
        }
        print("cleanup {tmp.len()}")
    }
    while true {
        xs.push(1)
    }
}

class Hog { label: str }

impl Drop for Hog {
    fn drop(self) {
        print("hook {self.label}")
        let mut more: List[int] = []
        while true {
            more.push(1)
        }
    }
}

fn hooks() {
    let a = Hog { label: "a" }
    let b = Hog { label: "b" }
    let mut xs: List[int] = []
    while true {
        xs.push(1)
    }
}

fn two_defers() {
    defer print("never printed")
    defer {
        print("first")
        let mut more: List[int] = []
        while true {
            more.push(1)
        }
    }
    let mut xs: List[int] = []
    while true {
        xs.push(1)
    }
}

fn spin_hosts(n: int) {
    for i in 0..n {
        stash(16384)
    }
}

fn call_hog(n: int) -> int => hog(n)
"#;

fn vm(limit: Option<u64>) -> (colibri::Vm, colibri::SharedBuffer) {
    let out = colibri::SharedBuffer::new();
    let mut b = colibri::Vm::builder().stdout(Box::new(out.clone()));
    if let Some(limit) = limit {
        b = b.memory_limit(limit);
    }
    let mut vm = b.build();
    vm.register_fn("stash", |n: i64| {
        STASH.with(|s| s.borrow_mut().push(vec![1u8; n as usize]));
    })
    .unwrap();
    // A host function that allocates in proportion to its argument and asks first.
    vm.register_fn("hog", |cx: &mut colibri::Ctx, n: i64| -> i64 {
        cx.mem_reserve(n as usize);
        "x".repeat(n as usize).len() as i64
    })
    .unwrap();
    vm.load_source("limit.colibri", PROGRAM).unwrap();
    (vm, out)
}

thread_local! {
    static STASH: RefCell<Vec<Vec<u8>>> = const { RefCell::new(Vec::new()) };
}

fn used(vm: &colibri::Vm) -> u64 {
    vm.memory_used().expect("the allocator is installed")
}

fn call(vm: &mut colibri::Vm, name: &str, n: i64) -> Result<i64, colibri::Error> {
    vm.get_fn::<(i64,), i64>(name).unwrap().call(vm, (n,))
}

fn call0(vm: &mut colibri::Vm, name: &str) -> Result<(), colibri::Error> {
    vm.get_fn::<(), ()>(name).unwrap().call(vm, ())
}

/// The limit, the function the script was in, and the rendered text of a memory-limit error.
fn exceeded(err: &colibri::Error) -> (u64, String, String) {
    match err {
        colibri::Error::MemoryLimit {
            limit,
            location,
            stack,
        } => {
            assert!(location.is_some(), "the script was running: {err:?}");
            let top = stack
                .first()
                .map(|f| f.function.clone())
                .unwrap_or_default();
            (*limit, top, err.render(false))
        }
        other => panic!("expected the memory limit, got {other:?}"),
    }
}

#[test]
fn a_list_grown_in_a_loop_hits_the_limit_and_the_vm_is_reusable() {
    let (mut vm, _) = vm(Some(8 * MIB));
    let base = used(&vm);
    let err = call(&mut vm, "grow", 100_000_000).unwrap_err();
    let (limit, top, text) = exceeded(&err);
    assert_eq!(limit, 8 * MIB);
    assert_eq!(top, "grow");
    assert_eq!(
        text,
        "error: memory limit of 8 MiB exceeded\n  at grow (limit.colibri:12:5)\n"
    );
    // The list was released by the unwind: the account is back near where it began, ...
    let after = used(&vm);
    eprintln!("base {base}, after {after}");
    assert!(after <= base + SLACK, "base {base}, after {after}");
    // ... and the next entries run: a small job fits, a big one is stopped again.
    assert_eq!(call(&mut vm, "small", 1000).unwrap(), 1000);
    assert!(matches!(
        call(&mut vm, "grow", 100_000_000),
        Err(colibri::Error::MemoryLimit { .. })
    ));
    assert_eq!(call(&mut vm, "small", 1000).unwrap(), 1000);
}

#[test]
fn doubling_a_string_in_a_loop_hits_the_limit() {
    let (mut vm, _) = vm(Some(8 * MIB));
    // 8 bytes doubled 26 times is 512 MiB, and the loop is far shorter than a slice of 4,096
    // steps: only the check of the concatenation itself can stop it.
    let err = call(&mut vm, "double", 26).unwrap_err();
    let (limit, top, _) = exceeded(&err);
    assert_eq!((limit, top.as_str()), (8 * MIB, "double"));
    assert!(used(&vm) <= 2 * MIB, "{}", used(&vm));
    assert_eq!(call(&mut vm, "double", 10).unwrap(), 8 * 1024);
}

/// What the largest single allocation of this thread was while `f` ran.
fn biggest_during<R>(f: impl FnOnce() -> R) -> (R, usize) {
    BIGGEST.with(|b| b.set(0));
    let r = f();
    (r, BIGGEST.with(Cell::get))
}

#[test]
fn a_built_in_that_allocates_by_an_argument_fails_before_it_allocates() {
    let (mut vm, _) = vm(Some(8 * MIB));
    // `pad_left` builds a string as long as the width; 100 MB is under the built-in's own
    // limit of a gigabyte, so only the reserve stops it.
    let (res, biggest) = biggest_during(|| call(&mut vm, "pad", 100_000_000));
    let err = res.unwrap_err();
    assert!(matches!(err, colibri::Error::MemoryLimit { .. }), "{err:?}");
    assert!(
        biggest < MIB as usize,
        "it allocated {biggest} bytes before failing"
    );
    // A width that fits is fine.
    assert_eq!(call(&mut vm, "pad", 1000).unwrap(), 1000);
}

#[test]
fn a_host_function_that_reserves_fails_before_it_allocates() {
    let (mut vm, _) = vm(Some(8 * MIB));
    let (res, biggest) = biggest_during(|| call(&mut vm, "call_hog", 100_000_000));
    let err = res.unwrap_err();
    let (limit, top, text) = exceeded(&err);
    // The host function's own frame is the innermost.
    assert_eq!((limit, top.as_str()), (8 * MIB, "hog"));
    assert!(
        text.starts_with("error: memory limit of 8 MiB exceeded\n"),
        "{text}"
    );
    assert!(
        biggest < MIB as usize,
        "it allocated {biggest} bytes before failing"
    );
    assert_eq!(call(&mut vm, "call_hog", 1000).unwrap(), 1000);
    // Without a limit the same call goes through.
    vm.set_memory_limit(None);
    assert_eq!(call(&mut vm, "call_hog", 5_000_000).unwrap(), 5_000_000);
}

#[test]
fn a_defer_runs_during_the_grace() {
    let (mut vm, out) = vm(Some(8 * MIB));
    let err = call0(&mut vm, "with_defer").unwrap_err();
    assert!(matches!(err, colibri::Error::MemoryLimit { .. }), "{err:?}");
    // The defer allocates a fifth of a MiB, over the limit the script held when it was stopped:
    // it ran to its end because the unwinding has 1 MiB more.
    assert_eq!(out.contents(), "cleanup 20000\n");
}

#[test]
fn a_drop_hook_that_allocates_forever_is_cut_off() {
    let (mut vm, out) = vm(Some(8 * MIB));
    let base = used(&vm);
    let err = call0(&mut vm, "hooks").unwrap_err();
    assert!(matches!(err, colibri::Error::MemoryLimit { .. }), "{err:?}");
    // The hook of `b` ran until the grace was used up; after that, `a`'s hook was skipped.
    assert_eq!(out.contents(), "hook b\n");
    assert!(used(&vm) <= base + SLACK, "{}", used(&vm));
    // The same call is stopped the same way again.
    assert!(matches!(
        call0(&mut vm, "hooks"),
        Err(colibri::Error::MemoryLimit { .. })
    ));
    assert_eq!(out.contents(), "hook b\nhook b\n");
}

#[test]
fn defers_after_the_grace_is_gone_are_skipped() {
    let (mut vm, out) = vm(Some(8 * MIB));
    let err = call0(&mut vm, "two_defers").unwrap_err();
    assert!(matches!(err, colibri::Error::MemoryLimit { .. }), "{err:?}");
    assert_eq!(out.contents(), "first\n");
}

fn coroutine_vm() -> (colibri::Vm, colibri::SharedBuffer) {
    let out = colibri::SharedBuffer::new();
    let mut vm = colibri::Vm::builder()
        .stdout(Box::new(out.clone()))
        .memory_limit(8 * MIB)
        .build();
    vm.load_source(
        "co.colibri",
        r#"
fn eat() {
    wait(1.0)
    let mut xs: List[int] = []
    while true {
        xs.push(1)
    }
}
fn late(c: Coroutine[()]) {
    wait(1.0)
    print("late got {c.join_result().is_err()}")
}
fn joiner(c: Coroutine[()]) {
    wait(1.0)
    c.join()
}
fn main() {
    let c = go eat()
    go late(c)
}
fn main_join() {
    let c = go eat()
    go joiner(c)
}
"#,
    )
    .unwrap();
    (vm, out)
}

#[test]
fn a_coroutine_over_the_limit_reaches_the_host_and_not_join_result() {
    let (mut vm, out) = coroutine_vm();
    vm.run_main().unwrap();
    let err = vm.tick(1.0).unwrap_err();
    assert!(matches!(err, colibri::Error::MemoryLimit { .. }), "{err:?}");
    // A `join_result` that came after it got an error value, and the host got the limit all
    // the same.
    assert_eq!(out.contents(), "late got true\n");
}

#[test]
fn a_joiner_of_a_coroutine_over_the_limit_does_not_take_it_over() {
    let (mut vm, _) = coroutine_vm();
    call0(&mut vm, "main_join").unwrap();
    let err = vm.tick(1.0).unwrap_err();
    assert!(matches!(err, colibri::Error::MemoryLimit { .. }), "{err:?}");
}

#[test]
fn two_vms_on_one_thread_are_limited_separately() {
    let (mut a, _) = vm(Some(MIB));
    let (mut b, _) = vm(Some(64 * MIB));
    // 100,000 integers take more than 1 MiB and far less than 64 MiB.
    assert!(matches!(
        call(&mut a, "grow", 100_000),
        Err(colibri::Error::MemoryLimit { limit, .. }) if limit == MIB
    ));
    assert_eq!(call(&mut b, "grow", 100_000).unwrap(), 100_000);
    // `a` failing did not leave `b` over a limit of its own, nor `b` succeeding `a` under one.
    assert_eq!(call(&mut a, "small", 100).unwrap(), 100);
    assert!(matches!(
        call(&mut a, "grow", 100_000),
        Err(colibri::Error::MemoryLimit { .. })
    ));
    assert_eq!(call(&mut b, "grow", 100_000).unwrap(), 100_000);
}

thread_local! {
    static SUNK: RefCell<Vec<colibri::Value>> = const { RefCell::new(Vec::new()) };
}

#[test]
fn a_value_freed_after_the_entry_is_credited() {
    let mut vm = colibri::Vm::builder().memory_limit(4 * MIB).build();
    vm.register_fn("sink", |v: colibri::Value| {
        SUNK.with(|s| s.borrow_mut().push(v));
    })
    .unwrap();
    vm.load_source(
        "sunk.colibri",
        "fn build(n: int) {\n    let mut xs: List[int] = []\n    for i in 0..n {\n        xs.push(i)\n    }\n    sink(xs)\n}\n",
    )
    .unwrap();
    let base = used(&vm);
    let build = vm.get_fn::<(i64,), ()>("build").unwrap();
    // Each list is a good part of the limit; the host frees it between entries. If the free did
    // not credit the account, the third round would be over the limit.
    for round in 0..8 {
        build
            .call(&mut vm, (60_000,))
            .unwrap_or_else(|e| panic!("round {round}: {e:?}"));
        let held = used(&vm);
        eprintln!("round {round}: held {held}");
        assert!(held > base + MIB, "round {round}: {held}");
        SUNK.with(|s| s.borrow_mut().clear());
        let after = used(&vm);
        assert!(
            after <= base + SLACK,
            "round {round}: base {base}, after {after}"
        );
    }
}

#[test]
fn set_memory_limit_none_lifts_the_limit() {
    let (mut vm, _) = vm(Some(4 * MIB));
    assert!(matches!(
        call(&mut vm, "grow", 1_000_000),
        Err(colibri::Error::MemoryLimit { .. })
    ));
    vm.set_memory_limit(None);
    assert_eq!(call(&mut vm, "grow", 1_000_000).unwrap(), 1_000_000);
    vm.set_memory_limit(Some(4 * MIB));
    assert!(matches!(
        call(&mut vm, "grow", 1_000_000),
        Err(colibri::Error::MemoryLimit { .. })
    ));
    // A limit can be raised too.
    vm.set_memory_limit(Some(256 * MIB));
    assert_eq!(call(&mut vm, "grow", 1_000_000).unwrap(), 1_000_000);
}

#[test]
fn a_host_functions_allocations_count_against_the_limit() {
    let (mut vm, _) = vm(Some(8 * MIB));
    let base = used(&vm);
    // 10,000 calls keep 16 KiB each, 160 MB in all, and the loop is long enough for a safepoint.
    let err = vm
        .get_fn::<(i64,), ()>("spin_hosts")
        .unwrap()
        .call(&mut vm, (10_000,))
        .unwrap_err();
    assert!(matches!(err, colibri::Error::MemoryLimit { .. }), "{err:?}");
    let kept: usize = STASH.with(|s| s.borrow().iter().map(Vec::len).sum());
    assert!(kept < 64 * MIB as usize, "the overshoot is bounded: {kept}");
    // What the host function holds is still the `Vm`'s, until the host lets go of it.
    assert!(used(&vm) > base + 4 * MIB, "{}", used(&vm));
    STASH.with(|s| s.borrow_mut().clear());
    assert!(used(&vm) <= base + SLACK, "{}", used(&vm));
}

#[test]
fn a_vm_without_a_limit_pays_for_no_safepoint() {
    let (mut plain, _) = vm(None);
    assert_eq!(call(&mut plain, "small", 50_000).unwrap(), 50_000);
    assert_eq!(plain.safepoints_run(), 0);
    let (mut limited, _) = vm(Some(64 * MIB));
    assert_eq!(call(&mut limited, "small", 50_000).unwrap(), 50_000);
    assert!(limited.safepoints_run() > 0);
    // Lifting the limit stops them again.
    limited.set_memory_limit(None);
    let before = limited.safepoints_run();
    assert_eq!(call(&mut limited, "small", 50_000).unwrap(), 50_000);
    assert_eq!(limited.safepoints_run(), before);
}

#[test]
fn the_limit_is_checked_again_at_each_entry() {
    let (mut vm, _) = vm(Some(8 * MIB));
    // An entry that went over and unwound does not leave the next one with a spent grace.
    for _ in 0..3 {
        assert!(matches!(
            call(&mut vm, "grow", 100_000_000),
            Err(colibri::Error::MemoryLimit { .. })
        ));
        assert_eq!(call(&mut vm, "small", 100).unwrap(), 100);
    }
}

/// A stricter reading of "bounded overshoot": with the reserve checks in place, a statement that
/// asks for a lot never gets it, so the largest single allocation of a refused call stays small.
#[test]
fn a_refused_concatenation_allocates_nothing_big() {
    let (mut vm, _) = vm(Some(8 * MIB));
    let (res, biggest) = biggest_during(|| call(&mut vm, "double", 26));
    assert!(matches!(res, Err(colibri::Error::MemoryLimit { .. })));
    // The last string that fit is at most the limit; the refused one was never made.
    assert!(biggest <= 8 * MIB as usize, "{biggest}");
}
