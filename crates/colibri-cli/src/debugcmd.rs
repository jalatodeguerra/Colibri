//! `colibri debug`: the debug adapter (Debug Adapter Protocol on standard input and output).
//! The protocol is in `colibri-dap`; this builds the `Vm` of a `launch` as `colibri run` does.

use std::io::{self, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use colibri_dap::LaunchArgs;

use crate::depscmd::{self, DepFlags};

/// Runs the adapter until the client disconnects.
///
/// A package with `[rust-dependencies]` runs in its project runner: this `colibri` reads the
/// requests up to `launch` (answering `initialize` itself, since a client waits for that before it
/// launches), finds the package the launch names, and when the package declares Rust bindings
/// builds the runner (what it prints goes to the client as `output` events: standard output is
/// the protocol), starts `runner debug`, replays the requests it read, and copies the two
/// streams. Anything else is served here, with the requests replayed the same way.
pub fn run() -> ExitCode {
    if std::env::var_os(crate::delegate::GUARD).is_some() {
        return run_local();
    }
    proxy()
}

/// The adapter in this process: what a runner, and `colibri` for a package without bindings, do.
pub fn run_local() -> ExitCode {
    serve_here(&[], BufReader::new(io::stdin()), protocol_output(), false)
}

type Output = std::sync::Arc<std::sync::Mutex<Box<dyn io::Write + Send>>>;

/// Serves in this process. `replay` is what a client already sent (read before it was known
/// who serves it); with `answered`, the response to its `initialize` was written already, so the
/// second one is dropped.
fn serve_here(
    replay: &[serde_json::Value],
    input: impl io::BufRead + Send + 'static,
    out: Box<dyn io::Write + Send>,
    answered: bool,
) -> ExitCode {
    let bytes = frames(replay);
    let input = BufReader::new(io::Read::chain(io::Cursor::new(bytes), input));
    let out: Box<dyn io::Write + Send> = if answered {
        Box::new(DropFirstInitialize::new(out))
    } else {
        out
    };
    let code = colibri_dap::serve(input, out, build_program_vm);
    ExitCode::from(code as u8)
}

/// The requests read so far, as they are written again.
fn frames(replay: &[serde_json::Value]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for message in replay {
        let _ = colibri_dap::write_message(&mut bytes, message);
    }
    bytes
}

/// Passes on what the adapter writes, except the first response to `initialize` (the client has
/// the one its proxy wrote). Frames are told apart as the adapter writes them; after that
/// response the bytes pass as they come.
struct DropFirstInitialize {
    inner: Box<dyn io::Write + Send>,
    pending: Vec<u8>,
    done: bool,
}

impl DropFirstInitialize {
    fn new(inner: Box<dyn io::Write + Send>) -> Self {
        DropFirstInitialize {
            inner,
            pending: Vec::new(),
            done: false,
        }
    }

    /// The length of the first whole frame in `pending`, and its body.
    fn frame(&self) -> Option<(usize, std::ops::Range<usize>)> {
        let end = self.pending.windows(4).position(|w| w == b"\r\n\r\n")? + 4;
        let head = String::from_utf8_lossy(&self.pending[..end]);
        let length: usize = head
            .lines()
            .find_map(|l| l.strip_prefix("Content-Length:"))?
            .trim()
            .parse()
            .ok()?;
        (self.pending.len() >= end + length).then_some((end + length, end..end + length))
    }
}

impl io::Write for DropFirstInitialize {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.done {
            return self.inner.write(buf);
        }
        self.pending.extend_from_slice(buf);
        while !self.done {
            let Some((total, body)) = self.frame() else {
                break;
            };
            let message: serde_json::Value =
                serde_json::from_slice(&self.pending[body]).unwrap_or_default();
            let frame: Vec<u8> = self.pending.drain(..total).collect();
            if message["type"] == "response" && message["command"] == "initialize" {
                self.done = true;
            } else {
                self.inner.write_all(&frame)?;
            }
        }
        if self.done {
            let rest = std::mem::take(&mut self.pending);
            self.inner.write_all(&rest)?;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// An `output` event written by the proxy (it has no `seq` of the adapter's to continue).
fn output_event(out: &Output, text: &str) {
    output_in(out, "console", &format!("{text}\n"));
}

fn output_in(out: &Output, category: &str, text: &str) {
    let message = serde_json::json!({
        "seq": 0,
        "type": "event",
        "event": "output",
        "body": {"category": category, "output": text},
    });
    let mut out = out.lock().unwrap_or_else(|e| e.into_inner());
    let _ = colibri_dap::write_message(&mut *out, &message);
}

/// What the adapter answers to `initialize`: its own capabilities, as its first message (the one
/// the adapter that serves the rest then does not repeat).
fn initialize_response(request: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "seq": 1,
        "type": "response",
        "request_seq": request["seq"],
        "command": "initialize",
        "success": true,
        "body": colibri_dap::capabilities(),
    })
}

/// The runner a launch needs, `None` when the package has no Rust bindings. `Err` is what to tell
/// the client.
fn runner_for(launch: &serde_json::Value, out: &Output) -> Result<Option<PathBuf>, String> {
    use crate::bindings::build::{self, Env, Options, Profile};
    use crate::delegate::{self, Wants};
    let args = &launch["arguments"];
    let Some(program) = args["program"].as_str().filter(|p| !p.is_empty()) else {
        return Ok(None);
    };
    let mut program = PathBuf::from(program);
    if let Some(cwd) = args["cwd"].as_str().filter(|c| !c.is_empty()) {
        program = Path::new(cwd).join(program);
    }
    let folder = delegate::package_folder(&program);
    let given = args["runner"].as_str().filter(|r| !r.is_empty());
    let wants = delegate::manifest_wants_bindings(&folder.join(colibri_runtime::MANIFEST_FILE));
    if wants == Wants::No && given.is_none() {
        return Ok(None);
    }
    if !folder.join(colibri_runtime::MANIFEST_FILE).is_file() {
        return match given {
            Some(given) => Err(format!(
                "`runner` {given}: `{}` has no `colibri.toml`, so no Rust bindings to run with it",
                folder.display()
            )),
            None => Ok(None),
        };
    }
    // Dependencies are fetched first (a dependency may declare bindings); what that says is the
    // client's to read. A failure is the program's to report when it is served here.
    let flags = DepFlags::default();
    let Ok(notes) = depscmd::prepare_quiet(&program, flags) else {
        return Ok(None);
    };
    for note in notes {
        output_event(out, &format!("  {note}"));
    }
    if wants == Wants::Maybe
        && given.is_none()
        && !colibri_runtime::declares_rust_bindings(
            &colibri_runtime::DiskFs,
            &folder,
            crate::build_cfg(),
        )?
    {
        return Ok(None);
    }
    let (root, found) = build::project(&folder)?;
    if let Some(given) = given {
        if found.is_empty() {
            return Err(format!(
                "`runner` {given}: this project declares no `[rust-dependencies]`, so it has no runner"
            ));
        }
        delegate::check_runner(Path::new(given), &found)?;
        return Ok(Some(PathBuf::from(given)));
    }
    if found.is_empty() {
        return Ok(None);
    }
    let env = Env::current()?;
    let options = Options {
        profile: Profile::Run,
        locked: flags.locked,
        offline: flags.offline,
        write_lock: true,
        write_interfaces: true,
        rebuilding: false,
    };
    let sink_out = std::sync::Arc::clone(out);
    build::set_progress(Some(Box::new(move |line| output_event(&sink_out, line))));
    let built = build::ensure_runner(&env, &root, &found, options);
    if let Ok(runner) = &built {
        delegate::say_interfaces(runner);
    }
    build::set_progress(None);
    Ok(Some(built?.path))
}

/// Reads requests up to `launch`, then serves the session here or in the project runner.
fn proxy() -> ExitCode {
    let out: Output = std::sync::Arc::new(std::sync::Mutex::new(protocol_output()));
    let mut input = BufReader::new(io::stdin());
    let mut buffered: Vec<serde_json::Value> = Vec::new();
    let mut answered = false;
    let launch = loop {
        match colibri_dap::read_message(&mut input) {
            // A message the adapter cannot use is reported when it comes, as the adapter does.
            Ok(message) if colibri_dap::request_problem(&message).is_some() => {
                let why = colibri_dap::request_problem(&message).unwrap_or_default();
                output_in(
                    &out,
                    "stderr",
                    &format!("colibri debug: message ignored: {why}\n"),
                );
            }
            Ok(message) if message["type"] == "request" => {
                let command = message["command"].as_str().unwrap_or("").to_string();
                if command == "initialize" && !answered {
                    answered = true;
                    let mut w = out.lock().unwrap_or_else(|e| e.into_inner());
                    let _ = colibri_dap::write_message(&mut *w, &initialize_response(&message));
                }
                buffered.push(message.clone());
                if command == "launch" {
                    break Some(message);
                }
                // A client that leaves before it launches: the adapter that serves here answers.
                if matches!(command.as_str(), "disconnect" | "terminate") {
                    break None;
                }
            }
            Ok(other) => buffered.push(other),
            // What cannot be used is left to the adapter that serves the rest.
            Err(colibri_dap::ReadError::Malformed(why)) => output_in(
                &out,
                "stderr",
                &format!("colibri debug: message ignored: {why}\n"),
            ),
            Err(_) => break None,
        }
    };
    let runner = match &launch {
        Some(launch) => match runner_for(launch, &out) {
            Ok(runner) => runner,
            Err(why) => return refuse(launch, &why, &out, &mut input),
        },
        None => None,
    };
    let Some(runner) = runner else {
        return serve_here(&buffered, input, Box::new(SharedWrite(out)), answered);
    };
    through_runner(&runner, &buffered, input, out, answered)
}

/// The launch cannot be served: the client is told, and the session ends when it disconnects.
fn refuse(
    launch: &serde_json::Value,
    why: &str,
    out: &Output,
    input: &mut impl io::BufRead,
) -> ExitCode {
    let send = |message: serde_json::Value| {
        let mut w = out.lock().unwrap_or_else(|e| e.into_inner());
        let _ = colibri_dap::write_message(&mut *w, &message);
    };
    send(serde_json::json!({
        "seq": 0, "type": "response", "request_seq": launch["seq"], "command": "launch",
        "success": false, "message": why,
    }));
    send(serde_json::json!({"seq": 0, "type": "event", "event": "terminated", "body": {}}));
    while let Ok(request) = colibri_dap::read_message(input) {
        let command = request["command"].as_str().unwrap_or("");
        let ok = matches!(command, "disconnect" | "terminate");
        send(serde_json::json!({
            "seq": 0, "type": "response", "request_seq": request["seq"], "command": command,
            "success": ok,
            "message": if ok { serde_json::Value::Null } else { "the program could not be started".into() },
        }));
        if ok {
            return ExitCode::SUCCESS;
        }
    }
    ExitCode::from(1)
}

/// Starts `runner debug`, replays what was read, and copies both streams until it ends.
fn through_runner(
    runner: &Path,
    buffered: &[serde_json::Value],
    mut input: impl io::BufRead + Send + 'static,
    out: Output,
    answered: bool,
) -> ExitCode {
    use std::process::{Command, Stdio};
    let mut command = Command::new(runner);
    command
        .arg("debug")
        .env(crate::delegate::GUARD, "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            output_event(
                &out,
                &format!("error: cannot run the runner `{}`: {e}", runner.display()),
            );
            return ExitCode::from(1);
        }
    };
    let mut to_runner = child.stdin.take().expect("piped");
    let mut from_runner = BufReader::new(child.stdout.take().expect("piped"));
    // The client's requests: those already read, then the rest as they come. The runner's end of
    // the pipe closes when the client's input ends, which is how it learns the client is gone.
    let replay = frames(buffered);
    std::thread::spawn(move || {
        if to_runner.write_all(&replay).is_err() {
            return;
        }
        while let Ok(message) = colibri_dap::read_message(&mut input) {
            if colibri_dap::write_message(&mut to_runner, &message).is_err() {
                return;
            }
        }
    });
    // The runner's messages, except the response to the `initialize` that was answered already.
    let copier = std::thread::spawn({
        let out = std::sync::Arc::clone(&out);
        move || {
            let mut skip = answered;
            while let Ok(message) = colibri_dap::read_message(&mut from_runner) {
                if skip && message["type"] == "response" && message["command"] == "initialize" {
                    skip = false;
                    continue;
                }
                let mut w = out.lock().unwrap_or_else(|e| e.into_inner());
                if colibri_dap::write_message(&mut *w, &message).is_err() {
                    return;
                }
            }
        }
    });
    let status = child.wait();
    let _ = copier.join();
    match status {
        Ok(status) => ExitCode::from(status.code().unwrap_or(1) as u8),
        Err(_) => ExitCode::from(1),
    }
}

