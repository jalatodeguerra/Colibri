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

/// While the main fiber waits for an operation, the loop keeps running the coroutines. The
/// operation here ends when the ticker has ticked `want` times (or after a safety limit), and
/// the ticker only runs if the wait drives the loop: so the test waits for a condition and
/// does not depend on how precisely this machine's timers sleep (a macOS runner sleeps far
/// longer than asked).
#[test]
fn on_the_main_fiber_the_wait_runs_the_loop() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI64, Ordering};
    let (mut vm, out) = vm();
    let ticks = Arc::new(AtomicI64::new(0));
    let counter = Arc::clone(&ticks);
    vm.register_fn("ticked", move |_cx: &mut colibri::Ctx| -> i64 {
        counter.fetch_add(1, Ordering::SeqCst) + 1
    })
    .unwrap();
    // A monotonic clock for the script, in microseconds since the test began.
    let began = Instant::now();
    vm.register_fn("micros", move |_cx: &mut colibri::Ctx| -> i64 {
        i64::try_from(began.elapsed().as_micros()).unwrap_or(i64::MAX)
    })
    .unwrap();
    let watched = Arc::clone(&ticks);
    vm.register_fn("until_ticks", move |cx: &mut colibri::Ctx, want: i64| {
        let watched = Arc::clone(&watched);
        cx.run_async(move || {
            let limit = Instant::now() + Duration::from_secs(20);
            while watched.load(Ordering::SeqCst) < want && Instant::now() < limit {
                std::thread::sleep(Duration::from_millis(1));
            }
            watched.load(Ordering::SeqCst)
        })
    })
    .unwrap();
    vm.load_source(
        "a.colibri",
        r#"
fn ticker() {
    for _ in 0..100000 {
        let _ = ticked()
        wait(0.001)
    }
}
fn main() {
    go ticker()
    let seen = until_ticks(6)
    print("seen {seen >= 6}")
    let v = slow(210, 3)
    print("got {v}")
    let before = ticked()
    let started = micros()
    wait(0.2)
    let waited = micros() - started
    let after = ticked()
    print("kept going {after - before >= 3}")
    // A lower bound only: a slow machine may wait longer, never less.
    print("waited long enough {waited >= 200000}")
}
"#,
    )
    .unwrap();
    vm.run_main().unwrap();
    assert_eq!(
        out.contents(),
        "seen true\ngot 3\nkept going true\nwaited long enough true\n"
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
    assert!(message.contains("worker blew up"), "{message}");
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
fn thousands_of_operations_reuse_threads() {
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
    assert!(peak >= 2, "operations overlap: peak {peak}");
}

#[test]
fn an_operation_cancelled_while_running_is_dropped() {
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
    assert!(started.load(Ordering::SeqCst) <= 64);
}

/// Operations that block until a later operation releases them: with a fixed pool the
/// releasing one would queue behind the blocked ones forever.
#[test]
fn blocked_operations_never_hold_back_the_one_that_unblocks_them() {
    let out = colibri::SharedBuffer::new();
    let mut vm = colibri::Vm::builder().stdout(Box::new(out.clone())).build();
    let gate = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let g = gate.clone();
    vm.register_fn("wait_gate", move |cx: &mut colibri::Ctx| {
        let g = g.clone();
        cx.run_async(move || {
            // Bounded, so a regression fails the test instead of hanging it.
            let start = std::time::Instant::now();
            while !g.load(Ordering::SeqCst) {
                if start.elapsed() > Duration::from_secs(30) {
                    return false;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            true
        })
    })
    .unwrap();
    let g = gate.clone();
    vm.register_fn("open_gate", move |cx: &mut colibri::Ctx| {
        let g = g.clone();
        cx.run_async(move || g.store(true, Ordering::SeqCst))
    })
    .unwrap();
    vm.load_source(
        "a.colibri",
        r#"
fn blocked() -> bool {
    return wait_gate()
}
fn main() {
    let group = CoGroup()
    var handles: List[Coroutine[bool]] = []
    for i in 0..48 {
        handles.push(group.go(blocked()))
    }
    wait(0.1)
    open_gate()
    group.join_all()
    var count = 0
    for h in handles {
        let done = h.join()
        if done {
            count += 1
        }
    }
    print("released {count}")
}
"#,
    )
    .unwrap();
    vm.run_main().unwrap();
    assert_eq!(out.contents(), "released 48\n");
}

/// Cancelling runs the operation's hooks before `cancel()` returns, however far the
/// operation is (here: it has not looked at anything for 30 s).
#[test]
fn a_cancel_hook_has_run_when_cancel_returns() {
    let out = colibri::SharedBuffer::new();
    let mut vm = colibri::Vm::builder().stdout(Box::new(out.clone())).build();
    let hooked = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let h = hooked.clone();
    vm.register_fn("hang", move |cx: &mut colibri::Ctx| {
        let h = h.clone();
        cx.run_async_cancellable(move |token| {
            let h = h.clone();
            token.on_cancel(move || h.store(true, Ordering::SeqCst));
            let start = std::time::Instant::now();
            while !token.is_cancelled() && start.elapsed() < Duration::from_secs(30) {
                std::thread::sleep(Duration::from_millis(1));
            }
        })
    })
    .unwrap();
    let h = hooked.clone();
    vm.register_fn("hooked", move || h.load(Ordering::SeqCst))
        .unwrap();
    vm.load_source(
        "a.colibri",
        r#"
fn waiter() {
    hang()
}
fn main() {
    let c = go waiter()
    wait(0.1)
    c.cancel()
    print("hook ran: {hooked()}")
}
"#,
    )
    .unwrap();
    vm.run_main().unwrap();
    assert_eq!(out.contents(), "hook ran: true\n");
}

/// Ending the `Vm` cancels what still runs.
#[test]
fn dropping_the_vm_cancels_running_operations() {
    let mut vm = colibri::Vm::builder().build();
    let hooked = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (h, s) = (hooked.clone(), started.clone());
    vm.register_fn("hang", move |cx: &mut colibri::Ctx| {
        let (h, s) = (h.clone(), s.clone());
        cx.run_async_cancellable(move |token| {
            let h = h.clone();
            token.on_cancel(move || h.store(true, Ordering::SeqCst));
            s.store(true, Ordering::SeqCst);
            let start = std::time::Instant::now();
            while !token.is_cancelled() && start.elapsed() < Duration::from_secs(30) {
                std::thread::sleep(Duration::from_millis(1));
            }
        })
    })
    .unwrap();
    vm.load_source(
        "a.colibri",
        "fn waiter() { hang() }\nfn main() {\n    go waiter()\n    wait(0.1)\n}\n",
    )
    .unwrap();
    vm.run_main().unwrap();
    assert!(started.load(Ordering::SeqCst));
    assert!(!hooked.load(Ordering::SeqCst));
    drop(vm);
    assert!(hooked.load(Ordering::SeqCst));
}

/// A fake clock does not move while the loop waits for an operation, so what a program does
/// at a fake time does not depend on how long the operation really takes.
#[test]
fn a_fake_clock_does_not_advance_while_an_operation_runs() {
    struct Fake(Rc<Cell<f64>>);
    impl colibri::Clock for Fake {
        fn now(&mut self) -> f64 {
            self.0.get()
        }
        fn sleep(&mut self, s: f64) {
            self.0.set(self.0.get() + s);
        }
    }
    for real_ms in [50, 200, 400] {
        let time = Rc::new(Cell::new(0.0));
        let out = colibri::SharedBuffer::new();
        let mut vm = colibri::Vm::builder()
            .stdout(Box::new(out.clone()))
            .clock(Fake(Rc::clone(&time)))
            .build();
        vm.register_fn("slow", |cx: &mut colibri::Ctx, ms: i64| {
            cx.run_async(move || std::thread::sleep(Duration::from_millis(ms as u64)))
        })
        .unwrap();
        vm.load_source(
            "a.colibri",
            &format!(
                "var ticks = 0\nfn counter() {{\n    while true {{\n        ticks += 1\n        wait(0.01)\n    }}\n}}\nfn main() {{\n    let c = go counter()\n    slow({real_ms})\n    print(\"ticks {{ticks}}\")\n    c.cancel()\n}}\n"
            ),
        )
        .unwrap();
        vm.run_main().unwrap();
        assert_eq!(out.contents(), "ticks 1\n", "after {real_ms} ms");
        assert_eq!(time.get(), 0.0);
    }
}

// ---- operations that another thread completes (`Ctx::try_await_completion`) ----

use std::sync::Mutex;

/// The result of a parked operation: counts how often it is dropped, so a value that is
/// delivered, a value that arrives after the waiter has gone and one that is never made can be
/// told apart.
struct Probe(Arc<AtomicUsize>);

impl Drop for Probe {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// What the host functions of `completion_vm` share with the test.
#[derive(Clone, Default)]
struct Parked {
    /// Completers handed out by `park()` and not yet completed or dropped.
    slots: Arc<Mutex<Vec<colibri::Completer<Probe>>>>,
    /// Hooks of `park()` that have run.
    hooks: Arc<AtomicUsize>,
    /// Drops of the probes made by `release`.
    probe_drops: Arc<AtomicUsize>,
    /// The gate of `gated`: a thread that holds a completer waits for `open()`.
    gate: Arc<(Mutex<bool>, std::sync::Condvar)>,
}

/// A bound for every wait for another thread: a ceiling, never the thing measured.
const CEILING: Duration = Duration::from_secs(30);

fn describe(outcome: Result<i64, String>) -> String {
    match outcome {
        Ok(v) => format!("ok {v}"),
        Err(message) => format!("err {message}"),
    }
}

fn completion_vm() -> (colibri::Vm, colibri::SharedBuffer, Parked) {
    let out = colibri::SharedBuffer::new();
    let mut vm = colibri::Vm::builder().stdout(Box::new(out.clone())).build();
    let shared = Parked::default();
    // `now(v)`: another thread completes at once.
    vm.register_fn("now", |cx: &mut colibri::Ctx, v: i64| -> String {
        describe(cx.try_await_completion(move |completer| {
            std::thread::spawn(move || completer.complete(v));
            Ok(())
        }))
    })
    .unwrap();
    // `gated(v)`: another thread completes once `open()` has been called.
    let gate = Arc::clone(&shared.gate);
    vm.register_fn("gated", move |cx: &mut colibri::Ctx, v: i64| -> String {
        let gate = Arc::clone(&gate);
        describe(cx.try_await_completion(move |completer| {
            std::thread::spawn(move || {
                let (open, changed) = &*gate;
                let guard = open.lock().unwrap();
                let (guard, _) = changed
                    .wait_timeout_while(guard, CEILING, |open| !*open)
                    .unwrap();
                drop(guard);
                completer.complete(v);
            });
            Ok(())
        }))
    })
    .unwrap();
    let gate = Arc::clone(&shared.gate);
    vm.register_fn("open", move || {
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
    })
    .unwrap();
    // `forgotten()`: the thread that was to complete drops the completer instead.
    vm.register_fn("forgotten", |cx: &mut colibri::Ctx| -> String {
        describe(
            cx.try_await_completion(|completer: colibri::Completer<i64>| {
                std::thread::spawn(move || drop(completer));
                Ok(())
            }),
        )
    })
    .unwrap();
    vm.register_fn("refuse", |cx: &mut colibri::Ctx| -> String {
        describe(cx.try_await_completion(|_: colibri::Completer<i64>| Err("no thread".into())))
    })
    .unwrap();
    // `park()`: keeps the completer in `slots` until `release` or `forget_parked`.
    let state = shared.clone();
    vm.register_fn("park", move |cx: &mut colibri::Ctx| -> String {
        let state = state.clone();
        describe(
            cx.try_await_completion(move |completer| {
                let hooks = Arc::clone(&state.hooks);
                completer.token().on_cancel(move || {
                    hooks.fetch_add(1, Ordering::SeqCst);
                });
                state.slots.lock().unwrap().push(completer);
                Ok(())
            })
            .map(|_probe: Probe| 1),
        )
    })
    .unwrap();
    let state = shared.clone();
    vm.register_fn("parked", move || state.slots.lock().unwrap().len() as i64)
        .unwrap();
    let state = shared.clone();
    vm.register_fn("hooks", move || state.hooks.load(Ordering::SeqCst) as i64)
        .unwrap();
    let state = shared.clone();
    vm.register_fn("release", move || -> i64 {
        let slots = std::mem::take(&mut *state.slots.lock().unwrap());
        let n = slots.len();
        for completer in slots {
            completer.complete(Probe(Arc::clone(&state.probe_drops)));
        }
        n as i64
    })
    .unwrap();
    let state = shared.clone();
    vm.register_fn("forget_parked", move || -> i64 {
        let slots = std::mem::take(&mut *state.slots.lock().unwrap());
        slots.len() as i64
    })
    .unwrap();
    (vm, out, shared)
}

/// Runs `main` of `source` and returns what it printed.
fn run_completion(source: &str) -> String {
    let (mut vm, out, _) = completion_vm();
    vm.load_source("a.colibri", source).unwrap();
    vm.run_main().unwrap();
    assert_eq!(vm.coroutine_count(), 0);
    out.contents()
}

#[test]
fn a_completion_from_another_thread_wakes_coroutines_that_overlap() {
    let text = run_completion(
        r#"
fn job(i: int) -> str { return now(i * 10) }
fn main() {
    let group = CoGroup()
    var handles: List[Coroutine[str]] = []
    for i in 0..4 {
        handles.push(group.go(job(i)))
    }
    group.join_all()
    for h in handles {
        print(h.join())
    }
}
"#,
    );
    assert_eq!(text, "ok 0\nok 10\nok 20\nok 30\n");
}

/// On the main fiber the wait runs the event loop: the coroutine that opens the gate only runs
/// if it does, and the thread that completes only does after the gate is open.
#[test]
fn a_completion_on_the_main_fiber_runs_the_loop_meanwhile() {
    let text = run_completion(
        r#"
fn opener() {
    wait(0.001)
    open()
}
fn main() {
    go opener()
    print(gated(7))
}
"#,
    );
    assert_eq!(text, "ok 7\n");
}

/// The thread that completes may finish before the coroutine has suspended; the wait looks at
/// what has completed before it sleeps.
#[test]
fn a_completion_that_arrives_before_the_waiter_suspends_is_not_lost() {
    for _ in 0..200 {
        let text = run_completion("fn main() { print(now(1)) }\n");
        assert_eq!(text, "ok 1\n");
    }
}

#[test]
fn a_completion_in_a_drop_hook_blocks_until_it_is_done() {
    let text = run_completion(
        r#"
class Res { label: str }

impl Drop for Res {
    fn drop(self) { print("drop {self.label} {now(3)}") }
}

fn main() {
    let r = Res { label: "r" }
    print("end of main")
}
"#,
    );
    assert_eq!(text, "end of main\ndrop r ok 3\n");
}

#[test]
fn a_completer_dropped_without_a_result_is_an_error_not_a_hang() {
    // In a coroutine, on the main fiber, and where the wait blocks (a drop hook).
    let text = run_completion(
        r#"
class Res { label: str }

impl Drop for Res {
    fn drop(self) { print("hook {forgotten()}") }
}

fn job() -> str { return forgotten() }
fn main() {
    let c = go job()
    print("main {forgotten()}")
    print("coroutine {c.join()}")
    let r = Res { label: "r" }
}
"#,
    );
    let none = "err the operation ended without a result";
    assert_eq!(
        text,
        format!("main {none}\ncoroutine {none}\nhook {none}\n")
    );
}

#[test]
fn a_start_that_fails_is_an_error_and_nothing_waits() {
    let (mut vm, out, shared) = completion_vm();
    vm.load_source(
        "a.colibri",
        "fn job() -> str { return refuse() }\nfn main() {\n    let c = go job()\n    print(c.join())\n    print(refuse())\n}\n",
    )
    .unwrap();
    vm.run_main().unwrap();
    assert_eq!(out.contents(), "err no thread\nerr no thread\n");
    assert_eq!(vm.coroutine_count(), 0);
    assert!(shared.slots.lock().unwrap().is_empty());
}

/// A parked operation that is cancelled: its hook has run when `cancel()` returns, the
/// coroutine unwound, and a `complete` that comes later changes nothing for anyone and drops
/// its value.
#[test]
fn a_cancelled_completion_has_run_its_hook_and_ignores_a_late_complete() {
    let (mut vm, out, shared) = completion_vm();
    vm.load_source(
        "a.colibri",
        r#"
fn waiter() {
    defer print("cleanup")
    let v = park()
    print("never {v}")
}
fn main() {
    let c = go waiter()
    while parked() == 0 {
        wait(0.001)
    }
    c.cancel()
    print("cancelled {c.done()} hooks {hooks()}")
    print("late {release()}")
    wait(0.01)
}
"#,
    )
    .unwrap();
    vm.run_main().unwrap();
    assert_eq!(out.contents(), "cleanup\ncancelled true hooks 1\nlate 1\n");
    assert_eq!(vm.coroutine_count(), 0);
    // The late value was dropped on the completing thread, not kept.
    assert_eq!(shared.probe_drops.load(Ordering::SeqCst), 1);
}

#[test]
fn a_completer_dropped_by_its_holder_wakes_a_parked_waiter_with_an_error() {
    let text = run_completion(
        r#"
fn waiter() -> str { return park() }
fn main() {
    let c = go waiter()
    while parked() == 0 {
        wait(0.001)
    }
    print("dropped {forget_parked()}")
    print(c.join())
}
"#,
    );
    assert_eq!(
        text,
        "dropped 1\nerr the operation ended without a result\n"
    );
}

#[test]
fn a_completed_parked_waiter_gets_its_value_once() {
    let (mut vm, out, shared) = completion_vm();
    vm.load_source(
        "a.colibri",
        r#"
fn waiter() -> str { return park() }
fn main() {
    let a = go waiter()
    let b = go waiter()
    while parked() < 2 {
        wait(0.001)
    }
    print("released {release()}")
    print(a.join())
    print(b.join())
}
"#,
    )
    .unwrap();
    vm.run_main().unwrap();
    assert_eq!(out.contents(), "released 2\nok 1\nok 1\n");
    assert_eq!(shared.hooks.load(Ordering::SeqCst), 0);
    assert_eq!(shared.probe_drops.load(Ordering::SeqCst), 2);
}

/// Ending the `Vm` with a completer outstanding is prompt, cancels its token (the hook runs),
/// and leaves the completer inert: completing it afterwards drops the value and does nothing
/// else.
#[test]
fn dropping_the_vm_with_a_completer_outstanding_cancels_its_token() {
    let (mut vm, _out, shared) = completion_vm();
    vm.load_source(
        "a.colibri",
        r#"
fn waiter() { let v = park() }
fn main() {
    go waiter()
    while parked() == 0 {
        wait(0.001)
    }
}
"#,
    )
    .unwrap();
    vm.run_main().unwrap();
    assert_eq!(shared.hooks.load(Ordering::SeqCst), 0);
    let began = Instant::now();
    drop(vm);
    assert!(began.elapsed() < CEILING, "dropping the Vm was not prompt");
    assert_eq!(shared.hooks.load(Ordering::SeqCst), 1);
    let slots = std::mem::take(&mut *shared.slots.lock().unwrap());
    assert_eq!(slots.len(), 1);
    for completer in slots {
        assert!(completer.token().is_cancelled());
        completer.complete(Probe(Arc::clone(&shared.probe_drops)));
    }
    assert_eq!(shared.probe_drops.load(Ordering::SeqCst), 1);
}
