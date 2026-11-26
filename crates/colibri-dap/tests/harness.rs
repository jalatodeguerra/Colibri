//! A scripted client for `colibri debug`: starts the real binary, sends DAP requests over its
//! standard input and reads what it writes to its standard output, frame by frame.
//!
//! Nothing here waits for a time to pass. Every wait is for a message, with a long timeout
//! that only ends a test whose adapter hangs.

#![allow(dead_code)]

use std::collections::VecDeque;
use std::io::{BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

use colibri_dap::{ReadError, read_message};
use serde_json::{Value, json};

/// How long a wait for one message may take before the test is called hung.
const HANG_GUARD: Duration = Duration::from_secs(120);

static NEXT: AtomicU32 = AtomicU32::new(0);

/// A folder of the test's own, removed at the end.
pub struct Temp(pub PathBuf);

impl Temp {
    pub fn new(name: &str) -> Temp {
        let n = NEXT.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("colibri-dap-{}-{n}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a temp folder");
        Temp(dir)
    }

    pub fn write(&self, file: &str, text: &str) -> PathBuf {
        let path = self.0.join(file);
        std::fs::write(&path, text).expect("write a source file");
        path
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The 1-based line that carries `// @name` in `source`.
pub fn line_of(source: &str, name: &str) -> i64 {
    let marker = format!("// @{name}");
    source
        .lines()
        .position(|l| l.contains(&marker))
        .unwrap_or_else(|| panic!("no `{marker}` in the program")) as i64
        + 1
}

pub struct Dap {
    child: Child,
    stdin: Option<ChildStdin>,
    /// What the adapter wrote to its own standard error, read on a thread until it ends.
    stderr: Option<std::thread::JoinHandle<String>>,
    rx: Receiver<Result<Value, String>>,
    /// Messages received and not yet taken, in order.
    inbox: VecDeque<Value>,
    /// Everything received.
    pub history: Vec<Value>,
    seq: i64,
    pub temp: Temp,
    /// The program's path, once `launch` has been sent.
    pub program: PathBuf,
}

impl Dap {
    pub fn start(name: &str) -> Dap {
        let mut child = Command::new(env!("CARGO_BIN_EXE_colibri"))
            .arg("debug")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start `colibri debug`");
        let stdin = child.stdin.take();
        let mut err = child.stderr.take().expect("piped stderr");
        let stderr = std::thread::spawn(move || {
            let mut text = String::new();
            let _ = std::io::Read::read_to_string(&mut err, &mut text);
            text
        });
        let mut out = BufReader::new(child.stdout.take().expect("piped stdout"));
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            loop {
                match read_message(&mut out) {
                    Ok(v) => {
                        if tx.send(Ok(v)).is_err() {
                            return;
                        }
                    }
                    Err(ReadError::Eof) => return,
                    Err(ReadError::Malformed(m) | ReadError::Fatal(m)) => {
                        let _ = tx.send(Err(m));
                        return;
                    }
                }
            }
        });
        Dap {
            child,
            stdin,
            stderr: Some(stderr),
            rx,
            inbox: VecDeque::new(),
            history: Vec::new(),
            seq: 0,
            temp: Temp::new(name),
            program: PathBuf::new(),
        }
    }

    /// Writes bytes to the adapter exactly as given.
    pub fn send_raw(&mut self, bytes: &[u8]) {
        let stdin = self.stdin.as_mut().expect("stdin is open");
        stdin.write_all(bytes).expect("write to the adapter");
        stdin.flush().expect("flush");
    }

    /// Sends a request; returns its `seq`.
    pub fn send(&mut self, command: &str, arguments: Value) -> i64 {
        self.seq += 1;
        let body = json!({
            "seq": self.seq,
            "type": "request",
            "command": command,
            "arguments": arguments,
        })
        .to_string();
        let frame = format!("Content-Length: {}\r\n\r\n{body}", body.len());
        self.send_raw(frame.as_bytes());
        self.seq
    }

    /// Reads one more message from the adapter into the inbox.
    fn pump(&mut self) {
        match self.rx.recv_timeout(HANG_GUARD) {
            Ok(Ok(message)) => {
                self.history.push(message.clone());
                self.inbox.push_back(message);
            }
            Ok(Err(why)) => panic!(
                "the adapter wrote something that is not a DAP message ({why}); so far: {:#?}",
                self.history
            ),
            Err(RecvTimeoutError::Timeout) => {
                panic!("the adapter went quiet; so far: {:#?}", self.history)
            }
            Err(RecvTimeoutError::Disconnected) => {
                panic!("the adapter ended its output; so far: {:#?}", self.history)
            }
        }
    }

    /// Takes the first message in the inbox that `want` accepts, waiting for one.
    fn take(&mut self, mut want: impl FnMut(&Value) -> bool) -> Value {
        loop {
            if let Some(at) = self.inbox.iter().position(&mut want) {
                return self.inbox.remove(at).expect("a message");
            }
            self.pump();
        }
    }

    /// The whole response to request `seq`.
    pub fn response_to(&mut self, seq: i64) -> Value {
        self.take(|m| m["type"] == "response" && m["request_seq"] == seq)
    }

    /// Sends a request that must succeed; returns its body.
    pub fn request(&mut self, command: &str, arguments: Value) -> Value {
        let seq = self.send(command, arguments);
        let response = self.response_to(seq);
        assert_eq!(response["success"], true, "{command}: {response}");
        response["body"].clone()
    }

    /// Sends a request that must be refused; returns the message.
    pub fn request_fails(&mut self, command: &str, arguments: Value) -> String {
        let seq = self.send(command, arguments);
        let response = self.response_to(seq);
        assert_eq!(response["success"], false, "{command}: {response}");
        response["message"].as_str().expect("a message").to_string()
    }

    /// The body of the next event called `name`.
    pub fn event(&mut self, name: &str) -> Value {
        self.take(|m| m["type"] == "event" && m["event"] == name)["body"].clone()
    }

    /// Whether the inbox (what arrived and was not taken) holds an event called `name`.
    pub fn has_pending_event(&self, name: &str) -> bool {
        self.inbox
            .iter()
            .any(|m| m["type"] == "event" && m["event"] == name)
    }

    /// The `output` text of the events in `history` with this category, in order.
    pub fn output(&self, category: &str) -> String {
        self.history
            .iter()
            .filter(|m| m["event"] == "output" && m["body"]["category"] == category)
            .map(|m| m["body"]["output"].as_str().unwrap_or(""))
            .collect()
    }

    /// `initialize`, then `launch` of `source` (written as `main.colibri` in the test's folder),
    /// then waits for `initialized`. `extra` is merged into the launch arguments.
    pub fn launch(&mut self, source: &str, extra: Value) -> PathBuf {
        let path = self.temp.write("main.colibri", source);
        let body = self.request(
            "initialize",
            json!({"adapterID": "colibri", "linesStartAt1": true, "columnsStartAt1": true}),
        );
        assert_eq!(body["supportsConfigurationDoneRequest"], true);
        let mut args = json!({"program": path.to_str().expect("a UTF-8 path")});
        if let Value::Object(more) = extra {
            for (k, v) in more {
                args[k] = v;
            }
        }
        self.program = path.clone();
        self.request("launch", args);
        self.event("initialized");
        path
    }

    /// `setBreakpoints` for the program's file; each entry is a breakpoint object.
    pub fn set_breakpoints(&mut self, entries: Vec<Value>) -> Vec<Value> {
        let body = self.request(
            "setBreakpoints",
            json!({
                "source": {"path": self.program.to_str().expect("a UTF-8 path")},
                "breakpoints": entries,
            }),
        );
        body["breakpoints"].as_array().expect("breakpoints").clone()
    }

    pub fn break_at(&mut self, lines: &[i64]) -> Vec<Value> {
        self.set_breakpoints(lines.iter().map(|l| json!({"line": l})).collect())
    }

    pub fn configuration_done(&mut self) {
        self.request("configurationDone", json!({}));
    }

    /// The body of the next `stopped` event.
    pub fn stopped(&mut self) -> Value {
        self.event("stopped")
    }

    /// The innermost frame of the stopped thread: its `stackTrace` entry.
    pub fn top_frame(&mut self) -> Value {
        let body = self.request("stackTrace", json!({"threadId": 1}));
        body["stackFrames"][0].clone()
    }

    /// `(function name, line)` of the top frame.
    pub fn where_(&mut self) -> (String, i64) {
        let f = self.top_frame();
        (
            f["name"].as_str().expect("a name").to_string(),
            f["line"].as_i64().expect("a line"),
        )
    }

    /// The variables of a `variablesReference`, as `name = value`.
    pub fn variables(&mut self, reference: i64) -> Vec<Value> {
        let body = self.request("variables", json!({"variablesReference": reference}));
        body["variables"].as_array().expect("variables").clone()
    }

    /// The scopes of the frame: `(name, variablesReference)`.
    pub fn scopes(&mut self, frame: i64) -> Vec<(String, i64)> {
        let body = self.request("scopes", json!({"frameId": frame}));
        body["scopes"]
            .as_array()
            .expect("scopes")
            .iter()
            .map(|s| {
                (
                    s["name"].as_str().expect("name").to_string(),
                    s["variablesReference"].as_i64().expect("reference"),
                )
            })
            .collect()
    }

    /// The locals of the top frame as `["name = value", ..]`.
    pub fn locals(&mut self) -> Vec<String> {
        let frame = self.top_frame()["id"].as_i64().expect("a frame id");
        let locals = self.scopes(frame)[0].1;
        self.variables(locals)
            .iter()
            .map(|v| {
                format!(
                    "{} = {}",
                    v["name"].as_str().unwrap(),
                    v["value"].as_str().unwrap()
                )
            })
            .collect()
    }

    /// `continue`, then the next stop.
    pub fn resume(&mut self, command: &str) -> Value {
        self.request(command, json!({"threadId": 1}));
        self.stopped()
    }

    /// Waits until the program has ended: `terminated`. Returns the exit code, if `exited` came.
    pub fn finish(&mut self) -> Option<i64> {
        self.event("terminated");
        self.history
            .iter()
            .find(|m| m["event"] == "exited")
            .map(|m| m["body"]["exitCode"].as_i64().expect("an exit code"))
    }

    /// `disconnect`, then the adapter must end by itself.
    pub fn disconnect(self) {
        self.disconnect_stderr();
    }

    /// [`Dap::disconnect`], returning what the adapter wrote to its standard error.
    pub fn disconnect_stderr(mut self) -> String {
        self.request("disconnect", json!({}));
        drop(self.stdin.take());
        let deadline = Instant::now() + HANG_GUARD;
        loop {
            match self.child.try_wait().expect("wait") {
                Some(status) => {
                    assert!(status.success(), "the adapter ended with {status}");
                    let stderr = self.stderr.take().expect("stderr is read");
                    return stderr.join().expect("the stderr reader");
                }
                None if Instant::now() > deadline => {
                    let _ = self.child.kill();
                    panic!("the adapter did not end after `disconnect`");
                }
                None => std::thread::sleep(Duration::from_millis(5)),
            }
        }
    }
}

impl Dap {
    /// Closes the adapter's input; it must end by itself, successfully.
    pub fn close_input(mut self) {
        drop(self.stdin.take());
        let deadline = Instant::now() + HANG_GUARD;
        loop {
            match self.child.try_wait().expect("wait") {
                Some(status) => {
                    assert!(status.success(), "the adapter ended with {status}");
                    return;
                }
                None if Instant::now() > deadline => {
                    let _ = self.child.kill();
                    panic!("the adapter did not end when its input did");
                }
                None => std::thread::sleep(Duration::from_millis(5)),
            }
        }
    }
}

impl Drop for Dap {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A path as it goes into JSON.
pub fn json_path(path: &Path) -> Value {
    json!(path.to_str().expect("a UTF-8 path"))
}
