//! `process`: running other programs. Everything needs the `os` capability.
//!
//! `run` waits for the program and collects its output (on a background thread: inside a
//! coroutine only that coroutine waits, and cancelling it kills the program); `spawn` starts it
//! and gives back a `Child` a script can poll with `try_wait` (and `wait(0.01)` between polls, so other
//! coroutines keep running). A program that cannot be started is an `err`; one that runs and
//! exits with a non-zero status is an `Output` with that `status`.
//!
//! `shell(line)` hands the line to the system's shell: `sh -c` on Unix, `cmd /D /S /C "line"` on
//! Windows (so `%VAR%` and `&` there, `$VAR` and `;` elsewhere: a script that must run on both
//! starts its programs with `run`, whose arguments no shell reads). On Windows the line is
//! passed to `cmd` as it is, not as an ordinary argument (Rust would write a `"` in it as `\"`,
//! which `cmd` does not understand): `cmd` strips the first and the last quote of the command
//! line and runs the rest verbatim, and `/D` keeps AutoRun commands out. A `cwd` option that names
//! no folder is refused before anything starts, with the same message on every system. The
//! text a program writes is passed on unchanged, so what `cmd` writes ends its lines with
//! `\r\n`.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child as OsChild, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use colibri_runtime::{CancelToken, Cap, Ctx, Error, HostType, TypeBuilder, TypedError, Vm};

use crate::config::{EnvSource, State};
use crate::errors::error_kind;
use crate::fs::reason;
use crate::needs;
use crate::values::value_struct;

error_kind!(
    "process",
    /// Why a program could not be run (`process.Error.NotFound`). A program that runs and
    /// exits with a non-zero status is an `Output`, except for `process.output`, which turns
    /// that into `Failed`.
    Kind {
        NotFound,
        PermissionDenied,
        Failed,
        Cancelled,
        /// The working folder (`options.cwd`) is missing or is not a folder.
        InvalidDirectory,
        Other,
    }
);

/// The variant of `process.Error` an I/O error is.
fn kind_of(e: &std::io::Error) -> Kind {
    match e.kind() {
        std::io::ErrorKind::NotFound => Kind::NotFound,
        std::io::ErrorKind::PermissionDenied => Kind::PermissionDenied,
        _ => Kind::Other,
    }
}

/// What a finished program left behind. `status` is its exit code; a program ended by a signal
/// has `-N` for signal N (Unix), and `-1` when the system says nothing more.
#[derive(Clone, Debug)]
pub(crate) struct Output {
    pub status: i64,
    pub stdout: String,
    pub stderr: String,
}

value_struct!(Output {
    status: i64,
    stdout: String,
    stderr: String,
});

/// How to start a program beyond its name and arguments.
#[derive(Clone, Debug, Default)]
pub(crate) struct Options {
    pub cwd: Option<String>,
    pub env: BTreeMap<String, String>,
    pub input: Option<String>,
}

value_struct!(Options {
    cwd: Option<String>,
    env: BTreeMap<String, String>,
    input: Option<String>,
});

fn status_code(status: ExitStatus) -> i64 {
    if let Some(code) = status.code() {
        return i64::from(code);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return -i64::from(signal);
        }
    }
    -1
}

fn text(bytes: Vec<u8>) -> String {
    String::from_utf8_lossy(&bytes).into_owned()
}

/// The command, with the environment of this program: the process's own (or the fixed one the
/// `Vm` was built with, and nothing of the host's) plus whatever
/// `os.set_env` changed, then the options' own variables on top.
///
/// `raw` is text appended to the command line exactly as it is, after the arguments, with no
/// quoting; only `shell` uses it, and only on Windows (elsewhere it is always `None`).
fn command(
    cx: &mut Ctx,
    program: &str,
    args: &[String],
    raw: Option<&str>,
    opts: &Options,
    piped: bool,
) -> Command {
    let mut cmd = Command::new(program);
    cmd.args(args);
    #[cfg(windows)]
    if let Some(raw) = raw {
        use std::os::windows::process::CommandExt;
        cmd.raw_arg(raw);
    }
    #[cfg(not(windows))]
    let _ = raw;
    if let Some(cwd) = &opts.cwd {
        cmd.current_dir(cwd);
    }
    if let Some(state) = cx.state_mut::<State>() {
        // The same view of the environment as `os.env` and `os.vars`: a `Vm` built with a fixed
        // environment is hermetic, so its children must not inherit the host's variables.
        if let EnvSource::Fixed(vars) = &state.env {
            cmd.env_clear();
            cmd.envs(vars);
        }
        cmd.envs(&state.env_overrides);
    }
    cmd.envs(&opts.env);
    cmd.stdin(if opts.input.is_some() {
        Stdio::piped()
    } else {
        // Never the script's own standard input: a program that reads it would steal it.
        Stdio::null()
    });
    if piped {
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    }
    cmd
}

