//! Tests of debug mode (`crate::debug`): breakpoints, stepping, pause, break on panic and the
//! inspection API, driven by a recording debugger.

use std::cell::RefCell;
use std::rc::Rc;

use colibri_core::engine::Engine;
use colibri_core::prelude::*;
use colibri_syntax::{FileId, SourceMap, Span};

use crate::debug::{
    BreakpointInfo, BreakpointSpec, DebugView, Debugger, HitCondition, Locator, Resume, StepKind, Stop,
};
use crate::fiber::VmFiber;
use crate::machine::Machine;
use crate::source::check_source;

#[derive(Clone, Default)]
struct Out(Rc<RefCell<Vec<u8>>>);

impl std::io::Write for Out {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct Lines(SourceMap);

impl Locator for Lines {
    fn line(&self, span: Span) -> Option<u32> {
        if colibri_check::is_prelude_span(span) || span.file.0 >= self.0.next_file_id().0 {
            return None;
        }
        Some(self.0.line_col(span.file, span.start).0)
    }
    fn file(&self, span: Span) -> Option<String> {
        (span.file.0 < self.0.next_file_id().0).then(|| self.0.name(span.file).to_string())
    }
}

/// What one stop looked like.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Seen {
    pub stop: Stop,
    /// The visible frames, innermost first: name and line.
    pub frames: Vec<(String, u32)>,
    /// The locals of the top frame, `name = value`.
    pub locals: Vec<String>,
    /// What the program had printed when it stopped.
    pub out: String,
}

