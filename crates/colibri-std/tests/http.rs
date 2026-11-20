//! `http` against a server inside the test process (127.0.0.1, an ephemeral port): no test
//! touches the real network. The server is a few lines of hand-written HTTP/1.1.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::Duration;

use colibri_runtime as colibri;

/// Starts the server and returns its base URL (`http://127.0.0.1:PORT`). It lives until the
/// process ends.
fn serve() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("binds");
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            thread::spawn(move || handle(stream));
        }
    });
    format!("http://127.0.0.1:{port}")
}

fn handle(mut stream: TcpStream) {
    let mut data = Vec::new();
    let mut chunk = [0u8; 1024];
    let head_end = loop {
        let n = stream.read(&mut chunk).unwrap_or(0);
        if n == 0 {
            return;
        }
        data.extend_from_slice(&chunk[..n]);
        if let Some(i) = data.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
    };
    let head = String::from_utf8_lossy(&data[..head_end]).into_owned();
    let length = head
        .lines()
        .find_map(|l| {
            let (name, value) = l.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())?
        })
        .unwrap_or(0);
    while data.len() < head_end + length {
        let n = stream.read(&mut chunk).unwrap_or(0);
        if n == 0 {
            break;
        }
        data.extend_from_slice(&chunk[..n]);
    }
    let body = &data[head_end..];
    let mut first = head.lines().next().unwrap_or("").split(' ');
    let method = first.next().unwrap_or("");
    let path = first.next().unwrap_or("");
    let header = |name: &str| {
        head.lines()
            .find_map(|l| {
                let (n, v) = l.split_once(':')?;
                n.eq_ignore_ascii_case(name).then(|| v.trim().to_string())
            })
            .unwrap_or_default()
    };
    let (status, extra, payload): (&str, &str, Vec<u8>) = match path {
        "/hello" => (
            "200 OK",
            "X-Thing: a\r\nSet-Cookie: one=1\r\nSet-Cookie: two=2\r\n",
            b"hello".to_vec(),
        ),
        "/echo" => {
            let text = format!(
                "{method} {}|{}",
                header("content-type"),
                String::from_utf8_lossy(body)
            );
            ("200 OK", "", text.into_bytes())
        }
        "/headers" => ("200 OK", "", header("x-token").into_bytes()),
        "/missing" => ("404 Not Found", "", b"nothing here".to_vec()),
        "/binary" => ("200 OK", "", vec![b'a', 0xff, b'b']),
        "/redirect" => ("302 Found", "Location: /hello\r\n", Vec::new()),
        "/slow" => {
            thread::sleep(Duration::from_millis(1500));
            ("200 OK", "", b"late".to_vec())
        }
        _ => ("500 Oops", "", Vec::new()),
    };
    let response = format!(
        "HTTP/1.1 {status}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n",
        payload.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.write_all(&payload);
}

/// Runs `body` as the contents of `main` with `import http` (and `json`), the net capability
/// granted and `base` bound to the server's URL; returns what it printed.
fn run(base: &str, body: &str) -> String {
    let out = colibri::SharedBuffer::new();
    let mut vm = colibri::Vm::builder()
        .stdout(Box::new(out.clone()))
        .allow(colibri::Cap::Net)
        .build();
    colibri_std::install(&mut vm).expect("installs");
    let source =
        format!("import http\nimport json\nfn main() {{\n    let base = \"{base}\"\n{body}\n}}\n");
    vm.load_source("app/main.colibri", &source)
        .unwrap_or_else(|e| panic!("{}", e.render(false)));
    vm.run_main()
        .unwrap_or_else(|e| panic!("{}", e.render(false)));
    out.contents()
}

#[test]
fn get_returns_status_headers_and_body() {
    let base = serve();
    let out = run(
        &base,
        r#"
    let r = http.get("{base}/hello") or panic("request failed")
    print(r.status)
    print(r.body)
    print(r.headers["x-thing"])
    print(r.headers["content-length"])
    print(r.headers["set-cookie"])
    print(r.headers.contains_key("X-Thing"))
"#,
    );
    assert_eq!(out, "200\nhello\na\n5\none=1, two=2\nfalse\n");
}

