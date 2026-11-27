//! The leak detector from the host's side: `VmBuilder::track_leaks`, `Vm::find_leaks`
//! (ARCHITECTURE §18).

use colibri_runtime as colibri;

const NODE: &str = "class Node {\n    name: str\n    next: Node? = none\n}\n";

fn vm(tracking: bool, body: &str) -> (colibri::Vm, colibri::SharedBuffer) {
    let out = colibri::SharedBuffer::new();
    let mut vm = colibri::Vm::builder()
        .track_leaks(tracking)
        .stdout(Box::new(out.clone()))
        .build();
    vm.load_source("leaks.colibri", &format!("{NODE}{body}"))
        .unwrap();
    (vm, out)
}

fn call(vm: &mut colibri::Vm, name: &str) {
    let f = vm.get_fn::<(), ()>(name).unwrap();
    f.call(vm, ()).unwrap();
}

#[test]
fn a_cycle_nothing_reaches_is_found_mid_run() {
    let (mut vm, _) = vm(
        true,
        r#"
fn leak() {
    let a = Node { name: "a" }
    let b = Node { name: "b" }
    a.next = some(b)
    b.next = some(a)
}
fn main() {}
"#,
    );
    vm.run_main().unwrap();
    assert!(vm.find_leaks().is_empty());
    call(&mut vm, "leak");
    let leaks = vm.find_leaks();
    assert_eq!(leaks.len(), 1);
    assert_eq!(leaks[0].objects, ["Node", "Node"]);
    assert_eq!(
        leaks[0].path,
        r#"Node(name: "a") -> next -> Node(name: "b") -> next -> Node(name: "a")"#
    );
    assert_eq!(leaks[0].retained, 0);
    // Where each object was made.
    assert_eq!(leaks[0].sites.len(), 2);
    assert!(
        leaks[0].sites[0].ends_with("leaks.colibri:7"),
        "{:?}",
        leaks[0].sites
    );
    assert!(
        leaks[0].sites[1].ends_with("leaks.colibri:8"),
        "{:?}",
        leaks[0].sites
    );
    // Asking again changes nothing, and nothing was freed.
    assert_eq!(vm.find_leaks(), leaks);
}

#[test]
fn a_cycle_that_is_still_held_is_not_a_leak() {
    let (mut vm, _) = vm(
        true,
        r#"
let keep: List[Node] = []
fn hold() {
    let a = Node { name: "a" }
    a.next = some(a)
    keep.push(a)
}
fn main() {}
"#,
    );
    vm.run_main().unwrap();
    call(&mut vm, "hold");
    // A global reaches it: live.
    assert!(vm.find_leaks().is_empty());
    vm.shutdown();
    // The globals are gone: now it is a leak.
    let leaks = vm.find_leaks();
    assert_eq!(leaks.len(), 1);
    assert_eq!(leaks[0].objects, ["Node"]);
}

#[test]
fn a_suspended_coroutine_keeps_its_cycle_alive() {
    let (mut vm, _) = vm(
        true,
        r#"
fn worker() {
    let a = Node { name: "a" }
    let b = Node { name: "b" }
    a.next = some(b)
    b.next = some(a)
    wait(1.0)
}
fn main() { go worker() }
"#,
    );
    vm.run_main().unwrap();
    assert_eq!(vm.coroutine_count(), 1);
    // Suspended in `wait`, the coroutine's frame owns the cycle.
    assert!(vm.find_leaks().is_empty());
    vm.tick(2.0).unwrap();
    // The coroutine finished and its frame is gone: the cycle is garbage.
    assert_eq!(vm.coroutine_count(), 0);
    assert_eq!(vm.find_leaks().len(), 1);
}

#[test]
fn a_cancelled_coroutine_leaks_what_only_it_held() {
    let (mut vm, _) = vm(
        true,
        r#"
fn worker() {
    let a = Node { name: "a" }
    a.next = some(a)
    wait(100.0)
}
fn main() { go worker() }
"#,
    );
    vm.run_main().unwrap();
    assert!(vm.find_leaks().is_empty());
    vm.cancel_coroutines();
    assert_eq!(vm.find_leaks().len(), 1);
}

