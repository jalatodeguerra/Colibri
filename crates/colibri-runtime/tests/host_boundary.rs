//! What happens at the border between Colibri code and host functions: the host function's
//! frame in a trace, the host depth, the call-depth limit, what a panic or a callback that fails
//! looks like on either side, the type arguments of a call and the hooks of a host class.
//!
//! Every scenario runs with host functions, and the outcome (the script's output and how the run
//! ended, stack included) is pinned.

use std::cell::Cell;
use std::rc::Rc;

use colibri::{Ctx, Error, HostType, TypeBuilder, Value};
use colibri_runtime as colibri;

/// Fetches function handles once the program is loaded, for host functions that call back.
type Handles = Rc<Cell<Option<colibri::Fn<(i64,), i64>>>>;

/// How a run ended: the output, then the panic or the value, each frame as `name:line`.
fn describe(out: &str, result: Result<(), Error>) -> String {
    let end = match result {
        Ok(()) => "ok".to_string(),
        Err(Error::Panic {
            message,
            location,
            stack,
            notes,
        }) => format!(
            "panic `{message}` at {:?}, stack {:?}, notes {notes:?}",
            location.map(|l| l.line),
            stack
                .iter()
                .map(|f| format!("{}:{}", f.function, f.location.line))
                .collect::<Vec<_>>()
        ),
        Err(other) => format!("{other:?}"),
    };
    format!("{out}--\n{end}")
}

/// What a scenario sets up: host functions before the program loads, and what needs the loaded
/// program (function handles) after it.
struct Scenario<'a> {
    before: &'a dyn Fn(&mut colibri::Vm, &Handles),
    after: &'a dyn Fn(&colibri::Vm, &Handles),
    depth: usize,
}

fn run_on(s: &Scenario<'_>, source: &str) -> String {
    let out = colibri::SharedBuffer::new();
    let mut vm = colibri::Vm::builder()
        .max_call_depth(s.depth)
        .stdout(Box::new(out.clone()))
        .build();
    let handles: Handles = Rc::new(Cell::new(None));
    (s.before)(&mut vm, &handles);
    let result = vm.load_source("a.colibri", source).and_then(|()| {
        (s.after)(&vm, &handles);
        vm.run_main()
    });
    let text = out.contents();
    describe(&text, result)
}

/// The run of a scenario. Returns the outcome.
fn both(s: &Scenario<'_>, source: &str) -> String {
    run_on(s, source)
}

fn plain<'a>(before: &'a dyn Fn(&mut colibri::Vm, &Handles)) -> Scenario<'a> {
    Scenario {
        before,
        after: &|_, _| {},
        depth: 1000,
    }
}

#[test]
fn a_rust_panic_in_a_host_function_is_a_panic_whose_trace_names_the_host_function() {
    let outcome = both(
        &plain(&|vm, _| {
            vm.register_fn("boom", || -> i64 { panic!("kaboom") })
                .unwrap();
        }),
        "fn outer() -> int {\n    return boom()\n}\nfn main() {\n    print(\"before\")\n    print(outer())\n}\n",
    );
    assert_eq!(
        outcome,
        "before\n--\npanic `host function `boom` panicked: kaboom` at Some(2), \
         stack [\"boom:2\", \"outer:2\", \"main:6\"], notes []"
    );
}

#[test]
fn raise_is_a_panic_with_exactly_its_message_and_defers_run() {
    let outcome = both(
        &plain(&|vm, _| {
            vm.register_fn("stop", |cx: &mut Ctx| -> () {
                cx.raise("stopped on purpose")
            })
            .unwrap();
        }),
        "fn main() {\n    defer print(\"cleanup\")\n    stop()\n    print(\"not reached\")\n}\n",
    );
    assert_eq!(
        outcome,
        "cleanup\n--\npanic `stopped on purpose` at Some(3), stack [\"stop:3\", \"main:3\"], notes []"
    );
}

/// `depth()` answers the host depth; `through(n)` calls the stored function with `n`, from a
/// host function.
fn depth_and_through(vm: &mut colibri::Vm, handles: &Handles) {
    vm.register_fn("depth", |cx: &mut Ctx| i64::from(cx.host_depth()))
        .unwrap();
    let handles = Rc::clone(handles);
    vm.register_fn("through", move |cx: &mut Ctx, n: i64| -> i64 {
        let f = handles.get().expect("the callback was stored");
        f.call(cx, (n,)).unwrap() + 100
    })
    .unwrap();
}

