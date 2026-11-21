//! Host types are scoped to the module that registers them (EMBEDDING §1): two modules may
//! each declare an `Item`, a module's interface may name another module's type once it says
//! it uses that module, and the interface text of all of it round-trips.

mod common;
use common::vm_pending;

use colibri::{Cx, FromColibri, HostType, Interface, IntoColibri, TypeBuilder, TypeDesc};
use colibri::{Handle, TypeRegistry, Value};
use colibri_runtime as colibri;

fn vm() -> (colibri::Vm, colibri::SharedBuffer) {
    let out = colibri::SharedBuffer::new();
    let vm = colibri::Vm::builder().stdout(Box::new(out.clone())).build();
    (vm, out)
}

fn run(vm: &mut colibri::Vm, source: &str) -> Result<(), colibri::Error> {
    vm.load_source("main.colibri", source)?;
    vm.run_main()
}

// ---- two modules with a class of the same name ----

macro_rules! item_class {
    ($rust:ident, $word:literal) => {
        struct $rust(i64);
        impl HostType for $rust {
            const NAME: &'static str = "Item";
            fn describe(d: &mut TypeBuilder<'_, Self>) {
                d.static_fn("new", ["n"], |n: i64| $rust(n));
                d.method("word", [], |_: &$rust| $word.to_string());
                d.method("n", [], |this: &$rust| this.0);
            }
        }
    };
}

item_class!(AItem, "a");
item_class!(BItem, "b");

fn vm_with_both() -> (colibri::Vm, colibri::SharedBuffer) {
    let (mut vm, out) = vm();
    vm.module("a").register_type::<AItem>().unwrap();
    vm.module("b").register_type::<BItem>().unwrap();
    (vm, out)
}

#[test]
fn two_modules_each_declare_an_item() {
    vm_pending("two_modules_each_declare_an_item", || {
        let (mut vm, out) = vm_with_both();
        // Neither is in scope by its bare name; each is the module's.
        let err = vm
            .load_source("main.colibri", "fn main() {\n    let x: Item = none\n}\n")
            .unwrap_err()
            .render(false);
        assert!(err.contains("unknown type `Item`"), "{err}");
        run(
        &mut vm,
        "import a\nimport b\nfn word(x: a.Item) -> str => x.word()\nfn main() {\n    let x = a.Item.new(1)\n    let y = b.Item.new(2)\n    print(word(x) + y.word() + \"{x.n() + y.n()}\")\n}\n",
    )
    .unwrap();
        assert_eq!(out.contents(), "ab3\n");
        // And they are different types.
        let (mut vm, _) = vm_with_both();
        let err = vm
        .load_source(
            "other.colibri",
            "import a\nimport b\nfn take(x: a.Item) {}\nfn main() {\n    take(b.Item.new(1))\n}\n",
        )
        .unwrap_err()
        .render(false);
        assert!(err.contains("a.Item") && err.contains("b.Item"), "{err}");
    })
}

#[test]
fn imported_names_and_aliases_name_a_module_type_too() {
    vm_pending("imported_names_and_aliases_name_a_module_type_too", || {
        let (mut vm, out) = vm();
        vm.module("a").register_type::<AItem>().unwrap();
        vm.module("b").register_type::<BItem>().unwrap();
        run(
        &mut vm,
        "import a.{Item}\nimport b as other\nfn main() {\n    let x: Item = Item.new(1)\n    let y: other.Item = other.Item.new(2)\n    print(x.word() + y.word())\n}\n",
    )
    .unwrap();
        assert_eq!(out.contents(), "ab\n");
    })
}

