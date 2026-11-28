//! `colibri debug` of a package with `[rust-dependencies]` runs in the project runner (here the
//! fixture runner, put where cargo would put it by a fake cargo): the transcript is the one of a
//! plain session, a breakpoint in a script that calls a binding stops, and a host object is
//! shown by its class. The scripted client is the one of the adapter's own tests.

#[path = "../../colibri-cli/tests/dap/harness.rs"]
mod harness;

mod common;

use harness::{Dap, line_of};
use serde_json::json;

use common::{BOUND, FAKE_CARGO, PLAIN, RUNNER, UNDER_TEST};

const PROGRAM: &str = "\
import fixture

fn main() {
    let c = fixture.Counter.new(5)
    let n = c.bump(3) // @body
    print(n)
}
";

/// A session of `colibri debug` with fake cargo and a cache of its own.
fn start(name: &str, extra_env: &[(&str, String)]) -> Dap {
    let _ = UNDER_TEST;
    let home =
        std::env::temp_dir().join(format!("colibri-debug-home-{}-{name}", std::process::id()));
    let log = home.join("cargo.log");
    std::fs::create_dir_all(&home).unwrap();
    let mut env = vec![
        ("COLIBRI_HOME", home.display().to_string()),
        ("CARGO", FAKE_CARGO.to_string()),
        ("FAKE_CARGO_LOG", log.display().to_string()),
        ("FAKE_CARGO_RUNNER", RUNNER.to_string()),
    ];
    env.extend(extra_env.iter().map(|(k, v)| (*k, v.clone())));
    Dap::start_with_env(name, &env)
}

/// The package in the session's folder: the manifest, the program, and the interface the checker
/// reads (from the runner, as `colibri bindings` writes it).
fn package(dap: &Dap, manifest: &str, source: &str) {
    dap.temp.write("colibri.toml", manifest);
    dap.temp.write("main.colibri", source);
    if manifest.contains("rust-dependencies") {
        let out = std::process::Command::new(RUNNER)
            .arg("__write-interfaces")
            .arg(&dap.temp.0)
            .output()
            .unwrap();
        assert!(out.status.success());
    }
}

fn initialize(dap: &mut Dap) {
    let body = dap.request(
        "initialize",
        json!({"adapterID": "colibri", "linesStartAt1": true, "columnsStartAt1": true}),
    );
    assert_eq!(body["supportsConfigurationDoneRequest"], true);
}

#[test]
fn a_breakpoint_in_a_script_that_calls_a_binding_stops_and_shows_the_host_object() {
    let body = line_of(PROGRAM, "body");
    let mut dap = start("bound", &[]);
    package(&dap, BOUND, PROGRAM);
    dap.launch(PROGRAM, json!({}));
    dap.break_at(&[body]);
    dap.configuration_done();
    dap.stopped();
    assert_eq!(dap.where_(), ("main".to_string(), body));
    let locals = dap.locals();
    assert!(
        locals.iter().any(|l| l == "c = <fixture.Counter #0>"),
        "{locals:?}"
    );
    dap.request("continue", json!({"threadId": 1}));
    assert_eq!(dap.finish(), Some(0));
    assert_eq!(dap.output("stdout"), "8\n");
    // The build was told as events, and the client got one answer to `initialize`.
    assert!(
        dap.output("console").contains("building the Rust bindings"),
        "{}",
        dap.output("console")
    );
    let answers = dap
        .history
        .iter()
        .filter(|m| m["type"] == "response" && m["command"] == "initialize")
        .count();
    assert_eq!(answers, 1);
    dap.disconnect_after_history();
}

#[test]
fn a_package_without_bindings_is_served_in_place_with_one_answer_to_initialize() {
    let mut dap = start("plain", &[]);
    package(&dap, PLAIN, "fn main() {\n    print(1)\n}\n");
    dap.launch("fn main() {\n    print(1)\n}\n", json!({}));
    dap.configuration_done();
    assert_eq!(dap.finish(), Some(0));
    assert_eq!(dap.output("stdout"), "1\n");
    let answers = dap
        .history
        .iter()
        .filter(|m| m["type"] == "response" && m["command"] == "initialize")
        .count();
    assert_eq!(answers, 1);
    assert!(!dap.output("console").contains("building the Rust bindings"));
    dap.disconnect();
}

