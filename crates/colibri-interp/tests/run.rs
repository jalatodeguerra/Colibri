//! The interpreter on small checked programs: unwinding, state between calls, limits.

use colibri_core::prelude::*;
use std::cell::RefCell;
use std::io::Write;
use std::rc::Rc;

use colibri_interp::{Fiber, Interp, PanicInfo, Value};
use colibri_syntax::{FileId, parse_file};

#[derive(Clone, Default)]
struct Out(Rc<RefCell<Vec<u8>>>);

impl Write for Out {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct Machine {
    interp: Interp,
    fiber: Fiber,
    program: colibri_check::Program,
    out: Out,
}

fn machine(src: &str, depth: usize) -> Machine {
    let (ast, diags) = parse_file(FileId(0), src);
    assert!(diags.is_empty(), "{diags:?}");
    let checked = colibri_check::check(&ast);
    let program = checked
        .program
        .unwrap_or_else(|| panic!("{:?}", checked.diagnostics));
    let out = Out::default();
    let mut interp = Interp::new(Box::new(out.clone()), depth);
    interp.set_native_stack_budget(64 << 20);
    interp.load(&program);
    Machine {
        interp,
        fiber: Fiber::new(),
        program,
        out,
    }
}

impl Machine {
    fn call(&mut self, name: &str, args: Vec<Value>) -> Result<Value, Box<PanicInfo>> {
        let id = self.program.fn_by_name(name).expect("function exists");
        self.interp.call(&mut self.fiber, id, args)
    }

