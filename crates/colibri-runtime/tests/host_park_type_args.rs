//! A generic host function that parks keeps the type arguments of its own call across the wait,
//! while other calls of it (on other coroutines, and on the main fiber that drives the loop
//! meanwhile) set theirs. Each call records what `cx.type_args()` says before and after its wait;
//! the result check of the call (which substitutes the type arguments) must not fail either.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use colibri::{Ctx, SharedBuffer, Value, Vm};
use colibri_runtime as colibri;

fn run(program: &str) -> (Result<(), String>, String, Vec<String>) {
    let out = SharedBuffer::new();
    let mut vm = Vm::builder().stdout(Box::new(out.clone())).build();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s = seen.clone();
    vm.register_fn("pick", move |cx: &mut Ctx, v: Value, ms: i64| -> Value {
        let before = format!("{:?}", cx.type_args());
        let parks = cx.can_suspend();
        cx.run_async(move || std::thread::sleep(Duration::from_millis(ms as u64)));
        let after = format!("{:?}", cx.type_args());
        s.lock()
            .unwrap()
            .push(format!("{before} -> {after} ({parks})"));
        v
    })
    .unwrap();
    vm.load_source("a.colibri", program).unwrap();
    let result = vm.run_main().map_err(|e| e.render(false));
    let mut seen = seen.lock().unwrap().clone();
    seen.sort();
    (result, out.contents(), seen)
}

#[test]
fn a_parked_generic_host_call_keeps_its_own_type_arguments() {
    let (result, out, seen) = run(
        "fn a() -> int {\n    return pick(1, 20)\n}\nfn b() -> str {\n    return pick(\"s\", 30)\n}\nfn main() {\n    let x = go a()\n    let y = go b()\n    print(pick(2.5, 200))\n    print(\"{x.join()} {y.join()}\")\n}\n",
    );
    assert_eq!(result, Ok(()), "{out}");
    assert_eq!(out, "2.5\n1 s\n");
    assert_eq!(
        seen,
        [
            "[Float] -> [Float] (true)",
            "[Int(I64)] -> [Int(I64)] (true)",
            "[Str] -> [Str] (true)",
        ]
    );
}

/// The same with every wait in coroutines: one parks twice in a row (resumed, then parked
/// again inside `continue_host`'s resume), while the other's calls interleave with it.
#[test]
fn generic_host_calls_that_park_again_keep_their_type_arguments() {
    let (result, out, seen) = run(
        "fn a() -> int {\n    let p = pick(1, 10)\n    let q = pick([p], 10)\n    return q[0]\n}\nfn b() -> str {\n    return pick(\"s\", 15)\n}\nfn main() {\n    let x = go a()\n    let y = go b()\n    print(\"{x.join()} {y.join()}\")\n}\n",
    );
    assert_eq!(result, Ok(()), "{out}");
    assert_eq!(out, "1 s\n");
    assert_eq!(
        seen,
        [
            "[Int(I64)] -> [Int(I64)] (true)",
            "[List(Int(I64))] -> [List(Int(I64))] (true)",
            "[Str] -> [Str] (true)",
        ]
    );
}

/// The restore in the parked branch of a call: a host function on the main fiber calls back into
/// Colibri, the callback starts a coroutine that parks in a generic host function, and when the
/// outer function reads its type arguments after the callback they are still its own.
#[test]
fn a_callback_that_starts_a_parking_generic_call_leaves_the_outer_type_arguments_alone() {
    let out = SharedBuffer::new();
    let mut vm = Vm::builder().stdout(Box::new(out.clone())).build();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s = seen.clone();
    vm.register_fn("pick", move |cx: &mut Ctx, v: Value, ms: i64| -> Value {
        cx.run_async(move || std::thread::sleep(Duration::from_millis(ms as u64)));
        s.lock().unwrap().push(format!("pick {:?}", cx.type_args()));
        v
    })
    .unwrap();
    let handle: Rc<Cell<Option<colibri::Fn<(), ()>>>> = Rc::new(Cell::new(None));
    let (h, s) = (Rc::clone(&handle), seen.clone());
    vm.register_fn("outer", move |cx: &mut Ctx, v: Value| -> Value {
        let before = format!("{:?}", cx.type_args());
        h.take().unwrap().call(cx, ()).unwrap();
        let after = format!("{:?}", cx.type_args());
        s.lock().unwrap().push(format!("outer {before} -> {after}"));
        v
    })
    .unwrap();
    vm.load_source(
        "a.colibri",
        "fn child() -> int {\n    return pick(1, 20)\n}\nfn callback() {\n    let _c = go child()\n}\nfn main() {\n    print(outer(\"s\"))\n}\n",
    )
    .unwrap();
    handle.set(Some(vm.get_fn::<(), ()>("callback").unwrap()));
    vm.run_main().unwrap();
    assert_eq!(out.contents(), "s\n");
    let seen = seen.lock().unwrap().clone();
    assert_eq!(
        seen,
        ["outer [Str] -> [Str]"],
        "the child is parked, not done"
    );
}