#[test]
fn objects_a_leaked_cycle_owns_are_counted_as_retained() {
    let (mut vm, _) = vm(
        true,
        r#"
class Holder { items: List[Node] = [], me: Holder? = none }
fn leak() {
    let h = Holder {}
    h.me = some(h)
    h.items.push(Node { name: "x" })
    h.items.push(Node { name: "y" })
}
fn main() { leak() }
"#,
    );
    vm.run_main().unwrap();
    let leaks = vm.find_leaks();
    assert_eq!(leaks.len(), 1);
    assert_eq!(leaks[0].objects, ["Holder"]);
    assert_eq!(leaks[0].retained, 2);
}

#[test]
fn a_cycle_through_a_closure_names_the_captured_variable() {
    let (mut vm, _) = vm(
        true,
        r#"
class Button { on_click: fn()? = none }
fn leak() {
    let button = Button {}
    button.on_click = some(fn() { print(button.on_click.is_some()) })
}
fn main() { leak() }
"#,
    );
    vm.run_main().unwrap();
    let leaks = vm.find_leaks();
    assert_eq!(leaks.len(), 1);
    assert_eq!(leaks[0].objects, ["Button", "closure"]);
    assert!(
        leaks[0].path.contains("captures button"),
        "{}",
        leaks[0].path
    );
}

#[test]
fn weak_references_are_not_edges() {
    let (mut vm, _) = vm(
        true,
        r#"
class Child { parent: weak Parent }
class Parent { child: Child? = none }
fn make() {
    let p = Parent {}
    p.child = some(Child { parent: p })
}
fn main() { make() }
"#,
    );
    vm.run_main().unwrap();
    assert!(vm.find_leaks().is_empty());
}

#[test]
fn without_tracking_there_is_nothing_to_report() {
    let (mut vm, _) = vm(
        false,
        r#"
fn main() {
    let a = Node { name: "a" }
    a.next = some(a)
}
"#,
    );
    vm.run_main().unwrap();
    assert!(vm.find_leaks().is_empty());
}

#[test]
fn a_big_graph_is_found_quickly() {
    let (mut vm, _) = vm(
        true,
        r#"
fn main() {
    let first = Node { name: "0" }
    let mut last = first
    for i in 1..20000 {
        let n = Node { name: "n" }
        last.next = some(n)
        last = n
    }
    last.next = some(first)
}
"#,
    );
    vm.run_main().unwrap();
    let start = std::time::Instant::now();
    let leaks = vm.find_leaks();
    let took = start.elapsed();
    assert_eq!(leaks.len(), 1);
    assert_eq!(leaks[0].objects.len(), 20000);
    assert!(took.as_secs_f64() < 5.0, "took {took:?}");
}

#[test]
fn many_short_lived_objects_do_not_pile_up_in_the_registry() {
    // Objects that die normally leave no leak and do not grow the registry without bound
    // (the registry drops dead entries as it grows).
    let (mut vm, _) = vm(
        true,
        r#"
fn main() {
    for i in 0..50000 {
        let n = Node { name: "n" }
    }
}
"#,
    );
    vm.run_main().unwrap();
    assert!(vm.find_leaks().is_empty());
}

/// The leaks one program makes: the path and the `allocated at` sites of each.
fn leak_reports(source: &str) -> Vec<(String, Vec<String>)> {
    let mut vm = colibri::Vm::builder().track_leaks(true).build();
    vm.load_source("leaks.colibri", source).unwrap();
    vm.run_main().unwrap();
    vm.find_leaks()
        .into_iter()
        .map(|l| (l.path, l.sites))
        .collect()
}

#[test]
fn a_path_through_an_optional_field_is_reported_with_a_shorter_route_beside_it() {
    let reports = leak_reports(
        r#"
class M {
    name: str
    xs: List[M]
    next: M?
}
fn main() {
    let a = M { name: "a", xs: [], next: none }
    let b = M { name: "b", xs: [], next: none }
    a.xs.push(b)
    a.next = some(b)
    b.next = some(a)
}
"#,
    );
    assert_eq!(reports.len(), 1);
    assert_eq!(
        reports[0].0,
        r#"M(name: "a") -> next -> M(name: "b") -> next -> M(name: "a")"#
    );
    assert_eq!(
        reports[0].1,
        ["M at leaks.colibri:8", "M at leaks.colibri:9"]
    );
}

