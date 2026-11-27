//! A task's random numbers depend on the program's seed and on the task, not on the worker that
//! runs it (phase 3 L18): the numbers of a run repeat, whatever the pool's size and however the
//! threads happen to start.

use colibri_runtime as colibri;
use colibri_std::{StdConfig, WorkerStd};

const PROGRAM: &str = r#"
import rand

fn draw(n: int) -> List[int] {
    let mut out = []
    for _ in 0..n {
        out.push(rand.int(0, 1000000))
    }
    return out
}

fn main() {
    // `rand.int` in the task is the task's own generator; the one of `main` is not touched.
    let mut tasks = []
    for _ in 0..8 {
        tasks.push(task.run(fn() => draw(3)))
    }
    let mut lines = []
    for t in tasks {
        lines.push("{t.join().expect("task")}")
    }
    print(lines.join(";"))
    print("main: {draw(2)}")
}
"#;

fn run(workers: usize, seed: Option<u64>) -> String {
    run_program(workers, seed, PROGRAM)
}

fn run_program(workers: usize, seed: Option<u64>, program: &str) -> String {
    let config = StdConfig {
        seed,
        ..StdConfig::default()
    };
    let worker_std = WorkerStd::from_config(&config);
    let out = colibri::SyncBuffer::new();
    let mut vm = colibri::Vm::builder()
        .worker_threads(workers)
        .stdout_shared(Box::new(out.clone()))
        .worker_setup(move |vm| {
            colibri_std::install_worker(vm, &worker_std).expect("the library registers");
        })
        .build();
    colibri_std::install_with(&mut vm, config).expect("the library registers");
    vm.load_source("task_seed.colibri", program).unwrap();
    vm.run_main().unwrap();
    out.contents()
}

#[test]
fn a_tasks_numbers_do_not_depend_on_the_pool_or_on_which_worker_ran_it() {
    let reference = run(1, Some(42));
    // Eight tasks, three numbers each, and the main thread's two.
    assert_eq!(reference.lines().count(), 2, "{reference}");
    assert_eq!(reference.lines().next().unwrap().split(';').count(), 8);
    for workers in [1, 2, 3, 8] {
        for _ in 0..3 {
            assert_eq!(run(workers, Some(42)), reference, "{workers} workers");
        }
    }
    // The seed decides: another seed, other numbers.
    assert_ne!(run(2, Some(43)), reference);
}

#[test]
fn two_tasks_do_not_repeat_each_other() {
    let text = run(2, Some(7));
    let line = text.lines().next().unwrap();
    let rows: Vec<&str> = line.split(';').collect();
    let mut sorted = rows.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), rows.len(), "{line}");
}

const NESTED: &str = r#"
import rand

fn draw(n: int) -> List[int] {
    let mut out = []
    for _ in 0..n {
        out.push(rand.int(0, 1000000))
    }
    return out
}

// Three levels. `outer` delays its joins, so that on a pool of threads `mid` is picked up by one,
// and on a pool of one `outer` runs it itself.
fn mid() -> List[List[int]] {
    let c = task.run(fn() => draw(3))
    let mine = draw(2)
    return [mine, c.join().expect("c")]
}

fn outer() -> List[List[List[int]]] {
    let b = task.run(mid)
    let after = task.run(fn() => draw(3))
    let mut spin = 0
    for i in 0..300000 {
        spin += i
    }
    return [b.join().expect("b"), [after.join().expect("after"), [spin]]]
}

fn main() {
    print("{task.run(outer).join().expect("outer")}")
    print("main: {draw(2)}")
}
"#;

#[test]
fn what_a_task_starts_is_numbered_from_it_whether_a_thread_or_its_joiner_runs_it() {
    let reference = run_program(3, Some(42), NESTED);
    assert_eq!(reference.lines().count(), 2, "{reference}");
    for workers in [1, 2, 3, 8] {
        for _ in 0..3 {
            assert_eq!(
                run_program(workers, Some(42), NESTED),
                reference,
                "{workers} workers"
            );
        }
    }
}

