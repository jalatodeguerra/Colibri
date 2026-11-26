//! Isolates and the worker pool (phase 3 L17, ARCHITECTURE §36.3): what a thread task's worker
//! `Vm` is built from, what it may call, which module state it sees, and how the pool starts,
//! reuses and ends its threads. `task.run` itself is the next step; these tests submit "call this
//! function with portable arguments" through `Vm::submit_portable`.
//!
//! No test waits on a clock: results come back through channels and joins.

use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use colibri::tasks::{Portable, TaskDone, TaskFailure};
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

fn run(
    vm: &mut colibri::Vm,
    function: &str,
    args: Vec<Portable>,
) -> Result<TaskDone, TaskFailure> {
    vm.submit_portable(function, args)
        .expect("the task is queued")
        .wait()
}

fn int(done: Result<TaskDone, TaskFailure>) -> i64 {
    match done.expect("the task ran").value {
        Portable::Int(n) => n,
        other => panic!("not an int: {other:?}"),
    }
}

fn panic_message(done: Result<TaskDone, TaskFailure>) -> String {
    match done {
        Err(TaskFailure::Panic { message, .. }) => message,
        other => panic!("expected a panic, got {other:?}"),
    }
}

fn internal(done: Result<TaskDone, TaskFailure>) -> String {
    match done {
        Err(TaskFailure::Internal(message)) => message,
        other => panic!("expected an internal failure, got {other:?}"),
    }
}

// ---- what a worker may call ----

const CALLS: &str = r#"
fn use_shared() -> int {
    return shared()
}

fn use_secret() -> int {
    return secret()
}
"#;

#[test]
fn a_host_function_the_worker_did_not_register_is_a_trap() {
    let register = |vm: &mut colibri::Vm, only_on_main: bool| {
        vm.register_fn("shared", || 7_i64).unwrap();
        if only_on_main {
            vm.register_fn("secret", || 99_i64).unwrap();
        }
    };
    let mut vm = colibri::Vm::builder()
        .worker_threads(1)
        .worker_setup(move |vm| register(vm, false))
        .build();
    register(&mut vm, true);
    vm.load_source("calls.colibri", CALLS).unwrap();
    // The main isolate has both; the worker has what its setup registered.
    let secret = vm.get_fn::<(), i64>("use_secret").unwrap();
    assert_eq!(secret.call(&mut vm, ()).unwrap(), 99);
    assert_eq!(int(run(&mut vm, "use_shared", vec![])), 7);
    assert_eq!(
        panic_message(run(&mut vm, "use_secret", vec![])),
        "`secret` is not available in a thread task (the host did not register it for workers)"
    );
    // The trap is a panic of the task: the worker goes on.
    assert_eq!(int(run(&mut vm, "use_shared", vec![])), 7);
}

#[test]
fn a_vm_without_worker_setup_traps_every_host_function() {
    let mut vm = colibri::Vm::builder().worker_threads(1).build();
    vm.register_fn("shared", || 7_i64).unwrap();
    vm.register_fn("secret", || 99_i64).unwrap();
    vm.load_source("calls.colibri", CALLS).unwrap();
    assert_eq!(
        panic_message(run(&mut vm, "use_shared", vec![])),
        "`shared` is not available in a thread task (the host did not register it for workers)"
    );
}

#[test]
fn a_worker_may_not_register_more_than_the_main_vm_declares() {
    // `extra` is only in the worker's interface.
    let mut vm = colibri::Vm::builder()
        .worker_threads(1)
        .worker_setup(|vm| {
            vm.register_fn("shared", || 7_i64).unwrap();
            vm.register_fn("extra", || 1_i64).unwrap();
        })
        .build();
    vm.register_fn("shared", || 7_i64).unwrap();
    vm.load_source("calls.colibri", "fn use_shared() -> int {\n    return shared()\n}\n")
        .unwrap();
    let why = internal(run(&mut vm, "use_shared", vec![]));
    assert!(
        why.contains("worker_setup registers") && why.contains("fn extra"),
        "{why}"
    );
}

