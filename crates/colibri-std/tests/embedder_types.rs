//! The types of a host module are that module's own: an embedder's `Child`, `Response` or
//! `Stat` (in the default module or in a module of its own) coexists with the standard
//! library's, whichever is registered first.

use colibri_runtime::{self as colibri, Cap, HostType, TypeBuilder};

macro_rules! embedder_class {
    ($rust:ident, $name:literal, $word:literal) => {
        struct $rust(i64);
        impl HostType for $rust {
            const NAME: &'static str = $name;
            fn describe(d: &mut TypeBuilder<'_, Self>) {
                d.static_fn("new", ["n"], |n: i64| $rust(n));
                d.method("word", [], |_this: &$rust| $word.to_string());
                d.method("n", [], |this: &$rust| this.0);
            }
        }
    };
}

embedder_class!(MyChild, "Child", "mine-child");
embedder_class!(MyResponse, "Response", "mine-response");
embedder_class!(MyStat, "Stat", "mine-stat");
embedder_class!(GameChild, "Child", "game-child");
embedder_class!(GameStat, "Stat", "game-stat");

fn vm() -> (colibri::Vm, colibri::SharedBuffer) {
    let out = colibri::SharedBuffer::new();
    // Pinned to the tree-walker: this tests the standard library, not the engine, and host
    // functions do not run on the bytecode VM yet (`COLIBRI_ENGINE=vm`). Unpin with step 2c.
    let vm = colibri::Vm::builder()
        // vm-pin: 2c
        .engine(colibri::EngineKind::Tree)
        .stdout(Box::new(out.clone()))
        .allow(Cap::Os)
        .allow(Cap::Fs)
        .allow(Cap::Net)
        .build();
    (vm, out)
}

/// What a program that uses both the embedder's types and the library's does.
const PROGRAM: &str = r#"
import process
import fs
import http
import game

fn mine(c: Child, s: Stat, r: Response) -> str {
    return "{c.word()} {s.word()} {r.word()} {c.n()}"
}

fn theirs(c: game.Child, s: game.Stat) -> str {
    return "{c.word()} {s.word()}"
}

fn std_side(c: process.Child?, s: fs.Stat?, r: http.Response?) -> bool {
    return c == none && s == none && r == none
}

fn main() {
    print(mine(Child.new(7), Stat.new(1), Response.new(2)))
    print(theirs(game.Child.new(3), game.Stat.new(4)))
    print(std_side(none, none, none))
    let st = fs.stat(".").expect("stat of the working folder")
    print(st.kind == "dir")
    let out = process.output("echo", ["hi"]).expect("echo")
    print(out.trim())
}
"#;

fn register_embedder(vm: &mut colibri::Vm) {
    vm.register_type::<MyChild>().unwrap();
    vm.register_type::<MyResponse>().unwrap();
    vm.register_type::<MyStat>().unwrap();
    let mut game = vm.module("game");
    game.register_type::<GameChild>().unwrap();
    game.register_type::<GameStat>().unwrap();
}

fn check(register_first: bool) {
    let (mut vm, out) = vm();
    if register_first {
        register_embedder(&mut vm);
        colibri_std::install(&mut vm).unwrap();
    } else {
        colibri_std::install(&mut vm).unwrap();
        register_embedder(&mut vm);
    }
    vm.load_source("app/main.colibri", PROGRAM)
        .unwrap_or_else(|e| panic!("{}", e.render(false)));
    vm.run_main().unwrap();
    assert_eq!(
        out.contents(),
        "mine-child mine-stat mine-response 7\ngame-child game-stat\ntrue\ntrue\nhi\n"
    );
}

#[test]
fn embedder_types_registered_before_the_library_coexist_with_its_own() {
    check(true);
}

#[test]
fn embedder_types_registered_after_the_library_coexist_with_its_own() {
    check(false);
}

#[test]
fn a_type_is_named_by_its_module_in_messages() {
    let (mut vm, _) = vm();
    colibri_std::install(&mut vm).unwrap();
    vm.register_type::<MyChild>().unwrap();
    vm.module("game").register_type::<GameChild>().unwrap();
    let err = vm
        .load_source(
            "app/main.colibri",
            "import process\nimport game\nfn f(c: process.Child) {}\nfn main() {\n    let c: game.Child = game.Child.new(1)\n    f(c)\n}\n",
        )
        .expect_err("a game.Child is not a process.Child")
        .render(false);
    assert!(err.contains("game.Child"), "{err}");
    assert!(err.contains("process.Child"), "{err}");
}

/// An embedder's struct that is field for field the library's `http.Response`.
struct MyShape {
    status: i64,
}

impl colibri::IntoColibri for MyShape {
    fn ty(reg: &mut colibri::TypeRegistry) -> colibri::TypeDesc {
        let status = <i64 as colibri::IntoColibri>::ty(reg);
        let headers = <std::collections::BTreeMap<String, String> as colibri::IntoColibri>::ty(reg);
        let body = <String as colibri::IntoColibri>::ty(reg);
        reg.define_struct(
            "Response",
            vec![("status", status), ("headers", headers), ("body", body)],
        )
    }
    fn into_colibri(self, cx: &mut colibri::Cx<'_>) -> colibri::Value {
        let fields = vec![
            self.status.into_colibri(cx),
            std::collections::BTreeMap::<String, String>::new().into_colibri(cx),
            String::from("mine").into_colibri(cx),
        ];
        cx.new_struct("Response", fields)
    }
}

