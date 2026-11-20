//! `net` through the runtime API and against sockets of `std::net`: what the golden tests cannot
//! reach (hundreds of connections, transfers that hit back-pressure, cancellation, the `Vm`
//! ending, a reset, full duplex, two coroutines on one connection). Loopback only, no fixed
//! ports, no sleeps used for synchronization: every wait is for something that has to happen,
//! with a generous limit.

use std::cell::Cell;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::rc::Rc;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use colibri_runtime as colibri;

const LIMIT: Duration = Duration::from_secs(20);

/// A test process on macOS starts with 256 descriptors, and a connection of these tests uses two
/// of them (both ends are in one process). Raises the soft limit to the hard one, once, so the
/// tests do not depend on what the machine's default is.
#[cfg(unix)]
fn raise_descriptor_limit() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let mut lim = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: `lim` is a valid `rlimit` that lives through both calls.
        unsafe {
            if libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) != 0 {
                return;
            }
            // macOS refuses a soft limit above `OPEN_MAX` (10240), even when the hard one is
            // unlimited.
            let want = lim.rlim_max.min(10240);
            if want > lim.rlim_cur {
                lim.rlim_cur = want;
                let _ = libc::setrlimit(libc::RLIMIT_NOFILE, &lim);
            }
        }
    });
}

#[cfg(not(unix))]
fn raise_descriptor_limit() {}

fn vm() -> (colibri::Vm, colibri::SharedBuffer) {
    raise_descriptor_limit();
    let out = colibri::SharedBuffer::new();
    let mut vm = colibri::Vm::builder()
        .stdout(Box::new(out.clone()))
        .allow(colibri::Cap::Net)
        .build();
    colibri_std::install(&mut vm).expect("installs");
    (vm, out)
}

fn run(vm: &mut colibri::Vm, source: &str) -> Result<(), colibri::Error> {
    vm.load_source("app/main.colibri", source)?;
    vm.run_main()
}

/// Runs `source`, which must succeed, and returns what it printed.
fn output(source: &str) -> String {
    let (mut vm, out) = vm();
    run(&mut vm, source).unwrap_or_else(|e| panic!("{e}"));
    out.contents()
}

/// A std listener on a free loopback port.
fn rust_listener() -> (TcpListener, String) {
    let l = TcpListener::bind("127.0.0.1:0").expect("binds");
    let addr = l.local_addr().unwrap().to_string();
    (l, addr)
}

fn with_timeouts(s: &TcpStream) {
    s.set_read_timeout(Some(LIMIT)).unwrap();
    s.set_write_timeout(Some(LIMIT)).unwrap();
}

/// Reads until `n` bytes or the end.
fn read_n(s: &mut TcpStream, n: usize) -> Vec<u8> {
    let mut out = vec![0u8; n];
    let mut at = 0;
    while at < n {
        match s.read(&mut out[at..]).expect("reads") {
            0 => break,
            k => at += k,
        }
    }
    out.truncate(at);
    out
}

#[test]
fn two_hundred_connections_each_exchange_data() {
    let out = output(
        r#"import net

fn echo(c: net.Conn) {
    defer c.close()
    while true {
        let line = c.read_line() or |e| { return }
        match line {
            none => return
            some(text) => { c.write("echo {text}\n") or |e| { return } }
        }
    }
}

fn serve(l: net.Listener) {
    while true {
        let c = l.accept() or |e| { return }
        go echo(c)
    }
}

var connected = 0

fn client(addr: str, i: int, total: int) -> bool {
    let c = net.connect(addr).expect("connected")
    defer c.close()
    connected += 1
    // Every connection stays open until all of them are up.
    while connected < total {
        wait(0.005)
    }
    for round in 0..3 {
        c.write("{i}:{round}\n").expect("written")
        let got = c.read_line().expect("read")
        if got != some("echo {i}:{round}") {
            return false
        }
    }
    return true
}

fn main() -> Result[()] {
    let l = net.listen("127.0.0.1:0")?
    let server = go serve(l)
    var clients: List[Coroutine[bool]] = []
    for i in 0..200 {
        clients.push(go client(l.addr(), i, 200))
        // At most 25 connects at a time: a system answers a burst of connects that does not
        // fit its listen backlog with resets (Windows does), and this test is not about that.
        if i % 25 == 24 {
            while connected < i + 1 {
                wait(0.005)
            }
        }
    }
    var good = 0
    for c in clients {
        if c.join() {
            good += 1
        }
    }
    print("good {good}")
    l.close()
    server.join()
    return ok(())
}
"#,
    );
    assert_eq!(out, "good 200\n");
}

/// A transfer much bigger than the socket buffers, with a receiver that reads in small steps
/// and pauses, so the sender's writes block on back-pressure. Both ends are coroutines of one
/// program; the digest of what was sent must equal the digest of what arrived.
#[test]
fn a_big_transfer_with_a_slow_reader_arrives_intact() {
    let out = output(
        r#"import net
import hash

fn payload(size: int) -> Bytes {
    let b = BytesBuilder()
    var x = 12345
    for i in 0..size {
        x = (x * 1103515245 + 12345) % 2147483648
        b.push(u8((x / 65536) % 256))
    }
    return b.to_bytes()
}

fn send(addr: str, data: Bytes) {
    let c = net.connect(addr).expect("connected")
    c.write_bytes(data).expect("sent")
    c.close()
}

fn receive(l: net.Listener, size: int) -> Bytes {
    let c = l.accept().expect("accepted")
    let b = BytesBuilder()
    var got = 0
    while got < size {
        let piece = c.read(65536).expect("read")
        if piece.len() == 0 {
            break
        }
        b.push_bytes(piece)
        got += piece.len()
        // Slow: the sender cannot get rid of 12 MB while this sleeps.
        wait(0.002)
    }
    c.close()
    return b.to_bytes()
}

fn main() -> Result[()] {
    let size = 12 * 1024 * 1024
    let seed = payload(65536)
    let b = BytesBuilder()
    for _ in 0..(size / 65536) {
        b.push_bytes(seed)
    }
    let data = b.to_bytes()
    let l = net.listen("127.0.0.1:0")?
    let r = go receive(l, size)
    let s = go send(l.addr(), data)
    let got = r.join()
    s.join()
    print(got.len() == size)
    print(hash.sha256_hex(got) == hash.sha256_hex(data))
    return ok(())
}
"#,
    );
    assert_eq!(out, "true\ntrue\n");
}

#[test]
fn a_rust_client_talks_to_a_colibri_server() {
    let (mut vm, out) = vm();
    let (tx, rx) = mpsc::channel::<String>();
    // The script prints its address; the client thread needs it, so the script runs in the
    // test thread and hands the address over through a host function.
    vm.register_fn("publish", move |addr: String| {
        tx.send(addr).unwrap();
    })
    .unwrap();
    let client = thread::spawn(move || {
        let addr = rx.recv_timeout(LIMIT).expect("the server's address");
        let mut s = TcpStream::connect(addr).expect("connects");
        with_timeouts(&s);
        s.write_all(b"hello\r\nsecond line\nrest").unwrap();
        s.shutdown(std::net::Shutdown::Write).unwrap();
        let mut answer = String::new();
        s.read_to_string(&mut answer).unwrap();
        answer
    });
    run(
        &mut vm,
        r#"import net

fn main() -> Result[()] {
    let l = net.listen("127.0.0.1:0")?
    publish(l.addr())
    let c = l.accept()?
    var n = 0
    while true {
        match c.read_line()? {
            none => break
            some(line) => {
                n += 1
                c.write("{n}: {line}\n")?
            }
        }
    }
    c.close()
    l.close()
    return ok(())
}
"#,
    )
    .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(out.contents(), "");
    assert_eq!(
        client.join().unwrap(),
        "1: hello\n2: second line\n3: rest\n"
    );
}

#[test]
fn a_colibri_client_talks_to_a_rust_server() {
    let (listener, addr) = rust_listener();
    let server = thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        with_timeouts(&s);
        let got = read_n(&mut s, 5);
        s.write_all(b"pong:").unwrap();
        s.write_all(&got).unwrap();
        // A line in two pieces, and then the end of the stream.
        s.write_all(b"\nhalf").unwrap();
        s.write_all(b" line\n").unwrap();
        got
    });
    let out = output(&format!(
        r#"import net

fn main() -> Result[()] {{
    let c = net.connect("{addr}")?
    c.write("ping!")?
    print(c.read_line()?)
    print(c.read_line()?)
    print(c.read_line()?)
    print(c.local_addr() != c.peer_addr())
    print(c.peer_addr() == "{addr}")
    return ok(())
}}
"#
    ));
    assert_eq!(server.join().unwrap(), b"ping!");
    assert_eq!(
        out,
        "some(\"pong:ping!\")\nsome(\"half line\")\nnone\ntrue\ntrue\n"
    );
}

#[test]
fn a_rust_client_gets_back_a_large_echo() {
    let (mut vm, out) = vm();
    let (tx, rx) = mpsc::channel::<String>();
    vm.register_fn("publish", move |addr: String| {
        tx.send(addr).unwrap();
    })
    .unwrap();
    let client = thread::spawn(move || {
        let addr = rx.recv_timeout(LIMIT).expect("the server's address");
        let s = TcpStream::connect(addr).unwrap();
        with_timeouts(&s);
        let data: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
        let mut writer = s.try_clone().unwrap();
        let sent = data.clone();
        let w = thread::spawn(move || {
            writer.write_all(&sent).unwrap();
            writer.shutdown(std::net::Shutdown::Write).unwrap();
        });
        let mut got = Vec::new();
        let mut s = s;
        // Reads slowly.
        let mut chunk = [0u8; 4096];
        loop {
            let n = s.read(&mut chunk).unwrap();
            if n == 0 {
                break;
            }
            got.extend_from_slice(&chunk[..n]);
            if got.len() % (1 << 18) < 4096 {
                thread::sleep(Duration::from_millis(1));
            }
        }
        w.join().unwrap();
        got == data
    });
    run(
        &mut vm,
        r#"import net

fn main() -> Result[()] {
    let l = net.listen("127.0.0.1:0")?
    publish(l.addr())
    let c = l.accept()?
    // Copies until the end of the stream, in pieces that are not the client's.
    while true {
        let piece = c.read(10000)?
        if piece.len() == 0 {
            break
        }
        c.write_bytes(piece)?
    }
    c.close_write()?
    c.close()
    l.close()
    return ok(())
}
"#,
    )
    .unwrap_or_else(|e| panic!("{e}"));
    assert!(
        client.join().unwrap(),
        "the echo differs from what was sent"
    );
    assert_eq!(out.contents(), "");
}