#[test]
fn the_interface_text_names_types_by_module_and_round_trips() {
    let (mut vm, _) = vm();
    vm.module("a").register_type::<AItem>().unwrap();
    vm.module("b").register_type::<BItem>().unwrap();
    vm.register_type::<DefaultItem>().unwrap();
    for name in ["a", "b", "host"] {
        let text = if name == "host" {
            vm.interface_text()
        } else {
            vm.module(name).interface_text()
        };
        assert!(text.contains("@host class Item {}"), "{name}: {text}");
        let parsed = Interface::from_text(&format!("{name}.cbi"), &text).unwrap();
        assert_eq!(parsed.canonical_text(), text, "{name}");
    }
    // A program checks against the three files without a `Vm`.
    let interfaces: Vec<Interface> = [
        vm.interface_text(),
        vm.module("a").interface_text(),
        vm.module("b").interface_text(),
    ]
    .iter()
    .enumerate()
    .map(|(i, t)| Interface::from_text(&format!("{i}.cbi"), t).unwrap())
    .collect();
    let config = colibri::CheckConfig {
        interfaces,
        ..colibri::CheckConfig::default()
    };
    let src = "import a\nimport b\nfn main() {\n    let d = Item.new(0)\n    let x: a.Item = a.Item.new(1)\n    let y: b.Item = b.Item.new(2)\n    print(d.word() + x.word() + y.word())\n}\n";
    let report = colibri::check_source_with("main.colibri", src, &config);
    assert!(!report.has_errors(), "{}", report.render(false));
    let bad = colibri::check_source_with(
        "main.colibri",
        "import a\nfn main() {\n    let x: a.Item = Item.new(1)\n}\n",
        &config,
    );
    let text = bad.render(false);
    assert!(text.contains("a.Item"), "{text}");
}

item_class!(DefaultItem, "default");

#[test]
fn a_default_module_type_keeps_its_bare_name_beside_module_types() {
    vm_pending(
        "a_default_module_type_keeps_its_bare_name_beside_module_types",
        || {
            let (mut vm, out) = vm();
            vm.register_type::<DefaultItem>().unwrap();
            vm.module("a").register_type::<AItem>().unwrap();
            run(
                &mut vm,
                "import a\nfn main() {\n    print(Item.new(1).word() + a.Item.new(1).word())\n}\n",
            )
            .unwrap();
            assert_eq!(out.contents(), "defaulta\n");
        },
    )
}

// ---- value structs of the same name ----

#[derive(Debug, Clone, Copy, PartialEq)]
struct PointA(i64);
#[derive(Debug, Clone, Copy, PartialEq)]
struct PointB(f64);

impl IntoColibri for PointA {
    fn ty(reg: &mut TypeRegistry) -> TypeDesc {
        let n = <i64 as IntoColibri>::ty(reg);
        reg.define_struct_of(std::any::TypeId::of::<PointA>(), "Point", vec![("v", n)])
    }
    fn into_colibri(self, cx: &mut Cx<'_>) -> Value {
        let fields = vec![self.0.into_colibri(cx)];
        cx.new_struct("Point", fields)
    }
}
impl FromColibri for PointA {
    fn ty(reg: &mut TypeRegistry) -> TypeDesc {
        <PointA as IntoColibri>::ty(reg)
    }
    fn from_colibri(value: &Value, cx: &mut Cx<'_>) -> Result<Self, colibri::Error> {
        let f = value.field(0).expect("a field");
        Ok(PointA(i64::from_colibri(&f, cx)?))
    }
}
impl IntoColibri for PointB {
    fn ty(reg: &mut TypeRegistry) -> TypeDesc {
        let n = <f64 as IntoColibri>::ty(reg);
        reg.define_struct_of(std::any::TypeId::of::<PointB>(), "Point", vec![("v", n)])
    }
    fn into_colibri(self, cx: &mut Cx<'_>) -> Value {
        let fields = vec![self.0.into_colibri(cx)];
        cx.new_struct("Point", fields)
    }
}

#[test]
fn two_modules_each_declare_a_struct_of_the_same_name() {
    vm_pending("two_modules_each_declare_a_struct_of_the_same_name", || {
        let (mut vm, out) = vm();
        vm.module("a").register_fn("point", || PointA(3)).unwrap();
        vm.module("b").register_fn("point", || PointB(1.5)).unwrap();
        run(
        &mut vm,
        "import a\nimport b\nfn main() {\n    let p = a.point()\n    let q = b.point()\n    print(p.v + 1)\n    print(q.v + 1.0)\n}\n",
    )
    .unwrap();
        assert_eq!(out.contents(), "4\n2.5\n");
        let shown = vm.module("a").interface_text();
        assert!(shown.contains("struct Point { v: int }"), "{shown}");
    })
}

