//! Coroutines from the host's side: `Vm::tick`, the event loop, host callbacks, stack sizes
//! (ARCHITECTURE §16).

use std::cell::Cell;
use std::rc::Rc;

use colibri_runtime as colibri;

fn vm() -> (colibri::Vm, colibri::SharedBuffer) {
    let out = colibri::SharedBuffer::new();
    let vm = colibri::Vm::builder().stdout(Box::new(out.clone())).build();
    (vm, out)
}

#[test]
fn a_host_loop_drives_coroutines_with_tick() {
    let (mut vm, out) = vm();
    vm.load_source(
        "game.colibri",
        r#"
fn door(name: str) {
    print("{name} opens")
    wait(0.5)
    print("{name} closes")
}
fn main() {
    go door("front")
    go door("back")
}
"#,
    )
    .unwrap();
    vm.run_main().unwrap();
    assert_eq!(out.contents(), "front opens\nback opens\n");
    assert_eq!(vm.coroutine_count(), 2);
    vm.tick(0.25).unwrap();
    assert_eq!(vm.coroutine_count(), 2);
    vm.tick(0.25).unwrap();
    // Both were due; they ran in the order they started.
    assert_eq!(
        out.contents(),
        "front opens\nback opens\nfront closes\nback closes\n"
    );
    assert_eq!(vm.coroutine_count(), 0);
}

#[test]
fn tick_zero_pauses_time() {
    let (mut vm, out) = vm();
    vm.load_source(
        "game.colibri",
        "fn w() {\n wait(1.0)\n print(\"woke\")\n}\nfn main() { go w() }\n",
    )
    .unwrap();
    vm.run_main().unwrap();
    for _ in 0..5 {
        vm.tick(0.0).unwrap();
    }
    assert_eq!(out.contents(), "");
    vm.tick(1.0).unwrap();
    assert_eq!(out.contents(), "woke\n");
}

#[test]
fn a_panic_in_a_coroutine_is_an_err_from_tick_and_the_vm_survives() {
    let (mut vm, out) = vm();
    vm.load_source(
        "game.colibri",
        r#"
fn bad() {
    wait(1.0)
    panic("bad coroutine")
}
fn good() {
    wait(1.0)
    print("good ran")
    wait(1.0)
    print("good again")
}
fn main() {
    go bad()
    go good()
}
"#,
    )
    .unwrap();
    vm.run_main().unwrap();
    let err = vm.tick(1.0).unwrap_err();
    let colibri::Error::Panic { message, stack, .. } = err else {
        panic!("expected a panic");
    };
    assert_eq!(message, "bad coroutine");
    assert_eq!(stack[0].function, "bad");
    // The other coroutine still ran in that tick, and the Vm keeps ticking.
    assert_eq!(out.contents(), "good ran\n");
    assert_eq!(vm.coroutine_count(), 1);
    vm.tick(1.0).unwrap();
    assert_eq!(out.contents(), "good ran\ngood again\n");
}

#[test]
fn wait_in_a_host_callback_reports_the_reason() {
    let (mut vm, _) = vm();
    let handle: Rc<Cell<Option<colibri::Fn<(), ()>>>> = Rc::new(Cell::new(None));
    let seen = Rc::new(Cell::new(false));
    let (h, s) = (Rc::clone(&handle), Rc::clone(&seen));
    vm.register_fn("call_back", move |cx: &mut colibri::Ctx| {
        if let Err(colibri::Error::Panic { message, .. }) = h.get().unwrap().call(cx, ()) {
            assert!(message.contains("host function's callback"), "{message}");
            s.set(true);
        }
    })
    .unwrap();
    vm.load_source(
        "game.colibri",
        "fn pauses() { wait(1.0) }\nfn co() { call_back() }\nfn main() { go co() }\n",
    )
    .unwrap();
    handle.set(Some(vm.get_fn::<(), ()>("pauses").unwrap()));
    vm.run_main().unwrap();
    assert!(seen.get());
}

#[test]
fn dropping_the_vm_cancels_suspended_coroutines() {
    let out = colibri::SharedBuffer::new();
    {
        let mut vm = colibri::Vm::builder().stdout(Box::new(out.clone())).build();
        vm.load_source(
            "game.colibri",
            "fn w(n: int) {\n defer print(\"cleanup {n}\")\n wait(100.0)\n}\nfn main() {\n go w(1)\n go w(2)\n}\n",
        )
        .unwrap();
        vm.run_main().unwrap();
        assert_eq!(out.contents(), "");
    }
    // Newest first.
    assert_eq!(out.contents(), "cleanup 2\ncleanup 1\n");
}

