//! `task.run` and `join` (phase 3 L18, ARCHITECTURE §36.4): a closure copied to a worker thread,
//! the three ways of waiting for it, and what a failed task gives its joiner.
//!
//! No test waits on a clock. A task that has to be held back blocks on a gate (a channel the test
//! opens), and a wait that could hang ends by a long timeout that only fails the test.

use std::io::Write;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use colibri_runtime as colibri;

/// A writer that threads share, and the test reads.
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

/// A gate that a task blocks on until the program opens it: `gate_wait()` in the task,
/// `gate_open()` in the program. A wait that is not answered in half a minute gives up with -1,
/// so a bug fails the test instead of hanging it.
#[derive(Clone)]
struct Gate {
    tx: mpsc::Sender<()>,
    rx: Arc<Mutex<mpsc::Receiver<()>>>,
}

impl Gate {
    fn new() -> Gate {
        let (tx, rx) = mpsc::channel();
        Gate {
            tx,
            rx: Arc::new(Mutex::new(rx)),
        }
    }

    /// Registers `gate_wait` on a `Vm` (the workers need it) and `gate_open` (only `main` does).
    fn register(&self, vm: &mut colibri::Vm, open: bool) {
        let rx = Arc::clone(&self.rx);
        vm.register_fn("gate_wait", move || -> i64 {
            match rx.lock().unwrap().recv_timeout(Duration::from_secs(30)) {
                Ok(()) => 1,
                Err(_) => -1,
            }
        })
        .unwrap();
        if open {
            let tx = self.tx.clone();
            vm.register_fn("gate_open", move || tx.send(()).is_ok())
                .unwrap();
        }
    }
}

fn thread_name() -> String {
    std::thread::current().name().unwrap_or("").to_string()
}

/// A `Vm` with `workers` threads, a shared output and the gate, with `source` loaded.
fn vm_with(workers: usize, source: &str) -> (colibri::Vm, Out, Gate) {
    let out = Out::default();
    let gate = Gate::new();
    let setup_gate = gate.clone();
    let mut vm = colibri::Vm::builder()
        .worker_threads(workers)
        .stdout_shared(Box::new(out.clone()))
        .worker_setup(move |vm| {
            setup_gate.register(vm, false);
            vm.register_fn("thread_name", thread_name).unwrap();
        })
        .build();
    gate.register(&mut vm, true);
    vm.register_fn("thread_name", thread_name).unwrap();
    vm.load_source("task_run.colibri", source).unwrap();
    (vm, out, gate)
}

