//! The adapter: requests in, responses and events out. See the crate documentation.

use std::cell::RefCell;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use colibri_runtime as colibri;
use colibri_runtime::debug::{
    BreakpointInfo, BreakpointSpec, DebugVar, DebugView, Debugger, FileId, HitCondition, Resume,
    StepKind, Stop,
};
use serde_json::{Map, Value, json};

use crate::frame::{ReadError, read_message, write_message};
use crate::path::{self, Seg};

/// The thread the program's main computation shows as. A coroutine `c` is thread `c + 2`.
const MAIN_THREAD: i64 = 1;

/// What the host needs to build the `Vm` of a `launch`.
pub struct LaunchArgs {
    /// The script or package folder to run.
    pub program: PathBuf,
    /// What the program's `os.args()` returns.
    pub args: Vec<String>,
    /// Feature flags for `@when` conditions.
    pub features: Vec<String>,
    /// Where `print` must write: the adapter turns it into `output` events.
    pub stdout: Box<dyn Write>,
    /// Where `io.eprint` must write.
    pub stderr: Box<dyn Write>,
}

/// Builds the `Vm` of a `launch`.
type Launcher = Box<dyn FnMut(LaunchArgs) -> Result<colibri::Vm, String>>;

/// One request of the client.
struct Request {
    seq: i64,
    command: String,
    args: Value,
}

enum Incoming {
    Request(Request),
    /// A message that could not be used; the session goes on.
    Malformed(String),
    /// The input cannot be followed any more; the session ends (`Closed` comes next).
    Broken(String),
    /// The input ended.
    Closed,
}

/// How long the VM thread has to take up an end of the session (`disconnect`, `terminate`, the
/// input closing) before the reader's watchdog ends the adapter itself. The VM thread answers
/// only at safepoints, so a program that is blocked in a native call (`time.sleep`) or idle
/// in the event loop does not.
const GRACE: Duration = Duration::from_secs(2);

/// How long, after the input has ended, the adapter waits for the program to finish ending
/// before it exits anyway. By then the first watchdog has been answered (the program was taken
/// up at a safepoint, so it ends its `defer`s without a limit), and a `defer` that blocks for
/// ever (a read that never returns) would otherwise leave the adapter running with nobody to
/// talk to. A `defer` that really takes longer than this after the client went away is cut.
const LONG_GRACE: Duration = Duration::from_secs(30);

/// [`GRACE`], or in a debug build what `COLIBRI_DEBUG_GRACE_MS` says. The variable is
/// internal: the tests set it to make the watchdog and the VM thread race for the same request.
/// A release build ignores it.
fn grace() -> Duration {
    internal_ms("COLIBRI_DEBUG_GRACE_MS").unwrap_or(GRACE)
}

/// [`LONG_GRACE`], or in a debug build what `COLIBRI_DEBUG_LONG_GRACE_MS` says (internal, for
/// the tests).
fn long_grace() -> Duration {
    internal_ms("COLIBRI_DEBUG_LONG_GRACE_MS").unwrap_or(LONG_GRACE)
}

/// A duration an internal environment variable sets, in a debug build only.
fn internal_ms(name: &str) -> Option<Duration> {
    if !cfg!(debug_assertions) {
        return None;
    }
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_millis)
}

/// Where an end of the session stands.
#[derive(Default)]
struct EndState {
    /// The VM thread has taken up an end (it answers, and ends the program): no watchdog is
    /// armed from now on, so a long `defer` is never cut.
    acked: bool,
    /// The watchdog is ending the process: the VM thread must not answer.
    exiting: bool,
}

/// Shared by the reader thread and the VM thread: how a session ends when the program does not
/// answer. It sets the pause flag (a running program stops, and `stopped` answers with
/// `Terminate`), and a watchdog ends the process if the VM thread has not taken up the request.
struct Ending {
    pause: Mutex<Option<colibri::debug::PauseHandle>>,
    state: Mutex<EndState>,
    /// Without a debugger nothing answers while the program runs: no grace is given then.
    no_debug: AtomicBool,
    /// The program is running (not between the steps of a session).
    running: AtomicBool,
    out: Arc<Mutex<Out>>,
}

impl Ending {
    /// The VM thread takes up the end of the session. False if the watchdog has it: the
    /// process is about to end and the caller must not write anything.
    fn ack(&self) -> bool {
        let mut state = self.state.lock().expect("the end state");
        if state.exiting {
            return false;
        }
        state.acked = true;
        true
    }

    /// [`Ending::ack`], and if the watchdog won, waits for the end of the process.
    fn ack_or_wait(&self) {
        if !self.ack() {
            loop {
                std::thread::park();
            }
        }
    }

    /// `reply`: the request to answer if the watchdog has to end the session.
    fn begin(self: &Arc<Self>, reply: Option<(i64, String)>) {
        if self.state.lock().expect("the end state").acked {
            return;
        }
        if let Some(handle) = self.pause.lock().expect("the pause handle").as_ref() {
            handle.pause();
        }
        let me = Arc::clone(self);
        std::thread::spawn(move || {
            let blind = me.no_debug.load(Ordering::Acquire) && me.running.load(Ordering::Acquire);
            if !blind {
                std::thread::sleep(grace());
            }
            me.end_session(reply, || std::process::exit(0));
        });
    }

    /// The input ended: besides [`Ending::begin`]'s watchdog, which only a program the VM thread
    /// has not taken up fears, a second one ends the process after [`long_grace`] whatever
    /// happens, so a `defer` that never returns cannot leave the adapter orphaned. When the
    /// session ends by itself the process exits first and this thread goes with it.
    fn begin_for_end_of_input(self: &Arc<Self>) {
        self.begin(None);
        let me = Arc::clone(self);
        std::thread::spawn(move || {
            std::thread::sleep(long_grace());
            me.out.lock().expect("the output").closed = true;
            std::process::exit(0);
        });
    }

    /// The watchdog's action, if the VM thread has not taken up the end: answers the request, says
    /// `terminated` for a `terminate`, closes the output, and calls `exit` while still holding
    /// the output (a frame the VM thread was writing is finished first, and none starts after).
    /// Returns false, doing nothing, when the VM thread has the end.
    fn end_session(&self, reply: Option<(i64, String)>, exit: impl FnOnce()) -> bool {
        {
            let mut state = self.state.lock().expect("the end state");
            if state.acked || state.exiting {
                return false;
            }
            state.exiting = true;
        }
        let mut out = self.out.lock().expect("the output");
        if let Some((seq, command)) = reply {
            let request = Request {
                seq,
                command: command.clone(),
                args: Value::Null,
            };
            out.respond(&request, Ok(Value::Null));
            if command == "terminate" {
                out.event("terminated", json!({}));
            }
        }
        out.closed = true;
        exit();
        true
    }
}

