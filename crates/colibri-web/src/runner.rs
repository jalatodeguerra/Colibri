//! The program of a pack, run by a host that ticks it (the page). Nothing here depends on the
//! web, so it is tested natively; `ffi` (wasm only) is the C ABI around it.
//!
//! **How it runs.** A page has one thread and must never block it, so the program does not run
//! to its end. The script's top-level statements and `main` start as coroutines (`Vm::start_entry`)
//! and run until their first `wait`; after that the host calls [`Runner::tick`] with the time that
//! has passed, and each tick resumes the coroutines that are due. [`Runner::next_deadline`] says
//! how long the host may sleep until the next one. The program is over when `main` has returned
//! and no coroutine is left that a timer could wake (the loop of `colibri run`).

use std::io::Write;
use std::rc::Rc;

use colibri_pack::{Pack, Start};
use colibri_runtime::{Entry, Error, Vm};

/// The size of the wasm stack, which `build.rs` asks the linker for.
pub const STACK_SIZE: usize = 16 * 1024 * 1024;

/// What the engine may use of the stack: a margin is left for the rest of the program. The engine
/// turns what would be a stack overflow into a Colibri panic before it reaches the end.
pub const NATIVE_STACK_BUDGET: usize = STACK_SIZE - 4 * 1024 * 1024;

/// Exit code of a program that panicked (what `colibri run` gives).
pub const EXIT_PANIC: i32 = 101;

/// Where a run stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Coroutines are alive; tick again.
    Running,
    /// The program ended normally.
    Done,
    /// The program failed (a compile error, a panic, `main` returning `err`): the reason was
    /// written to the error stream, and [`Runner::exit_code`] says which kind.
    Failed,
}

/// Where the output goes: standard output and the error stream, which the page shows apart
/// (the second goes to the console as well).
pub trait Sink {
    fn stdout(&mut self, text: &[u8]);
    fn stderr(&mut self, text: &[u8]);
}

/// `print` writes here, a piece at a time.
struct OutWriter(Rc<std::cell::RefCell<Box<dyn Sink>>>);

impl Write for OutWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().stdout(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A program that is running.
pub struct Runner {
    vm: Vm,
    sink: Rc<std::cell::RefCell<Box<dyn Sink>>>,
    state: State,
    code: i32,
}

impl Runner {
    /// Loads the pack in `bytes` and starts the program, which runs until its first pause. A pack
    /// that cannot be read, or a program that does not compile, gives a runner that has
    /// [`State::Failed`] (the reason is already on the error stream); this never fails otherwise.
    pub fn start(bytes: &[u8], sink: Box<dyn Sink>) -> Runner {
        let sink = Rc::new(std::cell::RefCell::new(sink));
        let mut runner = Runner {
            // Replaced once the pack is read; nothing runs in this one.
            vm: Vm::builder().stdout(Box::new(std::io::sink())).build(),
            sink: Rc::clone(&sink),
            state: State::Running,
            code: 0,
        };
        let pack = match Pack::from_bytes(bytes) {
            Ok(pack) => pack,
            Err(message) => {
                runner.fail_text(&format!("error: {message}\n"), 1);
                return runner;
            }
        };
        runner.vm = Runner::build_vm(&pack, &sink);
        let (fs, start) = pack.file_system();
        let report = Rc::clone(&sink);
        runner.vm.on_warnings(move |warnings| {
            report
                .borrow_mut()
                .stderr(warnings.render(false).as_bytes());
        });
        let loaded = match &start {
            Start::File(path) => runner
                .vm
                .load_from_deferred(&fs, Entry::File(std::path::Path::new(path))),
            Start::Package(dir) => runner
                .vm
                .load_from_deferred(&fs, Entry::Package(std::path::Path::new(dir))),
        };
        match loaded.and_then(|()| runner.vm.start_entry()) {
            Ok(()) => runner.settle(),
            Err(error) => runner.fail(&error),
        }
        runner
    }

    /// The `Vm` of a pack: no capability (nothing on the web reaches the machine), the pack's
    /// build configuration, and the standard modules that exist on the web.
    fn build_vm(pack: &Pack, sink: &Rc<std::cell::RefCell<Box<dyn Sink>>>) -> Vm {
        let mut vm = Vm::builder()
            .stdout(Box::new(OutWriter(Rc::clone(sink))))
            .build_cfg(pack.cfg.clone())
            .native_stack_budget(NATIVE_STACK_BUDGET)
            .build();
        colibri_std::install(&mut vm).expect("the standard library registers into an empty Vm");
        vm
    }

    /// Advances the program's time by `dt` seconds and resumes the coroutines that are due.
    /// A runner that is not [`State::Running`] does nothing.
    pub fn tick(&mut self, dt: f64) -> State {
        if self.state != State::Running {
            return self.state;
        }
        // Not negative, not NaN: a host's clock that steps back must not move time back.
        let dt = if dt.is_finite() && dt > 0.0 { dt } else { 0.0 };
        match self.vm.tick(dt) {
            Ok(()) => self.settle(),
            Err(error) => self.fail(&error),
        }
        self.state
    }

    /// How many milliseconds until the next coroutine needs a tick: `0` for one that is due or
    /// polls (at each frame), `-1` when nothing is due until something happens.
    pub fn next_deadline(&mut self) -> f64 {
        if self.state != State::Running {
            return -1.0;
        }
        self.vm.next_wake(0.0).map_or(-1.0, |seconds| {
            // Never a negative number or a NaN: `-1` means "nothing".
            if seconds.is_nan() || seconds < 0.0 {
                0.0
            } else {
                seconds * 1000.0
            }
        })
    }

    pub fn state(&self) -> State {
        self.state
    }

    /// `0` for a program that ended normally, `101` for a panic, `1` for any other failure.
    pub fn exit_code(&self) -> i32 {
        self.code
    }

    /// After a start or a tick: moves the entry on, notes a failure, decides whether the program
    /// is over.
    fn settle(&mut self) {
        let entry_over = match self.vm.entry_step() {
            Ok(over) => over,
            Err(error) => return self.fail(&error),
        };
        if let Err(error) = self.vm.take_coroutine_failure() {
            return self.fail(&error);
        }
        // Over: `main` has returned and nothing is left that a timer could wake (a coroutine that
        // waits for a signal nobody can emit is cancelled at the end, as in `colibri run`).
        let nothing_due = self.vm.next_wake(0.0).is_none();
        if entry_over && (self.vm.coroutine_count() == 0 || nothing_due) {
            self.state = State::Done;
            self.finish();
        } else if nothing_due {
            // `main` waits for an event (a signal, a join) that nothing can cause: there is no
            // other thread, and no input, to cause one. `colibri run` panics here too.
            self.fail_text(
                "error: the program waits for something that cannot happen (a signal nobody emits, or a coroutine that cannot end)\n",
                1,
            );
        }
    }

    fn fail(&mut self, error: &Error) {
        let code = match error {
            Error::Panic { .. } => EXIT_PANIC,
            _ => 1,
        };
        self.fail_text(&error.render(false), code);
    }

    fn fail_text(&mut self, text: &str, code: i32) {
        self.sink.borrow_mut().stderr(text.as_bytes());
        self.state = State::Failed;
        self.code = code;
        self.finish();
    }

    /// The end of the program: the coroutines are cancelled and the module-level values die, so
    /// that their `Drop` hooks print what they print.
    fn finish(&mut self) {
        self.vm.shutdown();
    }
}
