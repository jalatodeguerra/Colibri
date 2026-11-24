//! How the bytecode VM stops at a refusal (`the bytecode VM does not support ...`). Nothing a
//! checked program says is refused any more, so the host function `blocky` stands in for the
//! feature the VM lacks by failing with the text of a refusal. A refusal stops the program where it
//! is met, whichever
//! coroutine meets it and whatever resumed that coroutine: `go`, a wait or a join on the main
//! fiber, `task.tick` or a tick of the host. Nothing the program wrote runs for it (no `defer`,
//! no `Drop` hook, not even at shutdown), and it carries the trace of the fiber that raises it.
//!
//! Every test runs on the VM only: the tree-walker has no such refusal.

use colibri::{Ctx, EngineKind, Error, HostType, SharedBuffer, TypeBuilder, Vm};
use colibri_runtime as colibri;

const REFUSAL: &str = "the bytecode VM does not support a stand-in feature yet";

fn vm() -> (Vm, SharedBuffer) {
    let out = SharedBuffer::new();
    let mut vm = Vm::builder()
        .engine(EngineKind::Bytecode)
        .stdout(Box::new(out.clone()))
        .build();
    vm.register_fn("blocky", |cx: &mut Ctx| -> i64 { cx.raise(REFUSAL) })
        .unwrap();
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

/// A coroutine's entry arguments are not released after a refusal either: no `Drop` hook runs,
/// whether the argument was passed to the coroutine or captured by the closure it runs.
#[test]
fn a_refusal_does_not_release_the_arguments_of_a_coroutine() {
    let (mut vm, out) = vm();
    vm.load_source(
        "game.colibri",
        &format!(
            "{DROPS}fn sleeper(a: R) {{\n    defer print(\"co deferred {{a.n}}\")\n    wait(1.0)\n}}\nfn main() {{\n    let h = R {{ n: \"captured\" }}\n    let f = fn() => sleeper(R {{ n: \"inner {{h.n}}\" }})\n    let _c = go sleeper(R {{ n: \"arg\" }})\n    let _d = go f()\n    print(\"before\")\n    blocky()\n    print(\"after\")\n}}\n"
        ),
    )
    .unwrap();
    let e = vm.run_main().expect_err("the refusal");
    assert!(text(&e).contains(REFUSAL), "{}", text(&e));
    vm.shutdown();
    assert_eq!(out.contents(), "before\n");
}

/// (R1) `c.cancel()` resumes the coroutine to unwind it, and its `defer` meets the refusal: the
/// program stops at the `cancel`, not at the next `go` after it.
#[test]
fn a_refusal_met_by_a_cancelled_coroutine_stops_main_at_the_cancel() {
    let (mut vm, out) = vm();
    vm.load_source(
        "game.colibri",
        r#"
fn worker() {
    defer print("defer got {blocky()}")
    wait(10.0)
}
fn fine_job() {
    print("fine runs")
}
fn main() {
    let c = go worker()
    print("cancel")
    c.cancel()
    print("after cancel")
    let _f = go fine_job()
    print("after go")
}
"#,
    )
    .unwrap();
    let e = vm.run_main().expect_err("the refusal");
    let t = text(&e);
    assert!(t.contains(REFUSAL), "{t}");
    assert!(t.contains("main"), "{t}");
    assert_eq!(out.contents(), "cancel\n");
}

/// (R1) A `CoGroup` that dies cancels its coroutines at the end of a statement: a refusal met
/// in a member's `defer` stops the program there, not at the next wait.
#[test]
fn a_refusal_met_when_a_group_dies_stops_the_program_where_it_dies() {
    let (mut vm, out) = vm();
    vm.load_source(
        "game.colibri",
        r#"
fn member() {
    defer print("defer got {blocky()}")
    wait(10.0)
}
fn spawn() {
    let g = CoGroup()
    let _m = g.go(member())
    print("group dies at return")
}
fn main() {
    spawn()
    print("after spawn")
    wait(0.1)
    print("after wait")
}
"#,
    )
    .unwrap();
    let e = vm.run_main().expect_err("the refusal");
    assert!(text(&e).contains(REFUSAL), "{}", text(&e));
    assert_eq!(out.contents(), "group dies at return\n");
}

/// (R1) A refusal never leaks into the next entry of the host: the entry that met it fails with
/// it (here the coroutine was cancelled by `main`'s last act) and the calls after it run.
#[test]
fn a_refusal_from_a_cancel_is_the_error_of_its_entry_only() {
    let (mut vm, out) = vm();
    vm.load_source(
        "game.colibri",
        r#"
fn worker() {
    defer print("defer got {blocky()}")
    wait(10.0)
}
fn fine_job() {
    print("fine runs")
}
fn later() {
    print("later starts")
    let _c = go fine_job()
    print("later ends")
}
fn main() {
    let c = go worker()
    c.cancel()
    print("main ends")
}
"#,
    )
    .unwrap();
    let e = vm
        .run_main()
        .expect_err("the entry that met the refusal fails with it");
    assert!(text(&e).contains(REFUSAL), "{}", text(&e));
    out.take();
    let later = vm.get_fn::<(), ()>("later").unwrap();
    later
        .call(&mut vm, ())
        .expect("the next entry is not refused");
    assert_eq!(out.take(), "later starts\nfine runs\nlater ends\n");
    later.call(&mut vm, ()).expect("nor the one after");
}

/// (R1) The host cancelling coroutines has nobody to raise a refusal to; it does not wait for
/// the next entry to meet it.
#[test]
fn a_refusal_met_by_a_host_cancel_does_not_reach_the_next_entry() {
    let (mut vm, out) = vm();
    vm.load_source(
        "game.colibri",
        r#"
fn worker() {
    defer print("defer got {blocky()}")
    wait(10.0)
}
fn fine_job() {
    print("fine runs")
}
fn main() {
    go worker()
}
"#,
    )
    .unwrap();
    vm.run_main().unwrap();
    vm.cancel_coroutines();
    out.take();
    let main = vm.get_fn::<(), ()>("fine_job").unwrap();
    main.call(&mut vm, ()).expect("the entry is not refused");
    assert_eq!(out.take(), "fine runs\n");
}

/// A function or method marked `no_await` is called from a coroutine on the caller's stack, one that
/// is not marked on a stack of its own, where it can wait for a background operation: what
/// `Ctx::can_suspend` reads inside it.
struct Gauge;

impl HostType for Gauge {
    const NAME: &'static str = "Gauge";
    fn describe(d: &mut TypeBuilder<'_, Self>) {
        d.static_fn("new", [], || Gauge);
        d.method_mut("marked", [], |cx: &mut Ctx, _this: &mut Gauge| {
            cx.can_suspend()
        })
        .no_await();
        d.method_mut("unmarked", [], |cx: &mut Ctx, _this: &mut Gauge| {
            cx.can_suspend()
        });
    }
}

#[test]
fn a_host_class_method_can_be_marked_no_await() {
    let (mut vm, out) = vm();
    vm.register_type::<Gauge>().unwrap();
    vm.register_fn("fn_marked", |cx: &mut Ctx| cx.can_suspend())
        .unwrap()
        .no_await();
    vm.register_fn("fn_unmarked", |cx: &mut Ctx| cx.can_suspend())
        .unwrap();
    vm.load_source(
        "game.colibri",
        "fn probe(g: Gauge) {\n    print(\"{g.marked()} {g.unmarked()} {fn_marked()} {fn_unmarked()}\")\n}\nfn main() {\n    let g = Gauge.new()\n    let c = go probe(g)\n    c.join()\n}\n",
    )
    .unwrap();
    vm.run_main().expect("main");
    assert_eq!(out.contents(), "false true false true\n");
}

// ---- the end of an entry (re-review 3, F1) ----

const MEMBER: &str =
    "fn member() {\n    defer print(\"member defer {blocky()}\")\n    wait(10.0)\n}\n";

/// A `CoGroup` that is a local of `main` dies in `main`'s own frame teardown: settled where the
/// frame's drops fold, inside the entry.
#[test]
fn a_group_that_dies_in_mains_teardown_fails_the_entry() {
    let (mut vm, out) = vm();
    vm.load_source(
        "game.colibri",
        &format!("{MEMBER}fn main() {{\n    let g = CoGroup()\n    let _m = g.go(member())\n    print(\"main ends\")\n}}\n"),
    )
    .unwrap();
    let error = vm.run_main().expect_err("the refusal");
    assert!(text(&error).contains(REFUSAL), "{}", text(&error));
    assert_eq!(out.contents(), "main ends\n");
}

/// A `defer` of `main` that cancels the coroutine: settled at the cancel call.
#[test]
fn a_defer_of_main_that_cancels_fails_the_entry() {
    let (mut vm, out) = vm();
    vm.load_source(
        "game.colibri",
        &format!("{MEMBER}fn main() {{\n    let c = go member()\n    defer c.cancel()\n    print(\"main ends\")\n}}\n"),
    )
    .unwrap();
    let error = vm.run_main().expect_err("the refusal");
    assert!(text(&error).contains(REFUSAL), "{}", text(&error));
    assert_eq!(out.contents(), "main ends\n");
}

/// A `Drop` hook of a local of `main` that cancels the coroutine, at `main`'s end.
#[test]
fn a_drop_hook_at_mains_end_that_cancels_fails_the_entry() {
    let (mut vm, out) = vm();
    vm.load_source(
        "game.colibri",
        &format!("{MEMBER}class Stopper {{\n    c: Coroutine[()]\n}}\nimpl Drop for Stopper {{\n    fn drop(self) {{\n        self.c.cancel()\n    }}\n}}\nfn main() {{\n    let _s = Stopper {{ c: go member() }}\n    print(\"main ends\")\n}}\n"),
    )
    .unwrap();
    let error = vm.run_main().expect_err("the refusal");
    assert!(text(&error).contains(REFUSAL), "{}", text(&error));
    assert_eq!(out.contents(), "main ends\n");
}

/// The value an entry hands back is released by the host after the entry (`run_main` releases
/// `main`'s `Err` payload with `release_in_entry`). Its `CoGroup` dies there and the member's
/// `defer` meets the refusal: neither `settle_refusal`, nor a fold, nor the end-of-entry check
/// sees it. The refusal must reach the host one way or another.
#[test]
fn a_group_released_by_the_host_after_the_entry_is_not_lost() {
    let (mut vm, out) = vm();
    vm.load_source(
        "game.colibri",
        &format!("{MEMBER}class Holder {{\n    g: CoGroup\n}}\nimpl IntoError for Holder {{}}\nfn main() -> Result[()] {{\n    let g = CoGroup()\n    let _m = g.go(member())\n    print(\"main returns err\")\n    return err(Error.from(Holder {{ g: g }}))\n}}\n"),
    )
    .unwrap();
    let result = vm.run_main();
    let shown = match &result {
        Err(e) => text(e),
        Ok(()) => "Ok".to_string(),
    };
    let failure = vm.run_event_loop().err().map(|e| text(&e));
    let _ = out;
    // The refusal must reach the host one way or another.
    assert!(
        shown.contains(REFUSAL) || failure.as_deref().is_some_and(|f| f.contains(REFUSAL)),
        "the refusal was lost: run_main gave {shown:?}, no coroutine failure carries it"
    );
}

/// The same, but the host makes another entry before it looks at coroutine failures: that
/// entry's start discards the pending refusal but keeps the coroutine's failure, which the event
/// loop then reports.
#[test]
fn a_group_released_by_the_host_then_another_entry() {
    let (mut vm, out) = vm();
    vm.load_source(
        "game.colibri",
        &format!("{MEMBER}class Holder {{\n    g: CoGroup\n}}\nimpl IntoError for Holder {{}}\nfn main() -> Result[()] {{\n    let g = CoGroup()\n    let _m = g.go(member())\n    print(\"main returns err\")\n    return err(Error.from(Holder {{ g: g }}))\n}}\nfn other() {{\n    print(\"other runs\")\n}}\n"),
    )
    .unwrap();
    let _first = vm.run_main().err().map(|e| text(&e));
    let f = vm.get_fn::<(), ()>("other").unwrap();
    let second = f.call(&mut vm, ()).err().map(|e| text(&e));
    let failure = vm.run_event_loop().err().map(|e| text(&e));
    let _ = out;
    assert!(
        second.is_none(),
        "{second:?}: the refusal is not blamed on the next entry"
    );
    assert!(
        failure.as_deref().is_some_and(|f| f.contains(REFUSAL)),
        "the refusal was lost: run_main {_first:?}, other {second:?}, event loop {failure:?}"
    );
}