#[test]
fn a_peer_that_resets_the_connection_is_reset() {
    let (listener, addr) = rust_listener();
    let (up_tx, up_rx) = mpsc::channel::<()>();
    let peer = thread::spawn(move || {
        let (s, _) = listener.accept().unwrap();
        // Reset only once the script has its connection: a reset during `connect` is a
        // failed connect, which is another test.
        up_rx.recv_timeout(LIMIT).unwrap();
        // Closing with a zero linger sends a reset instead of an orderly end, on every system.
        socket2::SockRef::from(&s)
            .set_linger(Some(Duration::ZERO))
            .expect("sets the linger");
        drop(s);
    });
    let (mut vm, out) = vm();
    vm.register_fn("connected", move || up_tx.send(()).unwrap())
        .unwrap();
    run(
        &mut vm,
        &format!(
            r#"import net

fn main() -> Result[()] {{
    let c = net.connect("{addr}")?
    connected()
    match c.read(10) {{
        ok(b) => print("read {{b.len()}} bytes")
        err(e) => print("{{e.as[net.Error]().expect("a net error")}}")
    }}
    return ok(())
}}
"#
        ),
    )
    .unwrap_or_else(|e| panic!("{e}"));
    peer.join().unwrap();
    assert_eq!(out.contents(), "Reset\n");
}

/// One coroutine waits in `read` while another writes several times on the same connection:
/// the writes do not wait for the read.
#[test]
fn a_connection_is_full_duplex() {
    let (listener, addr) = rust_listener();
    let peer = thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        with_timeouts(&s);
        // Everything the script writes arrives while the script's reader still waits.
        let got = read_n(&mut s, "one\ntwo\nthree\n".len());
        // Only now does the reader get its data.
        s.write_all(b"wake up\n").unwrap();
        got
    });
    let out = output(&format!(
        r#"import net

fn reader(c: net.Conn) -> str {{
    return c.read_line().expect("read").expect("a line")
}}

fn main() -> Result[()] {{
    let c = net.connect("{addr}")?
    let r = go reader(c)
    wait(0.05)
    c.write("one\n")?
    c.write("two\n")?
    c.write("three\n")?
    print(r.join())
    return ok(())
}}
"#
    ));
    assert_eq!(peer.join().unwrap(), b"one\ntwo\nthree\n");
    assert_eq!(out, "wake up\n");
}

/// Two reads on one connection take turns and never split a call; so do two writes.
#[test]
fn concurrent_reads_and_writes_on_one_connection_are_serialized() {
    let (listener, addr) = rust_listener();
    let peer = thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        with_timeouts(&s);
        // Two big writes from the script, read here and checked for mixing.
        let got = read_n(&mut s, 2 * 3_000_000);
        // Eight bytes for two `read_exact(4)`, in pieces that cut across the calls.
        for piece in [&b"AA"[..], b"AABB", b"BB"] {
            s.write_all(piece).unwrap();
            s.flush().unwrap();
            thread::sleep(Duration::from_millis(30));
        }
        got
    });
    let out = output(&format!(
        r#"import net

fn writer(c: net.Conn, b: Bytes) {{
    c.write_bytes(b).expect("written")
}}

fn reader(c: net.Conn) -> str {{
    return c.read_exact(4).expect("read").to_str_lossy()
}}

fn main() -> Result[()] {{
    let c = net.connect("{addr}")?
    let one = BytesBuilder()
    let two = BytesBuilder()
    for _ in 0..3_000_000 {{
        one.push(u8(1))
        two.push(u8(2))
    }}
    let r1 = go reader(c)
    let r2 = go reader(c)
    let w1 = go writer(c, one.to_bytes())
    let w2 = go writer(c, two.to_bytes())
    w1.join()
    w2.join()
    let a = r1.join()
    let b = r2.join()
    var both = [a, b]
    both.sort()
    print(both)
    return ok(())
}}
"#
    ));
    let got = peer.join().unwrap();
    assert_eq!(got.len(), 6_000_000);
    // One whole block and then the other, in either order.
    let first = got[0];
    assert!(got[..3_000_000].iter().all(|&b| b == first));
    assert!(got[3_000_000..].iter().all(|&b| b != first));
    assert_eq!(out, "[\"AAAA\", \"BBBB\"]\n");
}

#[test]
fn cancelling_a_blocked_accept_or_read_is_prompt_and_leaves_the_object_usable() {
    let (listener, addr) = rust_listener();
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let peer = thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        with_timeouts(&s);
        // Nothing is sent until the script says it cancelled its read.
        done_rx.recv_timeout(LIMIT).unwrap();
        s.write_all(b"after the cancel\n").unwrap();
        // Keep the connection open until the script is done.
        let _ = s.read(&mut [0u8; 1]);
    });
    let (mut vm, out) = vm();
    vm.register_fn("signal", move || done_tx.send(()).unwrap())
        .unwrap();
    let started = Instant::now();
    run(
        &mut vm,
        &format!(
            r#"import net

fn accepting(l: net.Listener) {{
    let _ = l.accept()
    print("accept returned: it should not")
}}

fn reading(c: net.Conn) {{
    let _ = c.read(10)
    print("read returned: it should not")
}}

fn main() -> Result[()] {{
    let l = net.listen("127.0.0.1:0")?
    let a = go accepting(l)
    let c = net.connect("{addr}")?
    let r = go reading(c)
    wait(0.1)
    a.cancel()
    r.cancel()
    print(a.done())
    print(r.done())
    signal()
    // The same listener and connection work after the cancels.
    let c2 = go fn() -> net.Conn {{ return net.connect(l.addr()).expect("connected") }}()
    let s2 = l.accept()?
    s2.write("hi\n")?
    print(c2.join().read_line()?)
    print(c.read_line()?)
    c.close()
    return ok(())
}}
"#
        ),
    )
    .unwrap_or_else(|e| panic!("{e}"));
    assert!(started.elapsed() < LIMIT, "took {:?}", started.elapsed());
    peer.join().unwrap();
    assert_eq!(
        out.contents(),
        "true\ntrue\nsome(\"hi\")\nsome(\"after the cancel\")\n"
    );
}

/// `close()` on another coroutine's object ends its wait with `Closed`, promptly.
#[test]
fn closing_ends_a_blocked_accept_and_a_blocked_read() {
    let (listener, addr) = rust_listener();
    let peer = thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        with_timeouts(&s);
        // Held open and silent until the script closes its end.
        let mut b = [0u8; 1];
        s.read(&mut b).unwrap()
    });
    let started = Instant::now();
    let out = output(&format!(
        r#"import net

fn kind[T](r: Result[T]) -> str {{
    match r {{
        ok(_) => return "ok"
        err(e) => return "{{e.as[net.Error]().expect("a net error")}}"
    }}
}}

fn main() -> Result[()] {{
    let l = net.listen("127.0.0.1:0")?
    let c = net.connect("{addr}")?
    let a = go fn() -> str {{ return kind(l.accept()) }}()
    let r = go fn() -> str {{ return kind(c.read(1)) }}()
    let r2 = go fn() -> str {{ return kind(c.read_line()) }}()
    wait(0.1)
    l.close()
    c.close()
    print(a.join())
    print(r.join())
    print(r2.join())
    return ok(())
}}
"#
    ));
    assert!(started.elapsed() < LIMIT, "took {:?}", started.elapsed());
    // The peer sees the end of the stream: the socket really went away.
    assert_eq!(peer.join().unwrap(), 0);
    assert_eq!(out, "Closed\nClosed\nClosed\n");
}

