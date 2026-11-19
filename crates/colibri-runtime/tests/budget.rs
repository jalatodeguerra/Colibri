//! The instruction budget (EMBEDDING §5).

use std::cell::Cell;
use std::rc::Rc;

use colibri_runtime as colibri;

fn vm_with(budget: u64) -> (colibri::Vm, colibri::SharedBuffer) {
    let out = colibri::SharedBuffer::new();
    let vm = colibri::Vm::builder()
        .stdout(Box::new(out.clone()))
        .instruction_budget(budget)
        .build();
    (vm, out)
}

fn exhausted(err: &colibri::Error) -> (&Option<colibri::Location>, &Vec<colibri::StackFrame>) {
    match err {
        colibri::Error::BudgetExhausted { location, stack } => (location, stack),
        other => panic!("expected the budget to run out, got {other:?}"),
    }
}

#[test]
fn an_infinite_loop_runs_out_and_the_vm_stays_usable() {
    let (mut vm, out) = vm_with(10_000);
    vm.load_source(
        "spin.colibri",
        "var n = 0\nfn spin() {\n    while true {\n        n += 1\n    }\n}\nfn fine() -> int {\n    n += 1\n    return n\n}\n",
    )
    .unwrap();
    let spin = vm.get_fn::<(), ()>("spin").unwrap();
    let err = spin.call(&mut vm, ()).unwrap_err();
    let (location, stack) = exhausted(&err);
    assert_eq!(location.as_ref().unwrap().file, "spin.colibri");
    assert_eq!(stack[0].function, "spin");
    let text = err.render(false);
    assert!(
        text.starts_with("error: the instruction budget is exhausted\n"),
        "{text}"
    );
    assert!(text.contains("at spin (spin.colibri:"), "{text}");
    assert!(err.to_string().contains("instruction budget"));
    // It took about the allotment (plus the grace for cleanups).
    assert!(vm.steps_used() >= 10_000, "{}", vm.steps_used());
    assert!(vm.steps_used() < 10_000 + 10_100, "{}", vm.steps_used());
    // The next entry gets a fresh allotment, and the module state is intact.
    let fine = vm.get_fn::<(), i64>("fine").unwrap();
    let after = fine.call(&mut vm, ()).unwrap();
    assert!(after > 100, "the loop's writes are kept: {after}");
    assert!(vm.steps_used() < 20, "{}", vm.steps_used());
    assert_eq!(out.contents(), "");
}

#[test]
fn defers_and_drop_hooks_run_while_it_unwinds() {
    let (mut vm, out) = vm_with(5_000);
    vm.load_source(
        "spin.colibri",
        r#"
class Res { label: str }

impl Drop for Res {
    fn drop(self) { print("drop {self.label}") }
}

fn spin() {
    defer print("deferred")
    let r = Res { label: "r" }
    while true {
    }
}
"#,
    )
    .unwrap();
    let spin = vm.get_fn::<(), ()>("spin").unwrap();
    let err = spin.call(&mut vm, ()).unwrap_err();
    exhausted(&err);
    // Last in, first out: the binding, then the defer.
    assert_eq!(out.contents(), "drop r\ndeferred\n");
}

#[test]
fn cleanups_get_a_grace_and_then_are_skipped() {
    let (mut vm, out) = vm_with(2_000);
    vm.load_source(
        "spin.colibri",
        r#"
class Res { label: str }

impl Drop for Res {
    fn drop(self) {
        print("hook {self.label}")
        while true {
        }
    }
}

fn spin() {
    defer print("never printed")
    let a = Res { label: "a" }
    let b = Res { label: "b" }
    defer {
        print("deferred loop")
        while true {
        }
    }
    while true {
    }
}
"#,
    )
    .unwrap();
    let spin = vm.get_fn::<(), ()>("spin").unwrap();
    let err = spin.call(&mut vm, ()).unwrap_err();
    exhausted(&err);
    // The first defer ran (it loops until the grace is gone); after that the hooks and the
    // defer below it were skipped, but the call ended: no hang, no hook output.
    let printed = out.contents();
    assert!(printed.starts_with("deferred loop\n"), "{printed}");
    assert!(!printed.contains("never printed"), "{printed}");
    assert!(!printed.contains("hook"), "{printed}");
    // The whole thing is bounded: allotment plus the grace of 10 000.
    assert!(
        vm.steps_used() < 2_000 + 10_000 + 200,
        "{}",
        vm.steps_used()
    );
    // And the Vm is fine: the same call runs out again the same way.
    assert!(matches!(
        spin.call(&mut vm, ()),
        Err(colibri::Error::BudgetExhausted { .. })
    ));
}

