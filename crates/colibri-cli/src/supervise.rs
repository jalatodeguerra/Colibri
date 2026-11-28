//! `colibri run --watch` in a package with Rust bindings: this process stays and keeps the project
//! runner running. Colibri edits are the runner's business (it watches, patches and restarts in
//! place, as `colibri run --watch` always did). Rust edits are ours: when the runner the project
//! would have now is another one (a binding crate or a crate it names by path was edited, or a
//! manifest changed), the runner is built again, and on success the running one is stopped and the
//! new one started ("cold restart"); on failure cargo's errors are printed and the running one
//! goes on, until the next edit.
//!
//! Ending: the runner's exit code is ours, unchanged (a signal is `128 + n` on Unix). Ctrl-C
//! reaches both of us, so the runner ends and so do we. If this process dies in some other way, the
//! runner must not live on: on Unix it holds the read end of a pipe whose write end only this
//! process has, and ends when it sees the end of the file ([`die_with_parent`]); on Windows the
//! runner is in a job object that kills it when this process goes.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode};
use std::time::{Duration, Instant};

use colibri_runtime::ProjectBinding;

use crate::bindings::build::{self, Env, Options};
use crate::delegate::GUARD;

/// What the supervisor needs to build the runner again.
pub struct Project {
    pub env: Env,
    /// The folder of the package (what the command was given, or the script's folder).
    pub folder: PathBuf,
    pub root: PathBuf,
    pub found: Vec<ProjectBinding>,
    pub options: Options,
}

/// How often the child is looked at.
const TICK: Duration = Duration::from_millis(100);

fn note(text: &str) {
    eprintln!("colibri: {text}");
}

/// Runs `runner` with the command line this process got, and keeps it in step with the Rust code
/// of `project`. Returns when the runner ends.
pub fn supervise(mut project: Project, runner: PathBuf) -> ExitCode {
    let guard = match Guard::new() {
        Ok(g) => g,
        Err(e) => return fail(&format!("cannot supervise the runner: {e}")),
    };
    ignore_interrupts();
    let mut current = runner;
    let mut child = match spawn(&current, &guard) {
        Ok(c) => c,
        Err(e) => {
            return fail(&format!(
                "cannot run the runner `{}`: {e}",
                current.display()
            ));
        }
    };
    let mut manifests = manifest_times(&project);
    // The runner path a failed build was for: not tried again until it changes.
    let mut failed_for: Option<PathBuf> = None;
    let mut next_look = Instant::now() + MIN_LOOK;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return exit_code(status),
            Ok(None) => {}
            Err(e) => return fail(&format!("cannot wait for the runner: {e}")),
        }
        // Ctrl-C sent to this process alone (the terminal sends it to both): the runner gets it.
        if take_interrupt() {
            interrupt(&child);
        }
        std::thread::sleep(TICK);
        if Instant::now() < next_look {
            continue;
        }
        let looked = Instant::now();
        // A manifest edit may add, drop or change a binding: ask the loader again.
        let now = manifest_times(&project);
        if now != manifests {
            manifests = now;
            match build::project(&project.folder) {
                Ok((root, found)) if !found.is_empty() => {
                    project.root = root;
                    project.found = found;
                }
                Ok(_) => note(
                    "the package declares no Rust bindings any more; restart `colibri run --watch` to run it without a project runner",
                ),
                Err(e) => note(&format!("cannot read the Rust bindings: {e}")),
            }
        }
        let expected =
            build::expected_runner(&project.env, &project.root, &project.found, project.options);
        // The next look waits in proportion to what this one cost (a big crate is looked at less
        // often), so that an idle watch stays a small share of a core.
        next_look = Instant::now() + next_look_after(looked.elapsed());
        let expected = match expected {
            Ok(path) => path,
            Err(e) => {
                note(&format!("cannot tell whether the Rust code changed: {e}"));
                continue;
            }
        };
        if expected == current || failed_for.as_ref() == Some(&expected) {
            continue;
        }
        note("Rust code changed: building the runner");
        let options = Options {
            rebuilding: true,
            ..project.options
        };
        match build::ensure_runner(&project.env, &project.root, &project.found, options) {
            Ok(built) => {
                failed_for = None;
                if built.path == current {
                    continue;
                }
                // Ctrl-C in the meantime ended the runner: this is the end, not a restart.
                if let Some(status) = ended_meanwhile(&mut child, false) {
                    return exit_code(status);
                }
                crate::delegate::say_interfaces(&built);
                note("Rust code changed: rebuilt the runner and restarted");
                stop(&mut child);
                current = built.path;
                child = match spawn(&current, &guard) {
                    Ok(c) => c,
                    Err(e) => {
                        return fail(&format!(
                            "cannot run the runner `{}`: {e}",
                            current.display()
                        ));
                    }
                };
            }
            Err(e) => {
                // Ctrl-C during the build: the runner is gone, and that is all there is to say.
                if let Some(status) = ended_meanwhile(&mut child, e.contains("cargo was stopped")) {
                    return exit_code(status);
                }
                eprintln!("error: {e}");
                note("the Rust code does not build; the old runner keeps running");
                failed_for = Some(expected);
            }
        }
        next_look = Instant::now() + next_look_after(looked.elapsed());
    }
}

