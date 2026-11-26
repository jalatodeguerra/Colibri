//! Scripted transcripts of `colibri debug`: the real binary, driven over pipes with DAP
//! requests (`harness.rs`). Each test is one session.

mod harness;

use std::path::Path;

use harness::{Dap, json_path, line_of};
use serde_json::{Value, json};

/// A session stopped at its first stop: the program written, the breakpoints set (a breakpoint
/// is a line), configuration done, and the first `stopped` event taken.
fn stopped_at(name: &str, source: &str, lines: &[i64]) -> (Dap, Value) {
    let mut dap = Dap::start(name);
    dap.launch(source, json!({}));
    dap.break_at(lines);
    dap.configuration_done();
    let stop = dap.stopped();
    (dap, stop)
}

fn text(v: &Value) -> &str {
    v.as_str().expect("a string")
}

const LOOP: &str = "\
fn main() {
    let mut total = 0
    for i in 1..4 {
        total += i // @body
    }
    print(total)
}
";

#[test]
fn a_breakpoint_in_a_loop_stops_at_every_iteration_with_its_locals() {
    let body = line_of(LOOP, "body");
    let mut dap = Dap::start("loop");
    dap.launch(LOOP, json!({}));
    let set = dap.break_at(&[body]);
    assert_eq!(set.len(), 1);
    assert_eq!(set[0]["line"], body);
    let id = set[0]["id"].clone();
    dap.configuration_done();
    for i in 1..=3 {
        let stop = dap.stopped();
        assert_eq!(stop["reason"], "breakpoint");
        assert_eq!(stop["hitBreakpointIds"], json!([id]));
        assert_eq!(stop["threadId"], 1);
        assert_eq!(dap.where_(), ("main".to_string(), body));
        let locals = dap.locals();
        assert!(locals.contains(&format!("i = {i}")), "{locals:?}");
        dap.request("continue", json!({"threadId": 1}));
    }
    assert_eq!(dap.finish(), Some(0));
    assert_eq!(dap.output("stdout"), "6\n");
    dap.disconnect();
}

#[test]
fn the_stack_names_the_source_file_and_the_threads_list_main() {
    let body = line_of(LOOP, "body");
    let (mut dap, _) = stopped_at("stack", LOOP, &[body]);
    let frame = dap.top_frame();
    assert_eq!(frame["name"], "main");
    assert_eq!(frame["source"]["name"], "main.colibri");
    assert_eq!(
        Path::new(text(&frame["source"]["path"])),
        dap.program.as_path()
    );
    let threads = dap.request("threads", json!({}));
    assert_eq!(threads["threads"], json!([{"id": 1, "name": "main"}]));
    dap.disconnect();
}

#[test]
fn a_breakpoint_in_a_function_that_is_compiled_later_stops_there() {
    let src = "\
fn helper(n: int) -> int {
    let doubled = n * 2 // @inside
    return doubled
}
fn main() {
    print(\"start\")
    print(helper(21))
}
";
    let inside = line_of(src, "inside");
    let (mut dap, stop) = stopped_at("lazy", src, &[inside]);
    assert_eq!(stop["reason"], "breakpoint");
    assert_eq!(dap.where_(), ("helper".to_string(), inside));
    assert_eq!(dap.locals(), ["n = 21"]);
    // What the program printed before the stop came out before the stop was announced.
    let stopped = dap
        .history
        .iter()
        .position(|m| m["event"] == "stopped")
        .unwrap();
    let before: String = dap.history[..stopped]
        .iter()
        .filter(|m| m["event"] == "output")
        .map(|m| text(&m["body"]["output"]))
        .collect();
    assert_eq!(before, "start\n");
    dap.request("continue", json!({"threadId": 1}));
    assert_eq!(dap.finish(), Some(0));
    assert_eq!(dap.output("stdout"), "start\n42\n");
    dap.disconnect();
}

const MAPPED: &str = "\
fn main() {
    let xs = [1]
    let ys = xs.map(fn(x) { // @call
        let d = x * 2 // @inner
        return d
    })
    print(ys) // @after
}
";

