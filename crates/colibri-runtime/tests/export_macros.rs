//! `#[colibri::export]` used for real (EMBEDDING §2): a value struct and enum, opaque classes
//! with methods, and free functions with their own parameter names.

use colibri::{Cap, Ctx, Handle, Interface, Ref};
use colibri_runtime as colibri;

#[colibri::export(value)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Vec3 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

#[colibri::export(value)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Material {
    Wood,
    Steel,
}

#[colibri::export]
pub struct RigidBody {
    pos: Vec3,
    mass: f32,
    material: Material,
}

#[colibri::export]
impl RigidBody {
    pub fn new(pos: Vec3, mass: f32, material: Material) -> RigidBody {
        RigidBody {
            pos,
            mass,
            material,
        }
    }

    pub fn mass(&self) -> f32 {
        self.mass
    }

    pub fn material(&self) -> Material {
        self.material
    }

    /// Not `pub`: scripts do not see it.
    #[allow(dead_code)]
    fn secret(&self) -> i64 {
        1
    }
}

#[colibri::export]
pub struct World {
    gravity: Vec3,
    time: f32,
}

#[colibri::export]
impl World {
    pub fn new(gravity: Vec3) -> World {
        World { gravity, time: 0.0 }
    }

    pub fn step(&mut self, dt: f32) {
        self.time += dt;
    }

    pub fn position(&self, body: Ref<RigidBody>) -> Vec3 {
        Vec3 {
            x: body.pos.x + self.gravity.x * self.time,
            y: body.pos.y + self.gravity.y * self.time,
            z: body.pos.z + self.gravity.z * self.time,
        }
    }

    pub fn depth(&self, cx: &mut Ctx) -> u32 {
        cx.host_depth()
    }

    pub fn time(&self) -> f32 {
        self.time
    }

    pub fn find(&self, which: i64) -> Option<Handle<RigidBody>> {
        let _ = which;
        None
    }
}

#[colibri::export]
pub fn spawn_body(x: f32, y: f32, z: f32) -> RigidBody {
    RigidBody::new(Vec3 { x, y, z }, 1.0, Material::Wood)
}

#[colibri::export]
pub fn total_mass(first: Ref<RigidBody>, second: Ref<RigidBody>) -> f32 {
    first.mass + second.mass
}

#[colibri::export]
fn needs_cx(cx: &mut Ctx, n: i64) -> i64 {
    n + i64::from(cx.host_depth())
}

#[colibri::export(no_methods)]
pub struct Tag;

fn engine() -> (colibri::Vm, colibri::SharedBuffer) {
    let out = colibri::SharedBuffer::new();
    let mut vm = colibri::Vm::builder()
        .stdout(Box::new(out.clone()))
        .allow(Cap::Fs)
        .build();
    vm.register_type::<World>().unwrap();
    spawn_body::register(&mut vm).unwrap();
    total_mass::register(&mut vm).unwrap();
    needs_cx::register(&mut vm).unwrap();
    (vm, out)
}

#[test]
fn a_script_uses_exported_classes_value_types_and_functions() {
    let (mut vm, out) = engine();
    vm.load_source(
        "game.colibri",
        r#"
fn main() {
let world = World.new(Vec3 { x: 0.0, y: -10.0, z: 0.0 })
let body = spawn_body(1.0, 2.0, 3.0)
world.step(0.5)
world.step(0.5)
print(world.position(body))
print(world.time())
print(body.mass())
print(body.material())
print(total_mass(body, RigidBody.new(Vec3 { x: 0.0, y: 0.0, z: 0.0 }, 2.5, Material.Steel)))
print(world.depth())
print(needs_cx(40))
print(world.find(3))
}
"#,
    )
    .unwrap();
    vm.run_main().unwrap();
    assert_eq!(
        out.contents(),
        "Vec3 { x: 1.0, y: -8.0, z: 3.0 }\n1.0\n1.0\nWood\n3.5\n1\n41\nnone\n"
    );
}

#[test]
fn parameter_names_are_the_rust_ones_so_arguments_can_be_named() {
    let (mut vm, out) = engine();
    vm.load_source(
        "game.colibri",
        r#"
fn main() {
let body = spawn_body(z: 3.0, x: 1.0, y: 2.0)
let world = World.new(gravity: Vec3 { x: 1.0, y: 1.0, z: 1.0 })
world.step(dt: 2.0)
print(world.position(body: body))
}
"#,
    )
    .unwrap();
    vm.run_main().unwrap();
    assert_eq!(out.contents(), "Vec3 { x: 3.0, y: 4.0, z: 5.0 }\n");
}

