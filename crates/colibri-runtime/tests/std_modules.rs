//! The machinery the standard library is built on (ARCHITECTURE §13): `std.*` host modules,
//! constants in an interface, generic functions that read their type arguments, and
//! `Ctx::raise`. The library itself is tested in the `colibri-std` crate.

use colibri::{Cap, Ctx, Value, ValueKind, reflect};
use colibri_runtime as colibri;

fn vm() -> (colibri::Vm, colibri::SharedBuffer) {
    let out = colibri::SharedBuffer::new();
    let vm = colibri::Vm::builder().stdout(Box::new(out.clone())).build();
    (vm, out)
}

#[test]
fn a_std_module_is_imported_by_its_short_name_or_with_the_std_prefix() {
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
}

fn stop(cx: &mut Ctx) {
    cx.raise("stopped on purpose");
}

#[test]
fn raise_is_a_panic_with_exactly_this_message_and_defers_run() {
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
}

#[test]
fn the_write_function_of_a_context_goes_where_print_goes() {
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
