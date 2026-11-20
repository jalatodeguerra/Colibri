//! `fs.File` through the runtime API: what the golden tests cannot reach (a file bigger than
//! its buffers, a handle that blocks, cancellation, two coroutines on one handle).

use std::path::PathBuf;
use std::time::{Duration, Instant};

use colibri_runtime as colibri;

fn vm() -> (colibri::Vm, colibri::SharedBuffer) {
    let out = colibri::SharedBuffer::new();
    let mut vm = colibri::Vm::builder()
        .stdout(Box::new(out.clone()))
        .allow(colibri::Cap::Fs)
        .build();
    colibri_std::install(&mut vm).expect("installs");
    (vm, out)
}

fn run(vm: &mut colibri::Vm, source: &str) -> Result<(), colibri::Error> {
    vm.load_source("app/main.colibri", source)?;
    vm.run_main()
}

/// A folder of its own, removed afterwards.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!("colibri-file-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }

    fn file(&self, name: &str) -> String {
        self.0.join(name).to_string_lossy().replace('\\', "/")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn a_file_much_bigger_than_the_buffers_is_read_line_by_line() {
    let scratch = Scratch::new("big");
    let path = scratch.file("big.log");
    // About 9 MB in 150000 lines, in the mix of endings a log has, written by Rust so that
    // the contents are known independently of the code under test.
    let mut text = String::new();
    let mut expected_len = 0i64;
    for i in 0..150_000 {
        let line = format!("{i:06} {}", "x".repeat(i % 97));
        expected_len += line.len() as i64;
        text.push_str(&line);
        text.push_str(if i % 3 == 0 { "\r\n" } else { "\n" });
    }
    assert!(text.len() > 6_000_000, "{}", text.len());
    std::fs::write(&path, &text).unwrap();
    let (mut vm, out) = vm();
    run(
        &mut vm,
        &format!(
            r#"import fs

fn main() -> Result[()] {{
    let f = fs.open("{path}")?
    defer f.close()
    var count = 0
    var total = 0
    var longest = 0
    while true {{
        match f.read_line()? {{
            some(line) => {{
                if line.len() < 6 || line[0..6] != "{{count:06}}" {{
                    print("line {{count}} is wrong: {{line}}")
                    return ok(())
                }}
                count += 1
                total += line.len()
                if line.len() > longest {{
                    longest = line.len()
                }}
            }}
            none => break
        }}
    }}
    print(count)
    print(total)
    print(longest)
    print(f.position()?)
    return ok(())
}}
"#
        ),
    )
    .unwrap();
    assert_eq!(
        out.contents(),
        format!("150000\n{expected_len}\n103\n{}\n", text.len())
    );
}

#[test]
fn a_file_is_written_by_one_handle_and_checked_by_rust() {
    let scratch = Scratch::new("write");
    let path = scratch.file("out.log");
    let (mut vm, _) = vm();
    run(
        &mut vm,
        &format!(
            r#"import fs

fn main() -> Result[()] {{
    let f = fs.create("{path}")?
    for i in 0..100000 {{
        f.write("entry {{i}}\n")?
    }}
    f.write_bytes("tail".to_bytes())?
    f.close()?
    return ok(())
}}
"#
        ),
    )
    .unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    let mut expected = String::new();
    for i in 0..100_000 {
        expected.push_str(&format!("entry {i}\n"));
    }
    expected.push_str("tail");
    assert!(text == expected, "contents differ ({} bytes)", text.len());
}

#[test]
fn appending_line_by_line_across_two_opens() {
    let scratch = Scratch::new("append");
    let path = scratch.file("a.log");
    std::fs::write(&path, "before\n").unwrap();
    let (mut vm, out) = vm();
    run(
        &mut vm,
        &format!(
            r#"import fs

fn add(from: int, to: int) -> Result[()] {{
    let f = fs.open_append("{path}")?
    defer f.close()
    for i in from..to {{
        f.write("line {{i}}\n")?
    }}
    return ok(())
}}

fn main() -> Result[()] {{
    add(0, 5000)?
    add(5000, 10000)?
    print(fs.stat("{path}")?.size)
    return ok(())
}}
"#
        ),
    )
    .unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    let mut lines = text.lines();
    assert_eq!(lines.next(), Some("before"));
    for (i, line) in lines.enumerate() {
        assert_eq!(line, format!("line {i}"));
    }
    assert_eq!(text.lines().count(), 10_001);
    assert_eq!(out.contents(), format!("{}\n", text.len()));
}

#[test]
fn a_handle_that_is_still_open_when_the_program_ends_is_flushed() {
    let scratch = Scratch::new("exit");
    let path = scratch.file("kept.txt");
    let (mut vm, _) = vm();
    run(
        &mut vm,
        &format!(
            "import fs\nlet f = fs.create(\"{path}\").expect(\"created\")\nfn main() {{\n    f.write(\"still here\").expect(\"written\")\n}}\n"
        ),
    )
    .unwrap();
    drop(vm);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "still here");
}

