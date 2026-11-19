//! End-to-end tests of `colibri run --watch`: spawn the binary, edit the file, read what it
//! prints. Every wait has a generous timeout and polls, so a slow machine only makes the tests
//! slower, not flaky.

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

struct Watch {
    child: Child,
    out: Receiver<String>,
    err: Receiver<String>,
    seen_out: Vec<String>,
    seen_err: Vec<String>,
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

impl Watch {
    fn start(path: &Path) -> Watch {
        let mut child = Command::new(env!("CARGO_BIN_EXE_colibri"))
            .args(["run", "--watch", path.to_str().unwrap()])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("the colibri binary runs");
        let out = pump(child.stdout.take().unwrap());
        let err = pump(child.stderr.take().unwrap());
        Watch {
            child,
            out,
            err,
            seen_out: Vec::new(),
            seen_err: Vec::new(),
        }
    }

    /// Waits until a line of stdout (`stderr` false) or stderr contains `needle`, returning
    /// everything seen so far.
    fn wait_for(&mut self, stderr: bool, needle: &str) -> Vec<String> {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let (rx, seen) = if stderr {
                (&self.err, &mut self.seen_err)
            } else {
                (&self.out, &mut self.seen_out)
            };
            while let Ok(line) = rx.try_recv() {
                seen.push(line);
            }
            if seen.iter().any(|l| l.contains(needle)) {
                return seen.clone();
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {needle:?}; stdout {:?}, stderr {:?}",
                self.seen_out,
                self.seen_err
            );
            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn temp_file(name: &str, source: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("colibri-watch-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("prog.colibri");
    std::fs::write(&path, source).expect("temp file");
    path
}

#[test]
fn a_running_event_loop_is_patched_in_place() {
    let src = |tag: &str| {
        format!(
            "fn msg() -> str => \"{tag}\"\nfn ticker() {{\n    while true {{\n        print(msg())\n        wait_real(0.05)\n    }}\n}}\nfn main() {{\n    print(\"main\")\n    go ticker()\n}}\n"
        )
    };
    let path = temp_file("patch", &src("one"));
    let mut w = Watch::start(&path);
    w.wait_for(false, "one");
    std::fs::write(&path, src("two")).unwrap();
    w.wait_for(true, "patched 1 function");
    let out = w.wait_for(false, "two");
    // `main` ran once: a patch does not start the program again.
    assert_eq!(out.iter().filter(|l| *l == "main").count(), 1, "{out:?}");
}

#[test]
fn a_restart_runs_main_again_and_carries_state() {
    let v1 = "var n = 41\nfn save_state() -> str => \"{n}\"\nfn tick() -> int => 1\nfn main() {\n    print(\"v1 {n}\")\n    go forever()\n}\nfn forever() {\n    while true {\n        wait_real(0.05)\n    }\n}\n";
    let v2 = "var n = 0\nfn restore_state(s: str) {\n    print(\"restored {s}\")\n}\nfn tick(x: int) -> int => x\nfn main() {\n    print(\"v2\")\n    go forever()\n}\nfn forever() {\n    while true {\n        wait_real(0.05)\n    }\n}\n";
    let path = temp_file("restart", v1);
    let mut w = Watch::start(&path);
    w.wait_for(false, "v1 41");
    std::fs::write(&path, v2).unwrap();
    w.wait_for(true, "restarted");
    let out = w.wait_for(false, "v2");
    let err = w.wait_for(true, "state carried");
    assert!(out.iter().any(|l| l == "restored 41"), "{out:?} {err:?}");
}

#[test]
fn a_panic_waits_for_an_edit_then_runs_again() {
    let bad = "fn main() {\n    print(\"before\")\n    panic(\"boom\")\n}\n";
    let good = "fn main() {\n    print(\"fixed\")\n}\n";
    let path = temp_file("panic", bad);
    let mut w = Watch::start(&path);
    w.wait_for(true, "boom");
    w.wait_for(true, "waiting for a change");
    assert!(w.child.try_wait().unwrap().is_none(), "the tool exited");
    std::fs::write(&path, good).unwrap();
    w.wait_for(false, "fixed");
    w.wait_for(true, "the program ended");
    // Another edit runs it again.
    std::fs::write(&path, "fn main() {\n    print(\"again\")\n}\n").unwrap();
    w.wait_for(false, "again");
}

#[test]
fn code_that_does_not_check_is_reported_and_the_old_program_goes_on() {
    let ok = "fn msg() -> str => \"fine\"\nfn main() {\n    print(msg())\n    go t()\n}\nfn t() {\n    while true {\n        wait_real(0.05)\n    }\n}\n";
    let path = temp_file("check", ok);
    let mut w = Watch::start(&path);
    w.wait_for(false, "fine");
    std::fs::write(&path, ok.replace("-> str => \"fine\"", "-> str => 5")).unwrap();
    w.wait_for(true, "keeping the old program");
    assert!(w.child.try_wait().unwrap().is_none());
}

#[test]
fn a_program_that_does_not_check_at_start_runs_once_it_does() {
    let path = temp_file("startbad", "fn main() {\n    print(\n}\n");
    let mut w = Watch::start(&path);
    w.wait_for(true, "waiting for a change");
    std::fs::write(&path, "fn main() {\n    print(\"hello\")\n}\n").unwrap();
    w.wait_for(false, "hello");
}
