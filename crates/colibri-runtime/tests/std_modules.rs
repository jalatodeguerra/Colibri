//! The machinery the standard library is built on (ARCHITECTURE §13): `std.*` host modules,
//! constants in an interface, generic functions that read their type arguments, and
//! `Ctx::raise`. The library itself is tested in the `colibri-std` crate.

mod common;
use common::vm_pending;

use colibri::{Cap, Ctx, Value, ValueKind, reflect};
use colibri_runtime as colibri;

fn vm() -> (colibri::Vm, colibri::SharedBuffer) {
    let out = colibri::SharedBuffer::new();
    let vm = colibri::Vm::builder().stdout(Box::new(out.clone())).build();
    (vm, out)
}

#[test]
fn a_std_module_is_imported_by_its_short_name_or_with_the_std_prefix() {
    vm_pending(
        "a_std_module_is_imported_by_its_short_name_or_with_the_std_prefix",
        || {
            let (mut vm, out) = vm();
            vm.std_module("demo")
                .register_fn_named("twice", ["x"], |x: i64| x * 2)
                .unwrap();
            assert_eq!(vm.module_names(), ["host", "std.demo"]);
            vm.load_source(
        "a.colibri",
        "import demo\nimport std.demo as again\nfn main() {\n    print(demo.twice(2))\n    print(again.twice(3))\n}\n",
    )
    .unwrap();
            vm.run_main().unwrap();
            assert_eq!(out.contents(), "4\n6\n");
        },
    )
}

#[test]
fn the_name_std_is_reserved_for_the_standard_library() {
    let (mut vm, _) = vm();
    for name in ["std", "std.math", "std."] {
        let err = vm.module(name).register_fn("f", || 1i64).err().unwrap();
        assert!(
            err.to_string()
                .contains("reserved for the standard library"),
            "{name}: {err}"
        );
    }
    assert!(vm.module("stdlib").register_fn("f", || 1i64).is_ok());
}

#[test]
fn a_user_module_with_the_name_of_a_std_module_is_ambiguous() {
    vm_pending(
        "a_user_module_with_the_name_of_a_std_module_is_ambiguous",
        || {
            let (mut vm, _) = vm();
            vm.std_module("demo").register_fn("f", || 1i64).unwrap();
            vm.module("demo").register_fn("g", || 2i64).unwrap();
            let err = vm
                .load_source("a.colibri", "import demo\nfn main() {}\n")
                .unwrap_err()
                .render(false);
            assert!(err.contains("`import demo` is ambiguous"), "{err}");
            assert!(err.contains("the host module `demo`"), "{err}");
            assert!(
                err.contains("the standard library module `std.demo`"),
                "{err}"
            );
            // The prefix settles it.
            let (mut vm2, out) = self::vm();
            vm2.std_module("demo").register_fn("f", || 1i64).unwrap();
            vm2.module("demo").register_fn("g", || 2i64).unwrap();
            vm2.load_source(
                "a.colibri",
                "import std.demo\nfn main() {\n    print(demo.f())\n}\n",
            )
            .unwrap();
            vm2.run_main().unwrap();
            assert_eq!(out.contents(), "1\n");
        },
    )
}

#[test]
fn constants_are_part_of_the_interface_text() {
    let (mut vm, out) = vm();
    let mut m = vm.std_module("demo");
    m.register_const_float("RATIO", 1.5).unwrap();
    m.register_const_int("LIMIT", 10).unwrap();
    m.register_fn_named("f", [], || 1i64).unwrap();
    let text = m.interface_text();
    assert!(text.contains("const RATIO: float = 1.5\n"), "{text}");
    assert!(text.contains("const LIMIT: int = 10\n"), "{text}");
    vm.load_source(
        "a.colibri",
        "import demo\nfn main() {\n    print(demo.RATIO * 2.0)\n    print(demo.LIMIT + 1)\n}\n",
    )
    .unwrap();
    vm.run_main().unwrap();
    assert_eq!(out.contents(), "3.0\n11\n");
}

