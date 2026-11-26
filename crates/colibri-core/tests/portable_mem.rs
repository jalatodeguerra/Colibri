//! The portable copy runs inside an entry, so the entry's memory account pays for it (L15, on top
//! of the accounting of L13 and L14). Values here have no class objects: those need the engine's
//! `release`, which these tests do not have.

use std::alloc::System;

use colibri_core::Core;
use colibri_core::mem::CountingAlloc;
use colibri_core::portable::{StackGuard, Unnamed, from_portable, to_portable};
use colibri_core::value::Value;

#[global_allocator]
static ALLOC: CountingAlloc<System> = CountingAlloc(System);

const ITEMS: u64 = 20_000;

fn core() -> Core {
    Core::new(Box::new(std::io::sink()))
}

/// A list of strings of about twenty bytes each, built outside any entry (so not charged).
fn big_list() -> Value {
    Value::list(
        (0..ITEMS)
            .map(|i| Value::str(&format!("item number {i:08}")))
            .collect(),
    )
}

#[test]
fn a_large_copy_is_charged_to_the_account_of_the_entry_it_runs_in() {
    let mut core = core();
    let list = big_list();
    assert_eq!(core.memory_used(), Some(0));

    let entry = core.begin_entry();
    let portable = to_portable(&list, &Unnamed, StackGuard::here(1 << 20)).unwrap();
    core.end_entry(entry, false);
    let charged = core.memory_used().unwrap();
    // One boxed string per item at least (about twenty bytes, plus the allocator's prefix).
    assert!(charged >= ITEMS * 20, "charged {charged}");
    eprintln!("to_portable of {ITEMS} strings charged {charged} bytes");

    // A copy made outside an entry is nobody's.
    let before = core.memory_used().unwrap();
    let outside = to_portable(&list, &Unnamed, StackGuard::here(1 << 20)).unwrap();
    assert_eq!(core.memory_used().unwrap(), before);
    drop(outside);

    // Rebuilding is charged too: the portable form is freed (credited) as the values are made.
    let entry = core.begin_entry();
    let back = from_portable(portable, &mut core, StackGuard::here(1 << 20)).unwrap();
    core.end_entry(entry, false);
    let rebuilt = core.memory_used().unwrap();
    assert!(rebuilt >= ITEMS * 20, "charged {rebuilt}");
    drop(back);
    assert_eq!(core.memory_used(), Some(0));
}

#[test]
fn a_copy_that_would_pass_the_limit_is_refused_and_leaves_nothing_charged() {
    use colibri_core::portable::Refused;
    let mut core = core();
    let list = big_list();
    core.set_memory_limit(Some(100_000));
    // Many small nodes: the walk asks the limit every few thousand of them.
    let entry = core.begin_entry();
    let err = to_portable(&list, &core, StackGuard::here(1 << 20)).unwrap_err();
    core.end_entry(entry, false);
    assert_eq!(err.refused, Refused::MemoryLimit);
    assert!(err.to_string().contains("memory limit"), "{err}");
    drop(err);
    assert_eq!(core.memory_used(), Some(0), "the partial copy was freed");
    assert!(!core.memory_over(), "refused before the limit was passed");

    // One large string: refused on its length, before it is copied.
    let big = Value::str(&"x".repeat(1 << 20));
    let entry = core.begin_entry();
    let err = to_portable(&big, &core, StackGuard::here(1 << 20)).unwrap_err();
    core.end_entry(entry, false);
    assert_eq!(err.refused, Refused::MemoryLimit);
    drop(err);
    assert_eq!(core.memory_used(), Some(0));

    // The receiver asks too: a portable value made without a limit is refused when it would
    // not fit, and the partial rebuild is freed.
    core.set_memory_limit(None);
    let entry = core.begin_entry();
    let portable = to_portable(&list, &Unnamed, StackGuard::here(1 << 20)).unwrap();
    core.end_entry(entry, false);
    core.set_memory_limit(Some(portable_size_hint(&core)));
    let entry = core.begin_entry();
    let err = from_portable(portable, &mut core, StackGuard::here(1 << 20)).unwrap_err();
    core.end_entry(entry, false);
    assert_eq!(err, colibri_core::portable::RebuildError::MemoryLimit);
    assert_eq!(core.memory_used(), Some(0));
}

/// A limit a little above what the portable form of the big list holds, so the same amount more
/// does not fit.
fn portable_size_hint(core: &Core) -> u64 {
    core.memory_used().unwrap() + 100_000
}

#[test]
fn a_portable_value_dropped_on_another_thread_credits_the_account_that_paid() {
    let mut core = core();
    let list = big_list();
    let entry = core.begin_entry();
    let portable = to_portable(&list, &Unnamed, StackGuard::here(1 << 20)).unwrap();
    core.end_entry(entry, false);
    assert!(core.memory_used().unwrap() > 0);
    std::thread::spawn(move || drop(portable)).join().unwrap();
    assert_eq!(core.memory_used(), Some(0));
}
