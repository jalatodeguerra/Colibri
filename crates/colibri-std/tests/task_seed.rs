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
    vm.load_source("task_seed.colibri", PROGRAM).unwrap();
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