#[test]
fn constants_that_cannot_be_written_are_refused() {
    let (mut vm, _) = vm();
    let mut m = vm.std_module("demo");
    for bad in [f64::NAN, f64::INFINITY, -1.0, 1e300, 1e-10] {
        assert!(m.register_const_float("X", bad).is_err(), "{bad}");
    }
    assert!(m.register_const_int("N", -1).is_err());
    m.register_const_float("OK", 0.0).unwrap();
    assert!(m.register_const_float("OK", 1.0).is_err(), "twice");
    // The default module has no constants.
    assert!(vm.module("host").register_const_int("C", 1).is_err());
    // A function cannot take a constant's name.
    let mut m = vm.std_module("other");
    m.register_const_int("C", 1).unwrap();
    assert!(m.register_fn("C", || 1i64).is_err());
}

#[test]
fn a_constant_cannot_be_assigned_to() {
    let (mut vm, _) = vm();
    vm.std_module("demo").register_const_int("N", 1).unwrap();
    let err = vm
        .load_source("a.colibri", "import demo\nfn main() {\n    demo.N = 2\n}\n")
        .unwrap_err()
        .render(false);
    assert!(err.contains("cannot assign to the constant `N`"), "{err}");
}

#[test]
fn a_generic_host_function_gets_the_type_arguments_of_the_call() {
    vm_pending(
        "a_generic_host_function_gets_the_type_arguments_of_the_call",
        || {
            let (mut vm, out) = vm();
            vm.std_module("demo")
                .register_fn_named("name_of", ["value"], |cx: &mut Ctx, _value: Value| {
                    let ty = cx.type_args().first().cloned().unwrap();
                    let defs = cx.type_defs();
                    match ty {
                        reflect::Ty::Adt(id, _) => defs[id.0 as usize].name.clone(),
                        other => other.to_string(),
                    }
                })
                .unwrap()
                .typed()
                .unwrap();
            let text = vm.std_module("demo").interface_text();
            assert!(
                text.contains("@typed fn name_of[T](value: T) -> str"),
                "{text}"
            );
            vm.load_source(
        "a.colibri",
        "import demo\nstruct Point { x: int }\nfn main() {\n    print(demo.name_of(Point { x: 1 }))\n    print(demo.name_of(3))\n    print(demo.name_of[List[str]]([]))\n}\n",
    )
    .unwrap();
            vm.run_main().unwrap();
            assert_eq!(out.contents(), "Point\nint\nList[str]\n");
        },
    )
}

#[test]
fn a_typed_function_cannot_be_called_with_a_type_parameter() {
    let (mut vm, _) = vm();
    vm.std_module("demo")
        .register_fn_named("name_of", ["value"], |_value: Value| 1i64)
        .unwrap()
        .typed()
        .unwrap();
    let err = vm
        .load_source(
            "a.colibri",
            "import demo\nfn wrap[T](x: T) -> int => demo.name_of(x)\nfn main() {}\n",
        )
        .unwrap_err()
        .render(false);
    assert!(
        err.contains("needs to know its type at the call, but here the type is a type parameter"),
        "{err}"
    );
}

#[test]
fn typed_needs_a_generic_function() {
    let (mut vm, _) = vm();
    let mut module = vm.std_module("demo");
    let registration = module.register_fn_named("f", [], || 1i64).unwrap();
    let err = registration.typed().err().expect("not generic");
    assert!(err.to_string().contains("generic"), "{err}");
}