#[test]
fn step_in_over_and_out_across_a_closure_built_in() {
    let (call, inner, after) = (
        line_of(MAPPED, "call"),
        line_of(MAPPED, "inner"),
        line_of(MAPPED, "after"),
    );
    // In: into the closure, and the built-in that calls it is not on the stack.
    let (mut dap, _) = stopped_at("step-in", MAPPED, &[call]);
    assert_eq!(dap.where_(), ("main".to_string(), call));
    dap.request("stepIn", json!({"threadId": 1}));
    assert_eq!(dap.stopped()["reason"], "step");
    assert_eq!(dap.where_(), ("<closure in main>".to_string(), inner));
    let frames = dap.request("stackTrace", json!({"threadId": 1}));
    assert_eq!(frames["totalFrames"], 2);
    assert_eq!(dap.locals(), ["x = 1"]);
    dap.disconnect();
    // Over: past the whole call.
    let (mut dap, _) = stopped_at("step-over", MAPPED, &[call]);
    dap.request("next", json!({"threadId": 1}));
    assert_eq!(dap.stopped()["reason"], "step");
    assert_eq!(dap.where_(), ("main".to_string(), after));
    dap.request("continue", json!({"threadId": 1}));
    assert_eq!(dap.finish(), Some(0));
    assert_eq!(dap.output("stdout"), "[2]\n");
    dap.disconnect();
    // Out: from inside the closure to the statement after the call.
    let (mut dap, _) = stopped_at("step-out", MAPPED, &[inner]);
    assert_eq!(dap.where_(), ("<closure in main>".to_string(), inner));
    dap.request("stepOut", json!({"threadId": 1}));
    assert_eq!(dap.stopped()["reason"], "step");
    assert_eq!(dap.where_(), ("main".to_string(), after));
    dap.disconnect();
}

const COUNTED: &str = "\
fn main() {
    for i in 1..6 {
        print(i) // @body
    }
}
";

#[test]
fn a_hit_count_condition_stops_only_where_it_holds() {
    let body = line_of(COUNTED, "body");
    let cases: [(&str, &[&str]); 3] = [
        ("3", &["i = 3"]),
        (">= 4", &["i = 4", "i = 5"]),
        ("% 2", &["i = 2", "i = 4"]),
    ];
    for (condition, expected) in cases {
        let mut dap = Dap::start("hits");
        dap.launch(COUNTED, json!({}));
        let set = dap.set_breakpoints(vec![json!({"line": body, "hitCondition": condition})]);
        assert_eq!(set[0]["line"], body, "{condition}");
        dap.configuration_done();
        for want in expected {
            let stop = dap.stopped();
            assert_eq!(stop["reason"], "breakpoint", "{condition}");
            assert_eq!(dap.locals(), [want.to_string()], "{condition}");
            dap.request("continue", json!({"threadId": 1}));
        }
        assert_eq!(dap.finish(), Some(0));
        assert_eq!(dap.output("stdout"), "1\n2\n3\n4\n5\n");
        let stops = dap
            .history
            .iter()
            .filter(|m| m["event"] == "stopped")
            .count();
        assert_eq!(stops, expected.len(), "{condition}");
        dap.disconnect();
    }
}

#[test]
fn conditions_and_hit_counts_that_are_not_supported_are_refused_in_the_response() {
    let body = line_of(COUNTED, "body");
    let mut dap = Dap::start("refused");
    dap.launch(COUNTED, json!({}));
    let set = dap.set_breakpoints(vec![
        json!({"line": body, "condition": "i == 2"}),
        json!({"line": body, "hitCondition": "< 3"}),
        json!({"line": body, "hitCondition": "abc"}),
        json!({"line": body}),
    ]);
    assert_eq!(set.len(), 4);
    for refused in &set[..3] {
        assert_eq!(refused["verified"], false, "{refused}");
        assert_eq!(refused["line"], body);
        assert!(refused["message"].is_string());
    }
    assert!(text(&set[0]["message"]).contains("hit count"));
    assert!(set[3]["id"].is_number());
    // Only the plain one is set: the program stops at every iteration.
    dap.configuration_done();
    for _ in 0..5 {
        dap.stopped();
        dap.request("continue", json!({"threadId": 1}));
    }
    assert_eq!(dap.finish(), Some(0));
    dap.disconnect();
}

#[test]
fn a_breakpoint_that_cannot_be_set_is_not_verified() {
    let mut dap = Dap::start("unverified");
    let path = dap.launch(COUNTED, json!({}));
    // A line with no statement, and a file that is not part of the program.
    let set = dap.set_breakpoints(vec![json!({"line": 1000})]);
    assert_eq!(set[0]["verified"], false);
    let other = path.with_file_name("other.colibri");
    let body = dap.request(
        "setBreakpoints",
        json!({"source": {"path": json_path(&other)}, "breakpoints": [{"line": 1}]}),
    );
    assert_eq!(body["breakpoints"][0]["verified"], false);
    assert!(text(&body["breakpoints"][0]["message"]).contains("not part of the program"));
    dap.disconnect();
}

#[test]
fn a_logpoint_writes_to_the_console_and_does_not_stop() {
    let body = line_of(COUNTED, "body");
    let mut dap = Dap::start("logpoint");
    dap.launch(COUNTED, json!({}));
    dap.set_breakpoints(vec![json!({"line": body, "logMessage": "i is {i}"})]);
    dap.configuration_done();
    assert_eq!(dap.finish(), Some(0));
    assert_eq!(
        dap.output("console"),
        "i is 1\ni is 2\ni is 3\ni is 4\ni is 5\n"
    );
    assert_eq!(dap.output("stdout"), "1\n2\n3\n4\n5\n");
    assert!(dap.history.iter().all(|m| m["event"] != "stopped"));
    dap.disconnect();
}

