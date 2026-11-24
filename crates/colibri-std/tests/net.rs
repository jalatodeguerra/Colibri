//! `net` through the runtime API and against sockets of `std::net`: what the golden tests cannot
//! reach (hundreds of connections, transfers that hit back-pressure, cancellation, the `Vm`
//! ending, a reset, full duplex, two coroutines on one connection). Loopback only, no fixed
//! ports, no sleeps used for synchronization: every wait is for something that has to happen,
//! with a generous limit.

mod common;
use common::vm_pending;

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
    vm_pending("two_hundred_connections_each_exchange_data", || {
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

let mut connected = 0

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
    let mut clients: List[Coroutine[bool]] = []
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
    let mut good = 0
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
    })
}

/// A transfer much bigger than the socket buffers, with a receiver that reads in small steps
/// and pauses, so the sender's writes block on back-pressure. Both ends are coroutines of one
/// program; the digest of what was sent must equal the digest of what arrived.
#[test]
fn a_big_transfer_with_a_slow_reader_arrives_intact() {
    vm_pending("a_big_transfer_with_a_slow_reader_arrives_intact", || {
        let out = output(
            r#"import net
import hash

fn payload(size: int) -> Bytes {
    let b = BytesBuilder()
    let mut x = 12345
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
    let mut got = 0
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
    })
}

#[test]
fn a_rust_client_talks_to_a_colibri_server() {
    vm_pending("a_rust_client_talks_to_a_colibri_server", || {
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
    let mut n = 0
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
    })
}

#[test]
fn a_colibri_client_talks_to_a_rust_server() {
    vm_pending("a_colibri_client_talks_to_a_rust_server", || {
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
    })
}

#[test]
fn a_rust_client_gets_back_a_large_echo() {
    vm_pending("a_rust_client_gets_back_a_large_echo", || {
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
    })
}

#[test]
fn a_peer_that_resets_the_connection_is_reset() {
    vm_pending("a_peer_that_resets_the_connection_is_reset", || {
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
    })
}

/// One coroutine waits in `read` while another writes several times on the same connection:
/// the writes do not wait for the read.
#[test]
fn a_connection_is_full_duplex() {
    vm_pending("a_connection_is_full_duplex", || {
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
    })
}

/// Two reads on one connection take turns and never split a call; so do two writes.
#[test]
fn concurrent_reads_and_writes_on_one_connection_are_serialized() {
    vm_pending(
        "concurrent_reads_and_writes_on_one_connection_are_serialized",
        || {
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
    // In blocks: byte by byte takes seconds in a debug build, while the peer's read timeout runs.
    let block_one = BytesBuilder()
    let block_two = BytesBuilder()
    for _ in 0..1000 {{
        block_one.push(u8(1))
        block_two.push(u8(2))
    }}
    let bytes_one = block_one.to_bytes()
    let bytes_two = block_two.to_bytes()
    for _ in 0..3000 {{
        one.push_bytes(bytes_one)
        two.push_bytes(bytes_two)
    }}
    let r1 = go reader(c)
    let r2 = go reader(c)
    let w1 = go writer(c, one.to_bytes())
    let w2 = go writer(c, two.to_bytes())
    w1.join()
    w2.join()
    let a = r1.join()
    let b = r2.join()
    let mut both = [a, b]
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
        },
    )
}

#[test]
fn cancelling_a_blocked_accept_or_read_is_prompt_and_leaves_the_object_usable() {
    vm_pending(
        "cancelling_a_blocked_accept_or_read_is_prompt_and_leaves_the_object_usable",
        || {
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
        },
    )
}

/// `close()` on another coroutine's object ends its wait with `Closed`, promptly.
#[test]
fn closing_ends_a_blocked_accept_and_a_blocked_read() {
    vm_pending("closing_ends_a_blocked_accept_and_a_blocked_read", || {
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
    })
}

/// Ending the program with operations blocked returns at once and releases every socket.
#[test]
fn dropping_the_vm_with_blocked_operations_is_prompt_and_closes_the_sockets() {
    vm_pending(
        "dropping_the_vm_with_blocked_operations_is_prompt_and_closes_the_sockets",
        || {
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
        },
    )
}

/// A fake clock does not move while the program waits on the network: what a program does at
/// a fake time does not depend on how long the peer takes.
#[test]
fn a_fake_clock_does_not_advance_while_a_coroutine_waits_on_the_network() {
    vm_pending(
        "a_fake_clock_does_not_advance_while_a_coroutine_waits_on_the_network",
        || {
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

let mut ticks = 0

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
        },
    )
}

/// A line too long for `read_line`, and a line that is not text, are errors. The line that is
/// not text is taken off the stream (as `fs.File` does), the error says so, and the next line
/// is still there.
#[test]
fn read_line_refuses_a_huge_line_and_bad_utf8() {
    vm_pending("read_line_refuses_a_huge_line_and_bad_utf8", || {
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
    })
}

/// Operations of one kind on one connection are served in the order they were called, not in
/// the order their pool threads happen to start: a small write called after a big one waits
/// for it, and queued reads get the bytes in call order.
#[test]
fn operations_on_one_connection_are_served_in_call_order() {
    vm_pending(
        "operations_on_one_connection_are_served_in_call_order",
        || {
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
    let mut writers: List[Coroutine[()]] = []
    writers.push(go big(c, b.to_bytes()))
    writers.push(go small(c, "1"))
    writers.push(go small(c, "2"))
    writers.push(go small(c, "3"))
    writers.push(go small(c, "4"))
    writers.push(go small(c, "5"))
    writers.push(go small(c, "6"))
    writers.push(go small(c, "7"))
    writers.push(go small(c, "8"))
    let mut readers: List[Coroutine[str]] = []
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
        },
    )
}

/// An operation whose coroutine is cancelled before its turn never runs, and the ones behind
/// it are not held up.
#[test]
fn a_cancelled_queued_read_is_skipped() {
    vm_pending("a_cancelled_queued_read_is_skipped", || {
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
    })
}

// ---- regression tests for the review of the first version ----

/// A read that finished on its worker, whose coroutine is cancelled before it resumes, loses
/// nothing: the bytes stay for the next read. (The interpreter thread is held busy so the
/// worker is done before the cancel.)
#[test]
fn a_read_that_finished_before_its_coroutine_was_cancelled_loses_no_data() {
    vm_pending(
        "a_read_that_finished_before_its_coroutine_was_cancelled_loses_no_data",
        || {
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
        },
    )
}

/// A connection that arrived for a coroutine that was cancelled before it resumed is not lost:
/// the accept makes its system call only when its coroutine runs, so the cancelled one never
/// made it, and the connection is still in the listener's backlog for the next `accept`.
#[test]
fn an_accept_that_finished_before_its_coroutine_was_cancelled_loses_no_connection() {
    vm_pending(
        "an_accept_that_finished_before_its_coroutine_was_cancelled_loses_no_connection",
        || {
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
    // The interpreter thread is busy: the listener is ready, and the coroutine that waits for
    // it is cancelled before it resumes.
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
        },
    )
}

/// A connection that waits in the backlog of a listener (its accept was cancelled before it
/// made its system call) is closed with it. The listener value stays alive after `close()` until
/// the client has said what it saw, so the end of the stream comes from `close` and not from the
/// listener being dropped when the program ends.
///
/// Closing a listening socket resets the connections that wait in its backlog (on Linux and
/// macOS), so the client sees a reset or an end of the stream. What must never happen is a
/// connection that stays open: the client's read would then time out.
#[test]
fn closing_a_listener_closes_the_connections_it_kept() {
    vm_pending("closing_a_listener_closes_the_connections_it_kept", || {
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
    // The connection is in the backlog of `l`; closing `l` closes it.
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
            // The usual one: the connection was in the kernel's backlog, and a close resets it.
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => {}
            other => panic!("the client of a kept connection saw {other:?}, not an end or a reset"),
        }
    })
}

/// `defer c.close()` in a coroutine that is cancelled while its read waits: `close()` must not
/// suspend (a cancelled coroutine would skip the rest of its `defer`), and the descriptor must
/// be released when it returns, many times, so that the window where the worker still holds the
/// socket is hit.
#[test]
fn close_in_a_defer_of_a_cancelled_reader_runs_to_the_end() {
    vm_pending(
        "close_in_a_defer_of_a_cancelled_reader_runs_to_the_end",
        || {
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

let mut started = 0
let mut finished = 0

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
        },
    )
}

/// `close()` is not a scheduling point: no other coroutine runs between its call and the next
/// statement, even while a worker still holds the socket.
#[test]
fn close_is_not_a_scheduling_point() {
    vm_pending("close_is_not_a_scheduling_point", || {
        let (listener, addr) = rust_listener();
        let peer = thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            with_timeouts(&s);
            s.read(&mut [0u8; 1]).unwrap()
        });
        let out = output(&format!(
            r#"import net

let mut other_ran = false

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
    })
}

/// A write that times out after part of its data went out says how much, and that the stream
/// is out of step; the peer received exactly what the message says.
#[test]
fn a_write_that_times_out_says_how_much_was_sent() {
    vm_pending("a_write_that_times_out_says_how_much_was_sent", || {
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
    })
}

/// The text of a timeout is true when part of the line has arrived: it says the call did not
/// finish, not that nothing happened.
#[test]
fn a_timeout_in_the_middle_of_a_line_says_it_did_not_finish() {
    vm_pending(
        "a_timeout_in_the_middle_of_a_line_says_it_did_not_finish",
        || {
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
        },
    )
}

/// A deadline bounds the whole write, also when the peer reads steadily but slowly; and a
/// write whose coroutine is cancelled stops at once (the next send is not tried).
#[test]
fn a_big_write_stops_at_its_deadline_and_when_cancelled() {
    vm_pending(
        "a_big_write_stops_at_its_deadline_and_when_cancelled",
        || {
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
        },
    )
}

/// Many reads blocked at once (each holds one pool thread, see ARCHITECTURE §22) all end with
/// `Closed` when their connections are closed, and promptly.
#[test]
fn many_blocked_reads_end_when_their_connections_are_closed() {
    vm_pending(
        "many_blocked_reads_end_when_their_connections_are_closed",
        || {
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
    let mut conns: List[net.Conn] = []
    let mut readers: List[Coroutine[str]] = []
    for i in 0..50 {{
        let c = net.connect("{addr}")?
        conns.push(c)
        readers.push(go reading(c))
    }}
    wait(0.2)
    for c in conns {{
        c.close()
    }}
    let mut closed = 0
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
        },
    )
}

/// A bad count is an error like the one `fs.File.read` gives; a line of exactly the limit with
/// `\r\n` is a line; `read_exact` of a big block leaves what follows for the next read.
#[test]
fn counts_lines_at_the_limit_and_big_blocks() {
    vm_pending("counts_lines_at_the_limit_and_big_blocks", || {
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
    let mut good = true
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
    })
}

/// A big `read_exact` is correct and takes time in proportion to its size (the buffer grows
/// with what has arrived: a doubling, not a copy per piece).
#[test]
fn a_big_read_exact_over_loopback_is_correct_and_linear() {
    vm_pending(
        "a_big_read_exact_over_loopback_is_correct_and_linear",
        || {
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
    let mut bad = 0
    let mut i = 0
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
        },
    )
}

/// `connect_timeout` connects like `connect`, fails like it, and takes only a positive number.
#[test]
fn connect_timeout_connects_fails_and_checks_its_argument() {
    vm_pending(
        "connect_timeout_connects_fails_and_checks_its_argument",
        || {
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
        },
    )
}

/// `connect` by name: the lookup goes to a helper thread and the call goes on when it answers, and
/// `connect_timeout` by name does the same with a deadline over the whole call. The listener is
/// on `127.0.0.1` and `localhost` may give `::1` first, so the first attempt may be refused and
/// the next one connects.
#[test]
fn a_connect_by_name_finds_the_listener() {
    vm_pending("a_connect_by_name_finds_the_listener", || {
        let (listener, addr) = rust_listener();
        let port = addr.rsplit(':').next().unwrap().to_string();
        let peer = thread::spawn(move || {
            let mut got = Vec::new();
            for _ in 0..2 {
                let (mut s, _) = listener.accept().unwrap();
                with_timeouts(&s);
                let mut line = [0u8; 2];
                s.read_exact(&mut line).unwrap();
                got.push(line);
            }
            got
        });
        let out = output_within(
            format!(
                r#"import net

fn main() -> Result[()] {{
    let a = net.connect("localhost:{port}")?
    a.write("a\n")?
    let b = net.connect_timeout("localhost:{port}", 20.0)?
    b.write("b\n")?
    print("both")
    a.close()
    b.close()
    return ok(())
}}
"#
            ),
            LIMIT,
        );
        assert_eq!(out, "both\n");
        assert_eq!(peer.join().unwrap(), [*b"a\n", *b"b\n"]);
    })
}

/// A connect from a place that cannot suspend (a `Drop` hook) blocks the interpreter thread and
/// works: the lookup and the attempt are waited for on their completions. It connects, it is
/// refused where nothing listens.
#[test]
fn a_connect_from_a_drop_hook_connects_and_is_refused() {
    vm_pending("a_connect_from_a_drop_hook_connects_and_is_refused", || {
        let (listener, addr) = rust_listener();
        // A port that nothing is bound to: bound and let go of.
        let free = {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().to_string()
        };
        let peer = thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            drop(s);
        });
        let out = output_within(
            format!(
                r#"import net

class Guard {{ what: str }}

impl Drop for Guard {{
    fn drop(self) {{
        if self.what == "connect" {{
            match net.connect("{addr}") {{
                ok(c) => {{
                    print("connected")
                    c.close()
                }}
                err(e) => print("error {{e.as[net.Error]().expect("a net error")}}")
            }}
        }} else {{
            match net.connect_timeout("{free}", 20.0) {{
                ok(_) => print("connected")
                err(e) => print("error {{e.as[net.Error]().expect("a net error")}}")
            }}
        }}
    }}
}}

fn main() -> Result[()] {{
    let mut g: Option[Guard] = some(Guard {{ what: "connect" }})
    g = none
    g = some(Guard {{ what: "refused" }})
    g = none
    return ok(())
}}
"#
            ),
            LIMIT,
        );
        assert_eq!(out, "connected\nerror Refused\n");
        peer.join().unwrap();
    })
}