/// After a build: did the runner end while it ran (Ctrl-C)? If this process was interrupted, the
/// runner is given the signal and a moment to end, so that the answer does not depend on which of
/// the two processes the system got to first.
fn ended_meanwhile(child: &mut Child, stopped: bool) -> Option<std::process::ExitStatus> {
    // (`stopped`: cargo itself was ended by a signal, which may reach this process a moment later.)
    if take_interrupt() || stopped {
        interrupt(child);
        let until = Instant::now() + Duration::from_secs(10);
        while Instant::now() < until {
            if let Ok(Some(status)) = child.try_wait() {
                return Some(status);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    child.try_wait().ok().flatten()
}

/// The least time between two looks at the Rust code.
const MIN_LOOK: Duration = Duration::from_millis(500);

/// When the next look is, given what the last one took: never sooner than [`MIN_LOOK`], and not
/// before 20 times the cost of a look, which keeps the watch under about 5% of a core.
pub fn next_look_after(last: Duration) -> Duration {
    MIN_LOOK.max(last * 20)
}

/// Stops a runner to start another: it is asked to end (so that it, and what it started, can
/// clean up) and killed if it has not ended after a short while.
fn stop(child: &mut Child) {
    #[cfg(unix)]
    {
        // SAFETY: sending a signal to a child of this process.
        unsafe {
            libc::kill(child.id() as i32, libc::SIGTERM);
        }
        let until = Instant::now() + Duration::from_secs(2);
        while Instant::now() < until {
            if matches!(child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Set by the SIGINT handler: this process was interrupted.
#[cfg(unix)]
static INTERRUPTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(unix)]
fn take_interrupt() -> bool {
    INTERRUPTED.swap(false, std::sync::atomic::Ordering::SeqCst)
}

#[cfg(unix)]
fn interrupt(child: &Child) {
    // SAFETY: sending a signal to a child of this process; if the terminal sent it already,
    // a second one is harmless.
    unsafe {
        libc::kill(child.id() as i32, libc::SIGINT);
    }
}

// (On Windows the console sends the event to every process on it.)
#[cfg(windows)]
fn take_interrupt() -> bool {
    false
}

#[cfg(windows)]
fn interrupt(_child: &Child) {}

fn fail(message: &str) -> ExitCode {
    eprintln!("error: {message}");
    ExitCode::from(1)
}

/// The modification times of the manifests that name Rust bindings (and the root's).
fn manifest_times(project: &Project) -> Vec<(PathBuf, Option<std::time::SystemTime>)> {
    let mut paths = vec![project.root.join(colibri_runtime::MANIFEST_FILE)];
    for b in &project.found {
        let p = b.dir.join(colibri_runtime::MANIFEST_FILE);
        if !paths.contains(&p) {
            paths.push(p);
        }
    }
    paths
        .into_iter()
        .map(|p| {
            let t = std::fs::metadata(&p).and_then(|m| m.modified()).ok();
            (p, t)
        })
        .collect()
}

fn spawn(runner: &Path, guard: &Guard) -> std::io::Result<Child> {
    let mut command = Command::new(runner);
    command.args(std::env::args_os().skip(1)).env(GUARD, "1");
    guard.prepare(&mut command);
    let child = command.spawn()?;
    guard.adopt(&child);
    Ok(child)
}

#[cfg(unix)]
fn exit_code(status: std::process::ExitStatus) -> ExitCode {
    use std::os::unix::process::ExitStatusExt;
    match (status.code(), status.signal()) {
        (Some(code), _) => ExitCode::from(code as u8),
        (None, Some(signal)) => ExitCode::from((128 + signal) as u8),
        _ => ExitCode::from(1),
    }
}

#[cfg(windows)]
fn exit_code(status: std::process::ExitStatus) -> ExitCode {
    // The code as the system has it (a 32 bit number); `ExitCode` has 8 bits, so the process
    // leaves with `exit` instead.
    std::process::exit(status.code().unwrap_or(1))
}

/// This process passes Ctrl-C on to the runner: the runner ends, then so does this process, with the
/// runner's status. (A handler that only sets a flag, not `SIG_IGN`: an ignored signal stays ignored
/// in what is started, cargo and the runner, and a handler does not.)
#[cfg(unix)]
fn ignore_interrupts() {
    extern "C" fn interrupted(_signal: i32) {
        INTERRUPTED.store(true, std::sync::atomic::Ordering::SeqCst);
    }
    // SAFETY: installing a handler that does nothing.
    unsafe {
        libc::signal(libc::SIGINT, interrupted as extern "C" fn(i32) as usize);
    }
}

#[cfg(windows)]
fn ignore_interrupts() {
    use windows_sys::Win32::System::Console::{
        CTRL_BREAK_EVENT, CTRL_C_EVENT, SetConsoleCtrlHandler,
    };
    unsafe extern "system" fn ignore(kind: u32) -> i32 {
        i32::from(kind == CTRL_C_EVENT || kind == CTRL_BREAK_EVENT)
    }
    // SAFETY: the routine only reads its argument.
    unsafe {
        SetConsoleCtrlHandler(Some(ignore), 1);
    }
}

/// The variable that names the pipe a runner under a supervisor watches (Unix).
#[cfg(unix)]
pub const PARENT_FD: &str = "COLIBRI_SUPERVISOR_FD";

/// In a runner: ends the process when the supervisor that started it is gone. The runner holds the
/// read end of a pipe of which the supervisor holds the only write end; the read ends with the end
/// of the file when the supervisor's process ends, however it ends.
pub fn die_with_parent() {
    #[cfg(unix)]
    {
        use std::io::Read;
        use std::os::fd::FromRawFd;
        let Some(fd) = std::env::var(PARENT_FD)
            .ok()
            .and_then(|v| v.parse::<i32>().ok())
        else {
            return;
        };
        // Not for the programs this runner runs: the descriptor is closed on exec, and the
        // variable that names it is gone.
        // SAFETY: the start of `runner_main`, before any thread of ours exists.
        unsafe {
            std::env::remove_var(PARENT_FD);
            libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
        }
        // SAFETY: the supervisor passed this descriptor, open for reading, for this process alone.
        let mut pipe = unsafe { std::fs::File::from_raw_fd(fd) };
        let _ = std::thread::Builder::new()
            .name("colibri-supervisor".into())
            .spawn(move || {
                let mut byte = [0u8; 1];
                // Nothing is ever written: a read returns when the other end is gone.
                loop {
                    match pipe.read(&mut byte) {
                        Ok(n) if n > 0 => {}
                        // A signal interrupted the read: not the end of the file.
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                        _ => std::process::exit(1),
                    }
                }
            });
    }
}

/// What ties the runners to this process.
#[cfg(unix)]
struct Guard {
    read: i32,
    /// Kept open for as long as this process lives: its end is the signal.
    _write: std::fs::File,
}

#[cfg(unix)]
impl Guard {
    fn new() -> std::io::Result<Guard> {
        use std::os::fd::FromRawFd;
        let mut fds = [0i32; 2];
        // SAFETY: `fds` has room for the two descriptors.
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        // The write end must not be inherited (or the end of the file would never come); the read
        // end must.
        // SAFETY: both descriptors are ours.
        unsafe {
            libc::fcntl(fds[1], libc::F_SETFD, libc::FD_CLOEXEC);
            libc::fcntl(fds[0], libc::F_SETFD, 0);
        }
        // SAFETY: `fds[1]` is a descriptor just made and owned here.
        let write = unsafe { std::fs::File::from_raw_fd(fds[1]) };
        Ok(Guard {
            read: fds[0],
            _write: write,
        })
    }

    fn prepare(&self, command: &mut Command) {
        command.env(PARENT_FD, self.read.to_string());
    }

    fn adopt(&self, _child: &Child) {}
}

#[cfg(windows)]
struct Guard {
    job: windows_sys::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
impl Guard {
    fn new() -> std::io::Result<Guard> {
        use windows_sys::Win32::System::JobObjects::{
            CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject,
        };
        // SAFETY: plain system calls with valid arguments.
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return Err(std::io::Error::last_os_error());
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let ok = SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                (&raw const info).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            if ok == 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(Guard { job })
        }
    }

    fn prepare(&self, _command: &mut Command) {}

    /// The runner joins the job: when this process ends, the system kills what is in it.
    fn adopt(&self, child: &Child) {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;
        // SAFETY: both handles are valid; a failure leaves the runner outside the job.
        unsafe {
            AssignProcessToJobObject(self.job, child.as_raw_handle());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_next_look_waits_in_proportion_to_the_last() {
        // A cheap look: the least time.
        assert_eq!(next_look_after(Duration::from_millis(1)), MIN_LOOK);
        assert_eq!(next_look_after(Duration::ZERO), MIN_LOOK);
        // A look of 43 ms (20,000 files): 20 times that, so the watch is at most 5% busy.
        assert_eq!(
            next_look_after(Duration::from_millis(43)),
            Duration::from_millis(860)
        );
        // The boundary: 25 ms is exactly the least time.
        assert_eq!(next_look_after(Duration::from_millis(25)), MIN_LOOK);
        assert!(next_look_after(Duration::from_millis(26)) > MIN_LOOK);
    }
}