fn store_inner(vm: &colibri::Vm, handles: &Handles) {
    handles.set(Some(vm.get_fn::<(i64,), i64>("inner").unwrap()));
}

#[test]
fn the_host_depth_counts_host_functions_inside_one_another() {
    let outcome = both(
        &Scenario {
            before: &depth_and_through,
            after: &store_inner,
            depth: 1000,
        },
        "fn inner(n: int) -> int => depth() + n\nfn main() {\n    print(depth())\n    print(through(5))\n}\n",
    );
    // `depth()` from Colibri code is host depth 1; through a callback it is 2.
    assert_eq!(outcome, "1\n107\n--\nok");
}

#[test]
fn a_callback_that_panics_leaves_the_script_running_with_its_registers_intact() {
    // The host function swallows the failure and reports where the panic was; the script goes on
    // and calls again, with several values alive across the failing call.
    let outcome = both(
        &Scenario {
            before: &|vm, handles| {
                let handles = Rc::clone(handles);
                vm.register_fn("attempt", move |cx: &mut Ctx, n: i64| -> String {
                    let f = handles.get().expect("stored");
                    match f.call(cx, (n,)) {
                        Ok(v) => format!("ok {v}"),
                        Err(Error::Panic { message, stack, .. }) => format!(
                            "{message} in {:?}",
                            stack
                                .iter()
                                .map(|f| format!("{}:{}", f.function, f.location.line))
                                .collect::<Vec<_>>()
                        ),
                        Err(other) => panic!("{other:?}"),
                    }
                })
                .unwrap();
            },
            after: &store_inner,
            depth: 1000,
        },
        "fn inner(n: int) -> int {\n    return 100 / n\n}\nfn main() {\n    let a = \"alive\"\n    let b = [1, 2, 3]\n    print(attempt(0))\n    print(attempt(4))\n    print(a)\n    print(b)\n}\n",
    );
    assert_eq!(
        outcome,
        "division by zero in [\"inner:2\", \"attempt:7\", \"main:7\"]\nok 25\nalive\n[1, 2, 3]\n--\nok"
    );
}

#[test]
fn a_callback_failure_a_host_function_raises_again_has_the_host_function_in_its_trace() {
    let outcome = both(
        &Scenario {
            before: &|vm, handles| {
                let handles = Rc::clone(handles);
                vm.register_fn("relay", move |cx: &mut Ctx, n: i64| -> i64 {
                    let f = handles.get().expect("stored");
                    match f.call(cx, (n,)) {
                        Ok(v) => v,
                        Err(Error::Panic { message, .. }) => cx.raise(message),
                        Err(other) => panic!("{other:?}"),
                    }
                })
                .unwrap();
            },
            after: &store_inner,
            depth: 1000,
        },
        "fn inner(n: int) -> int => 10 / n\nfn main() {\n    defer print(\"unwound\")\n    print(relay(2))\n    print(relay(0))\n}\n",
    );
    assert_eq!(
        outcome,
        "5\nunwound\n--\npanic `division by zero` at Some(5), stack [\"relay:5\", \"main:5\"], notes []"
    );
}

#[test]
fn recursion_through_a_host_function_counts_the_host_frames_against_the_depth_limit() {
    let outcome = both(
        &Scenario {
            before: &|vm, handles| {
                let handles = Rc::clone(handles);
                vm.register_fn("bounce", move |cx: &mut Ctx, n: i64| -> i64 {
                    cx.write_stdout(&format!("bounce {n}\n"));
                    let f = handles.get().expect("stored");
                    match f.call(cx, (n + 1,)) {
                        Ok(v) => v,
                        Err(Error::Panic { message, .. }) => cx.raise(message),
                        Err(other) => panic!("{other:?}"),
                    }
                })
                .unwrap();
            },
            after: &store_inner,
            depth: 9,
        },
        "fn inner(n: int) -> int => bounce(n)\nfn main() {\n    print(inner(0))\n}\n",
    );
    // The depth limit is 9: `main`, then `inner` and `bounce` alternate, so a host frame uses one
    // of the nine. Four `bounce` calls fit; the callback of the fourth is the one that overflows.
    assert_eq!(
        outcome,
        "bounce 0\nbounce 1\nbounce 2\nbounce 3\n--\npanic `stack overflow: more than 9 nested calls` \
         at Some(1), stack [\"bounce:1\", \"inner:1\", \"main:3\"], notes []"
    );
}