/// The reader thread: parses frames and hands them over.
fn read_loop(mut input: impl BufRead, tx: mpsc::Sender<Incoming>, ending: Arc<Ending>) {
    loop {
        let item = match read_message(&mut input) {
            Ok(value) => match request_of(&value) {
                Ok(request) => {
                    if request.command == "disconnect" || request.command == "terminate" {
                        ending.begin(Some((request.seq, request.command.clone())));
                    }
                    Incoming::Request(request)
                }
                Err(why) => Incoming::Malformed(why),
            },
            Err(ReadError::Malformed(why)) => Incoming::Malformed(why),
            Err(ReadError::Eof) => {
                ending.begin_for_end_of_input();
                let _ = tx.send(Incoming::Closed);
                return;
            }
            Err(ReadError::Fatal(why)) => {
                ending.begin_for_end_of_input();
                let _ = tx.send(Incoming::Broken(why));
                let _ = tx.send(Incoming::Closed);
                return;
            }
        };
        if tx.send(item).is_err() {
            return;
        }
    }
}

fn request_of(value: &Value) -> Result<Request, String> {
    let kind = value.get("type").and_then(Value::as_str);
    if kind != Some("request") {
        return Err(format!("a message of type {kind:?} is not a request"));
    }
    let seq = value
        .get("seq")
        .and_then(Value::as_i64)
        .ok_or("a request has no `seq`")?;
    let command = value
        .get("command")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("request {seq} has no `command`"))?;
    Ok(Request {
        seq,
        command: command.to_string(),
        args: value.get("arguments").cloned().unwrap_or(Value::Null),
    })
}

/// What goes to the client.
struct Out {
    writer: Box<dyn Write + Send>,
    seq: i64,
    /// The client is gone or has disconnected: nothing more is sent.
    closed: bool,
}

impl Out {
    fn send(&mut self, mut message: Value) {
        if self.closed {
            return;
        }
        self.seq += 1;
        message["seq"] = json!(self.seq);
        if write_message(&mut self.writer, &message).is_err() {
            self.closed = true;
        }
    }

    fn event(&mut self, name: &str, body: Value) {
        self.send(json!({"type": "event", "event": name, "body": body}));
    }

    fn output(&mut self, category: &str, text: &str) {
        self.event("output", json!({"category": category, "output": text}));
    }

    fn respond(&mut self, request: &Request, result: Result<Value, String>) {
        let mut message = json!({
            "type": "response",
            "request_seq": request.seq,
            "command": request.command,
        });
        match result {
            Ok(body) => {
                message["success"] = json!(true);
                if !body.is_null() {
                    message["body"] = body;
                }
            }
            Err(why) => {
                message["success"] = json!(false);
                message["message"] = json!(why);
            }
        }
        self.send(message);
    }
}

/// A writer that makes each write an `output` event.
struct EventWriter {
    out: Arc<Mutex<Out>>,
    category: &'static str,
}