/// Looks at the folder the program is to run in before starting it, so the message is the
/// same on every system (Unix says the program is missing, Windows says "not a directory").
fn check_folder(program: &str, opts: &Options) -> Result<(), TypedError> {
    let Some(cwd) = &opts.cwd else {
        return Ok(());
    };
    let path = Path::new(cwd);
    if path.is_dir() {
        Ok(())
    } else if path.exists() {
        Err(Kind::InvalidDirectory
            .error(format!("cannot run `{program}`: `{cwd}` is not a folder")))
    } else {
        Err(Kind::InvalidDirectory.error(format!(
            "cannot run `{program}`: the folder `{cwd}` does not exist"
        )))
    }
}

fn start_error(program: &str, e: &std::io::Error) -> TypedError {
    kind_of(e).error(format!("cannot run `{program}`: {}", reason(e)))
}

/// Writes the input on its own thread (a program may write a lot before it reads) and closes
/// the pipe.
fn feed(stdin: Option<std::process::ChildStdin>, input: Option<String>) -> Option<JoinHandle<()>> {
    let (mut stdin, input) = (stdin?, input?);
    Some(std::thread::spawn(move || {
        // A program that stops reading early is not an error.
        let _ = stdin.write_all(input.as_bytes());
    }))
}

fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut bytes);
        }
        bytes
    })
}

/// A program a background operation waits for. The operation and whoever cancels it (the
/// waiting coroutine, or the `Vm` ending) share it, so the canceller can kill the program
/// itself instead of leaving it to the worker, which may not look for a while (or ever, when
/// the process is about to exit).
type Shared = Arc<Mutex<OsChild>>;

fn lock(child: &Shared) -> std::sync::MutexGuard<'_, OsChild> {
    child
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Starts the program as a shared child that `token` kills, and reaps, when it is cancelled.
/// This is the same for `run`, `run_with`, `shell` and `status`: the kill reaches the program
/// we started and nothing it started in turn (as in Python and Go). It is not a process
/// group, because Ctrl-C from the terminal must still reach the children. So
/// `shell("sleep 30")` kills `/bin/sh`, and where `sh` forks (dash does) the `sleep` lives
/// on; a program that starts helpers has to stop them itself. The threads that read the
/// killed program's output (`drain`) end when its pipes close, which is when the last process
/// holding them ends, not when the program is killed.
///
/// The spawn and the registration of the kill hook are one step as far as `cancel` is
/// concerned ([`CancelToken::start`]): a `Vm` that ends (and so cancels) at any moment either
/// stops the program from starting or finds the hook, never leaves it running unnoticed.
fn spawn_shared(
    cmd: &mut Command,
    program: &str,
    token: &CancelToken,
) -> Result<Shared, TypedError> {
    let started = token.start(|| {
        let child = cmd.spawn()?;
        let child = Arc::new(Mutex::new(child));
        let victim = Arc::clone(&child);
        let hook: Box<dyn FnOnce() + Send> = Box::new(move || {
            let mut child = lock(&victim);
            // Both fail when it has already ended and been collected, which is fine.
            let _ = child.kill();
            let _ = child.wait();
        });
        Ok::<_, std::io::Error>((child, hook))
    });
    match started {
        Ok(Some(child)) => Ok(child),
        Ok(None) => Err(Kind::Cancelled.error(format!(
            "`{program}` was not started: its caller was cancelled"
        ))),
        Err(e) => Err(start_error(program, &e)),
    }
}