/// A connect that cannot finish ends at its timeout, and one that is cancelled ends at once.
/// Linux and macOS only: a listener whose accept queue is full leaves further connects waiting
/// there (the SYN is dropped; BSD, and so macOS, does the same as Linux), while Windows answers
/// an overflowing backlog with a reset, so a connect cannot be kept pending on loopback there.
/// For Windows the cancel of a connect that is under way is `a_cancelled_connect_leaves_no_socket`
/// in `net.rs`, which there connects to a port nothing listens on (a refused connect stays
/// pending for about two seconds), and the deadline of a connect is
/// `a_connect_that_is_late_stops_before_it_connects`, which does not need a connect to stay
/// pending.
///
/// How many connections fill the queue is not the same on every system (Linux: one more than the
/// backlog; BSD lets in half as much again), so the test connects until one does not finish, up
/// to a generous limit, and keeps what it made open. The backlog is 1 and not 0 because on macOS
/// a backlog of 0 (or less) means the system's maximum (`somaxconn`, 128 by default, and a
/// machine may set it higher than the limit here); 1 is the smallest queue there is on both.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn a_connect_that_does_not_finish_is_bounded_and_cancellable() {
    vm_pending(
        "a_connect_that_does_not_finish_is_bounded_and_cancellable",
        || {
            /// More than any queue of these systems takes (their default maximum is 128, and BSD lets in
            /// half as much again).
            const FILL_LIMIT: usize = 400;
            raise_descriptor_limit();
            // A listener nobody accepts from, with the smallest backlog there is (see above for 1).
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
            sock.listen(1).unwrap();
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
        },
    )
}

/// A write that stopped with some of its data sent leaves the stream out of step: the write
/// side is broken (`BrokenPipe`, with a text that says why), reading still works, and what the
/// peer got is the partial block and nothing after it.
#[test]
fn a_write_cancelled_part_way_breaks_the_write_side_and_not_the_read_side() {
    vm_pending(
        "a_write_cancelled_part_way_breaks_the_write_side_and_not_the_read_side",
        || {
            const SIZE: usize = 64 * 1024 * 1024;
            let (listener, addr) = rust_listener();
            let (tx, rx) = mpsc::channel::<()>();
            // Set by the peer once some of the data has arrived, so the write has really sent part of
            // it (and, as the peer reads no more until told, cannot finish) when it is cancelled.
            let arrived = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let seen = std::sync::Arc::clone(&arrived);
            let peer = thread::spawn(move || {
                let (mut s, _) = listener.accept().unwrap();
                with_timeouts(&s);
                let mut first = [0u8; 1];
                s.read_exact(&mut first).unwrap();
                seen.store(true, std::sync::atomic::Ordering::SeqCst);
                rx.recv_timeout(LIMIT).unwrap();
                s.write_all(b"hello\n").unwrap();
                let mut all = first.to_vec();
                s.read_to_end(&mut all).unwrap();
                all
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
    let mut tries = 0
    while !peer_has_data() {{
        tries += 1
        if tries > 2000 {{
            panic("no part of the write reached the peer")
        }}
        wait(0.01)
    }}
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
        },
    )
}

/// A write that was cancelled before it sent anything changes nothing: the connection is usable,
/// and what the peer gets is the first write whole and then the next one, not the cancelled one.
#[test]
fn a_write_cancelled_before_it_sent_anything_leaves_the_connection_usable() {
    vm_pending(
        "a_write_cancelled_before_it_sent_anything_leaves_the_connection_usable",
        || {
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
        },
    )
}

/// `close_write` takes its turn behind the writes called before it: `go c.write(big)` then
/// `c.close_write()` sends all of `big` and then the end of the stream (it used to cut the write
/// with a broken pipe). Also from a `Drop` hook, which blocks until the writes ahead are done.
#[test]
fn close_write_waits_for_the_writes_called_before_it() {
    vm_pending("close_write_waits_for_the_writes_called_before_it", || {
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
                "let mut g: Option[Guard] = some(Guard { conn: c })\n    g = none"
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
    })
}

/// `output_within`, with a `Vm` that `setup` has put host functions into.
fn output_within_setup(
    setup: impl FnOnce(&mut colibri::Vm) + Send + 'static,
    source: String,
    limit: Duration,
) -> String {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let (mut vm, out) = vm();
        setup(&mut vm);
        let result = run(&mut vm, &source).map(|()| out.contents());
        let _ = tx.send(result.map_err(|e| e.to_string()));
    });
    match rx.recv_timeout(limit) {
        Ok(Ok(text)) => text,
        Ok(Err(e)) => panic!("{e}"),
        Err(_) => panic!("the program did not finish within {limit:?}: it hangs"),
    }
}

