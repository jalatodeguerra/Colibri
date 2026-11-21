//! The example runs in-process: its log is compared with a golden file, the committed
//! `host.cbi` must be current, and a misbehaving script must not take the host down.

/// Tests that do not pass on the bytecode VM yet (`COLIBRI_ENGINE=vm`), with the reason: the
/// example's host functions cannot be called from the VM so far. On the VM such a test is
/// expected to fail, and one that passes fails the run, so the list only shrinks.
const VM_PENDING: &[&str] = &[
    "a_panic_in_update_is_logged_and_the_host_goes_on",
    "a_script_without_on_progress_is_skipped_by_the_hook_lookup",
    "the_log_matches_the_golden_file",
    "script_tests_see_a_fresh_queue_each",
];

fn vm_pending(name: &str, body: impl FnOnce()) {
    let on_vm = std::env::var("COLIBRI_ENGINE").is_ok_and(|v| v == "vm");
    if !on_vm || !VM_PENDING.contains(&name) {
        return body();
    }
    let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) else {
        panic!("{name} passes on the VM now; remove it from VM_PENDING");
    };
    // It must fail by the VM's refusal of the missing feature, not another way.
    let text = payload
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_default();
    assert!(
        text.contains("the bytecode VM does not support"),
        "{name} is listed as pending but failed another way: {text}"
    );
}

use std::fs;

use colibri_example_host_jobs::{DEFAULT_TICKS, JobHost, STEP_BUDGET, scripts_dir};

fn host_with_scripts() -> JobHost {
    let mut host = JobHost::new().expect("the host API registers");
    host.load_file(scripts_dir().join("jobs.colibri"))
        .unwrap_or_else(|e| panic!("{}", e.render(false)));
    host
}

#[test]
fn the_log_matches_the_golden_file() {
    vm_pending("the_log_matches_the_golden_file", || {
        let log = host_with_scripts().run(DEFAULT_TICKS).join("\n") + "\n";
        let path = scripts_dir().join("../tests/expected.txt");
        // Never from the VM: its log is not the reference.
        let on_vm = std::env::var("COLIBRI_ENGINE").is_ok_and(|v| v == "vm");
        if std::env::var_os("UPDATE_EXPECTED").is_some() && !on_vm {
            fs::write(&path, &log).unwrap();
        }
        let expected = fs::read_to_string(&path).expect("tests/expected.txt");
        assert_eq!(log, expected, "run with UPDATE_EXPECTED=1 to rewrite it");
    })
}

#[test]
fn the_run_is_deterministic() {
    vm_pending("the_run_is_deterministic", || {
        let a = host_with_scripts().run(300);
        let b = host_with_scripts().run(300);
        assert_eq!(a, b);
    })
}

#[test]
fn the_committed_interface_is_current() {
    let host = JobHost::new().unwrap();
    let on_disk = fs::read_to_string(scripts_dir().join("host.cbi")).expect("scripts/host.cbi");
    assert_eq!(
        on_disk,
        host.interface_text(),
        "host.cbi is stale: run `cargo run -p colibri-example-host-jobs`"
    );
}

#[test]
fn a_panic_in_update_is_logged_and_the_host_goes_on() {
    vm_pending("a_panic_in_update_is_logged_and_the_host_goes_on", || {
        let mut host = JobHost::new().unwrap();
        host.load_source(
            "panicky.colibri",
            r#"
var ticks = 0

fn update(dt: f32) {
    ticks += 1
    if ticks == 5 {
        panic("boom at tick {ticks}")
    }
    if ticks == 8 {
        log("still running at tick {ticks}")
    }
}
"#,
        )
        .unwrap();
        let log = host.run(10);
        let errors: Vec<&String> = log.iter().filter(|l| l.contains("script error")).collect();
        assert_eq!(errors.len(), 1, "{log:#?}");
        assert!(
            errors[0].starts_with("[0005] script error in update: ")
                && errors[0].contains("boom at tick 5"),
            "{}",
            errors[0]
        );
        // Ticks 1-4 and 6-10 ran: the script kept its state and the scheduler kept its clock.
        assert!(
            log.iter()
                .any(|l| l == "[0008] script: still running at tick 8"),
            "{log:#?}"
        );
        assert!(log.last().unwrap().starts_with("summary: ticks=10 "));
        assert!(log.last().unwrap().contains("script_errors=1"));
    })
}

#[test]
fn a_runaway_script_runs_out_of_budget_and_the_host_goes_on() {
    let mut host = JobHost::new().unwrap();
    host.load_source(
        "spin.colibri",
        "fn update(dt: f32) {\n    while true {\n    }\n}\n",
    )
    .unwrap();
    let log = host.run(3);
    let errors = log
        .iter()
        .filter(|l| l.contains("instruction budget"))
        .count();
    assert_eq!(errors, 3, "{log:#?}");
    let max_steps: u64 = log
        .last()
        .unwrap()
        .rsplit("max_steps=")
        .next()
        .unwrap()
        .parse()
        .unwrap();
    assert!(max_steps >= STEP_BUDGET, "{log:#?}");
}

#[test]
fn a_script_without_on_progress_is_skipped_by_the_hook_lookup() {
    vm_pending(
        "a_script_without_on_progress_is_skipped_by_the_hook_lookup",
        || {
            let mut host = JobHost::new().unwrap();
            host.load_source(
                "no_hook.colibri",
                r#"
fn init() {
    submit(Load { cpu: 3.0, io: 0.0 })
}

fn update(dt: f32) {}
"#,
            )
            .unwrap();
            let log = host.run(60);
            // The worker still works (the host logs it), nobody finishes the job, no error.
            assert_eq!(
                log.iter().filter(|l| l.contains("host: worked #1")).count(),
                2,
                "{log:#?}"
            );
            assert!(log.iter().all(|l| !l.contains("script error")), "{log:#?}");
            assert!(
                log.last()
                    .unwrap()
                    .contains("worked=2 balance=0 script_errors=0"),
                "{log:#?}"
            );
        },
    )
}

#[test]
fn a_script_without_update_is_a_load_error() {
    let mut host = JobHost::new().unwrap();
    let err = host
        .load_source("empty.colibri", "fn init() {}\n")
        .unwrap_err();
    assert!(err.to_string().contains("update"), "{err}");
}

#[test]
fn script_tests_see_a_fresh_queue_each() {
    vm_pending("script_tests_see_a_fresh_queue_each", || {
        let mut host = JobHost::new().unwrap();
        let results = host
            .run_script_tests(scripts_dir().join("jobs.colibri"))
            .unwrap_or_else(|e| panic!("{}", e.render(false)));
        assert_eq!(results.len(), 4);
        for (name, result) in &results {
            assert!(result.is_ok(), "{name}: {result:?}");
        }
    })
}
