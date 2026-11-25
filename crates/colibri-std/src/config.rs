//! What the world looks like to scripts: the clock, the environment, the program's arguments,
//! standard input, the temporary folder and the default random seed. A host makes a run
//! hermetic by replacing them (the golden tests do, with a [`ManualClock`], a fixed
//! environment and a temp folder of their own).

use std::collections::BTreeMap;
use std::io::BufRead;
use std::path::PathBuf;
#[cfg(not(target_family = "wasm"))]
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::rand::Rng;

/// A source of time for `time` and `os`.
pub trait Clock {
    /// Nanoseconds on a monotonic clock, from an origin fixed when the clock was made. Never
    /// goes back.
    fn monotonic_nanos(&mut self) -> u64;
    /// Seconds since 1970-01-01 00:00 UTC on the wall clock. It can jump.
    fn unix_seconds(&mut self) -> f64;
    /// Blocks for `seconds` (not negative, finite).
    fn sleep(&mut self, seconds: f64);
}

/// The machine's clocks. This is the default.
///
/// On the web (`target_family = "wasm"`) `Instant`, `SystemTime` and a sleeping thread are not
/// there (they panic), so the clocks are the host's imports `host_now_real` (milliseconds on a
/// monotonic clock, `performance.now()`) and `host_now_unix` (seconds since 1970, `Date.now()`),
/// and `sleep` returns at once: nothing can block the page.
#[derive(Debug)]
pub struct SystemClock {
    #[cfg(not(target_family = "wasm"))]
    start: Instant,
    #[cfg(target_family = "wasm")]
    start_ms: f64,
}

impl SystemClock {
    pub fn new() -> Self {
        SystemClock {
            #[cfg(not(target_family = "wasm"))]
            start: Instant::now(),
            #[cfg(target_family = "wasm")]
            start_ms: web::now_real_ms(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        SystemClock::new()
    }
}

#[cfg(not(target_family = "wasm"))]
impl Clock for SystemClock {
    fn monotonic_nanos(&mut self) -> u64 {
        u64::try_from(self.start.elapsed().as_nanos()).unwrap_or(u64::MAX)
    }

    fn unix_seconds(&mut self) -> f64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0.0, |d| d.as_secs_f64())
    }

    fn sleep(&mut self, seconds: f64) {
        // `from_secs_f64` panics on a value too large to represent.
        let wait =
            Duration::try_from_secs_f64(seconds).unwrap_or(Duration::from_secs(u64::MAX / 4));
        std::thread::sleep(wait);
    }
}

#[cfg(target_family = "wasm")]
impl Clock for SystemClock {
    fn monotonic_nanos(&mut self) -> u64 {
        ms_to_nanos(web::now_real_ms() - self.start_ms)
    }

    fn unix_seconds(&mut self) -> f64 {
        web::now_unix_seconds()
    }

    fn sleep(&mut self, _seconds: f64) {}
}

/// Milliseconds as whole nanoseconds; a step back or a NaN is zero, and a huge value saturates.
#[cfg_attr(not(target_family = "wasm"), allow(dead_code))]
fn ms_to_nanos(ms: f64) -> u64 {
    if ms <= 0.0 { 0 } else { (ms * 1_000_000.0) as u64 }
}

/// The seed of a generator that was not given one, from `fill`: a source of random bytes (the
/// host's `crypto.getRandomValues` on the web).
#[cfg_attr(not(target_family = "wasm"), allow(dead_code))]
fn seed_from(fill: fn(&mut [u8])) -> u64 {
    let mut bytes = [0u8; 8];
    fill(&mut bytes);
    u64::from_le_bytes(bytes)
}

/// The seed of an unseeded generator: the host's random bytes on the web, whatever the clocks
/// say now elsewhere.
#[cfg(target_family = "wasm")]
fn default_seed(_clock: &mut dyn Clock) -> u64 {
    seed_from(web::fill_random)
}

#[cfg(not(target_family = "wasm"))]
fn default_seed(clock: &mut dyn Clock) -> u64 {
    clock.unix_seconds().to_bits() ^ clock.monotonic_nanos()
}

/// The imports of the web build (the module `env`), defined by the host page. `colibri-web`
/// documents them next to the others.
#[cfg(target_family = "wasm")]
mod web {
    #[link(wasm_import_module = "env")]
    unsafe extern "C" {
        fn host_now_real() -> f64;
        fn host_now_unix() -> f64;
        fn host_random(ptr: *mut u8, len: usize);
    }

    pub(super) fn now_real_ms() -> f64 {
        // SAFETY: no arguments, no preconditions.
        unsafe { host_now_real() }
    }

    pub(super) fn now_unix_seconds() -> f64 {
        // SAFETY: no arguments, no preconditions.
        unsafe { host_now_unix() }
    }

    pub(super) fn fill_random(bytes: &mut [u8]) {
        // SAFETY: the host writes `len` bytes at `ptr`, which `bytes` owns.
        unsafe { host_random(bytes.as_mut_ptr(), bytes.len()) }
    }
}