/// A wake that said "ready" is owed its try even when the coroutine runs again after the
/// deadline: the poller reports the data before the deadline, the interpreter thread is held
/// (by a host function) until after it, and the read must still give the line, not a timeout.
///
/// Nothing here beats a fixed sleep. The host function first tells the peer to write and waits
/// until the peer says it has (so the data is there), then holds the thread until the clock is
/// past the deadline: the deadline is the read's call time plus the timeout, and the call came
/// before the host function began, so `began + timeout` is after it, measured and not assumed.
/// If a very slow machine delivered the wake only after the deadline, the call still gets its
/// line (the timeout wake is owed a try too), so the test cannot fail for being slow; it can
/// only stop testing the ready-then-late order.
#[test]
fn a_ready_wake_that_resumes_after_the_deadline_still_gets_its_read() {
    vm_pending(
        "a_ready_wake_that_resumes_after_the_deadline_still_gets_its_read",
        || {
            const TIMEOUT: f64 = 1.5;
            let (listener, addr) = rust_listener();
            let (write_now, told) = mpsc::channel::<()>();
            let (written, confirmed) = mpsc::channel::<()>();
            let peer = thread::spawn(move || {
                let (mut s, _) = listener.accept().unwrap();
                with_timeouts(&s);
                told.recv_timeout(LIMIT).unwrap();
                s.write_all(b"hi\n").unwrap();
                written.send(()).unwrap();
                // Until the script closes.
                let _ = s.read(&mut [0u8; 1]);
            });
            let out = output_within_setup(
                move |vm| {
                    vm.register_fn("hold", move || {
                        let began = Instant::now();
                        write_now.send(()).unwrap();
                        confirmed.recv_timeout(LIMIT).unwrap();
                        let past =
                            began + Duration::from_secs_f64(TIMEOUT) + Duration::from_millis(100);
                        while Instant::now() < past {
                            std::hint::spin_loop();
                        }
                    })
                    .unwrap();
                },
                format!(
                    r#"import net

fn reader(c: net.Conn) -> str {{
    match c.read_line() {{
        ok(_) => return "line"
        err(e) => return "{{e.as[net.Error]().expect("a net error")}}"
    }}
}}

fn main() -> Result[()] {{
    let c = net.connect("{addr}")?
    c.set_timeout({TIMEOUT})
    let r = go reader(c)
    task.tick(0.0)
    hold()
    print(r.join())
    c.close()
    return ok(())
}}
"#
                ),
                LIMIT,
            );
            peer.join().unwrap();
            assert_eq!(out, "line\n");
        },
    )
}

/// A coroutine in a loop of reads and writes against a peer that is always ready never has to
/// wait, so none of its calls suspends; the connection's burst budget makes it give the thread
/// to the others now and then anyway (`MAX_BURST_CALLS`). One coroutine echoes a hundred
/// thousand lines that an eager Rust peer has sent (every read is served from what is buffered
/// or from the socket, every write is taken at once), another counts how often it gets to run;
/// the count must have moved a good deal before the echo is done. No timing is involved: the
/// bound is on how many times the echo gave way, and a read or write that does suspend only adds
/// to it.
#[test]
fn a_loop_of_calls_that_never_wait_still_lets_the_other_coroutines_run() {
    vm_pending(
        "a_loop_of_calls_that_never_wait_still_lets_the_other_coroutines_run",
        || {
            const LINES: usize = 40_000;
            let (listener, addr) = rust_listener();
            let peer = thread::spawn(move || {
                let (mut s, _) = listener.accept().unwrap();
                with_timeouts(&s);
                let mut sending = s.try_clone().unwrap();
                let sender = thread::spawn(move || {
                    sending.write_all("x\n".repeat(LINES).as_bytes()).unwrap();
                });
                // Takes the echoes as they come, so that the script's writes never find the socket full.
                let echoed = read_n(&mut s, LINES * 2).len();
                sender.join().unwrap();
                echoed
            });
            let out = output_within(
                format!(
                    r#"import net

let mut ticks = 0
let mut finished = false

fn counter() {{
    while !finished {{
        ticks += 1
        wait(0.0)
    }}
}}

fn echo(c: net.Conn) {{
    for _ in 0..{LINES} {{
        match c.read_line().expect("a line") {{
            some(text) => c.write("{{text}}\n").expect("echoed")
            none => return
        }}
    }}
    finished = true
}}

fn main() -> Result[()] {{
    let c = net.connect("{addr}")?
    let k = go counter()
    let e = go echo(c)
    e.join()
    k.join()
    // 80,000 calls in all: one yield for every 64 of them is over 1,200 turns of the counter.
    print(ticks >= 500)
    c.close()
    return ok(())
}}
"#
                ),
                LIMIT,
            );
            assert_eq!(out, "true\n");
            assert_eq!(peer.join().unwrap(), LINES * 2);
        },
    )
}

/// A loop of accepts that never wait (the backlog is full of connections that arrived before)
/// lets the other coroutines run, as a loop of reads does: the listener has a budget of calls of
/// its own. No timing is involved; an accept that does wait only adds turns of the counter.
#[test]
fn a_loop_of_accepts_that_never_wait_still_lets_the_other_coroutines_run() {
    vm_pending(
        "a_loop_of_accepts_that_never_wait_still_lets_the_other_coroutines_run",
        || {
            // Few enough to fit the queue of a listener on every system (macOS admits about 192, Windows
            // about 200), as the connects of the client are blocking and none may be left waiting.
            const CONNECTIONS: usize = 100;
            let (addr_tx, addr_rx) = mpsc::channel::<String>();
            let (ready_tx, ready_rx) = mpsc::channel::<()>();
            let (done_tx, done_rx) = mpsc::channel::<()>();
            let clients = thread::spawn(move || {
                let addr = addr_rx.recv_timeout(LIMIT).unwrap();
                let mut held = Vec::new();
                for _ in 0..CONNECTIONS {
                    held.push(TcpStream::connect(&addr).unwrap());
                }
                ready_tx.send(()).unwrap();
                // Kept open until the script is done.
                let _ = done_rx.recv_timeout(LIMIT);
                drop(held);
            });
            let (mut vm, out) = vm();
            vm.register_fn("publish", move |a: String| addr_tx.send(a).unwrap())
                .unwrap();
            vm.register_fn("clients_are_connected", move || {
                ready_rx.recv_timeout(LIMIT).unwrap()
            })
            .unwrap();
            run(
                &mut vm,
                &format!(
                    r#"import net

let mut ticks = 0
let mut finished = false

fn counter() {{
    while !finished {{
        ticks += 1
        wait(0.0)
    }}
}}

fn main() -> Result[()] {{
    let l = net.listen("127.0.0.1:0")?
    publish(l.addr())
    clients_are_connected()
    let k = go counter()
    for _ in 0..{CONNECTIONS} {{
        let c = l.accept()?
        c.close()
    }}
    finished = true
    k.join()
    // 100 accepts: one turn of the counter at call 64, and the one it had when it started.
    print(ticks >= 2)
    l.close()
    return ok(())
}}
"#
                ),
            )
            .unwrap_or_else(|e| panic!("{e}"));
            done_tx.send(()).unwrap();
            clients.join().unwrap();
            assert_eq!(out.contents(), "true\n");
        },
    )
}

/// A `Drop` hook that writes while a coroutine is in the middle of a write on the same connection
/// is refused at once, as a hook's read behind a read is: the hook blocks the interpreter thread
/// and the rest of the write can only be sent by the coroutine, so waiting would be for ever. A
/// `close_write` from the hook is not refused: it takes its place behind the write and returns at
/// once, and the writing end is shut when the write is done. Nothing is lost: the peer gets the
/// write whole and then the end of the stream (which it answers, so that the script cannot have
/// ended the stream by closing the connection).
#[test]
fn a_drop_hook_gets_an_error_for_a_write_and_a_scheduled_end_for_close_write() {
    vm_pending(
        "a_drop_hook_gets_an_error_for_a_write_and_a_scheduled_end_for_close_write",
        || {
            // More than a connection's buffers hold and more than one burst, so that the writer is
            // still in the middle of its write when the hook runs.
            const SIZE: usize = 64 * 1024 * 1024;
            let (listener, addr) = rust_listener();
            let (go_on, hold) = mpsc::channel::<()>();
            let peer = thread::spawn(move || {
                let (mut s, _) = listener.accept().unwrap();
                with_timeouts(&s);
                // Reads nothing until the script has made its two calls.
                hold.recv_timeout(LIMIT).unwrap();
                let mut all = Vec::new();
                s.read_to_end(&mut all).unwrap();
                // Only the shutdown can have ended the stream: the script is waiting for this byte.
                s.write_all(b"k").unwrap();
                all
            });
            let out = output_within_released(
                format!(
                    r#"import net

class Guard {{ conn: net.Conn, close_end: bool }}

impl Drop for Guard {{
    fn drop(self) {{
        if self.close_end {{
            print("hook close_write: {{self.conn.close_write().is_ok()}}")
        }} else {{
            match self.conn.write("x") {{
                ok(_) => print("hook write: done")
                err(e) => {{
                    print("hook write: {{e.as[net.Error]().expect("a net error")}}")
                    print(e.message().contains("already waiting in another coroutine"))
                }}
            }}
        }}
    }}
}}

fn sender(c: net.Conn, data: Bytes) {{
    c.write_bytes(data).expect("the write is not cut")
    print("written")
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
    let mut g: Option[Guard] = some(Guard {{ conn: c, close_end: false }})
    g = none
    g = some(Guard {{ conn: c, close_end: true }})
    g = none
    release()
    w.join()
    print(c.read_exact(1)?.to_str_lossy())
    c.close()
    return ok(())
}}
"#,
                    blocks = SIZE / 65536
                ),
                go_on,
            );
            assert_eq!(
                out,
                "hook write: Other\ntrue\nhook close_write: true\nwritten\nk\n"
            );
            let got = peer.join().unwrap();
            assert_eq!(got.len(), SIZE);
            assert!(got.iter().all(|&b| b == 120));
        },
    )
}

