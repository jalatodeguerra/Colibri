//! Robustness: golden programs with a line removed, swapped or cut short must never crash the
//! host or hit an interpreter "internal error": they compile with diagnostics, run, or panic
//! the Colibri way (DESIGN §10.4). Programs with a `while` or `loop` are skipped, since a
//! mutation can make them endless; so are programs that implement `Iter`, whose `next` a
//! mutation can stop from ever ending. A mutant of a channel program can wait for a value that
//! nobody sends (a receive that cannot be satisfied is not detected, DESIGN §14.1), so every
//! variant runs under a watchdog that stops it after a few seconds, through the flag that stops a
//! `Vm` from another thread; a variant that was stopped did not crash, and that is all this
//! test asks.

use colibri_runtime as colibri;

fn golden_sources() -> Vec<String> {
    fn walk(dir: &std::path::Path, out: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut paths: Vec<_> = entries.flatten().map(|e| e.path()).collect();
        paths.sort();
        for path in paths {
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|e| e == "colibri")
                && let Ok(text) = std::fs::read_to_string(&path)
            {
                out.push(text);
            }
        }
    }
    let mut out = Vec::new();
    walk(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/golden"),
        &mut out,
    );
    out
}

/// How long a variant may run before the watchdog stops it. A variant takes milliseconds; one
/// that waits for what cannot come is stopped, and counts as having not crashed.
const WATCHDOG: std::time::Duration = std::time::Duration::from_secs(3);

/// Loads and runs `src`; returns a panic message if the program ended in one.
fn run(src: &str) -> Option<String> {
    let mut vm = colibri::Vm::builder()
        .stdout(Box::new(colibri::SharedBuffer::new()))
        .trace_drops(true)
        .max_call_depth(200)
        .build();
    let stop = vm.stop_handle();
    let (done, finished) = std::sync::mpsc::channel::<()>();
    let watchdog = std::thread::spawn(move || {
        // The sender is dropped when the variant is over: only a timeout stops it.
        if let Err(std::sync::mpsc::RecvTimeoutError::Timeout) = finished.recv_timeout(WATCHDOG) {
            stop.store(true, std::sync::atomic::Ordering::Release);
        }
    });
    let result = vm
        .load_source("mutant.colibri", src)
        .and_then(|()| vm.run_main());
    vm.shutdown();
    drop(done);
    watchdog.join().unwrap();
    match result {
        Err(colibri::Error::Panic { message, .. }) => Some(message),
        _ => None,
    }
}

#[test]
fn mutated_golden_programs_never_hit_an_internal_error() {
    let mut checked = 0;
    for src in golden_sources() {
        if src.contains("while")
            || src.contains("loop")
            || src.contains("0..100000")
            || src.contains("impl Iter")
        {
            continue;
        }
        let lines: Vec<&str> = src.lines().collect();
        let mut variants = Vec::new();
        for skip in 0..lines.len() {
            let kept: Vec<&str> = lines
                .iter()
                .enumerate()
                .filter(|&(i, _)| i != skip)
                .map(|(_, l)| *l)
                .collect();
            variants.push(kept.join("\n"));
        }
        for cut in (1..lines.len()).step_by(2) {
            variants.push(lines[..cut].join("\n"));
        }
        for swap in 0..lines.len().saturating_sub(1) {
            let mut l = lines.clone();
            l.swap(swap, swap + 1);
            variants.push(l.join("\n"));
        }
        for variant in variants {
            if let Some(message) = run(&variant) {
                assert!(
                    !message.starts_with("internal error"),
                    "{message}\n--- program\n{variant}"
                );
            }
            checked += 1;
        }
    }
    assert!(checked > 1000, "only {checked} variants ran");
}