/// A struct described from interface-like text (`define_struct`), with no Rust type behind
/// it: never the same type as another module's of the same name.
macro_rules! gem {
    ($rust:ident) => {
        #[derive(Debug, Clone, Copy, PartialEq)]
        struct $rust(i64);
        impl IntoColibri for $rust {
            fn ty(reg: &mut TypeRegistry) -> TypeDesc {
                let n = <i64 as IntoColibri>::ty(reg);
                reg.define_struct("Gem", vec![("n", n)])
            }
            fn into_colibri(self, cx: &mut Cx<'_>) -> Value {
                let fields = vec![self.0.into_colibri(cx)];
                cx.new_struct("Gem", fields)
            }
        }
        impl FromColibri for $rust {
            fn ty(reg: &mut TypeRegistry) -> TypeDesc {
                <$rust as IntoColibri>::ty(reg)
            }
            fn from_colibri(value: &Value, cx: &mut Cx<'_>) -> Result<Self, colibri::Error> {
                let f = value.field(0).expect("a field");
                Ok($rust(i64::from_colibri(&f, cx)?))
            }
        }
    };
}

gem!(GemA);
gem!(GemB);

#[test]
fn a_bare_struct_name_that_several_modules_declare_is_an_error_naming_them() {
    // Two structs called `Gem`, declared without a Rust type (`define_struct`): each is its
    // module's own type, and the host cannot pick one by its bare name for `get_fn`.
    let (mut vm, _) = vm();
    vm.module("a")
        .register_fn("make", |n: i64| GemA(n))
        .unwrap();
    vm.module("b")
        .register_fn("make", |n: i64| GemB(n))
        .unwrap();
    vm.load_source(
        "main.colibri",
        "import a\nimport b\nfn show(i: a.Gem) -> int => i.n\n",
    )
    .unwrap();
    // Not the same type: `a.Gem` and `b.Gem` stay apart in scripts.
    assert!(
        vm.load_source(
            "other.colibri",
            "import a\nimport b\nfn bad(i: a.Gem) -> b.Gem => i\n"
        )
        .is_err()
    );
    let err = vm
        .get_fn::<(GemA,), i64>("show")
        .expect_err("the bare name is ambiguous");
    let text = err.to_string();
    assert!(text.contains("a, b") && text.contains("`Gem`"), "{text}");
}

// ---- a module that names another module's type ----

/// `json`'s class, which `toml` returns.
struct Val(i64);
impl HostType for Val {
    const NAME: &'static str = "Value";
    fn describe(d: &mut TypeBuilder<'_, Self>) {
        d.method("int", [], |this: &Val| this.0);
    }
}

fn register_json(vm: &mut colibri::Vm) -> Result<(), colibri::Error> {
    let mut m = vm.module("json2");
    m.register_type::<Val>()?;
    m.register_fn_named("parse", ["text"], |text: String| Val(text.len() as i64))
        .map(|_| ())
}

fn register_toml(vm: &mut colibri::Vm) -> Result<(), colibri::Error> {
    let mut m = vm.module("toml2");
    m.uses("json2")?;
    m.register_fn_named(
        "parse",
        ["text"],
        |text: String| Val(text.len() as i64 * 10),
    )
    .map(|_| ())
}

#[test]
fn a_module_returns_the_type_of_another_it_declares_it_uses() {
    vm_pending(
        "a_module_returns_the_type_of_another_it_declares_it_uses",
        || {
            let (mut vm, out) = vm();
            register_json(&mut vm).unwrap();
            register_toml(&mut vm).unwrap();
            let toml = vm.module("toml2").interface_text();
            assert!(toml.contains("uses = [\"json2\"]"), "{toml}");
            assert!(
                toml.contains("fn parse(text: str) -> json2.Value"),
                "{toml}"
            );
            assert!(!toml.contains("@host class"), "{toml}");
            run(
        &mut vm,
        "import json2\nimport toml2\nfn int_of(v: json2.Value) -> int => v.int()\nfn main() {\n    print(int_of(toml2.parse(\"ab\")))\n    print(int_of(json2.parse(\"abc\")))\n    print(toml2.parse(\"abc\").int())\n}\n",
    )
    .unwrap();
            assert_eq!(out.contents(), "20\n3\n30\n");
        },
    )
}