/// The script of the tests that follow: a coroutine `w` writes 64 MiB of `x` on `c` (more than
/// the connection's buffers hold, so that it is in the middle of its write while the peer reads
/// nothing), and `Guard` is a `Drop` hook that does `what` to the connection (`write`,
/// `close_write` or `close`) and prints the outcome. `main_body` runs after `w` was started.
fn hook_script(addr: &str, sender: &str, main_body: &str) -> String {
    format!(
        r#"import net

class Guard {{ conn: net.Conn, what: str }}

fn hook(c: net.Conn, what: str) {{
    let mut g: Option[Guard] = some(Guard {{ conn: c, what: what }})
    g = none
}}

impl Drop for Guard {{
    fn drop(self) {{
        if self.what == "close_write" {{
            print("hook close_write: {{self.conn.close_write().is_ok()}}")
        }} else if self.what == "close" {{
            self.conn.close()
            print("hook close")
        }} else {{
            match self.conn.write("x") {{
                ok(_) => print("hook write: done")
                err(e) => print("hook write: {{e.as[net.Error]().expect("a net error")}}")
            }}
        }}
    }}
}}

{sender}

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
{main_body}
    return ok(())
}}
"#,
        blocks = HOOK_SIZE / 65536
    )
}

const HOOK_SIZE: usize = 64 * 1024 * 1024;

/// What the peer of a `hook_script` does once it is let go: reads to the end (and answers with
/// one byte, so that the script can wait for the end of the stream without closing), or drops the
/// connection at once.
fn hook_peer_big(
    listener: TcpListener,
    hold: mpsc::Receiver<()>,
    read: bool,
) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        with_timeouts(&s);
        hold.recv_timeout(LIMIT).unwrap();
        if !read {
            return Vec::new();
        }
        let mut all = Vec::new();
        s.read_to_end(&mut all).unwrap();
        // Lost when the script closed the connection without waiting for it.
        let _ = s.write_all(b"k");
        all
    })
}

const COUNTING_SENDER: &str = r#"fn sender(c: net.Conn, data: Bytes) {
    match c.write_bytes(data) {
        ok(_) => print("written")
        err(e) => print("write: {e.as[net.Error]().expect("a net error")}")
    }
}"#;

/// Two `close_write`s from hooks behind one unfinished write are two places in the write order,
/// and a coroutine's write called after them is behind both: it gets the error a write after a
/// `close_write` gets, and none of its bytes reach the peer. The write ahead is whole and the
/// peer gets the end of the stream after it.
#[test]
fn two_scheduled_close_writes_and_a_write_called_after_them() {
    vm_pending(
        "two_scheduled_close_writes_and_a_write_called_after_them",
        || {
            let (listener, addr) = rust_listener();
            let (go_on, hold) = mpsc::channel::<()>();
            let peer = hook_peer_big(listener, hold, true);
            let sender = format!(
                r#"{COUNTING_SENDER}

fn late(c: net.Conn) {{
    match c.write("late") {{
        ok(_) => print("late write: done")
        err(e) => print("late write: {{e.as[net.Error]().expect("a net error")}}")
    }}
}}"#
            );
            let main_body = r#"    hook(c, "close_write")
    hook(c, "close_write")
    hook(c, "write")
    let l = go late(c)
    task.tick(0.0)
    release()
    w.join()
    l.join()
    print(c.read_exact(1)?.to_str_lossy())
    c.close()"#;
            let out = output_within_released(hook_script(&addr, &sender, main_body), go_on);
            assert_eq!(
                out,
                "hook close_write: true\nhook close_write: true\nhook write: BrokenPipe\nwritten\nlate write: BrokenPipe\nk\n"
            );
            let got = peer.join().unwrap();
            assert_eq!(got.len(), HOOK_SIZE, "no byte of the late write");
            assert!(got.iter().all(|&b| b == 120));
        },
    )
}

/// The write ahead of a scheduled shutdown ends early, and the shutdown still happens: the writer
/// is cancelled (the peer gets a cut stream and then the end), or fails (the peer is gone, the
/// shutdown has nobody to tell and the script goes on).
#[test]
fn a_scheduled_close_write_runs_when_the_write_ahead_is_cancelled_or_fails() {
    vm_pending(
        "a_scheduled_close_write_runs_when_the_write_ahead_is_cancelled_or_fails",
        || {
            // Cancelled.
            let (listener, addr) = rust_listener();
            let (go_on, hold) = mpsc::channel::<()>();
            let peer = hook_peer_big(listener, hold, true);
            let main_body = r#"    hook(c, "close_write")
    w.cancel()
    release()
    print(c.read_exact(1)?.to_str_lossy())
    c.close()"#;
            let out = output_within_released(hook_script(&addr, COUNTING_SENDER, main_body), go_on);
            assert_eq!(out, "hook close_write: true\nk\n");
            let got = peer.join().unwrap();
            assert!(got.len() < HOOK_SIZE, "the cancelled write was cut");
            // Failing: the peer lets go of the connection without reading.
            let (listener, addr) = rust_listener();
            let (go_on, hold) = mpsc::channel::<()>();
            let peer = hook_peer_big(listener, hold, false);
            let main_body = r#"    hook(c, "close_write")
    release()
    w.join()
    c.close()"#;
            let out = output_within_released(hook_script(&addr, COUNTING_SENDER, main_body), go_on);
            assert!(out.starts_with("hook close_write: true\nwrite: "), "{out}");
            assert!(out.ends_with('\n') && out.lines().count() == 2, "{out}");
            peer.join().unwrap();
        },
    )
}

/// `close()` while a shutdown is scheduled: the writer ahead gets `Closed`, the scheduled shutdown
/// finds the connection closed and does nothing, and nothing hangs.
#[test]
fn close_before_a_scheduled_close_write_runs_skips_it() {
    vm_pending("close_before_a_scheduled_close_write_runs_skips_it", || {
        let (listener, addr) = rust_listener();
        let (go_on, hold) = mpsc::channel::<()>();
        let peer = hook_peer_big(listener, hold, true);
        let main_body = r#"    hook(c, "close_write")
    c.close()
    w.join()
    release()"#;
        let out = output_within_released(hook_script(&addr, COUNTING_SENDER, main_body), go_on);
        assert_eq!(out, "hook close_write: true\nwrite: Closed\n");
        // The close cut the stream and ended it (the connection is closed, so the answer is lost).
        let got = peer.join().unwrap();
        assert!(got.len() < HOOK_SIZE, "the stream was cut");
    })
}

/// The `Vm` goes away with a shutdown scheduled behind an unfinished write: the program ends and
/// the peer gets the end of the stream, cut where the write was.
#[test]
fn a_vm_that_ends_with_a_scheduled_close_write_does_not_hang() {
    vm_pending(
        "a_vm_that_ends_with_a_scheduled_close_write_does_not_hang",
        || {
            let (listener, addr) = rust_listener();
            let (go_on, hold) = mpsc::channel::<()>();
            let peer = hook_peer_big(listener, hold, true);
            let main_body = r#"    hook(c, "close_write")
    release()"#;
            let out = output_within_released(hook_script(&addr, COUNTING_SENDER, main_body), go_on);
            assert!(out.starts_with("hook close_write: true\n"), "{out}");
            // Whether the answer of the peer arrived does not matter: the end of the stream came.
            let got = peer.join().unwrap();
            assert!(got.len() <= HOOK_SIZE);
        },
    )
}