#[test]
fn two_coroutines_writing_one_handle_never_mix_their_lines() {
    let scratch = Scratch::new("two-writers");
    let path = scratch.file("both.log");
    let dashes = "-".repeat(300);
    let (mut vm, out) = vm();
    run(
        &mut vm,
        &format!(
            r#"import fs

fn writer(f: fs.File, name: str, n: int) -> int {{
    let dashes = "{dashes}"
    for i in 0..n {{
        let _ = f.write("{{name}} {{i}} {{dashes}}\n")
        if i % 500 == 0 {{
            let _ = f.flush()
        }}
    }}
    return n
}}

fn main() -> Result[()] {{
    let f = fs.create("{path}")?
    let a = go writer(f, "A", 4000)
    let b = go writer(f, "B", 4000)
    print(a.join() + b.join())
    f.close()?
    return ok(())
}}
"#
        ),
    )
    .unwrap_or_else(|e| panic!("{}", e.render(false)));
    assert_eq!(out.contents(), "8000\n");
    let text = std::fs::read_to_string(&path).unwrap();
    let (mut a, mut b) = (0, 0);
    for line in text.lines() {
        let mut parts = line.split(' ');
        match parts.next() {
            Some("A") => {
                assert_eq!(parts.next(), Some(a.to_string().as_str()));
                a += 1;
            }
            Some("B") => {
                assert_eq!(parts.next(), Some(b.to_string().as_str()));
                b += 1;
            }
            other => panic!("a mixed line: {other:?}"),
        }
        assert_eq!(parts.next().map(str::len), Some(300), "{line}");
        assert_eq!(parts.next(), None);
    }
    assert_eq!((a, b), (4000, 4000));
}

// ---- a handle that blocks, and cancellation (Unix: a named pipe blocks like a slow disk) ----

#[cfg(unix)]
mod blocking {
    use super::*;
    use std::ffi::CString;
    use std::fs::OpenOptions;

    fn fifo(scratch: &Scratch, name: &str) -> String {
        let path = scratch.file(name);
        let c = CString::new(path.clone()).unwrap();
        // SAFETY: `c` is a valid NUL-terminated path.
        let rc = unsafe { libc::mkfifo(c.as_ptr(), 0o600) };
        assert_eq!(rc, 0, "mkfifo: {}", std::io::Error::last_os_error());
        path
    }

