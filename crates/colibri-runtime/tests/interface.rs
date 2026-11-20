//! Interface files (`.cbi`, EMBEDDING §3): printing, parsing, hashing, verifying, and value types.

use colibri::{
    Cap, Cx, FromColibri, Handle, HostType, Interface, IntoColibri, TypeBuilder, TypeDesc,
    TypeRegistry, Value,
};
use colibri_runtime as colibri;

#[derive(Debug, PartialEq, Clone, Copy)]
struct Vec3 {
    x: f32,
    y: f32,
    z: f32,
}

impl IntoColibri for Vec3 {
    fn ty(reg: &mut TypeRegistry) -> TypeDesc {
        let f = <f32 as IntoColibri>::ty(reg);
        reg.define_struct("Vec3", vec![("x", f.clone()), ("y", f.clone()), ("z", f)])
    }
    fn into_colibri(self, cx: &mut Cx<'_>) -> Value {
        let fields = vec![
            self.x.into_colibri(cx),
            self.y.into_colibri(cx),
            self.z.into_colibri(cx),
        ];
        cx.new_struct("Vec3", fields)
    }
}

fn field(value: &Value, i: usize) -> Value {
    value.field(i).expect("a Vec3 has three fields")
}

impl FromColibri for Vec3 {
    fn ty(reg: &mut TypeRegistry) -> TypeDesc {
        <Vec3 as IntoColibri>::ty(reg)
    }
    fn from_colibri(value: &Value, cx: &mut Cx<'_>) -> Result<Self, colibri::Error> {
        Ok(Vec3 {
            x: f32::from_colibri(&field(value, 0), cx)?,
            y: f32::from_colibri(&field(value, 1), cx)?,
            z: f32::from_colibri(&field(value, 2), cx)?,
        })
    }
}

#[derive(Debug, PartialEq, Clone, Copy)]
enum Dir {
    North,
    South,
}

impl IntoColibri for Dir {
    fn ty(reg: &mut TypeRegistry) -> TypeDesc {
        reg.define_enum("Dir", &["North", "South"])
    }
    fn into_colibri(self, cx: &mut Cx<'_>) -> Value {
        cx.new_variant("Dir", self as u32)
    }
}

impl FromColibri for Dir {
    fn ty(reg: &mut TypeRegistry) -> TypeDesc {
        <Dir as IntoColibri>::ty(reg)
    }
    fn from_colibri(value: &Value, _: &mut Cx<'_>) -> Result<Self, colibri::Error> {
        Ok(if value.variant_index() == Some(0) {
            Dir::North
        } else {
            Dir::South
        })
    }
}

/// A host class with a static function, a reading method and a writing one.
struct Entity {
    hp: i64,
}

impl HostType for Entity {
    const NAME: &'static str = "Entity";
    fn describe(d: &mut TypeBuilder<'_, Self>) {
        d.static_fn("spawn", ["hp"], |hp: i64| Entity { hp });
        d.method("hp", [], |this: &Entity| this.hp);
        d.method_mut("heal", ["by"], |this: &mut Entity, by: i64| this.hp += by);
    }
}

fn engine() -> colibri::Vm {
    let mut vm = colibri::Vm::builder()
        .stdout(Box::new(colibri::SharedBuffer::new()))
        .allow(Cap::Fs)
        .build();
    vm.register_fn("move_by", |p: Vec3, d: Dir| match d {
        Dir::North => Vec3 { z: p.z + 1.0, ..p },
        Dir::South => Vec3 { z: p.z - 1.0, ..p },
    })
    .unwrap();
    vm.register_fn("add", |a: i64, b: i64| a + b).unwrap();
    vm.register_fn("find_entity", |_id: i64| -> Option<Handle<Entity>> { None })
        .unwrap();
    vm.register_fn("read_save", |slot: i64| -> Result<String, String> {
        Ok(format!("slot {slot}"))
    })
    .unwrap()
    .requires(Cap::Fs);
    vm
}

