//! Host functions: Rust closures scripts call (EMBEDDING §1).

mod common;
use common::vm_pending;

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

fn colibri_vm() -> (colibri::Vm, colibri::SharedBuffer) {
    vm()
}

fn run(vm: &mut colibri::Vm, src: &str) -> Result<(), colibri::Error> {
    vm.load_source("game.colibri", src)?;
    vm.run_main()
}

#[test]
fn a_script_calls_closures_of_every_shape() {
    vm_pending("a_script_calls_closures_of_every_shape", || {
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
    })
}

#[test]
fn a_leading_context_gives_access_to_host_state() {
    vm_pending("a_leading_context_gives_access_to_host_state", || {
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
    })
}

#[test]
fn closures_may_capture_what_they_like() {
    vm_pending("closures_may_capture_what_they_like", || {
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
    })
}

#[test]
fn collections_cross_both_ways() {
    vm_pending("collections_cross_both_ways", || {
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
    })
}

#[test]
fn a_rust_err_is_a_colibri_err_not_a_panic() {
    vm_pending("a_rust_err_is_a_colibri_err_not_a_panic", || {
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
    })
}

#[test]
fn main_returning_a_host_err_reports_its_message() {
    vm_pending("main_returning_a_host_err_reports_its_message", || {
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
    })
}

#[test]
fn a_rust_panic_is_a_colibri_panic_and_the_vm_stays_usable() {
    vm_pending(
        "a_rust_panic_is_a_colibri_panic_and_the_vm_stays_usable",
        || {
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
        },
    )
}

#[test]
fn a_host_function_that_cannot_read_its_argument_panics_cleanly() {
    vm_pending(
        "a_host_function_that_cannot_read_its_argument_panics_cleanly",
        || {
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
                message.contains("host function `index`")
                    && message.contains("does not fit a `usize`"),
                "{message}"
            );
        },
    )
}

#[test]
fn a_host_function_calls_back_into_the_script() {
    vm_pending("a_host_function_calls_back_into_the_script", || {
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
    })
}

#[test]
fn a_panic_in_a_reentrant_call_reaches_the_host_function_not_the_vm() {
    vm_pending(
        "a_panic_in_a_reentrant_call_reaches_the_host_function_not_the_vm",
        || {
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
        },
    )
}

#[test]
fn nested_host_calls_track_the_depth() {
    vm_pending("nested_host_calls_track_the_depth", || {
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
    })
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
    vm_pending("host_functions_are_not_values", || {
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
    })
}

#[test]
fn closure_parameters_are_unnamed_and_named_arguments_are_rejected() {
    vm_pending(
        "closure_parameters_are_unnamed_and_named_arguments_are_rejected",
        || {
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
        },
    )
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
    vm_pending("an_unconsumed_host_value_is_released_properly", || {
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
    })
}

#[test]
fn the_default_module_needs_no_import() {
    vm_pending("the_default_module_needs_no_import", || {
        let (mut vm, out) = vm();
        vm.register_fn("submit_job", |x: f32, y: f32| format!("job at {x},{y}"))
            .unwrap();
        vm.load_source("game.colibri", "print(submit_job(1.0, 2.5))\n")
            .unwrap();
        assert_eq!(out.contents(), "job at 1,2.5\n");
    })
}

// ---- typed errors: `Result<T, TypedError>` and `HostModule::register_module_enum` ----

fn typed_vm() -> (colibri::Vm, colibri::SharedBuffer) {
    let (mut vm, out) = vm();
    {
        let mut disk = vm.module("disk");
        disk.register_module_enum("Error", &["Full", "Gone"])
            .unwrap();
        disk.register_fn("save", |n: i64| -> Result<i64, colibri::TypedError> {
            match n {
                0 => Err(colibri::TypedError::new(
                    "disk",
                    "Error",
                    "Full",
                    "the disk is full",
                )),
                1 => Err(colibri::TypedError::new(
                    "disk",
                    "Error",
                    "Gone",
                    "the disk is gone",
                )),
                n => Ok(n),
            }
        })
        .unwrap();
    }
    {
        // Another module with an `Error` of its own.
        let mut net = vm.module("net");
        net.register_module_enum("Error", &["Down", "Slow", "Gone"])
            .unwrap();
        net.register_fn("send", |n: i64| -> Result<i64, colibri::TypedError> {
            match n {
                0 => Err(colibri::TypedError::new(
                    "net",
                    "Error",
                    "Gone",
                    "the link is gone",
                )),
                n => Ok(n),
            }
        })
        .unwrap();
    }
    (vm, out)
}

