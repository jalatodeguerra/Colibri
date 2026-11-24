//! The slab of coroutine fibers: slot reuse, nested starts, no leak over many cycles, a double
//! free, a lost fiber, and resuming a finished entry (ported from the tree-walker's slot tests).

use std::cell::RefCell;
use std::io::Write;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use colibri_core::coro::Sched;
use colibri_vm::{FiberSlab, Machine, VmFiber};

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

impl Out {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.borrow()).into_owned()
    }
}

/// Loads `src` (checked, no host functions) into a fresh machine.
fn loaded(src: &str, out: Box<dyn Write>) -> (Machine, colibri_check::Program) {
    let (ast, diags) = colibri_syntax::parse_file(colibri_syntax::FileId(0), src);
    assert!(diags.is_empty(), "{diags:?}");
    let checked = colibri_check::check(&ast);
    let program = checked
        .program
        .unwrap_or_else(|| panic!("{:?}", checked.diagnostics));
    let mut machine = Machine::new(out, 200);
    machine.set_native_stack_budget(64 << 20);
    machine.load(&program);
    (machine, program)
}

fn call(m: &mut Machine, p: &colibri_check::Program, name: &str) {
    let mut fiber = VmFiber::new();
    m.call(&mut fiber, p.fn_by_name(name).expect(name), vec![])
        .unwrap_or_else(|e| panic!("{name}: {}", e.message));
}

/// Two coroutines suspended at once hold two slots; when one ends, its slot is reused by the next
/// start while the other keeps its own, and each resumes where it paused.
#[test]
fn a_freed_slot_is_reused_and_a_suspended_neighbour_keeps_its_own() {
    let src = "fn slow(tag: str) {\n    wait(2.0)\n    print(tag)\n}\nfn quick(tag: str) {\n    wait(0.5)\n    print(tag)\n}\nfn first() {\n    let _a = go slow(\"slow\")\n    let _b = go quick(\"quick\")\n}\nfn second() {\n    let _c = go quick(\"again\")\n}\n";
    let out = Out::default();
    let (mut m, p) = loaded(src, Box::new(out.clone()));
    call(&mut m, &p, "first");
    assert_eq!(m.coroutine_slots_live(), 2, "two coroutines, two slots");
    m.tick(1.0).expect("tick");
    assert_eq!(out.text(), "quick\n");
    assert_eq!(
        m.coroutine_slots_live(),
        1,
        "the quick one gave its slot back"
    );
    call(&mut m, &p, "second");
    assert_eq!(m.coroutine_slots_live(), 2);
    assert_eq!(m.coroutine_slots_made(), 2, "the freed slot was reused");
    for _ in 0..4 {
        m.tick(0.5).expect("tick");
    }
    assert_eq!(out.text(), "quick\nagain\nslow\n");
    assert_eq!(m.coroutine_slots_live(), 0);
}

/// A coroutine that starts another while it runs: its own fiber is out of the slab then, and the
/// slab grows before the fiber goes back into its slot.
#[test]
fn a_nested_start_while_a_fiber_is_out_of_its_slot() {
    let src = "fn leaf(n: int) {\n    wait(0.5)\n    print(n)\n}\nfn parent() {\n    let mut i = 0\n    while i < 40 {\n        let _h = go leaf(i)\n        i = i + 1\n    }\n    wait(0.1)\n    print(\"parent\")\n}\nfn main() {\n    let _p = go parent()\n}\n";
    let out = Out::default();
    let (mut m, p) = loaded(src, Box::new(out.clone()));
    call(&mut m, &p, "main");
    assert_eq!(
        m.coroutine_slots_live(),
        41,
        "the parent and its forty leaves"
    );
    for _ in 0..4 {
        m.tick(0.5).expect("tick");
    }
    let text = out.text();
    assert!(text.starts_with("parent\n0\n1\n"), "{text}");
    assert_eq!(text.lines().count(), 41);
    assert_eq!(m.coroutine_slots_live(), 0);
}

/// Many start and finish cycles keep the slab at the size of the most that lived at once.
#[test]
fn slots_are_freed_after_many_start_and_finish_cycles() {
    let src = "fn work() {\n    wait(0.0)\n}\nfn main() {\n    let mut i = 0\n    while i < 100 {\n        let h = go work()\n        h.join()\n        i = i + 1\n    }\n}\n";
    let (mut m, p) = loaded(src, Box::new(std::io::sink()));
    call(&mut m, &p, "main");
    assert_eq!(m.coroutine_slots_live(), 0, "every slot was freed");
    assert!(
        m.coroutine_slots_made() <= 2,
        "the slab has {} slots after 100 cycles",
        m.coroutine_slots_made()
    );
    assert_eq!(
        m.coroutine_fibers_pooled(),
        1,
        "one emptied fiber is reused"
    );
}

/// Freeing a slot twice would hand it to two coroutines: debug builds stop it at the second
/// `free`.
#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "freed twice")]
fn freeing_a_slot_twice_is_caught() {
    let mut slab = FiberSlab::default();
    let slot = slab.insert(Box::new(VmFiber::new()));
    drop(slab.take(slot));
    slab.free(slot);
    slab.free(slot);
}

/// A slot freed while its fiber is still in it is a bug too.
#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "still holds a fiber")]
fn freeing_a_slot_that_holds_a_fiber_is_caught() {
    let mut slab = FiberSlab::default();
    let slot = slab.insert(Box::new(VmFiber::new()));
    slab.free(slot);
}

