//! What a thread task's worker shares with `main` in each command that builds a `Vm` (phase 3
//! L17 review): the writer `print` goes through (whole lines, in one order, where the command
//! collects them), and the library's view of the world (arguments, environment, clock, assets,
//! standard input). Each test builds the `Vm` the way the command does and runs tasks on it
//! through `submit_portable`.
//!
//! Nothing waits on a clock; every `Vm` is dropped (and its threads joined) inside the test.

use std::fs;
use std::io::Write;
use std::path::PathBuf;

use colibri_lang as lang;
use colibri_runtime::tasks::Portable;
use colibri_runtime::{SyncBuffer, Vm};

const PROGRAM: &str = r#"import assets
import io
import os
import time

fn say(tag: str) -> int {
    print("{tag} one")
    print("{tag} two")
    return 0
}

fn env_value() -> str {
    return os.env("COLIBRI_GOLDEN") or "missing"
}

fn arg_count() -> int {
    return os.args().len()
}

fn asset_text() -> str {
    return assets.read("greeting.txt").expect("greeting").trim()
}

fn stdin_line() -> str {
    return io.read_line().expect("read") or "no input"
}

fn after_a_sleep() -> int {
    time.sleep(0.5)
    return time.monotonic()
}

fn complain() -> int {
    io.eprintln("worker complaint")
    return 0
}

fn main() {
    say("main")
}
"#;

/// A package folder with the program and one asset, removed at the end.
struct Package(PathBuf);