const RISKY: &str = "\
fn risky(n: int) {
    let label = \"boom\"
    panic(\"failed: {label}\") // @panic
}
fn main() {
    print(\"before\")
    risky(7)
}
";

#[test]
fn break_on_panic_stops_at_the_panic_with_its_locals() {
    let at = line_of(RISKY, "panic");
    let mut dap = Dap::start("panic");
    dap.launch(RISKY, json!({}));
    dap.request("setExceptionBreakpoints", json!({"filters": ["panic"]}));
    dap.configuration_done();
    let stop = dap.stopped();
    assert_eq!(stop["reason"], "exception");
    assert!(text(&stop["text"]).contains("failed: boom"), "{stop}");
    assert_eq!(dap.where_(), ("risky".to_string(), at));
    let locals = dap.locals();
    assert!(locals.contains(&"n = 7".to_string()), "{locals:?}");
    assert!(
        locals.contains(&"label = \"boom\"".to_string()),
        "{locals:?}"
    );
    dap.request("continue", json!({"threadId": 1}));
    assert_eq!(dap.finish(), Some(101));
    assert!(dap.output("stderr").contains("failed: boom"));
    assert_eq!(dap.output("stdout"), "before\n");
    dap.disconnect();
}

#[test]
fn without_the_panic_filter_a_panic_does_not_stop_and_the_exit_code_is_101() {
    let mut dap = Dap::start("no-filter");
    dap.launch(RISKY, json!({}));
    dap.request("setExceptionBreakpoints", json!({"filters": []}));
    dap.configuration_done();
    assert_eq!(dap.finish(), Some(101));
    assert!(dap.history.iter().all(|m| m["event"] != "stopped"));
    assert!(dap.output("stderr").contains("failed: boom"));
    dap.disconnect();
}

const SPIN: &str = "\
fn main() {
    let mut n = 0
    while true {
        n += 1 // @spin
    }
}
";

#[test]
fn pause_stops_a_loop_that_never_ends() {
    let mut dap = Dap::start("pause");
    dap.launch(SPIN, json!({}));
    dap.configuration_done();
    dap.request("pause", json!({"threadId": 1}));
    let stop = dap.stopped();
    assert_eq!(stop["reason"], "pause");
    assert_eq!(dap.where_().0, "main");
    let locals = dap.locals();
    assert!(locals[0].starts_with("n = "), "{locals:?}");
    // While stopped nothing runs: the count is the same when asked again.
    assert_eq!(dap.locals(), locals);
    dap.disconnect();
}

#[test]
fn a_breakpoint_set_while_the_program_runs_is_hit() {
    let spin = line_of(SPIN, "spin");
    let mut dap = Dap::start("running-bp");
    dap.launch(SPIN, json!({}));
    dap.configuration_done();
    let set = dap.break_at(&[spin]);
    assert_eq!(set[0]["line"], spin);
    let stop = dap.stopped();
    assert_eq!(stop["reason"], "breakpoint");
    assert_eq!(dap.where_(), ("main".to_string(), spin));
    // Cleared while stopped, it is gone: the program runs on until paused.
    dap.break_at(&[]);
    dap.request("continue", json!({"threadId": 1}));
    dap.request("pause", json!({"threadId": 1}));
    assert_eq!(dap.stopped()["reason"], "pause");
    dap.disconnect();
}

#[test]
fn a_running_program_ends_on_disconnect_and_on_terminate() {
    let mut dap = Dap::start("end-running");
    dap.launch(SPIN, json!({}));
    dap.configuration_done();
    dap.disconnect();

    let mut dap = Dap::start("terminate-running");
    dap.launch(SPIN, json!({}));
    dap.configuration_done();
    dap.request("terminate", json!({}));
    dap.event("terminated");
    // A program that the debugger ended has no exit code to report.
    assert!(dap.history.iter().all(|m| m["event"] != "exited"));
    dap.disconnect();
}

#[test]
fn terminate_while_stopped_runs_the_defers_and_ends() {
    let src = "\
fn main() {
    defer print(\"cleanup\")
    let x = 1 // @here
    print(x)
}
";
    let (mut dap, _) = stopped_at("terminate-stopped", src, &[line_of(src, "here")]);
    dap.request("terminate", json!({}));
    dap.event("terminated");
    assert_eq!(dap.output("stdout"), "cleanup\n");
    assert!(dap.history.iter().all(|m| m["event"] != "exited"));
    dap.disconnect();
}

