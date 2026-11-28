//! `colibri run --watch` in a package with Rust bindings: the supervisor keeps the runner running,
//! Colibri edits are patched in place inside it, a Rust edit builds again (fake cargo) and restarts
//! it, a build that fails leaves the old one running. Every wait polls with a generous deadline;
//! nothing here asserts on a fixed sleep.

mod common;

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use common::*;

const CRATE_TOML: &str = "[package]\nname = \"colibri-fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\ncolibri-runtime = \"0.1\"\n";
#[derive(Clone, Copy, PartialEq)]
enum Layout {
    Native,
    Inside,
    Same,
}

const PATH_BOUND: &str =
    "[package]\nname = \"app\"\n\n[rust-dependencies]\nfixture = { path = \"native/\" }\n";

/// A program with state (`ticks`) that goes on after a patch, and a call into the binding.
fn program(tag: &str) -> String {
    format!(
        "import fixture\n\nlet mut ticks = 0\n\nfn msg() -> str => \"{tag}\"\n\nfn ticker() {{\n    while true {{\n        ticks = ticks + 1\n        let d = fixture.double(21)\n        print(\"{{msg()}} {{d}} {{ticks}}\")\n        wait_real(0.05)\n    }}\n}}\n\nfn main() {{\n    print(\"main\")\n    go ticker()\n}}\n"
    )
}

fn pump(reader: impl std::io::Read + Send + 'static) -> Receiver<String> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(reader).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    rx
}

struct Watch {
    w: World,
    dir: PathBuf,
    rust_file: PathBuf,
    control: PathBuf,
    child: Child,
    out: Receiver<String>,
    err: Receiver<String>,
    seen_out: Vec<String>,
    seen_err: Vec<String>,
}

impl Watch {
    fn start(name: &str, main: &str) -> Watch {
        Watch::start_with(name, main, Layout::Native, false)
    }

