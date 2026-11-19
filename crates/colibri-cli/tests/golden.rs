//! Runs every `tests/golden/**/*.colibri` file at the repo root as one trial, and every folder
//! that holds a `main.colibri` as one trial (its files are one program).

use std::fs;
use std::path::{Path, PathBuf};

use colibri_lang::golden::{
    PackageDirectives, check, check_package, parse_directives, run_golden, run_golden_package,
};
use colibri_lang::on_big_stack;
use libtest_mimic::{Arguments, Failed, Trial};

enum Test {
    File(PathBuf),
    Package(PathBuf),
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

    let trials = tests
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
                    let skipped = directives.skip.is_some();
                    Trial::test(name, move || {
                        let outcome = on_big_stack(move || run_golden(&path, &source));
                        check(&directives, &outcome).map_err(Failed::from)
                    })
                    .with_ignored_flag(skipped)
                }
                Test::Package(dir) => {
                    let mut files = Vec::new();
                    package_files(&dir, &dir, &mut files);
                    let directives = PackageDirectives::parse(&files);
                    let skipped = directives.skip().is_some();
                    Trial::test(format!("{name}/"), move || {
                        let outcome = on_big_stack(move || run_golden_package(&dir));
                        check_package(&directives, &outcome).map_err(Failed::from)
                    })
                    .with_ignored_flag(skipped)
                }
            }
        })
        .collect();

    libtest_mimic::run(&Arguments::from_args(), trials).exit();
}
