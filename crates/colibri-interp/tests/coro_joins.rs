//! Waiting in `join_result`, `join` and `join_all` costs nothing while the target lives: the
//! waiter is registered with what it waits for and woken when that ends, never polled
//! (ARCHITECTURE §16). Counted in resumptions and in what `next_wake` tells an event loop.

use std::cell::RefCell;
use std::io::Write;
use std::rc::Rc;

use colibri_interp::{Fiber, Interp, Value};
use colibri_syntax::{FileId, parse_file};

#[derive(Clone, Default)]
struct Out(Rc<RefCell<Vec<u8>>>);

impl Write for Out {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct Machine {
    interp: Interp,
    fiber: Fiber,
    program: colibri_check::Program,
    out: Out,
}

fn machine(src: &str) -> Machine {
    let (ast, diags) = parse_file(FileId(0), src);
    assert!(diags.is_empty(), "{diags:?}");
    let checked = colibri_check::check(&ast);
    let program = checked
        .program
        .unwrap_or_else(|| panic!("{:?}", checked.diagnostics));
    let out = Out::default();
    let mut interp = Interp::new(Box::new(out.clone()), 200);
    interp.set_native_stack_budget(64 << 20);
    interp.load(&program);
    let mut fiber = Fiber::new();
    interp
        .run_module(&mut fiber)
        .unwrap_or_else(|e| panic!("module: {}", e.message));
    Machine {
        interp,
        fiber,
        program,
        out,
    }
}

impl Machine {
    fn call(&mut self, name: &str) {
        let id = self.program.fn_by_name(name).expect("function exists");
        self.interp
            .call(&mut self.fiber, id, Vec::<Value>::new())
            .unwrap_or_else(|e| panic!("{name}: {}", e.message));
    }

    fn tick(&mut self, dt: f64) {
        self.interp
            .tick(&mut self.fiber, dt)
            .unwrap_or_else(|e| panic!("tick: {}", e.message));
    }

