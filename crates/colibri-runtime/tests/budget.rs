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
        "let mut n = 0\nfn spin() {\n    while true {\n        n += 1\n    }\n}\nfn fine() -> int {\n    n += 1\n    return n\n}\n",
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
        "fn work(n: int) -> int {\n    let mut total = 0\n    for i in 0..n {\n        total += i\n    }\n    return total\n}\n",
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
        "fn work() -> int {\n    let mut t = 0\n    for i in 0..10000 {\n        t += 1\n    }\n    return t\n}\n",
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
let mut big: List[int] = []
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
fn iter_on_a_huge_range_is_cheap_and_what_it_yields_is_charged() {
    let (mut vm, _) = vm_with(10_000);
    vm.load_source(
        "r.colibri",
        r#"
fn first() -> int {
    for x in (0..9223372036854775807).iter() {
        return x
    }
    return -1
}
fn consume() -> int {
    let mut n = 0
    for x in (0..9223372036854775807).iter() {
        n += x
    }
    return n
}
fn stepping() -> int {
    let mut it = (0..9223372036854775807).iter()
    let mut n = 0
    while true {
        match it.next() {
            some(x) => n += x,
            none => break,
        }
    }
    return n
}
fn empty_body() {
    for _ in (0..9223372036854775807).iter() {
    }
}
fn to_list() -> int => (0..9223372036854775807).to_list().len()
"#,
    )
    .unwrap();
    // Creating the iterator is constant work.
    let first = vm.get_fn::<(), i64>("first").unwrap();
    assert_eq!(first.call(&mut vm, ()).unwrap(), 0);
    assert!(vm.steps_used() < 100, "{}", vm.steps_used());
    // The elements are charged as they are consumed.
    let consume = vm.get_fn::<(), i64>("consume").unwrap();
    assert!(matches!(
        consume.call(&mut vm, ()),
        Err(colibri::Error::BudgetExhausted { .. })
    ));
    let empty_body = vm.get_fn::<(), ()>("empty_body").unwrap();
    assert!(matches!(
        empty_body.call(&mut vm, ()),
        Err(colibri::Error::BudgetExhausted { .. })
    ));
    let stepping = vm.get_fn::<(), i64>("stepping").unwrap();
    assert!(matches!(
        stepping.call(&mut vm, ()),
        Err(colibri::Error::BudgetExhausted { .. })
    ));
    // `to_list` is still charged its length up front (or refused as too long).
    let to_list = vm.get_fn::<(), i64>("to_list").unwrap();
    assert!(to_list.call(&mut vm, ()).is_err());
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
        "fn f() -> int {\n    let mut t = 0\n    for i in 0..200000 {\n        t += 1\n    }\n    return t\n}\n",
    )
    .unwrap();
    let f = vm.get_fn::<(), i64>("f").unwrap();
    assert_eq!(f.call(&mut vm, ()).unwrap(), 200_000);
    assert!(vm.steps_used() > 200_000);
}

const SET_ALGEBRA: &str = "struct K { v: int }
impl Eq for K { fn eq(self, o: K) -> bool => self.v == o.v }
fn ints() -> int {
    let a = Set((0..5000).to_list())
    let b = Set((2500..7500).to_list())
    return a.union(b).len()
}
fn ks() -> int {
    let a = Set((0..400).to_list().map(fn(i) => K { v: i }))
    let b = Set((200..600).to_list().map(fn(i) => K { v: i }))
    return a.union(b).len()
}
";

#[test]
fn set_algebra_on_ints_ignores_an_unrelated_user_eq() {
    // A scan per element would take about 25 million steps.
    let (mut vm, _) = vm_with(500_000);
    vm.load_source("sets.colibri", SET_ALGEBRA).unwrap();
    let ints = vm.get_fn::<(), i64>("ints").unwrap();
    assert_eq!(ints.call(&mut vm, ()).unwrap(), 7500);
}

