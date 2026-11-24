//! Which coroutine panics are failures of the program once `join_result` is involved: a panic
//! a `join_result` returns is not one, a panic that nobody returns always is, and a budget panic
//! (EMBEDDING §5) always reaches the host.

use colibri_runtime as colibri;

fn vm(budget: Option<u64>) -> (colibri::Vm, colibri::SharedBuffer) {
    let out = colibri::SharedBuffer::new();
    let mut b = colibri::Vm::builder().stdout(Box::new(out.clone()));
    if let Some(n) = budget {
        b = b.instruction_budget(n);
    }
    (b.build(), out)
}

fn panic_message(err: colibri::Error) -> String {
    match err {
        colibri::Error::Panic { message, .. } => message,
        other => panic!("expected a panic, got {other:?}"),
    }
}

/// A woken `join_result` waiter that is cancelled before it runs must not take the panic with
/// it: the host cancels the coroutines between the tick of the panic and the next one.
#[test]
fn a_host_cancel_between_the_panic_and_the_waiter_does_not_lose_the_panic() {
    let (mut vm, out) = vm(None);
    vm.load_source(
        "lost.colibri",
        r#"
let gate = Signal()
fn boom() {
    wait_for(gate)
    panic("lost?")
}
fn waiter(c: Coroutine[()]) {
    defer print("waiter unwound")
    let r = c.join_result()
    print("not reached {r.is_err()}")
}
fn fire() {
    gate.emit()
}
fn main() {
    let b = go boom()
    go waiter(b)
}
"#,
    )
    .unwrap();
    vm.run_main().unwrap();
    vm.get_fn::<(), ()>("fire")
        .unwrap()
        .call(&mut vm, ())
        .unwrap();
    // The panic is held for the waiter, which has not run yet: this tick reports nothing.
    vm.tick(0.0).unwrap();
    vm.cancel_coroutines();
    assert_eq!(out.contents(), "waiter unwound\n");
    assert_eq!(vm.coroutine_count(), 0);
    let message = panic_message(vm.run_event_loop().unwrap_err());
    assert_eq!(message, "lost?");
    // Reported once.
    vm.run_event_loop().unwrap();
}

/// The same, but the waiter is not cancelled: it runs, returns the `err`, and nothing is
/// reported, however the ticks are driven.
#[test]
fn a_waiter_that_runs_takes_the_panic_from_the_queue() {
    let (mut vm, out) = vm(None);
    vm.load_source(
        "taken.colibri",
        r#"
let gate = Signal()
fn boom() {
    wait_for(gate)
    panic("taken")
}
fn waiter(c: Coroutine[()]) {
    print("got {c.join_result().is_err()}")
}
fn fire() {
    gate.emit()
}
fn main() {
    let b = go boom()
    go waiter(b)
}
"#,
    )
    .unwrap();
    vm.run_main().unwrap();
    vm.get_fn::<(), ()>("fire")
        .unwrap()
        .call(&mut vm, ())
        .unwrap();
    vm.tick(0.0).unwrap();
    vm.tick(0.0).unwrap();
    assert_eq!(out.contents(), "got true\n");
    vm.run_event_loop().unwrap();
}

/// A budget panic is the host's: a `join_result` that comes after it, in the same tick, does
/// not hide it.
#[test]
fn a_budget_panic_is_not_hidden_by_a_later_join_result_in_the_tick() {
    let (mut vm, out) = vm(Some(20_000));
    vm.load_source(
        "budget.colibri",
        r#"
fn spin() {
    wait(1.0)
    while true {
    }
}
fn late(c: Coroutine[()]) {
    wait(1.0)
    print("late got {c.join_result().is_err()}")
}
fn main() {
    let c = go spin()
    go late(c)
}
"#,
    )
    .unwrap();
    vm.run_main().unwrap();
    let err = vm.tick(1.0).unwrap_err();
    assert!(
        matches!(err, colibri::Error::BudgetExhausted { .. }),
        "{err:?}"
    );
    assert_eq!(out.contents(), "late got true\n");
}