/// Ending the program with operations blocked returns at once and releases every socket.
#[test]
fn dropping_the_vm_with_blocked_operations_is_prompt_and_closes_the_sockets() {
    let (listener, addr) = rust_listener();
    let (up_tx, up_rx) = mpsc::channel::<TcpStream>();
    let acceptor = thread::spawn(move || {
        for _ in 0..2 {
            let (s, _) = listener.accept().unwrap();
            with_timeouts(&s);
            up_tx.send(s).unwrap();
        }
    });
    let (addr_tx, addr_rx) = mpsc::channel::<String>();
    let (mut vm, _out) = vm();
    vm.register_fn("publish", move |addr: String| addr_tx.send(addr).unwrap())
        .unwrap();
    run(
        &mut vm,
        &format!(
            r#"import net

fn accepting(l: net.Listener) {{
    let _ = l.accept()
}}

fn reading(c: net.Conn) {{
    let _ = c.read(10)
}}

fn lines(c: net.Conn) {{
    let _ = c.read_line()
}}

fn main() -> Result[()] {{
    let l = net.listen("127.0.0.1:0")?
    publish(l.addr())
    go accepting(l)
    let c = net.connect("{addr}")?
    let d = net.connect("{addr}")?
    go reading(c)
    go lines(c)
    go reading(d)
    // Everything waits when `main` ends.
    wait(0.1)
    return ok(())
}}
"#
        ),
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let script_addr = addr_rx.recv_timeout(LIMIT).unwrap();
    let started = Instant::now();
    drop(vm);
    assert!(started.elapsed() < LIMIT, "took {:?}", started.elapsed());
    acceptor.join().unwrap();
    // The script's connections are closed: their peers read the end of the stream.
    for _ in 0..2 {
        let mut s = up_rx.recv_timeout(LIMIT).unwrap();
        let mut b = [0u8; 1];
        match s.read(&mut b) {
            Ok(n) => assert_eq!(n, 0),
            Err(e) => panic!("the peer of a dropped Vm did not see the end: {e}"),
        }
    }
    // And its listener is closed: nobody accepts there any more.
    let deadline = Instant::now() + LIMIT;
    while TcpStream::connect(&script_addr).is_ok() {
        assert!(
            Instant::now() < deadline,
            "the listener of a dropped Vm still accepts"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

/// A fake clock does not move while the program waits on the network: what a program does at
/// a fake time does not depend on how long the peer takes.
#[test]
fn a_fake_clock_does_not_advance_while_a_coroutine_waits_on_the_network() {
    struct Fake(Rc<Cell<f64>>);
    impl colibri::Clock for Fake {
        fn now(&mut self) -> f64 {
            self.0.get()
        }
        fn sleep(&mut self, s: f64) {
            self.0.set(self.0.get() + s);
        }
    }
    let (listener, addr) = rust_listener();
    let peer = thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        thread::sleep(Duration::from_millis(300));
        s.write_all(b"late\n").unwrap();
        let _ = s.read(&mut [0u8; 1]);
    });
    let time = Rc::new(Cell::new(0.0));
    let out = colibri::SharedBuffer::new();
    let mut vm = colibri::Vm::builder()
        .stdout(Box::new(out.clone()))
        .allow(colibri::Cap::Net)
        .clock(Fake(Rc::clone(&time)))
        .build();
    colibri_std::install(&mut vm).unwrap();
    run(
        &mut vm,
        &format!(
            r#"import net

var ticks = 0

fn counter() {{
    while true {{
        ticks += 1
        wait(0.01)
    }}
}}

fn main() -> Result[()] {{
    let c = net.connect("{addr}")?
    let t = go counter()
    print(c.read_line()?)
    print("ticks {{ticks}}")
    t.cancel()
    c.close()
    return ok(())
}}
"#
        ),
    )
    .unwrap_or_else(|e| panic!("{e}"));
    peer.join().unwrap();
    assert_eq!(out.contents(), "some(\"late\")\nticks 1\n");
    assert_eq!(time.get(), 0.0);
}

/// A line too long for `read_line`, and a line that is not text, are errors. The line that is
/// not text is taken off the stream (as `fs.File` does), the error says so, and the next line
/// is still there.
#[test]
fn read_line_refuses_a_huge_line_and_bad_utf8() {
    let (listener, addr) = rust_listener();
    let peer = thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        with_timeouts(&s);
        s.write_all(&[0xff, 0xfe, b'\n', b'o', b'k', b'\n'])
            .unwrap();
        // More than a mebibyte and no end of line: the reader must give up, not buffer it all.
        let chunk = vec![b'x'; 64 * 1024];
        for _ in 0..40 {
            if s.write_all(&chunk).is_err() {
                break;
            }
        }
        let _ = s.read(&mut [0u8; 1]);
    });
    let out = output(&format!(
        r#"import net

fn kind[T](r: Result[T]) -> str {{
    match r {{
        ok(_) => return "ok"
        err(e) => return "{{e.as[net.Error]().expect("a net error")}}"
    }}
}}

fn main() -> Result[()] {{
    let c = net.connect("{addr}")?
    let bad = c.read_line()
    print(kind(bad))
    print(bad.err().expect("an error").message().contains("taken off the stream"))
    print(c.read_line()?)
    print(kind(c.read_line()))
    c.close()
    return ok(())
}}
"#
    ));
    assert_eq!(out, "InvalidData\ntrue\nsome(\"ok\")\nLineTooLong\n");
    peer.join().unwrap();
}

/// Operations of one kind on one connection are served in the order they were called, not in
/// the order their pool threads happen to start: a small write called after a big one waits
/// for it, and queued reads get the bytes in call order.
#[test]
fn operations_on_one_connection_are_served_in_call_order() {
    let (listener, addr) = rust_listener();
    let peer = thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        with_timeouts(&s);
        // The big block first, then the markers, in the order they were called.
        let got = read_n(&mut s, 2_000_000 + 8);
        // 30 bytes for 30 queued reads, one byte at a time, so each read is a separate turn.
        for i in 0..30u8 {
            s.write_all(&[b'a' + i % 26]).unwrap();
            s.flush().unwrap();
        }
        let _ = s.read(&mut [0u8; 1]);
        got
    });
    let out = output(&format!(
        r#"import net

fn big(c: net.Conn, b: Bytes) {{
    c.write_bytes(b).expect("written")
}}

fn small(c: net.Conn, text: str) {{
    c.write(text).expect("written")
}}

fn reader(c: net.Conn) -> str {{
    return c.read_exact(1).expect("read").to_str_lossy()
}}

fn main() -> Result[()] {{
    let c = net.connect("{addr}")?
    let b = BytesBuilder()
    for _ in 0..2_000_000 {{
        b.push(u8(120))
    }}
    var writers: List[Coroutine[()]] = []
    writers.push(go big(c, b.to_bytes()))
    writers.push(go small(c, "1"))
    writers.push(go small(c, "2"))
    writers.push(go small(c, "3"))
    writers.push(go small(c, "4"))
    writers.push(go small(c, "5"))
    writers.push(go small(c, "6"))
    writers.push(go small(c, "7"))
    writers.push(go small(c, "8"))
    var readers: List[Coroutine[str]] = []
    for _ in 0..30 {{
        readers.push(go reader(c))
    }}
    for w in writers {{
        w.join()
    }}
    let text = StrBuilder()
    for r in readers {{
        text.push(r.join())
    }}
    print(text.to_str())
    c.close()
    return ok(())
}}
"#
    ));
    let got = peer.join().unwrap();
    assert!(
        got[..2_000_000].iter().all(|&b| b == b'x'),
        "a small write overtook the big one"
    );
    // The markers 1..8 arrive after the big block, one write each, in call order.
    assert_eq!(&got[2_000_000..], b"12345678");
    assert_eq!(out, "abcdefghijklmnopqrstuvwxyzabcd\n");
}

/// An operation whose coroutine is cancelled before its turn never runs, and the ones behind
/// it are not held up.
#[test]
fn a_cancelled_queued_read_is_skipped() {
    let (listener, addr) = rust_listener();
    let (go_tx, go_rx) = mpsc::channel::<()>();
    let peer = thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        with_timeouts(&s);
        go_rx.recv_timeout(LIMIT).unwrap();
        s.write_all(b"AB").unwrap();
        let _ = s.read(&mut [0u8; 1]);
    });
    let (mut vm, out) = vm();
    vm.register_fn("release", move || go_tx.send(()).unwrap())
        .unwrap();
    run(
        &mut vm,
        &format!(
            r#"import net

fn reader(c: net.Conn) -> str {{
    return c.read_exact(1).expect("read").to_str_lossy()
}}

fn main() -> Result[()] {{
    let c = net.connect("{addr}")?
    let first = go reader(c)
    let second = go reader(c)
    let third = go reader(c)
    // Let all three queue up behind the first, which waits for data.
    task.tick(0.0)
    second.cancel()
    release()
    print(first.join())
    print(third.join())
    c.close()
    return ok(())
}}
"#
        ),
    )
    .unwrap_or_else(|e| panic!("{e}"));
    peer.join().unwrap();
    assert_eq!(out.contents(), "A\nB\n");
}

// ---- regression tests for the review of the first version ----

/// A read that finished on its worker, whose coroutine is cancelled before it resumes, loses
/// nothing: the bytes stay for the next read. (The interpreter thread is held busy so the
/// worker is done before the cancel.)
#[test]
fn a_read_that_finished_before_its_coroutine_was_cancelled_loses_no_data() {
    let (listener, addr) = rust_listener();
    let (tx, rx) = mpsc::channel::<()>();
    let peer = thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        with_timeouts(&s);
        rx.recv_timeout(LIMIT).unwrap();
        s.write_all(b"hello\nworld\n").unwrap();
        let _ = s.read(&mut [0u8; 1]);
    });
    let (mut vm, out) = vm();
    vm.register_fn("signal", move || tx.send(()).unwrap())
        .unwrap();
    vm.register_fn("block", |s: f64| thread::sleep(Duration::from_secs_f64(s)))
        .unwrap();
    run(
        &mut vm,
        &format!(
            r#"import net

fn reader(c: net.Conn) -> str {{
    return c.read_line().expect("r").expect("l")
}}

fn main() -> Result[()] {{
    let c = net.connect("{addr}")?
    let r = go reader(c)
    task.tick(0.0)
    signal()
    // The interpreter thread is busy: the reader's worker finishes, its coroutine is not
    // resumed until later.
    block(0.5)
    r.cancel()
    c.set_timeout(5.0)
    print(c.read_line()?)
    print(c.read_line()?)
    c.close()
    return ok(())
}}
"#
        ),
    )
    .unwrap_or_else(|e| panic!("{e}"));
    peer.join().unwrap();
    assert_eq!(out.contents(), "some(\"hello\")\nsome(\"world\")\n");
}