impl Package {
    fn new(name: &str) -> Package {
        let dir = std::env::temp_dir().join(format!(
            "colibri-worker-paths-{}-{name}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("colibri.toml"),
            "[package]\nname = \"app\"\n\n[build]\nassets = [\"greeting.txt\"]\n",
        )
        .unwrap();
        fs::write(dir.join("greeting.txt"), "hello from the assets\n").unwrap();
        fs::write(dir.join("main.colibri"), PROGRAM).unwrap();
        Package(dir)
    }

    fn main(&self) -> PathBuf {
        self.0.join("main.colibri")
    }
}

impl Drop for Package {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn ask(vm: &mut Vm, name: &str, args: Vec<Portable>) -> Portable {
    vm.submit_portable(name, args)
        .unwrap()
        .wait()
        .unwrap_or_else(|e| panic!("{name}: {e}"))
        .value
}

fn text(value: Portable) -> String {
    match value {
        Portable::Str(ref s) => s.to_string(),
        Portable::SharedStr(_, ref s) => s.to_string(),
        other => panic!("not a string: {other:?}"),
    }
}

/// The lines of `out` that begin with `tag`, in order.
fn lines_of(out: &SyncBuffer, tag: &str) -> Vec<String> {
    out.contents()
        .lines()
        .filter(|l| l.starts_with(tag))
        .map(str::to_string)
        .collect()
}

fn say(vm: &mut Vm) {
    // The worker and the main thread print: the worker's lines are whole and are in the writer
    // the command gave, like the main thread's.
    assert!(matches!(
        ask(vm, "say", vec![Portable::Str("worker".into())]),
        Portable::Int(0)
    ));
    let say = vm.get_fn::<(String,), i64>("say").unwrap();
    say.call(vm, ("main".to_string(),)).unwrap();
}

#[test]
fn colibri_run_collects_what_a_worker_prints_and_gives_it_the_assets_and_arguments() {
    lang::on_big_stack(|| {
        let package = Package::new("run");
        let out = SyncBuffer::new();
        let mut vm = lang::runcmd::build_run_vm(
            &package.main(),
            vec!["one".to_string(), "two".to_string()],
            false,
            Box::new(out.clone()),
        );
        vm.load_path(package.main()).unwrap();
        say(&mut vm);
        assert_eq!(lines_of(&out, "worker"), ["worker one", "worker two"]);
        assert_eq!(lines_of(&out, "main"), ["main one", "main two"]);
        assert!(matches!(
            ask(&mut vm, "arg_count", vec![]),
            Portable::Int(2)
        ));
        assert_eq!(
            text(ask(&mut vm, "asset_text", vec![])),
            "hello from the assets"
        );
        // A worker never reads the host's standard input.
        assert_eq!(text(ask(&mut vm, "stdin_line", vec![])), "no input");
    });
}

#[test]
fn colibri_test_captures_what_a_worker_prints() {
    lang::on_big_stack(|| {
        let package = Package::new("test");
        let options = lang::testcmd::Options {
            path: package.main(),
            filter: None,
            budget: 0,
            nocapture: false,
            leaks: None,
            deps: lang::depscmd::DepFlags::default(),
            profile: None,
            profile_rc: false,
        };
        let captured = SyncBuffer::new();
        let mut vm = lang::testcmd::build_test_vm(&options, &captured);
        vm.load_path(package.main()).unwrap();
        say(&mut vm);
        // In the test's capture, where a failing test's output is shown from.
        assert_eq!(lines_of(&captured, "worker"), ["worker one", "worker two"]);
        assert_eq!(lines_of(&captured, "main"), ["main one", "main two"]);
        assert_eq!(
            text(ask(&mut vm, "asset_text", vec![])),
            "hello from the assets"
        );
    });
}

#[test]
fn the_golden_harness_collects_a_workers_output_and_gives_it_the_hermetic_world() {
    lang::on_big_stack(|| {
        let out = SyncBuffer::new();
        let (mut vm, _temp) = lang::golden::hermetic_vm(&out);
        vm.load_source("main.colibri", PROGRAM).unwrap();
        say(&mut vm);
        assert_eq!(lines_of(&out, "worker"), ["worker one", "worker two"]);
        assert_eq!(lines_of(&out, "main"), ["main one", "main two"]);
        // The fixed environment, the fixed arguments, and a clock that only the sleep moves.
        assert_eq!(text(ask(&mut vm, "env_value", vec![])), "yes");
        assert!(matches!(
            ask(&mut vm, "arg_count", vec![]),
            Portable::Int(2)
        ));
        assert!(matches!(
            ask(&mut vm, "after_a_sleep", vec![]),
            Portable::Int(500_000_000)
        ));
        // The goldens give the input `main` reads, not the workers: an empty one.
        assert_eq!(text(ask(&mut vm, "stdin_line", vec![])), "no input");
    });
}

/// A writer that records what it is given, for the two streams of the debug adapter.
#[derive(Clone, Default)]
struct Stream(SyncBuffer);

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.clone().write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn colibri_debug_sends_a_workers_output_to_the_adapters_streams_and_reads_nothing() {
    lang::on_big_stack(|| {
        let package = Package::new("debug");
        let (stdout, stderr) = (Stream::default(), Stream::default());
        let mut vm = lang::debugcmd::build_program_vm(colibri_dap::LaunchArgs {
            program: package.main(),
            args: vec!["a".to_string(), "b".to_string(), "c".to_string()],
            features: Vec::new(),
            stdout: Box::new(stdout.clone()),
            stderr: Box::new(stderr.clone()),
        })
        .unwrap();
        vm.load_path(package.main()).unwrap();
        say(&mut vm);
        // The adapter turns these into `output` events: the process's own streams (the protocol)
        // see nothing of a worker.
        assert_eq!(lines_of(&stdout.0, "worker"), ["worker one", "worker two"]);
        assert_eq!(lines_of(&stdout.0, "main"), ["main one", "main two"]);
        assert!(matches!(
            ask(&mut vm, "arg_count", vec![]),
            Portable::Int(3)
        ));
        assert_eq!(
            text(ask(&mut vm, "asset_text", vec![])),
            "hello from the assets"
        );
        assert_eq!(text(ask(&mut vm, "stdin_line", vec![])), "no input");
        ask(&mut vm, "complain", vec![]);
        assert_eq!(stderr.0.contents(), "worker complaint\n");
    });
}