    /// Cancelling the coroutine that waits for `fs.open` of a pipe nobody writes to ends the
    /// wait at once; the program goes on and ends while the background thread is still stuck.
    #[test]
    fn cancelling_a_coroutine_stuck_in_open_returns_at_once() {
        let scratch = Scratch::new("cancel-open");
        let pipe = fifo(&scratch, "pipe");
        let (mut vm, out) = vm();
        let started = Instant::now();
        run(
            &mut vm,
            &format!(
                r#"import fs

fn stuck() {{
    let _ = fs.open("{pipe}")
    print("never")
}}

fn main() {{
    let c = go stuck()
    wait(0.1)
    c.cancel()
    print("cancelled {{c.done()}}")
}}
"#
            ),
        )
        .unwrap();
        assert_eq!(out.contents(), "cancelled true\n");
        assert!(started.elapsed() < Duration::from_secs(5));
        // Let the stuck thread go: a writer opens the other end.
        drop(OpenOptions::new().write(true).open(&pipe).unwrap());
    }

    /// A read that blocks does not stop the other coroutines, and cancelling it unwinds the
    /// coroutine (its `defer` runs) without waiting for the read.
    #[test]
    fn a_blocked_read_lets_other_coroutines_run_and_can_be_cancelled() {
        let scratch = Scratch::new("cancel-read");
        let pipe = fifo(&scratch, "pipe");
        let (mut vm, out) = vm();
        // Keeps the write end open (so the read blocks instead of ending), until told.
        let (release, hold) = std::sync::mpsc::channel::<()>();
        let writer_path = pipe.clone();
        let writer = std::thread::spawn(move || {
            let w = OpenOptions::new().write(true).open(&writer_path).unwrap();
            let _ = hold.recv();
            drop(w);
        });
        let started = Instant::now();
        run(
            &mut vm,
            &format!(
                r#"import fs

fn reader(f: fs.File) {{
    defer print("reader unwound")
    let _ = f.read_line()
    print("never")
}}

fn ticker() -> int {{
    var n = 0
    while n < 20 {{
        wait(0.01)
        n += 1
    }}
    return n
}}

fn main() -> Result[()] {{
    let f = fs.open("{pipe}")?
    let r = go reader(f)
    let t = go ticker()
    print("ticks {{t.join()}}")
    r.cancel()
    print("cancelled {{r.done()}}")
    return ok(())
}}
"#
            ),
        )
        .unwrap_or_else(|e| panic!("{}", e.render(false)));
        assert_eq!(out.contents(), "ticks 20\nreader unwound\ncancelled true\n");
        assert!(started.elapsed() < Duration::from_secs(5));
        release.send(()).unwrap();
        writer.join().unwrap();
    }

    /// While one coroutine is stuck inside a read of a handle, another that uses the same
    /// handle waits its turn (its own coroutine waits, nobody else), and gets its answer once
    /// the first read is over.
    #[test]
    fn a_second_coroutine_on_a_busy_handle_waits_its_turn() {
        let scratch = Scratch::new("turns");
        let pipe = fifo(&scratch, "pipe");
        let (mut vm, out) = vm();
        let writer_path = pipe.clone();
        let writer = std::thread::spawn(move || {
            use std::io::Write;
            let mut w = OpenOptions::new().write(true).open(&writer_path).unwrap();
            std::thread::sleep(Duration::from_millis(300));
            w.write_all(b"first\nsecond\n").unwrap();
        });
        run(
            &mut vm,
            &format!(
                r#"import fs

fn next(f: fs.File, name: str) -> str {{
    match f.read_line() {{
        ok(some(line)) => return "{{name}} got {{line}}"
        ok(none) => return "{{name}} got the end"
        err(e) => return e.message()
    }}
}}

fn ticker() -> int {{
    var n = 0
    while n < 10 {{
        wait(0.01)
        n += 1
    }}
    return n
}}

fn main() -> Result[()] {{
    let f = fs.open("{pipe}")?
    let a = go next(f, "a")
    let b = go next(f, "b")
    let t = go ticker()
    print("ticks {{t.join()}}")
    let first = a.join()
    let second = b.join()
    print(first)
    print(second)
    return ok(())
}}
"#
            ),
        )
        .unwrap_or_else(|e| panic!("{}", e.render(false)));
        writer.join().unwrap();
        assert_eq!(out.contents(), "ticks 10\na got first\nb got second\n");
    }