type React = Box<dyn FnMut(&mut DebugView<'_>, &Stop, usize) -> Resume>;

struct Recorder {
    poll: Option<Box<dyn FnMut(&mut DebugView<'_>)>>,
    out: Out,
    seen: Rc<RefCell<Vec<Seen>>>,
    logs: Rc<RefCell<Vec<String>>>,
    react: React,
}

pub(crate) fn locals_of(view: &mut DebugView<'_>, coroutine: Option<u64>, frame: usize) -> Vec<String> {
    let scopes = view.scopes(coroutine, frame);
    view.children(scopes[0].handle, 0, 1000)
        .into_iter()
        .map(|v| format!("{} = {}", v.name, v.value))
        .collect()
}

impl Debugger for Recorder {
    fn stopped(&mut self, view: &mut DebugView<'_>, stop: &Stop) -> Resume {
        let frames: Vec<(String, u32)> = view
            .frames(None)
            .iter()
            .map(|f| (f.name.clone(), f.line.unwrap_or(0)))
            .collect();
        let top = view.frames(None).first().map_or(0, |f| f.index);
        let locals = locals_of(view, None, top);
        let n = self.seen.borrow().len();
        self.seen.borrow_mut().push(Seen {
            stop: stop.clone(),
            frames,
            locals,
            out: String::from_utf8_lossy(&self.out.0.borrow()).into_owned(),
        });
        (self.react)(view, stop, n)
    }
    fn poll(&mut self, view: &mut DebugView<'_>) {
        if let Some(poll) = self.poll.as_mut() {
            poll(view);
        }
    }
    fn output(&mut self, text: &str) {
        self.logs.borrow_mut().push(text.to_string());
    }
}

pub(crate) struct Run {
    pub out: String,
    pub seen: Vec<Seen>,
    pub logs: Vec<String>,
    pub panic: Option<String>,
    pub steps: u64,
    pub infos: Vec<BreakpointInfo>,
}

/// Breakpoints of `main.colibri` to set once the program is loaded (`after`) or before.
pub(crate) struct Setup {
    pub bps: Vec<BreakpointSpec>,
    pub before_load: bool,
    pub break_on_panic: bool,
    /// Runs at each poll of the debugger (every 1,024 steps while the program runs).
    pub poll: Option<Box<dyn FnMut(&mut DebugView<'_>)>>,
    /// Runs on the machine before the program does.
    pub on_machine: Option<Box<dyn FnOnce(&mut Machine)>>,
}

impl Setup {
    pub fn lines(lines: &[u32]) -> Setup {
        Setup {
            bps: lines.iter().map(|&line| bp(line)).collect(),
            before_load: false,
            break_on_panic: false,
            poll: None,
            on_machine: None,
        }
    }
}

pub(crate) fn bp(line: u32) -> BreakpointSpec {
    BreakpointSpec {
        line,
        hit: None,
        log: None,
    }
}

pub(crate) fn run_debug(
    src: &str,
    mut setup: Setup,
    react: impl FnMut(&mut DebugView<'_>, &Stop, usize) -> Resume + 'static,
) -> Run {
    let (program, sources) = check_source(src).expect("the program checks");
    let file = program
        .fn_by_name("main")
        .map(|f| program.fns[f.0 as usize].body.span.file)
        .unwrap_or(FileId(0));
    let out = Out::default();
    let mut m = Machine::new(Box::new(out.clone()), 1000);
    let seen = Rc::new(RefCell::new(Vec::new()));
    let logs = Rc::new(RefCell::new(Vec::new()));
    m.set_debugger(
        Box::new(Recorder {
            poll: setup.poll.take(),
            out: out.clone(),
            seen: Rc::clone(&seen),
            logs: Rc::clone(&logs),
            react: Box::new(react),
        }),
        Rc::new(Lines(sources)),
    );
    m.debug_break_on_panic(setup.break_on_panic);
    if let Some(f) = setup.on_machine.take() {
        f(&mut m);
    }
    let mut infos = Vec::new();
    if setup.before_load {
        infos = m.debug_set_breakpoints(file, &setup.bps);
    }
    m.load(&program);
    if !setup.before_load {
        infos = m.debug_set_breakpoints(file, &setup.bps);
    }
    let mut fiber = VmFiber::new();
    let mut failure = m.run_module(&mut fiber).err();
    if failure.is_none()
        && let Some(main) = program.main
    {
        match m.call(&mut fiber, main, Vec::new()) {
            Ok(v) => m.release_in_entry(v),
            Err(info) => failure = Some(info),
        }
    }
    m.core_mut().flush();
    let steps = m.core().steps_used();
    m.shutdown();
    let out = String::from_utf8_lossy(&out.0.borrow()).into_owned();
    let seen = seen.borrow().clone();
    let logs = logs.borrow().clone();
    Run {
        out,
        seen,
        logs,
        panic: failure.map(|f| f.message),
        steps,
        infos,
    }
}

const LOOP: &str = "fn main() {
    let mut total = 0
    for i in 0..3 {
        total += i
        print(total)
    }
}
";

#[test]
fn a_breakpoint_in_a_loop_hits_once_per_trip() {
    let run = run_debug(LOOP, Setup::lines(&[4]), |_, _, _| Resume::Continue);
    assert_eq!(run.out, "0\n1\n3\n");
    assert_eq!(run.seen.len(), 3, "{:#?}", run.seen);
    for (i, s) in run.seen.iter().enumerate() {
        assert!(matches!(s.stop, Stop::Breakpoint(_)));
        assert_eq!(s.frames, vec![("main".to_string(), 4)]);
        let expect_total = [0, 0, 1][i];
        assert_eq!(
            s.locals,
            vec![format!("total = {expect_total}"), format!("i = {i}")]
        );
    }
}

/// A debugger that answers each stop with the next of `script`, and continues when it is out.
fn scripted(script: Vec<Resume>) -> impl FnMut(&mut DebugView<'_>, &Stop, usize) -> Resume {
    move |_, _, n| script.get(n).copied().unwrap_or(Resume::Continue)
}

/// `function:line` of the top frame of each stop.
fn trail(run: &Run) -> Vec<String> {
    run.seen
        .iter()
        .map(|s| {
            let (name, line) = &s.frames[0];
            format!("{name}:{line}")
        })
        .collect()
}

const CALLS: &str = "fn double(x: int) -> int {
    let y = x * 2
    return y
}
fn main() {
    let a = 1
    let b = double(a)
    print(b)
}
";

#[test]
fn a_breakpoint_hits_before_its_statement_runs_and_the_program_goes_on_unchanged() {
    let run = run_debug(CALLS, Setup::lines(&[8]), |_, _, _| Resume::Continue);
    assert_eq!(run.seen.len(), 1);
    assert_eq!(run.seen[0].out, "", "stopped before `print(b)` ran");
    assert_eq!(run.seen[0].locals, ["a = 1", "b = 2"]);
    assert_eq!(run.out, "2\n");
    assert_eq!(run.panic, None);
}

#[test]
fn a_breakpoint_stays_in_the_code_while_the_debugger_runs_and_hits_once_per_trip() {
    use crate::bytecode::Op;
    // The patch is in the code while the debugger is called (the original runs after it, once),
    // so a loop that goes round again meets it again, and each trip hits exactly once.
    let run = run_debug(LOOP, Setup::lines(&[4]), |view, _, _| {
        let top = view.fiber.frames.last().expect("a frame");
        let at = top.pc as usize - 1;
        assert!(
            matches!(top.chunk.code[at].get(), Op::Breakpoint { .. }),
            "{:?}",
            top.chunk.code[at].get()
        );
        Resume::Continue
    });
    assert_eq!(run.seen.len(), 3);
    assert_eq!(run.out, "0\n1\n3\n");
}

#[test]
fn a_breakpoint_in_a_function_that_is_not_compiled_yet_is_put_in_when_it_is() {
    let src = "fn late(n: int) -> int {
    let m = n + 1
    return m * 2
}
fn main() {
    print(\"before\")
    print(late(4))
    print(late(5))
}
";
    // After the load nothing is compiled: not verified yet, and a hit when `late` is first
    // compiled, and again when it is called again.
    let run = run_debug(src, Setup::lines(&[3]), |_, _, _| Resume::Continue);
    assert!(!run.infos[0].verified);
    assert_eq!(trail(&run), ["late:3", "late:3"]);
    assert_eq!(run.seen[0].out, "before\n");
    assert_eq!(run.seen[1].out, "before\n10\n");
    assert_eq!(run.seen[1].locals, ["n = 5", "m = 6"]);
    assert_eq!(run.out, "before\n10\n12\n");
    // Set before the load it is the same.
    let mut setup = Setup::lines(&[3]);
    setup.before_load = true;
    let run = run_debug(src, setup, |_, _, _| Resume::Continue);
    assert_eq!(trail(&run), ["late:3", "late:3"]);
}

#[test]
fn step_over_in_and_out_follow_the_frames() {
    use StepKind::{In, Out, Over};
    let run = run_debug(CALLS, Setup::lines(&[7]), scripted(vec![
        Resume::Step(In),
        Resume::Step(In),
        Resume::Step(In),
    ]));
    // In: into the call, down its statements, back to the caller's next statement.
    assert_eq!(trail(&run), ["main:7", "double:2", "double:3", "main:8"]);
    assert_eq!(run.seen[1].frames, [("double".to_string(), 2), ("main".to_string(), 7)]);
    assert_eq!(run.seen[1].locals, ["x = 1"]);
    assert_eq!(run.seen[2].locals, ["x = 1", "y = 2"]);
    let run = run_debug(CALLS, Setup::lines(&[7]), scripted(vec![Resume::Step(Over), Resume::Step(Over)]));
    assert_eq!(trail(&run), ["main:7", "main:8"]);
    assert_eq!(run.out, "2\n");
    let run = run_debug(CALLS, Setup::lines(&[2]), scripted(vec![Resume::Step(Out), Resume::Step(Over)]));
    assert_eq!(trail(&run), ["double:2", "main:8"]);
    let _ = (In, Out, Over);
}

const MAPPED: &str = "fn main() {
    let xs = [1, 2, 3]
    let ys = xs.map(fn(x) {
        let d = x * 2
        return d
    })
    print(ys)
}
";

#[test]
fn stepping_in_to_a_closure_built_in_stops_in_the_closure_and_hides_the_built_in() {
    let run = run_debug(MAPPED, Setup::lines(&[3]), scripted(vec![Resume::Step(StepKind::In)]));
    assert_eq!(trail(&run), ["main:3", "<closure in main>:4"]);
    // The frames of the prelude's `map` are not on the stack the debugger sees.
    let names: Vec<&str> = run.seen[1].frames.iter().map(|f| f.0.as_str()).collect();
    assert_eq!(names.len(), 2, "{names:?}");
    assert_eq!(names[1], "main");
    assert_eq!(run.seen[1].locals, ["x = 1"]);
    assert_eq!(run.out, "[2, 4, 6]\n");
}

#[test]
fn stepping_over_and_out_across_a_closure_built_in() {
    // Over the whole `map` line: the next statement of `main`.
    let run = run_debug(MAPPED, Setup::lines(&[3]), scripted(vec![Resume::Step(StepKind::Over)]));
    assert_eq!(trail(&run), ["main:3", "main:7"]);
    // Out of the closure: past the built-in's loop, to `main`'s next statement.
    let one = MAPPED.replace("[1, 2, 3]", "[1]");
    let run = run_debug(&one, Setup::lines(&[4]), scripted(vec![Resume::Step(StepKind::Out)]));
    assert_eq!(trail(&run), ["<closure in main>:4", "main:7"]);
}

#[test]
fn a_hit_count_condition_counts_hits_and_acts_on_the_ones_it_names() {
    let at = |hit| Setup {
        bps: vec![BreakpointSpec { line: 4, hit: Some(hit), log: None }],
        before_load: false,
        break_on_panic: false,
        poll: None,
        on_machine: None,
    };
    let run = run_debug(LOOP, at(HitCondition::Equals(2)), |_, _, _| Resume::Continue);
    assert_eq!(run.seen.len(), 1);
    assert_eq!(run.seen[0].locals, ["total = 0", "i = 1"]);
    let run = run_debug(LOOP, at(HitCondition::AtLeast(2)), |_, _, _| Resume::Continue);
    assert_eq!(run.seen.len(), 2);
    let run = run_debug(LOOP, at(HitCondition::MultipleOf(2)), |_, _, _| Resume::Continue);
    assert_eq!(run.seen.len(), 1);
    assert_eq!(run.seen[0].locals, ["total = 0", "i = 1"]);
}

#[test]
fn a_logpoint_writes_its_text_with_the_locals_and_does_not_stop() {
    let setup = Setup {
        bps: vec![BreakpointSpec {
            line: 4,
            hit: None,
            log: Some("i={i} total={total} {{kept}} {nope} {i".to_string()),
        }],
        before_load: false,
        break_on_panic: false,
        poll: None,
        on_machine: None,
    };
    let run = run_debug(LOOP, setup, |_, _, _| Resume::Continue);
    assert!(run.seen.is_empty());
    assert_eq!(
        run.logs,
        [
            "i=0 total=0 {kept} {nope} {i",
            "i=1 total=0 {kept} {nope} {i",
            "i=2 total=1 {kept} {nope} {i",
        ]
    );
    assert_eq!(run.out, "0\n1\n3\n");
}

const RISKY: &str = "fn risky(n: int) -> int {
    defer print(\"cleanup {n}\")
    let d = n - 3
    return 10 / d
}
fn main() {
    let mut total = 0
    for i in 1..5 {
        total += risky(i)
    }
}
";

#[test]
fn break_on_panic_stops_at_the_site_with_its_locals_before_any_cleanup_runs() {
    let mut setup = Setup::lines(&[]);
    setup.break_on_panic = true;
    let run = run_debug(RISKY, setup, |_, _, _| Resume::Continue);
    assert_eq!(run.seen.len(), 1, "{:#?}", run.seen);
    let s = &run.seen[0];
    let Stop::Panic(message) = &s.stop else {
        panic!("{:?}", s.stop)
    };
    assert!(message.contains("zero"), "{message}");
    assert_eq!(s.frames, [("risky".to_string(), 4), ("main".to_string(), 9)]);
    assert_eq!(s.locals, ["n = 3", "d = 0"]);
    // The `defer` of the call that panics has not run; those of the calls before it have.
    assert_eq!(s.out, "cleanup 1\ncleanup 2\n");
    assert!(run.out.ends_with("cleanup 3\n"), "{}", run.out);
    assert!(run.panic.is_some(), "the panic goes on after the stop");
    // Not asked for: no stop.
    let run = run_debug(RISKY, Setup::lines(&[]), |_, _, _| Resume::Continue);
    assert!(run.seen.is_empty());
}

#[test]
fn a_panic_that_passes_a_native_entry_is_reported_once() {
    let src = "struct P { x: int }
impl Show for P {
    fn show(self) -> str {
        return \"{10 / self.x}\"
    }
}
fn main() {
    print(P { x: 0 })
}
";
    let mut setup = Setup::lines(&[]);
    setup.break_on_panic = true;
    let run = run_debug(src, setup, |_, _, _| Resume::Continue);
    assert_eq!(run.seen.len(), 1, "{:#?}", run.seen);
}

#[test]
fn a_running_while_true_pauses_when_another_thread_asks() {
    let src = "fn main() {
    let mut n = 0
    while true {
        n += 1
    }
}
";
    // The debugger polls every 1,024 steps; at the third poll it tells a helper thread, which
    // sends the pause. The program cannot end by itself: it stops at the first safepoint after
    // the request, and the debugger ends it.
    let (go, wait) = std::sync::mpsc::channel::<()>();
    let polls = Rc::new(std::cell::Cell::new(0));
    let polled = Rc::clone(&polls);
    let helper: Rc<RefCell<Option<std::thread::JoinHandle<()>>>> = Rc::default();
    let spawned = Rc::clone(&helper);
    let mut setup = Setup::lines(&[]);
    setup.on_machine = Some(Box::new(move |m| {
        let handle = m.debug_pause_handle();
        *spawned.borrow_mut() = Some(std::thread::spawn(move || {
            wait.recv().expect("the go-ahead");
            handle.pause();
        }));
    }));
    setup.poll = Some(Box::new(move |_| {
        polled.set(polled.get() + 1);
        if polled.get() == 3 {
            go.send(()).expect("the helper listens");
        }
    }));
    let run = run_debug(src, setup, |_, _, _| Resume::Terminate);
    helper.borrow_mut().take().expect("spawned").join().expect("the helper ends");
    assert!(polls.get() >= 3);
    assert_eq!(run.seen.len(), 1);
    assert_eq!(run.seen[0].stop, Stop::Pause);
    assert_eq!(run.seen[0].frames[0].0, "main");
    let n: i64 = run.seen[0].locals[0]
        .strip_prefix("n = ")
        .expect("n")
        .parse()
        .expect("a number");
    assert!(n > 100, "n = {n}");
    assert!(run.panic.expect("ended").contains("debugger"));
}