/// A connection accepted for a coroutine that was cancelled before it resumed is not lost: the
/// next `accept` returns it.
#[test]
fn an_accept_that_finished_before_its_coroutine_was_cancelled_loses_no_connection() {
    let (tx, rx) = mpsc::channel::<String>();
    let (back_tx, back_rx) = mpsc::channel::<()>();
    let client = thread::spawn(move || {
        let addr = rx.recv_timeout(LIMIT).unwrap();
        let mut s = TcpStream::connect(addr).unwrap();
        with_timeouts(&s);
        back_tx.send(()).unwrap();
        // What the script writes to the connection the second `accept` returns.
        let mut text = String::new();
        s.read_to_string(&mut text).unwrap();
        text
    });
    let (mut vm, out) = vm();
    vm.register_fn("publish", move |a: String| tx.send(a).unwrap())
        .unwrap();
    vm.register_fn("wait_client", move || back_rx.recv_timeout(LIMIT).unwrap())
        .unwrap();
    vm.register_fn("block", |s: f64| thread::sleep(Duration::from_secs_f64(s)))
        .unwrap();
    run(
        &mut vm,
        r#"import net

fn main() -> Result[()] {
    let l = net.listen("127.0.0.1:0")?
    let a = go fn() -> str {
        let c = l.accept().expect("a")
        c.write("to the first").expect("w")
        return "accepted"
    }()
    task.tick(0.0)
    publish(l.addr())
    wait_client()
    // The interpreter thread is busy: the accept finishes on its worker, and the coroutine
    // that asked for it is cancelled before it resumes.
    block(0.5)
    a.cancel()
    let c = l.accept()?
    c.write("to the second")?
    c.close()
    l.close()
    return ok(())
}
"#,
    )
    .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(out.contents(), "");
    assert_eq!(client.join().unwrap(), "to the second");
}

/// Connections that wait in the queue of a listener are closed with it. The listener value
/// stays alive after `close()` until the client has said what it saw, so the end of the stream
/// comes from `close` and not from the listener being dropped when the program ends.
///
/// Whether the connection is already in the listener's own queue when `close()` runs depends on
/// whether the accept worker was quicker than the cancel (a few hundred microseconds against
/// the half second this waits): when it was not, the connection is still in the kernel's
/// backlog, and closing a listening socket resets such a connection (on Linux and macOS), which
/// is acceptable and is the one other thing the client may see. What must never happen is a
/// connection that stays open: the client's read would then time out. The exact case, a
/// connection that is in the queue, is pinned without any timing by the unit test
/// `an_accepted_connection_that_was_not_taken_is_kept_and_closed_with_the_listener` in `net.rs`.
#[test]
fn closing_a_listener_closes_the_connections_it_kept() {
    let (tx, rx) = mpsc::channel::<String>();
    let (back_tx, back_rx) = mpsc::channel::<()>();
    let (seen_tx, seen_rx) = mpsc::channel::<std::io::Result<usize>>();
    let client = thread::spawn(move || {
        let addr = rx.recv_timeout(LIMIT).unwrap();
        let mut s = TcpStream::connect(addr).unwrap();
        // Only a bound for a failure: the end of the stream comes at once when `close` works.
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        back_tx.send(()).unwrap();
        // Waits for the end of the stream, or for a reset, or for the bound.
        let seen = s.read(&mut [0u8; 1]);
        seen_tx.send(seen).unwrap();
    });
    let (mut vm, _out) = vm();
    vm.register_fn("publish", move |a: String| tx.send(a).unwrap())
        .unwrap();
    vm.register_fn("wait_client", move || back_rx.recv_timeout(LIMIT).unwrap())
        .unwrap();
    // Waits until the client has read what it reads: the listener is alive until then.
    let seen = Rc::new(Cell::new(None));
    let seen_in = Rc::clone(&seen);
    vm.register_fn("client_has_seen", move || {
        seen_in.set(Some(
            seen_rx.recv_timeout(LIMIT).expect("the client reported"),
        ));
    })
    .unwrap();
    vm.register_fn("block", |s: f64| thread::sleep(Duration::from_secs_f64(s)))
        .unwrap();
    run(
        &mut vm,
        r#"import net

fn main() -> Result[()] {
    let l = net.listen("127.0.0.1:0")?
    let a = go fn() {
        let _ = l.accept()
    }()
    task.tick(0.0)
    publish(l.addr())
    wait_client()
    block(0.5)
    a.cancel()
    // The connection is in the queue of `l`; closing `l` closes it.
    l.close()
    client_has_seen()
    // `l` is used after the close (it is closed: an error), so it was alive all along.
    print(l.accept().is_err())
    return ok(())
}
"#,
    )
    .unwrap_or_else(|e| panic!("{e}"));
    client.join().unwrap();
    match seen.take().expect("the client reported") {
        Ok(0) => {}
        // The slow path, see above: the connection was still in the kernel's backlog.
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => {}
        other => panic!("the client of a kept connection saw {other:?}, not an end or a reset"),
    }
}

/// `defer c.close()` in a coroutine that is cancelled while its read waits: `close()` must not
/// suspend (a cancelled coroutine would skip the rest of its `defer`), and the descriptor must
/// be released when it returns, many times, so that the window where the worker still holds the
/// socket is hit.
#[test]
fn close_in_a_defer_of_a_cancelled_reader_runs_to_the_end() {
    let (listener, addr) = rust_listener();
    let peer = thread::spawn(move || {
        let mut eofs = 0;
        let mut other = Vec::new();
        for _ in 0..40 {
            let (mut s, _) = listener.accept().unwrap();
            with_timeouts(&s);
            match s.read(&mut [0u8; 1]) {
                Ok(0) => eofs += 1,
                r => other.push(format!("{r:?}")),
            }
        }
        (eofs, other)
    });
    let out = output(&format!(
        r#"import net

var started = 0
var finished = 0

fn reading(c: net.Conn) {{
    defer {{
        started += 1
        c.close()
        finished += 1
    }}
    let _ = c.read(1)
}}

fn main() -> Result[()] {{
    for i in 0..40 {{
        let c = net.connect("{addr}")?
        let r = go reading(c)
        wait(0.01 + 0.001 * float(i % 7))
        r.cancel()
    }}
    print("defers started {{started}}, past close() {{finished}}")
    return ok(())
}}
"#
    ));
    assert_eq!(out, "defers started 40, past close() 40\n");
    let (eofs, other) = peer.join().unwrap();
    assert_eq!((eofs, other), (40, Vec::new()));
}

/// `close()` is not a scheduling point: no other coroutine runs between its call and the next
/// statement, even while a worker still holds the socket.
#[test]
fn close_is_not_a_scheduling_point() {
    let (listener, addr) = rust_listener();
    let peer = thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        with_timeouts(&s);
        s.read(&mut [0u8; 1]).unwrap()
    });
    let out = output(&format!(
        r#"import net

var other_ran = false

fn other() {{
    // Waits for its turn: it only gets one when `main` stops running.
    wait(0.0)
    other_ran = true
}}

fn reading(c: net.Conn) {{
    let _ = c.read(1)
}}

fn main() -> Result[()] {{
    let c = net.connect("{addr}")?
    let r = go reading(c)
    task.tick(0.0)
    let o = go other()
    c.close()
    print(other_ran)
    o.join()
    r.join()
    return ok(())
}}
"#
    ));
    assert_eq!(peer.join().unwrap(), 0);
    assert_eq!(out, "false\n");
}

/// A write that times out after part of its data went out says how much, and that the stream
/// is out of step; the peer received exactly what the message says.
#[test]
fn a_write_that_times_out_says_how_much_was_sent() {
    let (listener, addr) = rust_listener();
    let (tx, rx) = mpsc::channel::<()>();
    let peer = thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        with_timeouts(&s);
        rx.recv_timeout(LIMIT).unwrap();
        let mut n = 0usize;
        let mut buf = [0u8; 65536];
        loop {
            match s.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(k) => n += k,
            }
        }
        n
    });
    let (mut vm, out) = vm();
    vm.register_fn("signal", move || tx.send(()).unwrap())
        .unwrap();
    run(
        &mut vm,
        &format!(
            r#"import net

fn main() -> Result[()] {{
    let c = net.connect("{addr}")?
    let seed = BytesBuilder()
    for _ in 0..65536 {{
        seed.push(u8(7))
    }}
    let piece = seed.to_bytes()
    let b = BytesBuilder()
    for _ in 0..(64 * 1024 * 1024 / 65536) {{
        b.push_bytes(piece)
    }}
    c.set_timeout(0.3)
    match c.write_bytes(b.to_bytes()) {{
        ok(_) => print("written")
        err(e) => {{
            print("{{e.as[net.Error]().expect("a net error")}}")
            print(e.message())
        }}
    }}
    // Part of the data is out: the write side is broken, and says so.
    match c.write("TAIL") {{
        ok(_) => print("written")
        err(e) => {{
            print("{{e.as[net.Error]().expect("a net error")}}")
            print(e.message())
        }}
    }}
    signal()
    c.close()
    return ok(())
}}
"#
        ),
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let received = peer.join().unwrap();
    let text = out.contents();
    let mut lines = text.lines();
    assert_eq!(lines.next(), Some("Timeout"), "{text}");
    let message = lines.next().unwrap_or_default();
    assert!(
        message.contains(&format!("{received} of 67108864 bytes were sent")),
        "the peer got {received} bytes: {message}"
    );
    assert!(message.contains("out of step"), "{message}");
    assert!(message.contains("close the connection"), "{message}");
    assert_eq!(lines.next(), Some("BrokenPipe"), "{text}");
    let later = lines.next().unwrap_or_default();
    assert!(
        later.contains("an earlier write stopped part way")
            && later.contains("close the connection"),
        "{later}"
    );
}

/// The text of a timeout is true when part of the line has arrived: it says the call did not
/// finish, not that nothing happened.
#[test]
fn a_timeout_in_the_middle_of_a_line_says_it_did_not_finish() {
    let (listener, addr) = rust_listener();
    let (tx, rx) = mpsc::channel::<()>();
    let peer = thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        with_timeouts(&s);
        s.write_all(b"par").unwrap();
        rx.recv_timeout(LIMIT).unwrap();
        s.write_all(b"t\n").unwrap();
        let _ = s.read(&mut [0u8; 1]);
    });
    let (mut vm, out) = vm();
    vm.register_fn("signal", move || tx.send(()).unwrap())
        .unwrap();
    run(
        &mut vm,
        &format!(
            r#"import net

fn main() -> Result[()] {{
    let c = net.connect("{addr}")?
    c.set_timeout(0.2)
    let first = c.read_line()
    print(first.err().expect("a timeout").message().replace("{addr}", "<addr>"))
    // What had arrived is still there.
    signal()
    c.set_timeout(10.0)
    print(c.read_line()?)
    c.close()
    return ok(())
}}
"#
        ),
    )
    .unwrap_or_else(|e| panic!("{e}"));
    peer.join().unwrap();
    assert_eq!(
        out.contents(),
        "cannot read from `<addr>`: did not finish within 0.2 seconds (see `set_timeout`; the connection is still usable)\nsome(\"part\")\n"
    );
}

