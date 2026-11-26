//! The stack that portable walks run on (phase 3 L17, ARCHITECTURE §36.3): a pooled stack of a
//! known size, so a walk's depth limit does not depend on the thread it happens on, and the
//! receiver of a value has a little more room than its sender.

use colibri_core::Core;
use colibri_core::portable::{StackGuard, Unnamed, from_portable, to_portable};
use colibri_core::value::Value;
use colibri_vm::{
    RECEIVER_EXTRA_BUDGET, SENDER_WALK_BUDGET, WALK_STACK, WalkRole, on_walk_stack,
    walk_stacks_pooled,
};

#[test]
fn the_receiver_has_more_room_than_the_sender_and_both_fit_the_stack() {
    assert_eq!(WalkRole::Sender.budget(), SENDER_WALK_BUDGET);
    assert_eq!(
        WalkRole::Receiver.budget(),
        SENDER_WALK_BUDGET + RECEIVER_EXTRA_BUDGET
    );
    assert!(WalkRole::Receiver.budget() - WalkRole::Sender.budget() >= 64 * 1024);
    // What is left of the stack under the walk is more than the extra, so the receiver's budget
    // is a real limit before the stack runs out.
    assert!(WalkRole::Receiver.budget() + WalkRole::Receiver.budget() / 8 < WALK_STACK);
}

#[test]
fn a_walk_runs_on_a_stack_of_its_own_and_the_stack_is_pooled() {
    // Each test has its own thread, and so its own pool.
    assert_eq!(walk_stacks_pooled(), 0);
    let here = colibri_core::stack_position();
    let there = on_walk_stack(WalkRole::Sender, |_| colibri_core::stack_position());
    // Another mapping, not a few hundred bytes down the same stack.
    assert!(here.abs_diff(there) > 64 * 1024, "{here:#x} {there:#x}");
    assert_eq!(walk_stacks_pooled(), 1);
    // The next walk takes that stack back out of the pool and puts it in again.
    let again = on_walk_stack(WalkRole::Receiver, |_| colibri_core::stack_position());
    assert_eq!(again, there);
    assert_eq!(walk_stacks_pooled(), 1);
}

#[test]
fn a_panic_in_a_walk_comes_out_and_the_stack_stays_usable() {
    let caught = std::panic::catch_unwind(|| {
        on_walk_stack(WalkRole::Sender, |_| panic!("walk failed"));
    });
    assert!(caught.is_err());
    assert_eq!(walk_stacks_pooled(), 1);
    assert_eq!(on_walk_stack(WalkRole::Sender, |_| 5), 5);
}

fn nested(depth: usize) -> Value {
    let mut v = Value::Int(0);
    for _ in 0..depth {
        v = Value::list(vec![v]);
    }
    v
}

#[test]
fn whatever_the_sender_accepts_on_the_walk_stack_the_receiver_takes() {
    // Dropping a list nested this deep recurses, so the test thread has a stack of its own.
    std::thread::Builder::new()
        .stack_size(1 << 30)
        .spawn(accepted_means_received)
        .unwrap()
        .join()
        .unwrap();
}

fn accepted_means_received() {
    let mut core = Core::new(Box::new(std::io::sink()));
    // The deepest list that the sender's walk accepts: grow until it refuses, then bisect.
    let sends = |depth: usize| {
        let v = nested(depth);
        let sent = on_walk_stack(WalkRole::Sender, |g| to_portable(&v, &Unnamed, g));
        drop(v);
        sent.ok()
    };
    let (mut ok, mut bad) = (1_000, 1_000);
    while sends(bad).is_some() {
        ok = bad;
        bad *= 2;
        assert!(bad < 64 << 20, "the walk never refused");
    }
    while bad - ok > 16 {
        let mid = (ok + bad) / 2;
        if sends(mid).is_some() {
            ok = mid;
        } else {
            bad = mid;
        }
    }
    // The two walks start a few hundred bytes apart, so the very last levels are not told apart:
    // two levels short of the edge must be received (a receiver that cost a tenth more per level
    // than the sender would not).
    let depth = ok - 2;
    let v = nested(depth);
    let sent = on_walk_stack(WalkRole::Sender, |g| to_portable(&v, &Unnamed, g))
        .expect("accepted by the sender");
    drop(v);
    let back = on_walk_stack(WalkRole::Receiver, |g| from_portable(sent, &mut core, g))
        .expect("what the sender accepted, the receiver takes");
    drop(back);
    eprintln!("walk stack: {depth} levels sent and received");
    let _ = StackGuard::here(0);
}
