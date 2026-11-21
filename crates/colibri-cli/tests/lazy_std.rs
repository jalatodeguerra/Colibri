//! `colibri run` and `colibri check` touch only the standard modules a program imports.

use std::path::PathBuf;

use colibri_lang::depscmd::DepFlags;

fn temp_file(name: &str, source: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("colibri-lazy-std-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join(name);
    std::fs::write(&path, source).expect("temp file");
    path
}

fn run_registered(name: &str, source: &str) -> Vec<String> {
    let path = temp_file(name, source);
    // About which modules get registered, not the engine: pinned to the tree-walker.
    let builder = colibri_lang::vm_builder()
        .engine(colibri_runtime::EngineKind::Tree)
        .allow_all()
        .stdout(Box::new(std::io::sink()));
    let mut vm = colibri_lang::build_vm(builder, Vec::new());
    let code = colibri_lang::runcmd::run_in(&mut vm, &path, None);
    assert_eq!(code, std::process::ExitCode::SUCCESS);
    vm.registered_std_modules()
}

#[test]
fn run_of_a_program_importing_nothing_registers_no_std_module() {
    let registered = run_registered("hello.colibri", "fn main() {\n    print(\"hi\")\n}\n");
    assert_eq!(registered, Vec::<String>::new());
}

#[test]
fn run_registers_the_imported_std_modules_only() {
    let registered = run_registered(
        "run_imports_math.colibri",
        "import math\nfn main() {\n    print(math.floor(1.5))\n}\n",
    );
    assert_eq!(registered, ["std.math"]);
}

fn checked_interfaces(name: &str, source: &str) -> (bool, Vec<String>) {
    let path = temp_file(name, source);
    let mut asked = Vec::new();
    let result =
        colibri_lang::checkcmd::check_with(&path, &[], true, DepFlags::default(), &mut |module| {
            asked.push(module.to_string());
            colibri_std::interface(module)
        });
    (result.is_ok(), asked)
}

#[test]
fn check_builds_the_interfaces_of_the_imported_std_modules_only() {
    let (ok, asked) = checked_interfaces(
        "check_imports_math.colibri",
        "import math\nfn main() {\n    print(math.floor(1.5))\n}\n",
    );
    assert!(ok);
    assert_eq!(asked, ["std.math"]);
}

#[test]
fn check_of_a_program_importing_nothing_builds_no_interface() {
    let (ok, asked) = checked_interfaces("check_none.colibri", "fn main() {\n    print(1)\n}\n");
    assert!(ok);
    assert!(asked.is_empty(), "{asked:?}");
}

/// The warnings of a program that checked are on stderr before any of its code runs: the
/// script blocks reading stdin at top level, and the test reads the warning first.
#[test]
fn run_prints_warnings_before_the_top_level_code_runs() {
    use std::io::{BufRead, BufReader, Write};
    use std::process::Stdio;

    let path = temp_file(
        "warn_first.colibri",
        "import io\nfn f() {\n    let unused = 1\n}\nf()\nlet line = io.read_line()\nprint(\"got\")\n",
    );
    let mut child = colibri_command()
        .args(["run", path.to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the colibri binary runs");
    let mut stdin = child.stdin.take().unwrap();
    let stderr = BufReader::new(child.stderr.take().unwrap());
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in stderr.lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    // Nothing has been written to the script's stdin yet, so it is blocked (or not started).
    let mut seen = String::new();
    loop {
        let line = rx
            .recv_timeout(std::time::Duration::from_secs(30))
            .unwrap_or_else(|_| {
                let _ = child.kill();
                panic!("no warning before the script read stdin: {seen}")
            });
        seen.push_str(&line);
        if line.contains("unused variable") {
            break;
        }
    }
    stdin.write_all(b"x\n").unwrap();
    drop(stdin);
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{seen}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "got\n");
}

#[test]
fn importing_toml_registers_json_too_and_importing_json_does_not_register_toml() {
    let toml = run_registered(
        "run_imports_toml.colibri",
        "import toml\nfn main() {\n    print(toml.parse(\"a = 1\").expect(\"p\").kind())\n}\n",
    );
    let mut toml = toml;
    toml.sort();
    assert_eq!(toml, ["std.json", "std.toml"]);
    let json = run_registered(
        "run_imports_json.colibri",
        "import json\nfn main() {\n    print(json.parse(\"1\").expect(\"p\").kind())\n}\n",
    );
    assert_eq!(json, ["std.json"]);
}

#[test]
fn check_of_toml_gets_the_json_interface_it_names_and_json_alone_never_asks_for_toml() {
    let (ok, mut asked) = checked_interfaces(
        "check_imports_toml.colibri",
        "import toml\nfn main() {\n    print(toml.parse(\"a = 1\").expect(\"p\").kind())\n}\n",
    );
    assert!(ok);
    asked.sort();
    assert_eq!(asked, ["std.json", "std.toml"]);
    let (ok, asked) = checked_interfaces(
        "check_imports_json.colibri",
        "import json\nfn main() {\n    print(json.parse(\"1\").expect(\"p\").kind())\n}\n",
    );
    assert!(ok);
    assert_eq!(asked, ["std.json"]);
}

/// The binary under test, pinned to the tree-walker: these tests are about the command line,
/// not the engine, and `COLIBRI_ENGINE=vm` would otherwise reach every process they spawn.
fn colibri_command() -> std::process::Command {
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_colibri"));
    cmd.env_remove("COLIBRI_ENGINE");
    cmd
}