/// A deadline bounds the whole write, also when the peer reads steadily but slowly; and a
/// write whose coroutine is cancelled stops at once (the next send is not tried).
#[test]
fn a_big_write_stops_at_its_deadline_and_when_cancelled() {
    let (listener, addr) = rust_listener();
    let peer = thread::spawn(move || {
        let mut ends = Vec::new();
        for _ in 0..2 {
            let (mut s, _) = listener.accept().unwrap();
            with_timeouts(&s);
            let start = Instant::now();
            let mut buf = [0u8; 16384];
            // Reads steadily but slowly for the first two seconds, then as fast as it can,
            // until the script closes its end (the socket buffers may hold megabytes).
            loop {
                match s.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) if start.elapsed() < Duration::from_secs(2) => {
                        thread::sleep(Duration::from_millis(20));
                    }
                    Ok(_) => {}
                }
            }
            ends.push(start.elapsed() < LIMIT);
        }
        ends
    });
    let started = Instant::now();
    let out = output(&format!(
        r#"import net

fn big() -> Bytes {{
    let seed = BytesBuilder()
    for _ in 0..65536 {{
        seed.push(u8(7))
    }}
    let piece = seed.to_bytes()
    let b = BytesBuilder()
    for _ in 0..640 {{
        b.push_bytes(piece)
    }}
    return b.to_bytes()
}}

fn sender(c: net.Conn, data: Bytes) {{
    let _ = c.write_bytes(data)
}}

fn main() -> Result[()] {{
    let data = big()
    let c = net.connect("{addr}")?
    c.set_timeout(0.5)
    match c.write_bytes(data) {{
        ok(_) => print("written")
        err(e) => print("{{e.as[net.Error]().expect("a net error")}}")
    }}
    c.close()

    let d = net.connect("{addr}")?
    let w = go sender(d, data)
    wait(0.2)
    w.cancel()
    print(w.done())
    // The cancelled write is over: closing does not wait for it.
    d.close()
    return ok(())
}}
"#
    ));
    assert_eq!(out, "Timeout\ntrue\n");
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "took {:?}",
        started.elapsed()
    );
    assert_eq!(peer.join().unwrap(), [true, true]);
}

/// Many reads blocked at once (each holds one pool thread, see ARCHITECTURE §22) all end with
/// `Closed` when their connections are closed, and promptly.
#[test]
fn many_blocked_reads_end_when_their_connections_are_closed() {
    raise_descriptor_limit();
    let (listener, addr) = rust_listener();
    let keep = thread::spawn(move || {
        let mut held = Vec::new();
        for _ in 0..50 {
            held.push(listener.accept().unwrap().0);
        }
        // Held open and silent until the script has closed its ends.
        for s in &mut held {
            with_timeouts(s);
            assert_eq!(s.read(&mut [0u8; 1]).unwrap(), 0);
        }
    });
    let started = Instant::now();
    let out = output(&format!(
        r#"import net

fn kind[T](r: Result[T]) -> str {{
    match r {{
        ok(_) => return "ok"
        err(e) => return "{{e.as[net.Error]().expect("a net error")}}"
    }}
}}

fn reading(c: net.Conn) -> str {{
    return kind(c.read(1))
}}

fn main() -> Result[()] {{
    var conns: List[net.Conn] = []
    var readers: List[Coroutine[str]] = []
    for i in 0..50 {{
        let c = net.connect("{addr}")?
        conns.push(c)
        readers.push(go reading(c))
    }}
    wait(0.2)
    for c in conns {{
        c.close()
    }}
    var closed = 0
    for r in readers {{
        if r.join() == "Closed" {{
            closed += 1
        }}
    }}
    print("closed {{closed}}")
    return ok(())
}}
"#
    ));
    assert_eq!(out, "closed 50\n");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "took {:?}",
        started.elapsed()
    );
    keep.join().unwrap();
}

/// A bad count is an error like the one `fs.File.read` gives; a line of exactly the limit with
/// `\r\n` is a line; `read_exact` of a big block leaves what follows for the next read.
#[test]
fn counts_lines_at_the_limit_and_big_blocks() {
    const BLOCK: usize = 300_000;
    let (listener, addr) = rust_listener();
    let peer = thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        with_timeouts(&s);
        let mut all = vec![b'x'; 1 << 20];
        all.extend_from_slice(b"\r\nnext\n");
        all.extend((0..BLOCK + 1000).map(|i| (i % 251) as u8));
        s.write_all(&all).unwrap();
        let _ = s.read(&mut [0u8; 1]);
    });
    let out = output(&format!(
        r#"import net

fn kind[T](r: Result[T]) -> str {{
    match r {{
        ok(_) => return "ok"
        err(e) => return "{{e.as[net.Error]().expect("a net error")}}"
    }}
}}

fn main() -> Result[()] {{
    let c = net.connect("{addr}")?
    print(kind(c.read(-1)))
    print(kind(c.read_exact(-5)))
    print(c.read(-1).err().expect("bad count").message())
    let line = c.read_line()?.expect("a line")
    print(line.len())
    print(c.read_line()?)
    let block = c.read_exact({BLOCK})?
    var good = true
    for i in 0..{BLOCK} {{
        if int(block[i]) != i % 251 {{
            good = false
        }}
    }}
    print("{{block.len()}} {{good}}")
    let rest = c.read_exact(1000)?
    print("{{rest.len()}} {{int(rest[0]) == {BLOCK} % 251}}")
    c.close()
    return ok(())
}}
"#
    ));
    peer.join().unwrap();
    assert_eq!(
        out,
        format!(
            "Other\nOther\ncannot read from `{}`: -1 is not a number of bytes (use 0 or more)\n1048576\nsome(\"next\")\n{BLOCK} true\n1000 true\n",
            addr
        )
    );
}

/// A big `read_exact` is correct and takes time in proportion to its size (the buffer grows
/// with what has arrived: a doubling, not a copy per piece).
#[test]
fn a_big_read_exact_over_loopback_is_correct_and_linear() {
    const SIZE: usize = 64 * 1024 * 1024;
    let (listener, addr) = rust_listener();
    let peer = thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        with_timeouts(&s);
        // Pieces that do not divide the total, each with a pattern that depends on the offset.
        let mut sent = 0usize;
        let mut piece = vec![0u8; 100_003];
        while sent < SIZE {
            let n = piece.len().min(SIZE - sent);
            for (i, b) in piece[..n].iter_mut().enumerate() {
                *b = ((sent + i) % 251) as u8;
            }
            s.write_all(&piece[..n]).unwrap();
            sent += n;
        }
        let _ = s.read(&mut [0u8; 1]);
    });
    let started = Instant::now();
    let out = output_within(
        format!(
            r#"import net

fn main() -> Result[()] {{
    let c = net.connect("{addr}")?
    let b = c.read_exact({SIZE})?
    var bad = 0
    var i = 0
    while i < {SIZE} {{
        if int(b[i]) != i % 251 {{
            bad += 1
        }}
        i += 40009
    }}
    print(b.len())
    print(bad)
    c.close()
    return ok(())
}}
"#
        ),
        Duration::from_secs(120),
    );
    peer.join().unwrap();
    assert_eq!(out, format!("{SIZE}\n0\n"));
    // Not a bound on speed (a debug build on a loaded machine is slow): a hang or a quadratic
    // buffer would take minutes.
    assert!(
        started.elapsed() < Duration::from_secs(100),
        "{:?}",
        started.elapsed()
    );
}

/// `connect_timeout` connects like `connect`, fails like it, and takes only a positive number.
#[test]
fn connect_timeout_connects_fails_and_checks_its_argument() {
    let (listener, addr) = rust_listener();
    let peer = thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        with_timeouts(&s);
        s.write_all(b"hi\n").unwrap();
        let _ = s.read(&mut [0u8; 1]);
    });
    // A port that is closed now.
    let closed = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().to_string()
    };
    let out = output(&format!(
        r#"import net

fn main() -> Result[()] {{
    let c = net.connect_timeout("{addr}", 10.0)?
    print(c.read_line()?)
    c.close()
    match net.connect_timeout("{closed}", 10.0) {{
        ok(_) => print("connected")
        err(e) => print("{{e.as[net.Error]().expect("a net error")}}")
    }}
    print(net.connect_timeout("nonsense", 1.0).err().is_some())
    return ok(())
}}
"#
    ));
    peer.join().unwrap();
    assert_eq!(out, "some(\"hi\")\nRefused\ntrue\n");
    for bad in ["0.0", "-1.0", "math.sqrt(-1.0)"] {
        let (mut vm, _out) = vm();
        let err = run(
            &mut vm,
            &format!(
                "import net\nimport math\n\nfn main() -> Result[()] {{\n    let _ = net.connect_timeout(\"127.0.0.1:1\", {bad})\n    return ok(())\n}}\n"
            ),
        )
        .expect_err("a bad number of seconds is a panic");
        assert!(
            err.to_string().contains("net.connect_timeout"),
            "{bad}: {err}"
        );
    }
}

