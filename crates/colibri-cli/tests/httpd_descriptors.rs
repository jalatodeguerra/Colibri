//! An `httpd` server that runs out of descriptors reports it and goes on: `accept` fails while
//! the table is full, the failure is logged and tried again, and the server serves the waiting
//! connections once others have ended. The server is a script run by the real `colibri` binary
//! in a process whose limit is lowered with `ulimit -n`, which is Unix only (a Windows process
//! has no such limit to lower). Loopback only.
#![cfg(unix)]

use std::io::{BufRead, Read, Write};
use std::net::TcpStream;
use std::process::{Command, Stdio};
use std::time::Duration;

const SERVER: &str = r#"import httpd
import io

fn main() -> Result[()] {
    let server = httpd.listen("127.0.0.1:0")?
    print("listening on {server.addr()}")
    let handler = fn(r: httpd.Request) -> Result[httpd.Response] {
        if r.path == "/__stop" {
            server.stop(2.0)
            return ok(httpd.text(200, "bye\n"))
        }
        return ok(httpd.text(200, "hello\n"))
    }
    return server.serve(handler)
}
"#;

/// One request on `stream`, and the text of the answer up to the end of its body.
fn ask(stream: &mut TcpStream, request: &str) -> String {
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    let mut got = Vec::new();
    let mut buf = [0u8; 1024];
    loop {
        let n = stream.read(&mut buf).expect("an answer");
        assert!(n > 0, "the connection ended before an answer: {got:?}");
        got.extend_from_slice(&buf[..n]);
        let text = String::from_utf8_lossy(&got).into_owned();
        if let Some(i) = text.find("\r\n\r\n") {
            let length = text
                .lines()
                .find_map(|l| l.strip_prefix("Content-Length: "))
                .and_then(|v| v.trim().parse::<usize>().ok())
                .expect("a Content-Length");
            if text.len() >= i + 4 + length {
                return text;
            }
        }
    }
}

#[test]
fn a_server_that_runs_out_of_descriptors_logs_it_and_serves_again_when_connections_end() {
    let dir = std::env::temp_dir().join(format!("colibri-httpd-fds-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("server.colibri");
    std::fs::write(&script, SERVER).unwrap();
    let mut child = Command::new("sh")
        .args(["-c", "ulimit -n 40; exec \"$0\" run \"$1\""])
        .arg(env!("CARGO_BIN_EXE_colibri"))
        .arg(&script)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("sh runs");
    let mut first = String::new();
    let mut out = std::io::BufReader::new(child.stdout.take().unwrap());
    out.read_line(&mut first).unwrap();
    let addr = first
        .trim()
        .strip_prefix("listening on ")
        .unwrap_or_else(|| panic!("unexpected first line {first:?}"))
        .to_string();

    // Many more connections than the server has descriptors for: each is served or waits in the
    // queue of the listener.
    let mut crowd: Vec<TcpStream> = (0..80)
        .map(|_| TcpStream::connect(&addr).expect("connects"))
        .collect();
    let mut a = crowd.remove(0);
    let answer = ask(&mut a, "GET / HTTP/1.1\r\nHost: t\r\n\r\n");
    assert!(answer.starts_with("HTTP/1.1 200 OK"), "{answer}");
    // They all go away; the server accepts the queued ones, finds them closed, and is free.
    drop(crowd);
    drop(a);
    let mut b = TcpStream::connect(&addr).expect("connects");
    let answer = ask(&mut b, "GET / HTTP/1.1\r\nHost: t\r\n\r\n");
    assert!(answer.ends_with("hello\n"), "{answer}");
    let answer = ask(
        &mut b,
        "POST /__stop HTTP/1.1\r\nHost: t\r\nContent-Length: 0\r\n\r\n",
    );
    assert!(answer.ends_with("bye\n"), "{answer}");
    drop(b);
    let mut log = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut log)
        .unwrap();
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(0), "{log}");
    assert!(log.contains("accept failed"), "nothing was logged: {log:?}");
    let _ = std::fs::remove_dir_all(&dir);
}