#[test]
fn a_worker_may_not_declare_a_function_differently() {
    let mut vm = colibri::Vm::builder()
        .worker_threads(1)
        .worker_setup(|vm| {
            vm.register_fn("shared", |x: String| x.len() as i64).unwrap();
        })
        .build();
    vm.register_fn("shared", || 7_i64).unwrap();
    vm.load_source("calls.colibri", "fn use_shared() -> int {\n    return shared()\n}\n")
        .unwrap();
    let why = internal(run(&mut vm, "use_shared", vec![]));
    assert!(why.contains("worker_setup registers"), "{why}");
}

#[test]
fn a_program_image_with_another_hash_is_refused() {
    let mut vm = colibri::Vm::builder().worker_threads(1).build();
    vm.load_source("a.colibri", "fn one() -> int {\n    return 1\n}\n")
        .unwrap();
    assert_eq!(int(run(&mut vm, "one", vec![])), 1);
    // The next task carries a hash that the worker's own check does not reproduce.
    vm.corrupt_image_hash();
    let why = internal(run(&mut vm, "one", vec![]));
    assert!(
        why.contains("does not match the main isolate's"),
        "{why}"
    );
}

#[test]
fn the_table_hash_changes_with_what_an_id_means() {
    let hash = |source: &str| {
        let mut vm = colibri::Vm::builder().worker_threads(1).build();
        vm.load_source("h.colibri", source).unwrap();
        vm.program_table_hash().unwrap()
    };
    let f = "fn f() -> int {\n    return 1\n}\n";
    let g = "fn g() -> int {\n    return 2\n}\n";
    let a = hash(&format!("{f}{g}"));
    assert_eq!(a, hash(&format!("{f}{g}")));
    // A body is not part of it, the tables are: the order of the functions, a signature, and
    // whether a module value is a `const`.
    assert_eq!(a, hash(&format!("{}{g}", f.replace("return 1", "return 5"))));
    assert_ne!(a, hash(&format!("{g}{f}")));
    assert_ne!(a, hash(&format!("{}{g}", f.replace("int", "float").replace("1", "1.0"))));
    assert_ne!(
        hash("const K = 1\nfn main() {}\n"),
        hash("let K = 1\nfn main() {}\n")
    );
}

// ---- module state ----

const STATE: &str = r#"
const LIMIT = 10
let mut hits = 0
let name = "main"

fn read_const() -> int {
    return LIMIT * 2
}

fn read_state() -> int {
    return hits
}

fn write_state() {
    hits += 1
}

fn read_name() -> str {
    return name
}

fn main() {}
"#;

#[test]
fn a_worker_reads_consts_and_traps_on_the_main_threads_values() {
    let mut vm = colibri::Vm::builder().worker_threads(1).build();
    vm.load_source("state.colibri", STATE).unwrap();
    assert_eq!(int(run(&mut vm, "read_const", vec![])), 20);
    assert_eq!(
        panic_message(run(&mut vm, "read_state", vec![])),
        "`hits` belongs to the main thread"
    );
    assert_eq!(
        panic_message(run(&mut vm, "write_state", vec![])),
        "`hits` belongs to the main thread (a thread task cannot change it)"
    );
    // An immutable `let` is module state too: only `const`s are shared.
    assert_eq!(
        panic_message(run(&mut vm, "read_name", vec![])),
        "`name` belongs to the main thread"
    );
    // The failed write changed nothing here, and the worker can still read its consts.
    let hits = vm.get_fn::<(), i64>("read_state").unwrap();
    assert_eq!(hits.call(&mut vm, ()).unwrap(), 0);
    assert_eq!(int(run(&mut vm, "read_const", vec![])), 20);
}

#[test]
fn a_script_value_is_the_main_threads_too() {
    let mut vm = colibri::Vm::builder().worker_threads(1).build();
    vm.load_source(
        "script.colibri",
        "const K = 3\nlet mut total = 5\nfn peek() -> int {\n    return total + K\n}\n",
    )
    .unwrap();
    assert_eq!(
        panic_message(run(&mut vm, "peek", vec![])),
        "`total` belongs to the main thread"
    );
}

