//! One Rust type registered by several modules is one Colibri type that each of them can name;
//! two Rust types of the same shape are two types (EMBEDDING §1).

use colibri::{CheckConfig, Ctx, Interface};
use colibri_runtime as colibri;

fn vm() -> (colibri::Vm, colibri::SharedBuffer) {
    let out = colibri::SharedBuffer::new();
    let vm = colibri::Vm::builder().stdout(Box::new(out.clone())).build();
    (vm, out)
}

fn run(vm: &mut colibri::Vm, out: &colibri::SharedBuffer, src: &str) -> String {
    vm.load_source("m.colibri", src)
        .unwrap_or_else(|e| panic!("{}", e.render(false)));
    vm.run_main()
        .unwrap_or_else(|e| panic!("{}", e.render(false)));
    out.contents()
}

#[colibri::export(value)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pos {
    pub x: i64,
}

mod ui {
    use colibri_runtime as colibri;
    // Another Rust type with the same Colibri shape.
    #[colibri::export(value)]
    #[derive(Clone, Copy, Debug, PartialEq)]
    pub struct Pos {
        pub x: i64,
    }
}

#[test]
fn s1_a_different_rust_type_of_the_same_shape_is_the_modules_own_type() {
    let (mut vm, out) = vm();
    vm.module("physics")
        .register_fn("make", |x: i64| Pos { x })
        .unwrap();
    vm.module("render")
        .register_fn("draw", |p: ui::Pos| p.x * 10)
        .unwrap();
    // A render-only script needs no `import physics` to name the argument.
    let text = run(
        &mut vm,
        &out,
        "import render\nfn main() { print(render.draw(render.Pos { x: 2 })) }\n",
    );
    assert_eq!(text, "20\n");
    let render = vm.module("render").interface_text();
    assert!(render.contains("struct Pos { x: int }"), "{render}");
    assert!(!render.contains("uses"), "{render}");
    // And they are two types: physics's Pos is not render's.
    vm.load_source(
        "n.colibri",
        "import physics\nimport render\nfn main() { render.draw(physics.make(1)) }\n",
    )
    .expect_err("physics.Pos is not render.Pos");
}

fn registered(physics_first: bool) -> (colibri::Vm, colibri::SharedBuffer) {
    let (mut vm, out) = vm();
    if physics_first {
        vm.module("physics")
            .register_fn("make", |x: i64| Pos { x })
            .unwrap();
        vm.module("render")
            .register_fn("draw", |p: Pos| p.x * 10)
            .unwrap();
    } else {
        vm.module("render")
            .register_fn("draw", |p: Pos| p.x * 10)
            .unwrap();
        vm.module("physics")
            .register_fn("make", |x: i64| Pos { x })
            .unwrap();
    }
    (vm, out)
}

const BOTH_NAMES: &str = "import physics\nimport render\nfn a(p: physics.Pos) -> int => render.draw(p)\nfn b(p: render.Pos) -> int => render.draw(p)\nfn main() {\n    print(a(physics.make(1)))\n    print(b(physics.make(2)))\n    print(b(render.Pos { x: 3 }))\n    print(a(physics.Pos { x: 4 }))\n}\n";

#[test]
fn s2_every_module_names_the_type_whichever_registered_first() {
    for physics_first in [true, false] {
        let (mut vm, out) = registered(physics_first);
        let text = run(&mut vm, &out, BOTH_NAMES);
        assert_eq!(text, "10\n20\n30\n40\n", "physics first: {physics_first}");
    }
}

#[test]
fn interfaces_written_in_either_order_check_scripts_written_for_either() {
    let texts = |physics_first: bool| {
        let (mut vm, _) = registered(physics_first);
        (
            vm.module("physics").interface_text(),
            vm.module("render").interface_text(),
        )
    };
    let (pa, ra) = texts(true);
    let (pb, rb) = texts(false);
    assert!(pa.contains("struct Pos { x: int }"), "{pa}");
    assert!(ra.contains("type Pos = physics.Pos"), "{ra}");
    assert!(ra.contains("uses = [\"physics\"]"), "{ra}");
    // Neither module is the default: the owner is the one whose name sorts first, whichever
    // registered first, so both orders give the same text.
    assert_eq!((&pa, &ra), (&pb, &rb));
    for (name, p, r) in [("physics first", &pa, &ra), ("render first", &pb, &rb)] {
        let interfaces: Vec<Interface> = [p, r]
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let parsed = Interface::from_text(&format!("{i}.cbi"), t).unwrap();
                assert_eq!(&parsed.canonical_text(), *t, "{name}");
                parsed
            })
            .collect();
        let config = CheckConfig {
            interfaces,
            ..CheckConfig::default()
        };
        let report = colibri::check_source_with("main.colibri", BOTH_NAMES, &config);
        assert!(!report.has_errors(), "{name}: {}", report.render(false));
    }
}

