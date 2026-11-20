//! The tests of the `httpd` source module that are written in Colibri (`tests/httpd/*.colibri`).
//!
//! `colibri test` does not collect `test` blocks from a standard source module, and the parser's
//! functions are private to the module, so each file is run in a package of its own: a temporary
//! folder with a `colibri.toml`, an empty `main.colibri`, the text of the `httpd` module (from
//! `colibri_std::sources()`, so it is the very text that ships) under the name `httpd_impl`, the
//! helpers of `common.colibri` and the test file. Package-private items are visible across the
//! files of a package, so the tests call the module's own functions. `Vm::load_tests` and
//! `Vm::run_test` run them. Nothing here writes a path into Colibri source: the folder is only
//! ever handed to the loader as a path.
//!
//! One Rust test runs the tests of one file. A second kind of entry, `expect_failure`, runs the
//! files of `tests/httpd/failing/`: there every `test` must panic, with the text its name gives
//! after `=>`, because `colibri test` has no way to say that a test should panic (the router
//! refuses a malformed pattern by panicking).

use std::path::PathBuf;
use std::time::Duration;

use colibri_runtime as colibri;

/// A folder of its own, removed afterwards.
struct Package(PathBuf);

impl Package {
    /// `name` is unique among the tests that run together (one folder for each Rust test).
    fn new(name: &str, files: &[(&str, &str)]) -> Package {
        let dir = std::env::temp_dir().join(format!("colibri-httpd-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let httpd = colibri_std::sources()
            .into_iter()
            .find(|s| s.name == "httpd")
            .expect("httpd is a standard source module");
        let mut all = vec![
            ("colibri.toml", "[package]\nname = \"httpd_tests\"\n"),
            ("main.colibri", "fn main() {}\n"),
            ("httpd_impl.colibri", httpd.text),
            ("common.colibri", include_str!("httpd/common.colibri")),
        ];
        all.extend_from_slice(files);
        for (file, text) in all {
            std::fs::write(dir.join(file), text).unwrap();
        }
        Package(dir)
    }

    /// A `Vm` with the standard library and the package loaded for its tests.
    fn load(&self) -> colibri::Vm {
        let mut vm = colibri::Vm::builder()
            .stdout(Box::new(colibri::SharedBuffer::new()))
            .build();
        colibri_std::install(&mut vm).expect("the standard library installs");
        if let Err(e) = vm.load_tests(&self.0) {
            panic!("the package does not load:\n{}", e.render(false));
        }
        vm
    }
}

impl Drop for Package {
    fn drop(&mut self) {
        // Windows refuses to remove a folder with a file that is still open; try again briefly.
        for _ in 0..50 {
            if std::fs::remove_dir_all(&self.0).is_ok() || !self.0.exists() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// The number of `test "..." {` blocks the text of a file has, to see that every one was found.
fn declared_tests(text: &str) -> usize {
    text.lines().filter(|l| l.starts_with("test \"")).count()
}

/// Runs every test of a file, which must all pass.
fn run_file(name: &str, text: &str) {
    let package = Package::new(name, &[(&format!("{name}.colibri"), text)]);
    let mut vm = package.load();
    let tests: Vec<colibri::TestInfo> = vm.tests().into_iter().filter(|t| t.root).collect();
    assert_eq!(
        tests.len(),
        declared_tests(text),
        "{name}.colibri: a test was not found"
    );
    assert!(!tests.is_empty(), "{name}.colibri has no tests");
    let mut failures = Vec::new();
    for test in &tests {
        if let Err(e) = vm.run_test(test.id) {
            failures.push(format!(
                "---- {} ({}:{}) ----\n{}",
                test.name,
                test.file,
                test.line,
                e.render(false)
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} tests of {name}.colibri failed:\n{}",
        failures.len(),
        tests.len(),
        failures.join("\n")
    );
}

/// Runs the tests of a file of `tests/httpd/failing/`: each must panic, and the panic's message
/// must hold the text after ` => ` in the test's name.
fn expect_failure(name: &str, text: &str) {
    let package = Package::new(&format!("{name}-failing"), &[(&format!("{name}.colibri"), text)]);
    let mut vm = package.load();
    let tests: Vec<colibri::TestInfo> = vm.tests().into_iter().filter(|t| t.root).collect();
    assert_eq!(
        tests.len(),
        declared_tests(text),
        "{name}.colibri: a test was not found"
    );
    assert!(!tests.is_empty(), "{name}.colibri has no tests");
    let mut wrong = Vec::new();
    for test in &tests {
        let (title, expected) = test
            .name
            .split_once(" => ")
            .unwrap_or_else(|| panic!("the test `{}` does not say `=> text`", test.name));
        match vm.run_test(test.id) {
            Ok(()) => wrong.push(format!("{title}: it did not panic")),
            Err(colibri::Error::Panic { message, .. }) if message.contains(expected) => {}
            Err(colibri::Error::Panic { message, .. }) => wrong.push(format!(
                "{title}: it panicked with `{message}`, not with `{expected}`"
            )),
            Err(other) => wrong.push(format!(
                "{title}: it failed, but not by a panic:\n{}",
                other.render(false)
            )),
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

macro_rules! suite {
    ($($name:ident),* $(,)?) => {
        $(
            #[test]
            fn $name() {
                run_file(
                    stringify!($name),
                    include_str!(concat!("httpd/", stringify!($name), ".colibri")),
                );
            }
        )*
    };
}

suite!(request_line, fields, host, framing, chunk);

macro_rules! failing {
    ($($name:ident),* $(,)?) => {
        $(
            #[test]
            fn $name() {
                expect_failure(
                    stringify!($name),
                    include_str!(concat!("httpd/failing/", stringify!($name), ".colibri")),
                );
            }
        )*
    };
}

failing!();