const DROPS: &str = r#"
let mut live = 0

class Tracked { id: int }

impl Drop for Tracked {
    fn drop(self) {
        live += 1
    }
}

fn make() -> int {
    let t = Tracked { id: 7 }
    return t.id
}

fn fine() -> int {
    return 3
}

fn main() {}
"#;

#[test]
fn an_implicit_drop_that_touches_module_state_is_reported_cleanly() {
    let mut vm = colibri::Vm::builder().worker_threads(1).build();
    vm.load_source("drops.colibri", DROPS).unwrap();
    // The checker does not follow an implicit `Drop`, so the worker's trap is what reports it.
    let message = panic_message(run(&mut vm, "make", vec![]));
    assert!(
        message.contains("`live` belongs to the main thread (a thread task cannot change it)"),
        "{message}"
    );
    // The main isolate runs the same hook without trouble, and the worker is still usable.
    let make = vm.get_fn::<(), i64>("make").unwrap();
    assert_eq!(make.call(&mut vm, ()).unwrap(), 7);
    assert_eq!(int(run(&mut vm, "fine", vec![])), 3);
    assert_eq!(vm.worker_stats().builds.load(Ordering::SeqCst), 1);
}

// ---- budget ----

const SPIN: &str = r#"
fn spin(n: int) -> int {
    let mut i = 0
    let mut total = 0
    while i < n {
        total += i
        i += 1
    }
    return total
}
"#;

#[test]
fn each_task_gets_the_whole_instruction_budget() {
    // How many steps 200 iterations take (the count is the engine's, and stable).
    let mut probe = colibri::Vm::builder().worker_threads(1).build();
    probe.load_source("spin.colibri", SPIN).unwrap();
    let one = run(&mut probe, "spin", vec![Portable::Int(200)]).unwrap().steps;
    assert!(one > 200, "{one}");
    let budget = one + one / 2;
    let mut vm = colibri::Vm::builder()
        .worker_threads(1)
        .instruction_budget(budget)
        .build();
    vm.load_source("spin.colibri", SPIN).unwrap();
    let first = run(&mut vm, "spin", vec![Portable::Int(200)]).unwrap();
    let second = run(&mut vm, "spin", vec![Portable::Int(200)]).unwrap();
    // Together they used more than the allotment: it is per task, not per worker.
    assert!(first.steps + second.steps > budget, "{first:?} {second:?}");
    // A task that needs more than the allotment runs out of it, and the next one is not affected.
    assert_eq!(
        run(&mut vm, "spin", vec![Portable::Int(2_000_000)]).unwrap_err(),
        TaskFailure::BudgetExhausted
    );
    assert_eq!(
        int(run(&mut vm, "spin", vec![Portable::Int(10)])),
        45
    );
}

#[test]
fn the_budget_can_change_between_tasks() {
    let mut vm = colibri::Vm::builder().worker_threads(1).build();
    vm.load_source("spin.colibri", SPIN).unwrap();
    assert_eq!(int(run(&mut vm, "spin", vec![Portable::Int(1000)])), 499_500);
    vm.set_instruction_budget(Some(100));
    assert_eq!(
        run(&mut vm, "spin", vec![Portable::Int(1000)]).unwrap_err(),
        TaskFailure::BudgetExhausted
    );
    vm.set_instruction_budget(None);
    assert_eq!(int(run(&mut vm, "spin", vec![Portable::Int(1000)])), 499_500);
}

// ---- the pool ----

#[test]
fn the_pool_has_the_size_the_builder_asked_for_and_starts_on_the_first_task() {
    for size in [1, 3] {
        let mut vm = colibri::Vm::builder().worker_threads(size).build();
        vm.load_source("a.colibri", "fn one() -> int {\n    return 1\n}\n")
            .unwrap();
        let stats = vm.worker_stats();
        assert_eq!(vm.worker_pool_size(), size);
        // Nothing runs until there is a task.
        assert_eq!(stats.started.load(Ordering::SeqCst), 0);
        assert_eq!(int(run(&mut vm, "one", vec![])), 1);
        assert_eq!(stats.started.load(Ordering::SeqCst), size);
        assert_eq!(stats.alive.load(Ordering::SeqCst), size);
        for _ in 0..10 {
            assert_eq!(int(run(&mut vm, "one", vec![])), 1);
        }
        assert_eq!(stats.started.load(Ordering::SeqCst), size);
    }
}

