//! What the machine keeps in the shared `Core`: the key-scan counter, the host types of
//! modules, and the module values at shutdown. (These were tests of the tree-walker, which
//! checked engine-neutral behaviour; they run on the VM now.)

use std::cell::RefCell;
use std::io::Write;
use std::rc::Rc;

use colibri_core::engine::Engine;
use colibri_core::value::Value;
use colibri_syntax::{FileId, parse_file};
use colibri_vm::{Machine, VmFiber};

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

struct Loaded {
    machine: Machine,
    fiber: VmFiber,
    program: colibri_check::Program,
}

fn loaded(src: &str) -> Loaded {
    let (ast, diags) = parse_file(FileId(0), src);
    assert!(diags.is_empty(), "{diags:?}");
    let checked = colibri_check::check(&ast);
    let program = checked
        .program
        .unwrap_or_else(|| panic!("{:?}", checked.diagnostics));
    let mut machine = Machine::new(Box::new(Out::default()), 200);
    machine.set_native_stack_budget(64 << 20);
    machine.load(&program);
    Loaded {
        machine,
        fiber: VmFiber::new(),
        program,
    }
}

impl Loaded {
    fn call(&mut self, name: &str) -> Value {
        let f = self.program.fn_by_name(name).expect("the function exists");
        self.machine
            .call(&mut self.fiber, f, vec![])
            .unwrap_or_else(|p| panic!("{}", p.message))
    }
}

#[test]
fn module_values_die_once_at_shutdown_in_reverse_order() {
    let src = "class T { name: str }\nlet a = T { name: \"a\" }\nlet b = T { name: \"b\" }\n";
    let mut m = loaded(src);
    m.machine.core_mut().set_trace_drops(true);
    m.machine.run_module(&mut m.fiber).unwrap();
    assert!(m.machine.core_mut().take_trace().is_empty());
    m.machine.shutdown();
    assert_eq!(m.machine.core_mut().take_trace(), ["b", "a"]);
    // A second shutdown drops nothing.
    m.machine.shutdown();
    assert!(m.machine.core_mut().take_trace().is_empty());
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
    let mut machine = Machine::new(Box::new(Out::default()), 64);
    machine.load(&program);
    // Each module's `Error` has its own id and its own variants.
    let fs = machine
        .core()
        .host_type_in("std.fs", "Error")
        .expect("fs.Error");
    let http = machine
        .core()
        .host_type_in("std.http", "Error")
        .expect("http.Error");
    assert_ne!(fs, http);
    assert_eq!(
        machine
            .core()
            .module_enum_variant("std.http", "Error", "Dns"),
        Ok((http, 2))
    );
    assert_eq!(
        machine
            .core()
            .module_enum_variant("std.fs", "Error", "Other"),
        Ok((fs, 1))
    );
    // The bare name would silently keep one of them: it is not in the map at all.
    assert_eq!(machine.core().host_type("Error"), None);
    // A struct of the module is its module's too: found with the module, not by its bare
    // name, and it is not a module enum.
    assert_eq!(machine.core().host_type("Stat"), None);
    assert!(machine.core().host_type_in("std.fs", "Stat").is_some());
    let message = machine
        .core()
        .module_enum_variant("std.fs", "Stat", "NotFound")
        .unwrap_err();
    assert!(
        message.contains("`fs.Stat` is not a module enum"),
        "{message}"
    );
    let message = machine
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
            let mut i = 0\n\
            while i < 300 { p[P { x: i, y: i }] = i; g[Pair { a: i }] = i; s.add(P { x: i, y: 0 }); i += 1 }\n\
            return p.len() + g.len() + s.len() + Set([P { x: 1, y: 1 }, P { x: 1, y: 1 }]).len()\n\
        }\n\
        fn user() -> int {\n\
            let m: Map[Q, int] = {:}\n\
            let g: Map[Pair[Q], int] = {:}\n\
            let t: Map[Tag, int] = {:}\n\
            let mut i = 0\n\
            while i < 30 { m[Q { v: i }] = i; g[Pair { a: Q { v: i } }] = i; t[Tag.Carry(Q { v: i })] = i; i += 1 }\n\
            return m.len() * 10000 + g.len() * 100 + t.len()\n\
        }\n";
    let mut m = loaded(src);
    assert_eq!(m.call("plain"), Value::Int(300 + 300 + 301 + 1));
    assert_eq!(
        m.machine.core().key_scans,
        0,
        "no key of these types may scan"
    );
    assert_eq!(m.call("user"), Value::Int(10 * 10000 + 10 * 100 + 10));
    assert!(
        m.machine.core().key_scans >= 90,
        "{}",
        m.machine.core().key_scans
    );
}