impl Write for EventWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if !buf.is_empty() {
            let text = String::from_utf8_lossy(buf);
            self.out
                .lock()
                .expect("the output")
                .output(self.category, &text);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// How the adapter can reach the machine at this moment.
enum Ctl<'a, 'v> {
    /// Between launch and the first step of the program: there is no stopped fiber.
    Idle(&'a mut colibri::Vm),
    /// Inside the debugger's calls; `stopped` is whether the program is stopped (else it runs).
    Live {
        view: &'a mut DebugView<'v>,
        stopped: bool,
    },
}

/// The answer to a request, and what the program does next, if the request lets it go on.
struct Handled {
    result: Result<Value, String>,
    resume: Option<Resume>,
}

impl Handled {
    fn ok(body: Value) -> Handled {
        Handled {
            result: Ok(body),
            resume: None,
        }
    }
    fn err(why: impl Into<String>) -> Handled {
        Handled {
            result: Err(why.into()),
            resume: None,
        }
    }
}

struct State {
    out: Arc<Mutex<Out>>,
    ending: Arc<Ending>,
    /// Counts the stops: it is in the frame ids and variable references, so one from an earlier
    /// stop is refused instead of meaning something else.
    stop_serial: u32,
    /// `launch` had `noDebug`: the program runs without the debugger.
    no_debug: bool,
    rx: Receiver<Incoming>,
    /// `initialize` was answered.
    initialized: bool,
    launched: bool,
    lines_start_at_1: bool,
    columns_start_at_1: bool,
    /// The files of the program, by the name the loader gave them.
    files: Vec<(String, FileId)>,
    stop_on_entry: bool,
    /// `launch` had `watch`: while the program's coroutines run, changed sources are reloaded
    /// between ticks (as `colibri run --watch` does), and breakpoints stay where they were set.
    watch: bool,
    /// The stop on entry has been asked for and has not arrived.
    entry_pending: bool,
    /// The client asked to end the session (or went away): the program is being ended.
    end_requested: bool,
    /// The client disconnected: no more events.
    disconnected: bool,
    break_on_panic: bool,
    /// The frames `stackTrace` handed out in this stop: frame id minus 1 is the index.
    frames: Vec<(Option<u64>, usize)>,
    /// The host's `Vm` builder.
    launcher: Option<Launcher>,
    /// A `launch` got as far as building the program (it started, or failed to load).
    attempted: bool,
    program: PathBuf,
}

/// Runs a debug session on `input` and `output` until the client disconnects or the input ends.
/// `launcher` builds the `Vm` for the program a `launch` names (with the standard library, the
/// writers of [`LaunchArgs`], and every capability). Returns the adapter's own exit code
/// (`0`: the session ended as the client asked).
///
/// Call it on a thread with a big stack: the program runs on it.
pub fn serve(
    input: impl BufRead + Send + 'static,
    output: impl Write + Send + 'static,
    launcher: impl FnMut(LaunchArgs) -> Result<colibri::Vm, String> + 'static,
) -> i32 {
    let (tx, rx) = mpsc::channel();
    let out = Arc::new(Mutex::new(Out {
        writer: Box::new(output),
        seq: 0,
        closed: false,
    }));
    let ending = Arc::new(Ending {
        pause: Mutex::new(None),
        state: Mutex::new(EndState::default()),
        no_debug: AtomicBool::new(false),
        running: AtomicBool::new(false),
        out: Arc::clone(&out),
    });
    let reader_ending = Arc::clone(&ending);
    std::thread::spawn(move || read_loop(input, tx, reader_ending));
    let state = Rc::new(RefCell::new(State {
        out,
        ending,
        stop_serial: 0,
        no_debug: false,
        rx,
        initialized: false,
        launched: false,
        lines_start_at_1: true,
        columns_start_at_1: true,
        files: Vec::new(),
        stop_on_entry: false,
        watch: false,
        entry_pending: false,
        end_requested: false,
        disconnected: false,
        break_on_panic: false,
        frames: Vec::new(),
        launcher: Some(Box::new(launcher)),
        attempted: false,
        program: PathBuf::new(),
    }));
    run_session(&state);
    0
}

fn run_session(state: &Rc<RefCell<State>>) {
    // Before the program runs: initialize, launch, the configuration, configurationDone.
    let mut vm: Option<colibri::Vm> = None;
    loop {
        let incoming = state.borrow().rx.recv();
        let request = match incoming {
            Ok(Incoming::Request(r)) => r,
            Ok(Incoming::Malformed(why)) => {
                state.borrow().report_malformed(&why);
                continue;
            }
            Ok(Incoming::Broken(why)) => {
                state.borrow().report_broken(&why);
                continue;
            }
            Ok(Incoming::Closed) | Err(_) => return,
        };
        match request.command.as_str() {
            "disconnect" | "terminate" => {
                let st = state.borrow_mut();
                st.ending.ack_or_wait();
                st.reply(&request, Ok(Value::Null));
                if request.command == "disconnect" {
                    return;
                }
                st.out
                    .lock()
                    .expect("the output")
                    .event("terminated", json!({}));
                drop(st);
                return after_program(state);
            }
            "launch" => match launch(state, &request) {
                Ok(launched) => {
                    vm = Some(launched);
                    let st = state.borrow();
                    st.reply(&request, Ok(Value::Null));
                    st.out
                        .lock()
                        .expect("the output")
                        .event("initialized", json!({}));
                }
                // A request that could not be used: the client may try again.
                Err(Failed::Request(why)) => state.borrow().reply(&request, Err(why)),
                Err(Failed::Program(why)) => {
                    let st = state.borrow();
                    st.reply(&request, Err(why));
                    st.out
                        .lock()
                        .expect("the output")
                        .event("terminated", json!({}));
                    drop(st);
                    return after_program(state);
                }
            },
            "configurationDone" if vm.is_some() => {
                state.borrow().reply(&request, Ok(Value::Null));
                break;
            }
            _ => {
                let mut st = state.borrow_mut();
                let handled = match vm.as_mut() {
                    Some(vm) => st.handle(&request, &mut Ctl::Idle(vm)),
                    None => st.handle_before_launch(&request),
                };
                st.answer(&request, handled);
            }
        }
    }
    let Some(mut vm) = vm else { return };
    if state.borrow().stop_on_entry {
        state.borrow_mut().entry_pending = true;
        vm.debug_stop_on_entry();
    }
    // No borrow of the state is held while the program runs: the debugger calls back into it.
    let ending = Arc::clone(&state.borrow().ending);
    ending.running.store(true, Ordering::Release);
    let watch = state.borrow().watch;
    let outcome = run_program(state, &mut vm, watch);
    ending.running.store(false, Ordering::Release);
    let (code, disconnected) = {
        let mut st = state.borrow_mut();
        (st.finish(&mut vm, outcome), st.disconnected)
    };
    if disconnected {
        return;
    }
    {
        let st = state.borrow();
        let mut out = st.out.lock().expect("the output");
        if let Some(code) = code {
            out.event("exited", json!({"exitCode": code}));
        }
        out.event("terminated", json!({}));
    }
    after_program(state);
}

/// How often a watched program looks at its sources, in seconds (the longest the event loop
/// sleeps between ticks).
const WATCH_INTERVAL: f64 = 0.15;

/// `main`, then the event loop; with `watch`, the sources are looked at before every tick.
/// A patch (only function bodies changed) leaves the program running with the new bodies; a
/// restart runs `main` again, as `colibri run --watch` does. The breakpoints are the machine's
/// and follow their files (§34.8), so what the adapter has to refresh is its own list of files.
fn run_program(
    state: &Rc<RefCell<State>>,
    vm: &mut colibri::Vm,
    watch: bool,
) -> Result<(), colibri::Error> {
    vm.run_main()?;
    if !watch {
        return vm.run_event_loop();
    }
    vm.run_event_loop_watching(0.001, WATCH_INTERVAL, |vm| {
        let note = |text: &str| {
            let st = state.borrow();
            st.out
                .lock()
                .expect("the output")
                .output("console", &format!("{text}\n"));
        };
        match vm.reload_changed()? {
            colibri::ReloadOutcome::Unchanged => {}
            colibri::ReloadOutcome::Patched { functions } => {
                state.borrow_mut().refresh_files(vm);
                let s = if functions == 1 { "" } else { "s" };
                note(&format!("patched {functions} function{s}"));
            }
            colibri::ReloadOutcome::Restarted { reason, .. } => {
                state.borrow_mut().refresh_files(vm);
                note(&format!("restarted ({reason})"));
                vm.run_main()?;
            }
            colibri::ReloadOutcome::CheckFailed(report) => {
                let st = state.borrow();
                st.out
                    .lock()
                    .expect("the output")
                    .output("stderr", &report.render(false));
                drop(st);
                note("the new code does not check; keeping the old program");
            }
        }
        Ok(())
    })
}

/// The program is over (or never started): the client may still ask things, then disconnects.
fn after_program(state: &Rc<RefCell<State>>) {
    loop {
        let incoming = state.borrow().rx.recv();
        let request = match incoming {
            Ok(Incoming::Request(r)) => r,
            Ok(Incoming::Malformed(why)) => {
                state.borrow().report_malformed(&why);
                continue;
            }
            Ok(Incoming::Broken(why)) => {
                state.borrow().report_broken(&why);
                continue;
            }
            Ok(Incoming::Closed) | Err(_) => return,
        };
        let mut st = state.borrow_mut();
        match request.command.as_str() {
            "disconnect" => {
                st.ending.ack_or_wait();
                st.reply(&request, Ok(Value::Null));
                return;
            }
            "terminate" => {
                st.ending.ack_or_wait();
                st.reply(&request, Ok(Value::Null));
            }
            "threads" => st.reply(&request, Ok(json!({"threads": []}))),
            "initialize" => {
                let result = st.initialize(&request);
                st.reply(&request, result);
            }
            other => {
                let why = format!("`{other}` is not possible: the program has ended");
                st.reply(&request, Err(why));
            }
        }
    }
}

/// Why a `launch` failed.
enum Failed {
    /// The request is wrong; nothing was started.
    Request(String),
    /// The program could not be built or loaded: the session has no program.
    Program(String),
}

/// `launch`: builds the `Vm`, attaches the debugger and loads the program.
fn launch(state: &Rc<RefCell<State>>, request: &Request) -> Result<colibri::Vm, Failed> {
    let args = &request.args;
    let (launcher, launch_args) = {
        let mut st = state.borrow_mut();
        if !st.initialized {
            return Err(Failed::Request("`initialize` comes before `launch`".into()));
        }
        if st.attempted {
            return Err(Failed::Request("the program was launched already".into()));
        }
        let program = args
            .get("program")
            .and_then(Value::as_str)
            .filter(|p| !p.is_empty())
            .ok_or_else(|| {
                Failed::Request(
                    "`launch` needs `program`: the script or package folder to run".into(),
                )
            })?;
        let strings = |key: &str| -> Result<Vec<String>, Failed> {
            match args.get(key) {
                None | Some(Value::Null) => Ok(Vec::new()),
                Some(Value::Array(items)) => items
                    .iter()
                    .map(|v| {
                        v.as_str().map(str::to_string).ok_or_else(|| {
                            Failed::Request(format!("`{key}` must be a list of strings"))
                        })
                    })
                    .collect(),
                Some(_) => Err(Failed::Request(format!(
                    "`{key}` must be a list of strings"
                ))),
            }
        };
        let (program_args, features) = (strings("args")?, strings("features")?);
        if let Some(cwd) = args.get("cwd").and_then(Value::as_str)
            && !cwd.is_empty()
            && let Err(e) = std::env::set_current_dir(cwd)
        {
            return Err(Failed::Request(format!(
                "cannot use `{cwd}` as the working folder: {e}"
            )));
        }
        let launcher = st.launcher.take().expect("checked above");
        st.no_debug = args
            .get("noDebug")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        st.watch = args.get("watch").and_then(Value::as_bool).unwrap_or(false);
        st.stop_on_entry = !st.no_debug
            && args
                .get("stopOnEntry")
                .and_then(Value::as_bool)
                .unwrap_or(false);
        st.program = PathBuf::from(program);
        let writer = |category| {
            Box::new(EventWriter {
                out: Arc::clone(&st.out),
                category,
            }) as Box<dyn Write>
        };
        let launch_args = LaunchArgs {
            program: st.program.clone(),
            args: program_args,
            features,
            stdout: writer("stdout"),
            stderr: writer("stderr"),
        };
        (launcher, launch_args)
    };
    let program = launch_args.program.clone();
    let mut launcher = launcher;
    let built = launcher(launch_args);
    {
        let mut st = state.borrow_mut();
        st.launcher = Some(launcher);
        // Nothing has started yet: the client may try again.
        if built.is_ok() {
            st.attempted = true;
        }
    }
    let mut vm = built.map_err(Failed::Request)?;
    let no_debug = state.borrow().no_debug;
    if no_debug {
        // Nothing answers while the program runs: the reader ends the session at once.
        state
            .borrow()
            .ending
            .no_debug
            .store(true, Ordering::Release);
    } else {
        // From here on the debugger may call back (the program's top level runs in `load_path`).
        vm.set_debugger(Box::new(Adapter(Rc::clone(state))));
        let ending = Arc::clone(&state.borrow().ending);
        *ending.pause.lock().expect("the pause handle") = Some(vm.debug_pause_handle());
    }
    let mut warnings = EventWriter {
        out: Arc::clone(&state.borrow().out),
        category: "stderr",
    };
    vm.on_warnings(move |report| {
        let _ = warnings.write_all(report.render(false).as_bytes());
    });
    vm.load_path(&program)
        .map_err(|e| Failed::Program(e.render(false)))?;
    let mut st = state.borrow_mut();
    st.refresh_files(&vm);
    st.launched = true;
    Ok(vm)
}

/// The thread id of a computation.
fn thread_id(coroutine: Option<u64>) -> i64 {
    coroutine.map_or(MAIN_THREAD, |c| c as i64 + 2)
}

/// What a thread is called: `main`, or `go fetch (#12)` for the coroutine `12` that `go fetch(..)`
/// started (the number is the scheduler's, so it is the same at every stop).
fn thread_name(coroutine: Option<u64>, function: &str) -> String {
    match coroutine {
        None => "main".to_string(),
        Some(id) => format!("go {function} (#{id})"),
    }
}

fn coroutine_of(thread: i64) -> Option<u64> {
    (thread >= 2).then(|| (thread - 2) as u64)
}

/// The absolute form of a path, with no change to what it names.
fn absolute(path: &str) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| PathBuf::from(path))
}