#[test]
fn an_embedder_struct_identical_to_a_std_struct_is_still_its_own() {
    let (mut vm, out) = vm();
    colibri_std::install(&mut vm).unwrap();
    vm.register_fn("mine", || MyShape { status: 201 }).unwrap();
    vm.load_source(
        "app/main.colibri",
        "import http\nfn show(r: http.Response) -> str => \"std {r.status}\"\nfn main() {\n    let r: Response = mine()\n    print(\"{r.status} {r.body}\")\n}\n",
    )
    .unwrap_or_else(|e| panic!("{}", e.render(false)));
    vm.run_main().unwrap();
    assert_eq!(out.contents(), "201 mine\n");
    let (mut vm, _) = self::vm();
    colibri_std::install(&mut vm).unwrap();
    vm.register_fn("mine", || MyShape { status: 1 }).unwrap();
    let err = vm
        .load_source(
            "app/main.colibri",
            "import http\nfn show(r: http.Response) {}\nfn main() {\n    show(mine())\n}\n",
        )
        .expect_err("a Response is not an http.Response")
        .render(false);
    assert!(err.contains("http.Response"), "{err}");
}

embedder_class!(MyValue, "Value", "mine-value");
embedder_class!(MyItem, "Item", "mine-item");
embedder_class!(GameValue, "Value", "game-value");

/// `json.Value` and the embedder's own `Value` (and an `Item`, the name `toml` once gave its node) are
/// different types, whichever is registered first, in the default module or a named one.
const VALUE_PROGRAM: &str = r#"
import json
import toml
import game

struct Doc { raw: json.Value, n: int }

fn mine(v: Value, i: Item) -> str {
    return "{v.word()} {i.word()} {v.n()}"
}

fn main() {
    print(mine(Value.new(5), Item.new(6)))
    print(game.Value.new(7).word())
    let j = json.parse("[1, 2]").expect("json")
    print(j.kind())
    let t = toml.parse("a = 1").expect("toml")
    print(t.kind())
    let d = json.decode[Doc](r"""{"raw": {"k": [true]}, "n": 3}""").expect("doc")
    print(d.raw.get("k").expect("k").kind())
    print(json.encode(d).expect("encode"))
}
"#;

fn check_values(register_first: bool) {
    let (mut vm, out) = vm();
    let register = |vm: &mut colibri::Vm| {
        vm.register_type::<MyValue>().unwrap();
        vm.register_type::<MyItem>().unwrap();
        vm.module("game").register_type::<GameValue>().unwrap();
    };
    if register_first {
        register(&mut vm);
        colibri_std::install(&mut vm).unwrap();
    } else {
        colibri_std::install(&mut vm).unwrap();
        register(&mut vm);
    }
    vm.load_source("app/main.colibri", VALUE_PROGRAM)
        .unwrap_or_else(|e| panic!("{}", e.render(false)));
    vm.run_main().unwrap();
    assert_eq!(
        out.contents(),
        "mine-value mine-item 5\ngame-value\nlist\nmap\nlist\n{\"raw\":{\"k\":[true]},\"n\":3}\n"
    );
}

#[test]
fn an_embedder_value_and_item_registered_before_the_library_coexist_with_json_value() {
    check_values(true);
}

#[test]
fn an_embedder_value_and_item_registered_after_the_library_coexist_with_json_value() {
    check_values(false);
}

#[test]
fn json_value_is_named_by_its_module_in_messages() {
    let (mut vm, _) = vm();
    colibri_std::install(&mut vm).unwrap();
    vm.register_type::<MyValue>().unwrap();
    let err = vm
        .load_source(
            "app/main.colibri",
            "import json\nfn f(v: json.Value) {}\nfn main() {\n    f(Value.new(1))\n}\n",
        )
        .expect_err("a Value is not a json.Value")
        .render(false);
    assert!(err.contains("json.Value"), "{err}");
}

#[test]
fn json_decode_does_not_mistake_an_embedder_value_for_json_value() {
    let (mut vm, out) = vm();
    colibri_std::install(&mut vm).unwrap();
    vm.register_type::<MyValue>().unwrap();
    vm.load_source(
        "app/main.colibri",
        "import json\nstruct Doc { v: Value }\nfn main() {\n    match json.decode[Doc](r\"\"\"{\"v\": 1}\"\"\") {\n        ok(_) => print(\"decoded\"),\n        err(e) => print(e),\n    }\n}\n",
    )
    .unwrap_or_else(|e| panic!("{}", e.render(false)));
    vm.run_main().unwrap();
    let text = out.contents();
    assert!(
        text.contains("`Value` is a class") && !text.contains("decoded"),
        "{text}"
    );
}