#[test]
fn values_can_be_read_and_built_without_a_static_type() {
    vm_pending("values_can_be_read_and_built_without_a_static_type", || {
        let (mut vm, out) = vm();
        vm.std_module("demo")
            .register_fn_named("describe", ["value"], |value: Value| match value.kind() {
                ValueKind::Int(i) => format!("int {i}"),
                ValueKind::Str(s) => format!("str {s}"),
                ValueKind::List(items) => format!("list of {}", items.len()),
                ValueKind::Map(entries) => format!("map of {}", entries.len()),
                ValueKind::Enum { variant, .. } => format!("variant {variant}"),
                ValueKind::Struct { fields, .. } => format!("struct of {}", fields.len()),
                other => format!("{other:?}"),
            })
            .unwrap();
        vm.std_module("demo")
            .register_fn_named("pair", ["a", "b"], |a: Value, b: Value| {
                Value::tuple(vec![b, a])
            })
            .unwrap();
        vm.load_source(
        "a.colibri",
        "import demo\nstruct P { a: int, b: int }\nenum E { X, Y }\nfn main() {\n    print(demo.describe(7))\n    print(demo.describe(\"s\"))\n    print(demo.describe([1, 2]))\n    print(demo.describe({\"k\": 1}))\n    print(demo.describe(E.Y))\n    print(demo.describe(P { a: 1, b: 2 }))\n    print(demo.pair(1, 2))\n}\n",
    )
    .unwrap();
        vm.run_main().unwrap();
        assert_eq!(
            out.contents(),
            "int 7\nstr s\nlist of 2\nmap of 1\nvariant 1\nstruct of 2\n(2, 1)\n"
        );
    })
}

fn stop(cx: &mut Ctx) {
    cx.raise("stopped on purpose");
}

#[test]
fn raise_is_a_panic_with_exactly_this_message_and_defers_run() {
    vm_pending(
        "raise_is_a_panic_with_exactly_this_message_and_defers_run",
        || {
            let (mut vm, out) = vm();
            vm.register_fn("stop", stop).unwrap();
            vm.load_source(
        "a.colibri",
        "fn main() {\n    defer print(\"cleanup\")\n    stop()\n    print(\"not reached\")\n}\n",
    )
    .unwrap();
            let err = vm.run_main().unwrap_err();
            match &err {
                colibri::Error::Panic { message, .. } => assert_eq!(message, "stopped on purpose"),
                other => panic!("{other:?}"),
            }
            assert_eq!(out.contents(), "cleanup\n");
            // The Vm is usable afterwards.
            assert!(vm.run_main().is_err());
        },
    )
}

#[test]
fn the_write_function_of_a_context_goes_where_print_goes() {
    vm_pending(
        "the_write_function_of_a_context_goes_where_print_goes",
        || {
            let (mut vm, out) = vm();
            vm.register_fn("emit", |cx: &mut Ctx, text: String| cx.write_stdout(&text))
                .unwrap();
            vm.load_source(
                "a.colibri",
                "fn main() {\n    emit(\"a\")\n    emit(\"b\")\n    print(\"c\")\n}\n",
            )
            .unwrap();
            vm.run_main().unwrap();
            assert_eq!(out.contents(), "abc\n");
        },
    )
}

#[test]
fn a_capability_of_a_std_function_is_checked_like_any_other() {
    let (mut vm, _) = vm();
    vm.std_module("demo")
        .register_fn_named("peek", [], || 1i64)
        .unwrap()
        .requires(Cap::Os);
    let err = vm
        .load_source(
            "a.colibri",
            "import demo\nfn main() {\n    print(demo.peek())\n}\n",
        )
        .unwrap_err()
        .render(false);
    assert!(
        err.contains("`demo.peek` needs the `os` capability, which this host did not grant"),
        "{err}"
    );
}

// ---- lazy modules ----

mod lazy {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    macro_rules! counted {
        ($counter:ident, $register:ident, $module:literal) => {
            static $counter: AtomicUsize = AtomicUsize::new(0);
            fn $register(vm: &mut colibri::Vm) -> Result<(), colibri::Error> {
                $counter.fetch_add(1, Ordering::SeqCst);
                vm.std_module($module)
                    .register_fn_named("twice", ["x"], |x: i64| x * 2)
                    .map(|_| ())
            }
        };
    }

