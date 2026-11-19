//! `process`: running other programs. Everything needs the `os` capability.
//!
//! `run` waits for the program and collects its output (on a background thread: inside a
//! coroutine only that coroutine waits, and cancelling it stops the program); `spawn` starts it and gives back a
//! `Child` a script can poll with `try_wait` (and `wait(0.01)` between polls, so other
//! coroutines keep running). A program that cannot be started is an `err`; one that runs and
//! exits with a non-zero status is an `Output` with that `status`.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child as OsChild, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use colibri_runtime::{Cap, Ctx, Error, HostType, TypeBuilder, Vm};

use crate::config::State;
use crate::fs::reason;
use crate::needs;
use crate::values::value_struct;

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

/// The command, with the environment of this program: the process's own plus whatever
/// `os.set_env` changed, then the options' own variables on top.
fn command(cx: &mut Ctx, program: &str, args: &[String], opts: &Options, piped: bool) -> Command {
    let mut cmd = Command::new(program);
    cmd.args(args);
    if let Some(cwd) = &opts.cwd {
        cmd.current_dir(cwd);
    }
    if let Some(state) = cx.state_mut::<State>() {
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

fn start_error(program: &str, opts: &Options, e: &std::io::Error) -> String {
    // A missing folder is reported by the OS as the program being missing.
    if e.kind() == std::io::ErrorKind::NotFound
        && let Some(cwd) = &opts.cwd
        && !Path::new(cwd).is_dir()
    {
        return format!("cannot run `{program}`: the folder `{cwd}` does not exist");
    }
    format!("cannot run `{program}`: {}", reason(e))
}

/// Writes the input on its own thread (a program may write a lot before it reads) and closes
/// the pipe.
fn feed(child: &mut OsChild, input: Option<String>) -> Option<JoinHandle<()>> {
    let (mut stdin, input) = (child.stdin.take()?, input?);
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

/// Waits for `child`, giving up when `cancelled` is set: the child is killed then, so that it
/// neither goes on running nor keeps a pool thread waiting for it.
fn wait_for(
    child: &mut OsChild,
    program: &str,
    cancelled: &AtomicBool,
) -> Result<ExitStatus, String> {
    // Short naps at first, so that a quick program costs almost nothing, then longer ones.
    let mut nap = Duration::from_micros(100);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => {}
            Err(e) => return Err(format!("cannot wait for `{program}`: {}", reason(&e))),
        }
        if cancelled.load(Ordering::SeqCst) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("`{program}` was stopped: its caller was cancelled"));
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
    cancelled: &AtomicBool,
) -> Result<Output, String> {
    let mut child = cmd.spawn().map_err(|e| start_error(program, opts, &e))?;
    let feeder = feed(&mut child, opts.input.clone());
    let out = drain(child.stdout.take());
    let err = drain(child.stderr.take());
    let status = wait_for(&mut child, program, cancelled)?;
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
/// running, and cancelling it stops the program.
fn run(cx: &mut Ctx, program: &str, args: &[String], opts: &Options) -> Result<Output, String> {
    let cmd = command(cx, program, args, opts, true);
    let (program, opts) = (program.to_string(), opts.clone());
    cx.run_async_cancellable(move |cancelled| run_to_end(cmd, &program, &opts, cancelled))
}

/// `run` without capturing: the program shares this one's standard output and error.
fn run_inheriting(cx: &mut Ctx, program: &str, args: &[String]) -> Result<i64, String> {
    let opts = Options::default();
    let mut cmd = command(cx, program, args, &opts, false);
    let program = program.to_string();
    cx.run_async_cancellable(move |cancelled| {
        let mut child = cmd.spawn().map_err(|e| start_error(&program, &opts, &e))?;
        wait_for(&mut child, &program, cancelled).map(status_code)
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

    fn try_wait(&mut self) -> Result<Option<Output>, String> {
        if let Some(done) = &self.done {
            return Ok(Some(done.clone()));
        }
        match self.child.try_wait() {
            Ok(Some(status)) => Ok(Some(self.finish(status))),
            Ok(None) => Ok(None),
            Err(e) => Err(format!(
                "cannot wait for `{}`: {}",
                self.program,
                reason(&e)
            )),
        }
    }

    fn wait(&mut self) -> Result<Output, String> {
        if let Some(done) = &self.done {
            return Ok(done.clone());
        }
        match self.child.wait() {
            Ok(status) => Ok(self.finish(status)),
            Err(e) => Err(format!(
                "cannot wait for `{}`: {}",
                self.program,
                reason(&e)
            )),
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

#[cfg(windows)]
fn shell_command(line: &str) -> (String, Vec<String>) {
    ("cmd".to_string(), vec!["/C".to_string(), line.to_string()])
}

#[cfg(not(windows))]
fn shell_command(line: &str) -> (String, Vec<String>) {
    ("sh".to_string(), vec!["-c".to_string(), line.to_string()])
}

pub(crate) fn register(vm: &mut Vm) -> Result<(), Error> {
    let mut m = vm.std_module("process");
    m.register_type::<Child>()?;
    // An empty `Options`: set the fields you need (`var o = process.options()`).
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
                run(cx, &program, &args, &Options::default())
            },
        ),
    )?;
    needs(
        Cap::Os,
        m.register_fn_named(
            "run_with",
            ["program", "args", "options"],
            |cx: &mut Ctx, program: String, args: Vec<String>, options: Options| {
                run(cx, &program, &args, &options)
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
                let out = run(cx, &program, &args, &Options::default())?;
                if out.status == 0 {
                    return Ok(out.stdout);
                }
                let detail = out.stderr.trim();
                Err(format!(
                    "`{program}` failed with status {}{}{detail}",
                    out.status,
                    if detail.is_empty() { "" } else { ": " }
                ))
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
    // `run` through the system shell (`sh -c`, `cmd /C` on Windows): pipes, globs and quoting
    // are the shell's, and so are its portability problems.
    needs(
        Cap::Os,
        m.register_fn_named("shell", ["line"], |cx: &mut Ctx, line: String| {
            let (program, args) = shell_command(&line);
            run(cx, &program, &args, &Options::default())
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

fn spawn(cx: &mut Ctx, program: String, args: &[String], opts: &Options) -> Result<Child, String> {
    let mut child = command(cx, &program, args, opts, true)
        .spawn()
        .map_err(|e| start_error(&program, opts, &e))?;
    let feeder = feed(&mut child, opts.input.clone());
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