#[test]
fn the_event_loop_uses_the_injected_clock() {
    struct Fake(Rc<Cell<f64>>);
    impl colibri::Clock for Fake {
        fn now(&mut self) -> f64 {
            self.0.get()
        }
        fn sleep(&mut self, s: f64) {
            self.0.set(self.0.get() + s);
        }
    }
    let time = Rc::new(Cell::new(0.0));
    let out = colibri::SharedBuffer::new();
    let mut vm = colibri::Vm::builder()
        .stdout(Box::new(out.clone()))
        .clock(Fake(Rc::clone(&time)))
        .build();
    vm.load_source(
        "game.colibri",
        "fn w() {\n wait(3600.0)\n print(\"an hour\")\n}\nfn main() { go w() }\n",
    )
    .unwrap();
    vm.run_main().unwrap();
    vm.run_event_loop().unwrap();
    assert_eq!(out.contents(), "an hour\n");
    assert!((time.get() - 3600.0).abs() < 1e-6);
}

#[test]
fn the_instruction_budget_applies_inside_a_coroutine() {
    let (mut vm, _) = vm();
    vm.set_instruction_budget(Some(10_000));
    vm.load_source(
        "game.colibri",
        "fn spin() {\n wait(1.0)\n while true { }\n}\nfn main() { go spin() }\n",
    )
    .unwrap();
    vm.run_main().unwrap();
    let err = vm.tick(1.0).unwrap_err();
    assert!(
        matches!(err, colibri::Error::BudgetExhausted { .. }),
        "{err:?}"
    );
    assert_eq!(vm.coroutine_count(), 0);
}

#[test]
fn a_configured_stack_size_bounds_recursion_in_a_coroutine() {
    let out = colibri::SharedBuffer::new();
    let mut vm = colibri::Vm::builder()
        .stdout(Box::new(out.clone()))
        .coroutine_stack_size(64 * 1024)
        .build();
    vm.load_source(
        "game.colibri",
        "fn d(n: int) -> int {\n if n == 0 { return 0 }\n return 1 + d(n - 1)\n}\nfn main() { go (fn() { print(d(100000)) })() }\n",
    )
    .unwrap();
    vm.run_main().unwrap();
    let err = vm.tick(0.0).unwrap_err();
    let colibri::Error::Panic { message, .. } = err else {
        panic!("expected a panic");
    };
    assert!(message.contains("stack overflow"), "{message}");
}

#[test]
fn a_thousand_idle_coroutines_tick_quickly() {
    let (mut vm, _) = vm();
    vm.load_source(
        "game.colibri",
        "fn idle() {\n wait(1000000.0)\n}\nfn main() {\n for i in 0..1000 { go idle() }\n}\n",
    )
    .unwrap();
    vm.run_main().unwrap();
    assert_eq!(vm.coroutine_count(), 1000);
    // Warm up, then measure the best of several ticks (DESIGN §17.4 asks for under 0.1 ms in
    // a release build; a debug build is only checked for being far from pathological).
    vm.tick(0.001).unwrap();
    let mut best = std::time::Duration::MAX;
    for _ in 0..20 {
        let start = std::time::Instant::now();
        vm.tick(0.001).unwrap();
        best = best.min(start.elapsed());
    }
    eprintln!("tick with 1000 idle coroutines: {best:?}");
    let limit = if cfg!(debug_assertions) { 20 } else { 1 };
    assert!(best.as_millis() < limit, "{best:?}");
}

#[test]
fn signal_waiters_cost_nothing_per_tick() {
    let (mut vm, out) = vm();
    vm.load_source(
        "waiters.colibri",
        r#"
let ready = Signal()
var woken = 0

fn listener() {
    wait_for(ready)
    woken += 1
}

fn fire() {
    ready.emit()
}

fn main() {
    for i in 0..1000 { go listener() }
}
"#,
    )
    .unwrap();
    vm.run_main().unwrap();
    assert_eq!(vm.coroutine_count(), 1000);
    let after_start = vm.coroutine_resumptions();
    for _ in 0..100 {
        vm.tick(0.01).unwrap();
    }
    // Not one of the 1,000 waiters was resumed by 100 ticks: they are not polled.
    assert_eq!(vm.coroutine_resumptions(), after_start);
    // The time of an idle tick does not depend on them being resumed.
    let mut best = std::time::Duration::MAX;
    for _ in 0..20 {
        let start = std::time::Instant::now();
        vm.tick(0.01).unwrap();
        best = best.min(start.elapsed());
    }
    eprintln!("tick with 1000 signal waiters: {best:?}");
    let limit = if cfg!(debug_assertions) { 20 } else { 1 };
    assert!(best.as_millis() < limit, "{best:?}");
    // One emit wakes all of them, at the next tick and not before.
    let fire = vm.get_fn::<(), ()>("fire").unwrap();
    fire.call(&mut vm, ()).unwrap();
    assert_eq!(vm.coroutine_resumptions(), after_start);
    assert_eq!(vm.coroutine_count(), 1000);
    vm.tick(0.0).unwrap();
    assert_eq!(vm.coroutine_resumptions(), after_start + 1000);
    assert_eq!(vm.coroutine_count(), 0);
    let _ = out;
}