/// Whether the client's `path` and the loader's file `name` are the same file.
fn same_file(name: &str, path: &str) -> bool {
    if name == path || absolute(name) == absolute(path) {
        return true;
    }
    matches!(
        (Path::new(name).canonicalize(), Path::new(path).canonicalize()),
        (Ok(a), Ok(b)) if a == b
    )
}

/// A hit condition the way a client writes it: `5`, `== 5`, `>= 5`, `> 5`, `% 5`.
fn parse_hit_condition(text: &str) -> Result<HitCondition, String> {
    let text = text.trim();
    let (op, digits) = match text.find(|c: char| c.is_ascii_digit()) {
        Some(at) => (text[..at].trim(), text[at..].trim()),
        None => (text, ""),
    };
    let n: u64 = digits
        .parse()
        .ok()
        .filter(|n| *n >= 1)
        .ok_or_else(|| format!("the hit count `{text}` needs a number of 1 or more"))?;
    match op {
        "" | "=" | "==" => Ok(HitCondition::Equals(n)),
        ">=" => Ok(HitCondition::AtLeast(n)),
        ">" => Ok(HitCondition::AtLeast(n + 1)),
        "%" => Ok(HitCondition::MultipleOf(n)),
        _ => Err(format!(
            "the hit count `{text}` is not supported (use N, == N, >= N, > N or % N)"
        )),
    }
}

