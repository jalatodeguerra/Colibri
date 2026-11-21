//! Runs every `tests/golden/**/*.colibri` file at the repo root as one trial, and every folder
//! that holds a `main.colibri` as one trial (its files are one program).
//!
//! `COLIBRI_ENGINE` picks the engine the tests run on: `tree` (the default), `vm`, or `both`,
//! which runs every test on the tree-walker (checked against the directives) and on the VM
//! and fails when the two outcomes differ. On the VM, `tests/golden/vm_pending.txt` lists the
//! tests that do not pass yet, one per line (`path  # reason`, a folder ends with `/`):
//! those are reported as ignored, and one that passes fails the run, so the list only
//! shrinks. `COLIBRI_VM_PENDING_REPORT=1` prints a pending line for every test that fails on
//! the VM, ignoring the list, to regenerate it.

use std::fs;
use std::path::{Path, PathBuf};

use colibri_lang::golden::{
    Directives, Outcome, PackageDirectives, check, check_package, diff_outcomes, parse_directives,
    run_golden_on, run_golden_package_on,
};
use colibri_lang::on_big_stack;
use colibri_runtime::EngineKind;
use libtest_mimic::{Arguments, Failed, Trial};

enum Test {
    File(PathBuf),
    Package(PathBuf),
}

/// The engines a run uses.
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Tree,
    Vm,
    Both,
}

impl Mode {
    fn from_env() -> Mode {
        match std::env::var("COLIBRI_ENGINE").as_deref() {
            Ok("vm") => Mode::Vm,
            Ok("both") => Mode::Both,
            _ => Mode::Tree,
        }
    }
}

enum Source {
    File {
        path: PathBuf,
        source: String,
        directives: Directives,
    },
    Package {
        dir: PathBuf,
        directives: PackageDirectives,
    },
}

/// One golden test: what to run and how to judge it.
struct Case {
    name: String,
    source: Source,
}

impl Case {
    fn not_run_here(&self) -> bool {
        match &self.source {
            Source::File { directives, .. } => directives.not_run_here().is_some(),
            Source::Package { directives, .. } => directives.not_run_here().is_some(),
        }
    }

    fn outcome(&self, engine: EngineKind) -> Outcome {
        match &self.source {
            Source::File { path, source, .. } => {
                let (path, source) = (path.clone(), source.clone());
                on_big_stack(move || run_golden_on(&path, &source, Some(engine)))
            }
            Source::Package { dir, .. } => {
                let dir = dir.clone();
                on_big_stack(move || run_golden_package_on(&dir, Some(engine)))
            }
        }
    }

    fn judge(&self, outcome: &Outcome) -> Result<(), String> {
        match &self.source {
            Source::File { directives, .. } => check(directives, outcome),
            Source::Package { directives, .. } => check_package(directives, outcome),
        }
    }

    /// Runs on `engine` and checks against the directives; a crash is a failure too.
    fn run_on(&self, engine: EngineKind) -> Result<(), String> {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.judge(&self.outcome(engine))
        }));
        result.unwrap_or_else(|_| Err("the engine crashed".to_string()))
    }

    /// Both engines: the tree-walker against the directives, the VM against the tree-walker.
    fn run_both(&self) -> Result<(), String> {
        let tree = self.outcome(EngineKind::Tree);
        self.judge(&tree)?;
        let vm = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.outcome(EngineKind::Bytecode)
        }))
        .map_err(|_| "the VM crashed".to_string())?;
        match diff_outcomes(&tree, &vm) {
            Some(diff) => Err(format!("the engines differ:\n{diff}")),
            None => Ok(()),
        }
    }
}

/// `tests/golden/vm_pending.txt`: test name (as the trial is named) to its reason.
fn read_pending(root: &Path) -> Vec<(String, String)> {
    let text = fs::read_to_string(root.join("vm_pending.txt")).unwrap_or_default();
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let (path, reason) = line.split_once('#').unwrap_or((line, ""));
            Some((path.trim().to_string(), reason.trim().to_string()))
        })
        .collect()
}

/// What to write after `#` for a test that fails on the VM: the missing feature if its panic
/// names one.
fn reason_of(failure: &str) -> String {
    const KEY: &str = "the bytecode VM does not support ";
    if let Some(at) = failure.find(KEY) {
        let rest = &failure[at + KEY.len()..];
        let end = rest.find(" yet").unwrap_or(rest.len());
        return rest[..end].replace('\n', " ");
    }
    if failure.contains("crashed") {
        return "crash".to_string();
    }
    "differs".to_string()
}