#[test]
fn a_module_that_returns_a_foreign_class_without_uses_is_a_registration_error() {
    let (mut vm, _) = vm();
    register_json(&mut vm).unwrap();
    let err = vm
        .module("toml2")
        .register_fn("parse", |text: String| Val(text.len() as i64))
        .map(|_| ())
        .expect_err("refused");
    let text = err.to_string();
    assert!(matches!(err, colibri::Error::Registration { .. }), "{text}");
    assert!(text.contains("json2.Value"), "{text}");
    assert!(text.contains("does not declare it uses"), "{text}");
    assert!(text.contains("uses(\"json2\")"), "{text}");
    // Nothing of it is left behind: the program loads.
    vm.load_source("main.colibri", "fn main() {}\n").unwrap();
}

#[test]
fn the_default_module_cannot_name_another_modules_type() {
    let (mut vm, _) = vm();
    register_json(&mut vm).unwrap();
    let err = vm
        .register_fn("parse", |text: String| Val(text.len() as i64))
        .map(|_| ())
        .expect_err("refused")
        .to_string();
    assert!(err.contains("default module cannot name"), "{err}");
    let err = vm.module("host").uses("json2").unwrap_err().to_string();
    assert!(err.contains("default module cannot name"), "{err}");
}

#[test]
fn uses_refuses_what_it_cannot_honour() {
    let (mut vm, _) = vm();
    let err = vm.module("toml2").uses("nowhere").unwrap_err().to_string();
    assert!(err.contains("nothing registered"), "{err}");
    register_json(&mut vm).unwrap();
    let err = vm.module("json2").uses("json2").unwrap_err().to_string();
    assert!(err.contains("own types need no declaration"), "{err}");
    let err = vm.module("toml2").uses("host").unwrap_err().to_string();
    assert!(err.contains("need no declaration"), "{err}");
    let err = vm
        .module("toml2")
        .uses("not a name")
        .unwrap_err()
        .to_string();
    assert!(err.contains("not a usable module name"), "{err}");
    // Twice is fine; two modules that read the same are not.
    vm.module("toml2").uses("json2").unwrap();
    vm.module("toml2").uses("json2").unwrap();
    vm.module("other").register_fn("f", || 1i64).unwrap();
    let mut other = vm.module("other");
    other.register_fn("g", || 1i64).unwrap();
    assert_eq!(
        vm.module("toml2").interface_text().matches("uses").count(),
        1
    );
}

#[test]
fn the_interface_with_uses_round_trips_and_needs_the_other_file() {
    let (mut vm, _) = vm();
    register_json(&mut vm).unwrap();
    register_toml(&mut vm).unwrap();
    let json = vm.module("json2").interface_text();
    let toml = vm.module("toml2").interface_text();
    let json_i = Interface::from_text("json2.cbi", &json).unwrap();
    let toml_i = Interface::from_text("toml2.cbi", &toml).unwrap();
    assert_eq!(toml_i.canonical_text(), toml);
    let src = "import toml2\nimport json2\nfn main() {\n    let v: json2.Value = toml2.parse(\"a\")\n    print(v.int())\n}\n";
    let both = colibri::CheckConfig {
        interfaces: vec![json_i, toml_i.clone()],
        ..colibri::CheckConfig::default()
    };
    let report = colibri::check_source_with("main.colibri", src, &both);
    assert!(!report.has_errors(), "{}", report.render(false));
    // The other file missing is said so, not a confusing unknown type.
    let alone = colibri::CheckConfig {
        interfaces: vec![toml_i],
        ..colibri::CheckConfig::default()
    };
    let report = colibri::check_source_with("main.colibri", "import toml2\nfn main() {}\n", &alone);
    let text = report.render(false);
    assert!(text.contains("toml2") && text.contains("json2"), "{text}");
    assert!(report.has_errors(), "{text}");
}

// ---- lazy loading materialises the module that is used first ----

fn register_lazy_json(vm: &mut colibri::Vm) -> Result<(), colibri::Error> {
    let mut m = vm.std_module("ljson");
    m.register_type::<Val>()?;
    Ok(())
}

fn register_lazy_toml(vm: &mut colibri::Vm) -> Result<(), colibri::Error> {
    let mut m = vm.std_module("ltoml");
    m.uses("std.ljson")?;
    m.register_fn_named("parse", ["text"], |text: String| Val(text.len() as i64))
        .map(|_| ())
}