#[test]
fn a_path_through_a_list_holding_both_forms_of_some_is_reported() {
    let reports = leak_reports(
        r#"
class L {
    name: str
    xs: List[L?]
    m: Map[int, L?]
}
fn main() {
    let c = L { name: "c", xs: [], m: {:} }
    let d = L { name: "d", xs: [], m: {:} }
    c.xs.push([d].first())
    c.xs.push(some(d))
    d.m[7] = [c].first()
}
"#,
    );
    assert_eq!(reports.len(), 1);
    assert_eq!(
        reports[0].0,
        r#"L(name: "c") -> xs[0] -> L(name: "d") -> m[7] -> L(name: "c")"#
    );
    assert_eq!(
        reports[0].1,
        ["L at leaks.colibri:8", "L at leaks.colibri:9"]
    );
}

// ---- thread tasks: the detector runs in every isolate (phase 3 L21, ARCHITECTURE §36.7) ----

const TASK_CYCLE: &str = r#"
fn cycle() -> int {
    let a = Node { name: "a" }
    let b = Node { name: "b" }
    a.next = some(b)
    b.next = some(a)
    return 1
}

fn main() {
    let t = task.run(fn() => cycle())
    print("task {t.join().expect("task")}")
}
"#;

fn task_vm(workers: usize, tracking: bool, body: &str) -> colibri::Vm {
    let mut vm = colibri::Vm::builder()
        .track_leaks(tracking)
        .worker_threads(workers)
        .stdout(Box::new(colibri::SharedBuffer::new()))
        .build();
    vm.load_source("leaks.colibri", &format!("{NODE}{body}"))
        .unwrap();
    vm
}

#[test]
fn a_cycle_a_task_leaks_is_reported_to_the_vm_at_shutdown() {
    for workers in [1, 3] {
        let mut vm = task_vm(workers, true, TASK_CYCLE);
        vm.run_main().unwrap();
        vm.run_event_loop().unwrap();
        vm.shutdown();
        let leaks = vm.find_leaks();
        assert_eq!(leaks.len(), 1, "{workers} workers: {leaks:?}");
        assert_eq!(leaks[0].objects, ["Node", "Node"], "{workers} workers");
        // Where each was made, in the program the task ran.
        let sites = leaks[0].sites.join(" ");
        assert!(
            sites.contains("leaks.colibri:7") && sites.contains("leaks.colibri:8"),
            "{sites}"
        );
    }
}

#[test]
fn a_task_that_leaks_nothing_and_a_vm_that_does_not_track_report_nothing() {
    let clean = r#"
fn work() -> int {
    let a = Node { name: "a" }
    let b = Node { name: "b" }
    a.next = some(b)
    return 2
}

fn main() {
    print("task {task.run(fn() => work()).join().expect("task")}")
}
"#;
    let mut vm = task_vm(3, true, clean);
    vm.run_main().unwrap();
    vm.shutdown();
    assert!(vm.find_leaks().is_empty());
    // Without tracking, a leaking task is not found either.
    let mut vm = task_vm(3, false, TASK_CYCLE);
    vm.run_main().unwrap();
    vm.shutdown();
    assert!(vm.find_leaks().is_empty());
}

#[test]
fn the_main_isolates_leaks_and_the_workers_are_reported_together() {
    let both = r#"
fn leak(name: str) {
    let a = Node { name: name }
    a.next = some(a)
}

fn main() {
    leak("main")
    task.run(fn() { leak("task") }).join().expect("task")
}
"#;
    let mut vm = task_vm(1, true, both);
    vm.run_main().unwrap();
    vm.shutdown();
    let mut paths: Vec<String> = vm.find_leaks().into_iter().map(|l| l.path).collect();
    paths.sort();
    assert_eq!(paths.len(), 2, "{paths:?}");
    assert!(
        paths[0].contains("main") && paths[1].contains("task"),
        "{paths:?}"
    );
}
