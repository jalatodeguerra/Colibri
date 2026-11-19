//! Host functions: Rust closures scripts call (EMBEDDING §1).

use std::cell::Cell;
use std::collections::BTreeMap;
use std::rc::Rc;

use colibri_runtime as colibri;

type Handle = Rc<Cell<Option<colibri::Fn<(i64,), i64>>>>;

fn vm() -> (colibri::Vm, colibri::SharedBuffer) {
    let out = colibri::SharedBuffer::new();
    let vm = colibri::Vm::builder().stdout(Box::new(out.clone())).build();
    (vm, out)
}

fn run(vm: &mut colibri::Vm, src: &str) -> Result<(), colibri::Error> {
    vm.load_source("game.colibri", src)?;
    vm.run_main()
}

#[test]
fn a_script_calls_closures_of_every_shape() {
    let (mut vm, out) = vm();
    vm.register_fn("zero", || 7i64).unwrap();
    vm.register_fn("add", |a: i64, b: i64| a + b).unwrap();
    vm.register_fn("shout", |s: String| s.to_uppercase())
        .unwrap();
    vm.register_fn("scale", |x: f32, by: f64| f64::from(x) * by)
        .unwrap();
    vm.register_fn(
        "many",
        |a: i64, b: i64, c: i64, d: i64, e: i64, f: i64, g: i64, h: i64| {
            a + b + c + d + e + f + g + h
        },
    )
    .unwrap();
    vm.register_fn("units", |v: Vec<i64>| v.len()).unwrap();
    vm.register_fn("nothing", || {}).unwrap();
    vm.register_fn("pair", |p: (i64, String)| (p.1, p.0))
        .unwrap();
    vm.register_fn("maybe", |n: i64| if n > 0 { Some(n) } else { None })
        .unwrap();
    run(
        &mut vm,
        r#"
fn main() {
    print(zero())
    print(add(2, 3))
    print(shout("hi"))
    print(scale(1.5, 2.0))
    print(many(1, 2, 3, 4, 5, 6, 7, 8))
    print(units([1, 2, 3]))
    nothing()
    print(pair((1, "a")))
    print(maybe(3))
    print(maybe(-3))
}
"#,
    )
    .unwrap();
    assert_eq!(
        out.contents(),
        "7\n5\nHI\n3.0\n36\n3\n(\"a\", 1)\nsome(3)\nnone\n"
    );
}

#[test]
fn a_leading_context_gives_access_to_host_state() {
    struct World {
        spawned: Vec<(f32, f32)>,
        gravity: f32,
    }
    let (mut vm, out) = vm();
    vm.insert_state(World {
        spawned: Vec::new(),
        gravity: 9.5,
    });
    vm.register_fn("spawn", |cx: &mut colibri::Ctx, x: f32, y: f32| -> i64 {
        let world = cx.state_mut::<World>().expect("the world is there");
        world.spawned.push((x, y));
        world.spawned.len() as i64
    })
    .unwrap();
    vm.register_fn("gravity", |cx: &mut colibri::Ctx| {
        cx.state_mut::<World>().map_or(0.0, |w| w.gravity)
    })
    .unwrap();
    run(
        &mut vm,
        "fn main() {\n    let a = spawn(1.0, 2.0)\n    let b = spawn(3.0, 4.0)\n    print(\"{a} {b} {gravity()}\")\n}\n",
    )
    .unwrap();
    assert_eq!(out.contents(), "1 2 9.5\n");
    // The host reaches the same state between calls.
    let world = vm.state_mut::<World>().unwrap();
    assert_eq!(world.spawned, [(1.0, 2.0), (3.0, 4.0)]);
    world.gravity = 1.0;
    assert_eq!(vm.remove_state::<World>().unwrap().gravity, 1.0);
    assert!(vm.state_mut::<World>().is_none());
}

#[test]
fn closures_may_capture_what_they_like() {
    let (mut vm, out) = vm();
    let counter = Rc::new(Cell::new(0));
    let seen = Rc::clone(&counter);
    vm.register_fn("tick", move || {
        seen.set(seen.get() + 1);
        seen.get()
    })
    .unwrap();
    run(&mut vm, "fn main() {\n    tick()\n    print(tick())\n}\n").unwrap();
    assert_eq!(out.contents(), "2\n");
    assert_eq!(counter.get(), 2);
}