#[test]
fn set_algebra_by_a_user_eq_charges_the_comparisons() {
    let (mut vm, _) = vm_with(50_000);
    vm.load_source("sets.colibri", SET_ALGEBRA).unwrap();
    let ks = vm.get_fn::<(), i64>("ks").unwrap();
    // 400 x 400 comparisons do not fit in 50 000 steps.
    let err = ks.call(&mut vm, ()).unwrap_err();
    exhausted(&err);
    let (mut vm, _) = vm_with(5_000_000);
    vm.load_source("sets.colibri", SET_ALGEBRA).unwrap();
    let ks = vm.get_fn::<(), i64>("ks").unwrap();
    assert_eq!(ks.call(&mut vm, ()).unwrap(), 600);
}

struct Pt {
    x: i64,
}

impl colibri::HostType for Pt {
    const NAME: &'static str = "Pt";
    fn describe(d: &mut colibri::TypeBuilder<'_, Self>) {
        d.static_fn("new", ["x"], |x: i64| Pt { x });
        d.show(|p: &Pt| format!("Pt{}", p.x));
        d.eq(|a: &Pt, b: &Pt| a.x == b.x);
    }
}

const PT_SET: &str = "fn main() {\n    let xs: List[Pt] = []\n    let mut i = 0\n    while i < 4000 {\n        xs.push(Pt.new(i + 100))\n        i += 1\n    }\n    let s = Set(xs)\n    print(s.len())\n}\n";

#[test]
fn host_eq_hooks_and_key_scans_are_charged() {
    // 4 000 objects with an `eq` hook make about 8 million comparisons; a budget of 200 000
    // steps must stop that (a hook call and a scanned key are a step each, EMBEDDING §5).
    let (mut vm, _) = vm_with(200_000);
    vm.register_type::<Pt>().unwrap();
    vm.load_source("pts.colibri", PT_SET).unwrap();
    let err = vm.run_main().unwrap_err();
    exhausted(&err);
    assert!(vm.steps_used() < 200_000 + 10_100, "{}", vm.steps_used());
    // Within a big enough budget the same program completes, and costs at least the
    // comparisons it made.
    let (mut vm, out) = vm_with(100_000_000);
    vm.register_type::<Pt>().unwrap();
    vm.load_source("pts.colibri", &PT_SET.replace("4000", "300"))
        .unwrap();
    vm.run_main().unwrap();
    assert_eq!(out.contents(), "300\n");
    assert!(vm.steps_used() > 300 * 299 / 2, "{}", vm.steps_used());
}

/// The steps of one run of `main` on an engine.
fn steps_of(kind: colibri::EngineKind, source: &str) -> u64 {
    let mut vm = colibri::Vm::builder()
        .engine(kind)
        .stdout(Box::new(colibri::SharedBuffer::new()))
        .instruction_budget(10_000_000)
        .build();
    vm.load_source("a.colibri", source).unwrap();
    vm.run_main().unwrap();
    vm.steps_used()
}

#[test]
fn the_closure_built_ins_cost_the_same_steps_on_both_engines() {
    // Written in Colibri on the VM and native in the tree-walker, they charge alike: a step for
    // each element of the receiver (whether or not the walk stops early), plus the calls of the
    // closure.
    for call in [
        "xs.map(fn(x) => x + 1).len()",
        "xs.filter(fn(x) => x > 4).len()",
        "xs.any(fn(x) => x == 5)",
        "xs.all(fn(x) => x < 3)",
        "xs.find(fn(x) => x == 5)",
        "xs.find_index(fn(x) => x == 5)",
        "xs.fold(0, fn(a, x) => a + x)",
        "xs.flat_map(fn(x) => [x, x]).len()",
        "xs.sorted_by(fn(a, b) => a - b).len()",
        "some(1).map(fn(x) => x + 1)",
        "some(1).and_then(fn(x) => some(x + 1))",
        "[1, 2].map(fn(x) => xs.filter(fn(y) => y > x).len()).len()",
    ] {
        let source = format!(
            "fn main() {{\n    let xs = [5, 3, 9, 1, 7, 2, 8]\n    let r = {call}\n    print(r)\n}}"
        );
        let tree = steps_of(colibri::EngineKind::Tree, &source);
        let vm = steps_of(colibri::EngineKind::Bytecode, &source);
        assert_eq!(vm, tree, "{call}");
    }
    for stmt in [
        "ys.retain(fn(x) => x > 4)",
        "ys.sort_by(fn(a, b) => a - b)",
        "ys.sort_by_key(fn(x) => 0 - x)",
        "ys.for_each(fn(x) => print(x))",
    ] {
        let source =
            format!("fn main() {{\n    let mut ys = [5, 3, 9, 1, 7, 2, 8]\n    {stmt}\n}}");
        let tree = steps_of(colibri::EngineKind::Tree, &source);
        let vm = steps_of(colibri::EngineKind::Bytecode, &source);
        assert_eq!(vm, tree, "{stmt}");
    }
}

