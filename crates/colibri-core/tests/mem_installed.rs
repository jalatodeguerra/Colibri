//! The counting allocator installed in a program that uses `Core` directly (L13). The runtime's
//! `memory_used` tests cover the `Vm` API; this covers the engine-neutral entry points.

use std::alloc::System;

use colibri_core::Core;
use colibri_core::mem::{CountingAlloc, installed, require_installed};

#[global_allocator]
static ALLOC: CountingAlloc<System> = CountingAlloc(System);

fn core() -> Core {
    Core::new(Box::new(std::io::sink()))
}

#[test]
fn the_flag_says_installed_and_the_check_passes() {
    // Anything allocates before this runs, so the flag is set.
    assert!(installed());
    require_installed("a memory limit");
}

#[test]
fn entries_make_the_account_current_and_restore_it() {
    let mut outer = core();
    let mut inner = core();
    assert_eq!(outer.memory_used(), Some(0));
    let o = outer.begin_entry();
    let first: Vec<u8> = Vec::with_capacity(1000);
    let i = inner.begin_entry();
    let second: Vec<u8> = Vec::with_capacity(5000);
    inner.end_entry(i, false);
    let third: Vec<u8> = Vec::with_capacity(2000);
    outer.end_entry(o, false);
    let after: Vec<u8> = Vec::with_capacity(9000);
    assert_eq!(outer.memory_used(), Some(1016 + 2016));
    assert_eq!(inner.memory_used(), Some(5016));
    drop((first, second, third, after));
    assert_eq!(outer.memory_used(), Some(0));
    assert_eq!(inner.memory_used(), Some(0));
}

#[test]
fn the_limit_sets_a_flag_and_nothing_fails() {
    let mut core = core();
    core.set_memory_limit(Some(10_000));
    assert!(!core.memory_over());
    let outermost = core.begin_entry();
    let big: Vec<u8> = vec![0; 100_000];
    core.end_entry(outermost, false);
    assert!(core.memory_over());
    drop(big);
    core.set_memory_limit(None);
    assert!(!core.memory_over());
}

#[test]
fn a_dead_cores_slot_waits_for_what_it_allocated() {
    let mut a = core();
    let outermost = a.begin_entry();
    let kept: Vec<u8> = Vec::with_capacity(4096);
    a.end_entry(outermost, false);
    let tag = a.mem.tag();
    drop(a);
    // A new account does not take the slot while an allocation made under the old one lives.
    let b = core();
    assert_ne!(b.mem.tag() as u32, tag as u32);
    drop(kept);
    assert_eq!(b.memory_used(), Some(0));
}

#[test]
fn a_panic_out_of_an_entry_gives_the_thread_back() {
    let mut a = core();
    let before = a.memory_used().expect("installed");
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _entry = a.begin_entry();
        let _inside: Vec<u8> = Vec::with_capacity(1000);
        panic!("a panic that nothing inside the entry catches");
    }));
    drop(result.expect_err("the closure panicked"));
    // Nothing allocated while the thread panicked (the payload, std's report) was charged, so the
    // account is where it was before the entry.
    let settled = a.memory_used().expect("installed");
    assert_eq!(settled, before);
    // The host goes on with the thread: its allocations are not the closed entry's.
    let host: Vec<u8> = Vec::with_capacity(5000);
    assert_eq!(
        a.memory_used(),
        Some(settled),
        "the host was charged to the entry's account"
    );
    drop(host);
    assert_eq!(a.memory_used(), Some(settled));
}

#[test]
fn a_block_made_outside_an_entry_is_charged_when_it_grows_inside_one() {
    let mut core = core();
    let mut host: Vec<u8> = Vec::with_capacity(16);
    host.push(1);
    let outermost = core.begin_entry();
    host.resize(100_000, 2);
    core.end_entry(outermost, false);
    let held = core.memory_used().expect("installed");
    assert!(held >= 100_000, "{held}");
    drop(host);
    assert_eq!(core.memory_used(), Some(0));
}

#[test]
fn a_reserve_asks_the_limit_and_a_second_failure_ends_the_grace() {
    let mut core = core();
    // No limit: everything fits, and nothing is checked.
    assert!(core.mem_reserve(usize::MAX));
    core.set_memory_limit(Some(100_000));
    let entry = core.begin_entry();
    assert!(core.mem_reserve(50_000));
    assert!(!core.mem_reserve(150_000));
    assert!(!core.memory_was_exhausted());
    // The first failure begins the grace (the account may now hold 1 MiB more than the limit) ...
    assert_eq!(core.exhaust_memory(), 100_000);
    assert!(core.memory_was_exhausted());
    assert!(!core.cleanups_are_dead());
    assert!(core.mem_reserve(500_000));
    assert!(!core.mem_reserve(2_000_000));
    // ... the second ends it: cleanups are skipped and nothing more is reserved.
    core.exhaust_memory();
    assert!(core.cleanups_are_dead());
    assert!(core.memory_exceeded());
    assert!(!core.mem_reserve(1));
    // The entry's end gives the next entry the host's limit and a fresh grace.
    core.end_entry(entry, false);
    assert!(!core.memory_was_exhausted());
    assert!(!core.cleanups_are_dead());
    assert!(!core.memory_exceeded());
    let entry = core.begin_entry();
    assert!(core.mem_reserve(50_000));
    assert!(!core.mem_reserve(150_000));
    core.end_entry(entry, false);
}

#[test]
fn an_entry_lost_to_a_panic_gives_the_next_one_its_limit_back() {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    let mut core = core();
    core.set_memory_limit(Some(10_000));
    let lost = catch_unwind(AssertUnwindSafe(|| {
        let _entry = core.begin_entry();
        // In the grace, the account may hold a MiB more.
        core.exhaust_memory();
        panic!("a bug in the engine");
    }));
    assert!(lost.is_err());
    let next = core.begin_entry();
    assert!(next.is_outermost());
    assert!(!core.memory_was_exhausted(), "the grace was left behind");
    let big: Vec<u8> = vec![0; 100_000];
    assert!(core.memory_over(), "the limit was not restored");
    core.end_entry(next, false);
    drop(big);
}

#[test]
fn the_flag_is_judged_again_when_an_entry_begins() {
    let mut core = core();
    core.set_memory_limit(Some(10_000));
    let entry = core.begin_entry();
    let big: Vec<u8> = vec![0; 100_000];
    assert!(core.memory_over());
    core.end_entry(entry, false);
    // Freed between entries: the flag raised earlier must not fail the next entry.
    drop(big);
    let entry = core.begin_entry();
    assert!(!core.memory_over());
    core.end_entry(entry, false);
}

#[test]
fn the_slot_tables_own_memory_is_not_charged_to_the_current_vm() {
    // Opening accounts grows the leaked slot table, and closing them grows the queue of closed
    // slots. Both are the table's own, and both outlive the Vm that happened to be running.
    let mut outer = core();
    let o = outer.begin_entry();
    let many: Vec<Core> = (0..300).map(|_| core()).collect();
    drop(many);
    outer.end_entry(o, false);
    assert_eq!(outer.memory_used(), Some(0));
}