#[test]
fn the_interfaces_are_written_and_verified_as_files() {
    let dir = std::env::temp_dir().join(format!("colibri-sharing-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Cleanup(dir.clone());
    for (n, physics_first) in [true, false].into_iter().enumerate() {
        let (mut vm, _) = registered(physics_first);
        for module in ["physics", "render"] {
            let file = dir.join(format!("{n}-{module}.cbi"));
            assert!(vm.module(module).write_interface(&file).unwrap());
            vm.module(module).verify_interface(&file).unwrap();
        }
    }
}

#[test]
fn s3_the_default_module_wins_in_any_order() {
    for default_first in [false, true] {
        let (mut vm, out) = vm();
        let register_default = |vm: &mut colibri::Vm| {
            vm.register_fn("dflt", |p: Pos| p.x + 2).unwrap();
        };
        if default_first {
            register_default(&mut vm);
        }
        vm.module("a")
            .register_fn("make", |x: i64| Pos { x })
            .unwrap();
        vm.module("b").register_fn("take", |p: Pos| p.x).unwrap();
        vm.module("c")
            .register_fn("take", |p: Pos| p.x + 1)
            .unwrap();
        if !default_first {
            register_default(&mut vm);
        }
        // One type, with the default module's bare name, and every module's name for it.
        let text = run(
            &mut vm,
            &out,
            "import a\nimport b\nimport c\nfn f(p: Pos) -> int => p.x\nfn g(p: b.Pos) -> int => p.x\nfn main() {\n    let p = a.make(1)\n    let q: Pos = p\n    let r: b.Pos = p\n    let s: c.Pos = p\n    let t: a.Pos = p\n    print(b.take(q) + c.take(r) + dflt(s) + t.x)\n}\n",
        );
        assert_eq!(text, "7\n", "default first: {default_first}");
        let dflt = vm.interface_text();
        assert!(dflt.contains("struct Pos { x: int }"), "{dflt}");
        let a = vm.module("a").interface_text();
        assert!(a.contains("type Pos = Pos"), "{a}");
        assert!(!a.contains("struct Pos"), "{a}");
        // The interface text, read back, checks the same script.
        let config = CheckConfig {
            interfaces: [
                vm.interface_text(),
                vm.module("a").interface_text(),
                vm.module("b").interface_text(),
                vm.module("c").interface_text(),
            ]
            .iter()
            .map(|t| Interface::from_text("x.cbi", t).unwrap())
            .collect(),
            ..CheckConfig::default()
        };
        let src = "import a\nimport b\nimport c\nfn f(p: Pos) -> int => p.x\nfn g(p: b.Pos) -> int => p.x\nfn h(p: a.Pos) -> int => p.x + c.take(p)\n";
        let report = colibri::check_source_with("main.colibri", src, &config);
        assert!(!report.has_errors(), "{}", report.render(false));
        for name in ["f", "g"] {
            let f = vm.get_fn::<(Pos,), i64>(name).unwrap();
            assert_eq!(f.call(&mut vm, (Pos { x: 9 },)).unwrap(), 9);
        }
    }
}

#[test]
fn new_struct_in_accepts_the_owner_or_any_module_that_registered_the_type() {
    let (mut vm, out) = registered(true);
    vm.module("render")
        .register_fn("mk", |cx: &mut Ctx| -> colibri::Value {
            let x = colibri::IntoColibri::into_colibri(7i64, cx);
            cx.new_struct_in("render", "Pos", vec![x])
        })
        .unwrap();
    vm.module("physics")
        .register_fn("mk", |cx: &mut Ctx| -> colibri::Value {
            let x = colibri::IntoColibri::into_colibri(8i64, cx);
            cx.new_struct_in("physics", "Pos", vec![x])
        })
        .unwrap();
    let text = run(
        &mut vm,
        &out,
        "import physics\nimport render\nfn main() {\n    print(render.draw(render.mk()))\n    print(render.draw(physics.mk()))\n}\n",
    );
    assert_eq!(text, "70\n80\n");
}
