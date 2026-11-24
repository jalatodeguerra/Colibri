//! A coroutine that waits in a host function (`Ctx::run_async`) on the bytecode VM: the host
//! function runs on a native stack of its own, which is parked while the coroutine waits
//! (vm-plan §3.2). What happens to the function when the coroutine is cancelled, fails or ends
//! there, seen through the real `Ctx`. Every test runs the VM, whatever `COLIBRI_ENGINE` says.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use colibri::{Ctx, EngineKind, SharedBuffer, Vm};
use colibri_runtime as colibri;

fn vm() -> (Vm, SharedBuffer) {
    let out = SharedBuffer::new();
    let vm = Vm::builder()
        .engine(EngineKind::Bytecode)
        .stdout(Box::new(out.clone()))
        .build();
    (vm, out)
}

/// What a host function left behind when it was unwound: its local was dropped, and Rust was
/// unwinding at the time (the cancellation is a Rust panic that crosses the function's frames).
struct Guard {
    dropped: Arc<AtomicBool>,
    unwinding: Arc<AtomicBool>,
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.unwinding
            .store(std::thread::panicking(), Ordering::SeqCst);
        self.dropped.store(true, Ordering::SeqCst);
    }
}

/// Cancelling a coroutine that is parked in `run_async_cancellable`: the token's hooks run before
/// `cancel` returns (the host function is not even resumed yet when they have), the host function is
/// unwound with a cancel (its destructors run, its code after the wait does not), and the
/// coroutine's own `defer` runs after.
#[test]
fn cancelling_a_coroutine_parked_in_run_async_runs_the_token_hooks_and_unwinds_the_host_function() {
    let (mut vm, out) = vm();
    let hooked = Arc::new(AtomicBool::new(false));
    let started = Arc::new(AtomicBool::new(false));
    let dropped = Arc::new(AtomicBool::new(false));
    let unwinding = Arc::new(AtomicBool::new(false));
    let reached_end = Arc::new(AtomicBool::new(false));
    {
        let (h, s, d, u, r) = (
            hooked.clone(),
            started.clone(),
            dropped.clone(),
            unwinding.clone(),
            reached_end.clone(),
        );
        vm.register_fn("hang", move |cx: &mut Ctx| {
            let _guard = Guard {
                dropped: d.clone(),
                unwinding: u.clone(),
            };
            let (h, s) = (h.clone(), s.clone());
            cx.run_async_cancellable(move |token| {
                let h = h.clone();
                token.on_cancel(move || h.store(true, Ordering::SeqCst));
                s.store(true, Ordering::SeqCst);
                let start = Instant::now();
                while !token.is_cancelled() && start.elapsed() < Duration::from_secs(30) {
                    std::thread::sleep(Duration::from_millis(1));
                }
            });
            r.store(true, Ordering::SeqCst);
        })
        .unwrap();
    }
    let s = started.clone();
    vm.register_fn("started", move || s.load(Ordering::SeqCst))
        .unwrap();
    let (h, d, u) = (hooked.clone(), dropped.clone(), unwinding.clone());
    vm.register_fn("report", move || {
        format!(
            "hook {} dropped {} unwinding {}",
            h.load(Ordering::SeqCst),
            d.load(Ordering::SeqCst),
            u.load(Ordering::SeqCst)
        )
    })
    .unwrap();
    vm.load_source(
        "a.colibri",
        r#"
fn waiter() {
    defer print("defer ran, {report()}")
    hang()
    print("not reached")
}
fn main() {
    let c = go waiter()
    while !started() {
        wait(0.001)
    }
    c.cancel()
    print("cancelled {c.done()}, {report()}")
}
"#,
    )
    .unwrap();
    vm.run_main().unwrap();
    assert_eq!(
        out.contents(),
        "defer ran, hook true dropped true unwinding true\ncancelled true, hook true dropped true unwinding true\n"
    );
    assert!(
        !reached_end.load(Ordering::SeqCst),
        "the host function went on after its cancelled wait"
    );
    assert_eq!(vm.coroutine_count(), 0);
}

