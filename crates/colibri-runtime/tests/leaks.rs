//! The leak detector from the host's side: `VmBuilder::track_leaks`, `Vm::find_leaks`
//! (ARCHITECTURE §18).

mod common;
use common::vm_pending;

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
    vm_pending("a_cycle_that_is_still_held_is_not_a_leak", || {
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
    })
}

#[test]
fn a_suspended_coroutine_keeps_its_cycle_alive() {
    vm_pending("a_suspended_coroutine_keeps_its_cycle_alive", || {
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
    })
}

#[test]
fn a_cancelled_coroutine_leaks_what_only_it_held() {
    vm_pending("a_cancelled_coroutine_leaks_what_only_it_held", || {
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
    })
}

#[test]
fn objects_a_leaked_cycle_owns_are_counted_as_retained() {
    vm_pending(
        "objects_a_leaked_cycle_owns_are_counted_as_retained",
        || {
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
        },
    )
}

#[test]
fn a_cycle_through_a_closure_names_the_captured_variable() {
    vm_pending(
        "a_cycle_through_a_closure_names_the_captured_variable",
        || {
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
        },
    )
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
var last = first
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