/// `close()` from a `Drop` hook while another coroutine is in the middle of a write: the writer
/// gets `Closed` and is told how much it had sent, and the peer gets that much, cut, and then the
/// end of the stream. (The writer that is in `give_way` is the unit test
/// `a_close_during_a_give_way_releases_the_socket_of_a_read_and_of_a_write`.)
#[test]
fn a_close_from_a_hook_while_another_coroutine_is_writing_cuts_its_write() {
    vm_pending(
        "a_close_from_a_hook_while_another_coroutine_is_writing_cuts_its_write",
        || {
            let (listener, addr) = rust_listener();
            let (go_on, hold) = mpsc::channel::<()>();
            let peer = thread::spawn(move || {
                let (mut s, _) = listener.accept().unwrap();
                with_timeouts(&s);
                hold.recv_timeout(LIMIT).unwrap();
                let mut all = Vec::new();
                // The end of the stream: the connection was closed under the write.
                s.read_to_end(&mut all).unwrap();
                all.len()
            });
            let sender = r#"fn sender(c: net.Conn, data: Bytes) {
    match c.write_bytes(data) {
        ok(_) => print("written")
        err(e) => {
            print("write: {e.as[net.Error]().expect("a net error")}")
            print(e.message())
        }
    }
}"#;
            let main_body = r#"    hook(c, "close")
    w.join()
    release()"#;
            let out = output_within_released(hook_script(&addr, sender, main_body), go_on);
            let got = peer.join().unwrap();
            let mut lines = out.lines();
            assert_eq!(lines.next(), Some("hook close"), "{out}");
            assert_eq!(lines.next(), Some("write: Closed"), "{out}");
            let message = lines.next().unwrap_or_default();
            let sent: usize = message
                .rsplit('(')
                .next()
                .and_then(|tail| tail.split(' ').next())
                .and_then(|n| n.parse().ok())
                .unwrap_or_else(|| panic!("the writer is told how much was sent: {message}"));
            assert!(
                message.contains(&format!("of {HOOK_SIZE} bytes were sent")),
                "{message}"
            );
            assert!(sent > 0 && sent < HOOK_SIZE, "{sent}");
            assert!(
                got > 0 && got <= sent,
                "the peer got {got} of the {sent} sent"
            );
        },
    )
}

/// A write that stopped part way (here: cancelled) breaks the write side, and `close_write` behind
/// it still shuts the writing end: it is the only way left to tell the peer that nothing more
/// comes, and it does not fail for what an earlier write did. Writes after it fail as before.
#[test]
fn close_write_after_a_cut_write_still_ends_the_stream() {
    vm_pending(
        "close_write_after_a_cut_write_still_ends_the_stream",
        || {
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
    let mut tries = 0
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
        },
    )
}

/// `close()` does not wait for anyone: two hundred sockets with a blocked acceptor, reader or
/// writer on each close together in far less than the 50 ms a poller's slice would cost each
/// (ten seconds for all of them), and every waiter ends with `Closed`.
#[test]
fn closing_two_hundred_sockets_with_waiters_does_not_wait_for_any_slice() {
    vm_pending(
        "closing_two_hundred_sockets_with_waiters_does_not_wait_for_any_slice",
        || {
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
    let mut listeners: List[net.Listener] = []
    let mut readers: List[net.Conn] = []
    let mut writers: List[net.Conn] = []
    let mut tasks: List[Coroutine[str]] = []
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
    let mut closed = 0
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
        },
    )
}

/// Binding a specific address of a port whose wildcard address is held is `AddrInUse` on Linux
/// and on Windows (`SO_EXCLUSIVEADDRUSE`). Not on macOS: there `SO_REUSEADDR`, which `listen`
/// sets so that a server can restart, lets a socket bind a specific address next to a
/// wildcard one (the BSD rule), so the answer differs there and the docs say so.
#[cfg(any(target_os = "linux", windows))]
#[test]
fn a_specific_address_of_a_port_held_on_the_wildcard_is_in_use() {
    vm_pending(
        "a_specific_address_of_a_port_held_on_the_wildcard_is_in_use",
        || {
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
        },
    )
}

/// A server that closes a connection first leaves the port in `TIME_WAIT` on its side, and the
/// port must still be listenable again after the listener is closed: `kvserver` does this on
/// SHUTDOWN and `--idle`. On Linux and macOS `SO_REUSEADDR` allows it; on Windows `listen` sets
/// `SO_EXCLUSIVEADDRUSE`, whose rule for a port with connections of its own still around is
/// what this test checks (it runs on Windows in CI and passes). No retry: the restart must work
/// at once.
#[test]
fn a_port_can_be_listened_on_again_after_the_server_closed_a_connection_first() {
    vm_pending(
        "a_port_can_be_listened_on_again_after_the_server_closed_a_connection_first",
        || {
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
        },
    )
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
    vm_pending(
        "a_write_to_a_peer_that_reset_is_a_broken_pipe_and_a_read_is_a_reset",
        || {
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
    let mut last = "ok"
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
        },
    )
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
    vm_pending(
        "a_drop_hook_that_reads_while_a_coroutine_reads_gets_an_error_not_a_hang",
        || {
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
    let mut g: Option[Guard] = some(Guard {{ conn: c }})
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
        },
    )
}

/// The same for `accept`: a hook cannot take a connection while a coroutine waits in `accept` on
/// the same listener (the coroutine's `accept` still works afterwards).
#[test]
fn a_drop_hook_that_accepts_while_a_coroutine_accepts_gets_an_error_not_a_hang() {
    vm_pending(
        "a_drop_hook_that_accepts_while_a_coroutine_accepts_gets_an_error_not_a_hang",
        || {
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
    let mut g: Option[Guard] = some(Guard { listener: l })
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
        },
    )
}

/// The controls: with no coroutine read outstanding the hook reads normally, and a write in the
/// hook works while a coroutine reads (writes have their own queue, and their turn is given back
/// on the worker).
#[test]
fn a_drop_hook_reads_normally_when_nothing_waits_and_writes_while_a_coroutine_reads() {
    vm_pending(
        "a_drop_hook_reads_normally_when_nothing_waits_and_writes_while_a_coroutine_reads",
        || {
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
    let mut g: Option[Guard] = some(Guard {{ conn: c }})
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
        },
    )
}

// One waiter among a thousand ends promptly. What "promptly" means here has no sleep in it: the
// call that is waited for ends while the other 999 are still waiting (none of them has ended when
// the script sees the end of this one: an order of events), and the time from the event to the
// end stays under a ceiling that is generous for a loaded machine and far above what an end takes
// (milliseconds), so that only an end that waits for something other than its own event (a scan of
// all the waiters, the deadline of another waiter, a periodic round) can miss it.

const WAITERS: usize = 1000;
/// The last connection made, so that the deadline of the timeout scenario (which starts when its
/// read starts) begins when every other reader is already waiting, however slow the machine.
const TARGET: usize = WAITERS - 1;
const PROMPT: Duration = Duration::from_secs(5);

/// The thousand-connection scenarios do not run side by side: each holds two thousand
/// descriptors, and macOS allows 10,240 to a process.
static THOUSAND: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[derive(Clone, Copy)]
enum Event {
    /// The peer sends one byte on the target's connection.
    Data,
    /// The peer closes the target's connection.
    PeerClose,
    /// The script cancels the coroutine that reads.
    Cancel,
    /// The target's connection has a timeout of half a second; the others have a minute.
    Timeout,
}

/// Runs the scenario and returns what the script printed, and the time from the event to the end
/// of the target's call.
fn one_among_a_thousand(event: Event) -> (String, Duration) {
    let _one_at_a_time = THOUSAND.lock().unwrap_or_else(|e| e.into_inner());
    let (listener, addr) = rust_listener();
    let (tx, rx) = mpsc::channel::<()>();
    let release = tx.clone();
    let peer = thread::spawn(move || {
        let mut held = Vec::new();
        for _ in 0..WAITERS {
            let (s, _) = listener.accept().unwrap();
            with_timeouts(&s);
            held.push(Some(s));
        }
        // The script says when every reader is waiting, and when it is done.
        rx.recv_timeout(LIMIT)
            .expect("the script's readers are waiting");
        match event {
            Event::Data => {
                held[TARGET].as_mut().unwrap().write_all(b"x").unwrap();
            }
            Event::PeerClose => drop(held[TARGET].take()),
            Event::Cancel | Event::Timeout => {}
        }
        rx.recv_timeout(LIMIT).expect("the script ends");
    });
    let (mut vm, out) = vm();
    let marks = std::sync::Arc::new(std::sync::Mutex::new((None::<Instant>, None::<Instant>)));
    let m = std::sync::Arc::clone(&marks);
    vm.register_fn("ready", move || {
        m.lock().unwrap().0 = Some(Instant::now());
        tx.send(()).unwrap();
    })
    .unwrap();
    let m = std::sync::Arc::clone(&marks);
    vm.register_fn("target_ended", move || {
        m.lock().unwrap().1 = Some(Instant::now());
    })
    .unwrap();
    let act = match event {
        Event::Cancel => "readers[TARGET].cancel()",
        _ => "",
    };
    let timeouts = match event {
        Event::Timeout => "c.set_timeout(if i == TARGET { 0.5 } else { 60.0 })",
        _ => "",
    };
    let wait_for_target = match event {
        Event::Cancel => "",
        _ => "let result = readers[TARGET].join()",
    };
    let result = match event {
        Event::Cancel => r#"readers[TARGET].done()"#,
        _ => "result",
    };
    run(
        &mut vm,
        &format!(
            r#"import net

let TARGET = {TARGET}
let mut started = 0
let mut ended = 0

fn reading(c: net.Conn) -> str {{
    started += 1
    let r = c.read(1)
    ended += 1
    match r {{
        ok(b) => return "data {{b.len()}}"
        err(e) => return "{{e.as[net.Error]().expect("a net error")}}"
    }}
}}

fn main() -> Result[()] {{
    let mut conns: List[net.Conn] = []
    let mut readers: List[Coroutine[str]] = []
    for i in 0..{WAITERS} {{
        let c = net.connect("{addr}")?
        {timeouts}
        conns.push(c)
        readers.push(go reading(c))
    }}
    // The script runs again only when every reader that has started is suspended in its read.
    while started < {WAITERS} {{
        task.tick(0.0)
    }}
    let ended_at_ready = ended
    ready()
    {act}
    {wait_for_target}
    target_ended()
    print("ended when the readers were waiting: {{ended_at_ready}}")
    print("target {{{result}}}, ended before the rest: {{ended}}")
    for c in conns {{
        c.close()
    }}
    let mut closed = 0
    for i in 0..{WAITERS} {{
        if i != TARGET {{
            if readers[i].join() == "Closed" {{
                closed += 1
            }}
        }}
    }}
    print("closed {{closed}}")
    return ok(())
}}
"#
        ),
    )
    .unwrap_or_else(|e| panic!("{e}"));
    release.send(()).unwrap();
    peer.join().unwrap();
    let (ready, ended) = *marks.lock().unwrap();
    let took = ended
        .expect("the target's end was seen")
        .saturating_duration_since(ready.expect("the readers were waiting"));
    (out.contents(), took)
}

