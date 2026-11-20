//! `httpd` over real sockets: a Colibri server in a `Vm` on its own thread and a raw TCP client
//! (`support/wire.rs`) that writes the exact bytes of each case. Loopback only, every port from
//! `server.addr()`. No sleep is the condition of a result: every wait is for an event, with a
//! ceiling; a timeout test uses a small option, asserts that the close did not come before it,
//! and gives the ceiling many times that. "Closed" is an end or a reset, never one of them.
//!
//! Each server records every request it handles (its method and target) and prints the list
//! when it stops, so a test can show that a request it smuggled behind a rejection was never
//! seen. `POST /__stop` ends the server and `GET /__stats` gives the number of open connections;
//! neither is recorded.

#[path = "support/wire.rs"]
mod wire;

use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use colibri_runtime as colibri;
use wire::{EVENT, Read1, Wire, get, request};

/// The longest a whole server run may take once it is told to stop.
const CEILING: Duration = Duration::from_secs(30);

/// The Colibri program around a test's `app` function and options.
const PROGRAM: &str = r#"import hash
import httpd
import str
@IMPORTS@

class Seen {
    items: List[str] = []
    release: Signal = Signal()
    writer: httpd.BodyWriter? = none
}

@APP@

fn main() -> Result[()] {
    let seen = Seen {}
    let server = httpd.listen("127.0.0.1:0", @OPTIONS@)?
    publish(server.addr())
    let handler = fn(r: httpd.Request) -> Result[httpd.Response] {
        if r.path == "/__stop" {
            server.stop(1.0)
            return ok(httpd.text(200, "stopping\n"))
        }
        if r.path == "/__stop_slowly" {
            server.stop(120.0)
            return ok(httpd.text(200, "stopping\n"))
        }
        if r.path == "/__seen" {
            return ok(httpd.text(200, "{seen.items.len()}\n"))
        }
        if r.path == "/__stats" {
            return ok(httpd.text(200, "{server.connections()}\n"))
        }
        seen.items.push("{r.method} {r.target}")
        return app(r, seen, server)
    }
    server.serve(handler)?
@AFTER@
    for item in seen.items {
        print(item)
    }
    return ok(())
}
"#;

/// The usual `app`: `/` says hello; anything else is a 404.
const HELLO: &str = r#"
fn app(r: httpd.Request, seen: Seen, server: httpd.Server) -> Result[httpd.Response] {
    if r.path == "/" {
        return ok(httpd.text(200, "hello\n"))
    }
    return ok(httpd.text(404, "no such thing\n"))
}
"#;

/// The `app` most tests use: routes for what the tests need a server to do.
const APP: &str = r#"
fn big() -> Bytes {
    let one = str.repeat("0123456789abcdef", 65536).to_bytes()
    let all = BytesBuilder()
    for i in 0..32 {
        all.push_bytes(one)
    }
    return all.to_bytes()
}

fn app(r: httpd.Request, seen: Seen, server: httpd.Server) -> Result[httpd.Response] {
    if r.path == "/" {
        return ok(httpd.text(200, "hello\n"))
    }
    if r.path.starts_with("/echo") {
        return match r.body() {
            ok(b) => ok(httpd.bytes(200, b))
            err(e) => {
                seen.items.push("body failed")
                err(e)
            }
        }
    }
    if r.path == "/stream" {
        return ok(httpd.stream(200, "text/plain", fn(w: httpd.BodyWriter) -> Result[()] {
            seen.items.push("stream ran")
            w.write_str("one\n")?
            w.write_str("two\n")?
            w.write_str("three\n")?
            return ok(())
        }))
    }
    if r.path == "/stream_len" {
        return ok(httpd.stream(200, "text/plain", fn(w: httpd.BodyWriter) -> Result[()] {
            seen.items.push("stream ran")
            w.write_str("one\n")?
            w.write_str("two\n")?
            w.write_str("three\n")?
            return ok(())
        }, length: some(14)))
    }
    if r.path == "/stream_short" {
        return ok(httpd.stream(200, "text/plain", fn(w: httpd.BodyWriter) -> Result[()] {
            w.write_str("abc")?
            return ok(())
        }, length: some(20)))
    }
    if r.path == "/stream_over" {
        return ok(httpd.stream(200, "text/plain", fn(w: httpd.BodyWriter) -> Result[()] {
            w.write_str("abc")?
            let r = w.write_str("defg")
            if r.is_err() {
                seen.items.push("past length")
            }
            return r
        }, length: some(5)))
    }
    if r.path == "/stream_fail" {
        return ok(httpd.stream(200, "text/plain", fn(w: httpd.BodyWriter) -> Result[()] {
            w.write_str("one\n")?
            return err(error("stream boom"))
        }))
    }
    if r.path == "/stream_big" {
        return ok(httpd.stream(200, "application/octet-stream", fn(w: httpd.BodyWriter) -> Result[()] {
            return w.write(str.repeat("0123456789abcdef", 65536).to_bytes())
        }))
    }
    if r.path == "/stream_empty" {
        return ok(httpd.stream(200, "text/plain", fn(w: httpd.BodyWriter) -> Result[()] {
            w.write_str("")?
            w.write_str("x")?
            w.write(Bytes())?
            return ok(())
        }))
    }
    if r.path == "/stream_panic" {
        return ok(httpd.stream(200, "text/plain", fn(w: httpd.BodyWriter) -> Result[()] {
            w.write_str("x")?
            let small = [1]
            seen.items.push("{small[5]}")
            return ok(())
        }))
    }
    if r.path == "/endless" {
        return ok(httpd.stream(200, "application/octet-stream", fn(w: httpd.BodyWriter) -> Result[()] {
            defer {
                seen.items.push("deferred")
            }
            let piece = str.repeat("0123456789abcdef", 4096).to_bytes()
            for i in 0..800 {
                let r = w.write(piece)
                if r.is_err() {
                    seen.items.push("write failed")
                    let _ = w.write(piece)
                    seen.items.push("not cancelled")
                    return ok(())
                }
            }
            return ok(())
        }))
    }
    if r.path == "/keep" {
        return ok(httpd.stream(200, "text/plain", fn(w: httpd.BodyWriter) -> Result[()] {
            seen.writer = some(w)
            w.write_str("kept")?
            return ok(())
        }))
    }
    if r.path == "/inject" {
        if let some(w) = seen.writer {
            let r = w.write_str("HTTP/1.1 200 OK\r\nX-Injected: yes\r\nContent-Length: 0\r\n\r\n")
            seen.items.push(if r.is_err() { "inject refused" } else { "injected" })
        }
        return ok(httpd.text(200, "done\n"))
    }
    if r.path == "/sum" {
        let b = r.body()?
        return ok(httpd.text(200, "{b.len()} {hash.sha256_hex(b)}\n"))
    }
    if r.path == "/ignore" {
        return ok(httpd.text(200, "ignored\n"))
    }
    if r.path == "/204" {
        return ok(httpd.status(204))
    }
    if r.path == "/304" {
        return ok(httpd.status(304))
    }
    if r.path == "/boom" {
        return err(error("boom"))
    }
    if r.path == "/missing" {
        return err(httpd.fail(404, "no item"))
    }
    if r.path == "/cr" {
        return ok(httpd.text(200, "x").header("X-A", "a\rb"))
    }
    if r.path == "/length" {
        return ok(httpd.text(200, "x").header("Content-Length", "5"))
    }
    if r.path == "/401" {
        return ok(httpd.text(401, "no\n"))
    }
    if r.path == "/big" {
        return ok(httpd.bytes(200, big()))
    }
    if r.path == "/wait" {
        wait_for(seen.release)
        return ok(httpd.text(200, "released\n"))
    }
    if r.path == "/release" {
        let _ = r.body()
        seen.release.emit()
        return ok(httpd.text(200, "ok\n"))
    }
    return ok(httpd.text(404, "no such thing\n"))
}
"#;

/// A server running on its own thread.
struct Server {
    addr: String,
    done: mpsc::Receiver<Result<String, String>>,
}

/// The outcome of a server run: what it printed, which is the requests it saw.
struct Outcome {
    seen: Vec<String>,
}

impl Server {
    /// Starts a server with `app` as the handler and `options` as the Colibri text of its
    /// `httpd.Options`, and waits for its address.
    fn start(app: &str, options: &str) -> Server {
        Server::start_with(app, options, "", "")
    }

    /// `start` with more imports for the program, and statements for after `serve` returns.
    #[allow(dead_code)]
    fn start_with(app: &str, options: &str, imports: &str, after: &str) -> Server {
        let source = PROGRAM
            .replace("@APP@", app)
            .replace("@OPTIONS@", options)
            .replace("@IMPORTS@", imports)
            .replace("@AFTER@", after);
        let (addr_tx, addr_rx) = mpsc::channel::<String>();
        let (done_tx, done) = mpsc::channel();
        thread::spawn(move || {
            let out = colibri::SharedBuffer::new();
            let mut vm = colibri::Vm::builder()
                .stdout(Box::new(out.clone()))
                .allow(colibri::Cap::Net)
                .allow(colibri::Cap::Os)
                .build();
            colibri_std::install(&mut vm).expect("installs");
            let started = addr_tx.clone();
            vm.register_fn("publish", move |addr: String| {
                let _ = started.send(addr);
            })
            .expect("registers publish");
            let result = vm
                .load_source("app/main.colibri", &source)
                .and_then(|()| vm.run_main())
                // As `colibri run` does: the loop collects the panic of a coroutine that nothing
                // reported.
                .and_then(|()| vm.run_event_loop())
                .map(|()| out.contents())
                .map_err(|e| e.render(false));
            let _ = done_tx.send(result);
        });
        // The script either publishes its address or ends with an error.
        let addr = match addr_rx.recv_timeout(EVENT) {
            Ok(addr) => addr,
            Err(_) => match done.try_recv() {
                Ok(Err(e)) => panic!("the server script failed to start:\n{e}"),
                _ => panic!("the server did not publish its address"),
            },
        };
        Server { addr, done }
    }