#[test]
fn importing_a_module_registers_the_module_it_uses_first() {
    vm_pending(
        "importing_a_module_registers_the_module_it_uses_first",
        || {
            let (mut vm, out) = vm();
            vm.lazy_std_module("ltoml", register_lazy_toml);
            vm.lazy_std_module("ljson", register_lazy_json);
            // Nothing is registered until something imports it.
            assert!(!vm.module_names().is_empty());
            run(
                &mut vm,
                "import ltoml\nfn main() {\n    print(ltoml.parse(\"abcd\").int())\n}\n",
            )
            .unwrap();
            assert_eq!(out.contents(), "4\n");
            let names = vm.module_names();
            assert!(names.contains(&"std.ljson".to_string()), "{names:?}");
        },
    )
}

#[test]
fn a_used_module_that_fails_to_register_fails_the_module_that_uses_it() {
    fn failing(vm: &mut colibri::Vm) -> Result<(), colibri::Error> {
        vm.std_module("bad").register_fn("f", || 1i64)?;
        Err(colibri::Error::Registration {
            name: "bad".into(),
            message: "it does not work".into(),
        })
    }
    fn user(vm: &mut colibri::Vm) -> Result<(), colibri::Error> {
        let mut m = vm.std_module("usesbad");
        m.uses("std.bad")?;
        m.register_fn("g", || 1i64).map(|_| ())
    }
    let (mut vm, _) = vm();
    vm.lazy_std_module("usesbad", user);
    vm.lazy_std_module("bad", failing);
    let err = vm
        .load_source("main.colibri", "import usesbad\nfn main() {}\n")
        .unwrap_err()
        .to_string();
    assert!(err.contains("std.usesbad"), "{err}");
    assert!(err.contains("it does not work"), "{err}");
}

// ---- a handle to a class of another module ----

struct Holder;
impl HostType for Holder {
    const NAME: &'static str = "Holder";
    fn describe(d: &mut TypeBuilder<'_, Self>) {
        d.method("get", ["h"], |_: &Holder, h: Handle<Val>| h);
    }
}

#[test]
fn a_method_may_take_a_class_of_a_used_module() {
    let (mut vm, _) = vm();
    register_json(&mut vm).unwrap();
    let mut m = vm.module("holder");
    m.uses("json2").unwrap();
    m.register_type::<Holder>().unwrap();
    let text = m.interface_text();
    assert!(
        text.contains("fn get(self, h: json2.Value) -> json2.Value"),
        "{text}"
    );
    // A class belongs to the module that first described it: another module cannot
    // register it too, it can only name it (after `uses`).
    let err = vm
        .module("nouse")
        .register_type::<Holder>()
        .map(|_| ())
        .expect_err("refused")
        .to_string();
    assert!(err.contains("belongs to the module `holder`"), "{err}");
}

// ---- type identity across reloads and sessions ----