#[test]
fn collections_cross_both_ways() {
    let (mut vm, out) = vm();
    vm.register_fn("evens", |xs: Vec<i64>| -> Vec<i64> {
        xs.into_iter().filter(|x| x % 2 == 0).collect()
    })
    .unwrap();
    vm.register_fn("tally", |words: Vec<String>| -> BTreeMap<String, i64> {
        let mut m = BTreeMap::new();
        for w in words {
            *m.entry(w).or_insert(0) += 1;
        }
        m
    })
    .unwrap();
    run(
        &mut vm,
        "fn main() {\n    print(evens([1, 2, 3, 4]))\n    print(tally([\"a\", \"b\", \"a\"]))\n}\n",
    )
    .unwrap();
    assert_eq!(out.contents(), "[2, 4]\n{\"a\": 2, \"b\": 1}\n");
}

#[test]
fn a_rust_err_is_a_colibri_err_not_a_panic() {
    let (mut vm, out) = vm();
    vm.register_fn(
        "parse",
        |s: String| -> Result<i64, std::num::ParseIntError> { s.parse::<i64>() },
    )
    .unwrap();
    vm.register_fn("custom", |n: i64| -> Result<(), String> {
        if n < 0 {
            Err(format!("{n} is negative"))
        } else {
            Ok(())
        }
    })
    .unwrap();
    run(
        &mut vm,
        r#"
fn main() -> Result[()] {
    print(parse("42"))
    match parse("x") {
        ok(v) => print("ok {v}")
        err(e) => print("err: {e}")
    }
    let n = parse("7")?
    print(n + 1)
    custom(-5)?
    print("unreachable")
}
"#,
    )
    .unwrap_err();
    assert_eq!(
        out.contents(),
        "ok(42)\nerr: invalid digit found in string\n8\n"
    );
}