fn temp(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("colibri-iface-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

#[test]
fn the_text_has_a_header_and_every_signature() {
    let text = engine().interface_text();
    let first = text.lines().nth(1).unwrap();
    assert!(first.starts_with("@interface("), "{text}");
    assert!(first.contains("format"), "{text}");
    assert!(first.contains("caps"), "{text}");
    assert!(text.contains("struct Vec3"), "{text}");
    assert!(text.contains("enum Dir"), "{text}");
    assert!(text.contains("fn add("), "{text}");
    assert!(text.contains("@requires(fs)"), "{text}");
    assert!(text.contains("fn read_save("), "{text}");
    // A host class is declared where it was first described, with its methods right after.
    assert!(
        text.contains(
            "@host class Entity {}\nimpl Entity {\n    fn spawn(hp: int) -> Entity\n    fn hp(self) -> int\n    fn heal(self, by: int)\n}\n"
        ),
        "{text}"
    );
    assert!(
        text.contains("@positional fn find_entity(a: int) -> Option[Entity]"),
        "{text}"
    );
}

#[test]
fn print_parse_print_is_identical_and_the_hash_is_stable() {
    let vm = engine();
    let text = vm.interface_text();
    let parsed = Interface::from_text("engine.cbi", &text).unwrap();
    assert_eq!(parsed.canonical_text(), text);
    assert_eq!(parsed.hash(), vm.interface_hash());
    let again = Interface::from_text("engine.cbi", &parsed.canonical_text()).unwrap();
    assert_eq!(again.canonical_text(), text);
    assert_eq!(engine().interface_hash(), vm.interface_hash());
}

#[test]
fn registration_order_does_not_change_the_hash_of_the_same_set() {
    let a = engine().interface_hash();
    let mut vm = colibri::Vm::builder().build();
    vm.register_fn("add", |a: i64, b: i64| a + b).unwrap();
    assert_ne!(vm.interface_hash(), a);
}

#[test]
fn write_interface_writes_only_when_the_text_changed() {
    let path = temp("write.cbi");
    let _ = std::fs::remove_file(&path);
    let mut vm = engine();
    assert!(vm.write_interface(&path).unwrap());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), vm.interface_text());
    assert!(!vm.write_interface(&path).unwrap());
    vm.register_fn("sub", |a: i64, b: i64| a - b).unwrap();
    assert!(vm.write_interface(&path).unwrap());
    assert!(!vm.write_interface(&path).unwrap());
}

#[test]
fn verify_interface_accepts_the_current_file_and_reports_a_stale_one() {
    let path = temp("verify.cbi");
    let mut vm = engine();
    vm.write_interface(&path).unwrap();
    vm.verify_interface(&path).unwrap();
    vm.register_fn("sub", |a: i64, b: i64| a - b).unwrap();
    match vm.verify_interface(&path) {
        Err(colibri::Error::InterfaceMismatch {
            expected, found, ..
        }) => {
            assert_eq!(expected, vm.interface_hash());
            assert_ne!(Some(expected), found);
        }
        other => panic!("expected a mismatch, got {other:?}"),
    }
}

#[test]
fn verify_interface_reports_a_missing_file() {
    let vm = engine();
    let err = vm.verify_interface(temp("nowhere.cbi")).unwrap_err();
    assert!(matches!(err, colibri::Error::Io { .. }), "{err:?}");
}

#[test]
fn a_hand_edited_file_is_refused() {
    let text = engine().interface_text().replace("fn add(", "fn plus(");
    let err = Interface::from_text("engine.cbi", &text).unwrap_err();
    assert!(matches!(err, colibri::Error::Interface { .. }), "{err:?}");
    // The file name and the reason are two words, not one run together.
    assert!(err.to_string().starts_with("engine.cbi: "), "{err}");
}

#[test]
fn a_script_checks_against_a_parsed_interface() {
    let text = engine().interface_text();
    let iface = Interface::from_text("engine.cbi", &text).unwrap();
    let config = colibri::CheckConfig {
        interfaces: vec![iface],
        caps: colibri::CapSet::all(),
        ..colibri::CheckConfig::default()
    };
    let ok = colibri::check_source_with("a.colibri", "fn main() { print(add(1, 2)) }\n", &config);
    assert!(ok.diagnostics.is_empty(), "{ok:?}");
    let bad = colibri::check_source_with("a.colibri", "fn main() { print(add(1)) }\n", &config);
    assert!(!bad.diagnostics.is_empty());
}

