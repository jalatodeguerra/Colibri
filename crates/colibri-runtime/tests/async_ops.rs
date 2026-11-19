//! Waiting on the main fiber and background operations (`Ctx::run_async`), ARCHITECTURE §22.
//! These use the real clock: the operations are real threads that sleep.

use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use colibri_runtime as colibri;

fn vm() -> (colibri::Vm, colibri::SharedBuffer) {
    let out = colibri::SharedBuffer::new();
    let mut vm = colibri::Vm::builder().stdout(Box::new(out.clone())).build();
    // `slow(ms, value)` sleeps on a background thread and returns `value`.
    vm.register_fn("slow", |cx: &mut colibri::Ctx, ms: i64, value: i64| {
        cx.run_async(move || {
            std::thread::sleep(Duration::from_millis(ms as u64));
            value
        })
    })
    .unwrap();
    vm.register_fn("explode", |cx: &mut colibri::Ctx| -> i64 {
        cx.run_async(|| -> i64 { panic!("worker blew up") })
    })
    .unwrap();
    (vm, out)
}

#[test]
fn background_operations_overlap_inside_coroutines() {
    let (mut vm, out) = vm();
    vm.load_source(
        "a.colibri",
        r#"
fn job(i: int) -> int {
    let v = slow(300, i * 10)
    print("job {i} got {v}")
    return v
}
fn main() {
    let group = CoGroup()
    var handles: List[Coroutine[int]] = []
    for i in 0..4 {
        handles.push(group.go(job(i)))
    }
    group.join_all()
    var total = 0
    for h in handles {
        total += h.join()
    }
    print("total {total}")
}
"#,
    )
    .unwrap();
    let start = Instant::now();
    vm.run_main().unwrap();
    let took = start.elapsed();
    assert!(
        took >= Duration::from_millis(280) && took < Duration::from_millis(800),
        "four 300 ms operations should overlap, took {took:?}"
    );
    let text = out.contents();
    assert!(text.ends_with("total 60\n"), "{text}");
    assert_eq!(text.lines().count(), 5, "{text}");
    assert_eq!(vm.coroutine_count(), 0);
}

#[test]
fn a_host_driven_loop_is_not_blocked_by_an_operation() {
    let (mut vm, out) = vm();
    vm.load_source(
        "a.colibri",
        "fn job() {\n let v = slow(150, 5)\n print(\"got {v}\")\n}\nfn main() { go job() }\n",
    )
    .unwrap();
    let start = Instant::now();
    vm.run_main().unwrap();
    // `go` returned at once: the coroutine is suspended on the operation.
    assert!(start.elapsed() < Duration::from_millis(100));
    assert_eq!(vm.coroutine_count(), 1);
    let mut ticks = 0;
    while vm.coroutine_count() > 0 {
        vm.tick(0.001).unwrap();
        ticks += 1;
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(ticks > 5, "the host kept ticking meanwhile ({ticks})");
    assert_eq!(out.contents(), "got 5\n");
}

#[test]
fn the_event_loop_sleeps_until_a_completion() {
    let (mut vm, out) = vm();
    vm.load_source(
        "a.colibri",
        "fn job() {\n let v = slow(100, 9)\n print(\"got {v}\")\n}\nfn main() { go job() }\n",
    )
    .unwrap();
    vm.run_main().unwrap();
    let before = vm.coroutine_resumptions();
    vm.run_event_loop().unwrap();
    assert_eq!(out.contents(), "got 9\n");
    // One resumption to finish: the loop did not poll the coroutine while it waited.
    assert_eq!(vm.coroutine_resumptions() - before, 1);
}

#[test]
fn on_the_main_fiber_the_wait_runs_the_loop() {
    let (mut vm, out) = vm();
    vm.load_source(
        "a.colibri",
        r#"
fn ticker() {
    for i in 0..100 {
        wait(0.02)
        print("tick")
    }
}
fn main() {
    go ticker()
    let v = slow(210, 3)
    print("got {v}")
    wait(0.05)
    print("after wait")
}
"#,
    )
    .unwrap();
    vm.run_main().unwrap();
    let text = out.contents();
    let ticks = text.matches("tick").count();
    assert!(
        (6..=14).contains(&ticks),
        "ticker ran while main waited: {text}"
    );
    assert!(
        text.contains("got 3\n") && text.contains("after wait\n"),
        "{text}"
    );
    vm.cancel_coroutines();
}

#[test]
fn cancelling_a_waiting_coroutine_unwinds_it() {
    let (mut vm, out) = vm();
    vm.load_source(
        "a.colibri",
        r#"
fn job() {
    defer print("cleanup")
    let v = slow(200, 1)
    print("never {v}")
}
fn main() {
    let c = go job()
    wait(0.01)
    c.cancel()
    print("cancelled {c.done()}")
}
"#,
    )
    .unwrap();
    vm.run_main().unwrap();
    assert_eq!(out.contents(), "cleanup\ncancelled true\n");
    assert_eq!(vm.coroutine_count(), 0);
}

#[test]
fn a_panicking_worker_is_a_panic_of_the_caller() {
    let (mut vm, _) = vm();
    vm.load_source("a.colibri", "fn main() { let _ = explode() }\n")
        .unwrap();
    let Err(colibri::Error::Panic { message, .. }) = vm.run_main() else {
        panic!("expected a panic");
    };
    assert!(message.contains("thread panicked"), "{message}");
}

#[test]
fn waiting_on_the_main_fiber_is_refused_inside_a_host_callback() {
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
        "a.colibri",
        "fn pauses() { wait(1.0) }\nfn main() { call_back() }\n",
    )
    .unwrap();
    handle.set(Some(vm.get_fn::<(), ()>("pauses").unwrap()));
    vm.run_main().unwrap();
    assert!(seen.get());
}