    fn hello() -> Server {
        Server::start(HELLO, "httpd.Options {}")
    }

    fn hello_with(options: &str) -> Server {
        Server::start(HELLO, options)
    }

    /// The server of `APP`, with `options` as the Colibri text of its options.
    fn app(options: &str) -> Server {
        Server::start(APP, options)
    }

    fn connect(&self) -> Wire {
        Wire::connect(&self.addr)
    }

    /// The number of connections the server has open, asked on a fresh connection (so the
    /// answer counts that one too).
    fn connections(&self) -> i64 {
        let mut w = self.connect();
        let r = w.call(get("/__stats"));
        r.text().trim().parse().expect("a number")
    }

    /// Waits until the server has no more than `n` connections open, up to the ceiling. The
    /// connection that asks counts, so `n` includes it.
    #[track_caller]
    fn wait_connections_at_most(&self, n: i64) {
        let start = Instant::now();
        loop {
            let now = self.connections();
            if now <= n {
                return;
            }
            assert!(
                start.elapsed() < EVENT,
                "the server still has {now} connections open"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// Waits until the server has handled `n` requests (the ones it records), by asking on a
    /// fresh connection. A request is recorded when its head is complete, before its handler
    /// reads anything.
    #[track_caller]
    fn wait_seen(&self, n: i64) {
        let start = Instant::now();
        loop {
            let mut w = self.connect();
            let now: i64 = w
                .call(get("/__seen"))
                .text()
                .trim()
                .parse()
                .expect("a number");
            if now >= n {
                return;
            }
            assert!(
                start.elapsed() < EVENT,
                "the server has seen only {now} requests"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// Stops the server by `POST /__stop` and waits for its run to end; what it saw.
    #[track_caller]
    fn finish(self) -> Outcome {
        let mut w = self.connect();
        let r = w.call(request("POST", "/__stop", &["Content-Length: 0"], b""));
        assert_eq!(r.status, 200);
        // A client that has read `Connection: close` closes its end; the server's lingering
        // close waits for that.
        drop(w);
        self.wait()
    }

    /// Like `finish`, but the grace of the stop is 120 s: the connections that are left are not
    /// cancelled by the grace timer before the ceiling, so one that closes did so because the
    /// stop closed it.
    #[track_caller]
    fn finish_slowly(self) -> Outcome {
        let mut w = self.connect();
        let r = w.call(request(
            "POST",
            "/__stop_slowly",
            &["Content-Length: 0"],
            b"",
        ));
        assert_eq!(r.status, 200);
        drop(w);
        self.wait()
    }

    /// Waits for a run that is ending on its own (stopped from a handler, say).
    #[track_caller]
    fn wait(self) -> Outcome {
        match self.done.recv_timeout(CEILING) {
            Ok(Ok(printed)) => Outcome {
                seen: printed.lines().map(str::to_string).collect(),
            },
            Ok(Err(e)) => panic!("the server script failed:\n{e}"),
            Err(_) => panic!("the server did not end within {CEILING:?}"),
        }
    }
}

#[test]
fn a_get_is_answered_with_a_length_and_a_date() {
    let server = Server::hello();
    let mut w = server.connect();
    let r = w.call(get("/"));
    assert_eq!(r.version, "HTTP/1.1");
    assert_eq!(r.status, 200);
    assert_eq!(r.reason, "OK");
    assert_eq!(r.text(), "hello\n");
    assert_eq!(r.header("content-length"), Some("6"));
    drop(w);
    let outcome = server.finish();
    assert_eq!(outcome.seen, ["GET /"]);
}

/// A request to `/smuggled` that a rejected connection must never get as far as handling.
const SMUGGLED: &[u8] = b"GET /smuggled HTTP/1.1\r\nHost: test\r\n\r\n";

/// Sends `raw` with a second request behind it, in the same write, and checks the answer: the
/// status, `Connection: close`, an error text, and that the connection then ends without
/// another response.
#[track_caller]
fn rejects(server: &Server, name: &str, raw: &[u8], status: u16) {
    let mut w = server.connect();
    let mut data = raw.to_vec();
    data.extend_from_slice(SMUGGLED);
    w.send(&data);
    let r = w.read();
    assert_eq!(r.status, status, "{name}: {r:?}");
    assert!(
        r.says_close(),
        "{name}: no Connection: close in {:?}",
        r.head
    );
    assert!(
        r.text().starts_with(&format!("{status} ")),
        "{name}: the body says {:?}",
        r.text()
    );
    // Nothing follows: a second response would be bytes here.
    w.expect_silent_close();
}

/// What a request needs above its line: a Host and a clean end.
fn head(line: &str, fields: &[&str]) -> Vec<u8> {
    let mut out = format!("{line}\r\n").into_bytes();
    for f in fields {
        out.extend_from_slice(f.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"\r\n");
    out
}

#[test]
fn a_request_line_that_is_wrong_is_refused_and_nothing_after_it_is_handled() {
    let server = Server::hello();
    let long = format!("GET /{} HTTP/1.1", "a".repeat(9000));
    let cases: Vec<(&str, Vec<u8>, u16)> = vec![
        ("two parts", head("GET /", &["Host: test"]), 400),
        ("two spaces", head("GET  / HTTP/1.1", &["Host: test"]), 400),
        (
            "a lower case version",
            head("GET / http/1.1", &["Host: test"]),
            400,
        ),
        (
            "a method that is not a token",
            head("G@T / HTTP/1.1", &["Host: test"]),
            400,
        ),
        (
            "a fragment",
            head("GET /a#b HTTP/1.1", &["Host: test"]),
            400,
        ),
        ("HTTP/2.0", head("GET / HTTP/2.0", &["Host: test"]), 505),
        ("HTTP/0.9", head("GET / HTTP/0.9", &["Host: test"]), 505),
        ("a line over the limit", head(&long, &["Host: test"]), 414),
        ("* with GET", head("GET * HTTP/1.1", &["Host: test"]), 400),
        (
            "CONNECT",
            head(
                "CONNECT example.test:443 HTTP/1.1",
                &["Host: example.test:443"],
            ),
            501,
        ),
    ];
    for (name, raw, status) in &cases {
        rejects(&server, name, raw, *status);
    }
    assert_eq!(server.finish().seen, Vec::<String>::new());
}

#[test]
fn a_header_field_that_is_wrong_is_refused_and_nothing_after_it_is_handled() {
    let server = Server::hello();
    let line = "GET / HTTP/1.1";
    let many: Vec<String> = (0..101).map(|i| format!("X-N{i}: 1")).collect();
    let mut many_fields: Vec<&str> = vec!["Host: test"];
    many_fields.extend(many.iter().map(String::as_str));
    let big: Vec<String> = (0..70)
        .map(|i| format!("X-B{i}: {}", "v".repeat(1000)))
        .collect();
    let mut big_fields: Vec<&str> = vec!["Host: test"];
    big_fields.extend(big.iter().map(String::as_str));
    let mut not_utf8 = head(line, &["Host: test"]);
    not_utf8.truncate(not_utf8.len() - 2);
    not_utf8.extend_from_slice(b"X-A: \xff\xfe\r\n\r\n");
    let cases: Vec<(&str, Vec<u8>, u16)> = vec![
        (
            "a space before the colon",
            head(line, &["Host: test", "X-A : 1"]),
            400,
        ),
        (
            "obsolete line folding",
            head(line, &["Host: test", "X-A: 1", "  folded"]),
            400,
        ),
        (
            "a bare CR in a value",
            head(line, &["Host: test", "X-A: a\rb"]),
            400,
        ),
        (
            "a NUL in a value",
            head(line, &["Host: test", "X-A: a\0b"]),
            400,
        ),
        ("a line that is not UTF-8", not_utf8, 400),
        ("no colon", head(line, &["Host: test", "nonsense"]), 400),
        ("too many lines", head(line, &many_fields), 431),
        ("too many bytes", head(line, &big_fields), 431),
    ];
    for (name, raw, status) in &cases {
        rejects(&server, name, raw, *status);
    }
    assert_eq!(server.finish().seen, Vec::<String>::new());
}

#[test]
fn a_host_that_is_missing_or_repeated_is_refused() {
    let server = Server::hello();
    rejects(&server, "no Host", &head("GET / HTTP/1.1", &[]), 400);
    rejects(
        &server,
        "two Host lines",
        &head("GET / HTTP/1.1", &["Host: a", "Host: b"]),
        400,
    );
    assert_eq!(server.finish().seen, Vec::<String>::new());
}

#[test]
fn a_body_length_that_is_wrong_or_unclear_is_refused_and_nothing_after_it_is_handled() {
    let server = Server::hello();
    let post = "POST / HTTP/1.1";
    let cases: Vec<(&str, Vec<u8>, u16)> = vec![
        (
            "a plus sign",
            head(post, &["Host: test", "Content-Length: +5"]),
            400,
        ),
        (
            "a list",
            head(post, &["Host: test", "Content-Length: 5, 6"]),
            400,
        ),
        (
            "two different lines",
            head(
                post,
                &["Host: test", "Content-Length: 5", "Content-Length: 6"],
            ),
            400,
        ),
        (
            "two equal lines",
            head(
                post,
                &["Host: test", "Content-Length: 5", "Content-Length: 5"],
            ),
            400,
        ),
        (
            "a length and chunking, with a request inside the body",
            [
                head(
                    post,
                    &[
                        "Host: test",
                        "Content-Length: 44",
                        "Transfer-Encoding: chunked",
                    ],
                ),
                b"0\r\n\r\nGET /smuggled HTTP/1.1\r\nHost: test\r\n\r\n".to_vec(),
            ]
            .concat(),
            400,
        ),
        (
            "chunking in HTTP/1.0",
            head("POST / HTTP/1.0", &["Transfer-Encoding: chunked"]),
            400,
        ),
        (
            "gzip then chunked",
            head(post, &["Host: test", "Transfer-Encoding: gzip, chunked"]),
            501,
        ),
        (
            "chunked twice",
            head(post, &["Host: test", "Transfer-Encoding: chunked, chunked"]),
            400,
        ),
    ];
    for (name, raw, status) in &cases {
        rejects(&server, name, raw, *status);
    }
    assert_eq!(server.finish().seen, Vec::<String>::new());
}

// ---------------------------------------------------------------------------------------------
// Framing that works
// ---------------------------------------------------------------------------------------------

#[test]
fn a_hundred_requests_in_turn_share_one_connection() {
    let server = Server::hello();
    let mut w = server.connect();
    for i in 0..100 {
        let r = w.call(get(&format!("/?n={i}")));
        assert_eq!(r.status, 200, "request {i}");
        assert!(!r.says_close(), "request {i} closed the connection");
    }
    drop(w);
    let seen = server.finish().seen;
    assert_eq!(seen.len(), 100);
    assert_eq!(seen[99], "GET /?n=99");
}

#[test]
fn http_1_0_without_keep_alive_closes_after_the_answer() {
    let server = Server::hello();
    let mut w = server.connect();
    let r = w.call("GET / HTTP/1.0\r\n\r\n");
    assert_eq!(r.status, 200);
    assert!(r.says_close());
    assert_eq!(r.text(), "hello\n");
    w.expect_silent_close();
    server.finish();
}

#[test]
fn http_1_0_with_keep_alive_says_so_and_stays_open() {
    let server = Server::hello();
    let mut w = server.connect();
    let r = w.call("GET / HTTP/1.0\r\nConnection: keep-alive\r\n\r\n");
    assert_eq!(r.status, 200);
    assert_eq!(r.header("connection"), Some("keep-alive"));
    let again = w.call("GET / HTTP/1.0\r\nConnection: keep-alive\r\n\r\n");
    assert_eq!(again.status, 200);
    drop(w);
    assert_eq!(server.finish().seen.len(), 2);
}

#[test]
fn connection_close_ends_it_and_a_request_behind_it_is_neither_answered_nor_handled() {
    let server = Server::hello();
    let mut w = server.connect();
    let mut data = request("GET", "/", &["Connection: close"], b"");
    data.extend_from_slice(SMUGGLED);
    w.send(&data);
    let r = w.read();
    assert_eq!(r.status, 200);
    assert!(r.says_close());
    w.expect_silent_close();
    assert_eq!(server.finish().seen, ["GET /"]);
}

#[test]
fn fifty_pipelined_requests_are_answered_in_order() {
    let server = Server::app("httpd.Options {}");
    let mut w = server.connect();
    let mut all = Vec::new();
    for i in 0..50 {
        all.extend(request(
            "POST",
            &format!("/echo/{i}"),
            &[],
            format!("body {i}").as_bytes(),
        ));
    }
    w.send(&all);
    for i in 0..50 {
        let r = w.read();
        assert_eq!(r.status, 200, "answer {i}");
        assert_eq!(r.text(), format!("body {i}"), "answer {i}");
    }
    drop(w);
    assert_eq!(server.finish().seen.len(), 50);
}

#[test]
fn head_gets_the_length_of_a_get_and_no_body_and_the_next_request_is_parsed() {
    let server = Server::hello();
    let mut w = server.connect();
    let get_answer = w.call(get("/"));
    let mut data = request("HEAD", "/", &[], b"");
    data.extend(get("/"));
    w.send(&data);
    let head = w.read_head();
    assert_eq!(head.status, 200);
    assert_eq!(
        head.header("content-length"),
        get_answer.header("content-length")
    );
    assert_eq!(
        head.header("content-type"),
        get_answer.header("content-type")
    );
    // The next bytes are the answer to the next request: if the body of the HEAD had been sent,
    // they would be "hello" and this would not be a status line.
    let next = w.read();
    assert_eq!(next.status, 200);
    assert_eq!(next.text(), "hello\n");
    drop(w);
    server.finish();
}

#[test]
fn a_204_and_a_304_have_no_length_and_no_body() {
    let server = Server::app("httpd.Options {}");
    let mut w = server.connect();
    let mut data = get("/204");
    data.extend(get("/304"));
    data.extend(get("/ignore"));
    w.send(&data);
    for status in [204, 304] {
        let r = w.read();
        assert_eq!(r.status, status);
        assert_eq!(r.header("content-length"), None, "status {status}");
        assert!(r.body.is_empty());
    }
    let last = w.read();
    assert_eq!(last.text(), "ignored\n");
    drop(w);
    server.finish();
}

/// `Sun, 06 Nov 1994 08:49:37 GMT`: the shape of an IMF-fixdate.
fn is_imf_fixdate(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 29 {
        return false;
    }
    let digit = |i: usize| b[i].is_ascii_digit();
    let upper = |i: usize| b[i].is_ascii_uppercase();
    let lower = |i: usize| b[i].is_ascii_lowercase();
    upper(0)
        && lower(1)
        && lower(2)
        && &s[3..5] == ", "
        && digit(5)
        && digit(6)
        && b[7] == b' '
        && upper(8)
        && lower(9)
        && lower(10)
        && b[11] == b' '
        && (12..16).all(digit)
        && b[16] == b' '
        && digit(17)
        && digit(18)
        && b[19] == b':'
        && digit(20)
        && digit(21)
        && b[22] == b':'
        && digit(23)
        && digit(24)
        && &s[25..] == " GMT"
}

#[test]
fn the_date_is_an_imf_fixdate_and_the_server_header_is_sent_only_when_named() {
    let server = Server::hello();
    let mut w = server.connect();
    let r = w.call(get("/"));
    let date = r.header("date").expect("a Date header");
    assert!(is_imf_fixdate(date), "{date:?}");
    assert_eq!(r.count("date"), 1);
    assert_eq!(r.header("server"), None);
    drop(w);
    server.finish();

    let named = Server::start(HELLO, r#"httpd.Options { server_name: "colibri-test" }"#);
    let mut w = named.connect();
    let r = w.call(get("/"));
    assert_eq!(r.header("server"), Some("colibri-test"));
    drop(w);
    named.finish();
}

#[test]
fn an_unread_body_of_a_kilobyte_is_drained_and_the_connection_goes_on() {
    let server = Server::app("httpd.Options {}");
    let mut w = server.connect();
    let body = vec![b'x'; 1024];
    let mut data = request("POST", "/ignore", &[], &body);
    data.extend(get("/ignore"));
    w.send(&data);
    let first = w.read();
    assert_eq!(first.status, 200);
    assert!(!first.says_close());
    let second = w.read();
    assert_eq!(second.status, 200);
    drop(w);
    assert_eq!(server.finish().seen, ["POST /ignore", "GET /ignore"]);
}

#[test]
fn an_unread_body_declared_a_megabyte_long_ends_the_connection() {
    let server = Server::app("httpd.Options {}");
    let mut w = server.connect();
    // The head and the first kilobyte: the server must not wait for the rest to decide.
    let mut data = request("POST", "/ignore", &["Content-Length: 1048576"], b"");
    data.extend(vec![b'x'; 1024]);
    w.send(&data);
    let r = w.read();
    assert_eq!(r.status, 200);
    assert!(r.says_close());
    drop(w);
    server.finish();
}

// ---------------------------------------------------------------------------------------------
// Timeouts: small options, a lower bound that the close did not come before, and a ceiling
// ---------------------------------------------------------------------------------------------

/// A close may not come before the timeout under test (0.5 s), less a little for the clock of
/// the system; it must come well within `EVENT`.
const NOT_BEFORE: Duration = Duration::from_millis(450);

#[test]
fn a_head_that_stops_halfway_is_answered_408_after_the_header_timeout() {
    let server = Server::app("httpd.Options { header_timeout: 0.5 }");
    let mut w = server.connect();
    let started = Instant::now();
    w.send("GET / HTTP/1.1\r\nHost: test\r\n");
    let r = w.read();
    let waited = started.elapsed();
    assert_eq!(r.status, 408);
    assert!(r.says_close());
    assert!(waited >= NOT_BEFORE, "the 408 came after only {waited:?}");
    assert!(waited < EVENT, "the 408 came after {waited:?}");
    w.expect_silent_close();
    assert_eq!(server.finish().seen, Vec::<String>::new());
}

#[test]
fn a_head_trickled_a_line_at_a_time_is_cut_off_by_the_time_of_the_whole_head() {
    // A line every 0.3 s never lets one line wait for 0.5 s, so a timeout that starts again at
    // each line never fires. The whole head has the time of `header_timeout`, checked after each
    // line.
    let server = Server::app("httpd.Options { header_timeout: 0.5 }");
    let mut w = server.connect();
    let started = Instant::now();
    w.send("GET / HTTP/1.1\r\nHost: test\r\n");
    let mut answer = None;
    for i in 0..40 {
        if !w.try_send(format!("X-Trickle-{i}: 1\r\n").as_bytes()) {
            break;
        }
        // Waiting for the answer is the pace: nothing within 0.3 s means "not yet".
        if let Read1::Response(r) = w.read_within(Duration::from_millis(300), false) {
            answer = Some(r);
            break;
        }
    }
    let waited = started.elapsed();
    let r = answer.expect("a 408 within forty lines");
    assert_eq!(r.status, 408);
    assert!(waited >= NOT_BEFORE, "the 408 came after only {waited:?}");
    assert!(waited < EVENT, "the 408 came after {waited:?}");
    assert_eq!(server.finish().seen, Vec::<String>::new());
}

#[test]
fn a_connection_that_sends_nothing_is_closed_without_a_word_after_the_header_timeout() {
    let server = Server::app("httpd.Options { header_timeout: 0.5 }");
    let mut w = server.connect();
    let started = Instant::now();
    w.expect_silent_close();
    let waited = started.elapsed();
    assert!(waited >= NOT_BEFORE, "the close came after only {waited:?}");
    assert!(waited < EVENT);
    server.finish();
}

#[test]
fn a_connection_that_waits_too_long_for_its_next_request_is_closed_without_a_word() {
    let server = Server::app("httpd.Options { idle_timeout: 0.5 }");
    let mut w = server.connect();
    let r = w.call(get("/"));
    assert_eq!(r.status, 200);
    let started = Instant::now();
    w.expect_silent_close();
    let waited = started.elapsed();
    assert!(waited >= NOT_BEFORE, "the close came after only {waited:?}");
    assert!(waited < EVENT);
    server.finish();
}

#[test]
fn half_a_body_ends_in_a_408_for_the_handler_that_reads_it_and_the_connection_closes() {
    let server = Server::app("httpd.Options { body_timeout: 0.5 }");
    let mut w = server.connect();
    let started = Instant::now();
    w.send(request("POST", "/echo", &["Content-Length: 10"], b""));
    w.send("12345");
    let r = w.read();
    let waited = started.elapsed();
    assert_eq!(r.status, 408);
    assert!(r.says_close());
    assert!(waited >= NOT_BEFORE, "the 408 came after only {waited:?}");
    assert!(waited < EVENT);
    drop(w);
    // The handler saw the error, so it was the read that failed and not the head.
    assert_eq!(server.finish().seen, ["POST /echo", "body failed"]);
}

#[test]
fn a_client_that_never_reads_a_large_answer_is_cut_off_by_the_write_timeout() {
    let server = Server::app("httpd.Options { write_timeout: 0.5 }");
    let mut a = server.connect();
    // 32 MiB is more than the buffers of both ends can hold, so the write blocks. The client
    // never reads, and the server closes after the write timeout.
    a.send(get("/big"));
    // The second connection's `/__stats` shows when the first is gone (it counts itself).
    server.wait_connections_at_most(1);
    drop(a);
    server.finish();
}

// ---------------------------------------------------------------------------------------------
// Peers that leave
// ---------------------------------------------------------------------------------------------

#[test]
fn a_client_that_leaves_in_the_middle_of_a_head_is_forgotten_and_the_server_goes_on() {
    let server = Server::hello();
    let mut w = server.connect();
    w.send("GET / HT");
    drop(w);
    server.wait_connections_at_most(1);
    let mut w = server.connect();
    assert_eq!(w.call(get("/")).status, 200);
    drop(w);
    assert_eq!(server.finish().seen, ["GET /"]);
}

#[test]
fn a_reset_during_a_body_read_fails_the_handlers_read_and_the_server_goes_on() {
    let server = Server::app("httpd.Options {}");
    let mut w = server.connect();
    w.send(request("POST", "/echo", &["Content-Length: 100"], b""));
    w.send("0123456789");
    // The server must have the head before the reset: on some systems a reset discards what
    // the server has not read yet. Then the handler is reading, and the reset ends its read.
    server.wait_seen(1);
    w.reset();
    // The handler's coroutine has seen the error once the connection is gone from the count.
    server.wait_connections_at_most(1);
    let mut w = server.connect();
    assert_eq!(w.call(get("/ignore")).status, 200);
    drop(w);
    assert_eq!(
        server.finish().seen,
        ["POST /echo", "body failed", "GET /ignore"]
    );
}

// ---------------------------------------------------------------------------------------------
// What handlers return
// ---------------------------------------------------------------------------------------------

#[test]
fn a_handler_that_fails_with_a_plain_error_gets_a_500_with_a_generic_body_and_keeps_the_connection()
{
    let server = Server::app("httpd.Options { log_errors: false }");
    let mut w = server.connect();
    let r = w.call(get("/boom"));
    assert_eq!(r.status, 500);
    assert!(
        !r.text().contains("boom"),
        "the body gave the error away: {:?}",
        r.text()
    );
    assert!(!r.says_close());
    // The same connection carries the next request.
    assert_eq!(w.call(get("/")).status, 200);
    drop(w);
    server.finish();
}

#[test]
fn a_failure_with_a_status_is_answered_with_that_status_and_its_message() {
    let server = Server::app("httpd.Options {}");
    let mut w = server.connect();
    let r = w.call(get("/missing"));
    assert_eq!(r.status, 404);
    assert!(r.text().contains("no item"), "{:?}", r.text());
    assert!(!r.says_close());
    assert_eq!(w.call(get("/")).status, 200);
    drop(w);
    server.finish();
}

#[test]
fn a_response_header_with_a_cr_is_a_500_and_never_goes_out_as_written() {
    let server = Server::app("httpd.Options { log_errors: false }");
    let mut w = server.connect();
    let r = w.call(get("/cr"));
    assert_eq!(r.status, 500);
    assert_eq!(r.header("x-a"), None);
    assert!(
        !r.head.contains('\r') || r.head.matches('\r').count() == r.head.matches("\r\n").count()
    );
    assert_eq!(w.call(get("/")).status, 200);
    drop(w);
    server.finish();
}

#[test]
fn a_handler_that_sets_the_content_length_gets_a_500() {
    let server = Server::app("httpd.Options { log_errors: false }");
    let mut w = server.connect();
    let r = w.call(get("/length"));
    assert_eq!(r.status, 500);
    assert_eq!(r.count("content-length"), 1);
    drop(w);
    server.finish();
}

// ---------------------------------------------------------------------------------------------
// Expect: 100-continue (the cases that need no stream)
// ---------------------------------------------------------------------------------------------

#[test]
fn a_handler_that_reads_the_body_makes_the_server_send_100_continue_first() {
    let server = Server::app("httpd.Options {}");
    let mut w = server.connect();
    w.send(request(
        "POST",
        "/echo",
        &["Expect: 100-continue", "Content-Length: 5"],
        b"",
    ));
    // The client holds the body back until it is told to go on.
    let interim = w.read_within(EVENT, false).response();
    assert_eq!(interim.status, 100);
    assert_eq!(interim.reason, "Continue");
    w.send("hello");
    let last = w.read();
    assert_eq!(last.status, 200);
    assert_eq!(last.text(), "hello");
    drop(w);
    server.finish();
}

#[test]
fn a_handler_that_answers_without_reading_sends_no_100_and_the_connection_closes() {
    let server = Server::app("httpd.Options {}");
    let mut w = server.connect();
    w.send(request(
        "POST",
        "/401",
        &["Expect: 100-continue", "Content-Length: 5"],
        b"",
    ));
    let r = w.read();
    assert_eq!(r.status, 401, "{r:?}");
    assert!(r.says_close());
    drop(w);
    server.finish();
}

#[test]
fn an_expectation_that_is_not_100_continue_is_a_417() {
    let server = Server::app("httpd.Options {}");
    rejects(
        &server,
        "Expect: foo",
        &head(
            "POST / HTTP/1.1",
            &["Host: test", "Expect: foo", "Content-Length: 0"],
        ),
        417,
    );
    assert_eq!(server.finish().seen, Vec::<String>::new());
}

#[test]
fn an_http_1_0_request_with_expect_gets_no_100() {
    let server = Server::app("httpd.Options {}");
    let mut w = server.connect();
    w.send("POST /echo HTTP/1.0\r\nExpect: 100-continue\r\nContent-Length: 5\r\n\r\nhello");
    let r = w.read();
    assert_eq!(r.status, 200, "the first response is the final one: {r:?}");
    assert_eq!(r.text(), "hello");
    drop(w);
    server.finish();
}

// ---------------------------------------------------------------------------------------------
// Chunked request bodies
// ---------------------------------------------------------------------------------------------

/// A chunked body of `parts`, each its own chunk, and the last chunk.
fn chunked(parts: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    for p in parts {
        out.extend_from_slice(format!("{:x}\r\n", p.len()).as_bytes());
        out.extend_from_slice(p);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"0\r\n\r\n");
    out
}

/// A POST of `body` to `path` with chunked framing.
fn chunked_post(path: &str, body: &[u8]) -> Vec<u8> {
    let mut out = head(
        &format!("POST {path} HTTP/1.1"),
        &["Host: test", "Transfer-Encoding: chunked"],
    );
    out.extend_from_slice(body);
    out
}

/// The same bytes every time: a body of `n` bytes that is not periodic with a chunk size.
fn pattern(n: usize) -> Vec<u8> {
    let mut x: u32 = 12345;
    (0..n)
        .map(|_| {
            x = x.wrapping_mul(1664525).wrapping_add(1013904223);
            (x >> 24) as u8
        })
        .collect()
}

#[test]
fn a_chunked_body_of_three_megabytes_in_chunks_of_random_sizes_arrives_whole() {
    let server = Server::app("httpd.Options { max_body_bytes: 4194304 }");
    let data = pattern(3 * 1024 * 1024);
    // Sizes from 1 to 70,000 bytes, from the same generator: some cross the 64 KiB of one read.
    let mut sizes = Vec::new();
    let mut x: u32 = 7;
    let mut at = 0;
    while at < data.len() {
        x = x.wrapping_mul(1664525).wrapping_add(1013904223);
        let n = ((x >> 8) as usize % 70_000 + 1).min(data.len() - at);
        sizes.push(n);
        at += n;
    }
    assert!(sizes.len() > 40, "{} chunks", sizes.len());
    let mut body = Vec::new();
    let mut at = 0;
    for n in &sizes {
        // Every tenth chunk has an extension, which is ignored.
        let ext = if n % 10 == 3 { ";name=value" } else { "" };
        body.extend_from_slice(format!("{n:X}{ext}\r\n").as_bytes());
        body.extend_from_slice(&data[at..at + n]);
        body.extend_from_slice(b"\r\n");
        at += n;
    }
    body.extend_from_slice(b"0\r\n\r\n");
    let mut w = server.connect();
    // The echo is the body, byte for byte; the digest is the one of the same bytes sent with a
    // length (the framing of that is tested elsewhere), and a request after the body is answered.
    w.send(chunked_post("/echo", &body));
    let r = w.read();
    assert_eq!(r.status, 200);
    assert!(
        r.body == data,
        "the echoed body differs ({} bytes)",
        r.body.len()
    );
    let by_length = {
        let mut req = request("POST", "/sum", &[], &data);
        req.extend(get("/"));
        w.send(&req);
        w.read().text()
    };
    assert_eq!(w.read().text(), "hello\n");
    w.send(chunked_post("/sum", &body));
    let chunked_sum = w.read().text();
    assert_eq!(chunked_sum, by_length);
    assert!(
        chunked_sum.starts_with(&format!("{} ", data.len())),
        "{chunked_sum}"
    );
    assert_eq!(w.call(get("/")).text(), "hello\n", "the connection is kept");
    drop(w);
    server.finish();
}

#[test]
fn chunk_extensions_and_trailers_are_ignored_and_the_next_request_is_answered() {
    let server = Server::app("httpd.Options {}");
    let mut w = server.connect();
    let mut data = chunked_post(
        "/echo",
        b"5;a=b\r\nhello\r\n1 ; c\r\n,\r\n0;done\r\nX-Sum: 1\r\nX-Other: two\r\n\r\n",
    );
    data.extend(get("/"));
    w.send(&data);
    let r = w.read();
    assert_eq!((r.status, r.text()), (200, "hello,".to_string()));
    assert!(!r.says_close());
    assert_eq!(w.read().text(), "hello\n");
    drop(w);
    assert_eq!(server.finish().seen, ["POST /echo", "GET /"]);
}

/// A POST with a chunked `body` and a request behind it in the same write: the answer must be
/// `status` with `Connection: close`, then a close with nothing more, and the second request is
/// never handled.
#[track_caller]
fn chunk_rejects(server: &Server, name: &str, body: &[u8], status: u16) {
    let mut w = server.connect();
    let mut data = chunked_post("/echo", body);
    data.extend_from_slice(SMUGGLED);
    w.send(&data);
    let r = w.read();
    assert_eq!(r.status, status, "{name}: {r:?}");
    assert!(
        r.says_close(),
        "{name}: no Connection: close in {:?}",
        r.head
    );
    w.expect_silent_close();
}

#[test]
fn a_chunked_body_that_is_wrong_is_refused_and_nothing_after_it_is_handled() {
    let server = Server::app("httpd.Options { max_header_bytes: 256, max_headers: 3 }");
    let long_ext = format!("1;{}\r\nx\r\n0\r\n\r\n", "a".repeat(5000));
    let long_trailer = format!("0\r\nX-T: {}\r\n\r\n", "a".repeat(300));
    let cases: Vec<(&str, &[u8], u16)> = vec![
        ("data not followed by CRLF", b"5\r\nhelloXX0\r\n\r\n", 400),
        ("data followed by a bare LF", b"5\r\nhello\n0\r\n\r\n", 400),
        (
            "sixteen hex digits",
            b"0000000000000001\r\nx\r\n0\r\n\r\n",
            400,
        ),
        ("no digits", b"\r\n0\r\n\r\n", 400),
        ("not hex", b"xyz\r\n0\r\n\r\n", 400),
        ("an extension over 4 KiB", long_ext.as_bytes(), 400),
        ("a bare LF after the size", b"5\nhello\r\n0\r\n\r\n", 400),
        ("a bare LF after the last size", b"0\nX-A: 1\r\n\r\n", 400),
        (
            "a bare LF in the extension",
            b"1;a\nb\r\nx\r\n0\r\n\r\n",
            400,
        ),
        (
            "a trailer that is not a field",
            b"0\r\nno colon\r\n\r\n",
            400,
        ),
        (
            "a trailer over max_header_bytes",
            long_trailer.as_bytes(),
            431,
        ),
        (
            "too many trailers",
            b"0\r\nA: 1\r\nB: 2\r\nC: 3\r\nD: 4\r\n\r\n",
            431,
        ),
    ];
    for (name, body, status) in &cases {
        chunk_rejects(&server, name, body, *status);
    }
    let seen = server.finish().seen;
    assert!(seen.iter().all(|s| !s.contains("smuggled")), "{seen:?}");
    assert_eq!(
        seen.iter().filter(|s| *s == "POST /echo").count(),
        cases.len()
    );
}

#[test]
fn a_chunked_body_that_ends_early_is_a_400_not_the_end_of_the_body() {
    let server = Server::app("httpd.Options {}");
    let cases: [(&str, &[u8]); 5] = [
        ("at a size line", b"5\r\nhello\r\n"),
        ("inside a size line", b"5\r\nhello\r\n3"),
        ("inside a chunk", b"5\r\nhel"),
        // The trap of `read_until`: the end of the stream right after the last chunk is not an
        // empty line, which would end the trailers.
        ("inside the trailers", b"5\r\nhello\r\n0\r\n"),
        ("inside a trailer field", b"5\r\nhello\r\n0\r\nX-A: 1"),
    ];
    for (name, body) in cases {
        let mut w = server.connect();
        w.send(chunked_post("/echo", body));
        w.close_write();
        let r = w.read();
        assert_eq!(r.status, 400, "{name}: {r:?}");
        w.expect_closed();
    }
    let seen = server.finish().seen;
    assert_eq!(
        seen.iter().filter(|s| *s == "body failed").count(),
        5,
        "{seen:?}"
    );
}

#[test]
fn a_chunked_body_over_the_limit_is_a_413_before_the_bytes_past_it_are_sent() {
    let server = Server::app("httpd.Options { max_body_bytes: 10 }");
    // 6 + 6 bytes: the second chunk's size says the total is over, and its data is never sent.
    let mut w = server.connect();
    w.send(chunked_post("/echo", b"6\r\nabcdef\r\n6\r\n"));
    let r = w.read();
    assert_eq!(r.status, 413, "{r:?}");
    assert!(r.says_close());
    w.expect_closed();
    // One chunk that says it is far over the limit: no data of it is sent.
    let mut w = server.connect();
    w.send(chunked_post("/echo", b"FFFFFFFFFFFFFFF\r\n"));
    assert_eq!(w.read().status, 413);
    w.expect_closed();
    // Exactly the limit is fine.
    let mut w = server.connect();
    w.send(chunked_post("/echo", &chunked(&[b"abcde", b"fghij"])));
    let r = w.read();
    assert_eq!((r.status, r.text()), (200, "abcdefghij".to_string()));
    drop(w);
    server.finish();
}

#[test]
fn chunk_extensions_count_against_the_body_limit() {
    let server = Server::app("httpd.Options { max_body_bytes: 1000 }");
    // Twenty chunks of one byte, each with 50 bytes of extension: 20 bytes of body and over
    // 1,000 on the size lines.
    let ext = "a".repeat(50);
    let mut body = Vec::new();
    for _ in 0..20 {
        body.extend_from_slice(format!("1;{ext}\r\nx\r\n").as_bytes());
    }
    body.extend_from_slice(b"0\r\n\r\n");
    let mut w = server.connect();
    w.send(chunked_post("/echo", &body));
    let r = w.read();
    assert_eq!(r.status, 413, "{r:?}");
    assert!(r.says_close());
    w.expect_closed();
    // Ordinary extensions are fine.
    let mut w = server.connect();
    w.send(chunked_post("/echo", b"5;a=b\r\nhello\r\n0;x\r\n\r\n"));
    assert_eq!(w.read().text(), "hello");
    drop(w);
    server.finish();
}

#[test]
fn a_chunked_body_the_handler_does_not_read_closes_the_connection() {
    let server = Server::app("httpd.Options {}");
    let mut w = server.connect();
    w.send(chunked_post("/ignore", &chunked(&[b"hello"])));
    let r = w.read();
    assert_eq!(r.status, 200);
    assert!(r.says_close(), "{r:?}");
    w.expect_closed();
    server.finish();
}

#[test]
fn expect_100_continue_with_a_chunked_body_is_sent_when_the_handler_reads() {
    let server = Server::app("httpd.Options {}");
    let mut w = server.connect();
    w.send(head(
        "POST /echo HTTP/1.1",
        &[
            "Host: test",
            "Expect: 100-continue",
            "Transfer-Encoding: chunked",
        ],
    ));
    let interim = w.read_within(EVENT, false).response();
    assert_eq!(interim.status, 100);
    w.send(chunked(&[b"hel", b"lo"]));
    let last = w.read();
    assert_eq!((last.status, last.text()), (200, "hello".to_string()));
    assert!(!last.says_close());
    // A handler that answers without reading sends no 100.
    let mut w2 = server.connect();
    w2.send(head(
        "POST /401 HTTP/1.1",
        &[
            "Host: test",
            "Expect: 100-continue",
            "Transfer-Encoding: chunked",
        ],
    ));
    let r = w2.read();
    assert_eq!(r.status, 401, "{r:?}");
    assert!(r.says_close());
    w2.expect_closed();
    drop(w);
    server.finish();
}

// ---------------------------------------------------------------------------------------------
// Streamed response bodies
// ---------------------------------------------------------------------------------------------

#[test]
fn a_stream_without_a_length_is_chunked_and_ends_with_a_bare_last_chunk() {
    let server = Server::app("httpd.Options {}");
    let mut w = server.connect();
    let r = w.call(get("/stream"));
    assert_eq!(r.status, 200);
    assert_eq!(r.text(), "one\ntwo\nthree\n");
    assert_eq!(r.header("transfer-encoding"), Some("chunked"));
    assert_eq!(r.header("content-length"), None);
    assert_eq!(r.chunk_sizes, [4, 4, 6]);
    assert_eq!(r.trailer_lines, 0);
    assert!(!r.says_close());
    // The connection is kept, and the framing was exact: the next response parses.
    assert_eq!(w.call(get("/")).text(), "hello\n");
    drop(w);
    server.finish();
}

#[test]
fn a_stream_with_a_length_has_content_length_and_no_chunks() {
    let server = Server::app("httpd.Options {}");
    let mut w = server.connect();
    let r = w.call(get("/stream_len"));
    assert_eq!(r.text(), "one\ntwo\nthree\n");
    assert_eq!(r.header("content-length"), Some("14"));
    assert_eq!(r.header("transfer-encoding"), None);
    assert_eq!(r.chunk_sizes, Vec::<usize>::new());
    assert_eq!(w.call(get("/")).text(), "hello\n");
    drop(w);
    server.finish();
}

#[test]
fn a_large_write_goes_out_in_chunks_of_at_most_256_kib_and_an_empty_one_not_at_all() {
    let server = Server::app("httpd.Options {}");
    let mut w = server.connect();
    let r = w.call(get("/stream_big"));
    assert_eq!(r.body.len(), 1 << 20);
    assert_eq!(r.chunk_sizes, [262144; 4]);
    let r = w.call(get("/stream_empty"));
    assert_eq!(r.text(), "x");
    assert_eq!(r.chunk_sizes, [1]);
    assert_eq!(w.call(get("/")).text(), "hello\n");
    drop(w);
    server.finish();
}

#[test]
fn a_stream_for_an_http_1_0_request_is_close_delimited() {
    let server = Server::app("httpd.Options {}");
    let mut w = server.connect();
    w.send("GET /stream HTTP/1.0\r\n\r\n");
    let r = w.read();
    assert_eq!(r.status, 200);
    assert_eq!(r.text(), "one\ntwo\nthree\n");
    assert!(r.until_close, "{r:?}");
    assert!(r.says_close());
    assert_eq!(r.header("transfer-encoding"), None);
    assert_eq!(r.header("content-length"), None);
    // Asking to keep the connection alive changes nothing for a stream with no length.
    let mut w = server.connect();
    w.send("GET /stream HTTP/1.0\r\nConnection: keep-alive\r\n\r\n");
    let r = w.read();
    assert!(r.until_close && r.says_close(), "{r:?}");
    assert_eq!(r.text(), "one\ntwo\nthree\n");
    // With a length it can be kept.
    let mut w = server.connect();
    w.send("GET /stream_len HTTP/1.0\r\nConnection: keep-alive\r\n\r\n");
    let r = w.read();
    assert_eq!(r.header("connection"), Some("keep-alive"));
    assert_eq!(r.text(), "one\ntwo\nthree\n");
    assert_eq!(w.call(get("/")).text(), "hello\n");
    drop(w);
    server.finish();
}

#[test]
fn head_of_a_stream_never_runs_the_closure_and_sends_the_length_only_when_it_is_known() {
    let server = Server::app("httpd.Options {}");
    let mut w = server.connect();
    let mut data = request("HEAD", "/stream", &[], b"");
    data.extend(request("HEAD", "/stream_len", &[], b""));
    data.extend(get("/"));
    w.send(&data);
    let h = w.read_head();
    assert_eq!(h.status, 200);
    assert_eq!(h.header("content-length"), None);
    assert_eq!(h.header("transfer-encoding"), None);
    let h = w.read_head();
    assert_eq!(h.header("content-length"), Some("14"));
    assert_eq!(w.read().text(), "hello\n");
    drop(w);
    assert_eq!(
        server.finish().seen,
        ["HEAD /stream", "HEAD /stream_len", "GET /"]
    );
}

#[test]
fn a_stream_that_ends_short_of_its_length_or_goes_past_it_or_fails_cuts_the_connection() {
    let server = Server::app("httpd.Options { log_errors: false }");
    // Short: the head promised 20 bytes, 3 came, and the end is a close, not a wait.
    let mut w = server.connect();
    w.send(get("/stream_short"));
    assert!(matches!(w.read_within(EVENT, false), Read1::Closed));
    // Past the length: the closure got the error, and at most 5 bytes of body were sent.
    let mut w = server.connect();
    w.send(get("/stream_over"));
    assert!(matches!(w.read_within(EVENT, false), Read1::Closed));
    // An error of the closure after the head: the status stays 200, and the body is cut, with
    // no last chunk to make it look whole.
    let mut w = server.connect();
    w.send(get("/stream_fail"));
    assert!(matches!(w.read_within(EVENT, false), Read1::Closed));
    let seen = server.finish().seen;
    assert_eq!(
        seen,
        [
            "GET /stream_short",
            "GET /stream_over",
            "past length",
            "GET /stream_fail"
        ]
    );
}

#[test]
fn a_client_that_leaves_during_a_stream_gets_the_closure_an_error_and_a_closure_that_goes_on_is_cancelled()
 {
    let server = Server::app("httpd.Options {}");
    let mut w = server.connect();
    w.send(get("/endless"));
    // Some of the body arrives, so the stream is running; then the client resets.
    let mut got = 0;
    while got < 100_000 {
        got += w.read_some(EVENT).len();
    }
    w.reset();
    server.wait_seen(3);
    let outcome = server.finish();
    // The closure saw the error, its next write was the one that was cancelled (nothing after
    // it ran, and its defer did), and the server went on to stop normally.
    assert_eq!(outcome.seen, ["GET /endless", "write failed", "deferred"]);
}

#[test]
fn a_client_that_never_reads_a_stream_is_cut_off_by_the_write_timeout() {
    let server = Server::app("httpd.Options { write_timeout: 0.5 }");
    let mut w = server.connect();
    w.send(get("/endless"));
    // Never read: the buffers fill, a write times out, and the connection goes.
    server.wait_connections_at_most(1);
    let outcome = {
        drop(w);
        server.finish()
    };
    assert_eq!(outcome.seen, ["GET /endless", "write failed", "deferred"]);
}

#[test]
fn a_writer_kept_after_its_response_cannot_write_into_the_next_one() {
    let server = Server::app("httpd.Options { log_errors: false }");
    let mut a = server.connect();
    let r = a.call(get("/keep"));
    assert_eq!(r.text(), "kept");
    assert!(!r.says_close());
    // Another connection's handler writes through the writer the closure kept.
    let mut b = server.connect();
    assert_eq!(b.call(get("/inject")).text(), "done\n");
    // The kept-alive connection's next response is clean: no bytes between the two.
    let next = a.call(get("/"));
    assert_eq!(next.status, 200);
    assert_eq!(next.text(), "hello\n");
    assert_eq!(next.header("x-injected"), None);
    assert_eq!(next.chunk_sizes, Vec::<usize>::new());
    drop(a);
    drop(b);
    let seen = server.finish().seen;
    assert!(seen.contains(&"inject refused".to_string()), "{seen:?}");
}

#[test]
fn a_panic_in_a_stream_closure_cuts_that_connection_and_the_server_goes_on() {
    let server = Server::app("httpd.Options {}");
    let mut w = server.connect();
    w.send(get("/stream_panic"));
    // The head and the first chunk may arrive; the response is never completed.
    assert!(matches!(w.read_within(EVENT, false), Read1::Closed));
    // Another connection is served as before.
    let mut other = server.connect();
    assert_eq!(other.call(get("/")).text(), "hello\n");
    // How a panic should be reported (a 500 for a handler, a log line) is for step 7, with
    // `join_result`. Until then the panic of a coroutine ends the run, which the harness
    // sees because it runs the event loop after `main` as `colibri run` does. Whether `serve`
    // returns before the panic surfaces is scheduler timing, and either way the run ends in it.
    let mut stop = server.connect();
    let r = stop.call(request("POST", "/__stop", &["Content-Length: 0"], b""));
    assert_eq!(r.status, 200);
    drop(stop);
    match server.done.recv_timeout(CEILING) {
        Ok(Err(e)) => assert!(e.contains("index 5 out of bounds"), "{e}"),
        Ok(Ok(printed)) => panic!("the run ended without the panic: {printed:?}"),
        Err(_) => panic!("the server did not end within {CEILING:?}"),
    }
}

// ---------------------------------------------------------------------------------------------
// max_connections and stop
// ---------------------------------------------------------------------------------------------

#[test]
fn at_the_cap_a_new_connection_waits_until_one_ends() {
    let server = Server::hello_with("httpd.Options { max_connections: 4 }");
    let mut open: Vec<Wire> = (0..4).map(|_| server.connect()).collect();
    for w in &mut open {
        assert_eq!(w.call(get("/")).status, 200);
    }
    let mut fifth = server.connect();
    fifth.send(get("/"));
    // Not answered while four are open. This is a lower bound only: the test never depends on
    // the server being slow, it asserts that it is not served while the cap is full.
    assert!(fifth.stays_quiet(Duration::from_millis(500)));
    drop(open.pop());
    let r = fifth.read();
    assert_eq!(r.status, 200);
    drop(fifth);
    drop(open);
    server.finish();
}

#[test]
fn stopping_closes_the_idle_connections_and_ends_serve_and_nothing_listens_any_more() {
    let server = Server::hello();
    let addr = server.addr.clone();
    let mut idle: Vec<Wire> = (0..3).map(|_| server.connect()).collect();
    for w in &mut idle {
        assert_eq!(w.call(get("/")).status, 200);
    }
    let seen = server.finish_slowly().seen;
    for w in &mut idle {
        w.expect_silent_close();
    }
    assert_eq!(seen, ["GET /", "GET /", "GET /"]);
    // The listener is gone: a client does not get a connection to it. What it sees is a refusal
    // (on Windows after about 2 s, so the ceiling is generous), never a served connection. The
    // port is not bound again here: whether it can be at once differs between systems.
    let target: std::net::SocketAddr = addr.parse().expect("an address");
    if let Ok(stream) = std::net::TcpStream::connect_timeout(&target, EVENT) {
        // Something else took the port in the meantime, or the system accepted into a dead
        // backlog: a request must then get no answer from this server.
        let mut w = Wire::from_stream(stream);
        let _ = w.try_send(&get("/"));
        assert!(matches!(
            w.read_within(Duration::from_secs(2), false),
            Read1::Closed | Read1::Timeout
        ));
    }
}

#[test]
fn a_stop_during_a_request_lets_it_finish_and_the_answer_says_close() {
    let server = Server::app("httpd.Options {}");
    let mut waiting = server.connect();
    waiting.send(get("/wait"));
    // A second request, whose body is yet to come, is the one that will release the first. It
    // must be under way when the server stops, or the stop would close it as an idle connection.
    let mut releaser = server.connect();
    releaser.send(request("POST", "/release", &["Content-Length: 4"], b""));
    server.wait_seen(2);
    let mut stopper = server.connect();
    let r = stopper.call(request("POST", "/__stop", &["Content-Length: 0"], b""));
    assert_eq!(r.status, 200);
    drop(stopper);
    // The stop does not end the waiting request, and nothing new is accepted.
    assert!(waiting.stays_quiet(Duration::from_millis(300)));
    releaser.send("done");
    let answer = waiting.read();
    assert_eq!(answer.status, 200);
    assert_eq!(answer.text(), "released\n");
    assert!(answer.says_close());
    let last = releaser.read();
    assert_eq!(last.status, 200);
    assert!(last.says_close());
    drop(waiting);
    drop(releaser);
    assert_eq!(server.wait().seen, ["GET /wait", "POST /release"]);
}

#[test]
fn a_stop_from_a_handler_still_delivers_that_handlers_response() {
    let server = Server::hello();
    let mut w = server.connect();
    let r = w.call(request("POST", "/__stop", &["Content-Length: 0"], b""));
    assert_eq!(r.status, 200);
    assert_eq!(r.text(), "stopping\n");
    assert!(r.says_close());
    drop(w);
    server.wait();
}

// ---------------------------------------------------------------------------------------------
// The lingering close, limits, and a slow client
// ---------------------------------------------------------------------------------------------

#[test]
fn the_answer_to_a_refused_request_arrives_although_the_client_goes_on_sending() {
    // The client has sent 200 KiB the server will never use. A server that closed at once with
    // that unread could reset the connection, and a reset can take the answer with it; the
    // lingering close reads the rest and then closes in order. What the client reads is what is
    // tested, and a system may show an end or a reset after the answer (Windows resets where
    // Unix ends in order), which `Wire` counts alike. On Linux a reset leaves the data that
    // already arrived readable, so this test cannot fail there for a missing lingering close
    // (the mutation that removes it survives on Linux); on Windows the reset can take the
    // answer with it, and that is where this test shows the defect.
    let server = Server::hello();
    let mut w = server.connect();
    let mut data = head("GET * HTTP/1.1", &["Host: test"]);
    data.extend(vec![b'x'; 200 * 1024]);
    w.send(&data);
    let r = w.read();
    assert_eq!(r.status, 400);
    assert!(r.says_close());
    w.expect_closed();
    assert_eq!(server.finish().seen, Vec::<String>::new());
}

#[test]
fn a_body_over_the_limit_is_a_413_for_the_handler_that_reads_it() {
    let server = Server::app("httpd.Options { max_body_bytes: 10 }");
    let mut w = server.connect();
    w.send(request("POST", "/echo", &[], &[b'y'; 100]));
    let r = w.read();
    assert_eq!(r.status, 413);
    assert!(r.says_close());
    drop(w);
    assert_eq!(server.finish().seen, ["POST /echo", "body failed"]);
}

#[test]
fn a_body_within_the_limit_is_read_whole() {
    let server = Server::app("httpd.Options { max_body_bytes: 100 }");
    let mut w = server.connect();
    let r = w.call(request("POST", "/echo", &[], &[b'y'; 100]));
    assert_eq!(r.status, 200);
    assert_eq!(r.body.len(), 100);
    drop(w);
    server.finish();
}

#[test]
fn a_client_that_sends_half_a_head_does_not_hold_up_another_one() {
    let server = Server::hello();
    let mut slow = server.connect();
    slow.send("GET / HTTP/1.1\r\nHost: te");
    let mut fast = server.connect();
    for i in 0..10 {
        let r = fast.call(get(&format!("/?n={i}")));
        assert_eq!(r.status, 200, "request {i}");
    }
    // The slow one is still connected, and can finish its request.
    slow.send("st\r\n\r\n");
    assert_eq!(slow.read().status, 200);
    drop(slow);
    drop(fast);
    assert_eq!(server.finish().seen.len(), 11);
}

// ---------------------------------------------------------------------------------------------
// The capabilities the module needs
// ---------------------------------------------------------------------------------------------

/// What loading a program that calls `httpd.listen` says when the host grants only `caps`.
fn load_error_with(caps: &[colibri::Cap]) -> String {
    let mut builder = colibri::Vm::builder().stdout(Box::new(std::io::sink()));
    for cap in caps {
        builder = builder.allow(*cap);
    }
    let mut vm = builder.build();
    colibri_std::install(&mut vm).expect("installs");
    let source = "import httpd\n\nfn main() {\n    let _ = httpd.listen(\"127.0.0.1:0\")\n}\n";
    match vm.load_source("app/main.colibri", source) {
        Ok(()) => panic!("the program loaded although the host grants only {caps:?}"),
        Err(e) => e.render(false),
    }
}

// The golden files can only mark an error in the program's own file, and these are inside the
// module (`<std>/httpd.colibri`, gap G6), so the text is read here.
#[test]
fn a_host_that_grants_no_net_gets_a_compile_error_that_names_the_capability_and_the_module() {
    let text = load_error_with(&[colibri::Cap::Os]);
    assert!(
        text.contains("`net.listen` needs the `net` capability"),
        "{text}"
    );
    assert!(text.contains("httpd.colibri"), "{text}");
}

#[test]
fn a_host_that_grants_no_os_gets_a_compile_error_for_the_clock_of_the_date_header() {
    let text = load_error_with(&[colibri::Cap::Net]);
    assert!(
        text.contains("`time.unix` needs the `os` capability"),
        "{text}"
    );
    assert!(!text.contains("`net.listen`"), "{text}");
}

// ---------------------------------------------------------------------------------------------
// Empty lines before the request line
// ---------------------------------------------------------------------------------------------

#[test]
fn empty_lines_before_a_request_line_are_skipped() {
    let server = Server::hello();
    let mut w = server.connect();
    let mut data = b"\r\n\r\n\r\n".to_vec();
    data.extend(get("/"));
    let r = w.call(data);
    assert_eq!(r.status, 200);
    drop(w);
    assert_eq!(server.finish().seen, ["GET /"]);
}

#[test]
fn endless_empty_lines_are_cut_off_by_the_header_byte_limit() {
    let server = Server::hello();
    let mut w = server.connect();
    // 40,000 lines of 2 bytes are over the 64 KiB of `max_header_bytes`.
    let mut data = "\r\n".repeat(40_000).into_bytes();
    data.extend_from_slice(SMUGGLED);
    w.send(&data);
    let r = w.read();
    assert_eq!(r.status, 431);
    assert!(r.says_close());
    w.expect_closed();
    assert_eq!(server.finish().seen, Vec::<String>::new());
}

// ---------------------------------------------------------------------------------------------
// The router over the wire (step 6)
// ---------------------------------------------------------------------------------------------

/// An `app` that sends everything through a `Router`.
const ROUTED: &str = r#"
fn item(r: httpd.Request) -> Result[httpd.Response] {
    return ok(httpd.text(200, "item {r.param("id") or "-"}\n"))
}

fn app(r: httpd.Request, seen: Seen, server: httpd.Server) -> Result[httpd.Response] {
    let router = httpd.Router()
    router.get("/items/:id", item)
    router.post("/items/:id", item)
    router.get("/admin", fn(r: httpd.Request) -> Result[httpd.Response] {
        return ok(httpd.text(200, "admin\n"))
    })
    router.get("/files/*rest", fn(r: httpd.Request) -> Result[httpd.Response] {
        return ok(httpd.text(200, "file {r.param("rest") or "-"}\n"))
    })
    return router.handler()(r)
}
"#;

#[test]
fn a_known_path_under_another_method_is_405_with_allow_and_the_connection_stays() {
    let server = Server::start(ROUTED, "httpd.Options {}");
    let mut w = server.connect();
    let r = w.call(request("DELETE", "/items/7", &[], b""));
    assert_eq!(r.status, 405);
    assert_eq!(r.header("allow"), Some("GET, HEAD, OPTIONS, POST"));
    assert!(!r.says_close());
    // The same connection carries the next request.
    assert_eq!(w.call(get("/items/7")).text(), "item 7\n");
    drop(w);
    server.finish();
}

#[test]
fn head_goes_through_the_route_of_get_and_has_no_body() {
    let server = Server::start(ROUTED, "httpd.Options {}");
    let mut w = server.connect();
    let mut data = request("HEAD", "/items/7", &[], b"");
    data.extend(get("/items/8"));
    w.send(&data);
    let head = w.read_head();
    assert_eq!(head.status, 200);
    assert_eq!(head.header("content-length"), Some("7"));
    let next = w.read();
    assert_eq!(next.text(), "item 8\n");
    drop(w);
    assert_eq!(server.finish().seen, ["HEAD /items/7", "GET /items/8"]);
}

#[test]
fn options_is_answered_by_the_router_with_allow() {
    let server = Server::start(ROUTED, "httpd.Options {}");
    let mut w = server.connect();
    let r = w.call(request("OPTIONS", "/items/7", &[], b""));
    assert_eq!(r.status, 204);
    assert_eq!(r.header("allow"), Some("GET, HEAD, OPTIONS, POST"));
    assert_eq!(r.header("content-length"), None);
    let star = w.call(request("OPTIONS", "*", &[], b""));
    assert_eq!(star.status, 204);
    assert!(star.header("allow").is_some());
    drop(w);
    server.finish();
}

#[test]
fn a_method_nobody_knows_is_501_and_an_unknown_path_is_404() {
    let server = Server::start(ROUTED, "httpd.Options {}");
    let mut w = server.connect();
    assert_eq!(w.call(request("BREW", "/items/7", &[], b"")).status, 501);
    assert_eq!(w.call(get("/nothing")).status, 404);
    assert_eq!(w.call(get("/items/")).status, 404);
    drop(w);
    server.finish();
}

#[test]
fn the_router_reads_the_path_as_it_came_and_decodes_only_parameters() {
    let server = Server::start(ROUTED, "httpd.Options {}");
    let mut w = server.connect();
    // An escaped letter does not make `/admin` out of `/%61dmin`.
    assert_eq!(w.call(get("/%61dmin")).status, 404);
    assert_eq!(w.call(get("/admin")).status, 200);
    assert_eq!(w.call(get("/items/a%20b")).text(), "item a b\n");
    assert_eq!(w.call(get("/files/a/b%2Fc")).text(), "file a/b/c\n");
    drop(w);
    server.finish();
}

#[test]
fn dot_segments_and_escaped_control_characters_are_400_and_never_reach_a_handler() {
    let server = Server::start(ROUTED, "httpd.Options {}");
    let mut w = server.connect();
    for target in [
        "/files/../../etc/passwd",
        "/items/%2e%2e",
        "/items/%00",
        "/items/a%0d%0ab",
        "/items/%zz",
        "/files/%2e%2e%2f",
        "/files/a%2F..%2F..%2Fetc%2Fpasswd",
        "/items/x%2F..%2F..%2Fpublic",
    ] {
        let r = w.call(get(target));
        assert_eq!(r.status, 400, "{target}");
        assert!(!r.says_close(), "{target}");
    }
    drop(w);
    // They are recorded as requests (the server saw them), but no route ran: the answers
    // above are the router's, and `item` would have said `item `.
    server.finish();
}

// ---------------------------------------------------------------------------------------------
// Deadlines that no sequence of bytes may extend
// ---------------------------------------------------------------------------------------------

/// Sends `piece` every 0.3 s (the pace is the wait for an answer) until the server ends the
/// connection or `rounds` pieces are sent; what ended it, and how long it took.
fn trickle(w: &mut Wire, piece: &[u8], rounds: usize) -> (Read1, Duration, usize) {
    let started = Instant::now();
    for sent in 0..rounds {
        if !w.try_send(piece) {
            return (Read1::Closed, started.elapsed(), sent);
        }
        match w.read_within(Duration::from_millis(300), false) {
            Read1::Timeout => {}
            other => return (other, started.elapsed(), sent + 1),
        }
    }
    (Read1::Timeout, started.elapsed(), rounds)
}

#[test]
fn empty_lines_sent_slowly_do_not_extend_the_header_timeout_of_a_new_connection() {
    // An empty line every 0.3 s: each read would wait its own 0.5 s, but the empty lines and
    // the request line have 0.5 s together.
    let server = Server::app("httpd.Options { header_timeout: 0.5 }");
    let mut w = server.connect();
    let (ended, waited, sent) = trickle(&mut w, b"\r\n", 40);
    assert!(
        matches!(ended, Read1::Closed),
        "{ended:?} after {sent} lines"
    );
    assert!(waited >= NOT_BEFORE, "closed after only {waited:?}");
    assert!(waited < EVENT, "closed after {waited:?}");
    assert_eq!(server.finish().seen, Vec::<String>::new());
}

#[test]
fn empty_lines_sent_slowly_do_not_extend_the_idle_timeout_of_a_kept_alive_connection() {
    let server = Server::app("httpd.Options { idle_timeout: 0.5 }");
    let mut w = server.connect();
    assert_eq!(w.call(get("/")).status, 200);
    let (ended, waited, sent) = trickle(&mut w, b"\r\n", 40);
    assert!(
        matches!(ended, Read1::Closed),
        "{ended:?} after {sent} lines"
    );
    assert!(waited >= NOT_BEFORE, "closed after only {waited:?}");
    assert!(waited < EVENT, "closed after {waited:?}");
    drop(w);
    assert_eq!(server.finish().seen, ["GET /"]);
}

#[test]
fn a_request_line_sent_a_byte_at_a_time_is_cut_off_by_the_same_deadline() {
    let server = Server::app("httpd.Options { header_timeout: 0.5 }");
    let mut w = server.connect();
    let (ended, waited, _) = trickle(&mut w, b"G", 40);
    assert!(matches!(ended, Read1::Closed), "{ended:?}");
    assert!(waited >= NOT_BEFORE && waited < EVENT, "{waited:?}");
    assert_eq!(server.finish().seen, Vec::<String>::new());
}

#[test]
fn a_body_sent_a_byte_at_a_time_gets_a_408_after_the_body_timeout() {
    // One byte every 0.3 s is a read that never waits 0.5 s, but a piece has 0.5 s altogether.
    let server = Server::app("httpd.Options { body_timeout: 0.5 }");
    let mut w = server.connect();
    w.send(request("POST", "/echo", &["Content-Length: 100"], b""));
    let (ended, waited, sent) = trickle(&mut w, b"x", 40);
    let r = ended.response();
    assert_eq!(r.status, 408, "after {sent} bytes");
    assert!(r.says_close());
    assert!(waited >= NOT_BEFORE && waited < EVENT, "{waited:?}");
    drop(w);
    assert_eq!(server.finish().seen, ["POST /echo", "body failed"]);
}

#[test]
fn a_body_that_arrives_in_pieces_within_the_timeout_is_read_whole() {
    let server = Server::app("httpd.Options { body_timeout: 5.0 }");
    let mut w = server.connect();
    w.send(request("POST", "/echo", &["Content-Length: 6"], b""));
    for piece in ["ab", "cd", "ef"] {
        w.send(piece);
        thread::sleep(Duration::from_millis(50));
    }
    let r = w.read();
    assert_eq!(r.status, 200);
    assert_eq!(r.text(), "abcdef");
    drop(w);
    server.finish();
}

// ---------------------------------------------------------------------------------------------
// Refusals: HEAD, and lines over 1 MiB
// ---------------------------------------------------------------------------------------------

#[test]
fn a_refusal_of_a_head_request_has_no_body() {
    let server = Server::hello();
    let mut w = server.connect();
    w.send(head("HEAD / HTTP/1.1", &["Host: test", "X-A : 1"]));
    let r = w.read_head();
    assert_eq!(r.status, 400);
    assert!(r.header("content-length").is_some());
    assert!(r.says_close());
    // Nothing follows the head: the text of the error is not sent.
    w.expect_silent_close();
    assert_eq!(server.finish().seen, Vec::<String>::new());
}

/// A line over its limit is refused as soon as the bytes show it, so a client that sends the
/// beginning of a huge line and then waits is answered without the rest. The deadlines are
/// larger than what a test waits for: without the line limit the server would wait for the end
/// of the line, and the answer would be a 408 or none.
const SLOW_HEAD: &str = "httpd.Options { header_timeout: 60.0, idle_timeout: 60.0 }";

#[test]
fn a_request_line_over_its_limit_is_a_414_before_its_end_arrives() {
    let server = Server::hello_with(SLOW_HEAD);
    let mut w = server.connect();
    // 8,193 bytes and no end of line: one over the default `max_request_line`.
    w.send(format!("GET /{}", "a".repeat(8193 - 5)));
    let r = w.read();
    assert_eq!(r.status, 414);
    assert!(r.says_close());
    w.expect_closed();
    assert_eq!(server.finish().seen, Vec::<String>::new());
}

#[test]
fn a_request_line_of_exactly_its_limit_is_served() {
    let server = Server::hello_with(SLOW_HEAD);
    let mut w = server.connect();
    // "GET " + path + " HTTP/1.1" is 8,192 bytes, the default `max_request_line`.
    let path = format!("/{}", "a".repeat(8192 - 13 - 1));
    w.send(format!("GET {path} HTTP/1.1\r\nHost: test\r\n\r\n"));
    assert_eq!(w.read().status, 404);
    let mut w = server.connect();
    let path = format!("/{}", "a".repeat(8192 - 13));
    w.send(format!("GET {path} HTTP/1.1\r\nHost: test\r\n\r\n"));
    assert_eq!(w.read().status, 414);
    server.finish();
}

#[test]
fn a_field_line_between_8_and_64_kib_is_served() {
    let server = Server::hello_with(SLOW_HEAD);
    let mut w = server.connect();
    // A large cookie: 10 KiB, over `max_request_line` and well under `max_header_bytes`.
    let cookie = "a".repeat(10 * 1024);
    w.send(format!(
        "GET / HTTP/1.1\r\nHost: test\r\nCookie: {cookie}\r\n\r\n"
    ));
    assert_eq!(w.read().status, 200);
    // And on the same connection, a request line over its limit is still refused before its
    // end arrives: the line limits are set again for each request.
    w.send(format!("GET /{}", "a".repeat(9000)));
    assert_eq!(w.read().status, 414);
    w.expect_closed();
    server.finish();
}

#[test]
fn a_field_line_over_its_limit_is_a_431_before_its_end_arrives() {
    let server = Server::hello_with(SLOW_HEAD);
    let mut w = server.connect();
    // The default `max_header_bytes` is 65,536: a field line of 65,537 bytes without its end.
    w.send(format!(
        "GET / HTTP/1.1\r\nHost: test\r\nX-Long: {}",
        "a".repeat(65_537)
    ));
    let r = w.read();
    assert_eq!(r.status, 431);
    assert!(r.says_close());
    w.expect_closed();
    // A request line over the limit with its end is a 414 as well, and a field line over it
    // with its end a 431: the answer does not depend on whether the end has arrived.
    let mut w = server.connect();
    w.send(format!(
        "GET /{} HTTP/1.1\r\nHost: test\r\n\r\n",
        "a".repeat(9000)
    ));
    assert_eq!(w.read().status, 414);
    assert_eq!(server.finish().seen, Vec::<String>::new());
}
