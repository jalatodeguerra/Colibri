//! The vector types across the embedding boundary (DESIGN §15.1): `glam` types are the Rust
//! side of `vec.vec2` .. `vec.ivec4`.

use colibri_runtime as colibri;
use colibri_runtime::glam::{IVec2, IVec3, Vec2, Vec3, Vec4};

fn vm() -> (colibri::Vm, colibri::SharedBuffer) {
    let out = colibri::SharedBuffer::new();
    let mut vm = colibri::Vm::builder().stdout(Box::new(out.clone())).build();
    colibri::install_vec(&mut vm).unwrap();
    (vm, out)
}

#[test]
fn host_functions_take_and_return_vectors() {
    let (mut vm, out) = vm();
    vm.register_fn("scale", |v: Vec3, by: f32| v * by).unwrap();
    vm.register_fn("midpoint", |a: Vec2, b: Vec2| (a + b) / 2.0)
        .unwrap();
    vm.register_fn("step", |c: IVec2| c + IVec2::new(1, -1))
        .unwrap();
    vm.register_fn("sum4", |v: Vec4| v.x + v.y + v.z + v.w)
        .unwrap();
    vm.register_fn("cells", |n: i64| {
        (0..n as i32)
            .map(|i| IVec3::new(i, i * 2, 0))
            .collect::<Vec<_>>()
    })
    .unwrap();
    vm.load_source(
        "game.colibri",
        r#"
import vec

fn main() {
    print(scale(vec.vec3(1, 2, 3), 2))
    print(midpoint(vec.vec2(0, 0), vec.vec2(3, 5)))
    print(step(vec.ivec2(1, 1)))
    print(sum4(vec.vec4(1, 2, 3, 4)))
    print(cells(3))
    let v = scale(vec.vec3(1, 1, 1), 0.5)
    print(v.xy + vec.vec2(v.z, v.z))
}
"#,
    )
    .unwrap();
    vm.run_main().unwrap();
    assert_eq!(
        out.contents(),
        "vec3(2.0, 4.0, 6.0)\nvec2(1.5, 2.5)\nivec2(2, 0)\n10.0\n[ivec3(0, 0, 0), ivec3(1, 2, 0), ivec3(2, 4, 0)]\nvec2(1.0, 1.0)\n"
    );
}

#[test]
fn the_interface_text_names_vectors_through_the_module() {
    let (mut vm, _) = vm();
    vm.register_fn("push", |p: Vec3, dir: IVec2| (p, dir))
        .unwrap();
    let text = vm.interface_text();
    assert!(
        text.contains("fn push(a: vec.vec3, b: vec.ivec2) -> (vec.vec3, vec.ivec2)"),
        "{text}"
    );
}

#[test]
fn a_script_function_with_vectors_is_called_from_rust() {
    let (mut vm, _) = vm();
    vm.load_source(
        "game.colibri",
        "import vec\n\nfn advance(p: vec.vec2, v: vec.vec2, dt: f32) -> vec.vec2 => p + v * dt\nfn main() {}\n",
    )
    .unwrap();
    let advance = vm.get_fn::<(Vec2, Vec2, f32), Vec2>("advance").unwrap();
    let p = advance
        .call(&mut vm, (Vec2::new(1.0, 1.0), Vec2::new(2.0, 4.0), 0.5))
        .unwrap();
    assert_eq!(p, Vec2::new(2.0, 3.0));
}

#[test]
fn a_vector_of_the_wrong_length_is_a_registration_mismatch() {
    let (mut vm, _) = vm();
    vm.load_source(
        "game.colibri",
        "import vec\n\nfn f(p: vec.vec3) -> vec.vec3 => p\nfn main() {}\n",
    )
    .unwrap();
    assert!(vm.get_fn::<(Vec2,), Vec2>("f").is_err());
    assert!(vm.get_fn::<(Vec3,), Vec3>("f").is_ok());
}
