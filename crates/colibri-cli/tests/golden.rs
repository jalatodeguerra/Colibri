//! Runs every `tests/golden/**/*.colibri` file at the repo root as one trial, and every folder
//! that holds a `main.colibri` as one trial (its files are one program).

use std::fs;
use std::path::{Path, PathBuf};

use colibri_lang::golden::{
    Directives, Outcome, PackageDirectives, check, check_package, parse_directives, run_golden,
    run_golden_package,
};
use colibri_lang::on_big_stack;
use libtest_mimic::{Arguments, Failed, Trial};

enum Test {
    File(PathBuf),
    Package(PathBuf),
}

enum Source {
    File {
        path: PathBuf,
        source: String,
        directives: Box<Directives>,
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

    fn outcome(&self) -> Outcome {
        match &self.source {
            Source::File { path, source, .. } => {
                let (path, source) = (path.clone(), source.clone());
                on_big_stack(move || run_golden(&path, &source))
            }
            Source::Package { dir, .. } => {
                let dir = dir.clone();
                on_big_stack(move || run_golden_package(&dir))
            }
        }
    }

    fn judge(&self, outcome: &Outcome) -> Result<(), String> {
        match &self.source {
            Source::File { directives, .. } => check(directives, outcome),
            Source::Package { directives, .. } => check_package(directives, outcome),
        }
    }

    /// Runs and checks against the directives; a crash is a failure too.
    fn run(&self) -> Result<(), String> {
        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.judge(&self.outcome())));
        result.unwrap_or_else(|payload| {
            Err(format!("the engine crashed: {}", payload_text(&payload)))
        })
    }
}

/// The text of a caught Rust panic.
fn payload_text(payload: &Box<dyn std::any::Any + Send>) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "(no message)".to_string())
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
                            directives: Box::new(directives),
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

    let mut trials: Vec<Trial> = Vec::new();
    for case in cases {
        // `// skip:` everywhere and `// only:` on the other systems: reported as ignored (not
        // run), never as passed.
        let ignored = case.not_run_here();
        let name = case.name.clone();
        let trial = Trial::test(name, move || {
            let result = case.run();
            result.map_err(Failed::from)
        })
        .with_ignored_flag(ignored);
        trials.push(trial);
    }

    libtest_mimic::run(&Arguments::from_args(), trials).exit();
}