#[test]
fn one_read_among_a_thousand_ends_promptly_on_data() {
    vm_pending("one_read_among_a_thousand_ends_promptly_on_data", || {
        let (out, took) = one_among_a_thousand(Event::Data);
        assert_eq!(
            out,
            format!(
                "ended when the readers were waiting: 0\ntarget data 1, ended before the rest: 1\nclosed {}\n",
                WAITERS - 1
            )
        );
        assert!(took < PROMPT, "took {took:?}");
    })
}

#[test]
fn one_read_among_a_thousand_ends_promptly_on_the_close_of_the_peer() {
    vm_pending(
        "one_read_among_a_thousand_ends_promptly_on_the_close_of_the_peer",
        || {
            let (out, took) = one_among_a_thousand(Event::PeerClose);
            assert_eq!(
                out,
                format!(
                    "ended when the readers were waiting: 0\ntarget data 0, ended before the rest: 1\nclosed {}\n",
                    WAITERS - 1
                )
            );
            assert!(took < PROMPT, "took {took:?}");
        },
    )
}

#[test]
fn one_read_among_a_thousand_ends_promptly_on_cancel() {
    vm_pending("one_read_among_a_thousand_ends_promptly_on_cancel", || {
        let (out, took) = one_among_a_thousand(Event::Cancel);
        assert_eq!(
            out,
            format!(
                "ended when the readers were waiting: 0\ntarget true, ended before the rest: 0\nclosed {}\n",
                WAITERS - 1
            )
        );
        assert!(took < PROMPT, "took {took:?}");
    })
}

#[test]
fn one_read_among_a_thousand_ends_promptly_on_its_timeout() {
    vm_pending(
        "one_read_among_a_thousand_ends_promptly_on_its_timeout",
        || {
            let (out, took) = one_among_a_thousand(Event::Timeout);
            assert_eq!(
                out,
                format!(
                    "ended when the readers were waiting: 0\ntarget Timeout, ended before the rest: 1\nclosed {}\n",
                    WAITERS - 1
                )
            );
            assert!(took < PROMPT, "took {took:?}");
        },
    )
}

/// The port of a listener is free when `close()` returns, also while an `accept` waits on the
/// listener at that moment (the case in which a close once had to wake the wait first): nothing
/// holds the socket while the accept is suspended, so the descriptor is released by the close.
/// No sleep, no retry. The script says which step failed.
#[test]
fn a_port_can_be_listened_on_again_at_once_after_a_close_with_an_accept_waiting() {
    vm_pending(
        "a_port_can_be_listened_on_again_at_once_after_a_close_with_an_accept_waiting",
        || {
            let out = output_within(
                r#"import net

let mut started = 0

fn accepting(l: net.Listener) -> str {
    started += 1
    match l.accept() {
        ok(_) => return "accepted"
        err(e) => return "{e.as[net.Error]().expect("a net error")}"
    }
}

fn main() -> Result[()] {
    let l = net.listen("127.0.0.1:0")?
    let addr = l.addr()
    let a = go accepting(l)
    // The script runs again only when the accept is suspended in its wait.
    while started < 1 {
        task.tick(0.0)
    }
    l.close()
    match net.listen(addr) {
        err(e) => print("step 2, listening again right after the close: {e.message()}")
        ok(again) => {
            print("listening again: ok")
            // And it serves on the port it got back.
            let c = net.connect(addr)?
            let s = again.accept()?
            c.write("hi\n")?
            print(s.read_line()?)
        }
    }
    print("step 1, the waiting accept ended with: {a.join()}")
    return ok(())
}
"#
                .to_string(),
                LIMIT,
            );
            assert_eq!(
                out,
                "listening again: ok\nsome(\"hi\")\nstep 1, the waiting accept ended with: Closed\n"
            );
        },
    )
}

// ---- `read_until` and `set_line_limit` ----

/// A script's name for what a call gave: the kind of an error, or the value.
const SHOW: &str = r#"
fn kind(e: Error) -> str {
    match e.as[net.Error]() {
        some(k) => return "{k}"
        none => return "other error"
    }
}

fn show(r: Result[Bytes?]) -> str {
    match r {
        ok(some(b)) => return "{b.len()} bytes"
        ok(none) => return "none"
        err(e) => return kind(e)
    }
}
"#;

/// A Rust peer sends `a\nb\r\n` and `read_until("\r\n")` gives `a\nb` (a bare `\n` is not the
/// end), an empty piece is `some` of empty bytes, a piece over `max` is `LineTooLong` with its
/// bytes still in the stream, and the end of the stream is told from a limit: `none` with nothing
/// pending, `UnexpectedEnd` with bytes pending (which stay readable).
#[test]
fn read_until_takes_the_bytes_before_a_delimiter_and_tells_a_limit_from_the_end() {
    vm_pending(
        "read_until_takes_the_bytes_before_a_delimiter_and_tells_a_limit_from_the_end",
        || {
            let (listener, addr) = rust_listener();
            let peer = thread::spawn(move || {
                let (mut s, _) = listener.accept().unwrap();
                with_timeouts(&s);
                s.write_all(b"a\nb\r\n").unwrap();
                s.write_all(b"\r\n").unwrap();
                s.write_all(b"0123456789abcdefg\r\n").unwrap();
                s.write_all(b"tail").unwrap();
                // The end of the stream, once the script has read all of it.
                s.shutdown(std::net::Shutdown::Write).unwrap();
                let _ = s.read(&mut [0u8; 1]);
            });
            let out = output(&format!(
                r#"import net
{SHOW}
fn main() -> Result[()] {{
    let c = net.connect("{addr}")?
    let crlf = "\r\n".to_bytes()
    let first = c.read_until(crlf, 64)?.expect("a piece")
    print(first.len())
    print(first == "a\nb".to_bytes())
    print(c.read_until(crlf, 64)?.expect("a piece").len())
    // 17 bytes before the delimiter: over a `max` of 16, under one of 17.
    let over = c.read_until(crlf, 16)
    print(show(over))
    print(over.err().expect("too long").message().contains("16"))
    // The bytes are still there, delimiter included.
    print(c.read_until(crlf, 17)?.expect("the piece").len())
    // The end of the stream with `tail` pending.
    print(show(c.read_until(crlf, 64)))
    print(c.read(10)?.len())
    print(show(c.read_until(crlf, 64)))
    print(show(c.read_until(crlf, 64)))
    c.close()
    return ok(())
}}
"#
            ));
            peer.join().unwrap();
            assert_eq!(
                out,
                "3\ntrue\n0\nLineTooLong\ntrue\n17\nUnexpectedEnd\n4\nnone\nnone\n"
            );
        },
    )
}

/// The limit of `read_line` set to 16 gives `LineTooLong` at 17 bytes and not at 16, with `\r\n`
/// and with `\n`, and `read_until` has its own `max`, which `set_line_limit` does not change.
#[test]
fn the_line_limit_is_at_one_byte_more_than_it_says_and_read_until_has_its_own() {
    vm_pending(
        "the_line_limit_is_at_one_byte_more_than_it_says_and_read_until_has_its_own",
        || {
            let (listener, addr) = rust_listener();
            let peer = thread::spawn(move || {
                let (mut s, _) = listener.accept().unwrap();
                with_timeouts(&s);
                let x = |n: usize, end: &[u8]| {
                    let mut v = vec![b'x'; n];
                    v.extend_from_slice(end);
                    v
                };
                for piece in [
                    x(16, b"\r\n"),
                    x(16, b"\n"),
                    x(17, b"\r\n"),
                    // What `read_until` gets: its own limit, 40, over the line limit of 16.
                    x(30, b"\r\n"),
                    x(41, b"\r\n"),
                ] {
                    s.write_all(&piece).unwrap();
                }
                let _ = s.read(&mut [0u8; 1]);
            });
            let out = output(&format!(
                r#"import net
{SHOW}
fn line(r: Result[str?]) -> str {{
    match r {{
        ok(some(text)) => return "{{text.len()}} chars"
        ok(none) => return "none"
        err(e) => return kind(e)
    }}
}}

fn main() -> Result[()] {{
    let c = net.connect("{addr}")?
    c.set_line_limit(16)
    print(line(c.read_line()))
    print(line(c.read_line()))
    // 17 bytes without a line end: too long, and the bytes stay (this is what `read_exact` finds).
    let long = c.read_line()
    print(line(long))
    print(c.read_exact(19)?.len())
    // `read_until` is not bound by `set_line_limit`.
    let crlf = "\r\n".to_bytes()
    print(show(c.read_until(crlf, 40)))
    print(show(c.read_until(crlf, 40)))
    c.close()
    return ok(())
}}
"#
            ));
            peer.join().unwrap();
            assert_eq!(
                out,
                "16 chars\n16 chars\nLineTooLong\n19\n30 bytes\nLineTooLong\n"
            );
        },
    )
}

