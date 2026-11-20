//! Checking against a host interface (EMBEDDING §4, §5).

use colibri_check::hir::{Callee, ExprKind, HostFnId, Program, StmtKind};
use colibri_check::host::{body_hash, header_line};
use colibri_check::{HostEnv, Ty};
use colibri_syntax::{FileId, parse_file};

/// The id of the first host type: the ids below it are the prelude's.
const FIRST: usize = colibri_check::types::FIRST_USER_TYPE as usize;

fn interface(body: &str) -> String {
    let caps: Vec<String> = ["fs"].iter().map(|s| s.to_string()).collect();
    format!("{}\n{body}", header_line("host", &body_hash(body), &caps))
}

const BODY: &str = "struct Vec3 { x: f32, y: f32, z: f32 }\n\
@positional fn spawn(a: f32, b: f32) -> Vec3\n\
fn named(count: int) -> int\n\
@requires(fs) fn read_save(path: str) -> Result[str]\n";

fn env() -> HostEnv {
    HostEnv::from_text(&interface(BODY)).expect("the interface parses")
}

fn check(src: &str, env: &HostEnv) -> (Option<Program>, Vec<String>) {
    let (ast, diags) = parse_file(FileId(0), src);
    assert!(diags.is_empty(), "{diags:?}");
    let checked = colibri_check::check_with_host(&ast, env, 1 << 20);
    let messages = checked
        .diagnostics
        .iter()
        .filter(|d| d.is_error())
        .map(|d| d.message.clone())
        .collect();
    (checked.program, messages)
}

#[test]
fn host_calls_lower_to_host_callees_in_interface_order() {
    let (program, errors) = check(
        "fn f() -> int {\n    let v = spawn(1.0, 2.0)\n    return named(count: 3) + int(v.x)\n}\n",
        &env(),
    );
    assert!(errors.is_empty(), "{errors:?}");
    let p = program.expect("checks");
    assert_eq!(p.host_fns.len(), 3);
    assert_eq!(p.host_fns[0].name, "spawn");
    assert_eq!(p.host_fns[2].requires.as_deref(), Some("fs"));
    assert_eq!(p.host_types, 1);
    assert_eq!(p.types[FIRST].name, "Vec3");
    // `fn_by_name` finds scripts' functions, not host stubs.
    assert!(p.fn_by_name("spawn").is_none());
    let f = p.fn_by_name("f").unwrap();
    let body = &p.fns[f.0 as usize].body;
    let StmtKind::Let { init, .. } = &body.block.stmts[0].kind else {
        panic!("a let");
    };
    let ExprKind::Call(call) = &init.kind else {
        panic!("a call");
    };
    assert_eq!(call.callee, Callee::Host(HostFnId(0)));
    assert!(matches!(&init.ty, Ty::Adt(id, _) if id.0 == FIRST as u32));
}

#[test]
fn positional_host_functions_reject_named_arguments() {
    let (_, errors) = check("let v = spawn(a: 1.0, b: 2.0)\n", &env());
    assert_eq!(errors, ["`spawn` takes positional arguments only"]);
}

#[test]
fn a_script_cannot_redefine_a_host_name() {
    let (_, errors) = check("fn named(count: int) -> int => count\n", &env());
    assert_eq!(
        errors,
        ["`named` is a host function and cannot be redefined"]
    );
    let (_, errors) = check("struct Vec3 { q: int }\n", &env());
    assert!(
        errors[0].contains("`Vec3` is defined more than once"),
        "{errors:?}"
    );
}

#[test]
fn host_functions_are_not_values() {
    let (_, errors) = check("let f = named\n", &env());
    assert_eq!(
        errors,
        ["the host function `named` cannot be used as a value"]
    );
}

#[test]
fn an_ungranted_capability_is_an_error_at_the_use_site() {
    let (_, errors) = check("let s = read_save(\"x\")\n", &env());
    assert_eq!(
        errors,
        ["`read_save` needs the `fs` capability, which this host did not grant"]
    );
    let mut granted = env();
    granted.grant("fs");
    let (program, errors) = check("let s = read_save(\"x\")\n", &granted);
    assert!(errors.is_empty() && program.is_some(), "{errors:?}");
    let mut all = env();
    all.grant_all();
    assert!(check("let s = read_save(\"x\")\n", &all).0.is_some());
}

#[test]
fn extern_blocks_need_the_ffi_capability() {
    let src = "extern \"C\" {\n    fn abs(x: int) -> int\n}\n";
    let (_, errors) = check(src, &HostEnv::new());
    assert_eq!(
        errors,
        ["`extern` blocks need the `ffi` capability, which this host did not grant"]
    );
    let mut ffi = HostEnv::new();
    ffi.grant("ffi");
    let (program, errors) = check(src, &ffi);
    assert!(errors.is_empty(), "{errors:?}");
    // The extern function is a host function with a C signature (ARCHITECTURE "C FFI as built").
    let program = program.expect("checks");
    assert_eq!(program.host_fns.len(), 1);
    let ext = program.host_fns[0]
        .ext
        .as_ref()
        .expect("an extern function");
    assert_eq!(ext.symbol, "abs");
    assert_eq!(ext.lib, None);
}