#[test]
fn value_types_cross_the_boundary() {
    let out = colibri::SharedBuffer::new();
    let mut vm = colibri::Vm::builder().stdout(Box::new(out.clone())).build();
    vm.register_fn("move_by", |p: Vec3, d: Dir| match d {
        Dir::North => Vec3 { z: p.z + 1.0, ..p },
        Dir::South => Vec3 { z: p.z - 1.0, ..p },
    })
    .unwrap();
    vm.load_source(
        "a.colibri",
        "fn main() {\n    let p = move_by(Vec3 { x: 1.0, y: 2.0, z: 3.0 }, Dir.North)\n    print(p.z)\n}\n",
    )
    .unwrap();
    vm.run_main().unwrap();
    assert_eq!(out.contents(), "4.0\n");
}

#[test]
fn get_fn_carries_value_types() {
    let mut vm = colibri::Vm::builder().build();
    vm.register_type::<Vec3>().unwrap();
    vm.register_type::<Dir>().unwrap();
    vm.load_source(
        "a.colibri",
        "fn lift(p: Vec3) -> Vec3 {\n    return Vec3 { x: p.x, y: p.y + 1.0, z: p.z }\n}\nfn flip(d: Dir) -> Dir {\n    if d == Dir.North {\n        return Dir.South\n    }\n    return Dir.North\n}\nfn main() {}\n",
    )
    .unwrap();
    let lift = vm.get_fn::<(Vec3,), Vec3>("lift").unwrap();
    let p = lift
        .call(
            &mut vm,
            (Vec3 {
                x: 1.0,
                y: 2.0,
                z: 3.0,
            },),
        )
        .unwrap();
    assert_eq!(
        p,
        Vec3 {
            x: 1.0,
            y: 3.0,
            z: 3.0
        }
    );
    let flip = vm.get_fn::<(Dir,), Dir>("flip").unwrap();
    assert_eq!(flip.call(&mut vm, (Dir::North,)).unwrap(), Dir::South);
}

#[test]
fn a_host_class_text_parses_prints_and_checks_a_script() {
    let body = "@host class Entity {}\nimpl Entity {\n    fn spawn(hp: int) -> Entity\n    fn hp(self) -> int\n    fn heal(self, by: int)\n}\n";
    let hash = colibri_check::host::body_hash(body);
    let text = format!(
        "// Generated by colibri-runtime; do not edit.\n{}\n{body}",
        colibri_check::host::header_line("host", &hash, &[])
    );
    let iface = Interface::from_text("engine.cbi", &text).unwrap();
    assert_eq!(iface.canonical_text(), text);
    let config = colibri::CheckConfig {
        interfaces: vec![iface],
        ..colibri::CheckConfig::default()
    };
    let ok = colibri::check_source_with(
        "a.colibri",
        "fn main() {\n    let e = Entity.spawn(10)\n    e.heal(by: 5)\n    print(e.hp() + 1)\n    print(e.alive())\n}\n",
        &config,
    );
    assert!(!ok.has_errors(), "{}", ok.render(false));
    let shown = |src: &str| colibri::check_source_with("a.colibri", src, &config).render(false);
    assert!(shown("fn main() {\n    Entity.spawn(\"x\")\n}\n").contains("expected"));
    assert!(
        shown("fn main() {\n    let e = Entity.spawn(1)\n    e.hp(2)\n}\n").contains("argument")
    );
    assert!(
        shown("fn main() {\n    let e = Entity.spawn(1)\n    let c = e.clone()\n}\n")
            .contains("`Entity` is a host class and cannot be cloned")
    );
}