#[test]
fn the_default_pool_is_one_thread_per_core() {
    let cores = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
    let vm = colibri::Vm::new();
    assert_eq!(vm.worker_pool_size(), cores);
}

/// What a worker's setup registers: a count of the tasks this worker's `Vm` has run, which lives
/// in the worker's state, and the thread's identity.
fn counting_setup(setups: Arc<AtomicUsize>) -> impl Fn(&mut colibri::Vm) + Send + Sync + 'static {
    struct Count(i64);
    move |vm| {
        setups.fetch_add(1, Ordering::SeqCst);
        vm.insert_state(Count(0));
        vm.register_fn("bump", |cx: &mut colibri::Ctx| {
            let count = cx.state_mut::<Count>().expect("set up");
            count.0 += 1;
            count.0
        })
        .unwrap();
    }
}

#[test]
fn a_worker_vm_is_built_once_per_thread_and_reused() {
    let setups = Arc::new(AtomicUsize::new(0));
    let mut vm = colibri::Vm::builder()
        .worker_threads(2)
        .worker_setup(counting_setup(Arc::clone(&setups)))
        .build();
    vm.register_fn("bump", || 0_i64).unwrap();
    vm.load_source("a.colibri", "fn count() -> int {\n    return bump()\n}\n")
        .unwrap();
    let mut highest = 0;
    for _ in 0..8 {
        highest = highest.max(int(run(&mut vm, "count", vec![])));
    }
    // Eight tasks on two threads: some `Vm` ran at least four of them, so it was kept.
    assert!(highest >= 4, "{highest}");
    let builds = vm.worker_stats().builds.load(Ordering::SeqCst);
    assert!(builds <= 2, "{builds}");
    assert_eq!(setups.load(Ordering::SeqCst), builds);
}

#[test]
fn a_new_program_gets_new_worker_vms() {
    let setups = Arc::new(AtomicUsize::new(0));
    let mut vm = colibri::Vm::builder()
        .worker_threads(1)
        .worker_setup(counting_setup(Arc::clone(&setups)))
        .build();
    vm.register_fn("bump", || 0_i64).unwrap();
    vm.load_source("a.colibri", "fn count() -> int {\n    return bump() + 100\n}\n")
        .unwrap();
    assert_eq!(int(run(&mut vm, "count", vec![])), 101);
    assert_eq!(int(run(&mut vm, "count", vec![])), 102);
    vm.shutdown();
    vm.load_source("b.colibri", "fn count() -> int {\n    return bump() + 200\n}\n")
        .unwrap();
    // The old isolate is gone: its state does not leak into the new program's.
    assert_eq!(int(run(&mut vm, "count", vec![])), 201);
    assert_eq!(setups.load(Ordering::SeqCst), 2);
}

// ---- the image ----

#[test]
fn a_worker_runs_the_modules_the_program_imported() {
    let mut vm = colibri::Vm::builder().worker_threads(1).build();
    vm.load_sources(&[
        (
            "app/main.colibri",
            "import util\nfn run() -> int {\n    return util.double(21)\n}\n",
        ),
        (
            "app/util.colibri",
            "fn double(x: int) -> int {\n    return x * 2\n}\n",
        ),
    ])
    .unwrap();
    assert_eq!(int(run(&mut vm, "run", vec![])), 42);
}