fn collect(dir: &Path, out: &mut Vec<Test>) {
    if dir.join("main.colibri").is_file() {
        out.push(Test::Package(dir.to_path_buf()));
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, out);
        } else if path.extension().is_some_and(|e| e == "colibri") {
            out.push(Test::File(path));
        }
    }
}

/// Every `.colibri` file under `dir`, as (path relative to `dir` with `/`, source).
fn package_files(dir: &Path, base: &Path, out: &mut Vec<(String, String)>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            package_files(&path, base, out);
        } else if path.extension().is_some_and(|e| e == "colibri") {
            let rel = path
                .strip_prefix(base)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            out.push((rel, fs::read_to_string(&path).expect("readable")));
        }
    }
}

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/golden");
    let mode = Mode::from_env();
    let mut tests = Vec::new();
    collect(&root, &mut tests);
    let key = |t: &Test| match t {
        Test::File(p) | Test::Package(p) => p.clone(),
    };
    tests.sort_by_key(key);

    let cases: Vec<Case> = tests
        .into_iter()
        .map(|test| {
            let path = key(&test);
            let name = path
                .strip_prefix(&root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            match test {
                Test::File(path) => {
                    let source = fs::read_to_string(&path).expect("golden file is readable");
                    let directives = parse_directives(&source);
                    Case {
                        name,
                        source: Source::File {
                            path,
                            source,
                            directives,
                        },
                    }
                }
                Test::Package(dir) => {
                    let mut files = Vec::new();
                    package_files(&dir, &dir, &mut files);
                    Case {
                        name: format!("{name}/"),
                        source: Source::Package {
                            dir,
                            directives: PackageDirectives::parse(&files),
                        },
                    }
                }
            }
        })
        .collect();

    // Printing what fails on the VM, to regenerate the list.
    if mode == Mode::Vm && std::env::var_os("COLIBRI_VM_PENDING_REPORT").is_some() {
        for case in cases.iter().filter(|c| !c.not_run_here()) {
            if let Err(why) = case.run_on(EngineKind::Bytecode) {
                println!("{}  # {}", case.name, reason_of(&why));
            }
        }
        return;
    }

    let pending = if mode == Mode::Tree {
        Vec::new()
    } else {
        read_pending(&root)
    };
    let is_pending = |name: &str| pending.iter().any(|(p, _)| p == name);

    let cases: Vec<std::sync::Arc<Case>> = cases.into_iter().map(std::sync::Arc::new).collect();
    let mut trials: Vec<Trial> = Vec::new();
    for case in &cases {
        // In `both` mode a test the VM cannot run yet is still run on the tree-walker.
        let vm_pending = is_pending(&case.name);
        let ignored = case.not_run_here() || (vm_pending && mode == Mode::Vm);
        let name = case.name.clone();
        let case = std::sync::Arc::clone(case);
        let trial = Trial::test(name, move || {
            let result = match mode {
                Mode::Tree => case.run_on(EngineKind::Tree),
                Mode::Vm => case.run_on(EngineKind::Bytecode),
                Mode::Both if vm_pending => case.run_on(EngineKind::Tree),
                Mode::Both => case.run_both(),
            };
            result.map_err(Failed::from)
        })
        // `// skip:` everywhere, `// only:` on the other systems, and on the VM the pending
        // list: reported as ignored (not run), never as passed.
        .with_ignored_flag(ignored);
        trials.push(trial);
    }

    // The ratchet: a test on the pending list that passes fails the run, and so does an entry
    // that names no test.
    if mode == Mode::Vm {
        let listed: Vec<(String, String)> = pending;
        trials.push(Trial::test("vm_pending_list_is_exact", move || {
            let mut report = String::new();
            for (name, reason) in &listed {
                match cases.iter().find(|c| &c.name == name) {
                    None => report.push_str(&format!("vm_pending.txt names no test: {name}\n")),
                    Some(case) if case.not_run_here() => {}
                    Some(case) => {
                        if case.run_on(EngineKind::Bytecode).is_ok() {
                            report.push_str(&format!(
                                "{name}: passes on the VM now but is listed as pending ({reason}); remove it from vm_pending.txt\n"
                            ));
                        }
                    }
                }
            }
            if report.is_empty() {
                Ok(())
            } else {
                Err(Failed::from(report))
            }
        }));
    }

    libtest_mimic::run(&Arguments::from_args(), trials).exit();
}