    fn printed(&self) -> String {
        String::from_utf8_lossy(&self.out.0.borrow()).into_owned()
    }
}

/// How many waiters and targets the tests start.
const N: usize = 1000;

/// `N` pairs: a target that waits on a signal, and a coroutine waiting for that target with
/// `how` (`c.join_result()` or `c.join()`), which prints its number when it is woken.
fn pairs(how: &str) -> String {
    format!(
        "let gate = Signal()\n\
         fn target() {{\n    wait_for(gate)\n}}\n\
         fn waiter(n: int, c: Coroutine[()]) {{\n    {how}\n    print(n)\n}}\n\
         fn main() {{\n    for i in 0..{N} {{\n        let c = go target()\n        go waiter(i, c)\n    }}\n}}\n\
         fn fire() {{\n    gate.emit()\n}}\n"
    )
}

/// While their targets live, the waiters are not resumed by any number of ticks, and the
/// event loop is told there is nothing to poll for. When the targets end, every waiter is
/// woken, in the order they began waiting.
fn waiters_cost_nothing(how: &str) {
    let mut m = machine(&pairs(how));
    m.call("main");
    assert_eq!(m.interp.coroutine_count(), 2 * N);
    let after_start = m.interp.coroutine_resumptions();
    for _ in 0..100 {
        m.tick(0.01);
    }
    assert_eq!(
        m.interp.coroutine_resumptions(),
        after_start,
        "a waiter was resumed without its target having ended"
    );
    assert_eq!(
        m.interp.next_wake(0.001),
        None,
        "nothing is due, so the event loop has nothing to poll for"
    );
    m.call("fire");
    // The targets end in the next tick; their waiters are woken for the one after.
    m.tick(0.0);
    assert_eq!(m.interp.coroutine_resumptions(), after_start + N as u64);
    assert_eq!(
        m.interp.next_wake(0.001),
        Some(0.0),
        "the woken are due now"
    );
    m.tick(0.0);
    assert_eq!(m.interp.coroutine_resumptions(), after_start + 2 * N as u64);
    assert_eq!(m.interp.coroutine_count(), 0);
    let expected: String = (0..N).map(|i| format!("{i}\n")).collect();
    assert_eq!(m.printed(), expected, "woken in the order they waited");
}

#[test]
fn a_thousand_join_result_waiters_cost_no_resumptions_and_no_polling() {
    waiters_cost_nothing("let _ = c.join_result()");
}

/// A waiter that is cancelled while it waits leaves the target's list at once: nothing stays
/// registered, the target's end wakes nobody (no resumption of a dead coroutine), and the
/// waiters that stay are woken in order.
#[test]
fn a_cancelled_join_result_waiter_leaves_no_entry_behind() {
    let src = "let gate = Signal()\n\
        class Box { c: Coroutine[()]? = none }\n\
        fn target() {\n    wait_for(gate)\n}\n\
        fn waiter(n: int, c: Coroutine[()]) {\n    defer print(\"unwound {n}\")\n    let _ = c.join_result()\n    print(\"woke {n}\")\n}\n\
        fn main() {\n    let t = go target()\n    go waiter(1, t)\n    let w2 = go waiter(2, t)\n    go waiter(3, t)\n    w2.cancel()\n}\n\
        fn fire() {\n    gate.emit()\n}\n";
    let mut m = machine(src);
    m.call("main");
    // The target waits for the gate; waiters 1 and 3 wait for the target; 2 is gone.
    assert_eq!(m.interp.coroutine_waiters(), 3, "gate + two joiners");
    assert_eq!(m.printed(), "unwound 2\n");
    m.call("fire");
    m.tick(0.0);
    assert_eq!(m.interp.coroutine_waiters(), 0);
    let before = m.interp.coroutine_resumptions();
    m.tick(0.0);
    assert_eq!(m.interp.coroutine_resumptions(), before + 2);
    assert_eq!(
        m.printed(),
        "unwound 2\nwoke 1\nunwound 1\nwoke 3\nunwound 3\n"
    );
    assert_eq!(m.interp.coroutine_count(), 0);
}

/// A coroutine that joins one that has already ended does not suspend: it runs to its end in
/// the resumption that started it and registers nothing.
#[test]
fn join_result_on_a_finished_coroutine_does_not_suspend() {
    let src = "fn quick() -> int {\n    return 5\n}\n\
        fn joiner(c: Coroutine[int]) {\n    print(c.join_result().expect(\"v\"))\n}\n\
        fn main() {\n    let c = go quick()\n    go joiner(c)\n}\n";
    let mut m = machine(src);
    m.call("main");
    assert_eq!(m.printed(), "5\n");
    assert_eq!(m.interp.coroutine_count(), 0);
    assert_eq!(m.interp.coroutine_waiters(), 0);
    assert_eq!(
        m.interp.coroutine_resumptions(),
        2,
        "the two starts, nothing else"
    );
}

/// Many waiters on one target are woken in the order they began waiting, all in the tick after
/// the target ends, and the target keeps one list: nothing is left registered.
fn one_target_many_waiters(how: &str) {
    let src = format!(
        "let gate = Signal()\n\
         fn target() {{\n    wait_for(gate)\n}}\n\
         fn waiter(n: int, c: Coroutine[()]) {{\n    {how}\n    print(n)\n}}\n\
         fn main() {{\n    let c = go target()\n    for i in 0..{N} {{\n        go waiter(i, c)\n    }}\n}}\n\
         fn fire() {{\n    gate.emit()\n}}\n"
    );
    let mut m = machine(&src);
    m.call("main");
    assert_eq!(m.interp.coroutine_waiters(), N + 1);
    let after_start = m.interp.coroutine_resumptions();
    for _ in 0..100 {
        m.tick(0.01);
    }
    assert_eq!(m.interp.coroutine_resumptions(), after_start);
    assert_eq!(m.interp.next_wake(0.001), None);
    m.call("fire");
    m.tick(0.0);
    assert_eq!(m.printed(), "", "the waiters are woken for the next tick");
    m.tick(0.0);
    let expected: String = (0..N).map(|i| format!("{i}\n")).collect();
    assert_eq!(m.printed(), expected, "woken in the order they waited");
    assert_eq!(m.interp.coroutine_waiters(), 0);
    assert_eq!(m.interp.coroutine_count(), 0);
}

#[test]
fn many_join_result_waiters_on_one_target_wake_in_waiting_order() {
    one_target_many_waiters("let _ = c.join_result()");
}