/// A clock that only moves when told to: `sleep` advances it by exactly the time asked, and
/// nothing else changes it. Tests and replays use it so that no run depends on the machine.
#[derive(Clone, Debug, Default)]
pub struct ManualClock {
    nanos: u64,
    unix: f64,
}

impl ManualClock {
    /// A clock at monotonic time zero whose wall clock reads `unix` seconds.
    pub fn new(unix: f64) -> Self {
        ManualClock { nanos: 0, unix }
    }

    /// Moves both clocks forward.
    pub fn advance(&mut self, seconds: f64) {
        let nanos = (seconds * 1e9).round();
        if nanos.is_finite() && nanos > 0.0 {
            // Saturates at `u64::MAX`.
            self.nanos = self.nanos.saturating_add(nanos as u64);
            self.unix += seconds;
        }
    }
}

impl Clock for ManualClock {
    fn monotonic_nanos(&mut self) -> u64 {
        self.nanos
    }

    fn unix_seconds(&mut self) -> f64 {
        self.unix
    }

    fn sleep(&mut self, seconds: f64) {
        self.advance(seconds);
    }
}

/// Where `os.env` reads from.
#[derive(Clone, Debug, Default)]
pub enum EnvSource {
    /// The process's own environment.
    #[default]
    Process,
    /// Exactly these variables, whatever the process has.
    Fixed(BTreeMap<String, String>),
}

/// Everything the standard library takes from the world, with the real thing as the default.
///
/// Build one with struct update syntax, naming only what differs, so that a field added later
/// (as `assets` was) does not break the code:
///
/// ```
/// let config = colibri_std::StdConfig {
///     args: vec!["one".to_string()],
///     ..colibri_std::StdConfig::default()
/// };
/// # let _ = config;
/// ```
pub struct StdConfig {
    pub clock: Box<dyn Clock>,
    /// What `os.args()` returns: the arguments of the script, not of the `colibri` tool.
    pub args: Vec<String>,
    pub env: EnvSource,
    /// What `os.temp_dir()` returns; the system's temporary folder if `None`.
    pub temp_dir: Option<PathBuf>,
    /// The seed of the generator behind `rand.int(..)` and friends. `None` seeds it from the
    /// system clock, so every run differs until the script calls `rand.seed(n)`.
    pub seed: Option<u64>,
    /// What `io.read_line()` reads; the process's standard input if `None`.
    pub stdin: Option<Box<dyn BufRead>>,
    /// The local time zone `datetime` uses, as seconds east of UTC (always this offset, no
    /// daylight saving); the machine's time zone if `None`.
    pub local_offset: Option<i32>,
    /// Where `assets.read` and friends find the program's assets; none if `None` (every read
    /// is `assets.Error.NotFound`).
    pub assets: Option<Box<dyn crate::AssetSource>>,
}

impl Default for StdConfig {
    fn default() -> Self {
        StdConfig {
            clock: Box::new(SystemClock::new()),
            args: Vec::new(),
            env: EnvSource::Process,
            temp_dir: None,
            seed: None,
            stdin: None,
            local_offset: None,
            assets: None,
        }
    }
}

/// The configuration plus what the modules change while a program runs. It is the host state
/// (`Vm::insert_state`) the functions reach through `Ctx::state_mut`.
#[cfg_attr(target_family = "wasm", allow(dead_code))]
pub(crate) struct State {
    pub clock: Box<dyn Clock>,
    pub args: Vec<String>,
    pub env: EnvSource,
    /// `os.set_env` writes here, never to the process: reads see it first.
    pub env_overrides: BTreeMap<String, String>,
    pub temp_dir: Option<PathBuf>,
    pub stdin: Option<Box<dyn BufRead>>,
    pub local_offset: Option<i32>,
    pub assets: Option<Box<dyn crate::AssetSource>>,
    /// The generator behind the free functions of `rand`.
    pub rng: Rng,
    /// The code of `os.exit`, once called.
    pub exit: Option<i32>,
}

impl State {
    pub fn new(config: StdConfig) -> State {
        let mut clock = config.clock;
        let seed = config.seed.unwrap_or_else(|| default_seed(&mut *clock));
        State {
            clock,
            args: config.args,
            env: config.env,
            env_overrides: BTreeMap::new(),
            temp_dir: config.temp_dir,
            stdin: config.stdin,
            local_offset: config.local_offset,
            assets: config.assets,
            rng: Rng::from_seed(seed),
            exit: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unseeded_generator_takes_its_seed_from_the_hosts_random_bytes() {
        let seed = seed_from(|bytes| bytes.copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]));
        assert_eq!(seed, 0x0807_0605_0403_0201);
        // The host's bytes decide the seed: different bytes, different sequence.
        let other = seed_from(|bytes| bytes.fill(9));
        assert_ne!(Rng::from_seed(seed).next_u64(), Rng::from_seed(other).next_u64());
    }

    #[test]
    fn milliseconds_become_nanoseconds_without_going_negative() {
        assert_eq!(ms_to_nanos(1.5), 1_500_000);
        assert_eq!(ms_to_nanos(-3.0), 0);
        assert_eq!(ms_to_nanos(f64::NAN), 0);
    }
}