#[test]
fn host_value_types_are_ordinary_structs() {
    let (program, errors) = check(
        "let v = Vec3 { x: 1.0, y: 2.0, z: 3.0 }\nlet w = v\nprint(w.x)\n",
        &env(),
    );
    assert!(errors.is_empty() && program.is_some(), "{errors:?}");
}

#[test]
fn unknown_types_in_an_interface_do_not_crash_the_checker() {
    let body = "fn f(a: Nope) -> int\n";
    let env = HostEnv::from_text(&interface(body)).unwrap();
    let (_, errors) = check("print(1)\n", &env);
    assert_eq!(errors, ["in the host interface: unknown type `Nope`"]);
}

const CLASSES: &str = "struct Vec3 { x: f32, y: f32, z: f32 }\n\
@host class World {}\n\
impl World {\n    fn new(gravity: Vec3) -> World\n    fn step(self, dt: f32)\n    fn position(self, body: Body) -> Vec3\n    @requires(fs) fn save(self, path: str)\n}\n\
@host class Body {}\n\
fn spawn_body(x: f32) -> Body\n";

fn classes() -> HostEnv {
    HostEnv::from_text(&interface(CLASSES)).expect("the interface parses")
}

#[test]
fn host_classes_are_declared_before_user_types_with_stable_ids() {
    use colibri_check::hir::TypeDefKind;
    let (program, errors) = check(
        "class Mine { n: int }\nstruct Plain { n: int }\n",
        &classes(),
    );
    assert!(errors.is_empty(), "{errors:?}");
    let p = program.expect("checks");
    // Vec3, World, Body come from the host, then the script's own.
    assert_eq!(p.host_types, 3);
    let names: Vec<&str> = p.types[FIRST..].iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["Vec3", "World", "Body", "Mine", "Plain"]);
    assert!(matches!(p.types[FIRST + 1].kind, TypeDefKind::HostClass));
    assert!(matches!(p.types[FIRST + 2].kind, TypeDefKind::HostClass));
    assert!(matches!(p.types[FIRST + 3].kind, TypeDefKind::Class { .. }));
}

#[test]
fn host_methods_lower_to_host_callees_with_the_receiver_first() {
    let (program, errors) = check(
        "fn f(w: World, b: Body) -> Vec3 {\n    w.step(0.5)\n    let g = Vec3 { x: 0.0, y: 0.0, z: 0.0 }\n    let other = World.new(g)\n    other.step(dt: 1.0)\n    return w.position(b)\n}\n",
        &classes(),
    );
    assert!(errors.is_empty(), "{errors:?}");
    let p = program.expect("checks");
    // Declaration order of the interface: `new`, `step`, `position`, `save`, `spawn_body`.
    let names: Vec<&str> = p.host_fns.iter().map(|h| h.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "World.new",
            "World.step",
            "World.position",
            "World.save",
            "spawn_body"
        ]
    );
    let f = p.fn_by_name("f").unwrap();
    let body = &p.fns[f.0 as usize].body;
    let StmtKind::Expr(first) = &body.block.stmts[0].kind else {
        panic!("a statement");
    };
    let ExprKind::Call(call) = &first.kind else {
        panic!("a call");
    };
    assert_eq!(call.callee, Callee::Host(HostFnId(1)));
    // The receiver is the first argument; `dt` follows.
    assert_eq!(call.args.len(), 2);
}

#[test]
fn a_host_class_has_alive_but_no_fields_literals_clone_or_weak_references() {
    let (program, errors) = check("fn f(w: World) -> bool => w.alive()\n", &classes());
    assert!(errors.is_empty() && program.is_some(), "{errors:?}");
    for (src, wanted) in [
        ("fn f(w: World) {\n    w.clone()\n}\n", "cannot be cloned"),
        ("fn f(w: World) -> int => w.gravity\n", "has no fields"),
        ("fn f() {\n    World {}\n}\n", "cannot be built"),
        (
            "fn f(w: World) {\n    w.alive(1)\n}\n",
            "`alive` takes no arguments",
        ),
        (
            "fn f(w: World) {\n    w.explode()\n}\n",
            "`World` has no method `explode`",
        ),
        (
            "fn f(w: World) {\n    w.step(\"x\")\n}\n",
            "expected `f32`, found `str`",
        ),
    ] {
        let (_, errors) = check(src, &classes());
        assert!(
            errors.iter().any(|e| e.contains(wanted)),
            "wanted `{wanted}` for {src:?}: {errors:?}"
        );
    }
}

#[test]
fn a_host_method_with_a_capability_needs_it_at_the_use_site() {
    let (_, errors) = check("fn f(w: World) {\n    w.save(\"x\")\n}\n", &classes());
    assert_eq!(
        errors,
        ["`World.save` needs the `fs` capability, which this host did not grant"]
    );
    let mut granted = classes();
    granted.grant("fs");
    let (program, errors) = check("fn f(w: World) {\n    w.save(\"x\")\n}\n", &granted);
    assert!(errors.is_empty() && program.is_some(), "{errors:?}");
}