#[test]
fn a_host_error_keeps_a_typed_value_that_is_and_as_recover() {
    vm_pending(
        "a_host_error_keeps_a_typed_value_that_is_and_as_recover",
        || {
            let (mut vm, out) = typed_vm();
            run(
                &mut vm,
                r#"import disk
import net

fn store(n: int) -> Result[int] {
    let v = disk.save(n).context("storing {n}")?
    return ok(v)
}

fn main() {
    for n in [0, 1, 5] {
        match store(n) {
            ok(v) => print("saved {v}")
            err(e) if e.is[disk.Error]() => match e.as[disk.Error]().expect("checked") {
                Full => print("full: {e}")
                Gone => print("gone: {e}")
                _ => print("another: {e}")
            }
            err(e) => print("other: {e}")
        }
    }
    let e = net.send(0).err().expect("fails")
    print("{e.is[net.Error]()} {e.is[disk.Error]()}")
    // `Gone` of `net.Error` is not `Gone` of `disk.Error`.
    print("{e.as[net.Error]()} {e.as[disk.Error]()}")
    print(e.message())
}
"#,
            )
            .unwrap();
            assert_eq!(
                out.contents(),
                "full: storing 0: the disk is full\ngone: storing 1: the disk is gone\nsaved 5\ntrue false\nsome(Gone) none\nthe link is gone\n"
            );
        },
    )
}

#[test]
fn a_typed_error_comes_back_to_rust_as_its_message() {
    vm_pending("a_typed_error_comes_back_to_rust_as_its_message", || {
        let (mut vm, _) = typed_vm();
        vm.load_source(
            "game.colibri",
            "import disk\nfn save(n: int) -> Result[int] {\n    return disk.save(n)\n}\n",
        )
        .unwrap();
        let save = vm.get_fn::<(i64,), Result<i64, String>>("save").unwrap();
        assert_eq!(
            save.call(&mut vm, (0,)).unwrap(),
            Err("the disk is full".to_string())
        );
        assert_eq!(save.call(&mut vm, (3,)).unwrap(), Ok(3));
    })
}

#[test]
fn a_module_enum_is_declared_before_the_program_loads_and_may_not_clash() {
    let (mut vm, _) = typed_vm();
    let text = vm.module("disk").interface_text();
    assert!(text.contains("enum Error { Full, Gone }"), "{text}");
    let text = vm.module("net").interface_text();
    assert!(text.contains("enum Error { Down, Slow, Gone }"), "{text}");
    // The same name in the same module with other variants is a conflict.
    let err = vm
        .module("disk")
        .register_module_enum("Error", &["Other"])
        .unwrap_err();
    assert!(matches!(err, colibri::Error::Registration { .. }), "{err}");
    vm.load_source("game.colibri", "fn main() {}\n").unwrap();
    let err = vm
        .module("disk")
        .register_module_enum("Other", &["A"])
        .unwrap_err();
    assert!(matches!(err, colibri::Error::AlreadyLoaded), "{err}");
}

#[test]
fn a_typed_error_of_an_undeclared_enum_is_a_bug_of_the_host() {
    vm_pending(
        "a_typed_error_of_an_undeclared_enum_is_a_bug_of_the_host",
        || {
            let (mut vm, _) = vm();
            vm.module("disk")
                .register_fn("save", || -> Result<(), colibri::TypedError> {
                    Err(colibri::TypedError::new("disk", "Error", "Full", "x"))
                })
                .unwrap();
            let err = run(
                &mut vm,
                "import disk\nfn main() {\n    let _ = disk.save()\n}\n",
            )
            .unwrap_err()
            .render(false);
            assert!(
                err.contains("is not part of the loaded host interface"),
                "{err}"
            );
        },
    )
}

/// Runs `import disk; disk.save()` against a host function that fails with the error `make`
/// builds, and gives the text of the failure.
#[colibri::export(value)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Stat {
    pub size: i64,
}

#[colibri::export(value)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Mode {
    Fast,
    Slow,
}