const TURNS: &str = r#"
import rand

fn draw(n: int) -> List[int] {
    let mut out = []
    for _ in 0..n {
        out.push(rand.int(0, 1000000))
    }
    return out
}

fn group() -> List[int] {
    return task.run(fn() => draw(2)).join().expect("child")
}

fn main() {
    let mut lines = []
    for _ in 0..6 {
        lines.push("{task.run(group).join().expect("group")}")
    }
    print(lines.join(";"))
}
"#;

#[test]
fn a_worker_that_runs_task_after_task_numbers_what_each_starts_from_its_own_start() {
    // Six tasks one after the other: on a pool of one the same thread runs them all, and the
    // children of the later ones are numbered as if the thread had run nothing before. (The unit
    // test of `WorkerLaunch` in `colibri-runtime` pins the counter itself; this is the program
    // level, which must agree with pools that spread the tasks over threads.)
    let reference = run_program(8, Some(5), TURNS);
    for _ in 0..3 {
        assert_eq!(run_program(2, Some(5), TURNS), reference);
        assert_eq!(run_program(1, Some(5), TURNS), reference);
    }
}

#[test]
fn a_task_starts_with_a_clock_made_afresh() {
    // The worker's clock is a manual one that `sleep` moves; a task that sleeps leaves the next
    // task on the same worker a clock at its start.
    let program = r#"
import time

fn nap() -> int {
    let before = time.monotonic()
    time.sleep(2.0)
    return time.monotonic() - before
}

fn read() -> int {
    return time.monotonic()
}

fn main() {
    print("{task.run(nap).join().expect("nap")}")
    print("{task.run(read).join().expect("read")}")
}
"#;
    for workers in [1, 3] {
        let config = StdConfig {
            seed: Some(1),
            ..StdConfig::default()
        };
        let worker_std = WorkerStd::from_config(&config)
            .with_clock(|| Box::new(colibri_std::ManualClock::new(10.0)));
        let out = colibri::SyncBuffer::new();
        let mut vm = colibri::Vm::builder()
            .worker_threads(workers)
            .allow(colibri::Cap::Os)
            .stdout_shared(Box::new(out.clone()))
            .worker_setup(move |vm| {
                colibri_std::install_worker(vm, &worker_std).expect("the library registers");
            })
            .build();
        colibri_std::install_with(&mut vm, config).expect("the library registers");
        vm.load_source("clock.colibri", program).unwrap();
        vm.run_main().unwrap();
        // The second task reads the monotonic time of a clock at its start: 0, whichever worker.
        assert_eq!(out.contents(), "2000000000\n0\n", "{workers} workers");
    }
}

const JOINER_COROUTINES: &str = r#"
import rand

fn b() -> int {
    wait(0.05)
    return 1
}

fn late() {
    wait(0.05)
    print("late ran")
}

fn tick() {
    wait(0.005)
    print("tick draws {rand.int(0, 1000000)}")
    go late()
}

fn outer() -> int {
    go tick()
    let t = task.run(b)
    let mut k = 0
    for i in 0..300000 {
        k += 1
    }
    let v = t.join().expect("b")
    wait(0.2)
    return v
}

fn main() {
    print("outer {task.run(outer).join().expect("o")}")
}
"#;

#[test]
fn the_coroutines_of_a_joiner_do_not_run_inside_the_task_it_runs_inline() {
    // `tick` is a coroutine of `outer`: it draws from the generator of `outer`, and the coroutine
    // it starts is not cancelled with `b`, whether `b` ran on a thread or inline in the join.
    // The reference is the order that every pool must give.
    let on_thread = run_program(3, Some(42), JOINER_COROUTINES);
    assert!(on_thread.contains("late ran"), "{on_thread}");
    assert!(on_thread.ends_with("outer 1\n"), "{on_thread}");
    for workers in [1, 3] {
        for _ in 0..3 {
            let text = run_program(workers, Some(42), JOINER_COROUTINES);
            assert_eq!(text, on_thread, "{workers} workers");
        }
    }
}
