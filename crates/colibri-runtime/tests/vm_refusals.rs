//! How the bytecode VM stops at its refusal of a host call that can wait while a coroutine is
//! alive (vm-plan step 12 lifts it). A refusal stops the program where it is met, whichever
//! coroutine meets it and whatever resumed that coroutine: `go`, a wait or a join on the main
//! fiber, `task.tick` or a tick of the host. Nothing the program wrote runs for it (no `defer`,
//! no `Drop` hook, not even at shutdown), and it carries the trace of the fiber that raises it.
//!
//! Every test runs on the VM only: the tree-walker has no such refusal, which the host
//! functions `blocky` (unmarked, so it may wait) and `fine` (marked `no_await`) stand for.

use std::cell::Cell;
use std::rc::Rc;

use colibri::{Ctx, EngineKind, Error, HostType, SharedBuffer, TypeBuilder, Vm};
use colibri_runtime as colibri;

const REFUSAL: &str =
    "the bytecode VM does not support a host function that can wait, beside running coroutines yet";

fn vm() -> (Vm, SharedBuffer) {
    let out = SharedBuffer::new();
    let mut vm = Vm::builder()
        .engine(EngineKind::Bytecode)
        .stdout(Box::new(out.clone()))
        .build();
    vm.register_fn("blocky", || 7i64).unwrap();
    vm.register_fn("fine", || 8i64).unwrap().no_await();
    (vm, out)
}

fn text(error: &Error) -> String {
    error.render(false)
}

const DROPS: &str = "class R {\n    n: str\n}\nimpl Drop for R {\n    fn drop(self) {\n        print(\"drop {self.n}\")\n    }\n}\n";

/// (a) A refusal belongs to the resumption that met it. The tick that meets it fails with it
/// and the ticks after it run what is left, whatever it starts: no state carries over.
#[test]
fn a_refusal_in_one_tick_does_not_spill_into_the_next() {
    let (mut vm, out) = vm();
    vm.load_source(
        "game.colibri",
        r#"
fn co() {
    wait(1.0)
    print("co got {blocky()}")
}
fn fine_job() {
    print("fine runs")
}
fn other() {
    wait(5.0)
    print("other wakes")
    let c = go fine_job()
    print("other started fine")
}
fn main() {
    go co()
    go other()
}
"#,
    )
    .unwrap();
    vm.run_main().unwrap();
    let first = vm.tick(1.0).expect_err("`co` meets the refusal");
    assert!(text(&first).contains(REFUSAL), "{}", text(&first));
    assert_eq!(out.contents(), "");
    vm.tick(10.0)
        .expect("the next tick runs what is left: no refusal is left over");
    assert_eq!(
        out.contents(),
        "other wakes\nfine runs\nother started fine\n"
    );
    vm.tick(10.0).expect("and the one after that");
    assert_eq!(vm.coroutine_count(), 0);
}

/// (b) The refusal has a trace: where the fiber that raised it was, here `main` at its `go`
/// (the new coroutine met the refusal while it ran its first part inside the `go`).
#[test]
fn a_refusal_carries_the_trace_of_the_fiber_that_raises_it() {
    let (mut vm, out) = vm();
    vm.load_source(
        "game.colibri",
        "fn worker() {\n    blocky()\n}\nfn main() {\n    print(\"start\")\n    let _c = go worker()\n    print(\"after go\")\n}\n",
    )
    .unwrap();
    let error = vm.run_main().expect_err("the refusal");
    let shown = text(&error);
    assert!(shown.contains(REFUSAL), "{shown}");
    assert_eq!(out.contents(), "start\n");
    assert!(
        shown.contains("at main (game.colibri:6:"),
        "the trace names main at the `go`: {shown}"
    );
}

/// (c) A refusal met at a later resumption stops `main` at the instruction that resumed the
/// coroutine, instead of letting `main` see a failed coroutine and go on.
#[test]
fn a_refusal_at_a_later_resumption_stops_main_where_it_waits() {
    let (mut vm, out) = vm();
    vm.load_source(
        "game.colibri",
        "fn worker() {\n    wait(0.0)\n    blocky()\n}\nfn main() {\n    let c = go worker()\n    print(\"main waits\")\n    let res = c.join_result()\n    print(\"joined {res.is_err()}\")\n    wait(0.0)\n    print(\"main end\")\n}\n",
    )
    .unwrap();
    let error = vm.run_main().expect_err("the refusal");
    let shown = text(&error);
    assert!(shown.contains(REFUSAL), "{shown}");
    assert_eq!(
        out.contents(),
        "main waits\n",
        "main does not run on after the join"
    );
    assert!(shown.contains("at main (game.colibri:8:"), "{shown}");
}