#[test]
fn deep_recursion_runs_out_before_the_stack_does() {
    // The default native stack of a test thread allows about eighty frames in a debug build.
    let (mut vm, _) = vm_with(40);
    vm.load_source("r.colibri", "fn down(n: int) -> int => down(n + 1) + 1\n")
        .unwrap();
    let down = vm.get_fn::<(i64,), i64>("down").unwrap();
    let err = down.call(&mut vm, (0,)).unwrap_err();
    let (_, stack) = exhausted(&err);
    assert!(stack.len() > 20 && stack.len() <= 41, "{}", stack.len());
    assert_eq!(stack[0].function, "down");
}

#[test]
fn every_entry_gets_its_own_allotment() {
    let (mut vm, _) = vm_with(1_000);
    vm.load_source(
        "w.colibri",
        "fn work(n: int) -> int {\n    var total = 0\n    for i in 0..n {\n        total += i\n    }\n    return total\n}\n",
    )
    .unwrap();
    let work = vm.get_fn::<(i64,), i64>("work").unwrap();
    for _ in 0..5 {
        assert_eq!(work.call(&mut vm, (100,)).unwrap(), 4950);
    }
    assert!(work.call(&mut vm, (100_000,)).is_err());
    assert_eq!(work.call(&mut vm, (100,)).unwrap(), 4950);
    // A step counts per statement, per iteration and per call, so the total is a small
    // multiple of the iterations.
    let used = vm.steps_used();
    assert!((100..=500).contains(&used), "{used}");
}

#[test]
fn the_budget_can_be_changed_or_lifted_at_any_time() {
    let (mut vm, _) = vm_with(100);
    vm.load_source(
        "w.colibri",
        "fn work() -> int {\n    var t = 0\n    for i in 0..10000 {\n        t += 1\n    }\n    return t\n}\n",
    )
    .unwrap();
    let work = vm.get_fn::<(), i64>("work").unwrap();
    assert!(work.call(&mut vm, ()).is_err());
    vm.set_instruction_budget(None);
    assert_eq!(work.call(&mut vm, ()).unwrap(), 10_000);
    vm.set_instruction_budget(Some(1_000_000));
    assert_eq!(work.call(&mut vm, ()).unwrap(), 10_000);
    assert!(vm.steps_used() > 10_000);
    vm.set_instruction_budget(Some(50));
    assert!(work.call(&mut vm, ()).is_err());
}

#[test]
fn looping_builtins_cost_a_step_per_element() {
    let (mut vm, _) = vm_with(1_000_000);
    vm.load_source(
        "b.colibri",
        r#"
var big: List[int] = []
fn fill() {
    for i in 0..50000 {
        big.push(i)
    }
}
fn search() -> bool => big.contains(-1)
fn sorted() -> int {
    big.sort()
    return big[0]
}
"#,
    )
    .unwrap();
    let fill = vm.get_fn::<(), ()>("fill").unwrap();
    let search = vm.get_fn::<(), bool>("search").unwrap();
    let sorted = vm.get_fn::<(), i64>("sorted").unwrap();
    fill.call(&mut vm, ()).unwrap();
    vm.set_instruction_budget(Some(10_000));
    // One built-in call, but it looks at 50 000 elements.
    assert!(matches!(
        search.call(&mut vm, ()),
        Err(colibri::Error::BudgetExhausted { .. })
    ));
    assert!(matches!(
        sorted.call(&mut vm, ()),
        Err(colibri::Error::BudgetExhausted { .. })
    ));
    vm.set_instruction_budget(Some(1_000_000));
    assert!(!search.call(&mut vm, ()).unwrap());
    assert!(vm.steps_used() >= 50_000);
}

