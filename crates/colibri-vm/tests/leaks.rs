//! Memory use returns to where it started after a program ends, whichever way it ends. A
//! counting allocator makes a leaked string visible, which no output check can.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

struct Counting;

thread_local! {
    static LIVE: Cell<isize> = const { Cell::new(0) };
}

// SAFETY: forwards to the system allocator unchanged; only a thread-local counter is added.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = LIVE.try_with(|c| c.set(c.get() + layout.size() as isize));
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let _ = LIVE.try_with(|c| c.set(c.get() - layout.size() as isize));
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

fn run(src: &str) -> colibri_vm::Outcome {
    let options = colibri_vm::Options {
        instruction_budget: Some(20_000_000),
        ..colibri_vm::Options::default()
    };
    colibri_vm::run_source_with(src, &options)
}

fn live() -> isize {
    LIVE.with(Cell::get)
}

/// Bytes still allocated after running `src` (and dropping its outcome), against before.
fn leaked(src: &str) -> isize {
    // A first run settles one-time allocations (interned tables, caches).
    drop(run(src));
    let before = live();
    let outcome = run(src);
    drop(outcome);
    live() - before
}

fn assert_no_leak(what: &str, src: &str) {
    assert_eq!(leaked(src), 0, "{what} leaks");
}

#[test]
fn returns_and_tails_release_their_strings() {
    assert_no_leak(
        "return",
        "fn f(s: str, n: int) -> str {
    let t = s + \"x\"
    if n > 0 { return \"early{t}{n}\" }
    let u = t + \"y\"
    return u + \"z\"
}
fn main() {
    let mut keep = \"k\" + \"eep\"
    keep = f(keep, 1) + f(keep, 0)
    print(keep)
}",
    );
}

#[test]
fn break_and_continue_release_the_strings_of_the_iteration() {
    assert_no_leak(
        "break and continue",
        "fn main() {
    let mut total = 0
    for i in 0..6 {
        let a = \"a\" + \"b\"
        if i == 1 { continue }
        let b = a + \"c\"
        if i == 4 { break }
        total += 1
    }
    let mut n = 0
    while true {
        let s = \"w\" + \"x\"
        n += 1
        if n < 3 { continue }
        break
    }
    print(total + n)
}",
    );
}

#[test]
fn reassigning_a_string_local_releases_the_old_value() {
    assert_no_leak(
        "reassignment",
        "fn main() {
    let mut s = \"a\" + \"b\"
    for i in 0..5 {
        s = s + \"c\"
    }
    s = \"fresh\" + \"er\"
    print(s)
}",
    );
}

#[test]
fn a_panic_in_a_nested_call_releases_everything_the_frames_owned() {
    let src = "fn deep(s: str, z: int) -> int {
    let t = s + \"x\"
    let u = t + \"y\"
    return 10 / z
}
fn mid(s: str, z: int) -> int {
    let m = s + \"m\"
    return deep(m + \"!\", z) + 1
}
fn main() {
    let a = \"a\" + \"b\"
    print(mid(a + \"c\", 0))
}";
    assert_eq!(leaked(src), 0, "a panic leaks");
    assert!(run(src).panic.is_some());
}
