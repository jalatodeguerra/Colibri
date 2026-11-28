//! What the tests of delegation share: a world of packages, the program under test and the fake cargo.
#![allow(dead_code)]

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub const UNDER_TEST: &str = env!("CARGO_BIN_EXE_colibri-under-test");
pub const RUNNER: &str = env!("CARGO_BIN_EXE_colibri-runner-fixture");
pub const RUNNER_V3: &str = env!("CARGO_BIN_EXE_colibri-runner-fixture-v3");
pub const RUNNER_V2: &str = env!("CARGO_BIN_EXE_colibri-runner-fixture-v2");
pub const FAKE_CARGO: &str = env!("CARGO_BIN_EXE_fake-cargo");

/// Arguments, standard input, exit codes: what a program shows of its host.
pub const PROGRAM: &str = "import os\nimport io\n\nfn main() {\n    print(os.args())\n    print(io.read_line())\n    io.eprint(\"to stderr\\n\")\n    let a = os.args()\n    if a.len() > 0 {\n        if a[0] == \"one\" {\n            os.exit(1)\n        }\n        if a[0] == \"panic\" {\n            let v = [1]\n            print(v[3])\n        }\n    }\n}\n";

pub const BOUND: &str = "[package]\nname = \"app\"\n\n[rust-dependencies]\nfixture = { binding = \"colibri-fixture\", version = \"0.1\" }\n";
pub const OTHER: &str = "[package]\nname = \"app\"\n\n[rust-dependencies]\nfixture = { binding = \"colibri-fixture\", version = \"0.2\" }\n";
pub const PLAIN: &str = "[package]\nname = \"app\"\n";

pub struct World {
    pub root: PathBuf,
}

pub struct Out {
    pub stdout: String,
    pub stderr: String,
    pub code: i32,
}

impl World {
    pub fn new(name: &str) -> World {
        let root =
            std::env::temp_dir().join(format!("colibri-delegate-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        // (macOS and Windows reach a temp folder through a link or a short name.)
        let root = fs::canonicalize(&root).unwrap();
        let root = PathBuf::from(
            colibri_lang::bindings::r#gen::strip_verbatim(&root.to_string_lossy()).to_string(),
        );
        World { root }
    }

    /// A package folder with `manifest` and the test program.
    pub fn package(&self, name: &str, manifest: &str) -> PathBuf {
        let dir = self.root.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("colibri.toml"), manifest).unwrap();
        fs::write(dir.join("main.colibri"), PROGRAM).unwrap();
        // The interface the checker reads, from the runner itself, as `colibri bindings` does.
        if manifest.contains("rust-dependencies") {
            let out = Command::new(RUNNER)
                .arg("__write-interfaces")
                .arg(&dir)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        dir
    }

    pub fn cargo_log(&self) -> Vec<String> {
        fs::read_to_string(self.root.join("cargo.log"))
            .map(|t| t.lines().map(str::to_string).collect())
            .unwrap_or_default()
    }

    pub fn command(&self, exe: &str, dir: &Path, args: &[&str]) -> Command {
        let mut c = Command::new(exe);
        c.args(args)
            .current_dir(dir)
            .env("COLIBRI_HOME", self.root.join("home"))
            .env("CARGO", FAKE_CARGO)
            .env("FAKE_CARGO_LOG", self.root.join("cargo.log"))
            .env("FAKE_CARGO_RUNNER", RUNNER)
            .env_remove("COLIBRI_PROJECT_RUNNER");
        c
    }

    pub fn run_with(&self, exe: &str, dir: &Path, args: &[&str], stdin: &str) -> Out {
        let mut child = self
            .command(exe, dir, args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        Out {
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            code: out.status.code().unwrap_or(-1),
        }
    }

    /// `colibri <args>` as the user types it, in `dir`.
    pub fn colibri(&self, dir: &Path, args: &[&str], stdin: &str) -> Out {
        self.run_with(UNDER_TEST, dir, args, stdin)
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
