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