#[test]
fn a_module_type_keeps_its_identity_across_a_patch_and_a_restart() {
    vm_pending(
        "a_module_type_keeps_its_identity_across_a_patch_and_a_restart",
        || {
            use colibri::ReloadOutcome;
            let dir = std::env::temp_dir().join(format!("colibri-modtypes-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let file = dir.join("app.colibri");
            let source = |word: &str, extra: &str| {
                format!(
                    "import a\nimport b\nfn make() -> a.Item => a.Item.new(1)\nfn greet() -> str => make().word() + b.Item.new(2).word() + \"{word}\"\n{extra}"
                )
            };
            std::fs::write(&file, source("1", "")).unwrap();
            let (mut vm, _) = vm_with_both();
            vm.load_file(&file).unwrap();
            let greet = vm.get_fn::<(), String>("greet").unwrap();
            assert_eq!(greet.call(&mut vm, ()).unwrap(), "ab1");
            // `make` returns an `a.Item` and a `b.Item` is not one: the signature is what get_fn reads.
            assert!(vm.get_fn::<(), Handle<AItem>>("make").is_ok());
            assert!(vm.get_fn::<(), Handle<BItem>>("make").is_err());
            std::fs::write(&file, source("22", "")).unwrap();
            assert!(matches!(
                vm.reload_changed().unwrap(),
                ReloadOutcome::Patched { .. }
            ));
            assert_eq!(greet.call(&mut vm, ()).unwrap(), "ab22");
            std::fs::write(&file, source("3", "fn more() -> b.Item => b.Item.new(5)\n")).unwrap();
            assert!(matches!(
                vm.reload_changed().unwrap(),
                ReloadOutcome::Restarted { .. }
            ));
            let greet = vm.get_fn::<(), String>("greet").unwrap();
            assert_eq!(greet.call(&mut vm, ()).unwrap(), "ab3");
            assert!(vm.get_fn::<(), Handle<BItem>>("more").is_ok());
            let _ = std::fs::remove_dir_all(&dir);
        },
    )
}

#[test]
fn a_session_keeps_module_values_between_steps() {
    vm_pending("a_session_keeps_module_values_between_steps", || {
        let (mut vm, _) = vm_with_both();
        vm.session_start().unwrap();
        let check = vm
            .session_check("import a\nimport b\nlet x = a.Item.new(1)\nlet y = b.Item.new(2)\n")
            .unwrap();
        vm.session_commit(check, &[], &[]).unwrap();
        // Later steps use the earlier values with the right types.
        let check = vm
        .session_check("import a\nimport b\nlet x = a.Item.new(1)\nlet y = b.Item.new(2)\nprint(x.word() + y.word())\nlet z: a.Item = x\n")
        .unwrap();
        vm.session_commit(check, &[], &[]).unwrap();
        let err = vm
        .session_check("import a\nimport b\nlet x = a.Item.new(1)\nlet y = b.Item.new(2)\nprint(x.word() + y.word())\nlet z: a.Item = x\nlet w: b.Item = x\n")
        .err()
        .expect("a.Item is not b.Item");
        assert!(err.to_string().contains("b.Item"), "{err}");
    })
}

// ---- a named module and the default module's types ----

#[test]
fn a_named_module_may_name_the_default_modules_types_as_before() {
    vm_pending(
        "a_named_module_may_name_the_default_modules_types_as_before",
        || {
            let (mut vm, out) = vm();
            vm.register_type::<DefaultItem>().unwrap();
            vm.register_fn("point", || PointA(1)).unwrap();
            let mut m = vm.module("game");
            // The class and the struct are the default module's: bare in the interface, and the same
            // types in a program.
            m.register_fn_named("twice", ["i"], |i: Handle<DefaultItem>| i)
                .unwrap();
            m.register_fn_named("shift", ["p"], |p: PointA| PointA(p.0 + 10))
                .unwrap();
            let text = m.interface_text();
            assert!(text.contains("fn twice(i: Item) -> Item"), "{text}");
            assert!(text.contains("fn shift(p: Point) -> Point"), "{text}");
            assert!(!text.contains("@host class"), "{text}");
            assert!(!text.contains("struct Point"), "{text}");
            run(
        &mut vm,
        "import game\nfn main() {\n    let i: Item = game.twice(Item.new(4))\n    let p: Point = game.shift(point())\n    print(i.n() + p.v)\n}\n",
    )
    .unwrap();
            assert_eq!(out.contents(), "15\n");
        },
    )
}

// ---- the host calling into a module's types, and one Rust struct in two modules ----

use std::cell::RefCell;

#[colibri::export(value)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Gem {
    pub n: i64,
}

#[colibri::export(value)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pos {
    pub x: i64,
}

#[test]
fn the_host_calls_a_function_that_takes_a_struct_of_a_named_module() {
    vm_pending(
        "the_host_calls_a_function_that_takes_a_struct_of_a_named_module",
        || {
            let (mut vm, out) = vm();
            vm.module("game")
                .register_fn("make", |n: i64| Gem { n })
                .unwrap();
            run(
        &mut vm,
        "import game\nfn show(i: game.Gem) -> int => i.n\nfn main() { print(game.make(3).n) }\n",
    )
    .unwrap();
            assert_eq!(out.contents(), "3\n");
            let show = vm.get_fn::<(Gem,), i64>("show").unwrap();
            assert_eq!(show.call(&mut vm, (Gem { n: 5 },)).unwrap(), 5);
        },
    )
}

