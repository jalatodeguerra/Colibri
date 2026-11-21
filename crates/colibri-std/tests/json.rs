//! `json` through the runtime API: field attributes, enums with data, the untyped `json.Value`.
//! The golden tests in `tests/golden/std` cover the same ground from the language side.

use colibri_runtime as colibri;

fn run(source: &str) -> String {
    let out = colibri::SharedBuffer::new();
    // Pinned to the tree-walker: this tests the standard library, not the engine, and host
    // functions do not run on the bytecode VM yet (`COLIBRI_ENGINE=vm`). Unpin with step 2c.
    let mut vm = colibri::Vm::builder()
        // vm-pin: 2c
        .engine(colibri::EngineKind::Tree)
        .stdout(Box::new(out.clone()))
        .build();
    colibri_std::install(&mut vm).expect("installs");
    vm.load_source("app/main.colibri", source)
        .unwrap_or_else(|e| panic!("does not compile: {}", e.render(false)));
    vm.run_main().expect("runs");
    out.contents()
}

fn compile_error(source: &str) -> String {
    let mut vm = colibri::Vm::builder()
        // vm-pin: 2c
        .engine(colibri::EngineKind::Tree)
        .stdout(Box::new(std::io::sink()))
        .build();
    colibri_std::install(&mut vm).expect("installs");
    vm.load_source("app/main.colibri", source)
        .expect_err("should not compile")
        .render(false)
}