/// `set_line_limit` made while a `read_line` waits counts at that call's next look (the limit is
/// read where the call looks, not where it began): lowered, a line that was fine when the call
/// began is too long when more of it arrives; raised, a line that would have been too long is
/// taken. The peer sends the second part only when the script has changed the limit, so the order
/// is fixed and no wait decides it.
#[test]
fn a_changed_line_limit_applies_to_a_waiting_read_line_at_its_next_look() {
    vm_pending(
        "a_changed_line_limit_applies_to_a_waiting_read_line_at_its_next_look",
        || {
            for (before, after, second, expected) in [
                // Lowered to 16 with 10 bytes buffered: 10 more bytes make it 20 without a line end.
                (1000, 16, 10, "LineTooLong"),
                // Raised to 100 with 10 bytes buffered: 30 more and an end make a line of 40.
                (16, 100, 30, "40 chars"),
            ] {
                let (listener, addr) = rust_listener();
                let (go_on, told) = mpsc::channel::<()>();
                let peer = thread::spawn(move || {
                    let (mut s, _) = listener.accept().unwrap();
                    with_timeouts(&s);
                    s.write_all(&[b'x'; 10]).unwrap();
                    told.recv_timeout(LIMIT).unwrap();
                    let mut more = vec![b'x'; second];
                    if second > 10 {
                        more.push(b'\n');
                    }
                    s.write_all(&more).unwrap();
                    s.shutdown(std::net::Shutdown::Write).unwrap();
                    let _ = s.read(&mut [0u8; 1]);
                });
                let out = output_within_setup(
                    move |vm| {
                        vm.register_fn("go_on", move || go_on.send(()).unwrap())
                            .unwrap();
                    },
                    format!(
                        r#"import net
{SHOW}
fn reading(c: net.Conn) -> str {{
    match c.read_line() {{
        ok(some(text)) => return "{{text.len()}} chars"
        ok(none) => return "none"
        err(e) => return kind(e)
    }}
}}

fn main() -> Result[()] {{
    let c = net.connect("{addr}")?
    c.set_line_limit({before})
    let r = go reading(c)
    task.tick(0.0)
    c.set_line_limit({after})
    go_on()
    print(r.join())
    c.close()
    return ok(())
}}
"#
                    ),
                    LIMIT,
                );
                peer.join().unwrap();
                assert_eq!(out, format!("{expected}\n"), "{before} to {after}");
            }
        },
    )
}

/// What the arguments may be: `read_until` takes a delimiter of at least one byte and a `max` of 0
/// to a gibibyte (`net.Error.Other`, with words of Colibri's own); `set_line_limit` takes 1 to a
/// gibibyte (a script error otherwise, as `set_timeout` has for a bad number).
#[test]
fn read_until_and_set_line_limit_check_their_arguments() {
    vm_pending(
        "read_until_and_set_line_limit_check_their_arguments",
        || {
            let (listener, addr) = rust_listener();
            let peer = thread::spawn(move || {
                let (mut s, _) = listener.accept().unwrap();
                with_timeouts(&s);
                s.write_all(b"ok\n").unwrap();
                let _ = s.read(&mut [0u8; 1]);
            });
            let out = output(&format!(
                r#"import net
{SHOW}
fn main() -> Result[()] {{
    let c = net.connect("{addr}")?
    let nl = "\n".to_bytes()
    print(show(c.read_until(Bytes(), 10)))
    print(c.read_until(Bytes(), 10).err().expect("an error").message().contains("delimiter is empty"))
    print(show(c.read_until(nl, -1)))
    print(show(c.read_until(nl, 1073741825)))
    c.set_line_limit(1)
    c.set_line_limit(1073741824)
    c.set_line_limit(1048576)
    // A `max` of a gibibyte and of 0 are fine; nothing was taken by the refusals.
    print(show(c.read_until(nl, 0)))
    print(show(c.read_until(nl, 1073741824)))
    c.close()
    return ok(())
}}
"#
            ));
            peer.join().unwrap();
            // `ok` is two bytes before `\n`: over a `max` of 0, then taken with a gibibyte.
            assert_eq!(out, "Other\ntrue\nOther\nOther\nLineTooLong\n2 bytes\n");
            for bad in [0i64, -1, 1_073_741_825] {
                let (listener, addr) = rust_listener();
                let peer = thread::spawn(move || {
                    let (mut s, _) = listener.accept().unwrap();
                    let _ = s.read(&mut [0u8; 1]);
                });
                let (mut vm, _out) = vm();
                let result = run(
                    &mut vm,
                    &format!(
                        r#"import net

fn main() -> Result[()] {{
    let c = net.connect("{addr}")?
    defer c.close()
    c.set_line_limit({bad})
    return ok(())
}}
"#
                    ),
                );
                let e = result.expect_err("a bad limit is an error").to_string();
                assert!(
                    e.contains("set_line_limit") && e.contains(&bad.to_string()),
                    "{bad}: {e}"
                );
                peer.join().unwrap();
            }
        },
    )
}

/// A `read_until` called behind another coroutine's `read_until` or `read_line` is served after
/// it, with the bytes after it: the first gets a piece that holds a line end of the other kind
/// (`\n` inside a `\r\n` piece), and the `read_line` called after it gets the line after that
/// piece, not the `\n` inside it. Both are parked when the data comes.
#[test]
fn a_read_until_is_served_in_call_order_with_the_other_reads() {
    vm_pending(
        "a_read_until_is_served_in_call_order_with_the_other_reads",
        || {
            let (listener, addr) = rust_listener();
            let (send_now, told) = mpsc::channel::<()>();
            let peer = thread::spawn(move || {
                let (mut s, _) = listener.accept().unwrap();
                with_timeouts(&s);
                told.recv_timeout(LIMIT).unwrap();
                s.write_all(b"xy\nzz\r\ntail\nefgh").unwrap();
                let _ = s.read(&mut [0u8; 1]);
            });
            let out = output_within_setup(
                move |vm| {
                    vm.register_fn("send_now", move || send_now.send(()).unwrap())
                        .unwrap();
                },
                format!(
                    r#"import net

fn until(c: net.Conn) -> str {{
    return "until: {{c.read_until("\r\n".to_bytes(), 64).expect("bytes").expect("a piece")}}"
}}

fn line(c: net.Conn) -> str {{
    return "line: {{c.read_line().expect("a line").expect("a line")}}"
}}

fn exact(c: net.Conn) -> str {{
    return "exact: {{c.read_exact(4).expect("four bytes")}}"
}}

fn main() -> Result[()] {{
    let c = net.connect("{addr}")?
    let a = go until(c)
    let b = go line(c)
    let d = go exact(c)
    task.tick(0.0)
    send_now()
    print(a.join())
    print(b.join())
    print(d.join())
    c.close()
    return ok(())
}}
"#
                ),
                LIMIT,
            );
            peer.join().unwrap();
            assert_eq!(out, "until: b\"xy\\nzz\"\nline: tail\nexact: b\"efgh\"\n");
        },
    )
}

/// A `read_until` that was cancelled while the start of a match was in the buffer loses nothing:
/// the next call gets the bytes of the first and the rest. (The half match is read into the
/// buffer by a `read_exact` that takes one byte, so that it is there, whatever the timing of the
/// peer's packets.)
#[test]
fn a_cancelled_read_until_loses_no_data() {
    vm_pending("a_cancelled_read_until_loses_no_data", || {
        let (listener, addr) = rust_listener();
        let (more_now, told) = mpsc::channel::<()>();
        let peer = thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            with_timeouts(&s);
            s.write_all(b"xab\r").unwrap();
            told.recv_timeout(LIMIT).unwrap();
            s.write_all(b"\ncd\r\n").unwrap();
            let _ = s.read(&mut [0u8; 1]);
        });
        let out = output_within_setup(
            move |vm| {
                vm.register_fn("more_now", move || more_now.send(()).unwrap())
                    .unwrap();
            },
            format!(
                r#"import net

fn until(c: net.Conn) -> str {{
    return "never: {{c.read_until("\r\n".to_bytes(), 64)}}"
}}

fn main() -> Result[()] {{
    let c = net.connect("{addr}")?
    print(c.read_exact(1)?)
    let r = go until(c)
    task.tick(0.0)
    r.cancel()
    print(r.done())
    more_now()
    print(c.read_until("\r\n".to_bytes(), 64)?.expect("a piece"))
    print(c.read_until("\r\n".to_bytes(), 64)?.expect("a piece"))
    c.close()
    return ok(())
}}
"#
            ),
            LIMIT,
        );
        peer.join().unwrap();
        assert_eq!(out, "b\"x\"\ntrue\nb\"ab\"\nb\"cd\"\n");
    })
}