#[test]
fn a_breakpoint_on_a_while_line_hits_where_the_condition_is_checked() {
    let src = "fn main() {
    let mut i = 0
    while i < 3 {
        i += 1
    }
}
";
    // Three trips and the check that ends the loop: four times, with no extra stop for the
    // statement that holds the loop or for the `break` the lowering adds.
    let run = run_debug(src, Setup::lines(&[3]), |_, _, _| Resume::Continue);
    assert_eq!(trail(&run), ["main:3"; 4]);
    let locals: Vec<&str> = run.seen.iter().map(|s| s.locals[0].as_str()).collect();
    assert_eq!(locals, ["i = 0", "i = 1", "i = 2", "i = 3"]);
}

#[test]
fn a_breakpoint_reaches_the_tail_of_a_closure_that_has_no_statement() {
    let src = "fn main() {
    let xs = [1, 2]
    let ys = xs.map(fn(x) => x * 2)
    print(ys)
}
";
    // The statement of `main` and the closure's tail, once per call.
    let run = run_debug(src, Setup::lines(&[3]), |_, _, _| Resume::Continue);
    assert_eq!(
        trail(&run),
        ["main:3", "<closure in main>:3", "<closure in main>:3"]
    );
    assert_eq!(run.seen[1].locals, ["x = 1"]);
    assert_eq!(run.seen[2].locals, ["x = 2"]);
    // Stepping in lands in the closure at once, tail or not.
    let run = run_debug(src, Setup::lines(&[3]), scripted(vec![Resume::Step(StepKind::In)]));
    assert_eq!(trail(&run)[..2], ["main:3", "<closure in main>:3"]);
}