#[test]
fn a_worker_builds_from_the_image_not_from_the_disk() {
    let dir = std::env::temp_dir().join(format!("colibri-l17-image-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("main.colibri"),
        "import util\nfn run() -> int {\n    return util.double(4)\n}\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("util.colibri"),
        "fn double(x: int) -> int {\n    return x * 2\n}\n",
    )
    .unwrap();
    let mut vm = colibri::Vm::builder().worker_threads(1).build();
    vm.load_file(dir.join("main.colibri")).unwrap();
    // Gone before any worker exists: what the load read is what the workers get.
    std::fs::remove_dir_all(&dir).unwrap();
    assert_eq!(int(run(&mut vm, "run", vec![])), 8);
}

#[test]
fn no_program_no_task() {
    let mut vm = colibri::Vm::builder().worker_threads(1).build();
    let why = vm.submit_portable("f", vec![]).unwrap_err();
    assert!(matches!(why, TaskFailure::Internal(_)), "{why:?}");
    assert_eq!(vm.worker_stats().started.load(Ordering::SeqCst), 0);
}

// ---- output ----

const SAY: &str = "fn say(s: str) {\n    print(s)\n}\n";

#[test]
fn worker_output_goes_through_the_shared_writer_in_join_order() {
    let out = Out::default();
    let mut vm = colibri::Vm::builder()
        .worker_threads(2)
        .stdout_shared(Box::new(out.clone()))
        .build();
    vm.load_source("say.colibri", SAY).unwrap();
    let say = vm.get_fn::<(String,), ()>("say").unwrap();
    // Each step is joined before the next starts, so the order is fixed.
    run(&mut vm, "say", vec![Portable::Str("worker one".into())]).unwrap();
    say.call(&mut vm, ("main one".to_string(),)).unwrap();
    run(&mut vm, "say", vec![Portable::Str("worker two".into())]).unwrap();
    say.call(&mut vm, ("main two".to_string(),)).unwrap();
    assert_eq!(out.text(), "worker one\nmain one\nworker two\nmain two\n");
}

#[test]
fn two_workers_print_whole_lines() {
    let out = Out::default();
    let mut vm = colibri::Vm::builder()
        .worker_threads(2)
        .stdout_shared(Box::new(out.clone()))
        .build();
    vm.load_source(
        "lines.colibri",
        "fn lines(tag: str) {\n    for i in 0..200 {\n        print(\"{tag}-{i}\")\n    }\n}\n",
    )
    .unwrap();
    let a = vm
        .submit_portable("lines", vec![Portable::Str("a".into())])
        .unwrap();
    let b = vm
        .submit_portable("lines", vec![Portable::Str("b".into())])
        .unwrap();
    a.wait().unwrap();
    b.wait().unwrap();
    let text = out.text();
    let (mut a_lines, mut b_lines) = (Vec::new(), Vec::new());
    for line in text.lines() {
        let (tag, n) = line.split_once('-').expect("a whole line");
        let n: usize = n.parse().expect("a whole line");
        match tag {
            "a" => a_lines.push(n),
            "b" => b_lines.push(n),
            other => panic!("torn line {other}-{n}"),
        }
    }
    // Each task's own lines are in order, and none was lost or torn.
    let expected: Vec<usize> = (0..200).collect();
    assert_eq!(a_lines, expected);
    assert_eq!(b_lines, expected);
}

// ---- shutdown ----

#[test]
fn dropping_an_idle_vm_ends_every_thread() {
    let mut vm = colibri::Vm::builder().worker_threads(3).build();
    vm.load_source("a.colibri", "fn one() -> int {\n    return 1\n}\n")
        .unwrap();
    assert_eq!(int(run(&mut vm, "one", vec![])), 1);
    let stats = vm.worker_stats();
    assert_eq!(stats.alive.load(Ordering::SeqCst), 3);
    drop(vm);
    assert_eq!(stats.started.load(Ordering::SeqCst), 3);
    assert_eq!(stats.alive.load(Ordering::SeqCst), 0);
}

/// Dropped with the `Vm`'s host state: lets the task that is blocked go.
struct Release(mpsc::Sender<()>);

impl Drop for Release {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}

#[test]
fn dropping_a_vm_lets_the_running_task_finish_and_drops_the_queued_ones() {
    let (started_tx, started_rx) = mpsc::channel::<()>();
    let (gate_tx, gate_rx) = mpsc::channel::<()>();
    let started_tx = Arc::new(Mutex::new(started_tx));
    let gate_rx = Arc::new(Mutex::new(gate_rx));
    let finished = Arc::new(AtomicUsize::new(0));
    let queued_ran = Arc::new(AtomicUsize::new(0));
    let mut vm = colibri::Vm::builder()
        .worker_threads(1)
        .worker_setup({
            let (started_tx, gate_rx) = (Arc::clone(&started_tx), Arc::clone(&gate_rx));
            let (finished, queued_ran) = (Arc::clone(&finished), Arc::clone(&queued_ran));
            move |vm| {
                // The first task tells the test it runs, then waits for the gate, which opens
                // when the main `Vm`'s host state is dropped.
                let (started_tx, gate_rx) = (Arc::clone(&started_tx), Arc::clone(&gate_rx));
                vm.register_fn("block", move || {
                    started_tx.lock().unwrap().send(()).unwrap();
                    gate_rx.lock().unwrap().recv().unwrap();
                })
                .unwrap();
                let finished = Arc::clone(&finished);
                vm.register_fn("finish", move || {
                    finished.fetch_add(1, Ordering::SeqCst);
                })
                .unwrap();
                let queued_ran = Arc::clone(&queued_ran);
                vm.register_fn("queued", move || {
                    queued_ran.fetch_add(1, Ordering::SeqCst);
                })
                .unwrap();
            }
        })
        .build();
    // The main `Vm` declares the same functions (it never calls them).
    vm.register_fn("block", || {}).unwrap();
    vm.register_fn("finish", || {}).unwrap();
    vm.register_fn("queued", || {}).unwrap();
    vm.insert_state(Release(gate_tx));
    vm.load_source(
        "gate.colibri",
        "fn first() {\n    block()\n    finish()\n}\n\nfn second() {\n    queued()\n}\n",
    )
    .unwrap();
    let stats = vm.worker_stats();
    let first = vm.submit_portable("first", vec![]).unwrap();
    let second = vm.submit_portable("second", vec![]).unwrap();
    // The first task is running (the only worker is in it), the second waits in the queue.
    started_rx.recv().unwrap();
    drop(vm);
    // `drop` closed the queue, freed the host state (which opened the gate) and joined the
    // worker: the running task finished, the queued one never ran, and no thread is left.
    assert_eq!(finished.load(Ordering::SeqCst), 1);
    assert_eq!(queued_ran.load(Ordering::SeqCst), 0);
    assert_eq!(stats.alive.load(Ordering::SeqCst), 0);
    assert!(first.wait().is_ok());
    assert_eq!(second.wait().unwrap_err(), TaskFailure::Cancelled);
}

// ---- deep values ----

fn nested(depth: usize) -> Portable {
    let mut value = Portable::Int(0);
    for id in 0..depth {
        value = Portable::List {
            id: id as u32,
            items: Box::new([value]),
        };
    }
    value
}

fn depth_of(mut value: &Portable) -> usize {
    let mut depth = 0;
    while let Portable::List { items, .. } = value {
        depth += 1;
        value = &items[0];
    }
    depth
}

const ECHO: &str = "fn echo[T](x: T) -> T {\n    return x\n}\n";

#[test]
fn a_nested_value_crosses_both_ways() {
    let mut vm = colibri::Vm::builder().worker_threads(1).build();
    vm.load_source("echo.colibri", ECHO).unwrap();
    let back = run(&mut vm, "echo", vec![nested(2_000)]).unwrap().value;
    assert_eq!(depth_of(&back), 2_000);
}

#[test]
fn a_value_nested_beyond_the_walk_budget_is_refused_not_crashed() {
    let mut vm = colibri::Vm::builder().worker_threads(1).build();
    vm.load_source("echo.colibri", ECHO).unwrap();
    // Far past what 48 MiB of stack can hold, on any build.
    let why = run(&mut vm, "echo", vec![nested(3_000_000)]).unwrap_err();
    match why {
        TaskFailure::Copy(message) => {
            assert!(message.contains("nested too deeply"), "{message}")
        }
        other => panic!("{other:?}"),
    }
    // The worker is fine.
    assert_eq!(depth_of(&run(&mut vm, "echo", vec![nested(10)]).unwrap().value), 10);
}