#[test]
fn top_level_code_has_a_budget_too() {
    let (mut vm, out) = vm_with(1_000);
    let err = vm
        .load_source("spin.colibri", "print(\"start\")\nwhile true {\n}\n")
        .unwrap_err();
    exhausted(&err);
    assert_eq!(out.contents(), "start\n");
    assert!(matches!(
        vm.get_fn::<(), ()>("main"),
        Err(colibri::Error::NotLoaded)
    ));
    vm.load_source("fine.colibri", "fn main() {\n    print(\"fine\")\n}\n")
        .unwrap();
    vm.run_main().unwrap();
    assert_eq!(out.contents(), "start\nfine\n");
}

#[test]
fn run_main_is_one_entry() {
    let (mut vm, _) = vm_with(500);
    vm.load_source("m.colibri", "fn main() {\n    while true {\n    }\n}\n")
        .unwrap();
    let err = vm.run_main().unwrap_err();
    exhausted(&err);
}

#[test]
fn reentrant_calls_share_the_entrys_budget() {
    let (mut vm, _) = vm_with(5_000);
    let handle: Rc<Cell<Option<colibri::Fn<(), i64>>>> = Rc::new(Cell::new(None));
    let h = Rc::clone(&handle);
    let swallowed = Rc::new(Cell::new(0));
    let s = Rc::clone(&swallowed);
    vm.register_fn("again", move |cx: &mut colibri::Ctx| -> i64 {
        // Each reentrant call is cheap, but they all draw from one allotment.
        for _ in 0..100_000 {
            match h.get().unwrap().call(cx, ()) {
                Ok(_) => {}
                Err(colibri::Error::BudgetExhausted { .. }) => {
                    s.set(s.get() + 1);
                    return -1;
                }
                Err(other) => panic!("unexpected {other:?}"),
            }
        }
        0
    })
    .unwrap();
    vm.load_source(
        "r.colibri",
        "fn one() -> int => 1\nfn run_it() -> int => again()\n",
    )
    .unwrap();
    handle.set(Some(vm.get_fn::<(), i64>("one").unwrap()));
    let run_it = vm.get_fn::<(), i64>("run_it").unwrap();
    // The host function swallowed the exhaustion and returned normally, but the entry as a
    // whole still ran out: the script cannot hide it.
    let err = run_it.call(&mut vm, ()).unwrap_err();
    exhausted(&err);
    assert_eq!(swallowed.get(), 1);
    // A reentrant call only shares: the next entry starts afresh.
    let one = vm.get_fn::<(), i64>("one").unwrap();
    assert_eq!(one.call(&mut vm, ()).unwrap(), 1);
}

#[test]
fn a_host_function_sees_the_steps_so_far() {
    let (mut vm, _) = vm_with(100_000);
    let seen = Rc::new(Cell::new(0u64));
    let s = Rc::clone(&seen);
    vm.register_fn("probe", move |cx: &mut colibri::Ctx| {
        s.set(cx.steps_used());
    })
    .unwrap();
    vm.load_source(
        "p.colibri",
        "fn f() {\n    for i in 0..10 {\n        let x = i\n    }\n    probe()\n}\n",
    )
    .unwrap();
    let f = vm.get_fn::<(), ()>("f").unwrap();
    f.call(&mut vm, ()).unwrap();
    assert!(seen.get() >= 20, "{}", seen.get());
    assert!(vm.steps_used() >= seen.get());
}

#[test]
fn an_unlimited_vm_never_runs_out() {
    let mut vm = colibri::Vm::builder()
        .stdout(Box::new(colibri::SharedBuffer::new()))
        .build();
    vm.load_source(
        "u.colibri",
        "fn f() -> int {\n    var t = 0\n    for i in 0..200000 {\n        t += 1\n    }\n    return t\n}\n",
    )
    .unwrap();
    let f = vm.get_fn::<(), i64>("f").unwrap();
    assert_eq!(f.call(&mut vm, ()).unwrap(), 200_000);
    assert!(vm.steps_used() > 200_000);
}
