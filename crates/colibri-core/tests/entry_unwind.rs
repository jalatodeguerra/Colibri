//! A Rust panic that unwinds past an entry (only a bug in the VM can do that: host functions are
//! caught) leaves the engine in a state the next entry must put right (phase 3 L14): the depth of
//! entries, the budget and the memory limit all start again.

use std::panic::{AssertUnwindSafe, catch_unwind};

use colibri_core::Core;

fn core() -> Core {
    Core::new(Box::new(std::io::sink()))
}

#[test]
fn an_outermost_entry_lost_to_a_panic_leaves_the_next_one_outermost_with_a_fresh_budget() {
    let mut core = core();
    core.set_instruction_budget(Some(1000));
    let lost = catch_unwind(AssertUnwindSafe(|| {
        let _entry = core.begin_entry();
        core.budget.left -= 600;
        panic!("a bug in the engine");
    }));
    assert!(lost.is_err());
    let next = core.begin_entry();
    assert!(next.is_outermost(), "the depth of entries was left behind");
    assert_eq!(core.budget.left, 1000, "the allotment was not renewed");
    core.end_entry(next, true);
    // Between entries nothing is counted and the entry that ended is the last one measured.
    assert_eq!(core.steps_used(), 0);
}

#[test]
fn a_nested_entry_lost_to_a_panic_does_not_keep_the_outer_one_from_ending() {
    let mut core = core();
    core.set_instruction_budget(Some(1000));
    let outer = core.begin_entry();
    assert!(outer.is_outermost());
    // A host function that catches the panic of an entry it started leaves one behind.
    let lost = catch_unwind(AssertUnwindSafe(|| {
        let _inner = core.begin_entry();
        panic!("a bug in the engine");
    }));
    assert!(lost.is_err());
    let again = core.begin_entry();
    assert!(!again.is_outermost(), "the outer entry is still running");
    core.end_entry(again, false);
    core.end_entry(outer, false);
    let next = core.begin_entry();
    assert!(next.is_outermost(), "the lost entry stayed in the depth");
    core.end_entry(next, false);
}

#[test]
fn a_lost_entry_is_settled_when_the_next_one_ends_too() {
    let mut core = core();
    let outer = core.begin_entry();
    let lost = catch_unwind(AssertUnwindSafe(|| {
        let _inner = core.begin_entry();
        panic!("a bug in the engine");
    }));
    assert!(lost.is_err());
    // No entry begins in between: the end of the outer one counts the lost one out.
    core.end_entry(outer, false);
    let next = core.begin_entry();
    assert!(next.is_outermost());
    core.end_entry(next, false);
}