#[test]
fn a_build_that_fails_is_a_failed_launch_with_the_cargo_output_as_events() {
    let mut dap = start("fails", &[("FAKE_CARGO_FAIL", "build".to_string())]);
    package(&dap, BOUND, PROGRAM);
    initialize(&mut dap);
    let path = dap.temp.0.join("main.colibri");
    let seq = dap.send("launch", json!({"program": path.to_str().unwrap()}));
    let response = dap.response_to(seq);
    assert_eq!(response["success"], false, "{response}");
    assert!(
        response["message"]
            .as_str()
            .unwrap()
            .contains("could not be built"),
        "{response}"
    );
    dap.event("terminated");
    assert!(
        dap.output("console").contains("could not compile"),
        "{}",
        dap.output("console")
    );
    // The proxy wrote this whole session: numbered 1, 2, 3, ...
    dap.disconnect_after_history();
}

#[test]
fn a_runner_in_the_launch_is_checked_like_run_runner() {
    let mut dap = start("given", &[]);
    package(&dap, BOUND, PROGRAM);
    initialize(&mut dap);
    let path = dap.temp.0.join("main.colibri");
    // The plain `colibri` has no bindings: refused, naming them.
    let seq = dap.send(
        "launch",
        json!({"program": path.to_str().unwrap(), "runner": UNDER_TEST}),
    );
    let response = dap.response_to(seq);
    assert_eq!(response["success"], false, "{response}");
    assert!(
        response["message"]
            .as_str()
            .unwrap()
            .contains("is a plain `colibri`"),
        "{response}"
    );
    dap.event("terminated");
    dap.disconnect();
}

#[test]
fn a_runner_in_the_launch_is_used_without_cargo() {
    let mut dap = start(
        "given-ok",
        &[("CARGO", "colibri-no-such-cargo".to_string())],
    );
    package(&dap, BOUND, PROGRAM);
    dap.launch(PROGRAM, json!({"runner": RUNNER}));
    dap.configuration_done();
    assert_eq!(dap.finish(), Some(0));
    assert_eq!(dap.output("stdout"), "8\n");
    assert!(!dap.output("console").contains("building the Rust bindings"));
    dap.disconnect();
}

/// A `disconnect` while the runner is being built stops cargo and what it started: the launch
/// fails as stopped and the session ends without waiting for the build (the fake build would take
/// half a minute), and the child the fake cargo started is gone.
#[test]
fn a_disconnect_during_the_build_stops_cargo() {
    let pid_file =
        std::env::temp_dir().join(format!("colibri-cancel-child-{}", std::process::id()));
    let _ = std::fs::remove_file(&pid_file);
    let mut dap = start(
        "cancel",
        &[
            ("FAKE_CARGO_SLEEP_MS", "30000".to_string()),
            ("FAKE_CARGO_CHILD_PID", pid_file.display().to_string()),
        ],
    );
    package(&dap, BOUND, PROGRAM);
    initialize(&mut dap);
    let started = std::time::Instant::now();
    let path = dap.temp.0.join("main.colibri");
    let launch = dap.send("launch", json!({"program": path.to_str().unwrap()}));
    // The build line says the build has begun (and the fake cargo has started its child).
    let deadline = std::time::Instant::now() + harness::HANG_GUARD;
    loop {
        dap.drain_available();
        if dap.output("console").contains("building the Rust bindings") && pid_file.is_file() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the build did not start: {:#?}",
            dap.history
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let disconnect = dap.send("disconnect", json!({}));
    let response = dap.response_to(launch);
    assert_eq!(response["success"], false, "{response}");
    assert!(
        response["message"]
            .as_str()
            .unwrap()
            .contains("was stopped"),
        "{response}"
    );
    assert_eq!(dap.response_to(disconnect)["success"], true);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(20),
        "the build was waited for: {:?}",
        started.elapsed()
    );
    dap.wait_exit();
    #[cfg(unix)]
    {
        let child = std::fs::read_to_string(&pid_file).unwrap();
        let gone = |pid: &str| {
            let out = std::process::Command::new("ps")
                .args(["-o", "stat=", "-p", pid])
                .output()
                .unwrap();
            let state = String::from_utf8_lossy(&out.stdout);
            let state = state.trim();
            state.is_empty() || state.starts_with('Z')
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while !gone(&child) {
            assert!(
                std::time::Instant::now() < deadline,
                "the child of cargo (pid {child}) is still running"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
    let _ = std::fs::remove_file(&pid_file);
}