/// Bits of a frame id or a variable reference that hold the position in the table.
const POSITION_BITS: u32 = 20;
const POSITION_MASK: i64 = (1 << POSITION_BITS) - 1;

impl<'v> Ctl<'_, 'v> {
    fn break_on_panic(&mut self, on: bool) {
        match self {
            Ctl::Idle(vm) => vm.debug_break_on_panic(on),
            Ctl::Live { view, .. } => view.break_on_panic(on),
        }
    }

    fn set_breakpoints(
        &mut self,
        name: &str,
        id: FileId,
        specs: &[BreakpointSpec],
    ) -> Vec<BreakpointInfo> {
        match self {
            Ctl::Idle(vm) => vm.debug_set_breakpoints(name, specs).unwrap_or_default(),
            Ctl::Live { view, .. } => view.set_breakpoints(id, specs),
        }
    }

    /// The view, when the program is stopped.
    fn stopped_view(&mut self) -> Result<&mut DebugView<'v>, String> {
        match self {
            Ctl::Idle(_) => Err("the program has not started".into()),
            Ctl::Live { stopped: false, .. } => Err("the program is running".into()),
            Ctl::Live { view, .. } => Ok(&mut **view),
        }
    }
}

impl State {
    fn reply(&self, request: &Request, result: Result<Value, String>) {
        self.out
            .lock()
            .expect("the output")
            .respond(request, result);
    }

    /// Sends the answer; returns how the program goes on when the request lets it.
    fn answer(&mut self, request: &Request, handled: Handled) -> Option<Resume> {
        let resume = handled.resume.filter(|_| handled.result.is_ok());
        self.reply(request, handled.result);
        if self.disconnected {
            self.out.lock().expect("the output").closed = true;
        }
        resume
    }

    fn report_malformed(&self, why: &str) {
        self.out.lock().expect("the output").output(
            "stderr",
            &format!("colibri debug: message ignored: {why}\n"),
        );
    }

    fn lines_to_client(&self, line: u32) -> u32 {
        if self.lines_start_at_1 {
            line
        } else {
            line.saturating_sub(1)
        }
    }

    fn report_broken(&self, why: &str) {
        self.out.lock().expect("the output").output(
            "stderr",
            &format!("colibri debug: the input cannot be followed ({why}); ending the session\n"),
        );
    }

    /// The part of an id that tells which stop it is from.
    fn stop_tag(&self) -> i64 {
        i64::from(self.stop_serial & 0x7ff) << POSITION_BITS
    }

    /// A frame id or variable reference for position `n` (1 or more) of this stop's table.
    fn tagged(&self, n: usize) -> i64 {
        if n as i64 > POSITION_MASK {
            0
        } else {
            self.stop_tag() | n as i64
        }
    }

    /// The position (1 or more) a tagged number holds, or why it is refused.
    fn untag(&self, id: i64, what: &str) -> Result<usize, String> {
        if id <= 0
            || id >> POSITION_BITS != self.stop_tag() >> POSITION_BITS
            || id & POSITION_MASK == 0
        {
            return Err(format!(
                "{what} {id} is not from this stop (references are good only until the program goes on)"
            ));
        }
        Ok((id & POSITION_MASK) as usize)
    }

    fn variable_json(&self, var: &DebugVar) -> Value {
        let mut v = json!({
            "name": var.name,
            "value": var.value,
            "type": var.ty,
            "variablesReference": var.handle.map_or(0, |h| self.tagged(h as usize + 1)),
        });
        if let Some(n) = var.indexed {
            v["indexedVariables"] = json!(n);
            v["namedVariables"] = json!(0);
        }
        v
    }

    fn lines_from_client(&self, line: i64) -> u32 {
        let line = if self.lines_start_at_1 {
            line
        } else {
            line + 1
        };
        u32::try_from(line.max(0)).unwrap_or(0)
    }

    fn initialize(&mut self, request: &Request) -> Result<Value, String> {
        let flag = |key: &str| request.args.get(key).and_then(Value::as_bool);
        self.lines_start_at_1 = flag("linesStartAt1").unwrap_or(true);
        self.columns_start_at_1 = flag("columnsStartAt1").unwrap_or(true);
        self.initialized = true;
        Ok(json!({
            "supportsConfigurationDoneRequest": true,
            "supportsHitConditionalBreakpoints": true,
            "supportsLogPoints": true,
            "supportsConditionalBreakpoints": false,
            "supportsEvaluateForHovers": true,
            "supportsTerminateRequest": true,
            "supportsSetVariable": false,
            "supportsStepBack": false,
            "supportsRestartRequest": false,
            "supportsExceptionInfoRequest": false,
            "exceptionBreakpointFilters": [{
                "filter": "panic",
                "label": "Panics",
                "description": "Stop at every panic, before anything unwinds (also one that \
                                `join_result` turns into an `err` later)",
                "default": false,
            }],
        }))
    }

    /// The requests that make sense before there is a program.
    fn handle_before_launch(&mut self, request: &Request) -> Handled {
        match request.command.as_str() {
            "initialize" => Handled {
                result: self.initialize(request),
                resume: None,
            },
            "launch" => Handled::err("the program was launched already"),
            other => Handled::err(format!("`{other}` needs a `launch` first")),
        }
    }