/// A connect that cannot finish ends at its timeout, and one that is cancelled ends at once.
/// Linux and macOS only: a listener whose accept queue is full leaves further connects waiting
/// there (the SYN is dropped; BSD, and so macOS, does the same as Linux), while Windows answers
/// an overflowing backlog with a reset, so a connect cannot be kept pending on loopback there.
/// For Windows (and for every system) the cancel and the deadline of a connect are covered by
/// the unit test `a_connect_that_is_cancelled_or_late_stops_before_it_connects` in `net.rs`,
/// which does not need a connect to stay pending.
///
/// How many connections fill the queue is not the same on every system (Linux: one more than the
/// backlog; macOS lets in more than the backlog says), so the test connects until one does not
/// finish, up to a generous limit, and keeps what it made open.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn a_connect_that_does_not_finish_is_bounded_and_cancellable() {
    /// More than any queue of these systems takes (their default maximum is 128, and BSD lets in
    /// half as much again).
    const FILL_LIMIT: usize = 400;
    raise_descriptor_limit();
    // A listener nobody accepts from, with the smallest backlog there is.
    let sock = socket2::Socket::new(
        socket2::Domain::IPV4,
        socket2::Type::STREAM,
        Some(socket2::Protocol::TCP),
    )
    .unwrap();
    sock.bind(
        &"127.0.0.1:0"
            .parse::<std::net::SocketAddr>()
            .unwrap()
            .into(),
    )
    .unwrap();
    sock.listen(0).unwrap();
    let addr = sock.local_addr().unwrap().as_socket().unwrap();
    let mut filling = Vec::new();
    let full = loop {
        match TcpStream::connect_timeout(&addr, Duration::from_millis(300)) {
            Ok(s) => filling.push(s),
            Err(e) => break e,
        }
        assert!(
            filling.len() < FILL_LIMIT,
            "the accept queue does not fill up on this machine after {FILL_LIMIT} connections"
        );
    };
    assert_eq!(
        full.kind(),
        std::io::ErrorKind::TimedOut,
        "a connect to a full queue should wait, but ended with `{full}` after {} connections",
        filling.len()
    );
    assert_eq!(
        TcpStream::connect_timeout(&addr, Duration::from_millis(300))
            .expect_err("the queue is still full")
            .kind(),
        std::io::ErrorKind::TimedOut
    );
    let started = Instant::now();
    let out = output(&format!(
        r#"import net

fn main() -> Result[()] {{
    match net.connect_timeout("{addr}", 0.4) {{
        ok(_) => print("connected")
        err(e) => print("{{e.as[net.Error]().expect("a net error")}}")
    }}
    let w = go fn() {{ let _ = net.connect("{addr}") }}()
    wait(0.1)
    w.cancel()
    print(w.done())
    return ok(())
}}
"#
    ));
    assert_eq!(out, "Timeout\ntrue\n");
    assert!(
        started.elapsed() >= Duration::from_millis(400),
        "the timeout ended early: {:?}",
        started.elapsed()
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "took {:?}",
        started.elapsed()
    );
    drop(filling);
}

/// A write that stopped with some of its data sent leaves the stream out of step: the write
/// side is broken (`BrokenPipe`, with a text that says why), reading still works, and what the
/// peer got is the partial block and nothing after it.
#[test]
fn a_write_cancelled_part_way_breaks_the_write_side_and_not_the_read_side() {
    const SIZE: usize = 64 * 1024 * 1024;
    let (listener, addr) = rust_listener();
    let (tx, rx) = mpsc::channel::<()>();
    let peer = thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        with_timeouts(&s);
        rx.recv_timeout(LIMIT).unwrap();
        s.write_all(b"hello\n").unwrap();
        let mut all = Vec::new();
        s.read_to_end(&mut all).unwrap();
        all
    });
    let (mut vm, out) = vm();
    vm.register_fn("signal", move || tx.send(()).unwrap())
        .unwrap();
    run(
        &mut vm,
        &format!(
            r#"import net

fn big() -> Bytes {{
    let seed = BytesBuilder()
    for _ in 0..65536 {{
        seed.push(u8(7))
    }}
    let piece = seed.to_bytes()
    let b = BytesBuilder()
    for _ in 0..{blocks} {{
        b.push_bytes(piece)
    }}
    return b.to_bytes()
}}

fn sender(c: net.Conn, data: Bytes) {{
    let _ = c.write_bytes(data)
}}

fn main() -> Result[()] {{
    let c = net.connect("{addr}")?
    let w = go sender(c, big())
    wait(0.3)
    w.cancel()
    // The write is over (the peer reads nothing yet, so some of it is out and some is not).
    match c.write("TAIL") {{
        ok(_) => print("written")
        err(e) => {{
            print("{{e.as[net.Error]().expect("a net error")}}")
            print(e.message().replace("{addr}", "<addr>"))
        }}
    }}
    // Still broken for any later write, and reading is not affected.
    print(c.write("again").is_err())
    signal()
    print(c.read_line()?)
    c.close()
    return ok(())
}}
"#,
            blocks = SIZE / 65536
        ),
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let got = peer.join().unwrap();
    assert_eq!(
        out.contents(),
        "BrokenPipe\ncannot write to `<addr>`: an earlier write stopped part way, so the stream is out of step (close the connection; reading still works)\ntrue\nsome(\"hello\")\n"
    );
    assert!(!got.is_empty() && got.len() < SIZE, "{} bytes", got.len());
    assert!(
        got.iter().all(|&b| b == 7),
        "something other than the block followed it"
    );
}

/// A write that was cancelled before it sent anything changes nothing: the connection is usable,
/// and what the peer gets is the first write whole and then the next one, not the cancelled one.
#[test]
fn a_write_cancelled_before_it_sent_anything_leaves_the_connection_usable() {
    const SIZE: usize = 64 * 1024 * 1024;
    let (listener, addr) = rust_listener();
    let (tx, rx) = mpsc::channel::<()>();
    let peer = thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        with_timeouts(&s);
        rx.recv_timeout(LIMIT).unwrap();
        let mut all = Vec::new();
        s.read_to_end(&mut all).unwrap();
        all
    });
    let (mut vm, out) = vm();
    vm.register_fn("signal", move || tx.send(()).unwrap())
        .unwrap();
    run(
        &mut vm,
        &format!(
            r#"import net

fn big() -> Bytes {{
    let seed = BytesBuilder()
    for _ in 0..65536 {{
        seed.push(u8(7))
    }}
    let piece = seed.to_bytes()
    let b = BytesBuilder()
    for _ in 0..{blocks} {{
        b.push_bytes(piece)
    }}
    return b.to_bytes()
}}

fn first(c: net.Conn, data: Bytes) -> bool {{
    return c.write_bytes(data).is_ok()
}}

fn second(c: net.Conn) {{
    let _ = c.write("SECOND")
}}

fn main() -> Result[()] {{
    let c = net.connect("{addr}")?
    let w1 = go first(c, big())
    // The first write fills the buffers and waits for the peer.
    wait(0.3)
    // The second one waits behind it and has sent nothing when it is cancelled.
    let w2 = go second(c)
    wait(0.1)
    w2.cancel()
    signal()
    print(w1.join())
    print(c.write("THIRD").is_ok())
    c.close_write()?
    c.close()
    return ok(())
}}
"#,
            blocks = SIZE / 65536
        ),
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let got = peer.join().unwrap();
    assert_eq!(out.contents(), "true\ntrue\n");
    assert_eq!(got.len(), SIZE + 5);
    assert!(got[..SIZE].iter().all(|&b| b == 7));
    assert_eq!(&got[SIZE..], b"THIRD");
}

/// `close_write` takes its turn behind the writes called before it: `go c.write(big)` then
/// `c.close_write()` sends all of `big` and then the end of the stream (it used to cut the write
/// with a broken pipe). Also from a `Drop` hook, which blocks until the writes ahead are done.
#[test]
fn close_write_waits_for_the_writes_called_before_it() {
    // A whole number of 64 KiB blocks, built the way the other tests build a big value.
    const SIZE: usize = 128 * 65536;
    for from_hook in [false, true] {
        let (listener, addr) = rust_listener();
        let peer = thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            with_timeouts(&s);
            let mut all = Vec::new();
            s.read_to_end(&mut all).unwrap();
            all.len()
        });
        let call = if from_hook {
            "var g: Option[Guard] = some(Guard { conn: c })\n    g = none"
        } else {
            "c.close_write()?"
        };
        let out = output_within(
            format!(
                r#"import net

class Guard {{ conn: net.Conn }}

impl Drop for Guard {{
    fn drop(self) {{
        print("close_write in a hook: {{self.conn.close_write().is_ok()}}")
    }}
}}

fn sender(c: net.Conn, data: Bytes) {{
    c.write_bytes(data).expect("the write is not cut")
}}

fn main() -> Result[()] {{
    let c = net.connect("{addr}")?
    let seed = BytesBuilder()
    for _ in 0..65536 {{
        seed.push(u8(120))
    }}
    let piece = seed.to_bytes()
    let b = BytesBuilder()
    for _ in 0..{blocks} {{
        b.push_bytes(piece)
    }}
    let w = go sender(c, b.to_bytes())
    task.tick(0.0)
    {call}
    w.join()
    print("sent")
    c.close()
    return ok(())
}}
"#,
                blocks = SIZE / 65536
            ),
            LIMIT,
        );
        assert_eq!(
            out,
            if from_hook {
                "close_write in a hook: true\nsent\n"
            } else {
                "sent\n"
            }
        );
        assert_eq!(peer.join().unwrap(), SIZE, "from_hook = {from_hook}");
    }
}