fn failure_of_typed_error(
    declare: impl FnOnce(&mut colibri::HostModule<'_>),
    make: fn() -> colibri::TypedError,
) -> String {
    let (mut vm, _) = vm();
    {
        let mut disk = vm.module("disk");
        declare(&mut disk);
        disk.register_fn("save", move || -> Result<(), colibri::TypedError> {
            Err(make())
        })
        .unwrap();
    }
    run(
        &mut vm,
        "import disk\nfn main() {\n    let _ = disk.save()\n}\n",
    )
    .unwrap_err()
    .render(false)
}

#[test]
fn a_typed_error_names_its_variant_and_the_name_is_checked() {
    vm_pending(
        "a_typed_error_names_its_variant_and_the_name_is_checked",
        || {
            let declare = |m: &mut colibri::HostModule<'_>| {
                m.register_module_enum("Error", &["Full", "Gone"]).unwrap();
            };
            // A variant the enum does not have: a host bug with a message, not an `<enum>` value.
            let err = failure_of_typed_error(declare, || {
                colibri::TypedError::new("disk", "Error", "Gone2", "x")
            });
            assert!(
                err.contains("`disk.Error` has no variant `Gone2`; its variants are Full, Gone"),
                "{err}"
            );
            // A struct is not a module enum.
            let err = failure_of_typed_error(
                |m| {
                    m.register_module_enum("Error", &["Full"]).unwrap();
                    m.register_type::<Stat>().unwrap();
                },
                || colibri::TypedError::new("disk", "Stat", "Full", "x"),
            );
            assert!(err.contains("`disk.Stat` is not a module enum"), "{err}");
            // A plain enum of the module is not one either.
            let err = failure_of_typed_error(
                |m| {
                    m.register_module_enum("Error", &["Full"]).unwrap();
                    m.register_type::<Mode>().unwrap();
                },
                || colibri::TypedError::new("disk", "Mode", "Fast", "x"),
            );
            assert!(err.contains("`disk.Mode` is not a module enum"), "{err}");
        },
    )
}

#[test]
fn a_match_over_a_module_enum_needs_a_catch_all_arm() {
    let load = |body: &str| {
        let (mut vm, _) = typed_vm();
        vm.module("disk").register_type::<Mode>().unwrap();
        vm.load_source(
            "game.colibri",
            &format!(
                "import disk\nfn f(e: disk.Error) -> int {{\n    match e {{\n{body}    }}\n}}\n"
            ),
        )
        .map_err(|e| e.render(false))
    };
    // Every variant of today, no catch-all: the enum can grow.
    let err = load("        Full => return 1\n        Gone => return 2\n").unwrap_err();
    assert!(
        err.contains("non-exhaustive `match`") && err.contains("`disk.Error` can gain variants"),
        "{err}"
    );
    assert!(err.contains("a `_ =>` arm"), "{err}");
    // A wildcard or a binding is enough, and the arms before it are not "unreachable".
    for tail in ["_ => return 3", "other => return 3"] {
        let body = format!("        Full => return 1\n        Gone => return 2\n        {tail}\n");
        load(&body).unwrap();
    }
    // An enum registered the ordinary way stays closed.
    let (mut vm, _) = typed_vm();
    vm.module("disk").register_type::<Mode>().unwrap();
    vm.load_source(
        "game.colibri",
        "import disk\nfn f(m: disk.Mode) -> int {\n    match m {\n        Fast => return 1\n        Slow => return 2\n    }\n}\n",
    )
    .unwrap();
}

// ---- what `register_module_enum` refuses ----

fn refused(result: Result<(), colibri::Error>) -> String {
    match result {
        Err(colibri::Error::Registration { message, .. }) => message,
        other => panic!("expected a registration error, got {other:?}"),
    }
}

#[test]
fn register_module_enum_refuses_what_every_later_load_would_fail_on() {
    let (mut vm, _) = vm();
    // The default module has no module name to tell two `Error`s apart.
    let message = refused(vm.module("host").register_module_enum("Error", &["A"]));
    assert!(message.contains("named module"), "{message}");
    let mut disk = vm.module("disk");
    // Names the language or the prelude declares.
    for name in ["Option", "Result", "Show", "List", "int", "Coroutine"] {
        let message = refused(disk.register_module_enum(name, &["A"]));
        assert!(message.contains("cannot be redefined"), "{name}: {message}");
    }
    assert!(refused(disk.register_module_enum("Error", &[])).contains("at least one variant"));
    assert!(refused(disk.register_module_enum("Error", &["A", "B", "A"])).contains("twice"));
    assert!(refused(disk.register_module_enum("Error", &["A B"])).contains("not a variant name"));
    assert!(refused(disk.register_module_enum("Not a name", &["A"])).contains("type name"));
    // Nothing of that was kept: the module still loads, and a good enum goes in.
    disk.register_module_enum("Error", &["Full"]).unwrap();
    drop(disk);
    vm.load_source("game.colibri", "import disk\nfn main() {}\n")
        .unwrap();
}

