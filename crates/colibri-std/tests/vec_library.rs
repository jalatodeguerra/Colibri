//! `vec` is Colibri source (`src/colibri/vec.colibri`), so a host reaches it the way it reaches any
//! script type: through its own value structs at the boundary, and through script functions that
//! return and take vectors. `install_vec` gives a `Vm` without the rest of the standard library
//! `import vec`.

use colibri_runtime::{self as colibri, Value, ValueKind};

/// The host's own vector, a plain value struct a script can build and read.
#[colibri::export(value)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Vec3 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

#[colibri::export]
pub fn scale(v: Vec3, by: f32) -> Vec3 {
    Vec3 {
        x: v.x * by,
        y: v.y * by,
        z: v.z * by,
    }
}

fn vm() -> (colibri::Vm, colibri::SharedBuffer) {
    let out = colibri::SharedBuffer::new();
    // Pinned to the tree-walker: this tests the standard library, not the engine, and host
    // functions do not run on the bytecode VM yet (`COLIBRI_ENGINE=vm`). Unpin with step 2c.
    let mut vm = colibri::Vm::builder()
        .engine(colibri::EngineKind::Tree)
        .stdout(Box::new(out.clone()))
        .build();
    colibri_std::install_vec(&mut vm).unwrap();
    (vm, out)
}

fn f32s(v: &Value) -> Vec<f32> {
    let ValueKind::Struct { fields, .. } = v.kind() else {
        panic!("not a struct: {v:?}");
    };
    fields
        .iter()
        .map(|f| match f.kind() {
            ValueKind::F32(x) => x,
            other => panic!("not an f32: {other:?}"),
        })
        .collect()
}

#[test]
fn a_host_value_struct_converts_to_and_from_vec_in_the_script() {
    let (mut vm, out) = vm();
    scale::register(&mut vm).unwrap();
    vm.load_source(
        "game.colibri",
        r#"
import vec

fn to_vec(v: Vec3) -> vec.vec3 => vec.vec3(v.x, v.y, v.z)
fn from_vec(v: vec.vec3) -> Vec3 => Vec3 { x: v.x, y: v.y, z: v.z }

fn main() {
    let v = to_vec(scale(Vec3 { x: 1.0, y: 2.0, z: 3.0 }, 2.0))
    print(v)
    print(from_vec(v * 0.5 + vec.vec3(1, 1, 1)))
}
"#,
    )
    .unwrap();
    vm.run_main().unwrap();
    assert_eq!(
        out.contents(),
        "vec3(2.0, 4.0, 6.0)\nVec3 { x: 2.0, y: 3.0, z: 4.0 }\n"
    );
}

#[test]
fn a_vector_reaches_the_host_as_a_struct_value() {
    let (mut vm, _) = vm();
    let seen = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let sink = seen.clone();
    // A host function that works on any type: the script's `vec.vec2` arrives as a `Value`.
    vm.register_fn("record", move |v: Value| sink.borrow_mut().push(v))
        .unwrap();
    vm.load_source(
        "game.colibri",
        "import vec\n\nfn main() {\n    let p = vec.vec2(1, 1) + vec.vec2(2, 4) * 0.5\n    record(p)\n    record(vec.ivec2(7, 8))\n}\n",
    )
    .unwrap();
    vm.run_main().unwrap();
    let seen = seen.borrow();
    assert_eq!(f32s(&seen[0]), vec![2.0, 3.0]);
    let ValueKind::Struct { fields, .. } = seen[1].kind() else {
        panic!("not a struct");
    };
    assert!(matches!(fields[1].kind(), ValueKind::Int(8)));
}

#[test]
fn a_script_function_with_vectors_in_its_signature_is_not_a_typed_entry_point() {
    // The type belongs to the script's `std.vec`, so no Rust type describes it: a host gets
    // vectors through a function of its own (above) or a script function of plain numbers.
    let (mut vm, _) = vm();
    vm.load_source(
        "game.colibri",
        "import vec\n\nfn advance(p: vec.vec2, dt: f32) -> vec.vec2 => p * dt\nfn len(x: f32, y: f32) -> f32 => vec.vec2(x, y).len()\nfn main() {}\n",
    )
    .unwrap();
    let err = vm.get_fn::<(Value, f32), Value>("advance").unwrap_err();
    assert!(
        matches!(&err, colibri::Error::SignatureMismatch { name, .. } if name == "advance"),
        "{err:?}"
    );
    let len = vm.get_fn::<(f32, f32), f32>("len").unwrap();
    assert_eq!(len.call(&mut vm, (3.0, 4.0)).unwrap(), 5.0);
}

#[test]
fn install_vec_gives_import_vec_and_math_without_the_rest_of_std() {
    let (mut vm, out) = vm();
    vm.load_source(
        "game.colibri",
        "import vec\n\nfn main() {\n    let u = vec.from_angle(0.0)\n    print(u)\n    print(vec.angle(vec.vec2(0, 2)) > 1.5)\n}\n",
    )
    .unwrap();
    vm.run_main().unwrap();
    assert_eq!(out.contents(), "vec2(1.0, 0.0)\ntrue\n");
    // Without `install_vec` the module does not exist.
    let mut other = colibri::Vm::builder()
        .engine(colibri::EngineKind::Tree)
        .stdout(Box::new(std::io::sink()))
        .build();
    let err = other
        .load_source("game.colibri", "import vec\nfn main() {}\n")
        .unwrap_err();
    let colibri::Error::Compile(report) = &err else {
        panic!("expected a compile error, got {err:?}");
    };
    assert!(
        report
            .diagnostics
            .iter()
            .any(|d| d.message == "no module `vec`"),
        "{:?}",
        report.diagnostics
    );
}

/// An embedder's own host module called `math` does not change what `import math` means inside
/// the library: it imports the standard `math`.
#[test]
fn a_host_module_named_math_does_not_break_import_vec() {
    let out = colibri::SharedBuffer::new();
    let mut vm = colibri::Vm::builder()
        .engine(colibri::EngineKind::Tree)
        .stdout(Box::new(out.clone()))
        .build();
    colibri_std::install(&mut vm).unwrap();
    vm.module("math")
        .register_fn("twice", |x: i64| x * 2)
        .unwrap();
    vm.load_source(
        "g.colibri",
        "import vec\nfn main() {\n    print(vec.from_angle(0.0))\n    print(vec.vec2(1, 2) * 2)\n}\n",
    )
    .unwrap();
    vm.run_main().unwrap();
    assert_eq!(out.contents(), "vec2(1.0, 0.0)\nvec2(2.0, 4.0)\n");
}

#[test]
fn the_interface_files_of_std_do_not_include_the_source_modules_and_the_sources_do() {
    for name in ["vec", "httpd"] {
        assert!(colibri_std::interface(&format!("std.{name}")).is_none());
        assert!(!colibri_std::MODULES.contains(&name));
    }
    let names: Vec<&str> = colibri_std::sources().iter().map(|s| s.name).collect();
    assert_eq!(names, ["vec", "httpd"]);
}

#[test]
fn install_vec_registers_vec_only_not_the_other_source_modules() {
    let (mut vm, _) = vm();
    let error = vm
        .load_source("h.colibri", "import httpd\nfn main() {}\n")
        .expect_err("`httpd` is not part of what install_vec gives")
        .render(false);
    // not "found, but its own imports are missing": the module is not registered at all
    assert!(error.contains("no module `httpd`"), "{error}");
}