#[test]
fn a_paused_coroutine_s_frames_and_locals_can_be_inspected() {
    let src = "fn worker(name: str, n: int) {
    let local = n * 10
    wait(1.0)
    print(\"{name} {local}\")
}
fn main() {
    let a = go worker(\"a\", 1)
    let b = go worker(\"b\", 2)
    print(\"started\")
}
";
    let seen_fibers: Rc<RefCell<Vec<(Option<u64>, String, Vec<(String, u32)>, Vec<String>)>>> =
        Rc::default();
    let sink = Rc::clone(&seen_fibers);
    let run = run_debug(src, Setup::lines(&[9]), move |view, _, _| {
        for fiber in view.fibers() {
            let frames = view.frames(fiber.coroutine);
            let top = frames.first().map_or(0, |f| f.index);
            let locals = locals_of(view, fiber.coroutine, top);
            sink.borrow_mut().push((
                fiber.coroutine,
                fiber.name,
                frames.iter().map(|f| (f.name.clone(), f.line.unwrap_or(0))).collect(),
                locals,
            ));
        }
        Resume::Continue
    });
    assert_eq!(run.seen.len(), 1);
    let fibers = seen_fibers.borrow();
    // The stopped computation first, then the two coroutines, each paused in its `wait`.
    assert_eq!(fibers.len(), 3);
    assert_eq!(fibers[0].1, "main");
    assert_eq!(fibers[0].2, [("main".to_string(), 9)]);
    let mut workers: Vec<_> = fibers[1..].iter().collect();
    workers.sort_by_key(|f| f.3[0].clone());
    assert_eq!(workers[0].1, "worker");
    assert_eq!(workers[0].2[0], ("worker".to_string(), 3));
    assert_eq!(workers[0].3, ["name = \"a\"", "n = 1", "local = 10"]);
    assert_eq!(workers[1].3, ["name = \"b\"", "n = 2", "local = 20"]);
    assert!(workers[0].0.is_some() && workers[0].0 != workers[1].0);
}

#[test]
fn values_show_structurally_with_their_types_and_no_program_code_runs() {
    let src = "struct P { x: int, y: int }
impl Show for P {
    fn show(self) -> str {
        print(\"SHOW RAN\")
        return \"shown\"
    }
}
enum Shape { Circle(radius: float), Quit }
class Node { name: str, next: Node? }
fn main() {
    let p = P { x: 1, y: 2 }
    let s = Shape.Circle(2.5)
    let xs = [p, p]
    let m = {\"a\": 1}
    let t = (1, \"two\")
    let n = Node { name: \"n\", next: none }
    let o: int? = some(3)
    print(\"end\")
}
";
    type Vars = Vec<(String, String, String, bool)>;
    let vars: Rc<RefCell<Vars>> = Rc::default();
    let kids: Rc<RefCell<Vec<(String, String, String)>>> = Rc::default();
    let (sink, kid_sink) = (Rc::clone(&vars), Rc::clone(&kids));
    let run = run_debug(src, Setup::lines(&[18]), move |view, _, _| {
        let top = view.frames(None)[0].index;
        let scopes = view.scopes(None, top);
        for v in view.children(scopes[0].handle, 0, 100) {
            if v.name == "xs"
                && let Some(h) = v.handle
            {
                for k in view.children(h, 0, 100) {
                    let inner = k.handle.map(|h| view.children(h, 0, 10)).unwrap_or_default();
                    for i in inner {
                        kid_sink.borrow_mut().push((i.name, i.value, i.ty));
                    }
                }
            }
            sink.borrow_mut().push((v.name, v.value, v.ty, v.handle.is_some()));
        }
        Resume::Continue
    });
    assert_eq!(run.out, "end\n", "a user `Show` was run");
    let vars = vars.borrow();
    let get = |name: &str| vars.iter().find(|v| v.0 == name).unwrap_or_else(|| panic!("{name}: {vars:?}")).clone();
    assert_eq!(get("p"), ("p".into(), "P { x: 1, y: 2 }".into(), "P".into(), true));
    assert_eq!(get("s"), ("s".into(), "Circle(radius: 2.5)".into(), "Shape".into(), true));
    assert_eq!(get("xs").1, "[P { x: 1, y: 2 }, P { x: 1, y: 2 }]");
    assert_eq!(get("xs").2, "List[P]");
    assert_eq!(get("m").1, "{\"a\": 1}");
    assert_eq!(get("t").1, "(1, \"two\")");
    assert_eq!(get("n").1, "Node { name: \"n\", next: none }");
    assert_eq!(get("n").2, "Node");
    assert_eq!(get("o").1, "some(3)");
    assert_eq!(get("o").2, "int?");
    // The children of a list element are its fields, with their types.
    assert_eq!(
        kids.borrow()[..2],
        [
            ("x".to_string(), "1".to_string(), "int".to_string()),
            ("y".to_string(), "2".to_string(), "int".to_string())
        ]
    );
}

/// Programs for the comparison of debug mode with the normal one.
const PROGRAMS: &[&str] = &[
    LOOP,
    CALLS,
    MAPPED,
    RISKY,
    "fn main() {
    let mut i = 0
    while i < 50 {
        i += 1
        if i % 7 == 0 {
            continue
        }
        if i > 40 {
            break
        }
    }
    print(i)
    let xs = (0..300).to_list()
    let ys = xs.map(fn(x) => x + 1).filter(fn(x) => x % 2 == 0)
    print(ys.len())
    let total = xs.fold(0, fn(a, b) => a + b)
    print(total)
    for x in xs {
        if x > 100 { break }
    }
    defer print(\"done\")
    let mut s = 0
    for k in 0..=20 {
        s += k
    }
    print(s)
}
",
];

#[test]
fn debug_mode_changes_neither_the_output_nor_the_steps() {
    use crate::source::{Options, run_source_with};
    for (i, src) in PROGRAMS.iter().enumerate() {
        let normal = run_source_with(src, &Options::default());
        let debug = run_source_with(
            src,
            &Options {
                debug: true,
                ..Options::default()
            },
        );
        assert_eq!(debug.stdout, normal.stdout, "program {i}");
        assert_eq!(debug.panic, normal.panic, "program {i}");
        assert_eq!(debug.trace, normal.trace, "program {i}");
        assert_eq!(debug.steps, normal.steps, "program {i}: steps");
    }
}
