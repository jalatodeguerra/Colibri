//! `colibri fmt`: the canonical formatter on the command line.

use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use colibri_runtime::{Diagnostic, SourceMap, render_diagnostic};

/// Directories `colibri fmt` never looks into when it walks a tree.
fn skipped_dir(name: &str) -> bool {
    name == "target" || name.starts_with('.')
}

/// The `.colibri` files under `dir`, sorted, skipping `target/` and hidden directories.
fn find_files(dir: &Path, out: &mut Vec<PathBuf>) -> io::Result<()> {
    let mut entries = std::fs::read_dir(dir)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() {
            if !skipped_dir(&name) {
                find_files(&path, out)?;
            }
        } else if path.extension().is_some_and(|e| e == "colibri") {
            out.push(path);
        }
    }
    Ok(())
}

fn report(name: &str, source: &str, diagnostics: &[Diagnostic]) {
    let color = io::stderr().is_terminal();
    let mut map = SourceMap::new();
    map.add_file(name, source);
    for diagnostic in diagnostics {
        eprint!("{}", render_diagnostic(diagnostic, &map, color));
    }
}

/// Formats `source`, reporting syntax errors on stderr.
fn format(name: &str, source: &str) -> Option<String> {
    match colibri_syntax::format_source(name, source) {
        Ok(out) => Some(out),
        Err(diagnostics) => {
            report(name, source, &diagnostics);
            None
        }
    }
}

/// Runs `colibri fmt`: `-` formats stdin to stdout; otherwise each file (or each `.colibri`
/// file under each directory, or under the current directory with no path) is rewritten in
/// place when it changes. With `check`, nothing is written and the exit code is 1 if a file
/// would change.
pub fn run(paths: &[PathBuf], check: bool) -> ExitCode {
    if paths.len() == 1 && paths[0].as_os_str() == "-" {
        let mut source = String::new();
        if let Err(e) = io::stdin().read_to_string(&mut source) {
            eprintln!("error: cannot read stdin: {e}");
            return ExitCode::from(1);
        }
        return match format("<stdin>", &source) {
            Some(out) if check => {
                if out == source {
                    ExitCode::SUCCESS
                } else {
                    eprintln!("<stdin>");
                    ExitCode::from(1)
                }
            }
            Some(out) => match io::stdout().write_all(out.as_bytes()) {
                Ok(()) => ExitCode::SUCCESS,
                Err(_) => ExitCode::from(1),
            },
            None => ExitCode::from(1),
        };
    }
    let roots: Vec<PathBuf> = if paths.is_empty() {
        vec![PathBuf::from(".")]
    } else {
        paths.to_vec()
    };
    let mut files = Vec::new();
    let mut failed = false;
    for root in &roots {
        if root.is_dir() {
            if let Err(e) = find_files(root, &mut files) {
                eprintln!("error: cannot read {}: {e}", root.display());
                failed = true;
            }
        } else {
            files.push(root.clone());
        }
    }
    let mut changed = Vec::new();
    for path in &files {
        let name = path.display().to_string();
        let source = match std::fs::read_to_string(path) {
            Ok(source) => source,
            Err(e) => {
                eprintln!("error: cannot read {name}: {e}");
                failed = true;
                continue;
            }
        };
        let Some(out) = format(&name, &source) else {
            failed = true;
            continue;
        };
        if out == source {
            continue;
        }
        changed.push(name.clone());
        if !check && let Err(e) = std::fs::write(path, out) {
            eprintln!("error: cannot write {name}: {e}");
            failed = true;
        }
    }
    if check && !changed.is_empty() {
        eprintln!("these files would be reformatted:");
        for name in &changed {
            eprintln!("  {name}");
        }
        failed = true;
    }
    if failed {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}
