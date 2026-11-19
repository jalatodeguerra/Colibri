//! Checking against a host interface (EMBEDDING §4, §5).

use colibri_check::hir::{Callee, ExprKind, HostFnId, Program, StmtKind};
use colibri_check::host::{body_hash, header_line};
use colibri_check::{HostEnv, Ty};
use colibri_syntax::{FileId, parse_file};

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
    assert_eq!(p.types[12].name, "Vec3");
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
    assert!(matches!(&init.ty, Ty::Adt(id, _) if id.0 == 12));
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
    let (_, errors) = check(src, &ffi);
    assert_eq!(errors, ["`extern` is not supported yet"]);
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