    fn handle(&mut self, request: &Request, ctl: &mut Ctl<'_, '_>) -> Handled {
        let args = &request.args;
        match request.command.as_str() {
            "initialize" => Handled {
                result: self.initialize(request),
                resume: None,
            },
            "launch" => Handled::err("the program was launched already"),
            "configurationDone" => Handled::ok(Value::Null),
            "setBreakpoints" if self.no_debug => {
                let entries = args
                    .get("breakpoints")
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len);
                let refused: Vec<Value> = (0..entries)
                    .map(|_| {
                        json!({
                            "verified": false,
                            "message": "the program runs without debugging (noDebug)",
                        })
                    })
                    .collect();
                Handled::ok(json!({"breakpoints": refused}))
            }
            "setBreakpoints" => self.set_breakpoints(args, ctl),
            "setExceptionBreakpoints" => {
                let listed = |key: &str| {
                    args.get(key).and_then(Value::as_array).is_some_and(|l| {
                        l.iter().any(|f| {
                            f.as_str() == Some("panic")
                                || f.get("filterId").and_then(Value::as_str) == Some("panic")
                        })
                    })
                };
                self.break_on_panic = listed("filters") || listed("filterOptions");
                ctl.break_on_panic(self.break_on_panic);
                Handled::ok(json!({}))
            }
            "threads" => Handled::ok(json!({"threads": self.threads(ctl)})),
            "stackTrace" => self.stack_trace(args, ctl),
            "scopes" => self.scopes(args, ctl),
            "variables" => self.variables(args, ctl),
            "evaluate" => self.evaluate(args, ctl),
            "continue" | "next" | "stepIn" | "stepOut" => {
                let resume = match request.command.as_str() {
                    "continue" => Resume::Continue,
                    "next" => Resume::Step(StepKind::Over),
                    "stepIn" => Resume::Step(StepKind::In),
                    _ => Resume::Step(StepKind::Out),
                };
                match ctl.stopped_view() {
                    Ok(_) => Handled {
                        result: Ok(if request.command == "continue" {
                            json!({"allThreadsContinued": true})
                        } else {
                            Value::Null
                        }),
                        resume: Some(resume),
                    },
                    Err(why) => Handled::err(format!("`{}`: {why}", request.command)),
                }
            }
            "pause" => match ctl {
                Ctl::Idle(_) => Handled::err("the program has not started"),
                Ctl::Live { stopped: true, .. } => Handled::ok(Value::Null),
                Ctl::Live { view, .. } => {
                    view.pause_handle().pause();
                    Handled::ok(Value::Null)
                }
            },
            "terminate" | "disconnect" => {
                self.ending.ack_or_wait();
                self.end_requested = true;
                self.disconnected = request.command == "disconnect";
                match ctl {
                    Ctl::Idle(_) => {}
                    Ctl::Live { view, stopped } => {
                        // A running program ends at its next safepoint, as a pause that
                        // `stopped` answers with `Terminate`.
                        if !*stopped {
                            view.pause_handle().pause();
                        }
                    }
                }
                Handled {
                    result: Ok(Value::Null),
                    resume: Some(Resume::Terminate),
                }
            }
            other => Handled::err(format!("`{other}` is not supported by this adapter")),
        }
    }

    fn threads(&self, ctl: &mut Ctl<'_, '_>) -> Vec<Value> {
        let main = json!({"id": MAIN_THREAD, "name": "main"});
        match ctl {
            Ctl::Idle(_) => vec![main],
            Ctl::Live { view, .. } => {
                let fibers = view.fibers();
                if fibers.is_empty() {
                    return vec![main];
                }
                fibers
                    .iter()
                    .map(|f| {
                        json!({"id": thread_id(f.coroutine), "name": thread_name(f.coroutine, &f.name)})
                    })
                    .collect()
            }
        }
    }

    fn set_breakpoints(&mut self, args: &Value, ctl: &mut Ctl<'_, '_>) -> Handled {
        let Some(path) = args
            .pointer("/source/path")
            .and_then(Value::as_str)
            .map(str::to_string)
        else {
            return Handled::err("`setBreakpoints` needs `source.path`");
        };
        let asked: Vec<&Value> = match args.get("breakpoints").and_then(Value::as_array) {
            Some(list) => list.iter().collect(),
            // The older form: just the lines.
            None => args
                .get("lines")
                .and_then(Value::as_array)
                .map(|l| l.iter().collect())
                .unwrap_or_default(),
        };
        // One slot per request entry: a refusal now, or the spec for the machine.
        let mut slots: Vec<Result<BreakpointSpec, (u32, String)>> = Vec::new();
        for entry in asked {
            let line_of = |v: &Value| v.as_i64().map(|l| self.lines_from_client(l));
            let line = match entry {
                Value::Number(_) => line_of(entry),
                _ => entry.get("line").and_then(line_of),
            }
            .unwrap_or(0);
            let text = |key: &str| {
                entry
                    .get(key)
                    .and_then(Value::as_str)
                    .filter(|t| !t.trim().is_empty())
            };
            if text("condition").is_some() {
                slots.push(Err((
                    line,
                    "conditions are not supported (only a hit count)".into(),
                )));
                continue;
            }
            let hit = match text("hitCondition").map(parse_hit_condition) {
                None => None,
                Some(Ok(h)) => Some(h),
                Some(Err(why)) => {
                    slots.push(Err((line, why)));
                    continue;
                }
            };
            slots.push(Ok(BreakpointSpec {
                line,
                hit,
                log: text("logMessage").map(str::to_string),
            }));
        }
        let specs: Vec<BreakpointSpec> = slots.iter().filter_map(|s| s.clone().ok()).collect();
        let file = self.files.iter().find(|(name, _)| same_file(name, &path));
        let infos = match file {
            Some((name, id)) => ctl.set_breakpoints(&name.clone(), *id, &specs),
            None => Vec::new(),
        };
        let mut infos = infos.into_iter();
        let breakpoints: Vec<Value> = slots
            .iter()
            .map(|slot| match slot {
                Err((line, why)) => json!({
                    "verified": false,
                    "line": self.lines_to_client(*line),
                    "message": why,
                }),
                Ok(spec) => match (file.is_some(), infos.next()) {
                    (true, Some(info)) => {
                        let mut b = json!({
                            "id": info.id,
                            "verified": info.verified,
                            "line": self.lines_to_client(info.line),
                        });
                        if !info.verified {
                            b["message"] = json!(
                                "no statement starts on this line (or its function is not compiled yet)"
                            );
                        }
                        b
                    }
                    _ => json!({
                        "verified": false,
                        "line": self.lines_to_client(spec.line),
                        "message": "this file is not part of the program",
                    }),
                },
            })
            .collect();
        Handled::ok(json!({"breakpoints": breakpoints}))
    }

    fn stack_trace(&mut self, args: &Value, ctl: &mut Ctl<'_, '_>) -> Handled {
        let view = match ctl.stopped_view() {
            Ok(v) => v,
            Err(why) => return Handled::err(format!("`stackTrace`: {why}")),
        };
        let thread = args
            .get("threadId")
            .and_then(Value::as_i64)
            .unwrap_or(MAIN_THREAD);
        let coroutine = coroutine_of(thread);
        let frames = view.frames(coroutine);
        let start = args.get("startFrame").and_then(Value::as_u64).unwrap_or(0) as usize;
        let levels = args.get("levels").and_then(Value::as_u64).unwrap_or(0) as usize;
        let take = if levels == 0 { usize::MAX } else { levels };
        let mut shown = Vec::new();
        for frame in frames.iter().skip(start).take(take) {
            self.frames.push((coroutine, frame.index));
            let id = self.tagged(self.frames.len());
            let mut entry = json!({
                "id": id,
                "name": frame.name,
                "line": self.lines_to_client(frame.line.unwrap_or(1)),
                "column": u32::from(self.columns_start_at_1),
            });
            if let (Some(file), false) = (&frame.file, frame.native) {
                let path = absolute(file);
                entry["source"] = json!({
                    "name": path.file_name().map_or_else(|| file.clone(), |n| n.to_string_lossy().into_owned()),
                    "path": path.to_string_lossy(),
                });
            } else {
                entry["presentationHint"] = json!("subtle");
                entry["line"] = json!(0);
            }
            shown.push(entry);
        }
        Handled::ok(json!({"stackFrames": shown, "totalFrames": frames.len()}))
    }

    fn frame_of(&self, args: &Value) -> Result<Option<(Option<u64>, usize)>, String> {
        match args.get("frameId").and_then(Value::as_i64) {
            None => Ok(None),
            Some(id) => {
                let position = self.untag(id, "frame")?;
                self.frames
                    .get(position - 1)
                    .copied()
                    .map(Some)
                    .ok_or_else(|| format!("frame {id} is not on the stack any more"))
            }
        }
    }

    fn scopes(&mut self, args: &Value, ctl: &mut Ctl<'_, '_>) -> Handled {
        let frame = match self.frame_of(args) {
            Ok(Some(f)) => f,
            Ok(None) => return Handled::err("`scopes` needs `frameId`"),
            Err(why) => return Handled::err(why),
        };
        let view = match ctl.stopped_view() {
            Ok(v) => v,
            Err(why) => return Handled::err(format!("`scopes`: {why}")),
        };
        let scopes: Vec<Value> = view
            .scopes(frame.0, frame.1)
            .iter()
            .map(|s| {
                json!({
                    "name": s.name,
                    "variablesReference": self.tagged(s.handle as usize + 1),
                    "expensive": s.name != "Locals",
                })
            })
            .collect();
        Handled::ok(json!({"scopes": scopes}))
    }

    fn variables(&mut self, args: &Value, ctl: &mut Ctl<'_, '_>) -> Handled {
        let view = match ctl.stopped_view() {
            Ok(v) => v,
            Err(why) => return Handled::err(format!("`variables`: {why}")),
        };
        let reference = args
            .get("variablesReference")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let handle = match self.untag(reference, "variablesReference") {
            Ok(position) => position as u32 - 1,
            Err(why) => return Handled::err(why),
        };
        let start = args.get("start").and_then(Value::as_u64).unwrap_or(0) as usize;
        let count = match args.get("count").and_then(Value::as_u64) {
            None | Some(0) => usize::MAX,
            Some(n) => n as usize,
        };
        let vars: Vec<Value> = view
            .children(handle, start, count)
            .iter()
            .map(|v| self.variable_json(v))
            .collect();
        Handled::ok(json!({"variables": vars}))
    }

    fn evaluate(&mut self, args: &Value, ctl: &mut Ctl<'_, '_>) -> Handled {
        let Some(expression) = args.get("expression").and_then(Value::as_str) else {
            return Handled::err("`evaluate` needs `expression`");
        };
        let segs = match path::parse(expression) {
            Ok(s) => s,
            Err(why) => return Handled::err(why),
        };
        let frame = match self.frame_of(args) {
            Ok(f) => f,
            Err(why) => return Handled::err(why),
        };
        let view = match ctl.stopped_view() {
            Ok(v) => v,
            Err(why) => return Handled::err(format!("`evaluate`: {why}")),
        };
        // Without a frame, the innermost frame of the thread that stopped.
        let (coroutine, index) = match frame {
            Some(f) => f,
            None => match view.frames(view.stopped_coroutine()).first() {
                Some(top) => (view.stopped_coroutine(), top.index),
                None => return Handled::err("there is no frame to evaluate in"),
            },
        };
        let scopes = view.scopes(coroutine, index);
        let mut segs = segs.into_iter();
        let Some(Seg::Name(root)) = segs.next() else {
            return Handled::err("a path starts with a name");
        };
        let found = scopes.iter().find_map(|scope| {
            view.children(scope.handle, 0, usize::MAX)
                .into_iter()
                .find(|v| v.name == root)
        });
        let Some(mut current) = found else {
            return Handled::err(format!("`{root}` is not defined here"));
        };
        for seg in segs {
            let Some(handle) = current.handle else {
                return Handled::err(format!("`{}` has no parts", current.name));
            };
            let wanted = match &seg {
                Seg::Name(n) => n.clone(),
                Seg::Index(i) => i.clone(),
            };
            let mut next = None;
            if let Seg::Index(i) = &seg
                && let Ok(n) = i.parse::<usize>()
            {
                // A list position needs only that one.
                next = view
                    .children(handle, n, 1)
                    .into_iter()
                    .find(|v| v.name == format!("[{n}]"));
            }
            if next.is_none() {
                let listed = format!("[{wanted}]");
                next = view.children(handle, 0, usize::MAX).into_iter().find(|v| {
                    v.name == wanted || (matches!(seg, Seg::Index(_)) && v.name == listed)
                });
            }
            match next {
                Some(v) => current = v,
                None => {
                    return Handled::err(format!("`{}` has no `{wanted}`", current.name));
                }
            }
        }
        Handled::ok(json!({
            "result": current.value,
            "type": current.ty,
            "variablesReference": current.handle.map_or(0, |h| self.tagged(h as usize + 1)),
        }))
    }

    /// The program ended with `outcome`: reports what it left on stderr and returns the exit code
    /// (`None` when the debugger ended it).
    fn finish(&mut self, vm: &mut colibri::Vm, outcome: Result<(), colibri::Error>) -> Option<i32> {
        let mut out = self.out.lock().expect("the output");
        match outcome {
            Ok(()) => Some(0),
            Err(colibri::Error::Panic { .. }) if colibri_std::exit_code(vm).is_some() => {
                colibri_std::exit_code(vm)
            }
            Err(colibri::Error::Panic { .. }) if self.end_requested => None,
            Err(e @ colibri::Error::Panic { .. }) => {
                out.output("stderr", &e.render(false));
                Some(101)
            }
            Err(e) => {
                out.output("stderr", &e.render(false));
                Some(1)
            }
        }
    }
}