/// A host function that waits and then fails raises its panic at the call, in the coroutine that made
/// it: the coroutine's `defer` runs, and the trace names the host function.
#[test]
fn a_host_function_that_fails_after_waiting_is_a_panic_of_its_coroutine() {
    let (mut vm, out) = vm();
    vm.register_fn("wait_then_fail", |cx: &mut Ctx| -> () {
        cx.run_async(|| ());
        cx.raise("failed after the wait")
    })
    .unwrap();
    vm.load_source(
        "a.colibri",
        "fn worker() {\n    defer print(\"defer ran\")\n    wait_then_fail()\n    print(\"not reached\")\n}\nfn main() {\n    let c = go worker()\n    let r = c.join_result()\n    print(\"failed: {r.is_err()}\")\n    let d = go worker()\n    d.join()\n}\n",
    )
    .unwrap();
    let error = vm.run_main().expect_err("the second join raises it");
    let shown = error.render(false);
    assert!(shown.contains("failed after the wait"), "{shown}");
    assert_eq!(out.contents(), "defer ran\nfailed: true\ndefer ran\n");
}

/// `go` of a host function that waits: the host function is the whole coroutine, and its result is
/// what `join` gives.
#[test]
fn a_host_function_that_go_starts_may_wait_and_its_result_is_the_coroutines() {
    let (mut vm, out) = vm();
    vm.register_fn("double", |cx: &mut Ctx, n: i64| -> i64 {
        cx.run_async(move || n * 2)
    })
    .unwrap();
    vm.load_source(
        "a.colibri",
        "fn main() {\n    let a = go double(21)\n    let b = go double(4)\n    print(\"started\")\n    print(\"{a.join()} {b.join()}\")\n}\n",
    )
    .unwrap();
    vm.run_main().unwrap();
    assert_eq!(out.contents(), "started\n42 8\n");
}

/// A host function that keeps `bytes` of native stack alive while it runs.
fn use_stack(bytes: usize) -> usize {
    let mut buffer = vec_on_stack(bytes);
    std::hint::black_box(&mut buffer);
    buffer.len()
}

#[inline(never)]
fn vec_on_stack(bytes: usize) -> Vec<u8> {
    // 64 KiB frames, one inside the other, until `bytes` are in use.
    fn deeper(left: usize, out: &mut Vec<u8>) {
        let mut frame = [0u8; 64 * 1024];
        std::hint::black_box(&mut frame);
        out.push(frame[0]);
        if left > frame.len() {
            deeper(left - frame.len(), out);
        }
        std::hint::black_box(&mut frame);
    }
    let mut out = Vec::new();
    deeper(bytes, &mut out);
    out
}

/// A coroutine's call of a host function that uses half a megabyte of native stack works with
/// the default stack of the VM, and one that uses more works when the builder gives the
/// coroutines bigger stacks (`coroutine_stack_size` is the size of the host function's stack on
/// the VM).
#[test]
fn a_host_function_that_needs_a_lot_of_native_stack_works_in_a_coroutine() {
    let program = "fn main() {\n    let c = go heavy()\n    print(\"{c.join()}\")\n}\nfn heavy() -> int {\n    return deep()\n}\n";
    let (mut vm, out) = vm();
    vm.register_fn("deep", || use_stack(512 * 1024) as i64)
        .unwrap();
    vm.load_source("a.colibri", program).unwrap();
    vm.run_main().unwrap();
    assert_eq!(out.contents(), "8\n");

    let out = SharedBuffer::new();
    let mut vm = Vm::builder()
        .engine(EngineKind::Bytecode)
        .coroutine_stack_size(8 << 20)
        .stdout(Box::new(out.clone()))
        .build();
    vm.register_fn("deep", || use_stack(3 << 20) as i64)
        .unwrap();
    vm.load_source("a.colibri", program).unwrap();
    vm.run_main().unwrap();
    assert_eq!(out.contents(), "48\n");
}
