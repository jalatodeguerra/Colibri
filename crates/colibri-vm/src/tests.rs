//! Tests that need the machine's internals: calls from Rust into Colibri while the dispatch
//! loop is running (`NativeEntry` frames), and the frame and window model.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use colibri_check::hir::FnId;
use colibri_core::engine::Engine;
use colibri_core::value::Value;

use crate::fiber::VmFiber;
use crate::machine::Machine;
use crate::source::check_source;

#[derive(Clone, Default)]
struct Out(Rc<RefCell<Vec<u8>>>);

impl std::io::Write for Out {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn load(src: &str) -> (Machine, colibri_check::hir::Program, Out) {
    let (program, _) = check_source(src).expect("the program checks");
    let out = Out::default();
    let mut m = Machine::new(Box::new(out.clone()), 1000);
    m.load(&program);
    (m, program, out)
}

fn fn_id(p: &colibri_check::hir::Program, name: &str) -> FnId {
    p.fn_by_name(name).expect("the function exists")
}

#[test]
fn native_entries_nest_inside_the_dispatch_loop() {
    let src = "
fn inner(n: int) -> int => n * 2
fn boom(n: int) -> int => 10 / n
fn echo(s: str) -> str => s + \"!\"
fn work(a: int) -> int {
    let b = a + 1
    let tag = \"t\" + \"ag\"
    print(b)
    print(tag)
    return a + b
}
fn main() { print(work(5)) }
";
    let (mut m, program, out) = load(src);
    let (inner, boom, echo) = (
        fn_id(&program, "inner"),
        fn_id(&program, "boom"),
        fn_id(&program, "echo"),
    );
    let fired = Rc::new(Cell::new(0));
    let seen = Rc::new(RefCell::new(Vec::new()));
    {
        let (fired, seen) = (Rc::clone(&fired), Rc::clone(&seen));
        m.print_hook = Some(Rc::new(move |m: &mut Machine, fiber: &mut VmFiber| {
            fired.set(fired.get() + 1);
            // `main -> work` are running for the two prints in `work`, `main` for the last:
            // the nested entries sit above every live window.
            let depth = fiber.depth();
            assert!(depth == 1 || depth == 2);
            let top = fiber.top();
            let r = m.call_fn(fiber, inner, vec![Value::Int(21)]);
            assert!(matches!(r, Ok(Value::Int(42))), "{r:?}");
            assert_eq!(fiber.depth(), depth, "the nested frame popped");
            // A string argument is owned by the entry and released by it.
            let r = m.call_fn(fiber, echo, vec![Value::Str(Rc::from("hi"))]);
            assert!(matches!(&r, Ok(Value::Str(s)) if &**s == "hi!"), "{r:?}");
            // A panic in the nested call unwinds only the nested frames.
            let Err(colibri_core::ctl::Ctl::Panic(info)) =
                m.call_fn(fiber, boom, vec![Value::Int(0)])
            else {
                panic!("expected a panic")
            };
            seen.borrow_mut().push((
                info.message.clone(),
                info.stack
                    .iter()
                    .map(|t| t.function.clone())
                    .collect::<Vec<_>>(),
            ));
            assert_eq!(fiber.depth(), depth);
            assert_eq!(fiber.top(), top, "no window was left behind");
            // Re-entering twice deep: a nested call that is itself running while we call again
            // is covered by `inner` being entered on top of `work`'s window, not main's.
        }));
    }
    let mut fiber = VmFiber::new();
    m.run_module(&mut fiber).expect("the module runs");
    let main = fn_id(&program, "main");
    m.call(&mut fiber, main, Vec::new()).expect("main runs");
    assert_eq!(fired.get(), 3, "print ran in work twice and in main once");
    assert_eq!(String::from_utf8_lossy(&out.0.borrow()), "6\ntag\n11\n");
    let seen = seen.borrow();
    assert_eq!(seen[0].0, "division by zero");
    assert_eq!(seen[0].1, ["boom", "work", "main"]);
    assert_eq!(seen[2].1, ["boom", "main"]);
}

#[test]
fn a_nested_entry_starts_above_the_live_window_and_leaves_it_untouched() {
    // `work` keeps a string alive in a register while the hook enters Colibri from Rust: the
    // string must survive and still be dropped by `work` itself (the debug check of the
    // Unit invariant fails the test otherwise).
    let src = "
fn echo(s: str) -> str => s + \"?\"
fn work(a: str) -> str {
    let b = a + \"1\"
    print(b)
    let c = b + \"2\"
    return c + a
}
print(work(\"x\"))
";
    let (mut m, program, out) = load(src);
    let echo = fn_id(&program, "echo");
    let bases = Rc::new(RefCell::new(Vec::new()));
    {
        let bases = Rc::clone(&bases);
        m.print_hook = Some(Rc::new(move |m: &mut Machine, fiber: &mut VmFiber| {
            bases.borrow_mut().push(fiber.top());
            for _ in 0..3 {
                let r = m.call_fn(fiber, echo, vec![Value::Str(Rc::from("p"))]);
                assert!(matches!(&r, Ok(Value::Str(s)) if &**s == "p?"), "{r:?}");
            }
        }));
    }
    let mut fiber = VmFiber::new();
    m.run_module(&mut fiber).expect("the module runs");
    assert_eq!(String::from_utf8_lossy(&out.0.borrow()), "x1\nx12x\n");
    // The registers above every frame hold nothing that owns a value.
    assert!(fiber.regs.iter().all(crate::fiber::is_scalar_value));
    assert!(!bases.borrow().is_empty());
}

#[test]
fn the_call_depth_limit_counts_nested_entries_too() {
    let src = "fn f(n: int) -> int => n\nfn main() { print(1) }\n";
    let (mut m, program, _out) = load(src);
    // `main` is one frame; a call from Rust on top of it would be the second.
    m.max_depth = 1;
    let f = fn_id(&program, "f");
    let refused = Rc::new(Cell::new(false));
    {
        let refused = Rc::clone(&refused);
        m.print_hook = Some(Rc::new(move |m: &mut Machine, fiber: &mut VmFiber| {
            let Err(colibri_core::ctl::Ctl::Panic(info)) = m.call_fn(fiber, f, vec![Value::Int(1)])
            else {
                panic!("the nested call should be refused")
            };
            assert_eq!(info.message, "stack overflow: more than 1 nested calls");
            assert_eq!(fiber.depth(), 1);
            refused.set(true);
        }));
    }
    let mut fiber = VmFiber::new();
    m.run_module(&mut fiber).expect("the module runs");
    let main = fn_id(&program, "main");
    m.call(&mut fiber, main, Vec::new()).expect("main runs");
    assert!(refused.get());
}

#[test]
fn a_panic_inside_a_native_entry_leaves_no_value_in_any_register() {
    let src = "
fn bad(s: str, z: int) -> int {
    let t = s + \"x\"
    let u = t + \"y\"
    return 10 / z
}
fn main() { print(1) }
";
    let (mut m, program, _out) = load(src);
    let bad = fn_id(&program, "bad");
    let ran = Rc::new(Cell::new(false));
    {
        let ran = Rc::clone(&ran);
        m.print_hook = Some(Rc::new(move |m: &mut Machine, fiber: &mut VmFiber| {
            let r = m.call_fn(fiber, bad, vec![Value::Str(Rc::from("s")), Value::Int(0)]);
            assert!(matches!(r, Err(colibri_core::ctl::Ctl::Panic(_))));
            assert!(
                fiber.regs.iter().all(crate::fiber::is_scalar_value),
                "a register still owns a value after the unwind: {:?}",
                fiber.regs
            );
            ran.set(true);
        }));
    }
    let mut fiber = VmFiber::new();
    m.run_module(&mut fiber).expect("the module runs");
    let main = fn_id(&program, "main");
    m.call(&mut fiber, main, Vec::new()).expect("main runs");
    assert!(ran.get());
}

#[test]
fn a_script_value_used_through_a_function_before_its_line_panics_at_run_time() {
    // The checker rejects a direct use, so the use is made from outside the program's text:
    // the print hook calls `get` while the script is still before the line of `late`.
    let src = "fn get() -> int => late
print(1)
let late = 5
print(late)
";
    let (mut m, program, out) = load(src);
    let get = fn_id(&program, "get");
    let msgs = Rc::new(RefCell::new(Vec::new()));
    {
        let msgs = Rc::clone(&msgs);
        m.print_hook = Some(Rc::new(
            move |m: &mut Machine, fiber: &mut VmFiber| match m.call_fn(fiber, get, Vec::new()) {
                Err(colibri_core::ctl::Ctl::Panic(info)) => {
                    msgs.borrow_mut().push(info.message.clone());
                }
                Ok(v) => msgs.borrow_mut().push(format!("returned {v:?}")),
                Err(_) => msgs.borrow_mut().push("other".to_string()),
            },
        ));
    }
    let mut fiber = VmFiber::new();
    m.run_module(&mut fiber).expect("the module runs");
    let msgs = msgs.borrow();
    // The first print runs before `late` has its value, the second after.
    assert_eq!(msgs.len(), 2, "{msgs:?}");
    assert!(
        msgs[0].contains("`late` is used before it is initialised")
            && msgs[0].contains("the script has not reached its line"),
        "{msgs:?}"
    );
    assert!(msgs[1].starts_with("returned"), "{msgs:?}");
    assert_eq!(String::from_utf8_lossy(&out.0.borrow()), "1\n5\n");
}

const SHAPES: &str = "
interface Speak { fn say(self) -> str }
struct A { n: int }
impl Speak for A { fn say(self) -> str => \"A{VERSION}\" }
fn run() -> str {
    let s: Speak = A { n: 1 }
    return s.say()
}
fn main() { print(run()) }
";

fn shapes(version: &str) -> String {
    SHAPES.replace("{VERSION}", version)
}

fn cache_of(chunk: &crate::bytecode::Chunk) -> Option<crate::bytecode::IcEntry> {
    use crate::bytecode::Site;
    chunk.sites.iter().find_map(|s| match s {
        Site::Iface(i) => Some(i.cache.get()),
        _ => None,
    })?
}

/// The cache of an interface call site lives in its chunk and names the load it was made for:
/// a chunk that a running frame keeps across a patch refreshes its entry (and runs the new
/// body of the method) instead of trusting the old one, and a new chunk starts empty.
#[test]
fn an_inline_cache_does_not_outlive_a_patch() {
    let (mut m, program, _out) = load(&shapes("1"));
    let mut fiber = VmFiber::new();
    let run = fn_id(&program, "run");
    let r = m.call(&mut fiber, run, vec![]).expect("runs");
    assert!(matches!(&r, Value::Str(s) if &**s == "A1"), "{r:?}");
    let old = m.chunk_of(run).expect("exists").expect("compiles");
    let first = cache_of(&old).expect("a filled cache");
    assert_eq!(first.epoch, m.epoch);

    let (patched, _) = check_source(&shapes("2")).expect("checks");
    m.patch(&patched).expect("same declarations");
    assert_ne!(first.epoch, m.epoch, "a patch starts a new epoch");
    let fresh = m.chunk_of(run).expect("exists").expect("compiles");
    assert!(cache_of(&fresh).is_none(), "a new chunk has no cache");
    let again = m.enter(&mut fiber, Rc::clone(&old), vec![]).0;
    assert!(
        matches!(&again, Ok(Value::Str(s)) if &**s == "A2"),
        "{again:?}"
    );
    let refreshed = cache_of(&old).expect("a filled cache");
    assert_eq!(refreshed.epoch, m.epoch, "the old chunk's entry was remade");
}

/// A closure called from Rust gets its captures installed and releases them with the frame.
#[test]
fn a_closure_called_from_rust_runs_with_its_captures() {
    let src = "
fn make(n: int) -> fn(int) -> int {
    let label = \"x\" + \"y\"
    return fn(k) => if label == \"xy\" { n + k + 2 } else { 0 }
}
fn main() {}
";
    let (mut m, program, _out) = load(src);
    let mut fiber = VmFiber::new();
    let f = m
        .call(&mut fiber, fn_id(&program, "make"), vec![Value::Int(10)])
        .expect("makes");
    assert!(matches!(f, Value::Closure(_)));
    let r = m.call_value(
        &mut fiber,
        &f,
        vec![Value::Int(5)],
        colibri_syntax::Span::new(colibri_syntax::FileId(0), 0, 0),
    );
    assert!(matches!(r, Ok(Value::Int(17))), "{r:?}");
}

#[test]
fn native_code_that_calls_user_code_counts_as_native_depth_and_the_closure_built_ins_do_not() {
    // `print` and `sort` call the user's `show` and `cmp` from Rust: no coroutine may pause there
    // (`native_depth`). A call the program makes itself, and a closure of `map` (Colibri code
    // here), are ordinary calls.
    let src = "
struct X { n: int }
impl Show for X { fn show(self) -> str {
    print(\"in show\")
    return \"x\"
} }
impl Ord for X { fn cmp(self, other: X) -> int {
    print(\"in cmp\")
    return self.n - other.n
} }
fn main() {
    let x = X { n: 1 }
    print(x)
    let t = x.show()
    print(t)
    let mut xs = [X { n: 2 }, x]
    xs.sort()
    let ys = [1].map(fn(v) {
        print(\"in map\")
        return v
    })
}
";
    let (mut m, program, out) = load(src);
    let depths = Rc::new(RefCell::new(Vec::new()));
    {
        let depths = Rc::clone(&depths);
        m.print_hook = Some(Rc::new(move |_: &mut Machine, fiber: &mut VmFiber| {
            depths.borrow_mut().push(fiber.state.native_depth);
        }));
    }
    let mut fiber = VmFiber::new();
    m.run_module(&mut fiber).expect("the module runs");
    let main = fn_id(&program, "main");
    m.call(&mut fiber, main, Vec::new()).expect("main runs");
    assert_eq!(
        String::from_utf8_lossy(&out.0.borrow()),
        "in show\nx\nin show\nx\nin cmp\nin map\n"
    );
    // 1 only for the prints inside `show` (run by `print`) and `cmp` (run by `sort`).
    assert_eq!(*depths.borrow(), [0, 1, 0, 0, 1, 0], "one entry per print");
    assert_eq!(fiber.state.native_depth, 0);
}

/// A program whose function `big` has more locals than a frame has registers: the checker accepts
/// such a program (it is made here by adding locals to a checked one), the compiler cannot.
fn program_with_an_uncompilable_function() -> colibri_check::hir::Program {
    let (mut program, _) =
        check_source("fn big() -> int => 1\nfn main() { print(big()) }\n").expect("checks");
    let big = fn_id(&program, "big");
    let body = Rc::get_mut(&mut program.fns[big.0 as usize].body).expect("the only holder");
    let local = colibri_check::hir::LocalInfo {
        name: "x".to_string(),
        ty: colibri_check::types::Ty::Int(colibri_check::types::IntTy::I64),
        mutable: false,
        storage: colibri_check::hir::Storage::Slot,
        span: colibri_syntax::Span::new(colibri_syntax::FileId(0), 0, 0),
    };
    body.locals.extend(std::iter::repeat_n(local, 70_000));
    program
}

fn panic_text(f: impl FnOnce()) -> Option<String> {
    let payload = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).err()?;
    Some(
        payload
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_default(),
    )
}

/// Chunks are compiled on first use, so a function that cannot be compiled is found by the run
/// that calls it; with `set_compile_all` the load finds it, whether or not anything calls it.
#[test]
fn compile_all_finds_an_uncompilable_function_at_load() {
    let program = program_with_an_uncompilable_function();
    let mut lazy = Machine::new(Box::new(Out::default()), 100);
    lazy.load(&program);
    let mut fiber = VmFiber::new();
    let main = fn_id(&program, "main");
    let _ = lazy.call(&mut fiber, main, vec![]);
    let err = lazy.chunk_of(fn_id(&program, "big")).expect("exists");
    assert!(err.is_err(), "the function cannot be compiled");

    let mut eager = Machine::new(Box::new(Out::default()), 100);
    eager.set_compile_all(true);
    let text = panic_text(|| eager.load(&program)).expect("the load fails");
    assert!(text.contains("cannot compile `big`"), "{text}");
    assert!(text.contains("registers"), "{text}");
}

/// The same for a patch: the new bodies are compiled when they are swapped in.
#[test]
fn compile_all_covers_a_patch_and_compiles_what_the_program_has() {
    let (program, _) = check_source("fn big() -> int => 1\nfn main() { print(big()) }\n").unwrap();
    let mut m = Machine::new(Box::new(Out::default()), 100);
    m.set_compile_all(true);
    m.load(&program);
    let big = fn_id(&program, "big");
    assert!(
        m.fns[big.0 as usize].chunk.get().is_some(),
        "compiled at the load, not at a call"
    );
    let bad = program_with_an_uncompilable_function();
    let text = panic_text(|| {
        let _ = m.patch(&bad);
    })
    .expect("the patch fails");
    assert!(text.contains("cannot compile `big`"), "{text}");
}