/// The debugger the machine calls.
struct Adapter(Rc<RefCell<State>>);

impl Debugger for Adapter {
    fn stopped(&mut self, view: &mut DebugView<'_>, stop: &Stop) -> Resume {
        let mut st = self.0.borrow_mut();
        if st.end_requested {
            return Resume::Terminate;
        }
        st.frames.clear();
        st.stop_serial = st.stop_serial.wrapping_add(1);
        let thread = thread_id(view.stopped_coroutine());
        let entry = std::mem::take(&mut st.entry_pending);
        let mut body = Map::new();
        match stop {
            Stop::Breakpoint(ids) => {
                body.insert("reason".into(), json!("breakpoint"));
                body.insert("hitBreakpointIds".into(), json!(ids));
            }
            Stop::Step if entry => {
                body.insert("reason".into(), json!("entry"));
            }
            Stop::Step => {
                body.insert("reason".into(), json!("step"));
            }
            Stop::Pause => {
                body.insert("reason".into(), json!("pause"));
            }
            Stop::Panic(message) => {
                body.insert("reason".into(), json!("exception"));
                body.insert("description".into(), json!("Paused on panic"));
                body.insert("text".into(), json!(message));
            }
        }
        body.insert("threadId".into(), json!(thread));
        body.insert("allThreadsStopped".into(), json!(true));
        st.out
            .lock()
            .expect("the output")
            .event("stopped", Value::Object(body));
        loop {
            let incoming = st.rx.recv();
            match incoming {
                Ok(Incoming::Request(request)) => {
                    let handled = st.handle(
                        &request,
                        &mut Ctl::Live {
                            view,
                            stopped: true,
                        },
                    );
                    let resume = st.answer(&request, handled);
                    if let Some(resume) = resume {
                        st.frames.clear();
                        return resume;
                    }
                }
                Ok(Incoming::Malformed(why)) => st.report_malformed(&why),
                Ok(Incoming::Broken(why)) => st.report_broken(&why),
                Ok(Incoming::Closed) | Err(_) => {
                    st.go_away();
                    return Resume::Terminate;
                }
            }
        }
    }