#[test]
fn the_interface_text_has_the_class_its_impl_and_real_names() {
    let (vm, _) = engine();
    let text = vm.interface_text();
    for wanted in [
        "struct Vec3 { x: f32, y: f32, z: f32 }\n",
        "enum Material { Wood, Steel }\n",
        "@host class World {}\nimpl World {\n    fn new(gravity: Vec3) -> World\n    fn step(self, dt: f32)\n    fn position(self, body: RigidBody) -> Vec3\n    fn depth(self) -> u32\n    fn time(self) -> f32\n    fn find(self, which: int) -> Option[RigidBody]\n}\n",
        "@host class RigidBody {}\nimpl RigidBody {\n    fn new(pos: Vec3, mass: f32, material: Material) -> RigidBody\n    fn mass(self) -> f32\n    fn material(self) -> Material\n}\n",
        "fn spawn_body(x: f32, y: f32, z: f32) -> RigidBody\n",
        "fn total_mass(first: RigidBody, second: RigidBody) -> f32\n",
        "fn needs_cx(n: int) -> int\n",
    ] {
        assert!(text.contains(wanted), "missing `{wanted}` in:\n{text}");
    }
    assert!(!text.contains("secret"), "{text}");
    assert!(!text.contains("@positional"), "{text}");
}

#[test]
fn a_class_without_methods_can_be_exported_too() {
    let mut vm = colibri::Vm::new();
    vm.register_type::<Tag>().unwrap();
    assert!(vm.interface_text().contains("@host class Tag {}\n"));
    assert!(!vm.interface_text().contains("impl Tag"));
}

#[test]
fn the_text_a_macro_class_prints_checks_a_script_without_a_vm() {
    let (vm, _) = engine();
    let iface = Interface::from_text("engine.cbi", &vm.interface_text()).unwrap();
    assert_eq!(iface.canonical_text(), vm.interface_text());
    let config = colibri::CheckConfig {
        interfaces: vec![iface],
        ..colibri::CheckConfig::default()
    };
    let ok = colibri::check_source_with(
        "a.colibri",
        "fn main() {\n    let w = World.new(Vec3 { x: 0.0, y: 0.0, z: 0.0 })\n    w.step(0.1)\n    print(w.position(spawn_body(1.0, 2.0, 3.0)))\n}\n",
        &config,
    );
    assert!(!ok.has_errors(), "{}", ok.render(false));
    // Without a host object's methods the checker knows what is wrong.
    let bad = colibri::check_source_with(
        "a.colibri",
        "fn main() {\n    let w = World.new(Vec3 { x: 0.0, y: 0.0, z: 0.0 })\n    w.explode()\n    w.step(\"slow\")\n}\n",
        &config,
    );
    let shown = bad.render(false);
    assert!(shown.contains("`World` has no method `explode`"), "{shown}");
    assert!(shown.contains("`str`") || shown.contains("f32"), "{shown}");
}

#[test]
fn value_types_convert_both_ways_through_get_fn() {
    let (mut vm, _) = engine();
    vm.load_source(
        "game.colibri",
        "fn lift(v: Vec3) -> Vec3 => Vec3 { x: v.x, y: v.y + 1.0, z: v.z }\nfn heavy(m: Material) -> bool => m == Material.Steel\n",
    )
    .unwrap();
    let lift = vm.get_fn::<(Vec3,), Vec3>("lift").unwrap();
    assert_eq!(
        lift.call(
            &mut vm,
            (Vec3 {
                x: 1.0,
                y: 2.0,
                z: 3.0
            },)
        )
        .unwrap(),
        Vec3 {
            x: 1.0,
            y: 3.0,
            z: 3.0
        }
    );
    let heavy = vm.get_fn::<(Material,), bool>("heavy").unwrap();
    assert!(heavy.call(&mut vm, (Material::Steel,)).unwrap());
    assert!(!heavy.call(&mut vm, (Material::Wood,)).unwrap());
}

#[test]
fn a_host_class_is_passed_by_handle_to_get_fn() {
    let (mut vm, _) = engine();
    let body = vm.insert(RigidBody::new(
        Vec3 {
            x: 0.0,
            y: 0.0,
            z: 0.0,
        },
        4.0,
        Material::Steel,
    ));
    vm.load_source(
        "game.colibri",
        "fn weight(b: RigidBody) -> f32 => b.mass()\n",
    )
    .unwrap();
    let weight = vm.get_fn::<(Handle<RigidBody>,), f32>("weight").unwrap();
    assert_eq!(weight.call(&mut vm, (body,)).unwrap(), 4.0);
    vm.remove(body).unwrap();
    let err = weight.call(&mut vm, (body,)).unwrap_err();
    assert!(
        err.to_string()
            .contains("`RigidBody` was destroyed by the host"),
        "{err}"
    );
}