#[test]
fn rename_changes_the_key_both_ways() {
    let out = run(r#"import json
struct P { @rename("full-name") name: str, age: int }
fn main() {
    print(json.encode(P { name: "Ann", age: 3 }).expect("e"))
    print(json.decode[P](r"""{"full-name": "Bo", "age": 4}"""))
    print(json.decode[P](r"""{"name": "Bo", "age": 4}"""))
}
"#);
    assert_eq!(
        out,
        "{\"full-name\":\"Ann\",\"age\":3}\n\
         ok(P { name: \"Bo\", age: 4 })\n\
         err(json: at `$`: the object has no field `full-name` (a `P` needs it))\n"
    );
}

#[test]
fn skip_is_not_written_and_not_read() {
    let out = run(r#"import json
struct P { a: int, @skip b: int = 5, @skip c: str? }
fn main() {
    print(json.encode(P { a: 1, b: 2, c: some("x") }).expect("e"))
    print(json.decode[P](r"""{"a": 1, "b": 9, "c": "y"}"""))
}
"#);
    assert_eq!(out, "{\"a\":1}\nok(P { a: 1, b: 5, c: none })\n");
}

#[test]
fn field_attribute_mistakes_are_compile_errors() {
    let e = compile_error("struct P { @rename(\"k\") a: int, k: int }\nfn main() {}\n");
    assert!(e.contains("both serialize as `k`"), "{e}");
    let e = compile_error("struct P { @skip a: int }\nfn main() {}\n");
    assert!(e.contains("needs a default value"), "{e}");
    let e = compile_error("struct P { @rename a: int }\nfn main() {}\n");
    assert!(e.contains("takes the new name as a string"), "{e}");
    // A skipped field gives its key up.
    run("struct P { @skip a: int = 0, @rename(\"a\") b: int }\nfn main() {}\n");
}

#[test]
fn enums_with_data_use_one_key_objects() {
    let out = run(r#"import json
enum E { A, B(int), C(int, str), D(x: int) }
fn main() {
    for e in [E.A, E.B(1), E.C(2, "s"), E.D(x: 3)] {
        let text = json.encode(e).expect("e")
        print(text)
        print(json.decode[E](text).expect("d"))
    }
    match json.decode[E](r"""{"Z": 1}""") {
        ok(_) => print("?")
        err(e) => print(e.message())
    }
}
"#);
    assert_eq!(
        out,
        "\"A\"\nA\n{\"B\":1}\nB(1)\n{\"C\":[2,\"s\"]}\nC(2, \"s\")\n{\"D\":{\"x\":3}}\nD(x: 3)\n\
         json: at `$`: `Z` is not a variant of `E` (expected one of: A, B (with data), C (with data), D (with data))\n"
    );
}

#[test]
fn value_keeps_integers_exactly_and_key_order() {
    let out = run(r#"import json
fn main() {
    let text = r"""{"z": 9007199254740993, "a": [-1, 1.5, 18446744073709551615, 0.1e1], "m": {"k": null}}"""
    let v = json.parse(text).expect("p")
    print(json.encode(v).expect("e"))
    print(v.keys())
    print(v.get("z").expect("z").as_int())
}
"#);
    assert_eq!(
        out,
        "{\"z\":9007199254740993,\"a\":[-1,1.5,18446744073709551615,1.0],\"m\":{\"k\":null}}\n\
         [\"z\", \"a\", \"m\"]\nsome(9007199254740993)\n"
    );
}

#[test]
fn value_is_declared_in_the_interface() {
    let mut vm = colibri::Vm::builder()
        // vm-pin: 2c
        .engine(colibri::EngineKind::Tree)
        .stdout(Box::new(std::io::sink()))
        .build();
    colibri_std::install(&mut vm).expect("installs");
    let text = vm.std_module("json").interface_text();
    assert!(text.contains("@host class Value"), "{text}");
    assert!(
        text.contains("fn get(self, key: str) -> Option[Value]"),
        "{text}"
    );
    assert!(
        text.contains("fn parse(text: str) -> Result[Value]"),
        "{text}"
    );
}

#[test]
fn decoding_an_object_with_many_keys_is_fast() {
    let n = 100_000;
    let body: Vec<String> = (0..n).map(|i| format!("\"k{i}\":{i}")).collect();
    let text = format!("{{{}}}", body.join(","));
    let source = format!(
        "import json\nfn main() {{\n    let text = r\"\"\"{text}\"\"\"\n    let m = json.decode[Map[str, int]](text).expect(\"d\")\n    print(m.len())\n    print(json.parse(text).expect(\"p\").len())\n}}\n"
    );
    let started = std::time::Instant::now();
    assert_eq!(run(&source), "100000\n100000\n");
    assert!(started.elapsed().as_secs() < 20, "{:?}", started.elapsed());
}

#[test]
fn a_very_deep_value_drops_and_encodes_cleanly_in_a_coroutine() {
    // 200k levels built with the constructors, dropped inside a coroutine (small stack).
    let out = run(r#"import json

fn build(n: int) -> json.Value {
    var v = json.Value.null()
    for i in 0..n {
        v = json.Value.list([v])
    }
    return v
}

fn work() -> int {
    var v = build(200000)
    var m = json.Value.map({"a": v})
    print(json.encode(v).is_err())
    print(json.pretty(m).is_err())
    print(v.equals(v))
    match json.encode(v) {
        ok(_) => print("?")
        err(e) => print(e.message())
    }
    v = json.Value.null()
    m = json.Value.null()
    return 1
}

fn main() {
    let r = go work()
    while !r.done() {
        task.tick(0.001)
    }
    print("end")
}
"#);
    assert_eq!(
        out,
        "true\ntrue\ntrue\njson: the value is nested too deeply\nend\n"
    );
}

#[test]
fn value_equals_and_double_equals_are_both_structural() {
    let out = run(r#"import json
fn main() {
    let a = json.parse(r"""{"x": [1, 2.5, "s", null, true], "y": {"p": 1, "q": 2}}""").expect("a")
    let b = json.parse(r"""{"y": {"q": 2, "p": 1}, "x": [1, 2.5, "s", null, true]}""").expect("b")
    let c = json.parse(r"""{"y": {"q": 2, "p": 1}, "x": [1, 2.5, "s", null, false]}""").expect("c")
    print(a.equals(b))
    print(a.equals(c))
    print(a == b)
    print(a == a)
    print(a.get("x").expect("x").equals(a.get("x").expect("x")))
    print(json.parse("1").expect("1").equals(json.parse("1.0").expect("1.0")))
    print(json.parse("[1]").expect("l").equals(json.parse("[1, 1]").expect("l")))
    print(json.parse("123456789012345678901234567890").expect("big").equals(json.parse("123456789012345678901234567890").expect("big")))
    print(json.Value.float(0.0 / 0.0).equals(json.Value.float(0.0 / 0.0)))
    print(a.to_json().expect("text"))
    print(json.Value.list([json.Value.int(1), json.Value.str("a")]).to_json())
    print(json.Value.float(1.0 / 0.0).to_json().is_err())
}
"#);
    assert_eq!(
        out,
        "true\nfalse\ntrue\ntrue\ntrue\nfalse\nfalse\ntrue\ntrue\n\
         {\"x\":[1,2.5,\"s\",null,true],\"y\":{\"p\":1,\"q\":2}}\n\
         ok(\"[1,\\\"a\\\"]\")\ntrue\n"
    );
}

#[test]
fn parse_reports_a_float_out_of_range_as_a_syntax_error_with_a_position() {
    let out = run(r#"import json
fn main() {
    match json.parse("[1,\n 1e999]") {
        ok(_) => print("?")
        err(e) => print("{e.as[json.Error]()} {e.message()}")
    }
}
"#);
    assert!(
        out.contains("Syntax") && out.contains("line 2, column 2") && out.contains("out of range"),
        "{out}"
    );
}

#[test]
fn a_non_finite_toml_float_is_not_stored_in_a_json_value() {
    let out = run(r#"import json
import toml
fn kind(e: Error) -> str {
    match e.as[toml.Error]() {
        some(k) => return "{k}"
        none => return "not a toml.Error"
    }
}
struct S { x: json.Value }
struct F { x: float }
struct Other { y: int }
fn main() {
    for text in ["x = nan", "x = inf", "x = -inf", "x = 1e999", "x = [1.5, nan]"] {
        match toml.decode[S](text) {
            ok(v) => print("ok " + json.encode(v).expect("e"))
            err(e) => print(kind(e))
        }
    }
    print(toml.decode[S]("x = 2.5").expect("ok").x.as_float())
    // A non-finite float that is never read into a Value is not a problem.
    print(toml.decode[Other]("y = 1\nz = nan").expect("ok").y)
    print(toml.decode[F]("x = nan").is_err())
    print(toml.parse("x = nan").expect("p").to_json().is_err())
}
"#);
    assert_eq!(
        out,
        "Unsupported\nUnsupported\nUnsupported\nUnsupported\nUnsupported\n\
         some(2.5)\n1\ntrue\ntrue\n"
    );
}

#[test]
fn a_value_shows_as_compact_json_and_compares_structurally() {
    let out = run(r#"import json
struct Holder { v: json.Value, n: int }
fn main() {
    let a = json.parse(r"""{"x": [1, 2.5, "s\n", null, true], "y": {"p": 1, "q": 2}}""").expect("a")
    let b = json.parse(r"""{"y": {"q": 2, "p": 1}, "x": [1, 2.5, "s\n", null, true]}""").expect("b")
    let c = json.parse("[1]").expect("c")
    print(a)
    print("<{c}> {a.at(0)}")
    print([c, c])
    print(some(c))
    print(Holder { v: c, n: 2 })
    print(a == b)
    print(a != c)
    print(some(a) == some(b))
    print([a, c] == [b, c])
    print(Holder { v: c, n: 2 } == Holder { v: json.parse("[1]").expect("c"), n: 2 })
    print(json.Value.float(0.0 / 0.0))
    print(json.Value.float(0.0 / 0.0) == json.Value.float(0.0 / 0.0))
    print(json.parse("1").expect("i") == json.parse("1.0").expect("f"))
}
"#);
    assert_eq!(
        out,
        "{\"x\":[1,2.5,\"s\\n\",null,true],\"y\":{\"p\":1,\"q\":2}}\n\
         <[1]> none\n[[1], [1]]\nsome([1])\nHolder { v: [1], n: 2 }\n\
         true\ntrue\ntrue\ntrue\ntrue\nnan\ntrue\nfalse\n"
    );
}

#[test]
fn a_value_as_a_map_key_or_set_element_is_found_by_structure() {
    let out = run(r#"import json
fn parse(text: str) -> json.Value => json.parse(text).expect("parses")
fn main() {
    let m: Map[json.Value, int] = {:}
    m[parse("[1]")] = 1
    m[parse("[1]")] = 2
    m[parse(r"""{"a": 1, "b": 2}""")] = 3
    m[parse(r"""{"b": 2, "a": 1}""")] = 4
    print(m.len())
    print(m.get(parse("[1]")))
    print(m[parse(r"""{"a": 1, "b": 2}""")])
    print(m.contains_key(parse("[2]")))
    let lit = {parse("1"): "x", parse("1"): "y"}
    print(lit.len())
    let s = Set([parse("[1]"), parse("[1]"), parse("null")])
    print(s.len())
    s.add(parse("null"))
    print(s.len())
    print(s.contains(parse("[1]")))
    print(s.union(Set([parse("null"), parse("2")])).len())
    print(s.intersection(Set([parse("null"), parse("2")])).len())
    // A key inside a tuple or a struct is found the same way.
    let t: Map[(json.Value, int), int] = {(parse("[1]"), 1): 7}
    print(t.get((parse("[1]"), 1)))
}
"#);
    assert_eq!(out, "2\nsome(2)\n4\nfalse\n1\n2\n2\ntrue\n3\n1\nsome(7)\n");
}

#[test]
fn showing_and_comparing_a_very_deep_value_is_safe() {
    // 200k levels, in the main fiber and in a coroutine (small stack).
    let out = run(r#"import json

fn build(n: int) -> json.Value {
    var v = json.Value.null()
    for i in 0..n {
        v = json.Value.list([v])
    }
    return v
}

fn work() -> int {
    let a = build(200000)
    let b = build(200000)
    let c = build(199999)
    let text = "{a}"
    print(text.len() < 1000)
    print(text.contains("[...]"))
    print(a == b)
    print(a == c)
    print([a] == [b])
    let m: Map[json.Value, int] = {a: 1}
    print(m.get(b))
    return 1
}

fn main() {
    let r = go work()
    while !r.done() {
        task.tick(0.001)
    }
    print("end")
}
"#);
    assert_eq!(out, "true\ntrue\ntrue\nfalse\ntrue\nsome(1)\nend\n");
}