#[test]
fn an_error_status_is_a_response_not_an_err() {
    let base = serve();
    let out = run(
        &base,
        r#"
    match http.get("{base}/missing") {
        ok(r) => print("{r.status} {r.body}")
        err(e) => print("error: {e.message()}")
    }
"#,
    );
    assert_eq!(out, "404 nothing here\n");
}

#[test]
fn post_and_request_send_the_method_headers_and_body() {
    let base = serve();
    let out = run(
        &base,
        r#"
    let a = http.post("{base}/echo", "plain") or panic("a")
    print(a.body)
    let b = http.post_as("{base}/echo", "{{\"k\": 1}}", "application/json") or panic("b")
    print(b.body)
    let c = http.request("put", "{base}/echo", {"content-type": "text/csv"}, "x,y") or panic("c")
    print(c.body)
    let d = http.request("GET", "{base}/headers", {"X-Token": "secret"}, "") or panic("d")
    print(d.body)
"#,
    );
    assert_eq!(
        out,
        "POST text/plain; charset=utf-8|plain\nPOST application/json|{\"k\": 1}\nPUT text/csv|x,y\nsecret\n"
    );
}

#[test]
fn redirects_are_followed() {
    let base = serve();
    let out = run(
        &base,
        r#"
    let r = http.get("{base}/redirect") or panic("request failed")
    print("{r.status} {r.body}")
"#,
    );
    assert_eq!(out, "200 hello\n");
}

#[test]
fn a_body_that_is_not_utf8_is_decoded_lossily() {
    let base = serve();
    let out = run(
        &base,
        r#"
    let r = http.get("{base}/binary") or panic("request failed")
    print(r.body)
"#,
    );
    assert_eq!(out, "a\u{fffd}b\n");
}

#[test]
fn json_decode_reads_a_response_body() {
    let base = serve();
    let out = run(
        &base,
        r#"
    let r = http.post_as("{base}/echo", "x", "t") or panic("request failed")
    let parts = json.decode[List[int]]("[1, 2]") or panic("json")
    print(parts)
    print(r.body)
"#,
    );
    assert_eq!(out, "[1, 2]\nPOST t|x\n");
}

#[test]
fn failures_are_errs_with_a_message() {
    let base = serve();
    let out = run(
        &base,
        r#"
    match http.get("not a url") {
        ok(_) => print("ok")
        err(e) => print(e.message())
    }
    match http.request("GE T", "{base}/hello", {:}, "") {
        ok(_) => print("ok")
        err(e) => print(e.message())
    }
    match http.request("GET", "{base}/hello", {"bad header": "x"}, "") {
        ok(_) => print("ok")
        err(e) => print("header: {e.message()}")
    }
    // Nothing listens on port 1.
    match http.get("http://127.0.0.1:1/") {
        ok(_) => print("ok")
        err(e) => print("refused: {e.message()}")
    }
"#,
    );
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 4, "{out}");
    assert!(lines[0].starts_with("http: GET not a url failed:"), "{out}");
    assert_eq!(lines[1], "http: `GE T` is not an HTTP method");
    assert!(lines[2].starts_with("header: http: GET "), "{out}");
    assert!(
        lines[3].starts_with("refused: http: GET http://127.0.0.1:1/ failed:"),
        "{out}"
    );
}

#[test]
fn the_timeout_is_configurable() {
    let base = serve();
    let out = run(
        &base,
        r#"
    http.set_timeout(0.2)
    match http.get("{base}/slow") {
        ok(r) => print("ok {r.body}")
        err(e) => print("timed out: {e.message()}")
    }
    http.set_timeout(0.0)
    let r = http.get("{base}/slow") or panic("no timeout")
    print(r.body)
"#,
    );
    let lines: Vec<&str> = out.lines().collect();
    assert!(lines[0].starts_with("timed out: http: GET "), "{out}");
    assert_eq!(lines[1], "late");
}