    fn printed(&self) -> String {
        String::from_utf8_lossy(&self.out.0.borrow()).into_owned()
    }
}

#[test]
fn a_panic_unwinds_every_frame_and_temporary() {
    let mut m = machine(
        "fn f(n: int) -> int => 1 + (2 + (3 + g(n)))\nfn g(n: int) -> int => 10 / n\nfn fine() -> int => 7\n",
        100,
    );
    let err = m.call("f", vec![Value::Int(0)]).unwrap_err();
    assert_eq!(err.message, "division by zero");
    let names: Vec<&str> = err.stack.iter().map(|e| e.function.as_str()).collect();
    assert_eq!(names, ["g", "f"]);
    assert_eq!(m.fiber.depth(), 0);
    assert_eq!(m.fiber.pending_temps(), 0);
    // The same fiber keeps working.
    assert_eq!(m.call("fine", vec![]).unwrap(), Value::Int(7));
    assert_eq!(m.call("f", vec![Value::Int(5)]).unwrap(), Value::Int(8));
}

#[test]
fn returns_and_breaks_inside_nested_blocks_leave_the_fiber_clean() {
    let mut m = machine(
        "fn f() -> int {\n    var i = 0\n    while true {\n        let a = i\n        if i == 3 {\n            let b = a * 2\n            return b\n        }\n        i += 1\n    }\n    return 0\n}\n",
        100,
    );
    assert_eq!(m.call("f", vec![]).unwrap(), Value::Int(6));
    assert_eq!((m.fiber.depth(), m.fiber.pending_temps()), (0, 0));
}

#[test]
fn module_state_survives_a_panic() {
    let mut m = machine(
        "var n = 0\nfn bump() -> int {\n    n += 1\n    return n\n}\nfn boom() -> int {\n    n += 100\n    return 1 / 0\n}\n",
        100,
    );
    m.interp.run_module(&mut m.fiber).unwrap();
    assert_eq!(m.call("bump", vec![]).unwrap(), Value::Int(1));
    assert!(m.call("boom", vec![]).is_err());
    assert_eq!(m.call("bump", vec![]).unwrap(), Value::Int(102));
}

#[test]
fn the_depth_limit_counts_active_calls() {
    let mut m = machine(
        "fn d(n: int) -> int => if n == 0 { 0 } else { 1 + d(n - 1) }\n",
        10,
    );
    assert_eq!(m.call("d", vec![Value::Int(9)]).unwrap(), Value::Int(9));
    let err = m.call("d", vec![Value::Int(10)]).unwrap_err();
    assert!(err.message.starts_with("stack overflow"), "{}", err.message);
    assert_eq!(err.stack.len(), 10);
    assert_eq!(m.fiber.depth(), 0);
}

#[test]
fn the_native_stack_budget_is_a_backstop_behind_the_depth_limit() {
    let mut m = machine(
        "fn d(n: int) -> int => if n == 0 { 0 } else { 1 + d(n - 1) }\n",
        1_000_000,
    );
    m.interp.set_native_stack_budget(256 << 10);
    let err = m.call("d", vec![Value::Int(100_000)]).unwrap_err();
    assert!(
        err.message.contains("native stack budget"),
        "{}",
        err.message
    );
    assert_eq!(m.fiber.depth(), 0);
}

#[test]
fn a_script_runs_its_values_and_statements_top_to_bottom() {
    let mut m = machine(
        "fn noisy(s: str) -> int {\n    print(s)\n    return 1\n}\nprint(\"script\")\nlet b = noisy(\"b\")\nlet a = noisy(\"a\") + b\n",
        100,
    );
    m.interp.run_module(&mut m.fiber).unwrap();
    assert_eq!(m.printed(), "script\nb\na\n");
    assert_eq!(
        m.interp.global(colibri_check::hir::GlobalId(1)),
        Some(&Value::Int(2))
    );
}

#[test]
fn a_program_initialises_its_values_in_dependency_order_first() {
    let mut m = machine(
        "fn noisy(s: str) -> int {\n    print(s)\n    return 1\n}\nprint(\"script\")\nlet a = noisy(\"a\") + b\nlet b = noisy(\"b\")\nfn main() {}\n",
        100,
    );
    m.interp.run_module(&mut m.fiber).unwrap();
    assert_eq!(m.printed(), "b\na\nscript\n");
    assert_eq!(
        m.interp.global(colibri_check::hir::GlobalId(0)),
        Some(&Value::Int(2))
    );
}

#[test]
fn panics_carry_the_call_site_of_each_frame() {
    let src = "fn a() -> int => b()\nfn b() -> int => c()\nfn c() -> int => 1 / 0\n";
    let mut m = machine(src, 100);
    let err = m.call("a", vec![]).unwrap_err();
    let at = |span: colibri_syntax::Span| &src[span.start as usize..span.end as usize];
    assert_eq!(at(err.span), "1 / 0");
    assert_eq!(at(err.stack[0].span), "1 / 0");
    assert_eq!(at(err.stack[1].span), "c()");
    assert_eq!(at(err.stack[2].span), "b()");
}

#[test]
fn a_panic_with_live_objects_releases_everything_and_traces_the_drops() {
    let src = "class T { name: str }\nfn f(z: int) -> int {\n    let a = T { name: \"a\" }\n    let b = T { name: \"b\" }\n    return 1 / z\n}\n";
    let mut m = machine(src, 100);
    m.interp.core_mut().set_trace_drops(true);
    let err = m.call("f", vec![Value::Int(0)]).unwrap_err();
    assert_eq!(err.message, "division by zero");
    assert_eq!((m.fiber.depth(), m.fiber.pending_temps()), (0, 0));
    assert_eq!(m.interp.core_mut().take_trace(), ["b", "a"]);
    // Taking the trace empties it.
    assert!(m.interp.core_mut().take_trace().is_empty());
}

#[test]
fn module_values_die_once_at_shutdown_in_reverse_order() {
    let src = "class T { name: str }\nlet a = T { name: \"a\" }\nlet b = T { name: \"b\" }\n";
    let mut m = machine(src, 100);
    m.interp.core_mut().set_trace_drops(true);
    m.interp.run_module(&mut m.fiber).unwrap();
    assert!(m.interp.core_mut().take_trace().is_empty());
    m.interp.shutdown();
    assert_eq!(m.interp.core_mut().take_trace(), ["b", "a"]);
    m.interp.shutdown();
    assert!(m.interp.core_mut().take_trace().is_empty());
}

#[test]
fn dropping_the_interpreter_releases_its_module_values() {
    // In a debug build a class object dropped without `release` aborts the test.
    let src = "class T { name: str }\nlet a = [T { name: \"a\" }]\n";
    let mut m = machine(src, 100);
    m.interp.run_module(&mut m.fiber).unwrap();
    drop(m);
}

#[test]
fn a_panicking_defer_is_a_note_on_the_first_panic() {
    let src = "fn bad() {\n    let z = 0\n    print(1 / z)\n}\nfn f() {\n    defer bad()\n    let z = 0\n    print(1 % z)\n}\n";
    let mut m = machine(src, 100);
    let err = m.call("f", vec![]).unwrap_err();
    assert_eq!(err.message, "modulo by zero");
    assert_eq!(err.notes.len(), 1);
    assert!(err.notes[0].contains("division by zero"), "{:?}", err.notes);
}

#[test]
fn module_enums_are_found_by_module_and_never_by_bare_name() {
    use colibri_check::HostEnv;
    use colibri_check::host::{body_hash, header_line};
    let module = |name: &str, body: &str| {
        let text = format!("{}\n{body}", header_line(name, &body_hash(body), &[]));
        HostEnv::from_text(&text).expect("the interface parses")
    };
    let mut env = module(
        "std.fs",
        "@module enum Error { NotFound, Other }\nstruct Stat { size: int }\n",
    );
    env.merge(module(
        "std.http",
        "@module enum Error { Timeout, Other, Dns }\n",
    ));
    let (ast, diags) = parse_file(FileId(0), "fn main() {}\n");
    assert!(diags.is_empty(), "{diags:?}");
    let checked = colibri_check::check_with_host(&ast, &env, 1 << 20);
    let program = checked
        .program
        .unwrap_or_else(|| panic!("{:?}", checked.diagnostics));
    let mut interp = Interp::new(Box::new(Out::default()), 64);
    interp.load(&program);
    // Each module's `Error` has its own id and its own variants.
    let fs = interp
        .core()
        .host_type_in("std.fs", "Error")
        .expect("fs.Error");
    let http = interp
        .core()
        .host_type_in("std.http", "Error")
        .expect("http.Error");
    assert_ne!(fs, http);
    assert_eq!(
        interp
            .core()
            .module_enum_variant("std.http", "Error", "Dns"),
        Ok((http, 2))
    );
    assert_eq!(
        interp
            .core()
            .module_enum_variant("std.fs", "Error", "Other"),
        Ok((fs, 1))
    );
    // The bare name would silently keep one of them: it is not in the map at all.
    assert_eq!(interp.core().host_type("Error"), None);
    // A struct of the module is its module's too: found with the module, not by its bare
    // name, and it is not a module enum.
    assert_eq!(interp.core().host_type("Stat"), None);
    assert!(interp.core().host_type_in("std.fs", "Stat").is_some());
    let message = interp
        .core()
        .module_enum_variant("std.fs", "Stat", "NotFound")
        .unwrap_err();
    assert!(
        message.contains("`fs.Stat` is not a module enum"),
        "{message}"
    );
    let message = interp
        .core()
        .module_enum_variant("std.fs", "Error", "Gone")
        .unwrap_err();
    assert!(
        message.contains("`fs.Error` has no variant `Gone`; its variants are NotFound, Other"),
        "{message}"
    );
}

/// Keys that cannot reach a user-written `Eq` take the hashed path even when the program has
/// an `impl Eq` for some other type; keys that can, scan (and are found by that `Eq`).
#[test]
fn only_keys_that_reach_a_user_eq_are_scanned() {
    let src = "struct P { x: int, y: int }\n\
        struct Q { v: int }\n\
        impl Eq for Q { fn eq(self, other: Q) -> bool => self.v % 10 == other.v % 10 }\n\
        struct Pair[T] { a: T }\n\
        enum Tag { Plain, Carry(Q) }\n\
        fn plain() -> int {\n\
            let p: Map[P, int] = {:}\n\
            let s = Set([P { x: 1, y: 1 }])\n\
            let g: Map[Pair[int], int] = {:}\n\
            var i = 0\n\
            while i < 300 { p[P { x: i, y: i }] = i; g[Pair { a: i }] = i; s.add(P { x: i, y: 0 }); i += 1 }\n\
            return p.len() + g.len() + s.len() + Set([P { x: 1, y: 1 }, P { x: 1, y: 1 }]).len()\n\
        }\n\
        fn user() -> int {\n\
            let m: Map[Q, int] = {:}\n\
            let g: Map[Pair[Q], int] = {:}\n\
            let t: Map[Tag, int] = {:}\n\
            var i = 0\n\
            while i < 30 { m[Q { v: i }] = i; g[Pair { a: Q { v: i } }] = i; t[Tag.Carry(Q { v: i })] = i; i += 1 }\n\
            return m.len() * 10000 + g.len() * 100 + t.len()\n\
        }\n";
    let mut m = machine(src, 200);
    assert_eq!(
        m.call("plain", vec![]).unwrap(),
        Value::Int(300 + 300 + 301 + 1)
    );
    assert_eq!(m.interp.key_scans(), 0, "no key of these types may scan");
    assert_eq!(
        m.call("user", vec![]).unwrap(),
        Value::Int(10 * 10000 + 10 * 100 + 10)
    );
    assert!(m.interp.key_scans() >= 90, "{}", m.interp.key_scans());
}