    counted!(NOT_IMPORTED, register_not_imported, "idle");
    #[test]
    fn a_module_nobody_imports_is_never_registered() {
        let (mut vm, out) = vm();
        vm.lazy_std_module("idle", register_not_imported);
        assert_eq!(vm.module_names(), ["host", "std.idle"]);
        vm.load_source("a.colibri", "print(1)\n").unwrap();
        assert_eq!(out.contents(), "1\n");
        assert_eq!(NOT_IMPORTED.load(Ordering::SeqCst), 0);
    }

    counted!(IMPORTED, register_imported, "busy");
    #[test]
    fn an_import_registers_the_module_once() {
        vm_pending("lazy::an_import_registers_the_module_once", || {
            let (mut vm, out) = vm();
            vm.lazy_std_module("busy", register_imported);
            vm.load_source(
                "a.colibri",
                "import busy\nimport std.busy as again\nprint(busy.twice(2) + again.twice(1))\n",
            )
            .unwrap();
            assert_eq!(out.contents(), "6\n");
            assert_eq!(IMPORTED.load(Ordering::SeqCst), 1);
            assert_eq!(vm.module_names(), ["host", "std.busy"]);
        })
    }

    counted!(BY_NAME, register_by_name, "named");
    #[test]
    fn asking_for_the_module_by_name_registers_it() {
        let (mut vm, _) = vm();
        vm.lazy_std_module("named", register_by_name);
        assert_eq!(BY_NAME.load(Ordering::SeqCst), 0);
        let text = vm.std_module("named").interface_text();
        assert!(text.contains("fn twice(x: int) -> int"), "{text}");
        assert_eq!(BY_NAME.load(Ordering::SeqCst), 1);
        // And once only.
        vm.std_module("named");
        assert_eq!(BY_NAME.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_name_that_is_not_declared_is_unknown_as_before() {
        let (mut vm, _) = vm();
        vm.lazy_std_module("known", register_by_name_unused);
        let err = vm
            .load_source("a.colibri", "import unknown\nfn main() {}\n")
            .unwrap_err()
            .render(false);
        assert!(err.contains("unknown"), "{err}");
    }

    fn register_by_name_unused(_: &mut colibri::Vm) -> Result<(), colibri::Error> {
        Ok(())
    }

    counted!(IN_SESSION, register_in_session, "session");
    #[test]
    fn a_session_has_every_module() {
        vm_pending("lazy::a_session_has_every_module", || {
            let (mut vm, _) = vm();
            vm.lazy_std_module("session", register_in_session);
            vm.session_start().unwrap();
            assert_eq!(IN_SESSION.load(Ordering::SeqCst), 1);
            let check = vm
                .session_check("import session\nlet x = session.twice(4)\n")
                .unwrap();
            vm.session_commit(check, &[], &[]).unwrap();
        })
    }

    counted!(AFTER_RELOAD, register_after_reload, "later");
    #[test]
    fn a_reload_can_start_importing_a_module() {
        vm_pending("lazy::a_reload_can_start_importing_a_module", || {
            let dir =
                std::env::temp_dir().join(format!("colibri-lazy-reload-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let file = dir.join("a.colibri");
            std::fs::write(&file, "fn n() -> int => 1\n").unwrap();
            let (mut vm, _) = vm();
            vm.lazy_std_module("later", register_after_reload);
            vm.load_file(&file).unwrap();
            assert_eq!(AFTER_RELOAD.load(Ordering::SeqCst), 0);
            // Same length, so only the content tells that it changed; the bump of the mtime is
            // what `reload_changed` polls.
            std::thread::sleep(std::time::Duration::from_millis(20));
            std::fs::write(&file, "import later\nfn n() -> int => later.twice(21)\n").unwrap();
            vm.reload_changed().unwrap();
            assert_eq!(AFTER_RELOAD.load(Ordering::SeqCst), 1);
            let n: i64 = vm
                .get_fn::<(), i64>("n")
                .unwrap()
                .call(&mut vm, ())
                .unwrap();
            assert_eq!(n, 42);
            let _ = std::fs::remove_dir_all(&dir);
        })
    }

    // ---- a host type that clashes with the module's, and a module that panics ----

    struct Mine;
    impl colibri::HostType for Mine {
        const NAME: &'static str = "Child";
        fn describe(d: &mut colibri::TypeBuilder<'_, Self>) {
            d.static_fn("make", [], || Mine);
            d.method("id", [], |_: &Mine| 1i64);
        }
    }

    struct Std;
    impl colibri::HostType for Std {
        const NAME: &'static str = "Child";
        fn describe(d: &mut colibri::TypeBuilder<'_, Self>) {
            d.static_fn("make", [], || Std);
            d.method("id", [], |_: &Std| 2i64);
        }
    }

    /// A second class of the same name in the same module: that is a clash.
    struct StdToo;
    impl colibri::HostType for StdToo {
        const NAME: &'static str = "Child";
        fn describe(d: &mut colibri::TypeBuilder<'_, Self>) {
            d.method("id", [], |_: &StdToo| 3i64);
        }
    }

    /// Registers `Child` once, which is fine, and another class of the same name, which is
    /// not: the module's own types are known by their name inside it.
    fn register_clashing(vm: &mut colibri::Vm) -> Result<(), colibri::Error> {
        let mut m = vm.std_module("clash");
        m.register_type::<Std>()?;
        m.register_type::<StdToo>()
    }

    fn register_alongside(vm: &mut colibri::Vm) -> Result<(), colibri::Error> {
        vm.std_module("clash").register_type::<Std>()
    }

    fn check_clash_is_an_error(vm: &mut colibri::Vm) {
        let err = vm
            .load_source("a.colibri", "import clash\nfn main() {}\n")
            .unwrap_err();
        let text = err.to_string();
        assert!(matches!(err, colibri::Error::Registration { .. }), "{text}");
        assert!(text.contains("std.clash"), "{text}");
        assert!(text.contains("two different Rust types"), "{text}");
        // The same failure again, not a panic and not a second registration.
        let again = vm
            .load_source("a.colibri", "import clash\nfn main() {}\n")
            .unwrap_err()
            .to_string();
        assert_eq!(again, text);
        // A program that does not import it still loads, and then the guard against
        // registering after a load is on.
        vm.load_source("b.colibri", "fn main() {}\n").unwrap();
        assert!(matches!(
            vm.register_fn("late", || 1i64),
            Err(colibri::Error::AlreadyLoaded)
        ));
    }

    #[test]
    fn two_classes_of_a_name_in_one_module_are_a_load_error() {
        let (mut vm, _) = vm();
        vm.lazy_std_module("clash", register_clashing);
        check_clash_is_an_error(&mut vm);
    }

    /// The embedder's `Child` and the module's own `Child` are two types: it is not an
    /// error whichever is registered first, and both work in one program.
    fn check_alongside(vm: &mut colibri::Vm, out: &colibri::SharedBuffer) {
        vm.load_source(
            "a.colibri",
            "import clash
fn main() {
    print(Child.make().id())
    print(clash.Child.make().id())
}
",
        )
        .unwrap_or_else(|e| panic!("{}", e.render(false)));
        vm.run_main().unwrap();
        assert_eq!(out.contents(), "1\n2\n");
    }

    #[test]
    fn an_embedder_type_of_the_modules_name_registered_before_it_coexists() {
        vm_pending(
            "lazy::an_embedder_type_of_the_modules_name_registered_before_it_coexists",
            || {
                let (mut vm, out) = vm();
                vm.register_type::<Mine>().unwrap();
                vm.lazy_std_module("clash", register_alongside);
                check_alongside(&mut vm, &out);
            },
        )
    }

    #[test]
    fn an_embedder_type_of_the_modules_name_registered_after_it_coexists() {
        vm_pending(
            "lazy::an_embedder_type_of_the_modules_name_registered_after_it_coexists",
            || {
                let (mut vm, out) = vm();
                vm.lazy_std_module("clash", register_alongside);
                vm.register_type::<Mine>().unwrap();
                check_alongside(&mut vm, &out);
            },
        )
    }

    #[test]
    fn a_failed_module_fails_every_session_entry_point() {
        vm_pending(
            "lazy::a_failed_module_fails_every_session_entry_point",
            || {
                let (mut vm, _) = vm();
                vm.register_type::<Mine>().unwrap();
                vm.lazy_std_module("clash", register_clashing);
                let first = vm.session_start().unwrap_err().to_string();
                assert!(first.contains("std.clash"), "{first}");
                // Retrying reports the same error, it does not start a session around a
                // half-registered module.
                let second = vm.session_start().unwrap_err().to_string();
                assert_eq!(second, first);
                let load = vm
                    .load_source("a.colibri", "import clash\nfn main() {}\n")
                    .unwrap_err()
                    .to_string();
                assert_eq!(second, load);
                let check = vm
                    .session_check("import clash\nlet x = 1\n")
                    .err()
                    .expect("the check fails")
                    .to_string();
                assert_eq!(check, load);
            },
        )
    }

    fn register_panicking(_: &mut colibri::Vm) -> Result<(), colibri::Error> {
        panic!("a bug in the module")
    }

    #[test]
    fn a_panicking_registration_leaves_the_vm_consistent() {
        let (mut vm, _) = vm();
        vm.lazy_std_module("boom", register_panicking);
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = vm.load_source("a.colibri", "import boom\nfn main() {}\n");
        }));
        assert!(outcome.is_err());
        // The module counts as failed: importing it again is an error, not a retry.
        let err = vm
            .load_source("a.colibri", "import boom\nfn main() {}\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("std.boom"), "{err}");
        // And `bulk` was restored: after a load, registering is refused.
        vm.load_source("b.colibri", "fn main() {}\n").unwrap();
        assert!(matches!(
            vm.register_fn("late", || 1i64),
            Err(colibri::Error::AlreadyLoaded)
        ));
    }
}

#[test]
fn a_field_default_call_refuses_a_function_that_takes_parameters() {
    vm_pending(
        "a_field_default_call_refuses_a_function_that_takes_parameters",
        || {
            let (mut vm, out) = vm();
            vm.std_module("demo")
                .register_fn_named("probe", [], |cx: &mut Ctx| {
                    // The real default of `S.x`.
                    let defs = cx.type_defs_shared();
                    let default = defs
                        .iter()
                        .find(|t| t.name == "S")
                        .and_then(|t| match &t.kind {
                            reflect::TypeDefKind::Struct { fields } => fields[0].default,
                            _ => None,
                        })
                        .unwrap();
                    let value = cx.call_field_default(default).unwrap();
                    let mut refused = 0;
                    // Every function of the program, whatever it takes: none may be called with
                    // missing arguments.
                    for id in 0..64 {
                        if id != default.0
                            && let Err(e) = cx.call_field_default(reflect::FnId(id))
                            && e.to_string().contains("takes 1 parameter")
                        {
                            refused += 1;
                        }
                    }
                    let ValueKind::Int(n) = value.kind() else {
                        panic!("not an int")
                    };
                    format!("{n} refused {refused}")
                })
                .unwrap();
            vm.load_source(
        "a.colibri",
        "import demo\nstruct S { x: int = 5 }\nfn add_one(a: int) -> int => a + 1\nfn main() {\n    print(demo.probe())\n}\n",
    )
    .unwrap();
            vm.run_main().unwrap();
            let text = out.contents();
            assert!(text.starts_with("5 refused "), "{text}");
            assert!(!text.starts_with("5 refused 0"), "{text}");
        },
    )
}