const SHAPES: &str = "\
struct P { x: int, y: int }
enum Shape { Circle(radius: float), Quit }
class Node { name: str, next: Node? }
fn main() {
    let p = P { x: 1, y: 2 }
    let s = Shape.Circle(2.5)
    let xs = [p, p]
    let m = {\"a\": 1}
    let t = (1, \"two\")
    let n = Node { name: \"n\", next: none }
    print(\"end\") // @end
}
";

/// A `variables` result as `(name, value, type, expandable)`.
fn plain(vars: &[Value]) -> Vec<(String, String, String, bool)> {
    vars.iter()
        .map(|v| {
            (
                text(&v["name"]).to_string(),
                text(&v["value"]).to_string(),
                text(&v["type"]).to_string(),
                v["variablesReference"].as_i64().unwrap() > 0,
            )
        })
        .collect()
}

#[test]
fn structs_lists_maps_tuples_enums_and_classes_expand() {
    let end = line_of(SHAPES, "end");
    let (mut dap, _) = stopped_at("variables", SHAPES, &[end]);
    let frame = dap.top_frame()["id"].as_i64().unwrap();
    // The module's values are many: the client asks for them only when they are opened.
    let raw = dap.request("scopes", json!({"frameId": frame}));
    assert_eq!(raw["scopes"][0]["expensive"], false);
    assert_eq!(raw["scopes"][1]["expensive"], true);
    let scopes = dap.scopes(frame);
    assert_eq!(scopes[0].0, "Locals");
    assert_eq!(scopes[1].0, "Module");
    let locals = dap.variables(scopes[0].1);
    let get = |name: &str| locals.iter().find(|v| v["name"] == name).unwrap().clone();
    let reference = |name: &str| get(name)["variablesReference"].as_i64().unwrap();
    let shown = |name: &str| {
        (
            text(&get(name)["value"]).to_string(),
            text(&get(name)["type"]).to_string(),
        )
    };
    assert_eq!(shown("p"), ("P { x: 1, y: 2 }".into(), "P".into()));
    assert_eq!(shown("s"), ("Circle(radius: 2.5)".into(), "Shape".into()));
    assert_eq!(shown("m").0, "{\"a\": 1}");
    assert_eq!(shown("t").0, "(1, \"two\")");
    assert_eq!(shown("n").1, "Node");
    let int = |n: &str, v: &str| (n.to_string(), v.to_string(), "int".to_string(), false);

    let p = dap.variables(reference("p"));
    assert_eq!(plain(&p), [int("x", "1"), int("y", "2")]);
    let s = dap.variables(reference("s"));
    assert_eq!(
        plain(&s),
        [("radius".into(), "2.5".into(), "float".into(), false)]
    );
    let xs = dap.variables(reference("xs"));
    assert_eq!(xs.len(), 2);
    assert_eq!(xs[1]["name"], "[1]");
    assert_eq!(xs[1]["value"], "P { x: 1, y: 2 }");
    let inner = dap.variables(xs[1]["variablesReference"].as_i64().unwrap());
    assert_eq!(plain(&inner), [int("x", "1"), int("y", "2")]);
    let m = dap.variables(reference("m"));
    assert_eq!((text(&m[0]["name"]), text(&m[0]["value"])), ("\"a\"", "1"));
    let t = dap.variables(reference("t"));
    assert_eq!(
        t.iter()
            .map(|v| (text(&v["name"]), text(&v["value"])))
            .collect::<Vec<_>>(),
        [("0", "1"), ("1", "\"two\"")]
    );
    let n = dap.variables(reference("n"));
    assert_eq!(
        n.iter()
            .map(|v| (text(&v["name"]), text(&v["value"])))
            .collect::<Vec<_>>(),
        [("name", "\"n\""), ("next", "none")]
    );
    // Paging: one element of the list.
    let page = dap.request(
        "variables",
        json!({"variablesReference": reference("xs"), "start": 1, "count": 1}),
    );
    assert_eq!(page["variables"][0]["name"], "[1]");
    assert_eq!(page["variables"].as_array().unwrap().len(), 1);
    dap.disconnect();
}

#[test]
fn evaluate_reads_paths_and_refuses_expressions() {
    let end = line_of(SHAPES, "end");
    let (mut dap, _) = stopped_at("evaluate", SHAPES, &[end]);
    let frame = dap.top_frame()["id"].as_i64().unwrap();
    let eval = |dap: &mut Dap, expression: &str| {
        dap.request(
            "evaluate",
            json!({"expression": expression, "frameId": frame, "context": "watch"}),
        )
    };
    assert_eq!(eval(&mut dap, "p.x")["result"], "1");
    assert_eq!(eval(&mut dap, "p.y")["type"], "int");
    assert_eq!(eval(&mut dap, "xs[1].y")["result"], "2");
    assert_eq!(eval(&mut dap, "m[\"a\"]")["result"], "1");
    assert_eq!(eval(&mut dap, "t.1")["result"], "\"two\"");
    assert_eq!(eval(&mut dap, "n.name")["result"], "\"n\"");
    let whole = eval(&mut dap, "p");
    assert_eq!(whole["result"], "P { x: 1, y: 2 }");
    assert!(whole["variablesReference"].as_i64().unwrap() > 0);
    // Without a frame: the innermost one.
    let body = dap.request("evaluate", json!({"expression": "xs[0].x"}));
    assert_eq!(body["result"], "1");

    for expression in ["p.x + 1", "1 + 2", "xs[p.x]", "len(xs)", "\"text\""] {
        let why = dap.request_fails(
            "evaluate",
            json!({"expression": expression, "frameId": frame}),
        );
        assert!(why.contains("only variable paths"), "{expression}: {why}");
    }
    let why = dap.request_fails(
        "evaluate",
        json!({"expression": "nothing", "frameId": frame}),
    );
    assert!(why.contains("not defined"), "{why}");
    let why = dap.request_fails("evaluate", json!({"expression": "p.z", "frameId": frame}));
    assert!(why.contains("no `z`"), "{why}");
    let why = dap.request_fails("evaluate", json!({"expression": "p.x.y", "frameId": frame}));
    assert!(why.contains("no parts"), "{why}");
    dap.disconnect();
}

#[test]
fn the_program_output_comes_as_output_events_and_the_stream_stays_protocol() {
    let src = "\
import io
fn main() {
    print(\"to stdout\")
    io.eprintln(\"to stderr\")
    io.write(\"no newline\")
}
";
    let mut dap = Dap::start("output");
    dap.launch(src, json!({}));
    dap.configuration_done();
    assert_eq!(dap.finish(), Some(0));
    assert_eq!(dap.output("stdout"), "to stdout\nno newline");
    assert_eq!(dap.output("stderr"), "to stderr\n");
    dap.disconnect();
}

#[test]
fn the_exit_code_of_the_program_is_reported() {
    let src = "\
import os
fn main() {
    print(\"bye\")
    os.exit(3)
}
";
    let mut dap = Dap::start("exit-code");
    dap.launch(src, json!({}));
    dap.configuration_done();
    assert_eq!(dap.finish(), Some(3));
    assert_eq!(dap.output("stdout"), "bye\n");
    // After the program the session still answers.
    let threads = dap.request("threads", json!({}));
    assert_eq!(threads["threads"], json!([]));
    let why = dap.request_fails("stackTrace", json!({"threadId": 1}));
    assert!(why.contains("ended"), "{why}");
    dap.disconnect();
}

#[test]
fn launch_arguments_reach_the_program() {
    let src = "\
import os
fn main() {
    print(os.args())
}
";
    let mut dap = Dap::start("args");
    dap.launch(
        src,
        json!({"args": ["one", "two words"], "cwd": json_path(&std::env::temp_dir())}),
    );
    dap.configuration_done();
    assert_eq!(dap.finish(), Some(0));
    assert_eq!(dap.output("stdout"), "[\"one\", \"two words\"]\n");
    dap.disconnect();
}

#[test]
fn stop_on_entry_stops_before_the_first_statement_runs() {
    let src = "\
fn main() {
    print(\"first\")
    print(\"second\") // @second
}
";
    let mut dap = Dap::start("entry");
    dap.launch(src, json!({"stopOnEntry": true}));
    dap.configuration_done();
    let stop = dap.stopped();
    assert_eq!(stop["reason"], "entry");
    assert_eq!(dap.where_().0, "main");
    assert_eq!(dap.output("stdout"), "");
    dap.request("next", json!({"threadId": 1}));
    dap.stopped();
    assert_eq!(dap.output("stdout"), "first\n");
    dap.request("continue", json!({"threadId": 1}));
    assert_eq!(dap.finish(), Some(0));
    dap.disconnect();
}

#[test]
fn a_program_that_does_not_compile_fails_the_launch() {
    let mut dap = Dap::start("compile-error");
    let path = dap
        .temp
        .write("main.colibri", "fn main() {\n    let x: int = \"a\"\n}\n");
    dap.request("initialize", json!({}));
    let why = dap.request_fails("launch", json!({"program": json_path(&path)}));
    assert!(why.contains("error"), "{why}");
    dap.event("terminated");
    dap.disconnect();
}

#[test]
fn bad_requests_and_bad_frames_are_answered_or_reported_and_the_session_goes_on() {
    let mut dap = Dap::start("malformed");
    // A body that is not JSON, JSON that is not a request, a request with no command.
    dap.send_raw(b"Content-Length: 5\r\n\r\n{nope");
    dap.send_raw(b"Content-Length: 2\r\n\r\n[]");
    let no_command = br#"{"seq":1,"type":"request"}"#;
    dap.send_raw(format!("Content-Length: {}\r\n\r\n", no_command.len()).as_bytes());
    dap.send_raw(no_command);
    let body = dap.request("initialize", json!({}));
    assert_eq!(body["supportsConfigurationDoneRequest"], true);
    let ignored = dap
        .output("stderr")
        .lines()
        .filter(|l| l.contains("message ignored"))
        .count();
    assert_eq!(ignored, 3, "{:?}", dap.output("stderr"));

    let why = dap.request_fails("launch", json!({}));
    assert!(why.contains("program"), "{why}");
    let why = dap.request_fails("threads", json!({}));
    assert!(why.contains("launch"), "{why}");
    // The launch that failed did not use up the session.
    let path = dap
        .temp
        .write("main.colibri", "fn main() {\n    print(1)\n}\n");
    dap.request("launch", json!({"program": json_path(&path)}));
    dap.event("initialized");
    let why = dap.request_fails("nosuchrequest", json!({}));
    assert!(why.contains("not supported"), "{why}");
    let why = dap.request_fails("stackTrace", json!({"threadId": 1}));
    assert!(why.contains("not started"), "{why}");
    dap.configuration_done();
    assert_eq!(dap.finish(), Some(0));
    dap.disconnect();
}

#[test]
fn a_header_without_a_length_ends_the_session_instead_of_losing_the_next_requests() {
    // The body that follows such a header cannot be told from the next header, so the adapter
    // says so and ends; it does not answer the requests after it with silence.
    for header in [
        "Content-Length: 99999999999999999999999",
        "Content-Length: abc",
        "X-Header: 1",
    ] {
        let mut dap = Dap::start("bad-header");
        dap.send_raw(format!("{header}\r\n\r\n{{\"seq\":1}}").as_bytes());
        dap.send("initialize", json!({}));
        let body = dap.event("output");
        assert_eq!(body["category"], "stderr");
        assert!(
            text(&body["output"]).contains("cannot be followed"),
            "{header}: {body}"
        );
        dap.wait_exit();
        assert!(
            dap.history.iter().all(|m| m["type"] != "response"),
            "{header}: {:?}",
            dap.history
        );
    }
}

#[test]
fn input_that_ends_inside_a_frame_ends_the_adapter() {
    let mut dap = Dap::start("truncated");
    dap.request("initialize", json!({}));
    dap.send_raw(b"Content-Length: 100\r\n\r\n{\"seq\": 2");
    dap.close_input();
}

#[test]
fn input_that_ends_while_the_program_runs_ends_the_program() {
    let mut dap = Dap::start("input-ends");
    dap.launch(SPIN, json!({}));
    dap.configuration_done();
    dap.close_input();
}

#[test]
fn the_capabilities_say_what_is_supported() {
    let mut dap = Dap::start("capabilities");
    let caps = dap.request("initialize", json!({}));
    assert_eq!(caps["supportsHitConditionalBreakpoints"], true);
    assert_eq!(caps["supportsLogPoints"], true);
    assert_eq!(caps["supportsConditionalBreakpoints"], false);
    assert_eq!(caps["supportsSetVariable"], false);
    assert_eq!(caps["exceptionBreakpointFilters"][0]["filter"], "panic");
    dap.disconnect();
}

/// Unix: a program that starts another one with `process.status` (which gives the child the
/// adapter's own standard output) must not write into the protocol stream, and must not be
/// able to reach the descriptor the protocol is written to.
#[cfg(unix)]
#[test]
fn a_child_process_cannot_write_into_the_stream_or_reach_its_descriptor() {
    let src = r#"import process
fn main() {
    let script = "for fd in 3 4 5 6 7 8 9; do (echo LEAK$fd >&$fd); done; echo from the child"
    let _ = process.status("sh", ["-c", script])
    print("after")
}
"#;
    let mut dap = Dap::start("child");
    dap.launch(src, json!({}));
    dap.configuration_done();
    assert_eq!(dap.finish(), Some(0));
    assert_eq!(dap.output("stdout"), "after\n");
    // What the child wrote to its standard output went to the adapter's standard error. A
    // write to the protocol descriptor would be a junk line in the stream, which the harness
    // reads strictly (`finish` above would have failed). Other descriptors the child
    // inherited from the test runner may point at the same standard error, so `LEAKn` lines
    // there say nothing: they are printed to show which ones, in a CI log.
    let stderr = dap.disconnect_stderr();
    eprintln!("child output on the adapter's standard error: {stderr:?}");
    assert!(stderr.contains("from the child"), "{stderr}");
}

const SLEEPERS: [&str; 2] = [
    "import time\nfn main() {\n    print(\"sleeping\")\n    time.sleep(600.0)\n}\n",
    "fn main() {\n    print(\"sleeping\")\n    wait(600.0)\n}\n",
];

#[test]
fn a_program_blocked_in_a_native_call_or_idle_still_ends_on_disconnect() {
    for src in SLEEPERS {
        let mut dap = Dap::start("sleep-disconnect");
        dap.launch(src, json!({}));
        dap.configuration_done();
        // The program is about to sleep for ten minutes: nothing but the adapter's own
        // watchdog can end the session.
        while !dap.output("stdout").contains("sleeping") {
            dap.event("output");
        }
        dap.disconnect();
    }
}

#[test]
fn a_program_blocked_in_a_native_call_or_idle_ends_with_the_input() {
    for src in SLEEPERS {
        let mut dap = Dap::start("sleep-input");
        dap.launch(src, json!({}));
        dap.configuration_done();
        while !dap.output("stdout").contains("sleeping") {
            dap.event("output");
        }
        dap.close_input();
    }
}

#[test]
fn terminating_runs_every_defer_even_one_that_has_a_breakpoint() {
    let src = "\
fn cleanup() {
    print(\"cleanup start\")
    print(\"cleanup end\") // @inner
}
fn main() {
    defer cleanup()
    defer print(\"outer\")
    let x = 1 // @here
    print(x)
}
";
    let lines = [line_of(src, "here"), line_of(src, "inner")];
    let (mut dap, _) = stopped_at("terminate-defers", src, &lines);
    dap.request("terminate", json!({}));
    dap.event("terminated");
    // The breakpoint in `cleanup` is not a stop while the program is ended.
    assert_eq!(dap.output("stdout"), "outer\ncleanup start\ncleanup end\n");
    let stops = dap
        .history
        .iter()
        .filter(|m| m["event"] == "stopped")
        .count();
    assert_eq!(stops, 1);
    dap.disconnect();
}

#[test]
fn references_from_an_earlier_stop_are_refused() {
    let body = line_of(LOOP, "body");
    let (mut dap, _) = stopped_at("stale", LOOP, &[body]);
    let frame = dap.top_frame()["id"].as_i64().unwrap();
    let locals = dap.scopes(frame)[0].1;
    assert!(!dap.variables(locals).is_empty());
    dap.request("continue", json!({"threadId": 1}));
    dap.stopped();
    // The new stop has frames and variables of its own; the old numbers are not among them.
    dap.top_frame();
    let why = dap.request_fails("variables", json!({"variablesReference": locals}));
    assert!(why.contains("not from this stop"), "{why}");
    let why = dap.request_fails("scopes", json!({"frameId": frame}));
    assert!(why.contains("not from this stop"), "{why}");
    dap.disconnect();
}

#[test]
fn a_zero_based_client_and_a_breakpoint_with_no_line_do_not_crash_the_adapter() {
    let mut dap = Dap::start("zero-based");
    let path = dap.temp.write("main.colibri", LOOP);
    dap.request(
        "initialize",
        json!({"linesStartAt1": false, "columnsStartAt1": false}),
    );
    dap.request("launch", json!({"program": json_path(&path)}));
    dap.event("initialized");
    let body = dap.request(
        "setBreakpoints",
        json!({"source": {"path": json_path(&path)}, "breakpoints": [{}, {"line": -1}, {"line": line_of(LOOP, "body") - 1}]}),
    );
    let set = body["breakpoints"].as_array().unwrap();
    assert_eq!(set.len(), 3);
    assert_eq!(set[2]["line"], line_of(LOOP, "body") - 1);
    dap.configuration_done();
    dap.stopped();
    // Lines go out the way they came in: 0-based.
    assert_eq!(dap.where_().1, line_of(LOOP, "body") - 1);
    dap.disconnect();
}

#[test]
fn a_list_says_how_many_items_it_has_so_that_the_client_pages() {
    let end = line_of(SHAPES, "end");
    let (mut dap, _) = stopped_at("paging", SHAPES, &[end]);
    let frame = dap.top_frame()["id"].as_i64().unwrap();
    let locals = dap.scopes(frame)[0].1;
    let vars = dap.variables(locals);
    let xs = vars.iter().find(|v| v["name"] == "xs").unwrap();
    assert_eq!(xs["indexedVariables"], 2);
    assert_eq!(xs["namedVariables"], 0);
    let p = vars.iter().find(|v| v["name"] == "p").unwrap();
    assert!(p.get("indexedVariables").is_none());
    dap.disconnect();
}

#[test]
fn no_debug_runs_the_program_without_stops_and_still_ends_on_disconnect() {
    let body = line_of(LOOP, "body");
    let mut dap = Dap::start("no-debug");
    dap.launch(LOOP, json!({"noDebug": true, "stopOnEntry": true}));
    let set = dap.break_at(&[body]);
    assert_eq!(set[0]["verified"], false);
    assert!(text(&set[0]["message"]).contains("noDebug"));
    dap.configuration_done();
    assert_eq!(dap.finish(), Some(0));
    assert_eq!(dap.output("stdout"), "6\n");
    assert!(dap.history.iter().all(|m| m["event"] != "stopped"));
    dap.disconnect();

    // No debugger is attached: the panic filter has nothing to stop.
    let mut dap = Dap::start("no-debug-panic");
    dap.launch(RISKY, json!({"noDebug": true}));
    dap.request("setExceptionBreakpoints", json!({"filters": ["panic"]}));
    dap.configuration_done();
    assert_eq!(dap.finish(), Some(101));
    assert!(dap.history.iter().all(|m| m["event"] != "stopped"));
    dap.disconnect();

    let mut dap = Dap::start("no-debug-spin");
    dap.launch(SPIN, json!({"noDebug": true}));
    dap.configuration_done();
    dap.disconnect();
}

#[test]
fn a_launch_that_cannot_start_fails_the_request_and_can_be_tried_again() {
    let mut dap = Dap::start("launch-retry");
    let path = dap
        .temp
        .write("main.colibri", "fn main() {\n    print(1)\n}\n");
    let manifest = dap.temp.write("colibri.toml", "[package]\nname = \"x\"\n");
    dap.request("initialize", json!({}));
    let why = dap.request_fails(
        "launch",
        json!({"program": json_path(&path), "features": ["no good!"]}),
    );
    assert!(why.contains("feature"), "{why}");
    let why = dap.request_fails("launch", json!({"program": json_path(&manifest)}));
    assert!(why.contains("colibri.toml"), "{why}");
    dap.request("launch", json!({"program": json_path(&path)}));
    dap.event("initialized");
    dap.configuration_done();
    assert_eq!(dap.finish(), Some(0));
    dap.disconnect();
}

#[test]
fn a_disconnect_after_the_program_ended_is_answered_once_and_the_stream_ends_whole() {
    // With no grace the adapter's watchdog and its main thread race for the same request: one
    // of them answers, never both and never half a frame.
    let src = "fn main() {\n    print(\"done\")\n}\n";
    for round in 0..30 {
        let mut dap = Dap::start_with_grace("end-race", Some(0));
        dap.launch(src, json!({"noDebug": true}));
        dap.configuration_done();
        dap.finish();
        let seq = dap.send("disconnect", json!({}));
        let response = dap.response_to(seq);
        assert_eq!(response["success"], true);
        dap.drain();
        let answers = dap
            .history
            .iter()
            .filter(|m| m["type"] == "response" && m["command"] == "disconnect")
            .count();
        assert_eq!(answers, 1, "round {round}: {:?}", dap.history);
        dap.wait_exit();
    }
}

#[test]
fn a_terminate_the_watchdog_answers_is_followed_by_terminated() {
    let mut dap = Dap::start("terminate-watchdog");
    dap.launch(SPIN, json!({"noDebug": true}));
    dap.configuration_done();
    let seq = dap.send("terminate", json!({}));
    assert_eq!(dap.response_to(seq)["success"], true);
    dap.event("terminated");
    dap.drain();
    dap.wait_exit();
}

#[test]
fn a_second_launch_with_other_features_is_refused() {
    let mut dap = Dap::start("features");
    let path = dap
        .temp
        .write("main.colibri", "fn main() {\n    print(1)\n}\n");
    // A manifest in a folder of its own, so that the script is not part of its package.
    let pkg = dap.temp.0.join("pkg");
    std::fs::create_dir(&pkg).unwrap();
    let manifest = pkg.join("colibri.toml");
    std::fs::write(&manifest, "[package]\nname = \"x\"\n").unwrap();
    dap.request("initialize", json!({}));
    // Fails after the features were taken, on the manifest.
    dap.request_fails(
        "launch",
        json!({"program": json_path(&manifest), "features": ["a"]}),
    );
    let why = dap.request_fails(
        "launch",
        json!({"program": json_path(&path), "features": ["b"]}),
    );
    assert!(why.contains("features"), "{why}");
    dap.request(
        "launch",
        json!({"program": json_path(&path), "features": ["a"]}),
    );
    dap.event("initialized");
    dap.disconnect();
}

#[test]
fn a_disconnect_followed_by_the_end_of_input_does_not_cut_a_long_defer() {
    // The adapter's watchdog is for a program that does not answer; a program that has taken
    // up the disconnect and is running its defers (here, for longer than the watchdog's
    // grace) is left alone, whatever else arrives after the disconnect.
    let src = r#"import fs
import time
fn cleanup() {
    time.sleep(3.0)
    let _ = fs.write("late.txt", "x")
}
fn main() {
    defer cleanup()
    print("ready")
    let mut n = 0
    while true {
        n += 1
    }
}
"#;
    let mut dap = Dap::start("long-defer");
    let folder = dap.temp.0.clone();
    dap.launch(src, json!({"cwd": json_path(&folder)}));
    dap.configuration_done();
    while !dap.output("stdout").contains("ready") {
        dap.event("output");
    }
    let seq = dap.send("disconnect", json!({}));
    dap.response_to(seq);
    dap.close_input();
    assert!(folder.join("late.txt").exists());
}
