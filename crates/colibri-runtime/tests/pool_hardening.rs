//! The pool under load (phase 3 L21, ARCHITECTURE §36.3): the temporary threads it lends to tasks
//! that wait, the limit on them, a pool that is only busy not growing, and a task that is
//! cancelled while it runs inline.
//!
//! No test waits on a clock. Every program prints what does not depend on timing, and a program
//! that could hang runs under a minute's limit that only fails the test.

use std::io::Write;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use colibri_runtime as colibri;

#[derive(Clone, Default)]
struct Out(Arc<Mutex<Vec<u8>>>);

impl Out {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

impl Write for Out {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Runs `body` on a thread and gives up on it after a minute, so that a deadlock fails a test.
fn within_a_minute<R: Send + 'static>(body: impl FnOnce() -> R + Send + 'static) -> R {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .stack_size(64 * 1024 * 1024)
        .spawn(move || {
            let _ = tx.send(body());
        })
        .unwrap();
    rx.recv_timeout(Duration::from_secs(60))
        .expect("the program deadlocked")
}

/// What `source` prints on a pool of `workers` threads, main and event loop run to the end.
fn run_on(workers: usize, source: &str) -> String {
    let source = source.to_string();
    within_a_minute(move || {
        let out = Out::default();
        let mut vm = colibri::Vm::builder()
            .worker_threads(workers)
            .stdout_shared(Box::new(out.clone()))
            .build();
        vm.load_source("channels.colibri", &source).unwrap();
        vm.run_main().unwrap();
        vm.run_event_loop().unwrap();
        out.text()
    })
}

/// Runs `source` on a pool of `workers` and says what it printed with how many threads the pool
/// started (the pool's own and the temporary ones), after the `Vm` is gone and every thread joined.
fn run_counting(workers: usize, cap: Option<usize>, source: &str) -> (String, usize, usize, usize) {
    let source = source.to_string();
    within_a_minute(move || {
        let out = Out::default();
        let mut builder = colibri::Vm::builder()
            .worker_threads(workers)
            .stdout_shared(Box::new(out.clone()));
        if let Some(cap) = cap {
            builder = builder.worker_extra_cap(cap);
        }
        let mut vm = builder.build();
        vm.load_source("pool.colibri", &source).unwrap();
        vm.run_main().unwrap();
        vm.run_event_loop().unwrap();
        let stats = vm.worker_stats();
        drop(vm);
        (
            out.text(),
            stats.started.load(Ordering::SeqCst),
            stats.joined.load(Ordering::SeqCst),
            stats.alive.load(Ordering::SeqCst),
        )
    })
}

/// The task that waits holds the only thread; the one that feeds it is queued, and is joined first.
const FEEDER_BEHIND: &str = r#"
fn main() {
    let ch = Channel[int]()
    let a = task.run(fn() => ch.recv().expect("fed"))
    let b = task.run(fn() {
        ch.send(5)
        return 1
    })
    print("b {b.join().expect("b")}")
    print("a {a.join().expect("a")}")
}
"#;

#[test]
fn a_task_blocked_on_a_channel_while_its_feeder_is_queued_gets_a_temporary_thread() {
    for workers in [1, 3] {
        let (out, started, joined, alive) = run_counting(workers, None, FEEDER_BEHIND);
        assert_eq!(out, "b 1\na 5\n", "{workers} workers");
        assert!(started >= workers, "{started} threads for {workers} workers");
        // Whatever the pool lent is joined with the pool.
        assert_eq!((joined, alive), (started, 0), "{workers} workers");
    }
    // On one thread the feeder has no thread but a lent one: it is always started.
    let (_, started, _, _) = run_counting(1, None, FEEDER_BEHIND);
    assert_eq!(started, 2);
}

#[test]
fn the_main_thread_waiting_for_a_channel_also_gets_a_thread_for_the_task_that_feeds_it() {
    // The pool's only thread is held by a task that waits for main, and the feeder of main's
    // `recv` is queued behind it.
    let source = r#"
fn main() {
    let gate = Channel[int]()
    let ch = Channel[int]()
    let held = task.run(fn() => gate.recv().expect("opened"))
    let feeder = task.run(fn() { ch.send(11) })
    print("fed {ch.recv().expect("a value")}")
    gate.send(1)
    print("held {held.join().expect("held")}")
    feeder.join().expect("feeder")
}
"#;
    for workers in [1, 3] {
        let (out, _, joined, alive) = run_counting(workers, None, source);
        assert_eq!(out, "fed 11\nheld 1\n", "{workers} workers");
        assert_eq!(alive, 0);
        assert!(joined >= workers);
    }
}

#[test]
fn a_pool_that_is_only_busy_does_not_grow() {
    // Sixty tasks that compute, joined in order by the main thread, which asks the pool for room at
    // every wait: no thread of the pool is waiting, so none is lent.
    let source = r#"
fn spin(n: int) -> int {
    let mut acc = 0
    for i in 0..n {
        acc += i % 7
    }
    return acc
}

fn main() {
    let mut tasks: List[Task[int]] = []
    for i in 0..60 {
        tasks.push(task.run(fn() => spin(30000 + i)))
    }
    let mut total = 0
    for t in tasks {
        total += t.join().expect("a task")
    }
    print("total {total}")
}
"#;
    for workers in [1, 3] {
        let (out, started, joined, alive) = run_counting(workers, None, source);
        assert!(out.starts_with("total "), "{out}");
        assert_eq!(started, workers, "a busy pool lent threads");
        assert_eq!((joined, alive), (started, 0));
    }
}

#[test]
fn more_nested_waits_than_the_limit_on_temporary_threads_panic_and_name_it() {
    // Every level keeps a coroutine alive, so each level's join waits for a thread of its own. The
    // pool of one lends at most 3, so the fifth level cannot be run, and the wait that needs it
    // says why instead of waiting for ever.
    let source = r#"
fn hold(gate: Channel[int]) {
    gate.recv()
}

fn level(n: int) -> Result[int] {
    if n == 0 {
        return ok(0)
    }
    let gate = Channel[int]()
    let held = go hold(gate)
    let inner = task.run(fn() => level(n - 1)).join()
    gate.close()
    held.join()
    match inner {
        ok(r) => return ok(r?.clone() + 1)
        err(e) => return err(e)
    }
}

fn main() {
    let top = task.run(fn() => level(8)).join().expect("the top task")
    match top {
        ok(_) => print("no limit was reached")
        err(e) => print("stopped: {e.message().contains("limit of 3 temporary threads")}")
    }
}
"#;
    let (out, started, joined, alive) = run_counting(1, Some(3), source);
    assert_eq!(out, "stopped: true\n");
    assert_eq!(started, 4, "the pool's thread and the 3 it was allowed to lend");
    assert_eq!((joined, alive), (4, 0));
}

#[test]
fn a_task_that_runs_inline_in_a_join_is_cancelled_when_the_vm_is_dropped() {
    // On a pool of one the joined task is queued, so the joiner runs it on its own thread. The pool
    // stops it at a safepoint, which unwinds as an exhausted budget; the join that ran it reports a
    // cancellation, as it does for a task that has a thread of its own. The joiner gets the
    // outcome before it is stopped itself (a join that finds its task done does not look at the
    // flag first), so it prints what it saw.
    let out = within_a_minute(|| {
        let out = Out::default();
        let mut vm = colibri::Vm::builder()
            .worker_threads(1)
            .stdout_shared(Box::new(out.clone()))
            .build();
        vm.load_source(
            "pool.colibri",
            r#"
fn spin() -> int {
    let mut n = 0
    while true {
        n += 1
    }
    return n
}

fn main() {
    let ready = Channel[int]()
    let outer = task.run(fn() {
        let inner = task.run(fn() -> int {
            ready.send(1)
            return spin()
        })
        match inner.join() {
            ok(_) => print("the inner task finished")
            err(e) => print("inner: {e.message()}")
        }
    })
    ready.recv()
    print("started")
}
"#,
        )
        .unwrap();
        vm.run_main().unwrap();
        drop(vm);
        out.text()
    });
    assert!(out.starts_with("started\n"), "{out}");
    assert!(out.contains("was cancelled"), "{out}");
}