#[test]
fn the_event_loop_ends_when_only_signal_waiters_remain() {
    let (mut vm, out) = vm();
    vm.load_source(
        "waiters.colibri",
        r#"
fn listener(s: Signal) {
    defer print("cancelled")
    wait_for(s)
    print("never")
}
fn main() {
    go listener(Signal())
}
"#,
    )
    .unwrap();
    vm.run_main().unwrap();
    // Nothing can emit that signal: the loop returns instead of sleeping forever.
    vm.run_event_loop().unwrap();
    assert_eq!(vm.coroutine_count(), 1);
    vm.cancel_coroutines();
    assert_eq!(out.contents(), "cancelled\n");
}

/// A fake clock for event loops that must not poll: sleeping moves time forward, and a loop
/// that sleeps more than `CEILING` times is stopped by a panic (a loop that polls a join every
/// millisecond would otherwise run for as long as its target lives, which is never).
struct CountingClock {
    time: Rc<Cell<f64>>,
    sleeps: Rc<Cell<u32>>,
}

const CEILING: u32 = 2_000;

impl colibri::Clock for CountingClock {
    fn now(&mut self) -> f64 {
        self.time.get()
    }
    fn sleep(&mut self, s: f64) {
        self.sleeps.set(self.sleeps.get() + 1);
        assert!(
            self.sleeps.get() < CEILING,
            "the event loop keeps sleeping: something polls"
        );
        self.time.set(self.time.get() + s);
    }
}

fn counting_vm() -> (colibri::Vm, colibri::SharedBuffer, Rc<Cell<u32>>) {
    let out = colibri::SharedBuffer::new();
    let sleeps = Rc::new(Cell::new(0));
    let vm = colibri::Vm::builder()
        .stdout(Box::new(out.clone()))
        .clock(CountingClock {
            time: Rc::new(Cell::new(0.0)),
            sleeps: Rc::clone(&sleeps),
        })
        .build();
    (vm, out, sleeps)
}

/// The panic of a coroutine that a `join_result` receives is a value: the event loop returns
/// `Ok`, with the panic nowhere left to report.
#[test]
fn a_panic_handed_to_join_result_is_not_a_failure_of_the_event_loop() {
    let (mut vm, out, _) = counting_vm();
    vm.load_source(
        "contained.colibri",
        r#"
fn bad() {
    wait(1.0)
    panic("contained")
}
fn handler() {
    let c = go bad()
    match c.join_result() {
        ok(_) => print("ok?"),
        err(e) => print("500: {e.message()}"),
    }
}
fn main() {
    go handler()
}
"#,
    )
    .unwrap();
    vm.run_main().unwrap();
    vm.run_event_loop().unwrap();
    assert_eq!(
        out.contents(),
        "500: the coroutine `bad` panicked: contained\n"
    );
    assert_eq!(vm.coroutine_count(), 0);
    // Nothing is left to report after the program either.
    vm.run_event_loop().unwrap();
}

/// A panic that nobody receives still fails the program: a `join_result` that comes after the
/// tick of the panic is too late, and so is a coroutine nobody joins.
#[test]
fn a_panic_nobody_receives_still_fails_the_event_loop() {
    for src in [
        // Nobody joins.
        "fn bad() {\n wait(1.0)\n panic(\"alone\")\n}\nfn main() { go bad() }\n",
        // The `join_result` comes in a later tick than the panic.
        "fn bad() {\n wait(1.0)\n panic(\"alone\")\n}\nfn late(c: Coroutine[()]) {\n wait(2.0)\n let _ = c.join_result()\n}\nfn main() {\n let c = go bad()\n go late(c)\n}\n",
        // A waiting `join` does not receive it either: it panics in turn.
        "fn bad() {\n wait(1.0)\n panic(\"alone\")\n}\nfn j(c: Coroutine[()]) {\n c.join()\n}\nfn main() {\n let c = go bad()\n go j(c)\n}\n",
    ] {
        let (mut vm, _, _) = counting_vm();
        vm.load_source("uncontained.colibri", src).unwrap();
        vm.run_main().unwrap();
        let err = vm.run_event_loop().unwrap_err();
        let colibri::Error::Panic { message, .. } = err else {
            panic!("expected a panic for {src}");
        };
        assert!(message.contains("alone"), "{message}");
    }
}

/// A `join_result` that waits for a coroutine that never ends does not keep the event loop
/// polling: with nothing left that a clock or a completion could wake, the loop returns.
#[test]
fn join_result_waiters_do_not_keep_the_event_loop_polling() {
    let (mut vm, out, sleeps) = counting_vm();
    vm.load_source(
        "stuck.colibri",
        r#"
let never = Signal()
fn target() {
    defer print("target cancelled")
    wait_for(never)
}
fn waiter(c: Coroutine[()]) {
    defer print("waiter cancelled")
    let _ = c.join_result()
    print("not reached")
}
fn main() {
    let c = go target()
    go waiter(c)
}
"#,
    )
    .unwrap();
    vm.run_main().unwrap();
    vm.run_event_loop().unwrap();
    assert_eq!(sleeps.get(), 0);
    assert_eq!(vm.coroutine_count(), 2);
    vm.cancel_coroutines();
    // Newest first: the waiter leaves the target's list, then the target ends.
    assert_eq!(out.contents(), "waiter cancelled\ntarget cancelled\n");
}