    /// With `Layout::Inside` the package is a folder of the binding crate (`path = ".."`), with
    /// `Layout::Same` it is the crate folder itself (`path = "."`); with `grouped` the command is the leader of a process group of its own (so a test can interrupt
    /// the group the way a terminal does).
    fn start_with(name: &str, main: &str, layout: Layout, grouped: bool) -> Watch {
        let w = World::new(name);
        let (dir, rust_file) = if layout == Layout::Same {
            let krate = w.root.join("crate");
            fs::create_dir_all(krate.join("src")).unwrap();
            fs::write(krate.join("Cargo.toml"), CRATE_TOML).unwrap();
            fs::write(
                krate.join("colibri.toml"),
                PATH_BOUND.replace("native/", "."),
            )
            .unwrap();
            (krate.clone(), krate.join("src/lib.rs"))
        } else if layout == Layout::Inside {
            let krate = w.root.join("crate");
            let dir = krate.join("app");
            fs::create_dir_all(krate.join("src")).unwrap();
            fs::create_dir_all(&dir).unwrap();
            fs::write(krate.join("Cargo.toml"), CRATE_TOML).unwrap();
            fs::write(
                dir.join("colibri.toml"),
                PATH_BOUND.replace("native/", ".."),
            )
            .unwrap();
            (dir, krate.join("src/lib.rs"))
        } else {
            let dir = w.root.join("app");
            fs::create_dir_all(dir.join("native/src")).unwrap();
            fs::write(dir.join("colibri.toml"), PATH_BOUND).unwrap();
            fs::write(dir.join("native/Cargo.toml"), CRATE_TOML).unwrap();
            (dir.clone(), dir.join("native/src/lib.rs"))
        };
        fs::write(&rust_file, "// version 1\n").unwrap();
        fs::write(dir.join("main.colibri"), main).unwrap();
        let out = std::process::Command::new(RUNNER)
            .arg("__write-interfaces")
            .arg(&dir)
            .output()
            .unwrap();
        assert!(out.status.success());
        let control = w.root.join("control");
        fs::write(&control, format!("runner={RUNNER}\n")).unwrap();
        let mut command = w.command(UNDER_TEST, &dir, &["run", "--watch", "main.colibri"]);
        command
            .env("FAKE_CARGO_CONTROL", &control)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        if grouped {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let _ = grouped;
        let mut child = command.spawn().unwrap();
        let out = pump(child.stdout.take().unwrap());
        let err = pump(child.stderr.take().unwrap());
        Watch {
            w,
            dir,
            rust_file,
            control,
            child,
            out,
            err,
            seen_out: Vec::new(),
            seen_err: Vec::new(),
        }
    }

    fn drain(&mut self) {
        while let Ok(l) = self.out.try_recv() {
            self.seen_out.push(l);
        }
        while let Ok(l) = self.err.try_recv() {
            self.seen_err.push(l);
        }
    }

    /// Waits until `done` holds for what has been seen (stdout, stderr).
    fn until(&mut self, what: &str, done: impl Fn(&[String], &[String]) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            self.drain();
            if done(&self.seen_out, &self.seen_err) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}; stdout {:?}, stderr {:?}",
                self.seen_out.iter().rev().take(8).collect::<Vec<_>>(),
                self.seen_err
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn out_has(&mut self, needle: &str) {
        self.until(needle, |o, _| o.iter().any(|l| l.contains(needle)));
    }

    fn err_has(&mut self, needle: &str) {
        self.until(needle, |_, e| e.iter().any(|l| l.contains(needle)));
    }

    fn edit_rust(&self, n: u32) {
        edit(&self.rust_file, &format!("// version {n}\n"));
    }

    fn builds(&self) -> usize {
        self.w.cargo_log().len()
    }

    fn alive(&mut self) -> bool {
        self.child.try_wait().unwrap().is_none()
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// An edit the way an editor saves: the new text is put in place whole, so a watcher never reads
/// half of it.
fn edit(path: &Path, text: &str) {
    let tmp = path.with_extension("saving");
    fs::write(&tmp, text).unwrap();
    fs::rename(&tmp, path).unwrap();
}

fn counter_of(line: &str) -> Option<u32> {
    line.split(' ').nth(2)?.parse().ok()
}

#[test]
fn a_colibri_edit_is_patched_in_place_and_state_is_kept() {
    let mut w = Watch::start("colibri-edit", &program("one"));
    w.out_has("one 42");
    edit(&w.dir.join("main.colibri"), &program("two"));
    w.err_has("patched 1 function");
    w.out_has("two 42");
    // Let it run on for a while (the supervisor looks at the Rust code every half second): by
    // the counter, not the clock. A restart would print `main` again and begin the counter at 1.
    let first = w
        .seen_out
        .iter()
        .find(|l| l.starts_with("two "))
        .and_then(|l| counter_of(l))
        .unwrap();
    w.until("the program to go on", |o, _| {
        o.iter()
            .filter(|l| l.starts_with("two "))
            .filter_map(|l| counter_of(l))
            .max()
            .is_some_and(|n| n >= first + 40)
    });
    let out = w.seen_out.clone();
    let counters: Vec<u32> = out
        .iter()
        .filter(|l| l.contains(" 42 "))
        .filter_map(|l| counter_of(l))
        .collect();
    assert!(
        counters.windows(2).all(|p| p[0] < p[1]),
        "restarted: {out:?}"
    );
    assert_eq!(out.iter().filter(|l| *l == "main").count(), 1, "{out:?}");
    // The counter went on across the patch: it is not 1 at the first "two".
    let last_one = out
        .iter()
        .filter(|l| l.starts_with("one "))
        .filter_map(|l| counter_of(l))
        .max()
        .unwrap();
    let first_two = out
        .iter()
        .find(|l| l.starts_with("two "))
        .and_then(|l| counter_of(l))
        .unwrap();
    assert!(first_two > last_one, "state lost: {out:?}");
    assert_eq!(w.builds(), 1, "a Colibri edit builds nothing");
}

#[test]
fn a_rust_edit_builds_again_and_restarts_with_the_new_behaviour() {
    let mut w = Watch::start("rust-edit", &program("one"));
    w.out_has("one 42");
    assert_eq!(w.builds(), 1);
    fs::write(&w.control, format!("runner={RUNNER_V3}\n")).unwrap();
    w.edit_rust(2);
    w.err_has("Rust code changed: rebuilt the runner and restarted");
    w.out_has("one 63");
    // The build line says "again" the second time.
    assert!(
        w.seen_err.iter().any(|l| l.contains("project again")),
        "{:?}",
        w.seen_err
    );
    assert_eq!(w.builds(), 2, "{:?}", w.w.cargo_log());
    let out = w.seen_out.clone();
    assert_eq!(out.iter().filter(|l| *l == "main").count(), 2, "{out:?}");
    // The program restarted: its counter began again.
    let after = out.iter().position(|l| l.starts_with("one 63")).unwrap();
    assert_eq!(counter_of(&out[after]), Some(1), "{out:?}");
    // An edit of the manifest's binding list builds again too.
    assert!(w.alive());
}

#[test]
fn a_build_that_fails_keeps_the_old_runner_going_until_the_next_edit() {
    let mut w = Watch::start("rust-fail", &program("one"));
    w.out_has("one 42");
    fs::write(&w.control, format!("runner={RUNNER_V3}\nfail=build\n")).unwrap();
    w.edit_rust(2);
    w.err_has("the Rust code does not build; the old runner keeps running");
    w.err_has("could not compile");
    // Still running, and still printing the old behaviour: wait for fresh lines.
    let seen = w
        .seen_out
        .iter()
        .filter(|l| l.starts_with("one 42"))
        .count();
    w.until("more output from the old runner", |o, _| {
        o.iter().filter(|l| l.starts_with("one 42")).count() >= seen + 3
    });
    assert!(
        w.seen_out.iter().all(|l| !l.contains("63")),
        "{:?}",
        w.seen_out
    );
    assert!(w.alive());
    assert_eq!(w.builds(), 2);
    // The next edit tries again, and this time it builds.
    fs::write(&w.control, format!("runner={RUNNER_V3}\n")).unwrap();
    w.edit_rust(3);
    w.err_has("rebuilt the runner and restarted");
    w.out_has("one 63");
    assert_eq!(w.builds(), 3);
}

#[test]
fn the_exit_code_of_the_program_is_the_exit_code_of_the_command() {
    let main = "import fixture\nimport os\n\nfn main() {\n    print(fixture.double(21))\n    print(os.env(\"COLIBRI_SUPERVISOR_FD\"))\n    os.exit(7)\n}\n";
    let mut w = Watch::start("exit", main);
    w.out_has("42");
    // The runner keeps its supervisor's descriptor to itself.
    w.out_has("none");
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(s) = w.child.try_wait().unwrap() {
            break s;
        }
        assert!(Instant::now() < deadline, "the command did not end");
        thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(status.code(), Some(7));
}

/// The supervisor is killed without a chance to clean up: the runner must not outlive it.
#[cfg(unix)]
#[test]
fn a_runner_does_not_outlive_its_supervisor() {
    let mut w = Watch::start("orphan", &program("one"));
    w.out_has("one 42");
    let parent = w.child.id().to_string();
    let children = std::process::Command::new("pgrep")
        .args(["-P", &parent])
        .output()
        .unwrap();
    let runner: Vec<String> = String::from_utf8_lossy(&children.stdout)
        .split_whitespace()
        .map(str::to_string)
        .collect();
    assert_eq!(runner.len(), 1, "{runner:?}");
    let alive = |pid: &str| {
        std::process::Command::new("kill")
            .args(["-0", pid])
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success()
    };
    assert!(alive(&runner[0]));
    w.child.kill().unwrap();
    let _ = w.child.wait();
    let deadline = Instant::now() + Duration::from_secs(60);
    while alive(&runner[0]) {
        assert!(Instant::now() < deadline, "the runner is still there");
        thread::sleep(Duration::from_millis(20));
    }
}

/// The package is a folder of the binding crate: its Colibri files are not Rust inputs.
#[test]
fn a_colibri_edit_in_a_package_inside_the_crate_folder_builds_nothing() {
    let mut w = Watch::start_with("inside", &program("one"), Layout::Inside, false);
    w.out_has("one 42");
    edit(&w.dir.join("main.colibri"), &program("two"));
    w.err_has("patched 1 function");
    w.out_has("two 42");
    let first = w
        .seen_out
        .iter()
        .find(|l| l.starts_with("two "))
        .and_then(|l| counter_of(l))
        .unwrap();
    // Longer than a look at the Rust code takes (the counter is the clock).
    w.until("the program to go on", |o, _| {
        o.iter()
            .filter(|l| l.starts_with("two "))
            .filter_map(|l| counter_of(l))
            .max()
            .is_some_and(|n| n >= first + 40)
    });
    assert_eq!(w.builds(), 1, "{:?}", w.w.cargo_log());
    assert_eq!(w.seen_out.iter().filter(|l| *l == "main").count(), 1);
    assert!(!w.seen_err.iter().any(|l| l.contains("Rust code changed")));
}

/// SIGINT to the supervisor alone (an editor's stop button, `timeout -s INT`): the runner gets it
/// and the command ends with the status of an interrupted program.
#[cfg(unix)]
#[test]
fn an_interrupt_sent_to_the_supervisor_alone_ends_the_runner_and_the_command() {
    let mut w = Watch::start("sigint", &program("one"));
    w.out_has("one 42");
    let pid = w.child.id().to_string();
    assert!(
        std::process::Command::new("kill")
            .args(["-INT", &pid])
            .status()
            .unwrap()
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(s) = w.child.try_wait().unwrap() {
            break s;
        }
        assert!(Instant::now() < deadline, "the command did not end");
        thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(status.code(), Some(130));
}

/// Ctrl-C (SIGINT to the whole group) while the runner is being built: the command ends, and says
/// nothing false about the build.
#[cfg(unix)]
#[test]
fn an_interrupt_during_a_rebuild_ends_quietly() {
    let mut w = Watch::start_with("sigint-build", &program("one"), Layout::Native, true);
    w.out_has("one 42");
    fs::write(&w.control, format!("runner={RUNNER_V3}\nsleep_ms=20000\n")).unwrap();
    w.edit_rust(2);
    w.err_has("building the Rust bindings of this project again");
    let group = format!("-{}", w.child.id());
    assert!(
        std::process::Command::new("kill")
            .args(["-INT", "--", &group])
            .status()
            .unwrap()
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(s) = w.child.try_wait().unwrap() {
            break s;
        }
        assert!(Instant::now() < deadline, "the command did not end");
        thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(status.code(), Some(130));
    w.drain();
    assert!(
        !w.seen_err.iter().any(|l| l.contains("does not build")),
        "{:?}",
        w.seen_err
    );
}

/// Edits of `main.colibri` and of a data file next to it, then a wait longer than a look at the
/// Rust code takes; nothing may build.
fn nothing_builds_for(w: &mut Watch, file: &Path, text: &str, tag: &str) {
    w.out_has("one 42");
    edit(file, text);
    if tag == "two" {
        w.out_has("two 42");
    }
    let before = w
        .seen_out
        .iter()
        .filter_map(|l| counter_of(l))
        .max()
        .unwrap_or(0);
    w.until("the program to go on", |o, _| {
        o.iter()
            .filter_map(|l| counter_of(l))
            .max()
            .is_some_and(|n| n >= before + 40)
    });
    assert_eq!(w.builds(), 1, "{:?}", w.w.cargo_log());
    assert!(!w.seen_err.iter().any(|l| l.contains("Rust code changed")));
}

/// `path = "."`: the package folder is the crate folder, so its Colibri files (and the manifest)
/// sit in the walked tree; they are skipped by name.
#[test]
fn a_colibri_edit_in_the_crate_folder_itself_builds_nothing() {
    let mut w = Watch::start_with("same", &program("one"), Layout::Same, false);
    let file = w.dir.join("main.colibri");
    nothing_builds_for(&mut w, &file, &program("two"), "two");
}

/// A file that is not Rust and not Colibri, in the package folder inside the crate folder: the
/// folder holds a `colibri.toml`, so the walk does not enter it.
#[test]
fn an_asset_in_a_package_inside_the_crate_folder_builds_nothing() {
    let mut w = Watch::start_with("asset", &program("one"), Layout::Inside, false);
    let file = w.dir.join("data.txt");
    fs::write(&file, "1").unwrap();
    nothing_builds_for(&mut w, &file, "22222", "one");
}

/// The next Rust edit is noticed soon after a slow build: the wait between looks follows what a
/// look costs, not what the build took.
#[test]
fn a_rust_edit_after_a_slow_build_is_noticed_soon() {
    for fail in [false, true] {
        let mut w = Watch::start(if fail { "slow-fail" } else { "slow-ok" }, &program("one"));
        w.out_has("one 42");
        let slow = if fail { "fail=build\n" } else { "" };
        fs::write(
            &w.control,
            format!("runner={RUNNER_V3}\n{slow}sleep_ms=2000\n"),
        )
        .unwrap();
        w.edit_rust(2);
        if fail {
            w.err_has("the Rust code does not build");
        } else {
            w.err_has("rebuilt the runner and restarted");
            w.out_has("one 63");
        }
        fs::write(&w.control, format!("runner={RUNNER}\n")).unwrap();
        let edited = Instant::now();
        w.edit_rust(3);
        w.until("the second edit to be built", |_, e| {
            e.iter()
                .filter(|l| l.contains("rebuilt the runner and restarted"))
                .count()
                >= if fail { 1 } else { 2 }
        });
        assert!(
            edited.elapsed() < Duration::from_secs(10),
            "noticed after {:?}",
            edited.elapsed()
        );
    }
}
