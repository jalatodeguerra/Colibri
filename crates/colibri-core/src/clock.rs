//! Where scheduler time comes from: the clock behind `wait_real` and the event loop.

use std::time::Duration;
#[cfg(not(target_family = "wasm"))]
use std::time::Instant;

/// Where scheduler time comes from, for `wait_real` and for the event loop (`colibri run`).
pub trait Clock {
    /// Seconds since an arbitrary, fixed origin.
    fn now(&mut self) -> f64;
    /// Waits for `seconds` (a fake clock just moves forward).
    fn sleep(&mut self, seconds: f64);
    /// Waits for up to `seconds`, but returns early when `wake(timeout)` reports that an
    /// external operation finished (`wake` blocks for up to `timeout` real time and says
    /// whether something arrived). The event loop calls this instead of `sleep` while
    /// background operations are pending (ARCHITECTURE section 22). The default is for fake
    /// clocks: it waits for real time and **does not move the clock**, so what a program
    /// does at a given fake time does not depend on how long an operation really takes (a
    /// coroutine that counts `wait(0.01)` while a program runs sees the same count every
    /// time). The cost: timers wait for background operations to end, so an operation that
    /// needs a later timer of the same program to fire cannot finish under such a clock;
    /// a clock that wants timers to run meanwhile overrides this.
    fn sleep_interruptible(&mut self, _seconds: f64, wake: &mut dyn FnMut(Duration) -> bool) {
        wake(Duration::from_millis(20));
    }
}

/// The wall clock.
///
/// On the web (`target_family = "wasm"`) `std::time::Instant` panics and a thread cannot sleep,
/// so this is a [`SourceClock`] on the host's `host_now_real` import.
#[cfg(not(target_family = "wasm"))]
pub struct SystemClock {
    origin: Instant,
}

#[cfg(not(target_family = "wasm"))]
impl SystemClock {
    pub fn new() -> Self {
        SystemClock {
            origin: Instant::now(),
        }
    }
}

#[cfg(not(target_family = "wasm"))]
impl Clock for SystemClock {
    fn now(&mut self) -> f64 {
        self.origin.elapsed().as_secs_f64()
    }
    fn sleep(&mut self, seconds: f64) {
        if seconds > 0.0 {
            std::thread::sleep(clamp_sleep(seconds));
        }
    }
    fn sleep_interruptible(&mut self, seconds: f64, wake: &mut dyn FnMut(Duration) -> bool) {
        if seconds > 0.0 {
            wake(clamp_sleep(seconds));
        }
    }
}

/// The wall clock of the web build: a [`SourceClock`] on the `host_now_real` import.
#[cfg(target_family = "wasm")]
pub struct SystemClock(SourceClock);

#[cfg(target_family = "wasm")]
impl SystemClock {
    pub fn new() -> Self {
        SystemClock(SourceClock::new(web::host_now_real_ms))
    }
}