#[test]
fn host_classes_compare_by_identity_and_key_maps() {
    let (program, errors) = check(
        "fn f(a: World, b: World) -> bool {\n    let m: Map[World, int] = {a: 1}\n    return a == b || a is b || m.len() == 1\n}\n",
        &classes(),
    );
    assert!(errors.is_empty() && program.is_some(), "{errors:?}");
    // A script may add methods of its own to a host class.
    let (program, errors) = check(
        "impl World {\n    fn twice(self) {\n        self.step(1.0)\n        self.step(1.0)\n    }\n}\n",
        &classes(),
    );
    assert!(errors.is_empty() && program.is_some(), "{errors:?}");
}

#[test]
fn interface_text_can_name_bytes_and_bytes_builder() {
    let body =
        "fn fill(out: BytesBuilder, data: Bytes) -> Result[Bytes]\nfn size(data: Bytes) -> int\n";
    let env = HostEnv::from_text(&interface(body)).expect("the interface parses");
    let (program, errors) = check(
        "fn f() -> int {\n    let b = BytesBuilder()\n    let r = fill(b, Bytes([1, 2]))\n    return size(b.to_bytes()) + r.expect(\"filled\").len()\n}\n",
        &env,
    );
    assert!(errors.is_empty(), "{errors:?}");
    assert!(program.is_some());
}

/// Attributes in interface text, at every position: the known ones where they apply, and a
/// clear refusal anywhere else (never silently ignored).
#[test]
fn host_interface_attributes_are_accepted_or_refused_at_every_position() {
    // (attribute, program with `{A}`, accepted)
    let cases: &[(&str, &str, bool)] = &[
        ("deprecated(\"old\")", "{A} fn f() -> int", true),
        ("requires(fs)", "{A} fn f() -> int", true),
        ("positional", "{A} fn f(a: int) -> int", true),
        ("when(true)", "{A} fn f() -> int", false),
        ("foo", "{A} fn f() -> int", false),
        ("deprecated(\"old\")", "{A} struct S { x: int }", true),
        ("host", "{A} struct S { x: int }", false),
        ("deprecated(\"old\")", "struct S { {A} x: int }", true),
        ("when(true)", "struct S { {A} x: int }", false),
        ("foo", "struct S { {A} x: int }", false),
        ("rename(\"k\")", "struct S { {A} x: int }", false),
        ("host", "{A} class C {}", true),
        ("deprecated(\"old\")", "@host {A} class C {}", true),
        ("foo", "@host class C {}\nimpl C { {A} fn m(self) }", false),
        (
            "requires(fs)",
            "@host class C {}\nimpl C { {A} fn m(self) }",
            true,
        ),
        (
            "deprecated(\"old\")",
            "@host class C {}\nimpl C { {A} fn m(self) }",
            true,
        ),
        (
            "when(true)",
            "@host class C {}\nimpl C { {A} fn m(self) }",
            false,
        ),
        (
            "deprecated(\"old\")",
            "@host class C {}\n{A} impl C { fn m(self) }",
            false,
        ),
        ("module", "{A} enum E { A, B }", true),
        ("deprecated(\"old\")", "{A} enum E { A, B }", true),
        ("foo", "{A} enum E { A, B }", false),
        ("deprecated(\"old\")", "{A} const K: int = 1", true),
        (
            "deprecated(\"old\")",
            "struct P { x: int }\n{A} type Q = P",
            true,
        ),
        ("foo", "struct P { x: int }\n{A} type Q = P", false),
        ("foo", "{A} const K: int = 1", false),
    ];
    for (attr, body, accepted) in cases {
        let body = body.replace("{A}", &format!("@{attr}"));
        let parsed = HostEnv::from_text(&interface(&format!("{body}\n")));
        assert_eq!(
            parsed.is_ok(),
            *accepted,
            "@{attr} in `{body}`: {:?}",
            parsed.err()
        );
    }
}

#[test]
fn a_deprecated_alias_in_an_interface_warns_where_it_is_used() {
    let env = HostEnv::from_text(&interface(
        "struct P { x: int }\n@deprecated(\"use P\") type Q = P\nfn make() -> P\n",
    ))
    .expect("the interface parses");
    let (ast, diags) = parse_file(FileId(0), "fn f(q: Q) -> int => q.x\n");
    assert!(diags.is_empty(), "{diags:?}");
    let checked = colibri_check::check_with_host(&ast, &env, 1 << 20);
    let warnings: Vec<String> = checked
        .diagnostics
        .iter()
        .filter(|d| !d.is_error())
        .map(|d| d.message.clone())
        .collect();
    assert!(checked.program.is_some(), "{:?}", checked.diagnostics);
    assert!(
        warnings.iter().any(|w| w == "`Q` is deprecated: use P"),
        "{warnings:?}"
    );
}
