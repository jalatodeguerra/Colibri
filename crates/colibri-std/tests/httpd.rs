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
use wire::{EVENT, Read1, Response, Wire, get, request};

/// The longest a whole server run may take once it is told to stop.
const CEILING: Duration = Duration::from_secs(30);

/// The Colibri program around a test's `app` function and options.
const PROGRAM: &str = r#"import httpd
import str
@IMPORTS@

class Seen {
    items: List[str] = []
    release: Signal = Signal()
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
    assert_eq!(r.status, 200);
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
