//! The C ABI of `colibri.wasm` (see the crate docs). One program runs at a time, on one thread.

use std::cell::RefCell;

use crate::runner::{Runner, Sink, State};

#[link(wasm_import_module = "env")]
unsafe extern "C" {
    /// Text for the page: `fd` 1 is standard output and 2 the error stream.
    fn host_write(fd: i32, ptr: *const u8, len: usize);
}

fn write(fd: i32, bytes: &[u8]) {
    if !bytes.is_empty() {
        // SAFETY: the host reads `len` bytes at `ptr` during the call and keeps none of them.
        unsafe { host_write(fd, bytes.as_ptr(), bytes.len()) }
    }
}

/// Output goes straight to the host.
struct HostSink;

impl Sink for HostSink {
    fn stdout(&mut self, text: &[u8]) {
        write(1, text);
    }

    fn stderr(&mut self, text: &[u8]) {
        write(2, text);
    }
}

thread_local! {
    static RUNNER: RefCell<Option<Runner>> = const { RefCell::new(None) };
}

fn code(state: State) -> i32 {
    match state {
        State::Running => 0,
        State::Done => 1,
        State::Failed => 2,
    }
}

/// A panic cannot unwind on this target: it ends the instance. What the page learns of it is
/// this text on the error stream.
fn report_panics() {
    std::panic::set_hook(Box::new(|info| {
        write(2, format!("internal error: {info}\n").as_bytes());
    }));
}

/// Memory of `len` bytes for the host to fill (a pack). Give it back with [`colibri_free`].
#[unsafe(no_mangle)]
pub extern "C" fn colibri_alloc(len: usize) -> *mut u8 {
    let mut bytes = Vec::<u8>::with_capacity(len.max(1));
    let ptr = bytes.as_mut_ptr();
    std::mem::forget(bytes);
    ptr
}

/// Gives back memory of [`colibri_alloc`] (the same `len`).
///
/// # Safety
/// `ptr` and `len` are what [`colibri_alloc`] gave and was asked for, and are not used again.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn colibri_free(ptr: *mut u8, len: usize) {
    // SAFETY: the caller gives back what `colibri_alloc` made, with the capacity it made it with.
    drop(unsafe { Vec::from_raw_parts(ptr, 0, len.max(1)) });
}

/// Loads the pack at `ptr` (`len` bytes, which the host has written) and starts the program, which
/// runs until its first pause. The bytes are copied: the host may free them at once.
///
/// # Safety
/// `ptr` points at `len` readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn colibri_start(ptr: *const u8, len: usize) -> i32 {
    report_panics();
    // SAFETY: the host wrote `len` bytes at `ptr`.
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
    let runner = Runner::start(bytes, Box::new(HostSink));
    let state = runner.state();
    RUNNER.with(|slot| *slot.borrow_mut() = Some(runner));
    code(state)
}

/// `dt` seconds have passed: runs the coroutines that are due.
#[unsafe(no_mangle)]
pub extern "C" fn colibri_tick(dt: f64) -> i32 {
    RUNNER.with(|slot| {
        slot.borrow_mut()
            .as_mut()
            .map_or(code(State::Failed), |runner| code(runner.tick(dt)))
    })
}

/// Milliseconds until the next tick is needed, `0` for one that is due now, `-1` for none.
#[unsafe(no_mangle)]
pub extern "C" fn colibri_next_deadline() -> f64 {
    RUNNER.with(|slot| {
        slot.borrow_mut()
            .as_mut()
            .map_or(-1.0, |runner| runner.next_deadline())
    })
}

/// `0` for a program that ended normally, `101` after a panic, `1` after another failure.
#[unsafe(no_mangle)]
pub extern "C" fn colibri_exit_code() -> i32 {
    RUNNER.with(|slot| slot.borrow().as_ref().map_or(1, Runner::exit_code))
}