#[test]
fn bad_host_class_text_is_refused_with_a_position() {
    let refuse = |body: &str| {
        let hash = colibri_check::host::body_hash(body);
        let text = format!(
            "// Generated by colibri-runtime; do not edit.\n{}\n{body}",
            colibri_check::host::header_line("host", &hash, &[])
        );
        match Interface::from_text("engine.cbi", &text) {
            Err(colibri::Error::Interface { message, .. }) => message,
            other => panic!("expected a refusal for {body:?}, got {other:?}"),
        }
    };
    assert!(refuse("@host class E { hp: int }\n").contains("no fields"));
    assert!(refuse("impl E {\n    fn f(self)\n}\n").contains("belongs to a `@host class`"));
    assert!(refuse("@host class E {}\nimpl E {\n    fn f(var self)\n}\n").contains("plain `self`"));
    assert!(
        refuse("@host class E {}\nimpl E {\n    fn alive(self) -> bool\n}\n")
            .contains("built into every host class")
    );
    assert!(refuse("class E {}\n").contains("must be a `@host class`"));
}

#[test]
fn the_hash_follows_every_method_of_a_host_class() {
    struct Plain;
    impl HostType for Plain {
        const NAME: &'static str = "Plain";
        fn describe(d: &mut TypeBuilder<'_, Self>) {
            d.method("a", [], |_: &Plain| 1i64);
        }
    }
    struct Plainer;
    impl HostType for Plainer {
        const NAME: &'static str = "Plain";
        fn describe(d: &mut TypeBuilder<'_, Self>) {
            d.method("a", [], |_: &Plainer| 1i64);
            d.method("b", [], |_: &Plainer| 2i64);
        }
    }
    let hash_of = |register: fn(&mut colibri::Vm)| {
        let mut vm = colibri::Vm::new();
        register(&mut vm);
        vm.interface_hash()
    };
    let one = hash_of(|vm| vm.register_type::<Plain>().unwrap());
    let same = hash_of(|vm| vm.register_type::<Plain>().unwrap());
    let two = hash_of(|vm| vm.register_type::<Plainer>().unwrap());
    assert_eq!(one, same);
    assert_ne!(one, two);
}

#[test]
fn a_registered_class_text_round_trips_through_the_checker_exactly() {
    // print -> parse -> print is identical, also with several classes that mention each other.
    struct A;
    struct B;
    impl HostType for A {
        const NAME: &'static str = "A";
        fn describe(d: &mut TypeBuilder<'_, Self>) {
            d.static_fn("make", ["b"], |_: Handle<B>| A);
            d.method("peer", [], |_: &A| -> Option<Handle<B>> { None });
        }
    }
    impl HostType for B {
        const NAME: &'static str = "B";
        fn describe(d: &mut TypeBuilder<'_, Self>) {
            d.method("mate", ["a"], |_: &B, a: colibri::Ref<A>| {
                let _ = a;
            });
        }
    }
    let mut vm = colibri::Vm::new();
    vm.register_type::<A>().unwrap();
    let text = vm.interface_text();
    // `A` mentions `B` while it is described, so both are declared, `A` first.
    let a = text.find("@host class A {}").expect("A is declared");
    let b = text.find("@host class B {}").expect("B is declared");
    assert!(a < b, "{text}");
    let parsed = Interface::from_text("engine.cbi", &text).unwrap();
    assert_eq!(parsed.canonical_text(), text);
    // The cycle loads and runs.
    vm.load_source("a.colibri", "fn main() {\n}\n").unwrap();
    vm.run_main().unwrap();
}

#[test]
fn a_function_that_never_returns_is_declared_with_a_bang() {
    let mut vm = colibri::Vm::builder().build();
    vm.register_fn(
        "quit",
        |cx: &mut colibri::Ctx, code: i64| -> colibri::Never { cx.raise(format!("quit {code}")) },
    )
    .unwrap();
    let text = vm.interface_text();
    assert!(text.contains("fn quit(a: int) -> !"), "{text}");
    let parsed = Interface::from_text("engine.cbi", &text).unwrap();
    assert_eq!(parsed.canonical_text(), text);
    // The script needs no value after the call.
    let mut vm = colibri::Vm::builder().build();
    vm.register_fn(
        "quit",
        |cx: &mut colibri::Ctx, code: i64| -> colibri::Never { cx.raise(format!("quit {code}")) },
    )
    .unwrap();
    vm.load_source("t.colibri", "fn f() -> int { quit(1) }\nfn main() { f() }")
        .expect("quit diverges");
}
