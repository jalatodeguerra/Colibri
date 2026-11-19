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

// ---- the pool ----

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// A `Vm` with `tracked(ms, value)`: like `slow`, and it counts how many operations ran at
/// once (`peak`) and how many started (`started`).
fn tracked_vm() -> (
    colibri::Vm,
    colibri::SharedBuffer,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
) {
    let out = colibri::SharedBuffer::new();
    let mut vm = colibri::Vm::builder().stdout(Box::new(out.clone())).build();
    let running = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(AtomicUsize::new(0));
    let (r, p, s) = (running.clone(), peak.clone(), started.clone());
    vm.register_fn(
        "tracked",
        move |cx: &mut colibri::Ctx, ms: i64, value: i64| {
            let (r, p, s) = (r.clone(), p.clone(), s.clone());
            cx.run_async(move || {
                s.fetch_add(1, Ordering::SeqCst);
                let now = r.fetch_add(1, Ordering::SeqCst) + 1;
                p.fetch_max(now, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(ms as u64));
                r.fetch_sub(1, Ordering::SeqCst);
                value
            })
        },
    )
    .unwrap();
    (vm, out, peak, started)
}

#[test]
fn thousands_of_operations_share_a_few_threads() {
    let (mut vm, out, peak, started) = tracked_vm();
    vm.load_source(
        "a.colibri",
        r#"
fn job(i: int) -> int {
    return tracked(2, i)
}
fn main() {
    let group = CoGroup()
    var handles: List[Coroutine[int]] = []
    for i in 0..2000 {
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
    vm.run_main().unwrap();
    assert_eq!(
        out.contents(),
        format!("total {}\n", (0..2000).sum::<i64>())
    );
    assert_eq!(started.load(Ordering::SeqCst), 2000);
    let peak = peak.load(Ordering::SeqCst);
    assert!(
        (2..=16).contains(&peak),
        "operations overlap, on a bounded number of threads: peak {peak}"
    );
}

#[test]
fn an_operation_cancelled_while_queued_never_runs() {
    let (mut vm, out, _, started) = tracked_vm();
    vm.load_source(
        "a.colibri",
        r#"
fn job(i: int) {
    defer print("cleanup")
    let v = tracked(300, i)
    print("never {v}")
}
fn main() {
    var handles: List[Coroutine[()]] = []
    // More than any pool has threads: the last ones are still queued.
    for i in 0..64 {
        handles.push(go job(i))
    }
    wait(0.05)
    for h in handles {
        h.cancel()
    }
    wait(0.5)
}
"#,
    )
    .unwrap();
    vm.run_main().unwrap();
    assert_eq!(out.contents().matches("cleanup").count(), 64);
    assert!(!out.contents().contains("never"));
    let started = started.load(Ordering::SeqCst);
    assert!(started <= 16, "only the first ones ran: {started}");
}
