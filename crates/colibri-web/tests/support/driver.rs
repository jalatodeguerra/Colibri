//! A host that ticks a [`Runner`] on a virtual clock, as `colibri.js` does on a page's.

use std::cell::RefCell;
use std::rc::Rc;

use colibri_web::{Runner, Sink, State};

/// One frame of the page's clock, in milliseconds (`colibri.js`'s `FRAME_MS`).
pub const FRAME_MS: f64 = 1000.0 / 60.0;

#[derive(Default)]
pub struct Captured {
    pub stdout: String,
    pub stderr: String,
}

struct Capture(Rc<RefCell<Captured>>);

impl Sink for Capture {
    fn stdout(&mut self, text: &[u8]) {
        self.0
            .borrow_mut()
            .stdout
            .push_str(&String::from_utf8_lossy(text));
    }

    fn stderr(&mut self, text: &[u8]) {
        self.0
            .borrow_mut()
            .stderr
            .push_str(&String::from_utf8_lossy(text));
    }
}

pub struct Outcome {
    pub state: State,
    pub exit_code: i32,
    pub out: Captured,
    /// The virtual milliseconds the run took.
    pub elapsed_ms: f64,
}

/// What a thread that runs a [`Runner`] natively needs: more than the engine may use of it (the
/// wasm stack is `STACK_SIZE`), as the page's stack is.
const STACK: usize = 64 * 1024 * 1024;

/// [`run_here`] on a thread with a stack as big as the web runner's budget needs.
pub fn run_pack(bytes: &[u8]) -> Outcome {
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .stack_size(STACK)
            .spawn_scoped(scope, || run_here(bytes))
            .expect("a thread")
            .join()
            .unwrap_or_else(|e| std::panic::resume_unwind(e))
    })
}

/// Runs a pack to its end: a tick on the next frame while a coroutine is due or polls, and after
/// the time to the next deadline otherwise.
fn run_here(bytes: &[u8]) -> Outcome {
    let out = Rc::new(RefCell::new(Captured::default()));
    let mut runner = Runner::start(bytes, Box::new(Capture(Rc::clone(&out))));
    let (mut elapsed, mut ticks) = (0.0, 0);
    while runner.state() == State::Running {
        let deadline = runner.next_deadline();
        assert!(deadline >= 0.0, "a running program that nothing will wake");
        let step = if deadline <= FRAME_MS {
            FRAME_MS
        } else {
            deadline.ceil()
        };
        elapsed += step;
        ticks += 1;
        assert!(ticks < 1_000_000, "the program does not end");
        runner.tick(step / 1000.0);
    }
    let code = runner.exit_code();
    let state = runner.state();
    drop(runner);
    let out = Rc::try_unwrap(out)
        .map_err(|_| ())
        .expect("the runner is gone")
        .into_inner();
    Outcome {
        state,
        exit_code: code,
        out,
        elapsed_ms: elapsed,
    }
}
