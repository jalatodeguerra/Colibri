//! What the world looks like to scripts: the clock, the environment, the program's arguments,
//! standard input, the temporary folder and the default random seed. A host makes a run
//! hermetic by replacing them (the golden tests do, with a [`ManualClock`], a fixed
//! environment and a temp folder of their own).

use std::collections::BTreeMap;
use std::io::BufRead;
use std::path::PathBuf;
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
#[derive(Debug)]
pub struct SystemClock {
    start: Instant,
}

impl SystemClock {
    pub fn new() -> Self {
        SystemClock {
            start: Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        SystemClock::new()
    }
}

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
        }
    }
}

/// The configuration plus what the modules change while a program runs. It is the host state
/// (`Vm::insert_state`) the functions reach through `Ctx::state_mut`.
pub(crate) struct State {
    pub clock: Box<dyn Clock>,
    pub args: Vec<String>,
    pub env: EnvSource,
    /// `os.set_env` writes here, never to the process: reads see it first.
    pub env_overrides: BTreeMap<String, String>,
    pub temp_dir: Option<PathBuf>,
    pub stdin: Option<Box<dyn BufRead>>,
    /// The generator behind the free functions of `rand`.
    pub rng: Rng,
    /// The code of `os.exit`, once called.
    pub exit: Option<i32>,
}

impl State {
    pub fn new(config: StdConfig) -> State {
        let mut clock = config.clock;
        let seed = config.seed.unwrap_or_else(|| {
            // Unseeded: whatever the clocks say now.
            clock.unix_seconds().to_bits() ^ clock.monotonic_nanos()
        });
        State {
            clock,
            args: config.args,
            env: config.env,
            env_overrides: BTreeMap::new(),
            temp_dir: config.temp_dir,
            stdin: config.stdin,
            rng: Rng::from_seed(seed),
            exit: None,
        }
    }
}