fn run(source: &str) -> (colibri::Vm, Out) {
    let (mut vm, out, _) = vm_with(2, source);
    vm.run_main().unwrap();
    vm.run_event_loop().unwrap();
    (vm, out)
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

#[test]
fn a_task_runs_on_a_pool_thread_and_its_value_comes_back() {
    let (vm, out) = run(r#"
fn main() -> Result[()] {
    let here = thread_name()
    let t = task.run(fn() => thread_name())
    let there = t.join()?
    print("main thread is {here}")
    print("the task ran on {there}")
    return ok(())
}
"#);
    let text = out.text();
    let there = text
        .lines()
        .find_map(|l| l.strip_prefix("the task ran on "))
        .expect(&text);
    assert!(there.starts_with("colibri-task-"), "{text}");
    assert!(!text.contains("main thread is colibri-task-"), "{text}");
    assert_eq!(
        vm.worker_stats()
            .tasks
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
}

#[test]
fn a_task_that_panics_gives_an_err_that_names_it_and_keeps_its_trace() {
    let (_vm, out) = run(r#"
fn explode(n: int) -> int {
    let xs = [1, 2, 3]
    return xs[n]
}

fn main() -> Result[()] {
    let t = task.run(fn() => explode(7))
    match t.join() {
        ok(v) => print("value {v}")
        err(e) => {
            print("message: {e.message()}")
            let trace = e.cause().expect("the trace is the cause")
            print("trace: {trace.message()}")
        }
    }
    // The program and the pool go on.
    print("after: {task.run(fn() => 5).join()?}")
    return ok(())
}
"#);
    let text = out.text();
    let mut lines = text.lines();
    assert_eq!(
        lines.next().unwrap(),
        "message: the task `<closure in main>` panicked: index 7 out of bounds for list of length 3"
    );
    let trace = lines.next().unwrap();
    // The innermost frame first, with where it was: the function and `file:line:column`.
    assert!(
        trace.starts_with("trace: explode (task_run.colibri:4:12)"),
        "{text}"
    );
    // The trace is more than one line: the closure that called it is the next frame.
    assert!(
        lines.next().unwrap().starts_with("<closure in main> ("),
        "{text}"
    );
    assert_eq!(lines.next().unwrap(), "after: 5");
}

#[test]
fn done_and_try_join_poll_a_task_that_is_held_back() {
    let (mut vm, out, _gate) = vm_with(
        1,
        r#"
fn main() -> Result[()] {
    let t = task.run(fn() => gate_wait())
    print("before: done={t.done()} try_join={t.try_join().is_some()}")
    gate_open()
    // Polling ends when the worker has finished: no assumption on how long that takes.
    while !t.done() {
        wait(0.001)
    }
    print("after: done={t.done()}")
    match t.try_join() {
        some(r) => print("try_join: {r?}")
        none => print("try_join: not ready")
    }
    return ok(())
}
"#,
    );
    vm.run_main().unwrap();
    vm.run_event_loop().unwrap();
    assert_eq!(
        out.text(),
        "before: done=false try_join=false\nafter: done=true\ntry_join: 1\n"
    );
}

#[test]
fn a_coroutine_that_joins_lets_the_others_run_until_the_task_is_done() {
    // `a` joins a task that is held back; `b` runs meanwhile and opens the gate only after three
    // turns of its own, so `a` can only finish after `b` ran: `join` did not block the thread.
    let (mut vm, out, _gate) = vm_with(
        1,
        r#"
fn a() {
    let t = task.run(fn() => gate_wait())
    print("a: waits")
    print("a: got {t.join().expect("the task")}")
}

fn b() {
    for i in 1..=3 {
        print("b: turn {i}")
        wait(0.0)
    }
    gate_open()
    print("b: opened the gate")
}

fn main() {
    go a()
    go b()
}
"#,
    );
    vm.run_main().unwrap();
    vm.run_event_loop().unwrap();
    let text = out.text();
    let at = |s: &str| text.find(s).unwrap_or_else(|| panic!("{s}: {text}"));
    assert!(at("a: waits") < at("b: turn 1"), "{text}");
    assert!(at("b: turn 3") < at("b: opened the gate"), "{text}");
    assert!(at("b: opened the gate") < at("a: got 1"), "{text}");
}

#[test]
fn the_main_fiber_runs_its_coroutines_while_it_joins() {
    let (mut vm, out, _gate) = vm_with(
        1,
        r#"
fn opener() {
    for i in 1..=2 {
        print("opener: turn {i}")
        wait(0.0)
    }
    gate_open()
}

fn drive() -> int {
    let t = task.run(fn() => gate_wait())
    go opener()
    print("joining")
    return t.join().expect("the task")
}
"#,
    );
    // A call from the host runs on the main fiber, which is not a coroutine: its `join` has to
    // run the event loop itself, and `opener` is what opens the gate.
    let drive = vm.get_fn::<(), i64>("drive").unwrap();
    assert_eq!(drive.call(&mut vm, ()).unwrap(), 1);
    assert_eq!(
        out.text(),
        "opener: turn 1\njoining\nopener: turn 2\n",
        "{}",
        out.text()
    );
}

#[test]
fn nested_tasks_finish_on_a_pool_of_one_thread() {
    // Every level waits for the next one, and there is one worker: the waiting worker has to run
    // the task it joins itself. A deadlock fails the test after a minute.
    let text = within_a_minute(|| {
        let (mut vm, out, _) = vm_with(
            1,
            r#"
fn chain(depth: int) -> int {
    if depth == 0 {
        return 0
    }
    let inner = task.run(fn() => chain(depth - 1))
    return 1 + inner.join().expect("inner")
}

fn main() {
    print("chain = {task.run(fn() => chain(8)).join().expect("chain")}")
}
"#,
        );
        vm.run_main().unwrap();
        vm.run_event_loop().unwrap();
        assert_eq!(
            vm.worker_stats()
                .started
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        out.text()
    });
    assert_eq!(text, "chain = 8\n");
}

#[test]
fn a_dropped_task_is_detached_and_still_runs() {
    let (mut vm, _out, gate) = vm_with(
        1,
        r#"
fn start() {
    let t = task.run(fn() => gate_wait())
    print("handles: {t.done()}")
}

fn main() {
    start()
}
"#,
    );
    vm.run_main().unwrap();
    // The handle died when `start` returned: nothing refers to the task any more, and it goes on.
    assert_eq!(vm.task_handles(), 0);
    gate.tx.send(()).unwrap();
    vm.run_event_loop().unwrap();
}

#[test]
fn a_handle_counts_until_it_is_dropped() {
    let (mut vm, _out, gate) = vm_with(
        1,
        r#"
let mut keep: Task[int]? = none

fn start() {
    keep = some(task.run(fn() => gate_wait()))
}

fn finish() -> int {
    let t = keep.expect("started")
    keep = none
    gate_open()
    return t.join().expect("the task")
}
"#,
    );
    let start = vm.get_fn::<(), ()>("start").unwrap();
    start.call(&mut vm, ()).unwrap();
    assert_eq!(vm.task_handles(), 1);
    let finish = vm.get_fn::<(), i64>("finish").unwrap();
    assert_eq!(finish.call(&mut vm, ()).unwrap(), 1);
    assert_eq!(vm.task_handles(), 0);
    drop(gate);
}

#[test]
fn a_task_that_runs_out_of_budget_is_an_err_to_the_joiner_and_main_goes_on() {
    let out = Out::default();
    let mut vm = colibri::Vm::builder()
        .worker_threads(1)
        .instruction_budget(200_000)
        .stdout_shared(Box::new(out.clone()))
        .build();
    vm.load_source(
        "budget.colibri",
        r#"
fn spin() -> int {
    let mut n = 0
    while true {
        n += 1
    }
    return n
}

fn main() {
    match task.run(spin).join() {
        ok(v) => print("value {v}")
        err(e) => print("err: {e}")
    }
    print("main goes on")
}
"#,
    )
    .unwrap();
    vm.run_main().unwrap();
    assert_eq!(
        out.text(),
        "err: the task `spin` failed: the instruction budget ran out\nmain goes on\n"
    );
}

#[test]
fn a_task_cannot_send_a_task_handle_and_says_where_it_is() {
    // The checker accepts a type parameter (a generic body is checked once for every `T`), so
    // the handle is only found when the closure is copied.
    let (mut vm, _out, _) = vm_with(
        1,
        r#"
fn wrap[T](x: T) -> Task[int] {
    return task.run(fn() -> int {
        let y = x
        return 1
    })
}

fn main() {
    let inner = task.run(fn() => 1)
    let outer = wrap(inner)
    print("not reached")
}
"#,
    );
    let err = vm.run_main().unwrap_err();
    let colibri::Error::Panic { message, .. } = err else {
        panic!("{err:?}")
    };
    assert_eq!(
        message,
        "`closure.x` is a `Task`, which cannot be sent to another thread"
    );
}

#[test]
fn done_sees_a_finished_task_with_nothing_else_running_the_scheduler() {
    // Only `done` is called, again and again, from the host: no `wait`, no join and no event
    // loop. The poll itself must collect what the worker delivered.
    let (mut vm, _out, gate) = vm_with(
        1,
        r#"
let mut pending: Task[int]? = none

fn start() {
    pending = some(task.run(fn() => gate_wait()))
}

fn finished() -> bool {
    return pending.expect("started").done()
}
"#,
    );
    let start = vm.get_fn::<(), ()>("start").unwrap();
    let finished = vm.get_fn::<(), bool>("finished").unwrap();
    start.call(&mut vm, ()).unwrap();
    assert!(!finished.call(&mut vm, ()).unwrap());
    gate.tx.send(()).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while !finished.call(&mut vm, ()).unwrap() {
        assert!(
            std::time::Instant::now() < deadline,
            "done never turned true"
        );
        std::thread::yield_now();
    }
}

/// Runs `source` (its `main`) on a pool of `workers`, with the instruction budget when given, and
/// gives what it printed. The inline path (a task its joiner runs itself) is taken when no other
/// thread picked the task up first: always on a pool of one, now and then on a larger one. The
/// result must be the same either way.
fn run_on(workers: usize, budget: Option<u64>, source: &str) -> String {
    let source = source.to_string();
    within_a_minute(move || {
        let out = Out::default();
        let mut builder = colibri::Vm::builder()
            .worker_threads(workers)
            .stdout_shared(Box::new(out.clone()));
        if let Some(steps) = budget {
            builder = builder.instruction_budget(steps);
        }
        let mut vm = builder.build();
        vm.load_source("inline.colibri", &source).unwrap();
        vm.run_main().unwrap();
        vm.run_event_loop().unwrap();
        out.text()
    })
}

#[test]
fn a_task_run_inline_has_an_instruction_budget_of_its_own() {
    // `spin` uses up its allotment; `outer`, which joins it, has used almost none of its own and
    // goes on, whoever ran `spin`.
    let source = r#"
fn spin() -> int {
    let mut n = 0
    while true {
        n += 1
    }
    return n
}

fn outer() -> int {
    let b = task.run(spin)
    let mut x = 0
    for i in 0..1000 {
        x += i
    }
    match b.join() {
        ok(v) => print("b ok {v}")
        err(e) => print("b err: {e}")
    }
    for i in 0..1000 {
        x += i
    }
    return x
}

fn main() {
    print("outer: {task.run(outer).join().expect("outer")}")
}
"#;
    let want = "b err: the task `spin` failed: the instruction budget ran out\nouter: 999000\n";
    for workers in [1, 3] {
        for _ in 0..3 {
            assert_eq!(
                run_on(workers, Some(2_000_000), source),
                want,
                "{workers} workers"
            );
        }
    }
}

#[test]
fn the_coroutines_a_task_run_inline_leaves_do_not_run_in_its_joiner() {
    let source = r#"
fn ghost() {
    wait(0.01)
    print("ghost ran")
}

fn inner() -> int {
    go ghost()
    return 1
}

fn outer() -> int {
    let r = task.run(inner).join().expect("inner")
    wait(0.05)
    return r
}

fn main() {
    print("outer {task.run(outer).join().expect("outer")}")
}
"#;
    for workers in [1, 3] {
        for _ in 0..3 {
            assert_eq!(
                run_on(workers, None, source),
                "outer 1\n",
                "{workers} workers"
            );
        }
    }
}

#[test]
fn a_coroutine_that_joins_a_task_run_inline_is_not_disturbed_by_what_the_task_leaves() {
    // The joiner is itself a coroutine, on a pool of one thread: the task it runs inline leaves a
    // coroutine, which is cancelled from inside the joiner's resumption.
    let source = r#"
fn ghost() {
    wait(0.01)
    print("ghost ran")
}

fn inner() -> int {
    go ghost()
    return 1
}

fn waiter() {
    let r = task.run(inner).join().expect("inner")
    wait(0.05)
    print("waiter {r}")
}

fn outer() -> int {
    go waiter()
    wait(0.2)
    return 2
}

fn main() {
    print("outer {task.run(outer).join().expect("outer")}")
}
"#;
    for workers in [1, 3] {
        assert_eq!(
            run_on(workers, None, source),
            "waiter 1\nouter 2\n",
            "{workers} workers"
        );
    }
}

#[test]
fn a_task_run_inline_in_a_coroutine_can_join_a_coroutine_of_its_own() {
    // `waiter` is a coroutine and joins `b`, which runs inline on a pool of one thread. `b` starts
    // a coroutine and joins it: it is the main fiber of its own scope, as on a thread of its own,
    // though the scheduler is busy with the tick that runs `waiter`.
    let source = r#"
fn work() -> int {
    wait(0.01)
    return 5
}

fn b() -> int {
    let c = go work()
    return c.join()
}

class Holder {
    v: int
}

fn waiter(h: Holder) {
    let t = task.run(b)
    let mut k = 0
    for i in 0..300000 {
        k += 1
    }
    h.v = t.join().expect("b")
}

fn outer() -> int {
    let h = Holder { v: 0 }
    let w = go waiter(h)
    w.join()
    return h.v
}

fn main() {
    print("outer {task.run(outer).join()}")
}
"#;
    for workers in [1, 3] {
        for _ in 0..3 {
            assert_eq!(
                run_on(workers, None, source),
                "outer ok(5)\n",
                "{workers} workers"
            );
        }
    }
}

#[test]
fn a_task_run_inline_inside_a_tick_leaves_the_entries_of_that_tick_alone() {
    // `w` and `z` are resumed by a tick; `w` joins `b`, which runs inline on a pool of one thread
    // and ticks on its own. The finished `quick` is in the list while that happens: compacting it
    // away under the outer tick's loop would run it off the end of the list.
    let source = r#"
fn work() -> int {
    wait(0.01)
    return 5
}

fn b() -> int {
    let c = go work()
    return c.join()
}

class Holder {
    v: int
}

fn quick() -> int {
    return 1
}

fn w(h: Holder) {
    wait(0.0)
    let t = task.run(b)
    let mut k = 0
    for i in 0..300000 {
        k += 1
    }
    h.v = h.v + t.join().expect("b")
}

fn z(h: Holder) {
    wait(0.0)
    h.v = h.v + 100
}

fn outer() -> int {
    let h = Holder { v: 0 }
    let a = go quick()
    let ww = go w(h)
    let zz = go z(h)
    ww.join()
    zz.join()
    return h.v + a.join()
}

fn main() {
    print("outer {task.run(outer).join()}")
}
"#;
    for workers in [1, 3] {
        for _ in 0..3 {
            assert_eq!(
                run_on(workers, None, source),
                "outer ok(106)\n",
                "{workers} workers"
            );
        }
    }
}
