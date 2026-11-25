//! `colibri run --watch`: run a program and keep it up to date with its source files
//! (DESIGN §16.3, ARCHITECTURE §17).
//!
//! The runtime is single-threaded, so the only moments the code can change are the ones
//! where no Colibri code is running:
//!
//! * while the event loop of a program that still has coroutines runs, between two ticks (the
//!   loop never sleeps longer than [`WATCH_INTERVAL`]): a change that only touches function
//!   bodies is patched into the running program, any other change restarts it and runs `main`
//!   again;
//! * after `main` and the event loop have ended, or after a panic: the tool prints the result,
//!   waits for an edit and then **restarts** the program (module state is built again, the
//!   `save_state` / `restore_state` hooks carry state) and runs `main` again.
//!
//! A `main` that never returns and never yields cannot be reloaded until it does.

use std::io::{self, IsTerminal};
use std::path::Path;
use std::process::ExitCode;
use std::thread;
use std::time::Duration;

use colibri_runtime::{Error, ReloadOptions, ReloadOutcome, Vm};

/// How often the files are looked at, in seconds.
pub const WATCH_INTERVAL: f64 = 0.15;

/// Runs `file` (a script or a package folder) and reloads it when its sources change. Returns
/// only when the program calls `os.exit`.
pub fn run(file: &Path, args: Vec<String>) -> ExitCode {
    let mut vm = crate::build_vm(crate::vm_builder().allow_all(), args);
    let mut watcher = Watcher {
        color: io::stderr().is_terminal(),
        last_io_error: None,
    };
    // Warnings are printed once the program has checked and before any of its code runs (its
    // top level may run for a long time or exit), at the first load and at every restart.
    let color = watcher.color;
    vm.on_warnings(move |report| eprint!("{}", report.render(color)));
    let mut ready = match vm.load_path(file) {
        Ok(()) => true,
        Err(e) => match watcher.failed(&mut vm, &e) {
            Some(code) => return code,
            None => {
                note("waiting for a change");
                false
            }
        },
    };
    loop {
        if ready {
            let outcome = run_program(&mut vm, &mut watcher);
            let ended = match outcome {
                Ok(()) => "the program ended; waiting for a change",
                Err(e) => {
                    if let Some(code) = watcher.failed(&mut vm, &e) {
                        return code;
                    }
                    "waiting for a change"
                }
            };
            note(ended);
        }
        // Whatever ended the program, an edit starts it again from fresh module state.
        ready = false;
        while !ready {
            thread::sleep(Duration::from_secs_f64(WATCH_INTERVAL));
            let options = ReloadOptions {
                force_restart: true,
            };
            match vm.reload_changed_with(options) {
                Ok(outcome) => ready = watcher.outcome(&vm, outcome),
                Err(e) => {
                    if let Some(code) = watcher.failed(&mut vm, &e) {
                        return code;
                    }
                }
            }
        }
    }
}

/// `main`, then the event loop, looking for edits between ticks.
fn run_program(vm: &mut Vm, watcher: &mut Watcher) -> Result<(), Error> {
    vm.run_main()?;
    vm.run_event_loop_watching(0.001, WATCH_INTERVAL, |vm| {
        let outcome = vm.reload_changed()?;
        let restarted = matches!(outcome, ReloadOutcome::Restarted { .. });
        watcher.outcome(vm, outcome);
        if restarted {
            // The new program starts the way a fresh run does.
            vm.run_main()?;
        }
        Ok(())
    })
}

struct Watcher {
    color: bool,
    /// So that a file that stays unreadable is reported once, not at every poll.
    last_io_error: Option<String>,
}

impl Watcher {
    /// Prints what a reload did. True when a (new) program is ready to run `main`.
    fn outcome(&mut self, vm: &Vm, outcome: ReloadOutcome) -> bool {
        self.last_io_error = None;
        match outcome {
            ReloadOutcome::Unchanged => false,
            ReloadOutcome::Patched { functions } => {
                self.warnings(vm);
                let s = if functions == 1 { "" } else { "s" };
                note(&format!("patched {functions} function{s}"));
                true
            }
            ReloadOutcome::Restarted {
                reason,
                state_carried,
            } => {
                let carried = if state_carried { ", state carried" } else { "" };
                note(&format!("restarted ({reason}{carried})"));
                true
            }
            ReloadOutcome::CheckFailed(report) => {
                eprint!("{}", report.render(self.color));
                note("the new code does not check; keeping the old program");
                false
            }
        }
    }

    fn warnings(&self, vm: &Vm) {
        if !vm.warnings().is_empty() {
            eprint!("{}", vm.render_warnings(self.color));
        }
    }

    /// Prints an error. `Some(code)` when the program asked to end the process (`os.exit`).
    fn failed(&mut self, vm: &mut Vm, error: &Error) -> Option<ExitCode> {
        match error {
            Error::Panic { .. } => {
                if let Some(code) = colibri_std::exit_code(vm) {
                    return Some(ExitCode::from(code as u8));
                }
                eprint!("{}", error.render(self.color));
            }
            Error::Io { .. } => {
                let text = error.render(self.color);
                if self.last_io_error.as_deref() != Some(text.as_str()) {
                    eprint!("{text}");
                    self.last_io_error = Some(text);
                }
            }
            other => eprint!("{}", other.render(self.color)),
        }
        None
    }
}

fn note(text: &str) {
    eprintln!("colibri: {text}");
}