thread_local! {
    static STORED: RefCell<Option<colibri::Callback<(Gem,), i64>>> = const { RefCell::new(None) };
}

#[test]
fn the_host_calls_a_stored_callback_that_takes_a_struct_of_a_named_module() {
    vm_pending(
        "the_host_calls_a_stored_callback_that_takes_a_struct_of_a_named_module",
        || {
            let (mut vm, _) = vm();
            let mut game = vm.module("game");
            game.register_fn("make", |n: i64| Gem { n }).unwrap();
            game.register_fn("on", |cb: colibri::Callback<(Gem,), i64>| {
                STORED.with(|c| *c.borrow_mut() = Some(cb));
            })
            .unwrap();
            run(
                &mut vm,
                "import game\nfn main() { game.on(fn(i: game.Gem) -> int => i.n * 2) }\n",
            )
            .unwrap();
            let cb = STORED.with(|c| c.borrow_mut().take()).unwrap();
            assert_eq!(cb.call(&mut vm, (Gem { n: 5 },)).unwrap(), 10);
        },
    )
}

#[test]
fn two_rust_structs_of_one_name_are_told_apart_by_their_rust_type() {
    // Two different structs called `Gem`: each Rust type is the Gem of the module that
    // registered it, so `get_fn` needs no module name to tell them apart.
    mod other {
        use colibri_runtime as colibri;
        #[colibri::export(value)]
        #[derive(Clone, Copy, Debug, PartialEq)]
        pub struct Gem {
            pub n: i64,
            pub weight: f64,
        }
    }
    let (mut vm, _) = vm();
    vm.module("a")
        .register_fn("make", |n: i64| Gem { n })
        .unwrap();
    vm.module("b")
        .register_fn("make", |n: i64| other::Gem { n, weight: 1.0 })
        .unwrap();
    vm.load_source(
        "main.colibri",
        "import a\nimport b\nfn show(i: a.Gem) -> int => i.n\nfn show_b(i: b.Gem) -> int => i.n\n",
    )
    .unwrap();
    let show = vm.get_fn::<(Gem,), i64>("show").unwrap();
    assert_eq!(show.call(&mut vm, (Gem { n: 5 },)).unwrap(), 5);
    let show_b = vm.get_fn::<(other::Gem,), i64>("show_b").unwrap();
    let gem = other::Gem { n: 6, weight: 2.0 };
    assert_eq!(show_b.call(&mut vm, (gem,)).unwrap(), 6);
    // The wrong Rust type for a function is a mismatch naming both.
    let err = vm.get_fn::<(other::Gem,), i64>("show").err().unwrap();
    assert!(err.to_string().contains("a.Gem"), "{err}");
}

#[test]
fn one_rust_struct_registered_by_two_modules_is_one_type() {
    vm_pending(
        "one_rust_struct_registered_by_two_modules_is_one_type",
        || {
            let (mut vm, out) = vm();
            vm.module("physics")
                .register_fn("make", |x: i64| Pos { x })
                .unwrap();
            vm.module("render")
                .register_fn("draw", |p: Pos| p.x * 10)
                .unwrap();
            // The first module to declare it owns it; the other names it `physics.Pos` and says so.
            let physics = vm.module("physics").interface_text();
            let render = vm.module("render").interface_text();
            assert!(physics.contains("struct Pos { x: int }"), "{physics}");
            assert!(!render.contains("struct Pos"), "{render}");
            assert!(
                render.contains("fn draw(a: physics.Pos) -> int"),
                "{render}"
            );
            assert!(render.contains("uses = [\"physics\"]"), "{render}");
            // The files read back, and a program checks against them without a `Vm`.
            let interfaces: Vec<Interface> = [&physics, &render]
                .iter()
                .enumerate()
                .map(|(i, t)| {
                    let parsed = Interface::from_text(&format!("{i}.cbi"), t).unwrap();
                    assert_eq!(&parsed.canonical_text(), *t);
                    parsed
                })
                .collect();
            let config = colibri::CheckConfig {
                interfaces,
                ..colibri::CheckConfig::default()
            };
            let src = "import physics\nimport render\nfn main() { print(render.draw(physics.make(4))) }\n";
            let report = colibri::check_source_with("main.colibri", src, &config);
            assert!(!report.has_errors(), "{}", report.render(false));
            run(&mut vm, src).unwrap();
            assert_eq!(out.contents(), "40\n");
        },
    )
}