#[test]
fn a_generic_host_function_sees_the_type_arguments_of_its_call() {
    let outcome = both(
        &plain(&|vm, _| {
            vm.register_fn("pick", |cx: &mut Ctx, v: Value| -> Value {
                let text = format!("{:?}\n", cx.type_args());
                cx.write_stdout(&text);
                v
            })
            .unwrap();
        }),
        "fn main() {\n    print(pick(1))\n    print(pick(\"s\"))\n    print(pick(2.5))\n}\n",
    );
    assert_eq!(
        outcome, "[Int(I64)]\n1\n[Str]\ns\n[Float]\n2.5\n--\nok",
        "{outcome}"
    );
}

#[test]
fn a_host_function_sees_an_optional_object_as_the_some_enum_it_is() {
    let outcome = both(
        &plain(&|vm, _| {
            vm.register_fn("probe", |cx: &mut Ctx, v: Value| -> Value {
                let text = match v.kind() {
                    colibri::ValueKind::Enum {
                        variant, payload, ..
                    } => format!(
                        "enum {variant} {} {:?} {}\n",
                        payload.len(),
                        v.variant_index(),
                        payload
                            .first()
                            .map_or("-".to_string(), |p| format!("{:?}", p.kind())),
                    ),
                    other => format!("other {other:?}\n"),
                };
                cx.write_stdout(&text);
                v
            })
            .unwrap();
        }),
        "class N { v: int }\nfn main() {\n    let n = N { v: 1 }\n    let s: N? = some(n)\n    let back = probe(s)\n    print(back == s)\n    print(back.is_some())\n    let z: N? = none\n    print(probe(z).is_none())\n}\n",
    );
    assert_eq!(
        outcome, "enum 1 1 Some(1) Other\ntrue\ntrue\nenum 0 0 Some(0) -\ntrue\n--\nok",
        "{outcome}"
    );
}

/// A host class that shows, compares and hashes itself, and a hook that fails for one value.
struct Pt(i64, i64);

impl HostType for Pt {
    const NAME: &'static str = "Pt";
    fn describe(d: &mut TypeBuilder<'_, Self>) {
        d.static_fn("new", ["x", "y"], |x: i64, y: i64| Pt(x, y));
        d.show(|this: &Pt| {
            assert!(this.0 >= 0, "a negative point cannot be shown");
            format!("({}, {})", this.0, this.1)
        });
        d.eq(|a: &Pt, b: &Pt| a.0 == b.0 && a.1 == b.1);
    }
}

#[test]
fn host_class_hooks_show_and_compare_and_their_panics_name_the_hook() {
    let outcome = both(
        &plain(&|vm, _| vm.register_type::<Pt>().unwrap()),
        "fn main() {\n    let a = Pt.new(1, 2)\n    let b = Pt.new(1, 2)\n    let c = Pt.new(-3, 4)\n    print(a)\n    print(\"{a}!\")\n    print([a, b])\n    print(a == b)\n    print(a == Pt.new(9, 9))\n    print([a] == [b])\n    print(c)\n}\n",
    );
    assert!(
        outcome.starts_with("(1, 2)\n(1, 2)!\n[(1, 2), (1, 2)]\ntrue\nfalse\ntrue\n--\npanic `"),
        "{outcome}"
    );
    assert!(
        outcome.contains("a negative point cannot be shown"),
        "{outcome}"
    );
}

#[test]
fn the_arguments_of_a_host_call_die_at_the_callers_statement_end_not_inside_the_call() {
    // The host function gets a reference of its own and drops it; the caller's twin lives to the
    // end of its statement, so the object dies after the rest of the statement has run.
    let outcome = both(
        &plain(&|vm, _| {
            vm.register_fn("take", |cx: &mut Ctx, _a: Value| -> i64 {
                cx.write_stdout("in the host function\n");
                1
            })
            .unwrap();
        }),
        "class Res { label: str }\nimpl Drop for Res {\n    fn drop(self) { print(\"drop {self.label}\") }\n}\nfn say(n: int) -> int {\n    print(\"say\")\n    return n\n}\nfn main() {\n    let r = take(Res { label: \"a\" }) + say(1)\n    print(\"after {r}\")\n}\n",
    );
    assert_eq!(
        outcome,
        "in the host function\nsay\ndrop a\nafter 2\n--\nok"
    );
}