/// A write that stopped part way (here: cancelled) breaks the write side, and `close_write` behind
/// it still shuts the writing end: it is the only way left to tell the peer that nothing more
/// comes, and it does not fail for what an earlier write did. Writes after it fail as before.
#[test]
fn close_write_after_a_cut_write_still_ends_the_stream() {
    const SIZE: usize = 64 * 1024 * 1024;
    let (listener, addr) = rust_listener();
    let (tx, rx) = mpsc::channel::<()>();
    // Set by the peer once some of the data has arrived: the write has then really sent part of
    // it, and (the peer reads no more until told) cannot finish, so cancelling it cuts it.
    let arrived = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let seen = std::sync::Arc::clone(&arrived);
    let peer = thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        with_timeouts(&s);
        let mut first = [0u8; 1];
        s.read_exact(&mut first).unwrap();
        seen.store(true, std::sync::atomic::Ordering::SeqCst);
        rx.recv_timeout(LIMIT).unwrap();
        let mut all = first.to_vec();
        s.read_to_end(&mut all).unwrap();
        all.len()
    });
    let (mut vm, out) = vm();
    vm.register_fn("signal", move || tx.send(()).unwrap())
        .unwrap();
    vm.register_fn("peer_has_data", move || {
        arrived.load(std::sync::atomic::Ordering::SeqCst)
    })
    .unwrap();
    run(
        &mut vm,
        &format!(
            r#"import net

fn sender(c: net.Conn, data: Bytes) {{
    let _ = c.write_bytes(data)
}}

fn main() -> Result[()] {{
    let c = net.connect("{addr}")?
    let seed = BytesBuilder()
    for _ in 0..65536 {{
        seed.push(u8(7))
    }}
    let piece = seed.to_bytes()
    let b = BytesBuilder()
    for _ in 0..{blocks} {{
        b.push_bytes(piece)
    }}
    let w = go sender(c, b.to_bytes())
    var tries = 0
    while !peer_has_data() {{
        tries += 1
        if tries > 2000 {{
            panic("no part of the write reached the peer")
        }}
        wait(0.01)
    }}
    w.cancel()
    print(c.close_write().is_ok())
    print(c.write("more").is_err())
    signal()
    c.close()
    return ok(())
}}
"#,
            blocks = SIZE / 65536
        ),
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let got = peer.join().unwrap();
    assert_eq!(out.contents(), "true\ntrue\n");
    assert!(got > 0 && got < SIZE, "{got}");
}

/// `close()` does not wait for anyone: two hundred sockets with a blocked acceptor, reader or
/// writer on each close together in far less than the 50 ms a poller's slice would cost each
/// (ten seconds for all of them), and every waiter ends with `Closed`.
#[test]
fn closing_two_hundred_sockets_with_waiters_does_not_wait_for_any_slice() {
    raise_descriptor_limit();
    let (listener, addr) = rust_listener();
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let keep = thread::spawn(move || {
        let mut held = Vec::new();
        for _ in 0..130 {
            held.push(listener.accept().unwrap().0);
        }
        // Silent, and not reading, until the script is over.
        let _ = done_rx.recv_timeout(LIMIT);
    });
    let marks = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Instant>::new()));
    let (mut vm, out) = vm();
    let m = std::sync::Arc::clone(&marks);
    vm.register_fn("mark", move || m.lock().unwrap().push(Instant::now()))
        .unwrap();
    run(
        &mut vm,
        &format!(
            r#"import net

fn kind[T](r: Result[T]) -> str {{
    match r {{
        ok(_) => return "ok"
        err(e) => return "{{e.as[net.Error]().expect("a net error")}}"
    }}
}}

fn accepting(l: net.Listener) -> str {{
    return kind(l.accept())
}}

fn reading(c: net.Conn) -> str {{
    return kind(c.read(1))
}}

fn writing(c: net.Conn, data: Bytes) -> str {{
    return kind(c.write_bytes(data))
}}

fn big() -> Bytes {{
    let seed = BytesBuilder()
    for _ in 0..65536 {{
        seed.push(u8(7))
    }}
    let piece = seed.to_bytes()
    let b = BytesBuilder()
    for _ in 0..512 {{
        b.push_bytes(piece)
    }}
    return b.to_bytes()
}}

fn main() -> Result[()] {{
    var listeners: List[net.Listener] = []
    var readers: List[net.Conn] = []
    var writers: List[net.Conn] = []
    var tasks: List[Coroutine[str]] = []
    for _ in 0..70 {{
        let l = net.listen("127.0.0.1:0")?
        listeners.push(l)
        tasks.push(go accepting(l))
    }}
    for _ in 0..120 {{
        let c = net.connect("{addr}")?
        readers.push(c)
        tasks.push(go reading(c))
    }}
    let data = big()
    for _ in 0..10 {{
        let c = net.connect("{addr}")?
        writers.push(c)
        tasks.push(go writing(c, data))
    }}
    // Everything is waiting now (the writers have filled their buffers).
    wait(0.5)
    mark()
    for l in listeners {{
        l.close()
    }}
    for c in readers {{
        c.close()
    }}
    for c in writers {{
        c.close()
    }}
    mark()
    var closed = 0
    for t in tasks {{
        if t.join() == "Closed" {{
            closed += 1
        }}
    }}
    print("closed {{closed}}")
    return ok(())
}}
"#
        ),
    )
    .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(out.contents(), "closed 200\n");
    let marks = marks.lock().unwrap();
    let took = marks[1].duration_since(marks[0]);
    // 200 closes that each waited for a slice would take 10 seconds; the bound is wide for a
    // slow machine and still below that.
    assert!(took < Duration::from_secs(8), "the closes took {took:?}");
    done_tx.send(()).unwrap();
    keep.join().unwrap();
}

/// Binding a specific address of a port whose wildcard address is held is `AddrInUse` on Linux
/// and on Windows (`SO_EXCLUSIVEADDRUSE`). Not on macOS: there `SO_REUSEADDR`, which `listen`
/// sets so that a server can restart, lets a socket bind a specific address next to a
/// wildcard one (the BSD rule), so the answer differs there and the docs say so.
#[cfg(any(target_os = "linux", windows))]
#[test]
fn a_specific_address_of_a_port_held_on_the_wildcard_is_in_use() {
    let out = output(
        r#"import net

fn main() -> Result[()] {
    let l = net.listen("0.0.0.0:0")?
    let specific = l.addr().replace("0.0.0.0", "127.0.0.1")
    match net.listen(specific) {
        ok(_) => print("bound")
        err(e) => print("{e.as[net.Error]().expect("a net error")}")
    }
    // The same address twice, too.
    match net.listen(l.addr()) {
        ok(_) => print("bound")
        err(e) => print("{e.as[net.Error]().expect("a net error")}")
    }
    // A listener that was closed gives its port back at once.
    let first = net.listen("127.0.0.1:0")?
    let addr = first.addr()
    first.close()
    let again = net.listen(addr)?
    print(again.addr() == addr)
    return ok(())
}
"#,
    );
    assert_eq!(out, "AddrInUse\nAddrInUse\ntrue\n");
}

/// A server that closes a connection first leaves the port in `TIME_WAIT` on its side, and the
/// port must still be listenable again after the listener is closed: `kvserver` does this on
/// SHUTDOWN and `--idle`. On Linux and macOS `SO_REUSEADDR` allows it; on Windows `listen` sets
/// `SO_EXCLUSIVEADDRUSE`, whose rule for a port with connections of its own still around is
/// what this test checks (it has not been run on Windows by the author of this change). No
/// retry: the restart must work at once.
#[test]
fn a_port_can_be_listened_on_again_after_the_server_closed_a_connection_first() {
    let out = output_within(
        r#"import net

fn main() -> Result[()] {
    let l = net.listen("127.0.0.1:0")?
    let addr = l.addr()
    let c = net.connect(addr)?
    let s = l.accept()?
    s.write("bye\n")?
    // The server side closes first.
    s.close()
    print(c.read_line()?)
    print(c.read_line()?)
    c.close()
    l.close()
    let again = net.listen(addr)?
    print(again.addr() == addr)
    // And it serves on the port it got back.
    let c2 = net.connect(addr)?
    let s2 = again.accept()?
    c2.write("hi\n")?
    print(s2.read_line()?)
    return ok(())
}
"#
        .to_string(),
        LIMIT,
    );
    assert_eq!(out, "some(\"bye\")\nnone\ntrue\nsome(\"hi\")\n");
}
/// Whether this machine has IPv6 at all: it can bind `[::1]`. A machine without it fails that
/// with "address not available" or "address family not supported" (EAFNOSUPPORT: 97 on Linux,
/// 47 on macOS, WSAEAFNOSUPPORT 10047 on Windows); the test is skipped then, and says so. Any
/// other failure is a failure of the test.
fn has_ipv6(test: &str) -> bool {
    match TcpListener::bind("[::1]:0") {
        Ok(_) => true,
        Err(e)
            if e.kind() == std::io::ErrorKind::AddrNotAvailable
                || e.kind() == std::io::ErrorKind::Unsupported
                || matches!(e.raw_os_error(), Some(97 | 47 | 10047)) =>
        {
            eprintln!("{test}: skipped, this machine has no IPv6 ({e})");
            false
        }
        Err(e) => panic!("binding [::1]:0 failed in an unexpected way: {e}"),
    }
}

/// `listen("[::]:P")` accepts IPv4 connections too, on every system (`IPV6_V6ONLY` is off
/// explicitly: Windows defaults to on), and `connect` to the address it reports goes to the
/// loopback address of its family.
#[test]
fn an_ipv6_wildcard_listener_accepts_ipv4_and_ipv6() {
    if !has_ipv6("an_ipv6_wildcard_listener_accepts_ipv4_and_ipv6") {
        return;
    }
    let out = output(
        r#"import net

fn main() -> Result[()] {
    let l = net.listen("[::]:0")?
    print(l.addr().starts_with("[::]:"))
    // An IPv4 client, by the port of the listener.
    let v4 = net.connect(l.addr().replace("[::]", "127.0.0.1"))?
    let s4 = l.accept()?
    v4.write("four\n")?
    print(s4.read_line()?)
    // An IPv6 client, written out and as the address the listener reports.
    let v6 = net.connect(l.addr().replace("[::]", "[::1]"))?
    let s6 = l.accept()?
    v6.write("six\n")?
    print(s6.read_line()?)
    let any = net.connect(l.addr())?
    let sa = l.accept()?
    any.write("any\n")?
    print(sa.read_line()?)
    print(any.peer_addr().starts_with("[::1]:"))
    return ok(())
}
"#,
    );
    assert_eq!(
        out,
        "true\nsome(\"four\")\nsome(\"six\")\nsome(\"any\")\ntrue\n"
    );
}