/// A budget panic that a waiting `join_result` receives is still reported at once.
#[test]
fn a_budget_panic_is_not_held_for_a_woken_join_result_waiter() {
    let (mut vm, _) = vm(Some(20_000));
    vm.load_source(
        "budget.colibri",
        r#"
fn spin() {
    wait(1.0)
    while true {
    }
}
fn waiter(c: Coroutine[()]) {
    let _ = c.join_result()
}
fn main() {
    let c = go spin()
    go waiter(c)
}
"#,
    )
    .unwrap();
    vm.run_main().unwrap();
    let err = vm.tick(1.0).unwrap_err();
    assert!(
        matches!(err, colibri::Error::BudgetExhausted { .. }),
        "{err:?}"
    );
}

/// On the main fiber, too.
#[test]
fn a_budget_panic_is_not_hidden_by_join_result_on_the_main_fiber() {
    let (mut vm, _) = vm(Some(20_000));
    vm.load_source(
        "budget.colibri",
        r#"
fn spin() {
    wait(0.0)
    while true {
    }
}
fn main() {
    let c = go spin()
    let _ = c.join_result()
}
"#,
    )
    .unwrap();
    let first = vm.run_main();
    let result = if first.is_ok() {
        vm.run_event_loop()
    } else {
        first
    };
    let err = result.unwrap_err();
    assert!(
        matches!(err, colibri::Error::BudgetExhausted { .. }),
        "{err:?}"
    );
}

/// A plain `join` of a coroutine that ran out of budget: the host gets the budget error, not
/// an ordinary panic of the joiner (before, the failure of the target was dropped and the
/// joiner's own panic was reported instead).
#[test]
fn join_of_a_budget_panicked_coroutine_reports_the_budget_error() {
    let (mut vm, _) = vm(Some(20_000));
    vm.load_source(
        "budget.colibri",
        r#"
fn spin() {
    wait(1.0)
    while true {
    }
}
fn joiner(c: Coroutine[()]) {
    wait(1.0)
    c.join()
}
fn main() {
    let c = go spin()
    go joiner(c)
}
"#,
    )
    .unwrap();
    vm.run_main().unwrap();
    let err = vm.tick(1.0).unwrap_err();
    assert!(
        matches!(err, colibri::Error::BudgetExhausted { .. }),
        "{err:?}"
    );
}

/// `cancel_all_coroutines` (host shutdown) leaves no join list behind.
#[test]
fn cancelling_everything_leaves_no_waiter_registered() {
    let (mut vm, _) = vm(None);
    vm.load_source(
        "lists.colibri",
        r#"
let never = Signal()
let group = CoGroup()
fn member() {
    wait_for(never)
}
fn joiner(c: Coroutine[()]) {
    c.join()
}
fn result_joiner(c: Coroutine[()]) {
    let _ = c.join_result()
}
fn all() {
    group.join_all()
}
fn main() {
    let m = group.go(member())
    go joiner(m)
    go result_joiner(m)
    go all()
}
"#,
    )
    .unwrap();
    vm.run_main().unwrap();
    assert!(vm.coroutine_waiters() >= 4);
    vm.cancel_coroutines();
    assert_eq!(vm.coroutine_waiters(), 0);
    assert_eq!(vm.coroutine_count(), 0);
}

/// Only join waiters are left: the event loop returns `Ok` with them alive (as for signal
/// waiters), a tick resumes nothing, and the main fiber's wait panics instead of hanging.
#[test]
fn when_only_join_waiters_are_left_nothing_polls_and_nothing_resumes() {
    let (mut vm, _) = vm(None);
    vm.load_source(
        "stuck.colibri",
        r#"
let never = Signal()
fn target() {
    wait_for(never)
}
fn joiner(c: Coroutine[()]) {
    c.join()
}
fn main() {
    let c = go target()
    go joiner(c)
}
"#,
    )
    .unwrap();
    vm.run_main().unwrap();
    let before = vm.coroutine_resumptions();
    vm.run_event_loop().unwrap();
    vm.tick(1.0).unwrap();
    assert_eq!(vm.coroutine_resumptions(), before);
    assert_eq!(vm.coroutine_count(), 2);
    vm.cancel_coroutines();

    let (mut vm, _) = self::vm(None);
    vm.load_source(
        "main_stuck.colibri",
        r#"
let never = Signal()
fn target() {
    wait_for(never)
}
fn main() {
    let c = go target()
    c.join()
}
"#,
    )
    .unwrap();
    let message = panic_message(vm.run_main().unwrap_err());
    assert!(message.contains("would wait forever"), "{message}");
}