#[test]
fn a_budget_that_runs_out_inside_a_closure_built_in_stops_both_engines_at_the_same_place() {
    for kind in [colibri::EngineKind::Tree, colibri::EngineKind::Bytecode] {
        let mut vm = colibri::Vm::builder()
            .engine(kind)
            .stdout(Box::new(colibri::SharedBuffer::new()))
            .instruction_budget(40)
            .build();
        vm.load_source(
            "a.colibri",
            "fn main() {\n    let xs = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]\n    let ys = xs.map(fn(x) => x + 1).map(fn(x) => x + 1).map(fn(x) => x + 1)\n    print(ys.len())\n}",
        )
        .unwrap();
        let err = vm.run_main().unwrap_err();
        let (location, stack) = exhausted(&err);
        assert_eq!(location.as_ref().unwrap().line, 3, "{kind:?}");
        assert_eq!(stack.last().unwrap().function, "main", "{kind:?}");
    }
}

#[test]
fn a_for_mut_over_structs_that_lends_its_slots_charges_what_copying_them_did() {
    let source = "struct P { x: int, y: float }
fn main() {
    let mut ps = [P { x: 0, y: 0.5 }, P { x: 1, y: 1.5 }, P { x: 2, y: 2.5 }]
    for round in 0..4 {
        for mut p in ps {
            p.x += round
            p.y *= 2.0
        }
    }
    print(\"{ps[0].x} {ps[1].x} {ps[2].x} {ps[2].y}\")
}";
    // A VM-only regression guard, not a contract between engines: step counts are approximate and
    // engine-specific (EMBEDDING §5), so the exact number is pinned for this engine alone.
    // The count was 47 before the loop lent the slot (the tree-walker charges one more; that
    // difference is older than the lending).
    let vm = steps_of(colibri::EngineKind::Bytecode, source);
    assert_eq!(vm, 47);
}

#[test]
fn a_budget_that_runs_out_in_a_for_mut_trip_writes_the_elements_back_before_the_defers() {
    // The loop lends the element's slot to the loop variable on the VM. The budget dies in the
    // middle of a trip; the trip's landing pad puts the variable back, so a deferred statement
    // of the function (outside the loop, so it may name the list) sees every element.
    let run = |kind: colibri::EngineKind| {
        let out = colibri::SharedBuffer::new();
        let mut vm = colibri::Vm::builder()
            .engine(kind)
            .stdout(Box::new(out.clone()))
            .instruction_budget(60)
            .build();
        vm.load_source(
            "a.colibri",
            "struct P { x: int }
fn main() {
    let mut ps = [P { x: 0 }, P { x: 0 }, P { x: 0 }, P { x: 0 }]
    defer print(\"{ps[0].x} {ps[1].x} {ps[2].x} {ps[3].x}\")
    for round in 0..1000 {
        for mut p in ps {
            p.x += 1
        }
    }
}",
        )
        .unwrap();
        let err = vm.run_main().unwrap_err();
        exhausted(&err);
        (out.contents(), vm.steps_used())
    };
    let tree = run(colibri::EngineKind::Tree);
    let vm = run(colibri::EngineKind::Bytecode);
    assert_eq!(vm, tree);
    // Some trips ran, and the elements are all there (not `()`, not the old values only).
    let sum: i64 =
        vm.0.trim()
            .split(' ')
            .map(|n| n.parse::<i64>().unwrap())
            .sum();
    assert!(sum > 4, "{}", vm.0);
}
