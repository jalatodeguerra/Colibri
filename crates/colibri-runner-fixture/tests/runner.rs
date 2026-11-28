//! The fixture runner (`runner_main` with a test binding): a program that imports the binding
//! runs, in the main `Vm` and in a thread task, and the hidden `__write-interfaces` command
//! writes the interface `colibri check` needs. No cargo is involved.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

struct Out {
    stdout: String,
    stderr: String,
    code: i32,
}

/// A package folder that declares the `fixture` binding, removed at the end.
struct Pkg(PathBuf);

impl Pkg {
    fn new(name: &str, main: &str) -> Pkg {
        let dir =
            std::env::temp_dir().join(format!("colibri-fixture-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("colibri.toml"),
            "[package]\nname = \"app\"\n\n[rust-dependencies]\nfixture = { binding = \"colibri-fixture\", version = \"0.1\" }\n",
        )
        .unwrap();
        fs::write(dir.join("main.colibri"), main).unwrap();
        let pkg = Pkg(dir);
        // The interface comes from the runner itself, as `colibri bindings` will do it.
        let out = runner(&["__write-interfaces", pkg.0.to_str().unwrap()]);
        assert_eq!(out.code, 0, "{}", out.stderr);
        pkg
    }

    fn run(&self) -> Out {
        runner(&["run", self.0.to_str().unwrap()])
    }
}

impl Drop for Pkg {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn runner(args: &[&str]) -> Out {
    // An empty `PATH` and a dead `CARGO`: a runner that tried to build anything would fail.
    let out = Command::new(env!("CARGO_BIN_EXE_colibri-runner-fixture"))
        .args(args)
        .env("PATH", "")
        .env("CARGO", "colibri-no-such-cargo")
        .output()
        .expect("the fixture runner starts");
    Out {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        code: out.status.code().unwrap_or(-1),
    }
}

fn program(name: &str) -> (String, String) {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/programs");
    (
        fs::read_to_string(dir.join(format!("{name}.colibri"))).unwrap(),
        fs::read_to_string(dir.join(format!("{name}.out"))).unwrap(),
    )
}

#[test]
fn a_program_imports_its_binding() {
    let (source, expected) = program("imports_the_binding");
    let out = Pkg::new("imports", &source).run();
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(out.stdout, expected);
}

#[test]
fn a_thread_task_calls_the_binding() {
    let (source, expected) = program("task_calls_the_binding");
    let out = Pkg::new("task", &source).run();
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(out.stdout, expected);
}

#[test]
fn the_written_interface_names_the_module_and_its_members() {
    let pkg = Pkg::new("cbi", "fn main() {\n}\n");
    let text = fs::read_to_string(pkg.0.join("fixture.cbi")).unwrap();
    assert!(text.contains("module = \"fixture\""), "{text}");
    for member in ["fn double", "fn shout", "Counter"] {
        assert!(text.contains(member), "{member}: {text}");
    }
}

#[test]
fn test_blocks_see_the_binding_too() {
    let pkg = Pkg::new(
        "tests",
        "import fixture\n\ntest \"doubles\" {\n    assert(fixture.double(4) == 8)\n}\n",
    );
    let out = runner(&["test", pkg.0.to_str().unwrap()]);
    assert_eq!(out.code, 0, "{}{}", out.stdout, out.stderr);
    assert!(out.stdout.contains("1 passed"), "{}", out.stdout);
}

#[test]
fn the_runner_has_no_check_or_build() {
    for verb in ["check", "build", "lsp", "fmt"] {
        let out = runner(&[verb]);
        assert_ne!(out.code, 0, "{verb}");
    }
}