    /// A coroutine cancelled while its read waits for the turn never reads: the line that
    /// would have been its goes to the coroutine behind it.
    #[test]
    fn a_cancelled_queued_read_never_runs_and_the_ones_behind_it_do() {
        let scratch = Scratch::new("cancel-queued");
        let pipe = fifo(&scratch, "pipe");
        let (mut vm, out) = vm();
        let writer_path = pipe.clone();
        let writer = std::thread::spawn(move || {
            use std::io::Write;
            let mut w = OpenOptions::new().write(true).open(&writer_path).unwrap();
            std::thread::sleep(Duration::from_millis(400));
            w.write_all(b"first\nsecond\nthird\n").unwrap();
        });
        run(
            &mut vm,
            &format!(
                r#"import fs

fn next(f: fs.File, name: str) -> str {{
    match f.read_line() {{
        ok(some(line)) => return "{{name}} got {{line}}"
        ok(none) => return "{{name}} got the end"
        err(e) => return e.message()
    }}
}}

fn main() -> Result[()] {{
    let f = fs.open("{pipe}")?
    let a = go next(f, "a")
    wait(0.05)
    let x = go next(f, "x")
    wait(0.05)
    let y = go next(f, "y")
    wait(0.05)
    x.cancel()
    print(a.join())
    print(y.join())
    print("main got {{f.read_line()?}}")
    return ok(())
}}
"#
            ),
        )
        .unwrap_or_else(|e| panic!("{}", e.render(false)));
        writer.join().unwrap();
        assert_eq!(
            out.contents(),
            "a got first\ny got second\nmain got some(\"third\")\n"
        );
    }

    /// The program ends while a read is stuck and other operations wait behind it: nothing
    /// hangs on the way out.
    #[test]
    fn ending_the_program_with_queued_operations_does_not_hang() {
        let scratch = Scratch::new("end-queued");
        let pipe = fifo(&scratch, "pipe");
        // Keeps the write end open, so the reads block instead of ending, until told.
        let (release, hold) = std::sync::mpsc::channel::<()>();
        let writer_path = pipe.clone();
        let writer = std::thread::spawn(move || {
            let w = OpenOptions::new().write(true).open(&writer_path).unwrap();
            let _ = hold.recv();
            drop(w);
        });
        let started = Instant::now();
        {
            let (mut vm, out) = vm();
            run(
                &mut vm,
                &format!(
                    r#"import fs

fn next(f: fs.File) {{
    let _ = f.read_line()
}}

fn main() -> Result[()] {{
    let f = fs.open("{pipe}")?
    let a = go next(f)
    let b = go next(f)
    let c = go next(f)
    wait(0.1)
    print("leaving")
    return ok(())
}}
"#
                ),
            )
            .unwrap_or_else(|e| panic!("{}", e.render(false)));
            assert_eq!(out.contents(), "leaving\n");
        }
        assert!(started.elapsed() < Duration::from_secs(5));
        release.send(()).unwrap();
        writer.join().unwrap();
    }
}

/// Operations on one handle are served in the order the script issued them, whatever their size
/// and however the threads are scheduled.
mod order {
    use super::*;

    /// A big write issued first and a small one issued second: the small one never lands first.
    /// Repeated, because the old failure depended on thread timing.
    #[test]
    fn a_small_write_never_overtakes_a_big_one_issued_before_it() {
        let scratch = Scratch::new("order-two");
        let file = scratch.file("order.txt");
        let (mut vm, out) = vm();
        run(
            &mut vm,
            &format!(
                r#"import fs

fn big(f: fs.File, data: Bytes) {{
    f.write_bytes(data).expect("big")
}}

fn small(f: fs.File) {{
    f.write("B").expect("small")
}}

fn main() -> Result[()] {{
    let builder = BytesBuilder()
    for _ in 0..70000 {{ builder.push(65) }}
    let data = builder.to_bytes()
    var wrong = 0
    for _ in 0..200 {{
        let f = fs.create("{file}")?
        let a = go big(f, data)
        let b = go small(f)
        a.join()
        b.join()
        f.close()?
        let back = fs.read_bytes("{file}")?
        if back.len() != 70001 || back[0] != 65 || back[70000] != 66 {{
            wrong += 1
        }}
    }}
    print("out of order: {{wrong}}")
    return ok(())
}}
"#
            ),
        )
        .unwrap_or_else(|e| panic!("{}", e.render(false)));
        assert_eq!(out.contents(), "out of order: 0\n");
    }

