//! `Mutex[T]` (phase 3 L21, ARCHITECTURE §36.7): updates that are never lost, the lock that is let
//! go when a callback fails or the `Vm` stops, and a lock that nothing can hold across a wait.
//!
//! No test waits on a clock. Every program prints what does not depend on timing, and a program
//! that could hang runs under a minute's limit that only fails the test. Each program runs on a
//! pool of one thread and on a pool of three.

use std::io::Write;
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

/// The same program on a pool of one and of three, which must print the same.
fn same_on_one_and_three(source: &str, want: &str) {
    for workers in [1, 3] {
        for _ in 0..3 {
            assert_eq!(run_on(workers, source), want, "{workers} workers");
        }
    }
}

#[test]
fn tasks_that_change_one_mutex_lose_no_update() {
    // Four tasks add to one counter 1,500 times each. The loop is long enough for threads to
    // meet in `with` many times; a lock that let two of them copy the same value would lose some.
    same_on_one_and_three(
        r#"
fn add(m: Mutex[int], times: int) {
    for _ in 0..times {
        m.with(fn(mut n) { n += 1 })
    }
}

fn main() {
    let m = Mutex(0)
    let tasks = [1, 2, 3, 4].map(fn(i) => task.run(fn() { add(m, 1500) }))
    for t in tasks {
        t.join().expect("a task")
    }
    print("total {m.get()}")
}
"#,
        "total 6000\n",
    );
}

#[test]
fn a_task_that_waits_for_the_lock_sees_what_the_holder_stored() {
    // `get` and `set` take the same lock as `with`: they never see half of a `with`, and a `set`
    // is never overwritten by a `with` that began before it. Each `with` here moves 1 from one
    // slot to the other, so the sum seen by any `get` is 100.
    same_on_one_and_three(
        r#"
fn shuffle(m: Mutex[List[int]]) {
    for _ in 0..400 {
        m.with(fn(l) {
            l[0] -= 1
            l[1] += 1
        })
    }
}

fn watch(m: Mutex[List[int]]) -> int {
    let mut bad = 0
    for _ in 0..400 {
        let l = m.get()
        if l[0] + l[1] != 100 {
            bad += 1
        }
    }
    return bad
}

fn main() {
    let m = Mutex([100, 0])
    let a = task.run(fn() { shuffle(m) })
    let w = task.run(fn() => watch(m))
    a.join().expect("a")
    print("bad reads {w.join().expect("w")}, final {m.get()}")
}
"#,
        "bad reads 0, final [-300, 400]\n",
    );
}

#[test]
fn a_waiting_task_is_stopped_with_the_vm_and_so_is_the_one_that_holds_the_lock() {
    // One task holds the lock in a callback that never ends; another waits for it. Dropping the
    // `Vm` stops the holder at a safepoint and ends the wait, so the pool's threads can be
    // joined. (Neither task can be joined by the program: nothing ends them but the drop.)
    let stats = within_a_minute(|| {
        let out = Out::default();
        let mut vm = colibri::Vm::builder()
            .worker_threads(3)
            .stdout_shared(Box::new(out.clone()))
            .build();
        vm.load_source(
            "mutex.colibri",
            r#"
fn hold(ready: Channel[int]) {
    ready.send(1)
    while true {
    }
}

fn main() {
    let m = Mutex(0)
    let ready = Channel[int]()
    let holder = task.run(fn() {
        m.with(fn(n) { hold(ready) })
    })
    // The holder has the lock now, and the waiter will be behind it.
    ready.recv()
    let waiter = task.run(fn() => m.get())
    print("started {holder.done()} {waiter.done()}")
}
"#,
        )
        .unwrap();
        vm.run_main().unwrap();
        vm.run_event_loop().unwrap();
        assert_eq!(out.text(), "started false false\n");
        let stats = vm.worker_stats();
        drop(vm);
        stats
    });
    use std::sync::atomic::Ordering;
    assert_eq!(stats.alive.load(Ordering::SeqCst), 0, "every thread ended");
}

#[test]
fn a_callback_that_runs_out_of_budget_leaves_the_value_and_the_lock_as_they_were() {
    let mut vm = colibri::Vm::builder().instruction_budget(200_000).build();
    vm.load_source(
        "mutex.colibri",
        r#"
let m = Mutex([1, 2, 3])

fn clear_and_spin(l: List[int]) {
    l.clear()
    while true {
    }
}

fn spin() {
    m.with(fn(l) { clear_and_spin(l) })
}

fn read() -> int {
    return m.get().len()
}

fn main() {}
"#,
    )
    .unwrap();
    vm.run_main().unwrap();
    let spin = vm.get_fn::<(), ()>("spin").unwrap();
    let err = spin.call(&mut vm, ()).unwrap_err();
    assert!(
        matches!(err, colibri::Error::BudgetExhausted { .. }),
        "{err:?}"
    );
    // The lock is free and the list is whole.
    let read = vm.get_fn::<(), i64>("read").unwrap();
    assert_eq!(read.call(&mut vm, ()).unwrap(), 3);
}

#[test]
fn a_mutex_is_one_value_in_every_isolate_that_holds_a_handle() {
    same_on_one_and_three(
        r#"
class Log {
    lines: List[str]
}

fn main() {
    let log = Mutex(Log { lines: [] })
    let first = task.run(fn() {
        log.with(fn(l) { l.lines.push("task") })
    })
    first.join().expect("first")
    log.with(fn(l) { l.lines.push("main") })
    // A task started later sees both lines, copied out at its `get`.
    let seen = task.run(fn() => log.get().lines.len()).join().expect("seen")
    print("{log.get().lines} {seen}")
}
"#,
        "[\"task\", \"main\"] 2\n",
    );
}

#[test]
fn a_task_that_would_wait_on_a_channel_under_the_lock_panics_and_the_mutex_goes_on() {
    same_on_one_and_three(
        r#"
fn main() {
    let m = Mutex(1)
    let ch = Channel[int]()
    let t = task.run(fn() { m.with(fn(v) { let _ = ch.recv() }) })
    print("panicked {t.join().is_err()}")
    let next = m.with(fn(mut v) -> int {
        v += 1
        return v
    })
    print("usable {next}")
}
"#,
        "panicked true\nusable 2\n",
    );
}