/// Blocks until the process has ended, without collecting its status (so a kill by the other
/// side stays safe: the process number is not reused while it is not collected). Where this
/// is not available the caller polls.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn block_until_ended(pid: u32) {
    // SAFETY: `info` is a plain out-parameter that is only written; `pid` is our child.
    unsafe {
        let mut info: libc::siginfo_t = std::mem::zeroed();
        loop {
            let r = libc::waitid(
                libc::P_PID,
                libc::id_t::from(pid),
                &mut info,
                libc::WEXITED | libc::WNOWAIT,
            );
            // Interrupted: again. Anything else (it was collected by the canceller already):
            // the caller finds out with `try_wait`.
            if r == 0 || std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                return;
            }
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn block_until_ended(_pid: u32) {}

/// Waits for `child`. If `token` is cancelled the child has been killed (by its hook) and
/// this gives up.
fn wait_for(child: &Shared, program: &str, token: &CancelToken) -> Result<ExitStatus, TypedError> {
    let pid = lock(child).id();
    // Cancelled before the kill hook was registered: the hook ran at once and has collected
    // the child, so its pid may be reused and must not be waited for.
    if token.is_cancelled() {
        return Err(
            Kind::Cancelled.error(format!("`{program}` was stopped: its caller was cancelled"))
        );
    }
    block_until_ended(pid);
    // Where `block_until_ended` does nothing, short naps at first so that a quick program
    // costs almost nothing, then longer ones.
    let mut nap = Duration::from_micros(100);
    loop {
        if token.is_cancelled() {
            return Err(
                Kind::Cancelled.error(format!("`{program}` was stopped: its caller was cancelled"))
            );
        }
        match lock(child).try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => {}
            Err(e) => {
                return Err(
                    Kind::Other.error(format!("cannot wait for `{program}`: {}", reason(&e)))
                );
            }
        }
        std::thread::sleep(nap);
        nap = (nap * 2).min(Duration::from_millis(5));
    }
}

/// Starts the program and collects what it writes until it ends. Runs on a background thread.
fn run_to_end(
    mut cmd: Command,
    program: &str,
    opts: &Options,
    token: &CancelToken,
) -> Result<Output, TypedError> {
    let child = spawn_shared(&mut cmd, program, token)?;
    // The threads start after the spawn, outside the critical section of `start`.
    let (stdin, stdout, stderr) = {
        let mut guard = lock(&child);
        (guard.stdin.take(), guard.stdout.take(), guard.stderr.take())
    };
    let feeder = feed(stdin, opts.input.clone());
    let out = drain(stdout);
    let err = drain(stderr);
    let status = wait_for(&child, program, token)?;
    if let Some(feeder) = feeder {
        let _ = feeder.join();
    }
    Ok(Output {
        status: status_code(status),
        stdout: text(out.join().unwrap_or_default()),
        stderr: text(err.join().unwrap_or_default()),
    })
}

/// Waits for the program and collects its output. The waiting happens on a background thread
/// (`Ctx::run_async_cancellable`): inside a coroutine only that coroutine waits, the others keep
/// running, and cancelling it (or ending the `Vm`) kills the program before it goes on.
fn run(
    cx: &mut Ctx,
    program: &str,
    args: &[String],
    raw: Option<&str>,
    opts: &Options,
) -> Result<Output, TypedError> {
    check_folder(program, opts)?;
    let cmd = command(cx, program, args, raw, opts, true);
    let (program, opts) = (program.to_string(), opts.clone());
    cx.run_async_cancellable(move |token| run_to_end(cmd, &program, &opts, token))
}

/// `run` without capturing: the program shares this one's standard output and error.
fn run_inheriting(cx: &mut Ctx, program: &str, args: &[String]) -> Result<i64, TypedError> {
    let opts = Options::default();
    let mut cmd = command(cx, program, args, None, &opts, false);
    let program = program.to_string();
    cx.run_async_cancellable(move |token| {
        let child = spawn_shared(&mut cmd, &program, token)?;
        wait_for(&child, &program, token).map(status_code)
    })
}

/// A running program, from `process.spawn`. Dropping one that has not finished kills it.
pub(crate) struct Child {
    program: String,
    child: OsChild,
    feeder: Option<JoinHandle<()>>,
    out: Option<JoinHandle<Vec<u8>>>,
    err: Option<JoinHandle<Vec<u8>>>,
    done: Option<Output>,
}