#[test]
fn a_negative_timeout_panics() {
    let base = serve();
    let out = colibri::SharedBuffer::new();
    let mut vm = colibri::Vm::builder()
        .stdout(Box::new(out.clone()))
        .allow(colibri::Cap::Net)
        .build();
    colibri_std::install(&mut vm).unwrap();
    let _ = base;
    vm.load_source(
        "app/main.colibri",
        "import http\nfn main() {\n    http.set_timeout(-1.0)\n}\n",
    )
    .unwrap();
    let err = vm.run_main().unwrap_err().render(false);
    assert!(
        err.contains("http.set_timeout: -1 is not a number of seconds"),
        "{err}"
    );
}

#[test]
fn https_to_a_closed_port_fails_cleanly() {
    // Exercises the rustls connector path without leaving the machine.
    let out = run(
        "",
        r#"
    match http.get("https://127.0.0.1:1/") {
        ok(_) => print("ok")
        err(e) => print("err")
    }
"#,
    );
    assert_eq!(out, "err\n");
}

/// A server that keeps connections open and counts them: `(base URL, connections accepted)`.
fn serve_keep_alive() -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let listener = TcpListener::bind("127.0.0.1:0").expect("binds");
    let port = listener.local_addr().unwrap().port();
    let accepted = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&accepted);
    thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            count.fetch_add(1, Ordering::SeqCst);
            thread::spawn(move || {
                let mut data = Vec::new();
                let mut chunk = [0u8; 1024];
                // One response per request head, until the client closes.
                loop {
                    let n = stream.read(&mut chunk).unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    data.extend_from_slice(&chunk[..n]);
                    while let Some(i) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                        data.drain(..i + 4);
                        let reply = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";
                        if stream.write_all(reply.as_bytes()).is_err() {
                            return;
                        }
                    }
                }
            });
        }
    });
    (format!("http://127.0.0.1:{port}"), accepted)
}

#[test]
fn requests_to_one_host_reuse_the_connection() {
    let (base, accepted) = serve_keep_alive();
    let out = run(
        &base,
        r#"
    for i in 0..5 {
        let r = http.get("{base}/n") or panic("request failed")
        print(r.body)
    }
"#,
    );
    assert_eq!(out, "ok\nok\nok\nok\nok\n");
    assert_eq!(
        accepted.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "five requests, one connection"
    );
}

/// A server that answers every connection with bytes that are not a TLS handshake and closes.
fn serve_garbage() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("binds");
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n");
            let _ = stream.flush();
            thread::sleep(Duration::from_millis(50));
        }
    });
    format!("127.0.0.1:{port}")
}

#[test]
fn failures_carry_an_http_error_to_branch_on() {
    let base = serve();
    let out = run(
        &base,
        r#"
    http.set_timeout(0.2)
    print(http.get("{base}/slow").err().expect("times out").as[http.Error]())
    http.set_timeout(30.0)
    print(http.get("http://127.0.0.1:1/").err().expect("refused").as[http.Error]())
    print(http.get("not a url").err().expect("bad url").as[http.Error]())
    print(http.request("GET", "{base}/hello", {"bad header": "x"}, "").err().expect("bad header").as[http.Error]())
    print(http.request("GE T", "{base}/hello", {:}, "").err().expect("bad method").as[http.Error]())
    print(error("plain").as[http.Error]())
    // The same after `?`-style wrapping, and the message is the old one.
    let e = http.get("http://127.0.0.1:1/").context("fetching").err().expect("refused")
    print(e.is[http.Error]())
    print(e.as[http.Error]().expect("kept") == http.Error.Connect)
    print("{e}".starts_with("fetching: http: GET http://127.0.0.1:1/ failed:"))
"#,
    );
    assert_eq!(
        out,
        "some(Timeout)\nsome(Connect)\nsome(InvalidUrl)\nsome(Other)\nsome(Other)\nnone\ntrue\ntrue\ntrue\n"
    );
}

#[test]
fn speaking_tls_to_a_server_that_does_not_is_a_tls_error() {
    let addr = serve_garbage();
    let out = run(
        "",
        &format!(
            r#"
    match http.get("https://{addr}/") {{
        ok(_) => print("ok")
        err(e) => {{
            print(e.as[http.Error]())
            print(e)
        }}
    }}
"#
        ),
    );
    assert!(out.starts_with("some(Tls)\nhttp: GET https://"), "{out}");
}
