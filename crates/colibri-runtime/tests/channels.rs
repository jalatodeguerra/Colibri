//! Channels (phase 3 L19, ARCHITECTURE §36.5): a handle that crosses to a task as it is, a channel
//! sent through a channel, the three ways of waiting, and what happens to a value whose receiver
//! gave up.
//!
//! No test waits on a clock. Every program prints what does not depend on timing, and a program
//! that could hang runs under a minute's limit that only fails the test.

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

/// What `source` prints on a pool of `workers` threads, main and event loop run to the end.
fn run_on(workers: usize, source: &str) -> String {
    let source = source.to_string();
    within_a_minute(move || {
        let out = Out::default();
        let mut vm = colibri::Vm::builder()
            .worker_threads(workers)
            .stdout_shared(Box::new(out.clone()))
            .build();
        vm.load_source("channels.colibri", &source).unwrap();
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

#[test]
fn a_channel_handle_captured_by_a_task_is_the_same_channel_on_both_sides() {
    // The closure captures `ch`; the handle crosses as it is, so the task's sends reach the
    // receiver that the main fiber holds, and a send from main reaches the task.
    same_on_one_and_three(
        r#"
fn main() -> Result[()] {
    let to_task = Channel[int]()
    let back = Channel[int]()
    let t = task.run(fn() {
        for n in to_task {
            back.send(n * 2)
        }
        back.close()
    })
    for i in 1..=4 {
        to_task.send(i)
    }
    to_task.close()
    let mut got: List[int] = []
    for v in back {
        got.push(v)
    }
    t.join()?
    print("{got}")
    return ok(())
}
"#,
        "[2, 4, 6, 8]\n",
    );
}

#[test]
fn a_channel_sent_through_a_channel_is_the_same_channel() {
    // A request carries its own reply channel: the worker answers on the handle it received.
    same_on_one_and_three(
        r#"
fn main() -> Result[()] {
    let requests = Channel[(int, Channel[str])]()
    let server = task.run(fn() {
        for req in requests {
            let (n, reply) = req
            reply.send("answer {n * n}")
            reply.close()
        }
    })
    for n in 1..=3 {
        let reply = Channel[str]()
        requests.send((n, reply))
        print(reply.recv().expect("an answer"))
        print("closed after: {reply.recv().is_none()}")
    }
    requests.close()
    server.join()?
    return ok(())
}
"#,
        "answer 1\nclosed after: true\nanswer 4\nclosed after: true\nanswer 9\nclosed after: true\n",
    );
}

#[test]
fn a_channel_in_a_value_twice_is_one_channel_after_the_copy() {
    // The same handle twice in one value: the copy keeps them as one channel, so a send on one
    // is a receive on the other.
    same_on_one_and_three(
        r#"
fn main() -> Result[()] {
    let pairs = Channel[(Channel[int], Channel[int])]()
    let ch = Channel[int](capacity: 1)
    pairs.send((ch, ch))
    let (a, b) = pairs.recv().expect("a pair")
    a.send(7)
    print("{b.recv()}")
    ch.send(8)
    print("{a.recv()}")
    // A close through one is seen through the other, and through the original.
    a.close()
    print("{b.recv()} {ch.try_recv()} {ch.recv()}")
    return ok(())
}
"#,
        "some(7)\nsome(8)\nnone none none\n",
    );
}

#[test]
fn a_task_run_inline_waits_on_a_channel_the_same_on_one_worker_and_three() {
    // `outer` fills the channel and then joins `inner`, which on a pool of one runs inline in
    // `outer`'s own thread, and on three may run elsewhere. Both receive what is queued.
    same_on_one_and_three(
        r#"
fn inner(ch: Channel[int], out: Channel[int]) {
    for v in ch {
        out.send(v + 100)
    }
}

fn outer() -> List[int] {
    let ch = Channel[int](capacity: 8)
    let out = Channel[int](capacity: 8)
    for i in 0..5 {
        ch.send(i)
    }
    ch.close()
    let t = task.run(fn() => inner(ch, out))
    t.join().expect("inner")
    out.close()
    let mut got: List[int] = []
    for v in out {
        got.push(v)
    }
    return got
}

fn main() {
    print("{task.run(outer).join().expect("outer")}")
}
"#,
        "[100, 101, 102, 103, 104]\n",
    );
}

#[test]
fn two_tasks_run_inline_in_a_third_feed_each_other_through_a_channel_on_both_pools() {
    // `outer` starts a feeder and a summer and joins them in the order that lets each one finish
    // by itself: on a pool of one both run inline in `outer`'s thread, one after the other; on
    // three they may run apart. (A task that waits on a channel keeps its thread, so a program
    // that needs the other task to run on the same single thread is a deadlock; see the notes.)
    same_on_one_and_three(
        r#"
fn feeder(ch: Channel[int]) {
    for i in 0..3 {
        ch.send(i)
    }
    ch.close()
}

fn outer() -> int {
    let ch = Channel[int]()
    let a = task.run(fn() => feeder(ch))
    let b = task.run(fn() {
        let mut sum = 0
        for v in ch {
            sum += v
        }
        return sum
    })
    a.join().expect("feeder")
    return b.join().expect("summer")
}

fn main() {
    print("sum {task.run(outer).join().expect("outer")}")
}
"#,
        "sum 3\n",
    );
}

#[test]
fn a_coroutine_that_waits_on_a_channel_lets_the_others_run_and_gets_its_value() {
    same_on_one_and_three(
        r#"
let mut log: List[str] = []

fn waiter(ch: Channel[int]) {
    let v = ch.recv().expect("a value")
    log.push("waiter got {v}")
}

fn other() {
    log.push("other ran")
}

fn main() {
    let ch = Channel[int]()
    let w = go waiter(ch)
    go other()
    wait(0.001)
    log.push("main sends")
    ch.send(5)
    w.join()
    for l in log {
        print(l)
    }
}
"#,
        "other ran\nmain sends\nwaiter got 5\n",
    );
}

#[test]
fn a_coroutine_that_joins_an_inline_task_and_receives_from_it_works_on_both_pools() {
    // The joiner is a coroutine; the task it joins sends into a channel the coroutine then reads.
    same_on_one_and_three(
        r#"
fn produce(ch: Channel[int]) {
    ch.send(1)
    ch.send(2)
    ch.close()
}

fn reader() {
    let ch = Channel[int](capacity: 4)
    let t = task.run(fn() => produce(ch))
    t.join().expect("produce")
    let mut got: List[int] = []
    for v in ch {
        got.push(v)
    }
    print("reader {got}")
}

fn main() {
    let r = go reader()
    r.join()
}
"#,
        "reader [1, 2]\n",
    );
}

#[test]
fn a_value_handed_to_a_coroutine_that_is_cancelled_is_not_lost() {
    same_on_one_and_three(
        r#"
fn waiter(ch: Channel[int]) {
    let v = ch.recv()
    print("never printed {v}")
}

fn main() {
    let ch = Channel[int]()
    let w = go waiter(ch)
    // Let it begin to wait.
    wait(0.001)
    ch.send(42)
    w.cancel()
    wait(0.001)
    print("{ch.try_recv()}")
    print("{ch.try_recv()}")
}
"#,
        "some(42)\nnone\n",
    );
}

#[test]
fn a_coroutine_that_is_cancelled_while_waiting_leaves_the_channel_usable() {
    same_on_one_and_three(
        r#"
fn waiter(ch: Channel[int]) {
    let v = ch.recv()
    print("never printed {v}")
}

fn main() {
    let ch = Channel[int]()
    let w = go waiter(ch)
    wait(0.001)
    w.cancel()
    wait(0.001)
    // The next value is queued, not given to the dead waiter.
    ch.send(1)
    print("{ch.try_recv()}")
}
"#,
        "some(1)\n",
    );
}

#[test]
fn close_wakes_every_waiting_receiver_and_stops_blocked_senders() {
    same_on_one_and_three(
        r#"
fn waiter(ch: Channel[int], name: str) {
    print("{name}: {ch.recv()}")
}

fn main() {
    let ch = Channel[int]()
    let a = go waiter(ch, "a")
    let b = go waiter(ch, "b")
    wait(0.001)
    ch.close()
    a.join()
    b.join()
}
"#,
        // Which prints first is the order they began to wait: a, then b.
        "a: none\nb: none\n",
    );
}

#[test]
fn a_sender_blocked_on_a_full_channel_panics_when_the_channel_is_closed() {
    let (out, err) = within_a_minute(|| {
        let out = Out::default();
        let mut vm = colibri::Vm::builder()
            .stdout_shared(Box::new(out.clone()))
            .build();
        vm.load_source(
            "channels.colibri",
            r#"
fn sender(ch: Channel[int]) {
    ch.send(2)
    print("not reached")
}

fn main() {
    let ch = Channel[int](capacity: 1)
    ch.send(1)
    go sender(ch)
    // The coroutine runs at this pause and blocks: the channel is full.
    wait(0.001)
    print("closing")
    ch.close()
    wait(0.001)
    print("not reached either")
}
"#,
        )
        .unwrap();
        let ended = vm.run_main().and_then(|()| vm.run_event_loop());
        (out.text(), ended.map_err(|e| e.render(false)))
    });
    assert_eq!(out, "closing\n");
    let err = err.unwrap_err();
    assert!(err.contains("cannot send on a closed channel"), "{err}");
}

#[test]
fn dropping_the_vm_while_a_worker_waits_on_a_channel_nobody_closes_ends() {
    within_a_minute(|| {
        let out = Out::default();
        let mut vm = colibri::Vm::builder()
            .worker_threads(2)
            .stdout_shared(Box::new(out.clone()))
            .build();
        vm.load_source(
            "channels.colibri",
            r#"
fn main() {
    let ch = Channel[int]()
    let ready = Channel[int]()
    let t = task.run(fn() {
        ready.send(1)
        for v in ch {
            print("never {v}")
        }
    })
    // The task is on its way to wait for a value that never comes.
    ready.recv()
    print("main done {t.done()}")
}
"#,
        )
        .unwrap();
        vm.run_main().unwrap();
        drop(vm);
        assert_eq!(out.text(), "main done false\n");
    });
}

#[test]
fn a_user_clone_runs_once_when_its_value_is_sent_with_clone_and_a_plain_clone_is_a_copy() {
    let body = "\
    let ch = Channel[Doc](capacity: 2)
    let d = Doc { text: \"x\", rev: 1 }
    ch.send(d.clone())
    print(\"{ch.recv().expect(\"one\").rev}\")
    let list = Channel[List[Doc]](capacity: 2)
    list.send([d, d].clone())
    print(\"{list.recv().expect(\"two\")[1].rev}\")
}
";
    let plain = format!("struct Doc {{ text: str, rev: int }}\nfn main() {{\n{body}");
    let own = format!(
        "struct Doc {{ text: str, rev: int }}\nimpl Clone for Doc {{\n    fn clone(self) -> Doc => Doc {{ text: self.text, rev: self.rev + 1 }}\n}}\nfn main() {{\n{body}"
    );
    // The program's own `Clone` runs in the clone that `send` was given, at any depth, exactly
    // once: the walk that copies the result does not run it again.
    assert_eq!(run_on(1, &plain), "1\n1\n");
    assert_eq!(run_on(1, &own), "2\n2\n");
}

#[test]
fn a_joiner_with_coroutines_of_its_own_does_not_run_the_task_inline_and_they_keep_running() {
    // `outer` starts a coroutine that sends after a pause, and joins a task that receives. If the
    // join ran the task inline it would set the sender aside, and nothing could end the wait.
    // The join waits instead, and a thread is started for the task when every one of the pool's
    // is busy (a pool of one is: `outer` is on it).
    let source = r#"
fn sender(ch: Channel[int]) {
    wait(0.01)
    ch.send(7)
}

fn outer() -> int {
    let ch = Channel[int]()
    let c = go sender(ch)
    let t = task.run(fn() => ch.recv().expect("a value"))
    let v = t.join().expect("the task")
    c.join()
    return v
}

fn main() {
    print("got {task.run(outer).join().expect("outer")}")
}
"#;
    for workers in [1, 3, 8] {
        for _ in 0..3 {
            assert_eq!(run_on(workers, source), "got 7\n", "{workers} workers");
        }
    }
}

#[test]
fn the_joiner_that_is_a_coroutine_with_a_sibling_coroutine_gets_a_thread_for_its_task_too() {
    // The joiner is a coroutine and another coroutine feeds the task: same wait, parked.
    let source = r#"
fn sender(ch: Channel[int]) {
    wait(0.01)
    ch.send(9)
}

fn joiner(ch: Channel[int]) {
    let t = task.run(fn() => ch.recv().expect("a value"))
    print("joiner got {t.join().expect("the task")}")
}

fn outer() {
    let ch = Channel[int]()
    let a = go sender(ch)
    let b = go joiner(ch)
    a.join()
    b.join()
}

fn main() {
    task.run(outer).join().expect("outer")
}
"#;
    for workers in [1, 3] {
        for _ in 0..3 {
            assert_eq!(
                run_on(workers, source),
                "joiner got 9\n",
                "{workers} workers"
            );
        }
    }
}

#[test]
fn nested_joins_that_wait_for_each_other_do_not_exhaust_a_pool_of_one() {
    // Each level has a coroutine, so none of them runs the next inline: every join needs a
    // thread of its own, which the pool of one lends for as long as the level waits.
    let source = r#"
fn hold(gate: Channel[int]) {
    gate.recv()
}

fn level(n: int) -> int {
    if n == 0 {
        return 0
    }
    // A coroutine that stays alive until this level has its answer.
    let gate = Channel[int]()
    let held = go hold(gate)
    let r = task.run(fn() => level(n - 1)).join().expect("level")
    gate.close()
    held.join()
    return r + 1
}

fn main() {
    print("depth {task.run(fn() => level(4)).join().expect("top")}")
}
"#;
    assert_eq!(run_on(1, source), "depth 4\n");
}

#[test]
fn the_thread_started_for_a_waiting_joiner_is_temporary_and_is_joined_with_the_pool() {
    let stats = within_a_minute(|| {
        let out = Out::default();
        let mut vm = colibri::Vm::builder()
            .worker_threads(1)
            .stdout_shared(Box::new(out.clone()))
            .build();
        vm.load_source(
            "channels.colibri",
            r#"
fn sender(ch: Channel[int]) {
    wait(0.01)
    ch.send(7)
}

fn outer() -> int {
    let ch = Channel[int]()
    let c = go sender(ch)
    let v = task.run(fn() => ch.recv().expect("a value")).join().expect("the task")
    c.join()
    return v
}

fn main() {
    print("got {task.run(outer).join().expect("outer")}")
}
"#,
        )
        .unwrap();
        vm.run_main().unwrap();
        vm.run_event_loop().unwrap();
        assert_eq!(out.text(), "got 7\n");
        let stats = vm.worker_stats();
        // The pool's one thread, and the one that the join asked for.
        assert_eq!(stats.started.load(std::sync::atomic::Ordering::SeqCst), 2);
        drop(vm);
        stats
    });
    let n = |a: &std::sync::atomic::AtomicUsize| a.load(std::sync::atomic::Ordering::SeqCst);
    assert_eq!(n(&stats.alive), 0);
    assert_eq!(n(&stats.joined), 2);
}

#[test]
fn the_drop_of_the_clone_that_send_was_given_does_not_depend_on_an_unrelated_clone() {
    // `Res` has a `Drop` that prints; the temporary clone is dropped after the send, whether or
    // not some other type in the program has its own `impl Clone`.
    let body = r#"
class Res {
    id: int
}

impl Drop for Res {
    fn drop(self) {
        print("drop {self.id}")
    }
}

fn main() {
    let ch = Channel[Res](capacity: 2)
    let r = Res { id: 1 }
    ch.send(r.clone())
    print("sent")
    let got = ch.recv().expect("one")
    print("got {got.id}")
}
"#;
    let other = "struct Other { n: int }\nimpl Clone for Other {\n    fn clone(self) -> Other => Other { n: self.n }\n}\n";
    let plain = run_on(1, body);
    assert_eq!(run_on(1, &format!("{other}{body}")), plain);
    // The temporary clone is dropped (once), as is the original and the received copy.
    assert_eq!(plain.matches("drop 1").count(), 3, "{plain}");
}

#[test]
fn a_coroutine_that_joins_never_runs_the_task_inline_under_the_main_fiber_that_feeds_it() {
    // The coroutine `joiner` is the only coroutine of `body`, and the main fiber of `body` is
    // what answers the task. If the join ran the task inline, the main fiber would be set aside
    // under it and the task's wait for the answer could never end.
    let source = r#"
fn joiner(req: Channel[int], resp: Channel[int]) -> int {
    let t = task.run(fn() {
        req.send(1)
        return resp.recv().expect("resp")
    })
    return t.join().expect("t")
}

fn body() -> int {
    let req = Channel[int]()
    let resp = Channel[int]()
    let c = go joiner(req, resp)
    let v = req.recv().expect("req")
    resp.send(v + 1)
    return c.join()
}

fn main() {
    print("got {task.run(body).join().expect("body")}")
}
"#;
    same_on_one_and_three(source, "got 2\n");
}