impl Child {
    fn finish(&mut self, status: ExitStatus) -> Output {
        if let Some(feeder) = self.feeder.take() {
            let _ = feeder.join();
        }
        let collect = |h: Option<JoinHandle<Vec<u8>>>| {
            h.map(|h| text(h.join().unwrap_or_default()))
                .unwrap_or_default()
        };
        let output = Output {
            status: status_code(status),
            stdout: collect(self.out.take()),
            stderr: collect(self.err.take()),
        };
        self.done = Some(output.clone());
        output
    }

    fn try_wait(&mut self) -> Result<Option<Output>, TypedError> {
        if let Some(done) = &self.done {
            return Ok(Some(done.clone()));
        }
        match self.child.try_wait() {
            Ok(Some(status)) => Ok(Some(self.finish(status))),
            Ok(None) => Ok(None),
            Err(e) => Err(Kind::Other.error(format!(
                "cannot wait for `{}`: {}",
                self.program,
                reason(&e)
            ))),
        }
    }

    fn wait(&mut self) -> Result<Output, TypedError> {
        if let Some(done) = &self.done {
            return Ok(done.clone());
        }
        match self.child.wait() {
            Ok(status) => Ok(self.finish(status)),
            Err(e) => Err(Kind::Other.error(format!(
                "cannot wait for `{}`: {}",
                self.program,
                reason(&e)
            ))),
        }
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        if self.done.is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

impl HostType for Child {
    const NAME: &'static str = "Child";

    fn describe(d: &mut TypeBuilder<Self>) {
        // The operating system's process number.
        d.method("id", [], |this: &Child| i64::from(this.child.id()));
        // `none` while it still runs; its output (the same every time) once it ended.
        d.method_mut("try_wait", [], |this: &mut Child| this.try_wait());
        // Blocks until it ends. Other coroutines do not run meanwhile (a host object stays
        // borrowed while it waits): poll `try_wait` with `wait(0.01)` between calls to let
        // them, or use `run`.
        d.method_mut("join", [], |this: &mut Child| this.wait());
        // Ends it now (fine if it already ended) and gives its output so far.
        d.method_mut("kill", [], |this: &mut Child| {
            if this.done.is_none() {
                let _ = this.child.kill();
            }
            this.wait()
        });
    }
}

/// What `shell` starts: the program, its ordinary arguments and the raw tail of the command
/// line (see `command`).
type ShellCommand = (String, Vec<String>, Option<String>);

/// The raw argument that makes `cmd` run `line` as typed: `/D /S /C "line"`. With `/S`, `cmd`
/// removes exactly the first and the last quote of what follows `/C` and runs the rest
/// verbatim, so quotes inside the line mean what they mean at a prompt. (A pure function, so
/// that the other systems' tests can look at it.)
fn windows_shell_tail(line: &str) -> String {
    format!("/D /S /C \"{line}\"")
}

fn shell_command(line: &str, windows: bool) -> ShellCommand {
    if windows {
        (
            "cmd".to_string(),
            Vec::new(),
            Some(windows_shell_tail(line)),
        )
    } else {
        (
            "sh".to_string(),
            vec!["-c".to_string(), line.to_string()],
            None,
        )
    }
}

pub(crate) fn register(vm: &mut Vm) -> Result<(), Error> {
    Kind::declare(vm)?;
    let mut m = vm.std_module("process");
    m.register_type::<Child>()?;
    // An empty `Options`: set the fields you need (`let mut o = process.options()`).
    needs(
        Cap::Os,
        m.register_fn_named("options", [], Options::default),
    )?;
    needs(
        Cap::Os,
        m.register_fn_named(
            "run",
            ["program", "args"],
            |cx: &mut Ctx, program: String, args: Vec<String>| {
                run(cx, &program, &args, None, &Options::default())
            },
        ),
    )?;
    needs(
        Cap::Os,
        m.register_fn_named(
            "run_with",
            ["program", "args", "options"],
            |cx: &mut Ctx, program: String, args: Vec<String>, options: Options| {
                run(cx, &program, &args, None, &options)
            },
        ),
    )?;
    // The standard output of a program that succeeded; a failing one is an `err` with its
    // status and standard error.
    needs(
        Cap::Os,
        m.register_fn_named(
            "output",
            ["program", "args"],
            |cx: &mut Ctx, program: String, args: Vec<String>| {
                let out = run(cx, &program, &args, None, &Options::default())?;
                if out.status == 0 {
                    return Ok(out.stdout);
                }
                let detail = out.stderr.trim();
                Err(Kind::Failed.error(format!(
                    "`{program}` failed with status {}{}{detail}",
                    out.status,
                    if detail.is_empty() { "" } else { ": " }
                )))
            },
        ),
    )?;
    // The program shares this one's standard output and error, which the `Vm`'s own output
    // buffer never sees; use `run` to capture it.
    needs(
        Cap::Os,
        m.register_fn_named(
            "status",
            ["program", "args"],
            |cx: &mut Ctx, program: String, args: Vec<String>| run_inheriting(cx, &program, &args),
        ),
    )?;
    // `run` through the system shell (`sh -c`, `cmd /D /S /C "line"` on Windows): pipes, globs and quoting
    // are the shell's, and so are its portability problems.
    needs(
        Cap::Os,
        m.register_fn_named("shell", ["line"], |cx: &mut Ctx, line: String| {
            let (program, args, raw) = shell_command(&line, cfg!(windows));
            run(cx, &program, &args, raw.as_deref(), &Options::default())
        }),
    )?;
    needs(
        Cap::Os,
        m.register_fn_named(
            "spawn",
            ["program", "args"],
            |cx: &mut Ctx, program: String, args: Vec<String>| {
                spawn(cx, program, &args, &Options::default())
            },
        ),
    )?;
    needs(
        Cap::Os,
        m.register_fn_named(
            "spawn_with",
            ["program", "args", "options"],
            |cx: &mut Ctx, program: String, args: Vec<String>, options: Options| {
                spawn(cx, program, &args, &options)
            },
        ),
    )?;
    Ok(())
}

fn spawn(
    cx: &mut Ctx,
    program: String,
    args: &[String],
    opts: &Options,
) -> Result<Child, TypedError> {
    check_folder(&program, opts)?;
    let mut child = command(cx, &program, args, None, opts, true)
        .spawn()
        .map_err(|e| start_error(&program, &e))?;
    let feeder = feed(child.stdin.take(), opts.input.clone());
    let out = Some(drain(child.stdout.take()));
    let err = Some(drain(child.stderr.take()));
    Ok(Child {
        program,
        child,
        feeder,
        out,
        err,
        done: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shell_line_is_unchanged_on_unix_and_one_raw_argument_on_windows() {
        assert_eq!(
            shell_command("echo \"a b\" & x", false),
            (
                "sh".to_string(),
                vec!["-c".to_string(), "echo \"a b\" & x".to_string()],
                None
            )
        );
        assert_eq!(
            shell_command("echo \"a b\" & x", true),
            (
                "cmd".to_string(),
                Vec::new(),
                Some("/D /S /C \"echo \"a b\" & x\"".to_string())
            )
        );
        assert_eq!(windows_shell_tail(""), "/D /S /C \"\"");
    }

    #[test]
    fn a_token_cancelled_before_the_wait_gives_up_without_waiting_for_the_pid() {
        let child = Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("sleep starts");
        let child: Shared = Arc::new(Mutex::new(child));
        let token = CancelToken::cancelled();
        // The hook runs at once on a cancelled token: the child is killed and collected, so
        // its pid may be reused and must not be waited for.
        let victim = Arc::clone(&child);
        token.on_cancel(move || {
            let mut child = lock(&victim);
            let _ = child.kill();
            let _ = child.wait();
        });
        let started = std::time::Instant::now();
        let result = wait_for(&child, "sleep", &token);
        assert!(result.is_err_and(|e| e.message().contains("was stopped")));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_token_cancelled_before_the_spawn_starts_no_program() {
        let marker = std::env::temp_dir().join(format!("colibri-never-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        // A program that would create the marker: `touch` on Unix, `cmd` on Windows.
        #[cfg(unix)]
        let (mut cmd, program) = {
            let mut cmd = Command::new("touch");
            cmd.arg(&marker);
            (cmd, "touch")
        };
        #[cfg(windows)]
        let (mut cmd, program) = {
            let mut cmd = Command::new("cmd");
            cmd.args(["/C", "type nul >"]).arg(&marker);
            (cmd, "cmd")
        };
        let token = CancelToken::cancelled();
        let result = spawn_shared(&mut cmd, program, &token);
        assert!(result.is_err_and(|e| e.message().contains("was not started")));
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            !marker.exists(),
            "the program ran although it was cancelled"
        );
    }
}
