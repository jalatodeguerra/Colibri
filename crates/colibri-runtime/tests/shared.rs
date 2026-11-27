//! `Shared[T]` (phase 3 L20, ARCHITECTURE §36.6): a frozen copy that every thread can read. The
//! copy an isolate gets from `get()` is its own, built once and kept; any write to it panics.
//!
//! No test waits on a clock. Every program prints what does not depend on timing, and a program
//! that could hang runs under a minute's limit that only fails the test. Each program runs on a
//! pool of one thread (a task joined at once runs inline in the joiner) and on a pool of three.

use std::io::Write;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use colibri_runtime as colibri;

#[derive(Clone, Default)]
struct Out(Arc<Mutex<Vec<u8>>>);

impl Out {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

impl Write for Out {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Runs `body` on a thread and gives up on it after a minute, so that a deadlock fails a test.
fn within_a_minute<R: Send + 'static>(body: impl FnOnce() -> R + Send + 'static) -> R {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .stack_size(64 * 1024 * 1024)
        .spawn(move || {
            let _ = tx.send(body());
        })
        .unwrap();
    rx.recv_timeout(Duration::from_secs(60))
        .expect("the program deadlocked")
}

/// What `source` prints on a pool of `workers` threads: main and the event loop run to the end.
fn run_on(workers: usize, source: &str) -> String {
    let source = source.to_string();
    within_a_minute(move || {
        let out = Out::default();
        let mut vm = colibri::Vm::builder()
            .worker_threads(workers)
            .stdout_shared(Box::new(out.clone()))
            .build();
        vm.load_source("shared.colibri", &source).unwrap();
        vm.run_main().unwrap();
        vm.run_event_loop().unwrap();
        out.text()
    })
}

/// The same program on a pool of one and of three, which must print the same.
fn same_on_one_and_three(source: &str, want: &str) {
    for workers in [1, 3] {
        for _ in 0..3 {
            assert_eq!(run_on(workers, source), want, "{workers} workers");
        }
    }
}

const FROZEN: &str = "cannot modify a frozen value (it came from `Shared`)";

/// The types the violation programs use.
const TYPES: &str = r#"
struct Pt { x: int, y: int }
struct Holder { n: int }
impl Holder { fn inc(mut self) { self.n += 1 } }
class Node { v: int, items: List[int], pos: Pt }
class Counter { n: int }
impl Counter { fn bump(self) { self.n += 1 } }
fn node() -> Node => Node { v: 1, items: [1, 2], pos: Pt { x: 1, y: 2 } }
fn strs() -> StrBuilder {
    let b = StrBuilder()
    b.push("x")
    b.push("y")
    return b
}
fn bytes_in() -> BytesBuilder {
    let b = BytesBuilder()
    b.push(1)
    b.push(2)
    return b
}
"#;

/// A program that makes a `Shared` of `value`, gets it in a task, and runs `mutation` on it there;
/// the task's `err` is printed, or `no panic` when the mutation went through. The same program on
/// a pool of one and of three prints the message of the frozen value.
fn violation(value: &str, mutation: &str) {
    let source = format!(
        r#"{TYPES}
fn main() {{
    let s = Shared({value})
    let t = task.run(fn() {{
        let mut v = s.get()
        {mutation}
        print("not reached")
    }})
    match t.join() {{
        ok(_) => print("no panic")
        err(e) => print(e.message())
    }}
}}
"#
    );
    let want = format!("the task `<closure in main>` panicked: {FROZEN}\n");
    same_on_one_and_three(&source, &want);
}

#[test]
fn an_index_write_is_refused() {
    violation("[1, 2, 3]", "v[0] = 9");
}

#[test]
fn a_compound_assignment_to_an_element_is_refused() {
    violation("[1, 2, 3]", "v[0] += 1");
}

#[test]
fn push_pop_insert_remove_clear_and_extend_are_refused() {
    violation("[1, 2, 3]", "v.push(4)");
    violation("[1, 2, 3]", "let x = v.pop()");
    violation("[1, 2, 3]", "v.insert(0, 4)");
    violation("[1, 2, 3]", "let x = v.remove(0)");
    violation("[1, 2, 3]", "v.clear()");
    violation("[1, 2, 3]", "v.extend([4])");
}

#[test]
fn the_in_place_built_ins_are_refused() {
    violation("[3, 2, 1]", "v.sort()");
    violation("[3, 2, 1]", "v.sort_by(fn(a, b) => a - b)");
    violation("[3, 2, 1]", "v.reverse()");
    violation("[3, 2, 1]", "v.retain(fn(x) => x > 1)");
    violation("[3, 3, 1]", "v.dedup()");
}

#[test]
fn a_write_to_a_map_is_refused() {
    violation(r#"{"a": 1}"#, r#"v["a"] = 2"#);
    violation(r#"{"a": 1}"#, r#"v["b"] = 2"#);
    violation(r#"{"a": 1}"#, r#"v["a"] += 2"#);
    violation(r#"{"a": 1}"#, r#"v.insert("b", 2)"#);
    violation(r#"{"a": 1}"#, r#"let x = v.remove("a")"#);
    violation(r#"{"a": 1}"#, "v.clear()");
}

#[test]
fn a_write_to_a_set_is_refused() {
    violation("Set([1])", "v.add(2)");
    violation("Set([1])", "v.remove(1)");
    violation("Set([1])", "v.clear()");
}

#[test]
fn a_field_write_of_a_class_object_is_refused() {
    violation("node()", "v.v = 2");
    violation("node()", "v.v += 2");
    violation("node()", "v.pos.x = 5");
    violation("Counter { n: 1 }", "v.bump()");
}

#[test]
fn a_change_of_a_collection_inside_an_object_is_refused() {
    violation("node()", "v.items.push(3)");
    violation("node()", "v.items[0] = 3");
}

#[test]
fn an_update_of_an_element_that_is_a_struct_is_refused() {
    // A struct is a value, but the list that holds it is frozen: it cannot be written back to.
    violation("[Pt { x: 1, y: 2 }]", "v[0].x = 5");
    violation("[Holder { n: 1 }]", "v[0].inc()");
    violation("[Holder { n: 1 }]", "for mut h in v { h.inc() }");
    violation(r#"{"k": Holder { n: 1 }}"#, r#"v["k"].inc()"#);
    violation("[Counter { n: 1 }]", "v[0].bump()");
}

#[test]
fn a_builder_in_a_frozen_copy_cannot_be_added_to_or_cleared() {
    violation("strs()", "v.push(\"y\")");
    violation("strs()", "v.push_char('y')");
    violation("strs()", "v.clear()");
    violation("bytes_in()", "v.push(2)");
    violation("bytes_in()", "v.push_str(\"y\")");
    violation("bytes_in()", "v.push_bytes(Bytes([1, 2]))");
    violation("bytes_in()", "v.clear()");
}

#[test]
fn a_builder_in_a_frozen_copy_can_be_read() {
    same_on_one_and_three(
        &format!(
            r#"{TYPES}
fn main() {{
    let s = Shared((strs(), bytes_in()))
    let r = task.run(fn() -> str {{
        let (a, b) = s.get()
        // Asking for the text twice joins the pieces without keeping the result in the builder.
        return "{{a.to_str()}} {{a.to_str()}} {{a.len()}} {{b.len()}} {{b.to_bytes().len()}}"
    }})
    print(r.join().expect("read"))
}}
"#
        ),
        "xy xy 2 2 2\n",
    );
}

#[test]
fn a_frozen_value_can_be_read_iterated_shown_and_compared() {
    same_on_one_and_three(
        &format!(
            r#"{TYPES}
fn main() {{
    let s = Shared(node())
    let l = Shared([3, 1, 2])
    let m = Shared({{"a": 1, "b": 2}})
    let r = task.run(fn() -> str {{
        let n = s.get()
        let xs = l.get()
        let mut sum = 0
        for x in xs {{
            sum += x
        }}
        // These make new values, and the new values are not frozen.
        let mut sorted = xs.sorted()
        sorted.push(10)
        let mut copy = xs.clone()
        copy.push(11)
        let mut mapped = xs.map(fn(x) => x * 2)
        mapped.push(12)
        let mut keys = m.get().keys()
        keys.push("c")
        return "{{n.v}} {{n.items}} {{n.pos.x}} {{sum}} {{xs}} {{sorted}} {{copy}} {{mapped}} {{keys.len()}} {{m.get()["b"]}} {{xs.contains(2)}} {{xs.len()}}"
    }})
    print(r.join().expect("read"))
}}
"#
        ),
        "1 [1, 2] 1 6 [3, 1, 2] [1, 2, 3, 10] [3, 1, 2, 11] [6, 2, 4, 12] 3 2 true 3\n",
    );
}

#[test]
fn the_main_thread_gets_a_frozen_copy_too() {
    // The program that made the `Shared` cannot change its copy either, and its own value is its own.
    let source = format!(
        r#"{TYPES}
fn main() {{
    let mut mine = [1, 2, 3]
    let s = Shared(mine)
    mine.push(4)
    print("mine {{mine}}, shared {{s.get()}}")
    let mut v = s.get()
    v.push(5)
}}
"#
    );
    let out = Out::default();
    let mut vm = colibri::Vm::builder()
        .stdout_shared(Box::new(out.clone()))
        .build();
    vm.load_source("shared.colibri", &source).unwrap();
    let err = vm.run_main().unwrap_err();
    assert!(err.to_string().contains(FROZEN), "{err}");
    assert_eq!(out.text(), "mine [1, 2, 3, 4], shared [1, 2, 3]\n");
}

#[test]
fn get_gives_the_same_object_every_time_in_an_isolate_and_not_across_isolates() {
    // `==` on classes is identity: two `get`s in one isolate are one object.
    same_on_one_and_three(
        &format!(
            r#"{TYPES}
fn main() {{
    let s = Shared(node())
    let here = s.get() == s.get()
    let t = task.run(fn() => (s.get() == s.get(), s.get().v))
    let (same, v) = t.join().expect("task")
    // Another handle of the same graph gets the same copy.
    let again = s.clone()
    let same_handle = again.get() == s.get()
    print("{{here}} {{same}} {{v}} {{same_handle}}")
}}
"#
        ),
        "true true 1 true\n",
    );
}

#[test]
fn the_copies_of_two_tasks_are_separate_objects_and_a_task_cannot_change_what_another_sees() {
    // Each task sees the list as it was made. Neither can change it, and a copy it made with
    // `clone` is its own.
    same_on_one_and_three(
        r#"
fn work(s: Shared[List[int]], k: int) -> int {
    let mut mine = s.get().clone()
    mine.push(k)
    let mut total = 0
    for x in mine {
        total += x
    }
    return total
}

fn main() {
    let s = Shared([1, 2, 3])
    let mut tasks = []
    for k in 0..6 {
        tasks.push(task.run(fn() => work(s, k)))
    }
    let mut out = []
    for t in tasks {
        out.push(t.join().expect("task"))
    }
    print("{out} {s.get()}")
}
"#,
        "[6, 7, 8, 9, 10, 11] [1, 2, 3]\n",
    );
}

#[test]
fn a_task_that_returns_its_frozen_copy_gives_the_joiner_a_copy_it_can_change() {
    same_on_one_and_three(
        r#"
fn main() {
    let s = Shared([1, 2, 3])
    let mut got = task.run(fn() => s.get()).join().expect("task")
    got.push(4)
    print("{got} {s.get()}")
}
"#,
        "[1, 2, 3, 4] [1, 2, 3]\n",
    );
}

#[test]
fn nested_tasks_share_one_handle_on_either_pool() {
    // The inner tasks of `outer` read the same `Shared` as `outer` does. On a pool of one they
    // run inline in `outer`'s isolate, and see its copy; on three they may not. The results do
    // not tell.
    same_on_one_and_three(
        r#"
fn outer(s: Shared[List[int]]) -> int {
    let a = task.run(fn() => s.get().len())
    let b = task.run(fn() => s.get()[1])
    return s.get()[0] + a.join().expect("a") + b.join().expect("b")
}

fn main() {
    let s = Shared([10, 20, 30])
    print("{task.run(fn() => outer(s)).join().expect("outer")}")
}
"#,
        "33\n",
    );
}

#[test]
fn a_shared_in_a_channel_message_is_the_same_graph_on_the_other_side() {
    same_on_one_and_three(
        r#"
fn main() -> Result[()] {
    let words = Shared(["alpha", "beta", "gamma"])
    let jobs = Channel[(Shared[List[str]], int)]()
    let out = Channel[str]()
    let worker = task.run(fn() {
        for job in jobs {
            let (table, i) = job
            out.send(table.get()[i].to_upper())
        }
        out.close()
    })
    for i in 0..3 {
        // A `Shared` needs no `.clone()` to be sent: it is not copied.
        jobs.send((words, i))
    }
    jobs.close()
    for w in out {
        print(w)
    }
    worker.join()?
    return ok(())
}
"#,
        "ALPHA\nBETA\nGAMMA\n",
    );
}

#[test]
fn a_shared_sent_alone_and_twice_in_one_message_is_one_handle_on_the_other_side() {
    same_on_one_and_three(
        r#"
fn main() -> Result[()] {
    let s = Shared([1, 2, 3])
    let ch = Channel[Shared[List[int]]](capacity: 2)
    let pair = Channel[(Shared[List[int]], Shared[List[int]])](capacity: 2)
    ch.send(s)
    pair.send((s, s))
    let t = task.run(fn() -> str {
        let one = ch.recv().expect("one")
        let (a, b) = pair.recv().expect("two")
        // Reached twice in one message: one handle, so one copy.
        return "{one.get().len()} {a.get() == b.get()}"
    })
    print(t.join()?)
    return ok(())
}
"#,
        "3 true\n",
    );
}

#[test]
fn a_shared_holds_another_shared_and_a_channel() {
    same_on_one_and_three(
        r#"
fn main() {
    let inner = Shared([1, 2, 3])
    let ch = Channel[int](capacity: 4)
    let outer = Shared((inner, ch))
    let t = task.run(fn() {
        let (i, c) = outer.get()
        c.send(i.get().len())
    })
    t.join().expect("task")
    print("{ch.recv()}")
}
"#,
        "some(3)\n",
    );
}

#[test]
fn a_struct_with_a_shared_field_crosses_with_the_handle_not_a_copy() {
    same_on_one_and_three(
        r#"
struct Config {
    name: str
    table: Shared[Map[str, int]]
}

fn lookup(c: Config, k: str) -> int {
    return c.table.get()[k]
}

fn main() {
    let c = Config { name: "c", table: Shared({"a": 1, "b": 2}) }
    let a = task.run(fn() => lookup(c, "a"))
    let b = task.run(fn() => lookup(c, "b"))
    print("{a.join().expect("a")} {b.join().expect("b")} {c.name}")
}
"#,
        "1 2 c\n",
    );
}

#[test]
fn a_clone_of_a_shared_is_the_same_handle_and_a_clone_of_its_value_is_mutable() {
    same_on_one_and_three(
        r#"
fn lend(s: Shared[List[int]]) -> int {
    // The clone is the same handle, and it dies here without taking the graph with it.
    let t = s.clone()
    return t.get().len()
}

fn main() {
    let s = Shared([1, 2, 3])
    let t = s.clone()
    let mut c = t.get().clone()
    c.push(4)
    print("{c} {s.get()} {t.get() == s.get()} {lend(s)} {lend(s)} {s.get()}")
}
"#,
        "[1, 2, 3, 4] [1, 2, 3] true 3 3 [1, 2, 3]\n",
    );
}

#[test]
fn a_shared_closure_runs_in_every_isolate() {
    same_on_one_and_three(
        r#"
fn main() {
    let base = 100
    let table = [1, 2, 3]
    let s = Shared(fn(x: int) => base + table[x])
    let t = task.run(fn() => s.get()(2))
    print("{s.get()(0)} {t.join().expect("task")}")
}
"#,
        "101 103\n",
    );
}

#[test]
fn a_closure_that_captures_a_mutable_variable_cannot_be_shared() {
    // Whether it changes the variable or only reads it: the variable is a cell the closure shares
    // with its function, and a frozen copy cannot have one.
    let source = r#"
fn main() {
    let mut n = 0
    n += 1
    let s = Shared(fn() => n + 1)
    print("not reached")
}
"#;
    let out = Out::default();
    let mut vm = colibri::Vm::builder()
        .stdout_shared(Box::new(out.clone()))
        .build();
    vm.load_source("shared.colibri", source).unwrap();
    let err = vm.run_main().unwrap_err();
    assert!(
        err.to_string()
            .contains("a `Shared` cannot hold a closure that captures a `let mut` variable"),
        "{err}"
    );
    assert_eq!(out.text(), "");
}

#[test]
fn a_shared_made_in_a_task_comes_back_as_the_same_graph() {
    same_on_one_and_three(
        r#"
fn build(n: int) -> Shared[List[int]] {
    let mut xs = []
    for i in 0..n {
        xs.push(i * i)
    }
    return Shared(xs)
}

fn main() {
    let s = task.run(fn() => build(5)).join().expect("task")
    let again = task.run(fn() => s.get().len()).join().expect("task")
    print("{s.get()} {again}")
}
"#,
        "[0, 1, 4, 9, 16] 5\n",
    );
}

#[test]
fn a_task_reads_a_shared_after_the_program_has_let_go_of_its_own_handle() {
    // `start` makes the `Shared` and returns before the task has read it: the task's own handle is
    // the last one, and the graph lives as long as that.
    same_on_one_and_three(
        r#"
fn start(gate: Channel[int]) -> Task[int] {
    let s = Shared([4, 5, 6])
    return task.run(fn() -> int {
        gate.recv()
        return s.get()[2]
    })
}

fn main() {
    let gate = Channel[int](capacity: 1)
    let t = start(gate)
    gate.send(1)
    print("{t.join().expect("task")}")
}
"#,
        "6\n",
    );
}

#[test]
fn many_tasks_on_a_big_shared_agree_on_a_pool_of_one_and_of_three() {
    same_on_one_and_three(
        r#"
fn count(s: Shared[Map[int, int]], lo: int, hi: int) -> int {
    let m = s.get()
    let mut n = 0
    for i in lo..hi {
        if m.contains_key(i) {
            n += m[i]
        }
    }
    return n
}

fn main() {
    let mut table: Map[int, int] = {:}
    for i in 0..2000 {
        table[i] = i % 7
    }
    let s = Shared(table)
    let mut tasks = []
    for k in 0..10 {
        tasks.push(task.run(fn() => count(s, k * 200, (k + 1) * 200)))
    }
    let mut total = 0
    for t in tasks {
        total += t.join().expect("task")
    }
    print("{total}")
}
"#,
        &format!("{}\n", (0..2000).map(|i| i % 7).sum::<i64>()),
    );
}

#[test]
fn a_shared_is_shown_as_a_handle_and_its_value_is_not_part_of_it() {
    same_on_one_and_three(
        r#"
fn main() {
    let s = Shared([1, 2, 3])
    print("{s}")
}
"#,
        "<Shared>\n",
    );
}

/// Runs `source` on `workers` threads to the end of the Vm, so that what a worker's copy prints
/// when it is freed at shutdown is in the output. Gives what `run_main` returned and the output.
fn run_to_the_end(workers: usize, source: &str) -> (Result<(), String>, String) {
    let source = source.to_string();
    within_a_minute(move || {
        let out = Out::default();
        let mut vm = colibri::Vm::builder()
            .worker_threads(workers)
            .stdout_shared(Box::new(out.clone()))
            .build();
        vm.load_source("shared.colibri", &source).unwrap();
        let main = vm.run_main().map_err(|e| e.to_string());
        if main.is_ok() {
            vm.run_event_loop().unwrap();
        }
        drop(vm);
        (main, out.text())
    })
}

const OWN_FIELD_HOOK: &str = r#"
class Res { n: int }
impl Drop for Res {
    fn drop(self) {
        print("drop {self.n}")
        self.n = 0
        print("wrote {self.n}")
    }
}
"#;

#[test]
fn a_drop_hook_may_write_the_fields_of_its_own_frozen_object() {
    for workers in [1, 3] {
        // The copy that main reads. The hook of the original (not frozen) runs when it is
        // copied into the `Shared`, and the hook of the frozen copy when the handle dies.
        let main = format!(
            "{OWN_FIELD_HOOK}\nfn main() {{\n    let s = Shared(Res {{ n: 7 }})\n    print(\"got {{s.get().n}}\")\n}}\n"
        );
        let (result, text) = run_to_the_end(workers, &main);
        assert_eq!(result, Ok(()), "{workers} workers");
        assert_eq!(
            text, "drop 7\nwrote 0\ngot 7\ndrop 7\nwrote 0\n",
            "{workers} workers"
        );
        // The copy that a task reads, freed when the Vm ends.
        let worker = format!(
            "{OWN_FIELD_HOOK}\nfn main() {{\n    let s = Shared(Res {{ n: 8 }})\n    let n = task.run(fn() => s.get().n).join().expect(\"task\")\n    print(\"got {{n}}\")\n}}\n"
        );
        let (result, text) = run_to_the_end(workers, &worker);
        assert_eq!(result, Ok(()), "{workers} workers");
        assert_eq!(
            text, "drop 8\nwrote 0\ngot 8\ndrop 8\nwrote 0\n",
            "{workers} workers"
        );
    }
}

const CHILD_HOOK: &str = r#"
class Res { items: List[int] }
impl Drop for Res {
    fn drop(self) {
        print("drop {self.items.len()}")
        self.items.push(9)
        print("pushed")
    }
}
"#;

#[test]
fn a_drop_hook_that_writes_a_frozen_child_still_panics_where_main_sees_it_and_a_worker_does_not() {
    for workers in [1, 3] {
        // On main the hook's panic is the result of `run_main`.
        let main = format!(
            "{CHILD_HOOK}\nfn main() {{\n    let s = Shared(Res {{ items: [1, 2] }})\n    print(\"got {{s.get().items.len()}}\")\n}}\n"
        );
        let (result, text) = run_to_the_end(workers, &main);
        let err = result.expect_err("the hook of the frozen copy panics");
        assert!(err.contains(FROZEN), "{workers} workers: {err}");
        assert!(text.contains("got 2\ndrop 2\n"), "{text}");
        assert_eq!(text.matches("pushed").count(), 1, "{text}");
        // In a worker the panic is dropped: the hook stops before `pushed`, and the program is
        // not an error.
        let worker = format!(
            "{CHILD_HOOK}\nfn main() {{\n    let s = Shared(Res {{ items: [1, 2, 3] }})\n    let n = task.run(fn() => s.get().items.len()).join().expect(\"task\")\n    print(\"got {{n}}\")\n}}\n"
        );
        let (result, text) = run_to_the_end(workers, &worker);
        assert_eq!(result, Ok(()), "{workers} workers");
        assert_eq!(
            text.matches("drop 3").count(),
            2,
            "{workers} workers: {text}"
        );
        assert_eq!(text.matches("pushed").count(), 1, "{text}");
    }
}