#[test]
fn a_host_call_at_the_depth_limit_is_a_stack_overflow() {
    let outcome = both(
        &Scenario {
            before: &|vm, _| {
                vm.register_fn("noop", || {}).unwrap();
            },
            after: &|_, _| {},
            depth: 3,
        },
        "fn b() {\n    noop()\n}\nfn a() {\n    b()\n}\nfn main() {\n    noop()\n    a()\n}\n",
    );
    // Two frames deep a host call fits (`main`, `a`, and the host function's own); from `b` it
    // does not.
    assert_eq!(
        outcome,
        "--\npanic `stack overflow: more than 3 nested calls` at Some(2), stack [\"b:2\", \"a:5\", \"main:9\"], notes []"
    );
}

/// The steps one entry used running `body` 50 times.
fn steps_of(body: &str) -> u64 {
    let mut vm = colibri::Vm::builder().instruction_budget(1_000_000).build();
    vm.register_fn("noop", || {}).unwrap();
    vm.load_source(
        "a.colibri",
        &format!("fn f() {{\n    for i in 0..50 {{\n        {body}\n    }}\n}}\n"),
    )
    .unwrap();
    let f = vm.get_fn::<(), ()>("f").unwrap();
    f.call(&mut vm, ()).unwrap();
    vm.steps_used()
}

#[test]
fn every_host_call_costs_a_budget_step() {
    let with = steps_of("noop()");
    let without = steps_of("let x = i");
    // The calls are 50 statements like the `let`s, and each also costs the step of the call.
    assert!(with >= without + 50, "{with} steps against {without}");
}

/// A host function `keep` that stores the object it is given, and `lie`, declared `-> T`, that
/// hands it back for a `T` the script instantiates with `int`. The boundary refuses that
/// result with a panic naming the host function, whatever the script does with the value next.
fn lying_host(source_body: &str) -> String {
    let before = |vm: &mut colibri::Vm, _: &Handles| {
        let stash: Rc<std::cell::RefCell<Option<Value>>> = Rc::default();
        let put = Rc::clone(&stash);
        vm.register_fn("keep", move |v: Value| {
            *put.borrow_mut() = Some(v);
        })
        .unwrap();
        vm.register_fn("lie", move |x: Value| -> Value {
            drop(x);
            stash.borrow_mut().take().expect("keep first")
        })
        .unwrap();
    };
    both(
        &plain(&before),
        &format!(
            "class Res {{ name: str }}\nimpl Drop for Res {{\n    fn drop(self) {{ print(\"drop {{self.name}}\") }}\n}}\n{source_body}"
        ),
    )
}

#[test]
fn a_host_result_of_the_wrong_kind_is_a_panic_from_an_untyped_let() {
    let outcome = lying_host(
        "fn main() {\n    keep(Res { name: \"a\" })\n    let n = lie(1)\n    print(\"after\")\n}\n",
    );
    assert_clean_lie_panic(&outcome);
}

#[test]
fn a_host_result_of_the_wrong_kind_is_a_panic_from_a_let_typed_int() {
    let outcome = lying_host(
        "fn main() {\n    keep(Res { name: \"b\" })\n    let n: int = lie(1)\n    print(\"after\")\n}\n",
    );
    assert_clean_lie_panic(&outcome);
}

#[test]
fn a_host_result_of_the_wrong_kind_is_a_panic_when_an_int_var_is_overwritten() {
    let outcome = lying_host(
        "fn main() {\n    let mut n = 0\n    keep(Res { name: \"c\" })\n    n = lie(1)\n    print(\"after\")\n    n = 5\n    print(\"reassigned\")\n}\n",
    );
    assert_clean_lie_panic(&outcome);
}

#[test]
fn a_host_result_of_the_wrong_kind_is_a_panic_by_each_trip_of_a_loop() {
    let outcome = lying_host(
        "fn main() {\n    for i in 0..3 {\n        keep(Res { name: \"l{i}\" })\n        let n = lie(i)\n    }\n    print(\"after\")\n}\n",
    );
    assert_clean_lie_panic(&outcome);
}