#[test]
fn a_struct_that_differs_between_modules_stays_two_types() {
    let (mut vm, _) = vm();
    vm.module("a").register_fn("point", || PointA(1)).unwrap();
    vm.module("b").register_fn("point", || PointB(1.5)).unwrap();
    vm.module("c").register_fn("point", || PointA(2)).unwrap();
    assert!(
        vm.module("b")
            .interface_text()
            .contains("struct Point { v: float }")
    );
    // `c` shares `a`'s identical one.
    assert!(!vm.module("c").interface_text().contains("struct Point"));
}

#[test]
fn modules_that_use_each_other_are_a_cycle_with_its_own_message() {
    fn a(vm: &mut colibri::Vm) -> Result<(), colibri::Error> {
        let mut m = vm.std_module("cyca");
        m.uses("std.cycb")?;
        m.register_fn("f", || 1i64).map(|_| ())
    }
    fn b(vm: &mut colibri::Vm) -> Result<(), colibri::Error> {
        let mut m = vm.std_module("cycb");
        m.uses("std.cyca")?;
        m.register_fn("g", || 1i64).map(|_| ())
    }
    let (mut vm, _) = vm();
    vm.lazy_std_module("cyca", a);
    vm.lazy_std_module("cycb", b);
    let err = vm
        .load_source("main.colibri", "import cyca\nfn main() {}\n")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("std.cyca -> std.cycb -> std.cyca") && err.contains("cycle"),
        "{err}"
    );
    assert!(!err.contains("panicked"), "{err}");
    // A module that uses itself.
    fn me(vm: &mut colibri::Vm) -> Result<(), colibri::Error> {
        vm.std_module("cycme").uses("std.cycme")
    }
    let (mut vm2, _) = self::vm();
    vm2.lazy_std_module("cycme", me);
    let err = vm2
        .load_source("main.colibri", "import cycme\nfn main() {}\n")
        .unwrap_err()
        .to_string();
    assert!(!err.contains("panicked"), "{err}");
}

// ---- deprecated host functions ----

#[test]
fn a_deprecated_host_function_warns_where_it_is_used() {
    vm_pending("a_deprecated_host_function_warns_where_it_is_used", || {
        let (mut vm, out) = vm();
        vm.register_fn("old_add", |a: i64, b: i64| a + b)
            .unwrap()
            .deprecated("use add instead \"now\" {x}");
        vm.register_fn("bare_old", || 1_i64).unwrap().deprecated("");
        vm.register_fn("add", |a: i64, b: i64| a + b).unwrap();
        // The text carries the attribute, and reads back.
        let text = vm.interface_text();
        assert!(
            text.contains("@deprecated(\"use add instead \\\"now\\\" \\{x\\}\")"),
            "{text}"
        );
        assert!(
            text.contains("@deprecated @positional fn bare_old"),
            "{text}"
        );
        let parsed = Interface::from_text("engine.cbi", &text).unwrap();
        assert_eq!(parsed.canonical_text(), text);
        // A program checked against the file (no `Vm`) gets the warning too.
        let config = colibri::CheckConfig {
            interfaces: vec![parsed],
            ..colibri::CheckConfig::default()
        };
        let src = "fn main() {\n    print(old_add(1, 2) + bare_old() + add(1, 1))\n}\n";
        let report = colibri::check_source_with("main.colibri", src, &config);
        assert!(!report.has_errors(), "{}", report.render(false));
        let shown = report.render(false);
        assert!(
            shown.contains("`old_add` is deprecated: use add instead \"now\" {x}"),
            "{shown}"
        );
        assert!(shown.contains("`bare_old` is deprecated"), "{shown}");
        assert!(!shown.contains("`add` is deprecated"), "{shown}");
        // The program still loads and runs; the warnings are the `Vm`'s.
        vm.load_source("main.colibri", src).unwrap();
        assert!(
            vm.render_warnings(false)
                .contains("`old_add` is deprecated")
        );
        vm.run_main().unwrap();
        assert_eq!(out.contents(), "6\n");
    })
}