#[test]
fn main_returning_a_host_err_reports_its_message() {
    let (mut vm, _) = vm();
    vm.register_fn("fail", || -> Result<(), String> {
        Err("disk on fire".into())
    })
    .unwrap();
    let err = run(&mut vm, "fn main() -> Result[()] {\n    fail()?\n}\n").unwrap_err();
    match err {
        colibri::Error::MainReturnedErr { message } => assert_eq!(message, "disk on fire"),
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn a_rust_panic_is_a_colibri_panic_and_the_vm_stays_usable() {
    let (mut vm, out) = vm();
    vm.register_fn("explode", |n: i64| -> i64 {
        if n > 0 {
            panic!("kaboom {n}");
        }
        n
    })
    .unwrap();
    vm.load_source(
        "game.colibri",
        "fn launch(n: int) -> int => explode(n)\nvar calls = 0\nfn bump() -> int {\n    calls += 1\n    return calls\n}\nfn main() {\n    print(\"before\")\n}\n",
    )
    .unwrap();
    let launch = vm.get_fn::<(i64,), i64>("launch").unwrap();
    let bump = vm.get_fn::<(), i64>("bump").unwrap();
    assert_eq!(bump.call(&mut vm, ()).unwrap(), 1);
    assert_eq!(launch.call(&mut vm, (-4,)).unwrap(), -4);
    let err = launch.call(&mut vm, (3,)).unwrap_err();
    let colibri::Error::Panic { message, stack, .. } = &err else {
        panic!("expected a panic, got {err:?}");
    };
    assert_eq!(message, "host function `explode` panicked: kaboom 3");
    // The host function is in the trace, then its caller.
    let names: Vec<&str> = stack.iter().map(|f| f.function.as_str()).collect();
    assert_eq!(names, ["explode", "launch"]);
    assert!(
        err.render(false).contains("at explode (game.colibri:1:"),
        "{}",
        err.render(false)
    );
    // Nothing is damaged: module state, calls, output.
    assert_eq!(bump.call(&mut vm, ()).unwrap(), 2);
    vm.run_main().unwrap();
    assert_eq!(out.contents(), "before\n");
}

#[test]
fn a_host_function_that_cannot_read_its_argument_panics_cleanly() {
    let (mut vm, _) = vm();
    vm.register_fn("index", |i: usize| i).unwrap();
    vm.load_source("t.colibri", "fn f(n: int) -> int => index(n)\n")
        .unwrap();
    let f = vm.get_fn::<(i64,), i64>("f").unwrap();
    assert_eq!(f.call(&mut vm, (3,)).unwrap(), 3);
    let err = f.call(&mut vm, (-1,)).unwrap_err();
    let colibri::Error::Panic { message, .. } = err else {
        panic!("expected a panic");
    };
    assert!(
        message.contains("host function `index`") && message.contains("does not fit a `usize`"),
        "{message}"
    );
}

#[test]
fn a_host_function_calls_back_into_the_script() {
    let (mut vm, out) = vm();
    // `apply` calls the script's `double` through a handle it receives... by name here: the
    // host looks the function up through the context's caller, which is the running Vm.
    let handle: Handle = Rc::new(Cell::new(None));
    let h = Rc::clone(&handle);
    vm.register_fn("apply", move |cx: &mut colibri::Ctx, n: i64| -> i64 {
        let f = h.get().expect("handle set");
        assert_eq!(cx.host_depth(), 1);
        f.call(cx, (n,)).expect("the callback works") + 1
    })
    .unwrap();
    vm.load_source(
        "game.colibri",
        "fn double(n: int) -> int => n * 2\nfn launch(n: int) -> int => apply(n)\n",
    )
    .unwrap();
    handle.set(Some(vm.get_fn::<(i64,), i64>("double").unwrap()));
    let launch = vm.get_fn::<(i64,), i64>("launch").unwrap();
    assert_eq!(launch.call(&mut vm, (20,)).unwrap(), 41);
    assert_eq!(out.contents(), "");
}

#[test]
fn a_panic_in_a_reentrant_call_reaches_the_host_function_not_the_vm() {
    let (mut vm, _) = vm();
    let handle: Handle = Rc::new(Cell::new(None));
    let h = Rc::clone(&handle);
    vm.register_fn("try_call", move |cx: &mut colibri::Ctx, n: i64| -> i64 {
        match h.get().unwrap().call(cx, (n,)) {
            Ok(v) => v,
            Err(colibri::Error::Panic { message, stack, .. }) => {
                assert!(message.contains("division by zero"), "{message}");
                // The trace of the inner panic is the whole active stack, host function included.
                let names: Vec<&str> = stack.iter().map(|f| f.function.as_str()).collect();
                assert_eq!(names, ["inverse", "try_call", "launch"]);
                -1
            }
            Err(other) => panic!("unexpected {other:?}"),
        }
    })
    .unwrap();
    vm.load_source(
        "game.colibri",
        "fn inverse(n: int) -> int => 100 / n\nfn launch(n: int) -> int => try_call(n)\n",
    )
    .unwrap();
    handle.set(Some(vm.get_fn::<(i64,), i64>("inverse").unwrap()));
    let launch = vm.get_fn::<(i64,), i64>("launch").unwrap();
    assert_eq!(launch.call(&mut vm, (4,)).unwrap(), 25);
    assert_eq!(launch.call(&mut vm, (0,)).unwrap(), -1);
    assert_eq!(launch.call(&mut vm, (5,)).unwrap(), 20);
}

#[test]
fn nested_host_calls_track_the_depth() {
    let (mut vm, _) = vm();
    let handle: Handle = Rc::new(Cell::new(None));
    let h = Rc::clone(&handle);
    vm.register_fn("down", move |cx: &mut colibri::Ctx, n: i64| -> i64 {
        let depth = i64::from(cx.host_depth());
        if n == 0 {
            return depth;
        }
        h.get().unwrap().call(cx, (n - 1,)).unwrap()
    })
    .unwrap();
    vm.load_source("game.colibri", "fn step(n: int) -> int => down(n)\n")
        .unwrap();
    handle.set(Some(vm.get_fn::<(i64,), i64>("step").unwrap()));
    let step = vm.get_fn::<(i64,), i64>("step").unwrap();
    // Three nested host functions, then the innermost one sees depth 4 (the 4th `down`).
    assert_eq!(step.call(&mut vm, (3,)).unwrap(), 4);
    assert_eq!(step.call(&mut vm, (0,)).unwrap(), 1);
}

#[test]
fn registering_after_a_load_is_refused() {
    let (mut vm, _) = vm();
    vm.load_source("a.colibri", "fn f() -> int => 1\n").unwrap();
    assert!(matches!(
        vm.register_fn("late", || 1i64).err(),
        Some(colibri::Error::AlreadyLoaded)
    ));
    assert!(matches!(
        vm.register_type::<f32>().err(),
        Some(colibri::Error::AlreadyLoaded)
    ));
}

#[test]
fn bad_registrations_are_errors() {
    let (mut vm, _) = vm();
    vm.register_fn("once", || 1i64).unwrap();
    let dup = vm.register_fn("once", || 2i64).err().unwrap();
    assert!(
        matches!(&dup, colibri::Error::Registration { name, .. } if name == "once"),
        "{dup:?}"
    );
    assert!(dup.to_string().contains("already") || dup.to_string().contains("registered"));
    for bad in [
        "print",
        "panic",
        "some",
        "if",
        "has space",
        "",
        "9lives",
        "a.b",
    ] {
        let err = vm.register_fn(bad, || 1i64).err();
        assert!(
            matches!(err, Some(colibri::Error::Registration { .. })),
            "`{bad}` should be refused, got {err:?}"
        );
    }
    // Nothing of the failures is left behind.
    vm.load_source("a.colibri", "fn main() {\n    print(once())\n}\n")
        .unwrap();
}

#[test]
fn a_script_cannot_redefine_a_host_function() {
    let (mut vm, _) = vm();
    vm.register_fn("spawn", |x: f32| x).unwrap();
    let err = vm
        .load_source("game.colibri", "fn spawn(x: f32) -> f32 => x\n")
        .unwrap_err();
    let text = err.render(false);
    assert!(
        text.contains("`spawn` is a host function and cannot be redefined"),
        "{text}"
    );
    assert!(text.contains("game.colibri"), "{text}");
    // A compile error loads nothing: another program can still be loaded.
    vm.load_source("game.colibri", "fn main() {\n    print(spawn(1.0))\n}\n")
        .unwrap();
}

#[test]
fn host_functions_are_not_values() {
    let (mut vm, _) = vm();
    vm.register_fn("spawn", |x: f32| x).unwrap();
    let err = vm
        .load_source("game.colibri", "let f = spawn\n")
        .unwrap_err();
    let text = err.render(false);
    assert!(
        text.contains("the host function `spawn` cannot be used as a value"),
        "{text}"
    );
    assert!(text.contains("wrap it in a closure"), "{text}");
    // The suggested closure works.
    let (mut vm, out) = self::vm();
    vm.register_fn("spawn", |x: f32| x + 1.0).unwrap();
    run(
        &mut vm,
        "fn main() {\n    let f = fn(x: f32) => spawn(x)\n    print(f(1.0))\n}\n",
    )
    .unwrap();
    assert_eq!(out.contents(), "2.0\n");
}

#[test]
fn closure_parameters_are_unnamed_and_named_arguments_are_rejected() {
    let (mut vm, _) = vm();
    vm.register_fn("spawn", |x: f32, y: f32| x + y).unwrap();
    let err = vm
        .load_source("game.colibri", "print(spawn(x: 1.0, y: 2.0))\n")
        .unwrap_err();
    let text = err.render(false);
    assert!(
        text.contains("`spawn` takes positional arguments only"),
        "{text}"
    );
    let (mut vm, out) = self::vm();
    vm.register_fn("spawn", |x: f32, y: f32| x + y).unwrap();
    vm.load_source("game.colibri", "print(spawn(1.0, 2.0))\n")
        .unwrap();
    assert_eq!(out.contents(), "3.0\n");
}

#[test]
fn argument_type_errors_are_ordinary_diagnostics() {
    let (mut vm, _) = vm();
    vm.register_fn("spawn", |x: f32, y: f32| x + y).unwrap();
    let err = vm
        .load_source(
            "game.colibri",
            "print(spawn(1.0, \"two\"))\nprint(spawn(1.0))\n",
        )
        .unwrap_err();
    let text = err.render(false);
    assert!(text.contains("expected `f32`, found `str`"), "{text}");
    assert!(text.contains("missing argument"), "{text}");
}

#[test]
fn an_unconsumed_host_value_is_released_properly() {
    // A host function that returns a script-visible `Error` and a list of them: the values
    // the script drops again go through the interpreter's release.
    let (mut vm, out) = vm();
    vm.register_fn("many_errs", || -> Vec<Result<i64, String>> {
        vec![Ok(1), Err("bad".into()), Ok(3)]
    })
    .unwrap();
    run(
        &mut vm,
        "fn main() {\n    for r in many_errs() {\n        print(r)\n    }\n}\n",
    )
    .unwrap();
    assert_eq!(out.contents(), "ok(1)\nerr(bad)\nok(3)\n");
}

#[test]
fn the_default_module_needs_no_import() {
    let (mut vm, out) = vm();
    vm.register_fn("spawn_enemy", |x: f32, y: f32| format!("enemy at {x},{y}"))
        .unwrap();
    vm.load_source("game.colibri", "print(spawn_enemy(1.0, 2.5))\n")
        .unwrap();
    assert_eq!(out.contents(), "enemy at 1,2.5\n");
}
