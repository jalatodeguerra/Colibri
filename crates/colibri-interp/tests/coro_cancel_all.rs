//! `cancel_all_coroutines` called from a host function while a coroutine runs, and what the
//! slab of coroutine stacks must keep true meanwhile. The call must change nothing (it returns at
//! once, as it always did): the stacks that suspended coroutines own stay where they are.

use colibri_core::prelude::*;
use std::cell::RefCell;
use std::io::Write;
use std::rc::Rc;

use colibri_check::HostEnv;
use colibri_check::hir::HostFnId;
use colibri_check::host::{body_hash, header_line};
use colibri_interp::{Fiber, Interp, Value};
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

fn with_poke(src: &str) -> (Interp, colibri_check::Program, Out) {
    let body = "fn poke()\n";
    let text = format!("{}\n{body}", header_line("host", &body_hash(body), &[]));
    let env = HostEnv::from_text(&text).expect("the interface parses");
    let (ast, diags) = parse_file(FileId(0), src);
    assert!(diags.is_empty(), "{diags:?}");
    let checked = colibri_check::check_with_host(&ast, &env, 1 << 20);
    let program = checked
        .program
        .unwrap_or_else(|| panic!("{:?}", checked.diagnostics));
    let out = Out::default();
    let mut interp = Interp::new(Box::new(out.clone()), 200);
    interp.set_native_stack_budget(64 << 20);
    interp.load(&program);
    // A host function that cancels every coroutine, called from inside one.
    interp.set_host_fn(
        HostFnId(0),
        Rc::new(
            |interp: &mut Interp, _fiber: &mut Fiber, _args: Vec<Value>| {
                interp.cancel_all_coroutines();
                Ok(Value::Unit)
            },
        ),
    );
    (interp, program, out)
}

/// While a coroutine runs, `cancel_all_coroutines` must leave the scheduler and the stacks as
/// they were.
#[test]
fn cancel_all_from_inside_a_coroutine_changes_nothing() {
    let src = "fn worker() {\n    wait(1.0)\n    print(\"worker\")\n}\nfn body() {\n    poke()\n    wait(0.5)\n    print(\"body\")\n}\nfn main() {\n    let a = go worker()\n    let b = go body()\n}\n";
    let (mut interp, program, out) = with_poke(src);
    let mut fiber = Fiber::new();
    let main = program.fn_by_name("main").expect("main");
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        interp.call(&mut fiber, main, vec![]).map(|_| ())
    }));
    let msg = r.as_ref().err().map(|p| {
        p.downcast_ref::<String>()
            .cloned()
            .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_default()
    });
    assert!(r.is_ok(), "the call panicked in Rust: {msg:?}");
    assert!(r.unwrap().is_ok());
    assert_eq!(interp.core().sched.coroutine_count(), 2);
    for _ in 0..3 {
        interp.tick(&mut fiber, 0.5).expect("tick");
    }
    assert_eq!(interp.core().sched.coroutine_count(), 0);
    let printed = String::from_utf8_lossy(&out.0.borrow()).into_owned();
    assert_eq!(printed, "body\nworker\n");
}

/// Same, but the coroutine that cancels then starts another one before it pauses: the new
/// stack must not take the slot of a coroutine that is still suspended.
#[test]
fn a_start_after_cancel_all_from_inside_does_not_share_a_slot() {
    let src = "fn worker(name: str, s: float) {\n    wait(s)\n    print(name)\n}\nfn body() {\n    poke()\n    let c = go worker(\"c\", 2.0)\n    wait(0.5)\n    print(\"body\")\n}\nfn main() {\n    let a = go worker(\"a\", 1.0)\n    let b = go body()\n}\n";
    let (mut interp, program, out) = with_poke(src);
    let mut fiber = Fiber::new();
    let main = program.fn_by_name("main").expect("main");
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        interp.call(&mut fiber, main, vec![]).map(|_| ())
    }));
    assert!(r.is_ok(), "the call panicked in Rust");
    for _ in 0..5 {
        let t = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            interp.tick(&mut fiber, 0.5)
        }));
        assert!(t.is_ok(), "a tick panicked in Rust");
    }
    let printed = String::from_utf8_lossy(&out.0.borrow()).into_owned();
    assert_eq!(printed, "body\na\nc\n");
}

/// The cancelling coroutine paused first, so its slot is not the last one: starts after the
/// slab was emptied land on slots that suspended entries still name.
#[test]
fn slots_named_by_live_entries_are_not_handed_out_again() {
    let src = "fn worker(name: str, s: float) {\n    wait(s)\n    print(name)\n}\nfn body() {\n    wait(0.1)\n    poke()\n    let c = go worker(\"c\", 3.0)\n    let d = go worker(\"d\", 4.0)\n    wait(0.1)\n    print(\"body\")\n}\nfn main() {\n    let a = go worker(\"a\", 1.0)\n    let b = go body()\n}\n";
    let (mut interp, program, out) = with_poke(src);
    let mut fiber = Fiber::new();
    let main = program.fn_by_name("main").expect("main");
    interp.call(&mut fiber, main, vec![]).expect("main runs");
    for _ in 0..12 {
        let t = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            interp.tick(&mut fiber, 0.5)
        }));
        assert!(t.is_ok(), "a tick panicked in Rust");
    }
    let printed = String::from_utf8_lossy(&out.0.borrow()).into_owned();
    assert_eq!(printed, "a\nbody\nc\nd\n");
}