/// The timeout counts for the whole call, and a call that times out with a half match buffered
/// leaves the connection usable with the bytes still there.
#[test]
fn a_read_until_that_times_out_leaves_the_half_match_for_the_next_call() {
    vm_pending(
        "a_read_until_that_times_out_leaves_the_half_match_for_the_next_call",
        || {
            let (listener, addr) = rust_listener();
            let (more_now, told) = mpsc::channel::<()>();
            let peer = thread::spawn(move || {
                let (mut s, _) = listener.accept().unwrap();
                with_timeouts(&s);
                s.write_all(b"ab\r").unwrap();
                // Only when the call has timed out.
                told.recv_timeout(LIMIT).unwrap();
                s.write_all(b"\nrest\r\n").unwrap();
                let _ = s.read(&mut [0u8; 1]);
            });
            let out = output_within_setup(
                move |vm| {
                    vm.register_fn("more_now", move || more_now.send(()).unwrap())
                        .unwrap();
                },
                format!(
                    r#"import net
{SHOW}
fn main() -> Result[()] {{
    let c = net.connect("{addr}")?
    c.set_timeout(0.3)
    let crlf = "\r\n".to_bytes()
    print(show(c.read_until(crlf, 64)))
    more_now()
    c.set_timeout(0.0)
    print(c.read_until(crlf, 64)?.expect("a piece"))
    print(c.read_until(crlf, 64)?.expect("a piece"))
    c.close()
    return ok(())
}}
"#
                ),
                LIMIT,
            );
            peer.join().unwrap();
            assert_eq!(out, "Timeout\nb\"ab\"\nb\"rest\"\n");
        },
    )
}

/// A `Drop` hook that calls `read_until` while a coroutine waits in a read of the same connection
/// gets `net.Error.Other` at once and not a hang, as `read_line` does; the coroutine keeps its
/// bytes.
#[test]
fn a_drop_hook_that_calls_read_until_while_a_coroutine_reads_gets_an_error_not_a_hang() {
    vm_pending(
        "a_drop_hook_that_calls_read_until_while_a_coroutine_reads_gets_an_error_not_a_hang",
        || {
            let (addr, release, peer) = hook_peer();
            let out = output_within_released(
                format!(
                    r#"import net

class Guard {{ conn: net.Conn }}

impl Drop for Guard {{
    fn drop(self) {{
        match self.conn.read_until("\n".to_bytes(), 64) {{
            ok(piece) => print("drop hook read: {{piece}}")
            err(e) => {{
                print("drop hook: {{e.as[net.Error]().expect("a net error")}}")
                print(e.message().contains("already waiting in another coroutine"))
            }}
        }}
    }}
}}

fn reader(c: net.Conn) {{
    print("coroutine read: {{c.read_until("\n".to_bytes(), 64)}}")
}}

fn main() -> Result[()] {{
    let c = net.connect("{addr}")?
    let r = go reader(c)
    task.tick(0.0)
    release()
    let mut g: Option[Guard] = some(Guard {{ conn: c }})
    g = none
    r.join()
    print("then: {{c.read_until("\n".to_bytes(), 64)}}")
    return ok(())
}}
"#
                ),
                release,
            );
            peer.join().unwrap();
            assert_eq!(
                out,
                "drop hook: Other\ntrue\ncoroutine read: ok(some(b\"one\"))\nthen: ok(some(b\"two\"))\n"
            );
        },
    )
}

/// A loop of `read_until` calls that never wait still lets the other coroutines run, as a loop of
/// `read_line` calls does (the burst budget counts calls, whatever kind they are).
#[test]
fn a_loop_of_read_until_calls_that_never_wait_still_lets_the_other_coroutines_run() {
    vm_pending(
        "a_loop_of_read_until_calls_that_never_wait_still_lets_the_other_coroutines_run",
        || {
            const LINES: usize = 40_000;
            let (listener, addr) = rust_listener();
            let peer = thread::spawn(move || {
                let (mut s, _) = listener.accept().unwrap();
                with_timeouts(&s);
                let mut sending = s.try_clone().unwrap();
                let sender = thread::spawn(move || {
                    sending.write_all("x\r\n".repeat(LINES).as_bytes()).unwrap();
                });
                let echoed = read_n(&mut s, LINES * 3).len();
                sender.join().unwrap();
                echoed
            });
            let out = output_within(
                format!(
                    r#"import net

let mut ticks = 0
let mut finished = false

fn counter() {{
    while !finished {{
        ticks += 1
        wait(0.0)
    }}
}}

fn echo(c: net.Conn) {{
    let crlf = "\r\n".to_bytes()
    for _ in 0..{LINES} {{
        match c.read_until(crlf, 64).expect("a piece") {{
            some(piece) => c.write_bytes(piece).expect("echoed")
            none => return
        }}
        c.write("\r\n").expect("echoed")
    }}
    finished = true
}}

fn main() -> Result[()] {{
    let c = net.connect("{addr}")?
    let k = go counter()
    let e = go echo(c)
    e.join()
    k.join()
    print(ticks >= 500)
    c.close()
    return ok(())
}}
"#
                ),
                LIMIT,
            );
            assert_eq!(out, "true\n");
            assert_eq!(peer.join().unwrap(), LINES * 3);
        },
    )
}

/// A delimiter of 1,024 bytes is taken (and found when it is there); one of 1,025 is a
/// `net.Error.Other` with words of Colibri's own, and nothing is taken from the stream.
#[test]
fn a_delimiter_of_a_thousand_and_twenty_four_bytes_is_the_longest() {
    vm_pending(
        "a_delimiter_of_a_thousand_and_twenty_four_bytes_is_the_longest",
        || {
            let (listener, addr) = rust_listener();
            let peer = thread::spawn(move || {
                let (mut s, _) = listener.accept().unwrap();
                with_timeouts(&s);
                let mut piece = b"head".to_vec();
                piece.extend_from_slice(&[b'z'; 1024]);
                piece.extend_from_slice(b"tail");
                s.write_all(&piece).unwrap();
                s.shutdown(std::net::Shutdown::Write).unwrap();
                let _ = s.read(&mut [0u8; 1]);
            });
            let out = output(&format!(
                r#"import net
import str
{SHOW}
fn main() -> Result[()] {{
    let c = net.connect("{addr}")?
    let longest = str.repeat("z", 1024).to_bytes()
    let too_long = str.repeat("z", 1025).to_bytes()
    let refused = c.read_until(too_long, 64)
    print(show(refused))
    print(refused.err().expect("an error").message().contains("at most 1024"))
    print(c.read_until(longest, 64)?.expect("a piece").len())
    print(c.read(10)?.len())
    c.close()
    return ok(())
}}
"#
            ));
            peer.join().unwrap();
            assert_eq!(out, "Other\ntrue\n4\n4\n");
        },
    )
}

/// The `httpd` shape that stopped the interpreter: a closure hands a big write to a child
/// (`go fn() {..}()`) and returns while the write is in flight, so the child is the last holder
/// of the object it captured (here a `Guard` around the connection). When the child's write
/// ends, the closure it ran is dropped and the guard must be released, so its `Drop` hook runs.
///
/// The write must still be in flight when the starter returns, whatever the kernel's buffers
/// are: the peer does not read until a while after the starter has returned, its receive
/// buffer is pinned small (on Unix, which also stops its autotuning), and the payload is
/// 64 MiB, far above any send buffer a loopback socket autotunes to (`tcp_wmem` tops out at a
/// few MiB by default).
#[test]
fn a_child_writing_through_a_captured_object_releases_it_when_it_ends() {
    vm_pending(
        "a_child_writing_through_a_captured_object_releases_it_when_it_ends",
        || {
            const PAYLOAD: usize = 64 * 1024 * 1024;
            let (listener, addr) = rust_listener();
            #[cfg(unix)]
            pin_small_receive_buffer(&listener);
            let peer = thread::spawn(move || {
                let (mut s, _) = listener.accept().unwrap();
                with_timeouts(&s);
                // Reads late, so the child's write is still in flight when the starter has returned.
                thread::sleep(Duration::from_millis(300));
                let mut total = 0;
                let mut buf = [0u8; 65536];
                while total < PAYLOAD {
                    match s.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => total += n,
                    }
                }
                total
            });
            let out = output(&format!(
                r#"import net

class Guard {{
    conn: net.Conn
}}

impl Drop for Guard {{
    fn drop(self) {{
        print("guard dropped")
    }}
}}

fn start(conn: net.Conn, data: Bytes) -> Coroutine[()] {{
    let g = Guard {{ conn: conn }}
    let child = go fn() {{
        let res = g.conn.write_bytes(data)
        print("write {{res.is_ok()}}")
    }}()
    wait(0.05)
    print("start returns")
    return child
}}

fn main() -> Result[()] {{
    let seed = BytesBuilder()
    for _ in 0..65536 {{
        seed.push(u8(7))
    }}
    let piece = seed.to_bytes()
    let b = BytesBuilder()
    for _ in 0..1024 {{
        b.push_bytes(piece)
    }}
    let c = net.connect("{addr}")?
    let child = start(c, b.to_bytes())
    child.join()
    c.close()
    return ok(())
}}
"#
            ));
            assert_eq!(out, "start returns\nwrite true\nguard dropped\n");
            assert_eq!(peer.join().unwrap(), PAYLOAD);
        },
    )
}

/// Sets a small `SO_RCVBUF` on a listening socket, which the connections accepted from it
/// inherit: the sender then cannot park much of a big write in the peer's buffer.
#[cfg(unix)]
fn pin_small_receive_buffer(listener: &TcpListener) {
    use std::os::fd::AsRawFd;
    let size: libc::c_int = 64 * 1024;
    // SAFETY: the descriptor is open for the whole call and `size` is a valid `c_int`.
    let rc = unsafe {
        libc::setsockopt(
            listener.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_RCVBUF,
            (&raw const size).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    assert_eq!(rc, 0, "SO_RCVBUF is set");
}