#[test]
fn a_host_result_of_the_wrong_kind_is_a_panic_when_a_loop_assigns_it_to_a_var() {
    let outcome = lying_host(
        "fn main() {\n    let mut n = 0\n    for i in 0..3 {\n        n = 7\n        keep(Res { name: \"m{i}\" })\n        n = lie(i)\n    }\n    print(\"after\")\n}\n",
    );
    assert_clean_lie_panic(&outcome);
}

#[test]
fn a_host_result_of_the_wrong_kind_is_a_panic_from_a_temporary() {
    let outcome = lying_host(
        "fn main() {\n    keep(Res { name: \"d\" })\n    print(lie(1))\n    print(\"after\")\n}\n",
    );
    assert_clean_lie_panic(&outcome);
}

fn assert_clean_lie_panic(outcome: &str) {
    assert!(
        outcome.contains(
            "panic `host function `lie` returned a `Res` object where `int` was expected`"
        ),
        "{outcome}"
    );
    assert!(!outcome.contains("after"), "{outcome}");
    // The stray object is dropped before the panic is raised, and the trace starts at the host
    // function.
    let (printed, end) = outcome.split_once("--\n").expect("an outcome");
    assert!(printed.starts_with("drop "), "{outcome}");
    assert!(end.contains("stack [\"lie:"), "{outcome}");
    assert!(end.contains("\"main:"), "{outcome}");
}

#[test]
fn a_list_result_with_an_element_of_the_wrong_kind_is_a_panic() {
    let before = |vm: &mut colibri::Vm, _: &Handles| {
        let stash: Rc<std::cell::RefCell<Option<Value>>> = Rc::default();
        let put = Rc::clone(&stash);
        vm.register_fn("keep", move |v: Value| {
            *put.borrow_mut() = Some(v);
        })
        .unwrap();
        vm.register_fn("lies", move |x: Value| -> Vec<Value> {
            let stray = stash.borrow_mut().take().expect("keep first");
            vec![x, stray]
        })
        .unwrap();
    };
    let outcome = both(
        &plain(&before),
        "class Res { name: str }\nimpl Drop for Res {\n    fn drop(self) { print(\"drop {self.name}\") }\n}\nfn main() {\n    keep(Res { name: \"q\" })\n    let xs: List[int] = lies(1)\n    print(\"after\")\n    for x in xs {\n        print(x)\n    }\n}\n",
    );
    assert!(
        outcome.contains(
            "panic `host function `lies` returned a `Res` object where `int` was expected`"
        ),
        "{outcome}"
    );
    assert!(outcome.starts_with("drop q\n"), "{outcome}");
    assert!(outcome.contains("stack [\"lies:"), "{outcome}");
    assert!(!outcome.contains("after"), "{outcome}");
}

#[test]
fn a_host_result_of_the_wrong_kind_is_a_panic_when_copied_to_another_local() {
    let outcome = lying_host(
        "fn main() {\n    keep(Res { name: \"p1\" })\n    let a = lie(1)\n    let b = a\n    print(\"after\")\n}\n",
    );
    assert_clean_lie_panic(&outcome);
}

#[test]
fn a_host_result_of_the_wrong_kind_is_a_panic_when_returned_with_return() {
    let outcome = lying_host(
        "fn f() -> int {\n    return lie(1)\n}\nfn main() {\n    keep(Res { name: \"p2\" })\n    let n = f()\n    print(\"after\")\n}\n",
    );
    assert_clean_lie_panic(&outcome);
}

#[test]
fn a_host_result_of_the_wrong_kind_is_a_panic_when_returned_as_a_body_value() {
    let outcome = lying_host(
        "fn f() -> int => lie(1)\nfn main() {\n    keep(Res { name: \"p2b\" })\n    let n = f()\n    print(\"after\")\n}\n",
    );
    assert_clean_lie_panic(&outcome);
}

#[test]
fn a_host_result_of_the_wrong_kind_is_a_panic_when_passed_to_a_function_that_copies_it() {
    let outcome = lying_host(
        "fn g(x: int) {\n    let y = x\n    print(\"in g\")\n}\nfn main() {\n    keep(Res { name: \"p3b\" })\n    g(lie(1))\n    print(\"after\")\n}\n",
    );
    assert_clean_lie_panic(&outcome);
    assert!(!outcome.contains("in g"), "{outcome}");
}
