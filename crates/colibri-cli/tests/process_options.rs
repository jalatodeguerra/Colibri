//! What `process.run` hands to a program, checked on every system with the same program on
//! both sides: the `colibri` binary under test is the child. A script runs `colibri run child`
//! through `process.run`, so the arguments pass through Rust's command-line quoting (the part
//! that differs most between Unix and Windows), and the child reports what it received.

mod common;

use std::fs;
use std::path::{Path, PathBuf};

/// The contents of a Colibri string literal for a path: `\` starts an escape in Colibri
/// and `{` an interpolation, so a native path pasted into Colibri source has to be escaped.
fn string_contents(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('{', "\\{")
        .replace('}', "\\}")
}

fn folder(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "colibri-process-options-{}-{name}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Runs `parent` (Colibri source) with `colibri run` and gives its standard output with line
/// ends normalised.
fn run_script(dir: &Path, parent: &str) -> String {
    let file = dir.join("parent.colibri");
    fs::write(&file, parent).unwrap();
    let out = common::checked(
        colibri_command()
            .args(["run", file.to_str().unwrap()])
            .output()
            .unwrap(),
    );
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n")
}

#[test]
fn arguments_reach_the_program_exactly_as_given() {
    let dir = folder("args");
    let child = dir.join("child.colibri");
    fs::write(
        &child,
        "import os\n\nfn main() {\n    for a in os.args() {\n        print(\"[{a}]\")\n    }\n}\n",
    )
    .unwrap();
    let parent = format!(
        r#"import process
import io

fn main() {{
    let args = [
        "run", "{child}", "--",
        "a b", "", "%X%", "$HOME", "say \"hi\"", "trailing\\", "a&b", "x | y > z",
        "quote\\\"back", "two  spaces", "C:\\dir with space\\", "tab\there",
    ]
    let r = process.run("{exe}", args) or |e| {{ panic(e.message()) }}
    print(r.status)
    print(r.stderr.len())
    io.write(r.stdout)
}}
"#,
        child = string_contents(&child),
        exe = string_contents(Path::new(env!("CARGO_BIN_EXE_colibri"))),
    );
    let got = run_script(&dir, &parent);
    let expected = [
        "0",
        "0",
        "[a b]",
        "[]",
        "[%X%]",
        "[$HOME]",
        "[say \"hi\"]",
        "[trailing\\]",
        "[a&b]",
        "[x | y > z]",
        "[quote\\\"back]",
        "[two  spaces]",
        "[C:\\dir with space\\]",
        "[tab\there]",
        "",
    ]
    .join("\n");
    assert_eq!(got, expected);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn cwd_and_input_options_reach_the_program() {
    let dir = folder("options");
    let work = dir.join("work folder");
    fs::create_dir_all(&work).unwrap();
    fs::write(work.join("found-here.txt"), "x").unwrap();
    let child = dir.join("child.colibri");
    fs::write(
        &child,
        r#"import os
import io
import fs
import path

fn main() {
    let cwd = os.cwd() or |e| { panic(e.message()) }
    print("cwd: {path.file_name(cwd) or "none"}")
    print("file: {fs.exists("found-here.txt")}")
    let text = io.read_all() or |e| { panic(e.message()) }
    print("input: {text.len()} bytes")
    io.write(text)
}
"#,
    )
    .unwrap();
    let parent = format!(
        r#"import process
import io

fn main() {{
    let mut o = process.options()
    o.cwd = some("{work}")
    o.input = some("first line\nsecond line\n")
    let r = process.run_with("{exe}", ["run", "{child}"], o) or |e| {{ panic(e.message()) }}
    print(r.status)
    print(r.stderr.len())
    io.write(r.stdout)
}}
"#,
        work = string_contents(&work),
        child = string_contents(&child),
        exe = string_contents(Path::new(env!("CARGO_BIN_EXE_colibri"))),
    );
    let got = run_script(&dir, &parent);
    assert_eq!(
        got,
        "0\n0\ncwd: work folder\nfile: true\ninput: 23 bytes\nfirst line\nsecond line\n"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// The binary under test. It inherits `COLIBRI_ENGINE`, so these tests also run the programs they
/// spawn on the VM (see `vm_pending.txt` for the ones that cannot pass there yet).
fn colibri_command() -> std::process::Command {
    std::process::Command::new(env!("CARGO_BIN_EXE_colibri"))
}