    /// Three coroutines and the main fiber write pieces of different sizes: the file holds them
    /// in the order of the calls.
    #[test]
    fn writes_of_different_sizes_land_in_the_order_they_were_issued() {
        let scratch = Scratch::new("order-many");
        let file = scratch.file("many.txt");
        let (mut vm, out) = vm();
        run(
            &mut vm,
            &format!(
                r#"import fs

fn piece(byte: u8, n: int) -> Bytes {{
    let builder = BytesBuilder()
    for _ in 0..n {{ builder.push(byte) }}
    return builder.to_bytes()
}}

fn put(f: fs.File, data: Bytes) {{
    f.write_bytes(data).expect("put")
}}

fn main() -> Result[()] {{
    let a = piece(65, 150000)
    let b = piece(66, 3)
    let c = piece(67, 90000)
    var wrong = 0
    for _ in 0..40 {{
        let f = fs.create("{file}")?
        let x = go put(f, a)
        let y = go put(f, b)
        let z = go put(f, c)
        f.write("D")?
        x.join()
        y.join()
        z.join()
        f.close()?
        let back = fs.read_bytes("{file}")?
        if back != a + b + c + "D".to_bytes() {{
            wrong += 1
        }}
    }}
    print("wrong files: {{wrong}}")
    return ok(())
}}
"#
            ),
        )
        .unwrap_or_else(|e| panic!("{}", e.render(false)));
        assert_eq!(out.contents(), "wrong files: 0\n");
    }

    /// `close()` issued behind a big write takes its turn: the write is in the file, and a
    /// later use of the handle says it is closed.
    #[test]
    fn close_waits_for_the_writes_issued_before_it() {
        let scratch = Scratch::new("order-close");
        let file = scratch.file("close.txt");
        let (mut vm, out) = vm();
        run(
            &mut vm,
            &format!(
                r#"import fs

fn put(f: fs.File, data: Bytes) {{
    f.write_bytes(data).expect("put")
}}

fn main() -> Result[()] {{
    let builder = BytesBuilder()
    for _ in 0..200000 {{ builder.push(65) }}
    let f = fs.create("{file}")?
    let w = go put(f, builder.to_bytes())
    f.close()?
    print("size {{fs.stat("{file}")?.size}}")
    w.join()
    print(f.write("x"))
    return ok(())
}}
"#
            ),
        )
        .unwrap_or_else(|e| panic!("{}", e.render(false)));
        assert!(
            out.contents().starts_with("size 200000\nerr("),
            "{}",
            out.contents()
        );
    }

    /// `flush` on a handle opened for reading has nothing to push and is fine.
    #[test]
    fn flush_on_a_handle_opened_for_reading_does_nothing() {
        let scratch = Scratch::new("flush-reader");
        let file = scratch.file("r.txt");
        std::fs::write(&file, "one\ntwo\n").unwrap();
        let (mut vm, out) = vm();
        run(
            &mut vm,
            &format!(
                r#"import fs

fn main() -> Result[()] {{
    let f = fs.open("{file}")?
    f.flush()?
    print(f.read_line()?)
    return ok(())
}}
"#
            ),
        )
        .unwrap_or_else(|e| panic!("{}", e.render(false)));
        assert_eq!(out.contents(), "some(\"one\")\n");
    }
}