#[cfg(target_family = "wasm")]
impl Clock for SystemClock {
    fn now(&mut self) -> f64 {
        self.0.now()
    }
    fn sleep(&mut self, seconds: f64) {
        self.0.sleep(seconds);
    }
    fn sleep_interruptible(&mut self, seconds: f64, wake: &mut dyn FnMut(Duration) -> bool) {
        self.0.sleep_interruptible(seconds, wake);
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

/// Where a host without `std::time` gets real time: a function that returns milliseconds on a
/// monotonic clock with any fixed origin (`performance.now()` in a browser).
pub type TimeSource = fn() -> f64;

/// A clock that reads a [`TimeSource`]. It is the web build's [`SystemClock`], and it takes any
/// source, so a host (and a native test) can drive it.
///
/// It cannot wait: with one thread that the host owns, blocking would freeze the page, so
/// `sleep` and `sleep_interruptible` return at once. The web runner drives the event loop from
/// its own ticks and timers and does not sleep through this clock; code that did would find
/// that time moves only as the source moves.
pub struct SourceClock {
    source: TimeSource,
    origin_ms: f64,
}

impl SourceClock {
    /// A clock whose zero is the source's reading now.
    pub fn new(source: TimeSource) -> Self {
        SourceClock {
            source,
            origin_ms: source(),
        }
    }
}

impl Clock for SourceClock {
    fn now(&mut self) -> f64 {
        // A source that steps back (a host bug) must not make time run backwards.
        ((self.source)() - self.origin_ms).max(0.0) / 1000.0
    }
    fn sleep(&mut self, _seconds: f64) {}
    fn sleep_interruptible(&mut self, _seconds: f64, _wake: &mut dyn FnMut(Duration) -> bool) {}
}

/// Measures real elapsed time for the profiler and the idle count. `std::time::Instant` on a
/// native build, the host's `host_now_real` import on the web.
#[derive(Clone, Copy)]
pub struct Stopwatch {
    #[cfg(not(target_family = "wasm"))]
    began: Instant,
    #[cfg(target_family = "wasm")]
    began_ms: f64,
}

impl Stopwatch {
    pub fn start() -> Self {
        #[cfg(not(target_family = "wasm"))]
        {
            Stopwatch {
                began: Instant::now(),
            }
        }
        #[cfg(target_family = "wasm")]
        {
            Stopwatch {
                began_ms: web::host_now_real_ms(),
            }
        }
    }

    /// The nanoseconds since [`Stopwatch::start`], saturating.
    pub fn elapsed_nanos(&self) -> u64 {
        #[cfg(not(target_family = "wasm"))]
        {
            u64::try_from(self.began.elapsed().as_nanos()).unwrap_or(u64::MAX)
        }
        #[cfg(target_family = "wasm")]
        {
            ms_to_nanos(web::host_now_real_ms() - self.began_ms)
        }
    }
}

/// Milliseconds as whole nanoseconds; a step back or a NaN is zero, and a huge value saturates.
#[cfg_attr(not(target_family = "wasm"), allow(dead_code))]
pub(crate) fn ms_to_nanos(ms: f64) -> u64 {
    // The cast saturates, and turns a NaN into zero.
    if ms <= 0.0 {
        0
    } else {
        (ms * 1_000_000.0) as u64
    }
}

/// The imports the web build needs from its host (the module `env`). The web runner
/// (`colibri-web`) defines them in JavaScript; the engine crates only declare them.
#[cfg(target_family = "wasm")]
mod web {
    #[link(wasm_import_module = "env")]
    unsafe extern "C" {
        /// Milliseconds on a monotonic clock (`performance.now()`); the origin is the host's.
        fn host_now_real() -> f64;
    }

    pub(super) fn host_now_real_ms() -> f64 {
        // SAFETY: the import takes no arguments and has no preconditions.
        unsafe { host_now_real() }
    }
}

/// A sleep length that cannot overflow `Duration` (an event loop waiting only for background
/// operations asks for an unbounded sleep; it is cut to a day and asked again).
#[cfg(not(target_family = "wasm"))]
pub(crate) fn clamp_sleep(seconds: f64) -> Duration {
    Duration::from_secs_f64(seconds.clamp(0.0, 86_400.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// The reading of the fake host clock, in microseconds (an integer, so an atomic holds it).
    static HOST_US: AtomicU64 = AtomicU64::new(0);

    fn host_ms() -> f64 {
        HOST_US.load(Ordering::SeqCst) as f64 / 1000.0
    }

    /// One test owns `HOST_US`: tests run in parallel and the source is a plain `fn`.
    #[test]
    fn a_source_clock_counts_seconds_from_its_own_zero_and_never_runs_backwards() {
        HOST_US.store(5_000_000, Ordering::SeqCst); // the host's origin is not ours: 5000 ms
        let mut clock = SourceClock::new(host_ms);
        assert_eq!(clock.now(), 0.0);
        HOST_US.store(7_500_000, Ordering::SeqCst);
        assert_eq!(clock.now(), 2.5);
        // A host whose clock steps back does not make time negative.
        HOST_US.store(1_000_000, Ordering::SeqCst);
        assert_eq!(clock.now(), 0.0);
        // Neither sleep waits (this test would hang for an hour if one did), and an interruptible sleep does not ask the pool to wake it.
        clock.sleep(3600.0);
        let mut woken = false;
        clock.sleep_interruptible(3600.0, &mut |_| {
            woken = true;
            true
        });
        assert!(!woken);
    }

    #[test]
    fn milliseconds_become_nanoseconds_without_going_negative() {
        assert_eq!(ms_to_nanos(1.5), 1_500_000);
        assert_eq!(ms_to_nanos(0.0), 0);
        assert_eq!(ms_to_nanos(-3.0), 0);
        assert_eq!(ms_to_nanos(f64::NAN), 0);
        assert_eq!(ms_to_nanos(f64::INFINITY), u64::MAX);
    }
}