    fn poll(&mut self, view: &mut DebugView<'_>) {
        let mut st = self.0.borrow_mut();
        loop {
            match st.rx.try_recv() {
                Ok(Incoming::Request(request)) => {
                    let handled = st.handle(
                        &request,
                        &mut Ctl::Live {
                            view,
                            stopped: false,
                        },
                    );
                    st.answer(&request, handled);
                }
                Ok(Incoming::Malformed(why)) => st.report_malformed(&why),
                Ok(Incoming::Broken(why)) => st.report_broken(&why),
                Err(TryRecvError::Empty) => return,
                Ok(Incoming::Closed) | Err(TryRecvError::Disconnected) => {
                    st.go_away();
                    view.pause_handle().pause();
                    return;
                }
            }
        }
    }

    fn breakpoints_verified(&mut self, infos: &[BreakpointInfo]) {
        let st = self.0.borrow();
        let mut out = st.out.lock().expect("the output");
        for info in infos {
            out.event(
                "breakpoint",
                json!({
                    "reason": "changed",
                    "breakpoint": {
                        "id": info.id,
                        "verified": true,
                        "line": st.lines_to_client(info.line),
                    },
                }),
            );
        }
    }

    fn output(&mut self, text: &str) {
        let st = self.0.borrow();
        let mut text = text.to_string();
        if !text.ends_with('\n') {
            text.push('\n');
        }
        st.out.lock().expect("the output").output("console", &text);
    }
}

impl State {
    /// The files of the program as the loader named them (after a load or a reload).
    fn refresh_files(&mut self, vm: &colibri::Vm) {
        let sources = vm.sources();
        self.files = (0..sources.next_file_id().0)
            .map(|n| (sources.name(FileId(n)).to_string(), FileId(n)))
            .collect();
    }

    /// The client went away: the program is ended, and nothing more is sent.
    fn go_away(&mut self) {
        self.ending.ack_or_wait();
        self.end_requested = true;
        self.disconnected = true;
        self.out.lock().expect("the output").closed = true;
    }
}

#[cfg(test)]
mod ending_tests {
    use super::*;

    /// A writer whose bytes the test can read.
    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);

    impl Write for Sink {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn ending() -> (Arc<Ending>, Sink) {
        let sink = Sink::default();
        let out = Arc::new(Mutex::new(Out {
            writer: Box::new(sink.clone()),
            seq: 0,
            closed: false,
        }));
        let ending = Arc::new(Ending {
            pause: Mutex::new(None),
            state: Mutex::new(EndState::default()),
            no_debug: AtomicBool::new(false),
            running: AtomicBool::new(false),
            out,
        });
        (ending, sink)
    }

    /// The messages written so far, each read whole.
    fn messages(sink: &Sink) -> Vec<Value> {
        let bytes = sink.0.lock().unwrap().clone();
        let mut input = io::Cursor::new(bytes);
        let mut out = Vec::new();
        while let Ok(m) = read_message(&mut input) {
            out.push(m);
        }
        assert_eq!(
            input.position() as usize,
            input.get_ref().len(),
            "a torn frame"
        );
        out
    }

    #[test]
    fn when_the_watchdog_has_the_end_the_vm_thread_is_told_so_and_one_answer_is_written() {
        let (ending, sink) = ending();
        let mut held = false;
        let fired = ending.end_session(Some((7, "disconnect".into())), || {
            // The output is still held when the process would end.
            held = ending.out.try_lock().is_err();
        });
        assert!(fired && held);
        assert!(!ending.ack(), "the VM thread must not answer");
        assert!(!ending.end_session(Some((7, "disconnect".into())), || {}));
        let written = messages(&sink);
        assert_eq!(written.len(), 1);
        assert_eq!(written[0]["request_seq"], 7);
        assert!(ending.out.lock().unwrap().closed);
    }

    #[test]
    fn when_the_vm_thread_has_the_end_the_watchdog_does_nothing() {
        let (ending, sink) = ending();
        assert!(ending.ack());
        assert!(!ending.end_session(Some((1, "terminate".into())), || panic!("exit")));
        assert!(messages(&sink).is_empty());
        assert!(!ending.out.lock().unwrap().closed);
    }

    #[test]
    fn a_terminate_the_watchdog_answers_is_followed_by_terminated() {
        let (ending, sink) = ending();
        assert!(ending.end_session(Some((2, "terminate".into())), || {}));
        let written = messages(&sink);
        assert_eq!(written.len(), 2);
        assert_eq!(written[0]["type"], "response");
        assert_eq!(written[1]["event"], "terminated");
    }
}
