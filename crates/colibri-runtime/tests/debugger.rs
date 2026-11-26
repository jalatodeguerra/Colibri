//! The debugger interface of the `Vm` (hidden: `set_debugger`, `debug_set_breakpoints`), which
//! the Debug Adapter Protocol server drives. The cases that need the machine's internals are in
//! `colibri-vm` (`debug_tests`); these use only what an embedder or the adapter sees.

use std::cell::RefCell;
use std::rc::Rc;

use colibri_runtime as colibri;
use colibri_runtime::debug::{
    BreakpointSpec, DebugView, Debugger, HitCondition, Resume, StepKind, Stop,
};

type Seen = Rc<RefCell<Vec<(Stop, Vec<(String, u32)>, Vec<String>)>>>;

struct Script {
    seen: Seen,
    logs: Rc<RefCell<Vec<String>>>,
    next: Box<dyn FnMut(usize) -> Resume>,
}

impl Debugger for Script {
    fn stopped(&mut self, view: &mut DebugView<'_>, stop: &Stop) -> Resume {
        let frames = view.frames(None);
        let named = frames
            .iter()
            .map(|f| (f.name.clone(), f.line.unwrap_or(0)))
            .collect();
        let top = frames.first().map_or(0, |f| f.index);
        let scopes = view.scopes(None, top);
        let locals = view
            .children(scopes[0].handle, 0, 100)
            .into_iter()
            .map(|v| format!("{} = {}", v.name, v.value))
            .collect();
        let n = self.seen.borrow().len();
        self.seen.borrow_mut().push((stop.clone(), named, locals));
        (self.next)(n)
    }

    fn output(&mut self, text: &str) {
        self.logs.borrow_mut().push(text.to_string());
    }
}

struct Session {
    vm: colibri::Vm,
    out: colibri::SharedBuffer,
    seen: Seen,
    logs: Rc<RefCell<Vec<String>>>,
}

fn session(next: impl FnMut(usize) -> Resume + 'static) -> Session {
    let out = colibri::SharedBuffer::new();
    let mut vm = colibri::Vm::builder().stdout(Box::new(out.clone())).build();
    let seen: Seen = Rc::default();
    let logs = Rc::default();
    vm.set_debugger(Box::new(Script {
        seen: Rc::clone(&seen),
        logs: Rc::clone(&logs),
        next: Box::new(next),
    }));
    Session {
        vm,
        out,
        seen,
        logs,
    }
}

fn bp(line: u32) -> BreakpointSpec {
    BreakpointSpec {
        line,
        hit: None,
        log: None,
    }
}

const PROGRAM: &str = "\
fn square(n: int) -> int {
    let r = n * n
    return r
}
fn main() {
    let mut total = 0
    for i in 1..4 {
        total += square(i)
    }
    print(total)
}
";

#[test]
fn breakpoints_set_by_file_name_hit_in_main_and_in_what_it_calls() {
    let mut s = session(|_| Resume::Continue);
    s.vm.load_source("p.colibri", PROGRAM).unwrap();
    let set = s.vm.debug_set_breakpoints("p.colibri", &[bp(3), bp(8)]).unwrap();
    assert_eq!(set.len(), 2);
    assert!(s.vm.debug_set_breakpoints("other.colibri", &[bp(1)]).is_none());
    s.vm.run_main().unwrap();
    let seen = s.seen.borrow();
    let trail: Vec<(String, u32)> = seen.iter().map(|(_, f, _)| f[0].clone()).collect();
    let line = |name: &str, n| (name.to_string(), n);
    assert_eq!(
        trail,
        [
            line("main", 8),
            line("square", 3),
            line("main", 8),
            line("square", 3),
            line("main", 8),
            line("square", 3)
        ]
    );
    assert_eq!(seen[1].2, ["n = 1", "r = 1"]);
    assert_eq!(seen[5].2, ["n = 3", "r = 9"]);
    assert_eq!(s.out.contents(), "14\n");
}

#[test]
fn a_debugger_changes_neither_the_output_nor_the_steps() {
    let run = |attached: bool| {
        let out = colibri::SharedBuffer::new();
        let mut vm = colibri::Vm::builder().stdout(Box::new(out.clone())).build();
        if attached {
            vm.set_debugger(Box::new(Script {
                seen: Rc::default(),
                logs: Rc::default(),
                next: Box::new(|_| Resume::Continue),
            }));
        }
        vm.load_source("p.colibri", PROGRAM).unwrap();
        let mut steps = vm.steps_used();
        vm.run_main().unwrap();
        steps += vm.steps_used();
        (out.contents(), steps)
    };
    assert_eq!(run(true), run(false));
}

#[test]
fn a_hit_count_and_a_logpoint_through_the_vm() {
    let mut s = session(|_| Resume::Continue);
    s.vm.load_source("p.colibri", PROGRAM).unwrap();
    s.vm.debug_set_breakpoints(
        "p.colibri",
        &[
            BreakpointSpec {
                line: 3,
                hit: Some(HitCondition::Equals(2)),
                log: None,
            },
            BreakpointSpec {
                line: 8,
                hit: None,
                log: Some("i={i} total={total}".into()),
            },
        ],
    )
    .unwrap();
    s.vm.run_main().unwrap();
    assert_eq!(s.seen.borrow().len(), 1);
    assert_eq!(s.seen.borrow()[0].2, ["n = 2", "r = 4"]);
    assert_eq!(*s.logs.borrow(), ["i=1 total=0", "i=2 total=1", "i=3 total=5"]);
}

#[test]
fn a_breakpoint_in_a_coroutine_that_the_event_loop_resumes_hits() {
    let src = "\
fn worker(n: int) {
    wait(1.0)
    let doubled = n * 2
    print(doubled)
}
fn main() {
    go worker(21)
    print(\"main done\")
}
";
    let mut s = session(|_| Resume::Continue);
    s.vm.load_source("w.colibri", src).unwrap();
    s.vm.debug_set_breakpoints("w.colibri", &[bp(4)]).unwrap();
    s.vm.run_main().unwrap();
    assert!(s.seen.borrow().is_empty(), "not reached before the wait ends");
    s.vm.run_event_loop().unwrap();
    let seen = s.seen.borrow();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].1[0], ("worker".to_string(), 4));
    assert_eq!(seen[0].2, ["n = 21", "doubled = 42"]);
    assert_eq!(s.out.contents(), "main done\n42\n");
}

#[test]
fn break_on_panic_and_stepping_through_the_vm() {
    let src = "\
fn boom(n: int) -> int {
    return 10 / n
}
fn main() {
    let a = 1
    print(boom(a - 1))
}
";
    let mut s = session(|n| if n == 0 { Resume::Step(StepKind::In) } else { Resume::Continue });
    s.vm.debug_break_on_panic(true);
    s.vm.load_source("b.colibri", src).unwrap();
    s.vm.debug_set_breakpoints("b.colibri", &[bp(6)]).unwrap();
    let failure = s.vm.run_main();
    assert!(failure.is_err());
    let seen = s.seen.borrow();
    // The breakpoint, a step into `boom`, then the panic at its site.
    assert_eq!(seen.len(), 3, "{seen:?}");
    assert_eq!(seen[0].1[0], ("main".to_string(), 6));
    assert_eq!(seen[1].1[0], ("boom".to_string(), 2));
    assert!(matches!(&seen[2].0, Stop::Panic(m) if m.contains("zero")));
    assert_eq!(seen[2].2, ["n = 0"]);
}
