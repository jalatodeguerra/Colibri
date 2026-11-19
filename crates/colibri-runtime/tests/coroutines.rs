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