#[test]
fn a_module_enum_and_another_host_type_never_share_a_name_in_either_order() {
    // A plain host type first, then a module enum of that name.
    let (mut vm, _) = colibri_vm();
    vm.module("disk").register_type::<Mode>().unwrap();
    let message = refused(vm.module("net").register_module_enum("Mode", &["A"]));
    assert!(message.contains("a name of its own"), "{message}");
    let message = refused(vm.module("disk").register_module_enum("Mode", &["A"]));
    assert!(message.contains("a name of its own"), "{message}");
    // A module enum first, then a plain host type of that name, in its module or another.
    let (mut vm, _) = colibri_vm();
    vm.module("net")
        .register_module_enum("Mode", &["A"])
        .unwrap();
    let message = refused(vm.module("disk").register_type::<Mode>());
    assert!(message.contains("a name of its own"), "{message}");
    let message = refused(vm.module("net").register_type::<Mode>());
    assert!(message.contains("a name of its own"), "{message}");
    // Two modules may each declare a module enum of the same name.
    vm.module("disk")
        .register_module_enum("Mode", &["B"])
        .unwrap();
}

/// A type whose description is the module enum `disk.Error`, to put it in a signature.
struct ErrorKind;

impl colibri::IntoColibri for ErrorKind {
    fn ty(reg: &mut colibri::TypeRegistry) -> colibri::TypeDesc {
        reg.define_module_enum("Error", &["Full"])
    }
    fn into_colibri(self, _cx: &mut colibri::Cx<'_>) -> colibri::Value {
        colibri::Value::unit()
    }
}

#[test]
fn a_host_signature_cannot_mention_a_module_enum() {
    let (mut vm, _) = vm();
    let mut disk = vm.module("disk");
    let err = disk
        .register_fn("kind", || ErrorKind)
        .err()
        .expect("refused");
    let colibri::Error::Registration { message, .. } = err else {
        panic!("a registration error: {err:?}");
    };
    assert!(message.contains("module enum"), "{message}");
}

#[test]
fn an_embedders_module_enum_converts_to_error_without_an_impl() {
    vm_pending(
        "an_embedders_module_enum_converts_to_error_without_an_impl",
        || {
            let (mut vm, out) = typed_vm();
            run(
                &mut vm,
                r#"import disk

fn fake() -> Result[int] {
    return err(disk.Error.Gone)
}

fn typed(n: int) -> Result[int, disk.Error] {
    if n == 0 { return err(disk.Error.Full) }
    return ok(n)
}

fn wrapped(n: int) -> Result[int] {
    let v = typed(n)?
    return ok(v)
}

fn main() {
    let e = fake().err().expect("fails")
    print("{e} {e.is[disk.Error]()} {e.as[disk.Error]()}")
    let e = wrapped(0).err().expect("fails")
    print("{e} {e.as[disk.Error]() == some(disk.Error.Full)}")
    print(wrapped(2).expect("ok"))
}
"#,
            )
            .unwrap();
            assert_eq!(out.contents(), "Gone true some(Gone)\nFull true\n2\n");
            // The name stays the module's: a program cannot write the impl itself.
            let err = run(
                &mut typed_vm().0,
                "import disk\nimpl IntoError for disk.Error {}\nfn main() {}\n",
            )
            .unwrap_err()
            .render(false);
            assert!(
                err.contains("cannot implement the built-in `IntoError` for `disk.Error`"),
                "{err}"
            );
        },
    )
}

#[test]
fn a_display_error_keeps_a_str_and_a_typed_error_keeps_none() {
    vm_pending(
        "a_display_error_keeps_a_str_and_a_typed_error_keeps_none",
        || {
            let (mut vm, out) = typed_vm();
            vm.register_fn("plain", || -> Result<(), String> {
                Err("plain failure".to_string())
            })
            .unwrap();
            run(
                &mut vm,
                r#"import disk

fn main() {
    let p = plain().err().expect("fails")
    print("{p.as[str]()} {p.message()}")
    let t = disk.save(0).err().expect("fails")
    print("{t.as[str]()} {t.message()}")
}
"#,
            )
            .unwrap();
            assert_eq!(
                out.contents(),
                "some(\"plain failure\") plain failure\nnone the disk is full\n"
            );
        },
    )
}
