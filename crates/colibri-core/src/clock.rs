//! Where scheduler time comes from: the clock behind `wait_real` and the event loop.

use std::time::{Duration, Instant};

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
pub struct SystemClock {
    origin: Instant,
}

impl SystemClock {
    pub fn new() -> Self {
        SystemClock {
            origin: Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

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

/// A sleep length that cannot overflow `Duration` (an event loop waiting only for background
/// operations asks for an unbounded sleep; it is cut to a day and asked again).
pub(crate) fn clamp_sleep(seconds: f64) -> Duration {
    Duration::from_secs_f64(seconds.clamp(0.0, 86_400.0))
}