/// Where the protocol is written. On Unix the adapter keeps its own copy of the standard
/// output and points file descriptor 1 at standard error, so that whatever else writes there
/// (a program started with `process.status`, a C function called through `extern`) cannot
/// corrupt the stream. Windows does the same with its standard output handle. C code called through
/// `extern` that writes to the C runtime's descriptor 1 still reaches the protocol stream there.
#[cfg(unix)]
fn protocol_output() -> Box<dyn io::Write + Send> {
    use std::os::fd::FromRawFd;
    // SAFETY: `fcntl` with `F_DUPFD_CLOEXEC` gives a new descriptor (3 or above, closed in
    // every program this one starts) that this function owns from here on (it is checked);
    // `dup2` then makes descriptor 1 refer to what descriptor 2 refers to.
    unsafe {
        let own = libc::fcntl(1, libc::F_DUPFD_CLOEXEC, 3);
        if own < 0 || libc::dup2(2, 1) < 0 {
            if own >= 0 {
                libc::close(own);
            }
            return Box::new(io::stdout());
        }
        Box::new(std::fs::File::from_raw_fd(own))
    }
}

/// Windows: the adapter keeps a private (not inheritable) duplicate of the standard output
/// handle for the protocol, and makes the process's standard output handle the standard error
/// handle. A program started with `process.status` takes the process's standard output at the
/// moment it starts (std reads it then), so it writes to standard error as on Unix.
#[cfg(windows)]
fn protocol_output() -> Box<dyn io::Write + Send> {
    use std::os::windows::io::{AsHandle, AsRawHandle};

    unsafe extern "system" {
        /// `STD_OUTPUT_HANDLE` is `-11` as a `u32`; a non-zero result is success.
        fn SetStdHandle(which: u32, handle: *mut std::ffi::c_void) -> i32;
        /// `HANDLE_FLAG_INHERIT` is 1; a non-zero result is success.
        fn SetHandleInformation(handle: *mut std::ffi::c_void, mask: u32, flags: u32) -> i32;
    }
    const HANDLE_FLAG_INHERIT: u32 = 1;
    const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;

    let stdout = io::stdout();
    let raw = stdout.as_raw_handle();
    // No standard output (a null or invalid handle), or one that cannot be copied: the
    // protocol goes where it always went, and children keep the real standard output.
    if raw.is_null() || raw as isize == -1 {
        return Box::new(io::stdout());
    }
    let Ok(own) = stdout.as_handle().try_clone_to_owned() else {
        return Box::new(io::stdout());
    };
    let err = io::stderr().as_raw_handle();
    if err.is_null() || err as isize == -1 {
        return Box::new(io::stdout());
    }
    // SAFETY: `SetStdHandle` only stores a handle value in the process; `err` is the live
    // standard error handle, which is not closed here.
    if unsafe { SetStdHandle(STD_OUTPUT_HANDLE, err) } == 0 {
        return Box::new(io::stdout());
    }
    // The handle the client gave is inheritable, and std starts children with every inheritable
    // handle: one that outlives the adapter would keep the client from ever seeing the end of
    // the stream. The protocol is written through the private copy, so the original (and the
    // input handle, tidily) are made non-inheritable.
    // SAFETY: both are the live standard handles of this process, which stay open.
    unsafe {
        SetHandleInformation(raw, HANDLE_FLAG_INHERIT, 0);
        let input = io::stdin().as_raw_handle();
        if !input.is_null() && input as isize != -1 {
            SetHandleInformation(input, HANDLE_FLAG_INHERIT, 0);
        }
    }
    Box::new(std::fs::File::from(own))
}