/// A peer that is gone gives `BrokenPipe` on a write and `Reset` on a read: the same variant on
/// every system (Windows says "reset" for the write, Linux and macOS say "broken pipe").
#[test]
fn a_write_to_a_peer_that_reset_is_a_broken_pipe_and_a_read_is_a_reset() {
    let (listener, addr) = rust_listener();
    let (tx, rx) = mpsc::channel::<()>();
    let peer = thread::spawn(move || {
        let (s, _) = listener.accept().unwrap();
        // Only after the script's connect has finished: a reset that arrives earlier fails
        // the connect itself.
        rx.recv_timeout(LIMIT).unwrap();
        // Closing with unread data in the buffer makes the system send a reset.
        let sock = socket2::SockRef::from(&s);
        sock.set_linger(Some(Duration::ZERO)).unwrap();
        drop(s);
    });
    let (mut vm, out) = vm();
    vm.register_fn("connected", move || tx.send(()).unwrap())
        .unwrap();
    run(
        &mut vm,
        &format!(
            r#"import net

fn kind[T](r: Result[T]) -> str {{
    match r {{
        ok(_) => return "ok"
        err(e) => return "{{e.as[net.Error]().expect("a net error")}}"
    }}
}}

fn main() -> Result[()] {{
    let c = net.connect("{addr}")?
    connected()
    // The peer's reset arrives; nothing else does.
    print(kind(c.read(1)))
    // Writes to a connection the peer reset: the first may be accepted by the system, a later
    // one is refused. Either way the variant is `BrokenPipe`.
    var last = "ok"
    for _ in 0..500 {{
        last = kind(c.write("data"))
        if last != "ok" {{
            break
        }}
        wait(0.02)
    }}
    print(last)
    return ok(())
}}
"#
        ),
    )
    .unwrap_or_else(|e| panic!("{e}"));
    peer.join().unwrap();
    assert_eq!(out.contents(), "Reset\nBrokenPipe\n");
}

/// What a script can see of the module: `connect_timeout` is there, and `Cancelled` is not (a
/// cancelled coroutine is unwound, so a script never holds that error).
#[test]
fn the_interface_has_connect_timeout_and_no_cancelled() {
    let mut vm = colibri::Vm::builder()
        .stdout(Box::new(std::io::sink()))
        .build();
    colibri_std::install(&mut vm).unwrap();
    let text = vm.std_module("net").interface_text();
    assert!(
        text.contains("fn connect_timeout(addr: str, seconds: float)"),
        "{text}"
    );
    assert!(!text.contains("Cancelled"), "{text}");
}

// ---- a read or accept that cannot suspend ----

/// Runs `source` on a thread of its own and waits for it at most `limit`, so that a program that
/// hangs fails the test instead of hanging it (the thread that hangs is left behind; the process
/// ends when the tests do).
fn output_within(source: String, limit: Duration) -> String {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let (mut vm, out) = vm();
        let result = run(&mut vm, &source).map(|()| out.contents());
        let _ = tx.send(result.map_err(|e| e.to_string()));
    });
    match rx.recv_timeout(limit) {
        Ok(Ok(text)) => text,
        Ok(Err(e)) => panic!("{e}"),
        Err(_) => panic!("the program did not finish within {limit:?}: it hangs"),
    }
}

/// A Rust peer for the hook tests: accepts one connection and, when told, writes `one\ntwo\n` to
/// it; then it reads what the script writes until the end of the stream. The data is sent when
/// the script calls `release()`, a host function that is not a scheduling point, so a coroutine
/// that waits for it cannot finish its read before the next thing the script does.
fn hook_peer() -> (String, mpsc::Sender<()>, thread::JoinHandle<String>) {
    let (listener, addr) = rust_listener();
    let (tx, rx) = mpsc::channel::<()>();
    let peer = thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        with_timeouts(&s);
        rx.recv_timeout(LIMIT).unwrap();
        s.write_all(b"one\ntwo\n").unwrap();
        let mut got = String::new();
        s.read_to_string(&mut got).unwrap();
        got
    });
    (addr, tx, peer)
}

/// Runs `source` like `output_within`, with `release()` registered.
fn output_within_released(source: String, release: mpsc::Sender<()>) -> String {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let (mut vm, out) = vm();
        vm.register_fn("release", move || release.send(()).unwrap())
            .unwrap();
        let result = run(&mut vm, &source).map(|()| out.contents());
        let _ = tx.send(result.map_err(|e| e.to_string()));
    });
    match rx.recv_timeout(LIMIT) {
        Ok(Ok(text)) => text,
        Ok(Err(e)) => panic!("{e}"),
        Err(_) => panic!("the program did not finish within {LIMIT:?}: it hangs"),
    }
}

/// A `Drop` hook cannot suspend, so it blocks the interpreter thread. A coroutine that has a
/// read outstanding on the same connection holds the turn until it resumes, which it cannot do
/// while the hook blocks: the hook's read used to wait for ever. It is an error at once now.
#[test]
fn a_drop_hook_that_reads_while_a_coroutine_reads_gets_an_error_not_a_hang() {
    let (addr, release, peer) = hook_peer();
    let out = output_within_released(
        format!(
            r#"import net

class Guard {{ conn: net.Conn }}

impl Drop for Guard {{
    fn drop(self) {{
        match self.conn.read_line() {{
            ok(line) => print("drop hook read: {{line}}")
            err(e) => {{
                print("drop hook: {{e.as[net.Error]().expect("a net error")}}")
                print(e.message().contains("already waiting in another coroutine"))
                print(e.message().contains("`Drop` hook"))
            }}
        }}
    }}
}}

fn reader(c: net.Conn) {{
    print("coroutine read: {{c.read_line()}}")
}}

fn main() -> Result[()] {{
    let c = net.connect("{addr}")?
    let r = go reader(c)
    task.tick(0.0)
    // The peer sends now; nothing on this fiber lets the coroutine run before the hook does.
    release()
    var g: Option[Guard] = some(Guard {{ conn: c }})
    g = none
    r.join()
    // Nothing was lost: the coroutine got the first line, the next read gets the second.
    print("then: {{c.read_line()}}")
    print("done")
    return ok(())
}}
"#
        ),
        release,
    );
    peer.join().unwrap();
    assert_eq!(
        out,
        "drop hook: Other\ntrue\ntrue\ncoroutine read: ok(some(\"one\"))\nthen: ok(some(\"two\"))\ndone\n"
    );
}

/// The same for `accept`: a hook cannot take a connection while a coroutine waits in `accept` on
/// the same listener (the coroutine's `accept` still works afterwards).
#[test]
fn a_drop_hook_that_accepts_while_a_coroutine_accepts_gets_an_error_not_a_hang() {
    let out = output_within(
        r#"import net

class Guard { listener: net.Listener }

impl Drop for Guard {
    fn drop(self) {
        match self.listener.accept() {
            ok(_) => print("drop hook accepted")
            err(e) => {
                print("drop hook: {e.as[net.Error]().expect("a net error")}")
                print(e.message().contains("already waiting in another coroutine"))
            }
        }
    }
}

fn acceptor(l: net.Listener) {
    let c = l.accept().expect("accepted")
    c.write("hello\n").expect("written")
    c.close()
}

fn main() -> Result[()] {
    let l = net.listen("127.0.0.1:0")?
    let r = go acceptor(l)
    task.tick(0.0)
    var g: Option[Guard] = some(Guard { listener: l })
    g = none
    let c = net.connect(l.addr())?
    r.join()
    print(c.read_line()?)
    print("done")
    return ok(())
}
"#
        .to_string(),
        LIMIT,
    );
    assert_eq!(out, "drop hook: Other\ntrue\nsome(\"hello\")\ndone\n");
}

/// The controls: with no coroutine read outstanding the hook reads normally, and a write in the
/// hook works while a coroutine reads (writes have their own queue, and their turn is given back
/// on the worker).
#[test]
fn a_drop_hook_reads_normally_when_nothing_waits_and_writes_while_a_coroutine_reads() {
    let program = |addr: &str, body: &str, spawned: &str| {
        format!(
            r#"import net

class Guard {{ conn: net.Conn }}

impl Drop for Guard {{
    fn drop(self) {{
        {body}
    }}
}}

fn reader(c: net.Conn) {{
    print("coroutine read: {{c.read_line()}}")
}}

fn main() -> Result[()] {{
    let c = net.connect("{addr}")?
    let r = go {spawned}
    task.tick(0.0)
    release()
    var g: Option[Guard] = some(Guard {{ conn: c }})
    g = none
    r.join()
    print("done")
    c.close()
    return ok(())
}}
"#
        )
    };
    let (addr, release, peer) = hook_peer();
    let out = output_within_released(
        program(
            &addr,
            r#"print("drop hook reads: {self.conn.read_line()}")"#,
            r#"fn() { print("no read here") }()"#,
        ),
        release,
    );
    peer.join().unwrap();
    assert_eq!(
        out,
        "no read here\ndrop hook reads: ok(some(\"one\"))\ndone\n"
    );
    // A hook that writes while a coroutine reads: the write goes out whole, and the peer gets it
    // (it reads to the end of the stream, which `close` brings).
    let (addr, release, peer) = hook_peer();
    let out = output_within_released(
        program(
            &addr,
            r#"print("drop hook writes: {self.conn.write("x\n").is_ok()}")"#,
            "reader(c)",
        ),
        release,
    );
    assert_eq!(peer.join().unwrap(), "x\n");
    assert_eq!(
        out,
        "drop hook writes: true\ncoroutine read: ok(some(\"one\"))\ndone\n"
    );
}
