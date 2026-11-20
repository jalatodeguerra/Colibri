//! A raw HTTP/1.1 client for the wire tests of `httpd`: it writes the exact bytes a test gives
//! it and reads what comes back, without any of the leniency or the tidiness of a real client.
//!
//! "Closed" means the server's end of the connection is gone, whether it ended in an orderly
//! way (EOF) or with a reset: which one a system shows after a close with unread data differs
//! (Windows resets where Unix ends in order), and the tests care that the connection is over,
//! never how. A read that gets nothing within its time is `Timeout`, and no test treats that as
//! more than "nothing yet".

use std::io::{ErrorKind, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// The longest a test waits for one event that has to happen.
pub const EVENT: Duration = Duration::from_secs(10);

/// What a read of more bytes found.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Fill {
    Data,
    Closed,
    Timeout,
}

/// What reading a response ended with.
#[derive(Debug)]
pub enum Read1 {
    Response(Response),
    /// The connection ended before a status line was complete (or never carried one).
    Closed,
    /// Nothing, or only part of a response, arrived in time.
    Timeout,
}

impl Read1 {
    /// The response, or a panic that says what came instead.
    #[track_caller]
    pub fn response(self) -> Response {
        match self {
            Read1::Response(r) => r,
            other => panic!("expected a response, got {other:?}"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Response {
    pub version: String,
    pub status: u16,
    pub reason: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// The raw bytes of the head, with the final empty line.
    pub head: String,
    /// Whether the body ended with the connection (no length, no chunks).
    pub until_close: bool,
}

impl Response {
    /// The value of the first header named `name` (case-insensitive).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// The number of header lines named `name`.
    pub fn count(&self, name: &str) -> usize {
        self.headers
            .iter()
            .filter(|(n, _)| n.eq_ignore_ascii_case(name))
            .count()
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// Whether the response says the connection will end.
    pub fn says_close(&self) -> bool {
        self.header("connection")
            .is_some_and(|v| v.eq_ignore_ascii_case("close"))
    }
}

pub struct Wire {
    stream: TcpStream,
    buf: Vec<u8>,
    /// Set once a read saw the end.
    closed: bool,
}

impl Wire {
    /// Connects, and fails the test if it cannot within the ceiling.
    pub fn connect(addr: &str) -> Wire {
        let stream = TcpStream::connect(addr).expect("connects to the server");
        stream.set_nodelay(true).ok();
        Wire {
            stream,
            buf: Vec::new(),
            closed: false,
        }
    }

    /// Writes `data`; a failure (the server already closed) is the caller's to ignore or not.
    pub fn try_send(&mut self, data: &[u8]) -> bool {
        self.stream.set_write_timeout(Some(EVENT)).ok();
        self.stream.write_all(data).is_ok()
    }

    #[track_caller]
    pub fn send(&mut self, data: impl AsRef<[u8]>) {
        assert!(self.try_send(data.as_ref()), "the write failed");
    }

    /// Sets a reset on close (`SO_LINGER` 0) and drops the connection.
    pub fn reset(self) {
        let sock = socket2::SockRef::from(&self.stream);
        let _ = sock.set_linger(Some(Duration::ZERO));
        drop(self);
    }

    fn fill(&mut self, wait: Duration) -> Fill {
        if self.closed {
            return Fill::Closed;
        }
        self.stream
            .set_read_timeout(Some(wait.max(Duration::from_millis(1))))
            .ok();
        let mut chunk = [0u8; 16384];
        loop {
            match self.stream.read(&mut chunk) {
                Ok(0) => {
                    self.closed = true;
                    return Fill::Closed;
                }
                Ok(n) => {
                    self.buf.extend_from_slice(&chunk[..n]);
                    return Fill::Data;
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                    return Fill::Timeout;
                }
                // A reset, an abort, a broken pipe: the connection is over.
                Err(_) => {
                    self.closed = true;
                    return Fill::Closed;
                }
            }
        }
    }

    /// Reads one response, waiting up to `wait` for each piece of it. `head_only` is for the
    /// answer to a HEAD: its length says how long the body would be, and none follows.
    pub fn read_within(&mut self, wait: Duration, head_only: bool) -> Read1 {
        // The head.
        let head_end = loop {
            if let Some(i) = find(&self.buf, b"\r\n\r\n") {
                break i + 4;
            }
            match self.fill(wait) {
                Fill::Data => {}
                Fill::Closed => return Read1::Closed,
                Fill::Timeout => return Read1::Timeout,
            }
        };
        let head = String::from_utf8_lossy(&self.buf[..head_end]).into_owned();
        let mut lines = head.split("\r\n");
        let status_line = lines.next().unwrap_or("");
        let mut parts = status_line.splitn(3, ' ');
        let version = parts.next().unwrap_or("").to_string();
        // A status line is `HTTP/1.x`, a code and a reason, on one line: bytes of a body that
        // was sent when it should not have been would show up here as something else.
        assert!(
            (version == "HTTP/1.1" || version == "HTTP/1.0") && !status_line.contains(['\r', '\n']),
            "not a status line: {status_line:?}"
        );
        let status: u16 = parts
            .next()
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| panic!("not a status line: {status_line:?}"));
        let reason = parts.next().unwrap_or("").to_string();
        let mut headers = Vec::new();
        for line in lines {
            if line.is_empty() {
                continue;
            }
            let (n, v) = line
                .split_once(':')
                .unwrap_or_else(|| panic!("not a header line: {line:?}"));
            headers.push((n.to_string(), v.trim().to_string()));
        }
        self.buf.drain(..head_end);
        let mut r = Response {
            version,
            status,
            reason,
            headers,
            body: Vec::new(),
            head,
            until_close: false,
        };
        let no_body = head_only || status / 100 == 1 || status == 204 || status == 304;
        if no_body {
            return Read1::Response(r);
        }
        let chunked = r
            .header("transfer-encoding")
            .is_some_and(|v| v.to_ascii_lowercase().contains("chunked"));
        if chunked {
            loop {
                // A chunk size line.
                let line_end = loop {
                    if let Some(i) = find(&self.buf, b"\r\n") {
                        break i;
                    }
                    match self.fill(wait) {
                        Fill::Data => {}
                        Fill::Closed => return Read1::Closed,
                        Fill::Timeout => return Read1::Timeout,
                    }
                };
                let size_text = String::from_utf8_lossy(&self.buf[..line_end]).into_owned();
                let size_text = size_text.split(';').next().unwrap_or("").trim().to_string();
                let size = usize::from_str_radix(&size_text, 16)
                    .unwrap_or_else(|_| panic!("not a chunk size: {size_text:?}"));
                self.buf.drain(..line_end + 2);
                if size == 0 {
                    // Trailers (none here) and the final empty line.
                    loop {
                        let Some(i) = find(&self.buf, b"\r\n") else {
                            match self.fill(wait) {
                                Fill::Data => continue,
                                Fill::Closed => return Read1::Closed,
                                Fill::Timeout => return Read1::Timeout,
                            }
                        };
                        let empty = i == 0;
                        self.buf.drain(..i + 2);
                        if empty {
                            break;
                        }
                    }
                    return Read1::Response(r);
                }
                while self.buf.len() < size + 2 {
                    match self.fill(wait) {
                        Fill::Data => {}
                        Fill::Closed => return Read1::Closed,
                        Fill::Timeout => return Read1::Timeout,
                    }
                }
                r.body.extend_from_slice(&self.buf[..size]);
                self.buf.drain(..size + 2);
            }
        }
        if let Some(n) = r.header("content-length") {
            let n: usize = n.parse().unwrap_or_else(|_| panic!("Content-Length {n:?}"));
            while self.buf.len() < n {
                match self.fill(wait) {
                    Fill::Data => {}
                    Fill::Closed => return Read1::Closed,
                    Fill::Timeout => return Read1::Timeout,
                }
            }
            r.body = self.buf.drain(..n).collect();
            return Read1::Response(r);
        }
        // Until the connection ends.
        loop {
            match self.fill(wait) {
                Fill::Data => {}
                Fill::Closed => break,
                Fill::Timeout => return Read1::Timeout,
            }
        }
        r.body = std::mem::take(&mut self.buf);
        r.until_close = true;
        Read1::Response(r)
    }

    /// The next response, which must arrive within the ceiling.
    #[track_caller]
    pub fn read(&mut self) -> Response {
        self.read_within(EVENT, false).response()
    }

    /// The answer to a HEAD request.
    #[track_caller]
    pub fn read_head(&mut self) -> Response {
        self.read_within(EVENT, true).response()
    }

    /// Sends `request` and reads its response.
    #[track_caller]
    pub fn call(&mut self, request: impl AsRef<[u8]>) -> Response {
        self.send(request);
        self.read()
    }

    /// Reads everything up to the end of the connection, and returns it. Fails the test if the
    /// end does not come within the ceiling.
    #[track_caller]
    pub fn read_to_close(&mut self) -> Vec<u8> {
        loop {
            match self.fill(EVENT) {
                Fill::Data => {}
                Fill::Closed => return std::mem::take(&mut self.buf),
                Fill::Timeout => panic!("the connection did not end within {EVENT:?}"),
            }
        }
    }

    /// The connection must end, and nothing must have come first: a silent close.
    #[track_caller]
    pub fn expect_silent_close(&mut self) {
        let rest = self.read_to_close();
        assert!(
            rest.is_empty(),
            "expected a silent close, got {:?}",
            String::from_utf8_lossy(&rest)
        );
    }

    /// The connection must end within the ceiling, with nothing more than `already` read.
    #[track_caller]
    pub fn expect_closed(&mut self) {
        let _ = self.read_to_close();
    }

    /// Whether nothing at all arrives within `wait` (and the connection stays open).
    pub fn stays_quiet(&mut self, wait: Duration) -> bool {
        self.buf.is_empty() && self.fill(wait) == Fill::Timeout
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// A request with the given extra header lines (each without its line end) and body.
pub fn request(method: &str, target: &str, headers: &[&str], body: &[u8]) -> Vec<u8> {
    let mut out = format!("{method} {target} HTTP/1.1\r\nHost: test\r\n").into_bytes();
    for h in headers {
        out.extend_from_slice(h.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    if !body.is_empty()
        && !headers
            .iter()
            .any(|h| h.to_ascii_lowercase().starts_with("content-length"))
    {
        out.extend_from_slice(format!("Content-Length: {}\r\n", body.len()).as_bytes());
    }
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(body);
    out
}

pub fn get(target: &str) -> Vec<u8> {
    request("GET", target, &[], b"")
}
