//! Runs the golden programs the VM is expected to handle at this step through `run_source` and
//! compares them with the `// expect:` and `// panic:` directives (the format of
//! `colibri-cli/src/golden.rs`).
//!

use std::fs;
use std::path::{Path, PathBuf};

use colibri_vm::{Options, run_source_with};

/// The directories that are in scope for this step, and single files.
const DIRS: &[&str] = &["basics", "functions", "control", "numbers", "globals"];
const FILES: &[&str] = &[
    "hello.colibri",
    "panics/stack_overflow.colibri",
    "panics/mutual_recursion_stack_overflow.colibri",
];

#[derive(Default)]
struct Directives {
    expect: Vec<String>,
    panic: Vec<String>,
    drops: Option<Vec<String>>,
    has_error: bool,
    /// Directives the harness cannot check on the VM yet.
    unchecked: Vec<&'static str>,
    skip: bool,
}

fn parse(source: &str) -> Directives {
    let mut d = Directives::default();
    for line in source.lines() {
        let mut rest = line;
        while let Some(pos) = rest.find("//") {
            let comment = rest[pos + 2..].trim_start();
            if let Some(v) = comment.strip_prefix("expect:") {
                d.expect
                    .push(v.strip_prefix(' ').unwrap_or(v).trim_end().to_string());
            } else if let Some(v) = comment.strip_prefix("panic:") {
                d.panic.push(v.trim().to_string());
            } else if let Some(v) = comment.strip_prefix("drops:") {
                d.drops = Some(
                    v.split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect(),
                );
            } else if comment.starts_with("error:") {
                d.has_error = true;
            } else if comment.starts_with("main-err:") {
                d.unchecked.push("main-err");
            } else if comment.starts_with("leaks:") {
                d.unchecked.push("leaks");
            } else if comment.starts_with("caps:") {
                d.unchecked.push("caps");
            } else if comment.starts_with("features:") {
                d.unchecked.push("features");
            } else if comment.starts_with("skip:") {
                d.skip = true;
            } else if let Some(v) = comment.strip_prefix("only:") {
                if v.trim_start().starts_with("windows") && !cfg!(windows) {
                    d.skip = true;
                }
                if v.trim_start().starts_with("unix") && cfg!(windows) {
                    d.skip = true;
                }
            } else {
                rest = &rest[pos + 2..];
                continue;
            }
            break;
        }
    }
    d
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if !path.join("main.colibri").is_file() {
                collect(&path, out);
            }
        } else if path.extension().is_some_and(|e| e == "colibri") {
            out.push(path);
        }
    }
}

/// `Ok` when the program behaves as its directives say.
fn run_one(source: &str, d: &Directives) -> Result<(), String> {
    if !d.unchecked.is_empty() {
        return Err(format!("directives not checked yet: {:?}", d.unchecked));
    }
    let options = Options {
        trace_drops: true,
        // A runaway program fails the test instead of hanging it.
        instruction_budget: Some(20_000_000),
        ..Options::default()
    };
    let outcome = run_source_with(source, &options);
    if outcome.compile_error {
        return Err(format!(
            "did not compile: {}",
            outcome.panic.unwrap_or_default()
        ));
    }
    let mut problems = Vec::new();
    let expected: String = d.expect.iter().map(|l| format!("{l}\n")).collect();
    if outcome.stdout != expected {
        problems.push(format!(
            "stdout mismatch\n--- expected\n{expected}--- actual\n{}",
            outcome.stdout
        ));
    }
    match (&outcome.panic, d.panic.is_empty()) {
        (Some(got), false) => {
            for want in d.panic.iter().filter(|w| !got.contains(w.as_str())) {
                problems.push(format!("panic {got:?} does not contain {want:?}"));
            }
        }
        (Some(got), true) => problems.push(format!("unexpected panic: {got}")),
        (None, false) => problems.push(format!("expected a panic containing {:?}", d.panic)),
        (None, true) => {}
    }
    if let Some(want) = &d.drops
        && *want != outcome.drops
    {
        problems.push(format!(
            "drop order mismatch: expected [{}], got [{}]",
            want.join(", "),
            outcome.drops.join(", ")
        ));
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems.join("\n"))
    }
}

fn golden_run() -> String {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/golden");
    let mut files = Vec::new();
    for dir in DIRS {
        collect(&root.join(dir), &mut files);
    }
    for file in FILES {
        files.push(root.join(file));
    }
    files.sort();
    let rel = |p: &Path| {
        p.strip_prefix(&root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/")
    };
    let mut report = String::new();
    let mut passed = 0;
    let mut not_applicable = 0;
    for path in &files {
        let name = rel(path);
        let source = fs::read_to_string(path).expect("a golden file is readable");
        let d = parse(&source);
        if d.has_error || d.skip {
            // Compile-error goldens are checked by the other engine's harness; `// skip:` and
            // the other system's `// only:` files are not run anywhere.
            not_applicable += 1;
            continue;
        }
        match run_one(&source, &d) {
            Ok(()) => passed += 1,
            Err(why) => report.push_str(&format!("{name}: FAILS\n{why}\n")),
        }
    }
    eprintln!("golden on the VM: {passed} passed, {not_applicable} not applicable");
    report
}

#[test]
fn goldens_pass() {
    // Deep expressions in a few files need more than a test thread's stack to check.
    let handle = std::thread::Builder::new()
        .stack_size(256 << 20)
        .spawn(golden_run)
        .expect("a thread starts");
    let report = handle.join().expect("the run does not crash");
    assert!(report.is_empty(), "{report}");
}
