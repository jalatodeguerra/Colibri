//! Runs every `tests/golden/**/*.colibri` file at the repo root as one trial.

use std::fs;
use std::path::{Path, PathBuf};

use colibri_lang::golden::{Outcome, check, parse_directives, run_golden};
use libtest_mimic::{Arguments, Failed, Trial};

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, out);
        } else if path.extension().is_some_and(|e| e == "colibri") {
            out.push(path);
        }
    }
}

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/golden");
    let mut files = Vec::new();
    collect(&root, &mut files);
    files.sort();

    let trials = files
        .into_iter()
        .map(|path| {
            let name = path
                .strip_prefix(&root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            let source = fs::read_to_string(&path).expect("golden file is readable");
            let directives = parse_directives(&source);
            let skipped = directives.skip.is_some();
            let outcome = run_golden(&path, &source);
            let ignored = skipped || outcome == Outcome::NotImplemented;
            Trial::test(name, move || {
                check(&directives, &outcome).map_err(Failed::from)
            })
            .with_ignored_flag(ignored)
        })
        .collect();

    libtest_mimic::run(&Arguments::from_args(), trials).exit();
}