/// The same for a wait on the main fiber, for `task.tick`, and for a coroutine that started
/// the one that met the refusal.
#[test]
fn a_refusal_stops_the_main_fiber_at_a_wait_a_task_tick_and_a_nested_go() {
    for (name, main, line) in [
        (
            "wait",
            "    let _c = go worker()\n    wait(0.0)\n    print(\"not reached\")\n",
            11,
        ),
        (
            "task.tick",
            "    let _c = go worker()\n    task.tick(0.0)\n    print(\"not reached\")\n",
            11,
        ),
        (
            "nested go",
            "    let _c = go outer()\n    print(\"not reached\")\n",
            0,
        ),
    ] {
        let (mut vm, out) = vm();
        let src = format!(
            "fn worker() {{\n    wait(0.0)\n    blocky()\n}}\nfn outer() {{\n    let _w = go bad()\n    wait(0.0)\n}}\nfn main() {{\n{main}}}\nfn bad() {{\n    blocky()\n}}\n"
        );
        vm.load_source("game.colibri", &src).unwrap();
        let result = vm.run_main();
        let error = match result {
            Err(e) => e,
            Ok(()) => vm.tick(3.0).expect_err("the refusal comes with the tick"),
        };
        let shown = text(&error);
        assert!(shown.contains(REFUSAL), "{name}: {shown}");
        assert_eq!(out.contents(), "", "{name}");
        if line != 0 {
            assert!(
                shown.contains(&format!("at main (game.colibri:{line}:")),
                "{name}: {shown}"
            );
        }
    }
}

/// What the refusal is on the main fiber, pinned as the replacement of the old test of the
/// refusal of a construct: the exact message, output that stops, no `defer` and no `Drop`
/// before it run (not at shutdown either), and the trace at the call.
#[test]
fn a_refusal_on_the_main_fiber_stops_everything_where_it_is_met() {
    let (mut vm, out) = vm();
    vm.load_source(
        "game.colibri",
        &format!(
            "{DROPS}fn sleeper() {{\n    let _r = R {{ n: \"co\" }}\n    wait(1.0)\n}}\nfn main() {{\n    let _r = R {{ n: \"main\" }}\n    defer print(\"deferred\")\n    print(\"before\")\n    let _c = go sleeper()\n    blocky()\n    print(\"after\")\n}}\n"
        ),
    )
    .unwrap();
    let error = vm.run_main().expect_err("the refusal");
    let shown = text(&error);
    assert!(
        shown.starts_with(&format!("panic: {REFUSAL}")),
        "the exact message: {shown}"
    );
    assert!(shown.contains("at main (game.colibri:18:"), "{shown}");
    // Nothing runs after it: not the pads of `main`, and not the cancellation of the sleeping
    // coroutine or the module values when the program ends.
    vm.shutdown();
    drop(vm);
    assert_eq!(out.contents(), "before\n");
}

/// The refusal also stops the cancellation between runs: `release_globals` (what `colibri test`
/// runs between tests) cancels the live coroutines without running their pads.
#[test]
fn coroutines_are_not_unwound_through_pads_after_a_refusal() {
    let (mut vm, out) = vm();
    vm.load_source(
        "game.colibri",
        &format!(
            "{DROPS}fn sleeper() {{\n    let _r = R {{ n: \"co\" }}\n    defer print(\"co deferred\")\n    wait(1.0)\n}}\nfn main() {{\n    let _c = go sleeper()\n    blocky()\n}}\n"
        ),
    )
    .unwrap();
    vm.run_main().expect_err("the refusal");
    vm.shutdown();
    assert_eq!(out.contents(), "");
}

/// An embedder's host-class method can be marked `no_await`, and then it is called beside
/// coroutines like a host function; an unmarked one is refused.
struct Gauge {
    n: Rc<Cell<i64>>,
}

impl HostType for Gauge {
    const NAME: &'static str = "Gauge";
    fn describe(d: &mut TypeBuilder<'_, Self>) {
        d.static_fn("new", [], || Gauge {
            n: Rc::new(Cell::new(0)),
        });
        d.method("read", [], |this: &Gauge| this.n.get()).no_await();
        d.method_mut("bump", [], |_cx: &mut Ctx, this: &mut Gauge| {
            this.n.set(this.n.get() + 1)
        })
        .no_await();
        d.method("slow", [], |this: &Gauge| this.n.get());
    }
}

#[test]
fn a_host_class_method_can_be_marked_no_await() {
    let (mut vm, out) = vm();
    vm.register_type::<Gauge>().unwrap();
    vm.load_source(
        "game.colibri",
        "fn tick(g: Gauge) {\n    wait(1.0)\n    g.bump()\n    print(g.read())\n}\nfn main() {\n    let g = Gauge.new()\n    let _c = go tick(g)\n    g.bump()\n    print(g.read())\n    task.tick(2.0)\n}\n",
    )
    .unwrap();
    vm.run_main()
        .expect("marked methods run beside a coroutine");
    assert_eq!(out.contents(), "1\n2\n");

    let (mut vm, out) = vm_with_gauge();
    vm.load_source(
        "game.colibri",
        "fn sleeper() {\n    wait(1.0)\n}\nfn main() {\n    let g = Gauge.new()\n    let _c = go sleeper()\n    print(\"before\")\n    print(g.slow())\n}\n",
    )
    .unwrap();
    let shown = text(&vm.run_main().expect_err("an unmarked method is refused"));
    assert!(shown.contains(REFUSAL), "{shown}");
    assert_eq!(out.contents(), "before\n");
}

fn vm_with_gauge() -> (Vm, SharedBuffer) {
    let (mut vm, out) = vm();
    vm.register_type::<Gauge>().unwrap();
    (vm, out)
}