#[cfg(not(any(unix, windows)))]
fn protocol_output() -> Box<dyn io::Write + Send> {
    Box::new(io::stdout())
}

/// A writer that threads share: each `write` takes the lock.
struct SharedWrite(std::sync::Arc<std::sync::Mutex<Box<dyn io::Write + Send>>>);

impl io::Write for SharedWrite {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).flush()
    }
}

/// The `Vm` for the program a `launch` names: every capability, the program's standard output
/// and error as the adapter gives them, and no standard input (the client's input is the
/// protocol).
#[doc(hidden)]
pub fn build_program_vm(args: LaunchArgs) -> Result<colibri_runtime::Vm, String> {
    let mut cfg = colibri_runtime::BuildCfg::default();
    for name in &args.features {
        if let Some((message, hint)) = colibri_runtime::feature_name_problem(name) {
            return Err(format!("feature `{name}`: {message} ({hint})"));
        }
        cfg = cfg.with_feature(name);
    }
    cfg.debug = true;
    // The build configuration is set once for the process: a second launch with other features
    // would silently keep the first ones. The order of the features does not matter, nor does a
    // feature named twice.
    static FIRST: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    let mut features = args.features.clone();
    features.sort();
    features.dedup();
    if *FIRST.get_or_init(|| features.clone()) != features {
        return Err(format!(
            "the features of this session were fixed by the first launch ({}); start a new debug session to use others",
            FIRST.get().map_or(String::new(), |f| f.join(", "))
        ));
    }
    crate::set_build_cfg(cfg);
    let mut args = args;
    // What `colibri run` prints on standard error goes to the client as output.
    let notes = depscmd::prepare_quiet(&args.program, DepFlags::default())?;
    for note in notes {
        let _ = writeln!(args.stderr, "  {note}");
    }
    let program = args.program.clone();
    let config = colibri_std::StdConfig {
        args: args.args,
        assets: Some(crate::assets::for_entry(&program)),
        stdin: Some(Box::new(io::empty())),
        ..colibri_std::StdConfig::default()
    };
    // The workers of thread tasks print through the same writer as the program (the adapter turns
    // it into `output` events) and write to the same error stream; their standard input is empty.
    // Nothing they do reaches the protocol's own channels.
    let stderr = std::sync::Arc::new(std::sync::Mutex::new(args.stderr));
    let worker = colibri_std::WorkerStd::from_config(&config)
        .with_assets(move || crate::assets::for_entry(&program))
        .with_stderr({
            let stderr = std::sync::Arc::clone(&stderr);
            move || Box::new(SharedWrite(std::sync::Arc::clone(&stderr)))
        });
    let mut vm = crate::build_vm_with_worker_std(
        crate::vm_builder().allow_all().stdout_shared(args.stdout),
        config,
        worker,
        crate::bindings::installed(),
    );
    colibri_std::set_stderr(&mut vm, Box::new(SharedWrite(stderr)));
    Ok(vm)
}