/// If the engine cannot find the fiber of a suspended coroutine (`Resumed::Gone`, a broken
/// invariant), the coroutine ends as a failure with an internal error instead of staying
/// suspended: the next tick reports it, and `cancel_all_coroutines` terminates.
#[test]
fn a_lost_fiber_ends_its_coroutine_as_an_internal_error() {
    let src = "fn worker() {\n    wait(1.0)\n    print(\"never\")\n}\nfn main() {\n    let _a = go worker()\n}\n";
    let out = Out::default();
    let (mut m, p) = loaded(src, Box::new(out.clone()));
    call(&mut m, &p, "main");
    assert_eq!(
        colibri_core::engine::Engine::core(&m)
            .sched
            .coroutine_count(),
        1
    );
    assert!(m.lose_coroutine_fiber(0), "the worker's fiber");
    // If the lost coroutine stayed suspended, the cancel below would never end: a watchdog turns
    // that hang into a failure.
    let done = Arc::new(AtomicUsize::new(0));
    let flag = Arc::clone(&done);
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(20));
        if flag.load(Ordering::SeqCst) == 0 {
            let _ = std::io::Write::write_all(
                &mut std::io::stderr(),
                b"a_lost_fiber_ends_its_coroutine_as_an_internal_error: cancel_all_coroutines hangs\n",
            );
            std::process::exit(1);
        }
    });
    let failure = m.tick(2.0).expect_err("the tick reports it");
    assert!(
        failure.message.contains("internal error"),
        "{}",
        failure.message
    );
    assert_eq!(
        colibri_core::engine::Engine::core(&m)
            .sched
            .coroutine_count(),
        0,
        "the coroutine is over"
    );
    assert_eq!(out.text(), "");
    m.cancel_all_coroutines();
    done.store(1, Ordering::SeqCst);
}

/// Asking the scheduler to resume a coroutine that already finished (its entry is not yet
/// compacted away) changes nothing: it does not reach the engine, which would report the fiber
/// as lost and turn the finished coroutine into a failure.
#[test]
fn resuming_a_finished_entry_does_nothing() {
    let src = "fn quick() {\n    print(\"quick\")\n}\nfn main() {\n    let _a = go quick()\n}\n";
    let (mut m, p) = loaded(src, Box::new(std::io::sink()));
    call(&mut m, &p, "main");
    assert_eq!(m.coroutine_slots_live(), 0, "the coroutine finished");
    // The first coroutine has id 1.
    Sched::resume_coro(&mut m, 1, false, true);
    assert!(m.take_coroutine_failure().is_none());
    m.tick(1.0).expect("a later tick still works");
}

/// The pool of emptied fibers is capped: a burst of forty coroutines that all end leaves sixteen
/// of their fibers for the next starts, and the slots of all forty are free.
#[test]
fn the_pool_of_emptied_fibers_is_capped() {
    let src = "fn work() {\n    wait(0.5)\n}\nfn main() {\n    let mut i = 0\n    while i < 40 {\n        let _h = go work()\n        i = i + 1\n    }\n}\n";
    let (mut m, p) = loaded(src, Box::new(std::io::sink()));
    call(&mut m, &p, "main");
    assert_eq!(m.coroutine_slots_live(), 40);
    m.tick(1.0).expect("tick");
    assert_eq!(m.coroutine_slots_live(), 0);
    assert_eq!(m.coroutine_slots_made(), 40);
    assert_eq!(m.coroutine_fibers_pooled(), 16, "the pool holds at most 16");
}

/// A fiber that ended by a cancellation (through its pads) is emptied like any other: the next
/// coroutine reuses it and runs normally, in the same slot.
#[test]
fn a_cancelled_fiber_is_recycled_and_reused() {
    let src = "fn sleeper() {\n    defer print(\"cleanup\")\n    wait(5.0)\n}\nfn quick() {\n    wait(0.0)\n    print(\"quick\")\n}\nfn main() {\n    let h = go sleeper()\n    h.cancel()\n    let k = go quick()\n    k.join()\n}\n";
    let out = Out::default();
    let (mut m, p) = loaded(src, Box::new(out.clone()));
    call(&mut m, &p, "main");
    assert_eq!(out.text(), "cleanup\nquick\n");
    assert_eq!(m.coroutine_slots_live(), 0);
    assert_eq!(
        m.coroutine_slots_made(),
        1,
        "the second coroutine took the slot of the cancelled one"
    );
    assert_eq!(m.coroutine_fibers_pooled(), 1);
}

/// The call-depth limit holds on a fiber that was used before: a fiber taken from the pool
/// starts with no frames counted, overflows at the limit, and is usable again afterwards.
#[test]
fn the_call_depth_limit_holds_on_a_reused_fiber() {
    let src = "fn deep(n: int) -> int {\n    if n == 0 {\n        return 0\n    }\n    return 1 + deep(n - 1)\n}\nfn work(n: int) -> int {\n    return deep(n)\n}\nfn main() {\n    let a = go work(10)\n    print(a.join())\n    let b = go work(1000)\n    let r = b.join_result()\n    print(r.is_err())\n    let c = go work(10)\n    print(c.join())\n}\n";
    let out = Out::default();
    let (mut m, p) = loaded(src, Box::new(out.clone()));
    call(&mut m, &p, "main");
    assert_eq!(out.text(), "10\ntrue\n10\n");
    assert_eq!(m.coroutine_slots_made(), 1, "all three used the one fiber");
}
