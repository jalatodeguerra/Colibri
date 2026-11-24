//! An `httpd` server that runs out of descriptors reports it and goes on: `accept` fails while
//! the table is full, the failure is logged and tried again, and the server serves the waiting
//! connections once others have ended. The server is a script run by the real `colibri` binary
//! in a process whose limit is lowered with `ulimit -n`, which is Unix only (a Windows process
//! has no such limit to lower). Loopback only.
#![cfg(unix)]

mod common;
use common::vm_pending;

use std::io::{BufRead, Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// How long the test waits for the server to say something or to end.
const PATIENCE: Duration = Duration::from_secs(30);

/// The server process; it is killed and waited for when the test ends, so that a failure leaves
/// no orphan behind.
struct Server(Child);

/// The folder of the script, removed when the test ends, also when it fails.
struct TempDir(std::path::PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

const SERVER: &str = r#"import httpd

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
    vm_pending(
        "a_server_that_runs_out_of_descriptors_logs_it_and_serves_again_when_connections_end",
        || {
            let dir =
                std::env::temp_dir().join(format!("colibri-httpd-fds-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let _cleanup = TempDir(dir.clone());
            let script = dir.join("server.colibri");
            std::fs::write(&script, SERVER).unwrap();
            let mut child = Server(
                Command::new("sh")
                    .args(["-c", "ulimit -n 40; exec \"$0\" run \"$1\""])
                    .arg(env!("CARGO_BIN_EXE_colibri"))
                    .arg(&script)
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .expect("sh runs"),
            );
            // The pipes are read on threads, so that a server that says nothing, or never ends, fails the
            // test after `PATIENCE` instead of hanging it.
            let (first_tx, first_rx) = mpsc::channel();
            let stdout = child.0.stdout.take().unwrap();
            std::thread::spawn(move || {
                let mut out = std::io::BufReader::new(stdout);
                let mut first = String::new();
                let _ = out.read_line(&mut first);
                let _ = first_tx.send(first);
                let _ = std::io::copy(&mut out, &mut std::io::sink());
            });
            // Each line of the log is sent as it comes, so the test can wait for the failure itself
            // instead of guessing when the server has tried to accept past its limit.
            let (log_tx, log_rx) = mpsc::channel::<String>();
            let stderr = child.0.stderr.take().unwrap();
            let owner = std::thread::current().name().map(String::from);
            std::thread::spawn(move || {
                for line in std::io::BufReader::new(stderr).lines() {
                    let Ok(line) = line else { break };
                    common::note_child_stderr(&owner, &line);
                    if log_tx.send(line).is_err() {
                        break;
                    }
                }
            });
            let mut log = String::new();
            let first = first_rx
                .recv_timeout(PATIENCE)
                .expect("the server printed its address in time");
            let Some(addr) = first.trim().strip_prefix("listening on ") else {
                // The server did not start: its stderr says why.
                let mut err = String::new();
                while let Ok(line) = log_rx.recv_timeout(Duration::from_secs(2)) {
                    err.push_str(&line);
                    err.push('\n');
                }
                common::no_refusal(&err);
                panic!("unexpected first line {first:?}; stderr: {err}");
            };
            let addr = addr.to_string();

            // Many more connections than the server has descriptors for: each is served or waits in the
            // queue of the listener.
            let mut crowd: Vec<TcpStream> = (0..80)
                .map(|_| TcpStream::connect(&addr).expect("connects"))
                .collect();
            // The server accepts until its table is full and then logs the failure; nothing goes away
            // before that, so the failure cannot be missed however the scheduler orders the accepts.
            let started = Instant::now();
            while !log.contains("accept failed") {
                let left = PATIENCE
                    .checked_sub(started.elapsed())
                    .unwrap_or_else(|| panic!("nothing was logged in time: {log:?}"));
                match log_rx.recv_timeout(left) {
                    Ok(line) => {
                        log.push_str(&line);
                        log.push('\n');
                    }
                    Err(_) => panic!("nothing was logged in time: {log:?}"),
                }
            }
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
            let started = Instant::now();
            let status = loop {
                if let Some(status) = child.0.try_wait().unwrap() {
                    break status;
                }
                assert!(
                    started.elapsed() < PATIENCE,
                    "the server did not end: {log}"
                );
                std::thread::sleep(Duration::from_millis(20));
            };
            // The rest of the log, which ends when the server closes its stderr.
            while let Ok(line) = log_rx.recv_timeout(Duration::from_secs(5)) {
                log.push_str(&line);
                log.push('\n');
            }
            assert_eq!(status.code(), Some(0), "{log}");
        },
    )
}
