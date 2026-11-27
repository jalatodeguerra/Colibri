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
    if ms <= 0.0 {
        0
    } else {
        (ms * 1_000_000.0) as u64
    }
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

/// The offset of the page's time zone at `unix` (whole seconds), in seconds east of UTC, from what
/// the host said: a number of seconds, or anything else (NaN, infinity, a day or more) for "UTC".
#[cfg_attr(not(target_family = "wasm"), allow(dead_code))]
pub(crate) fn offset_from_host(seconds: f64) -> i64 {
    // A time zone offset is under a day (the largest in use is 14 hours).
    if seconds.is_finite() && seconds.abs() < 86_400.0 {
        seconds.round() as i64
    } else {
        0
    }
}

/// The local time zone's offset at `unix`, as the page says (`host_utc_offset`).
#[cfg(target_family = "wasm")]
pub(crate) fn host_offset(unix: i64) -> i64 {
    offset_from_host(web::utc_offset_seconds(unix as f64))
}

/// The imports of the web build (the module `env`), defined by the host page. `colibri-web`
/// documents them next to the others.
#[cfg(target_family = "wasm")]
mod web {
    #[link(wasm_import_module = "env")]
    unsafe extern "C" {
        fn host_now_real() -> f64;
        fn host_now_unix() -> f64;
        fn host_utc_offset(unix: f64) -> f64;
        fn host_random(ptr: *mut u8, len: usize);
    }

    pub(super) fn utc_offset_seconds(unix: f64) -> f64 {
        // SAFETY: a number in, a number out; no preconditions.
        unsafe { host_utc_offset(unix) }
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
    /// The library of a web build: the functions a page cannot have are not registered
    /// (`time.sleep`). True by default on the web target (`target_family = "wasm"`), false
    /// elsewhere; `colibri build --web` checks a program against the interface it gives.
    pub web: bool,
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
            web: cfg!(target_family = "wasm"),
        }
    }
}

/// A factory that a worker thread calls to make its own copy of something that is not `Send`.
pub type Factory<T> = std::sync::Arc<dyn Fn() -> T + Send + Sync>;

/// What a thread task's worker sees of the world: the settings of the main `Vm`'s [`StdConfig`]
/// that can be handed to another thread, so that a task reads the same environment, arguments,
/// time zone, clock and assets and starts from the same seed as `main`. A host that builds its
/// `Vm` with a custom [`StdConfig`] builds the worker's from the same facts and gives it to
/// `worker_setup` (see `colibri-lang`'s `build_vm_with_worker_std`).
///
/// What a worker does not share with the main thread: standard input (a worker reads an empty
/// one, so it never competes with `main` or a debug adapter for the host's input), and the
/// state a program changes while it runs (`os.set_env`, the generator's position, a manual
/// clock's time): each worker has its own. A task's random numbers do not depend on the worker
/// that runs it: each task starts a generator of its own, seeded from the main seed and the task's
/// serial number (see [`task_seed`]), so two tasks do not repeat each other and a run repeats.
#[derive(Clone)]
pub struct WorkerStd {
    args: Vec<String>,
    env: EnvSource,
    temp_dir: Option<PathBuf>,
    seed: Option<u64>,
    local_offset: Option<i32>,
    web: bool,
    clock: Factory<Box<dyn Clock>>,
    assets: Option<Factory<Box<dyn crate::AssetSource>>>,
    stderr: Option<Factory<Box<dyn std::io::Write>>>,
}

impl WorkerStd {
    /// The plain settings of `config`, the machine's clock, no assets and the process's standard
    /// error: what a worker gets when the host says nothing more.
    pub fn from_config(config: &StdConfig) -> WorkerStd {
        WorkerStd {
            args: config.args.clone(),
            env: config.env.clone(),
            temp_dir: config.temp_dir.clone(),
            seed: config.seed,
            local_offset: config.local_offset,
            web: config.web,
            clock: std::sync::Arc::new(|| Box::new(SystemClock::new())),
            assets: None,
            stderr: None,
        }
    }

    /// Each worker's clock is made by `make` (a manual clock for a hermetic run).
    pub fn with_clock(mut self, make: impl Fn() -> Box<dyn Clock> + Send + Sync + 'static) -> Self {
        self.clock = std::sync::Arc::new(make);
        self
    }

    /// Each worker's assets are made by `make` (a source that reads the same program's assets).
    pub fn with_assets(
        mut self,
        make: impl Fn() -> Box<dyn crate::AssetSource> + Send + Sync + 'static,
    ) -> Self {
        self.assets = Some(std::sync::Arc::new(make));
        self
    }

    /// Where a worker's `io.eprint` writes, made by `make` for each worker.
    pub fn with_stderr(
        mut self,
        make: impl Fn() -> Box<dyn std::io::Write> + Send + Sync + 'static,
    ) -> Self {
        self.stderr = Some(std::sync::Arc::new(make));
        self
    }

    /// The main seed, which every task's own seed is derived from; `None` (a seed from the clock)
    /// leaves the generator alone.
    pub(crate) fn seed(&self) -> Option<u64> {
        self.seed
    }

    /// The configuration of one worker `Vm`.
    pub fn config(&self) -> StdConfig {
        StdConfig {
            clock: (self.clock)(),
            args: self.args.clone(),
            env: self.env.clone(),
            temp_dir: self.temp_dir.clone(),
            seed: self.seed,
            stdin: Some(Box::new(std::io::empty())),
            local_offset: self.local_offset,
            assets: self.assets.as_ref().map(|make| make()),
            web: self.web,
        }
    }

    /// Where the worker's `io.eprint` goes, if the host chose.
    pub(crate) fn stderr(&self) -> Option<Box<dyn std::io::Write>> {
        self.stderr.as_ref().map(|make| make())
    }
}

/// The seed of the generator that the task with this serial number draws from, given the main
/// seed: the same on every run, whichever worker runs the task.
pub(crate) fn task_seed(main: u64, serial: u64) -> u64 {
    main ^ serial.wrapping_mul(0x9E37_79B9_7F4A_7C15)
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
    /// Where `io.eprint` and `io.eprintln` write, when a host has taken it (`set_stderr`).
    pub stderr: Option<Box<dyn std::io::Write>>,
    pub local_offset: Option<i32>,
    pub assets: Option<Box<dyn crate::AssetSource>>,
    pub web: bool,
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
            stderr: None,
            local_offset: config.local_offset,
            assets: config.assets,
            web: config.web,
            rng: Rng::from_seed(seed),
            exit: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_workers_world_is_the_main_ones_but_for_its_input_and_its_seed() {
        use std::io::BufRead;
        let main = StdConfig {
            args: vec!["a".to_string()],
            env: EnvSource::Fixed(BTreeMap::from([("K".to_string(), "v".to_string())])),
            temp_dir: Some(PathBuf::from("/tmp/t")),
            seed: Some(7),
            stdin: Some(Box::new(std::io::Cursor::new("main input\n"))),
            local_offset: Some(3600),
            web: false,
            ..StdConfig::default()
        };
        let worker = WorkerStd::from_config(&main).with_clock(|| Box::new(ManualClock::new(5.0)));
        let first = worker.config();
        assert_eq!(first.args, ["a"]);
        assert!(matches!(&first.env, EnvSource::Fixed(m) if m["K"] == "v"));
        assert_eq!(first.temp_dir, Some(PathBuf::from("/tmp/t")));
        assert_eq!(first.local_offset, Some(3600));
        // The worker's clock is the one the host chose.
        let mut clock = first.clock;
        assert_eq!(clock.unix_seconds(), 5.0);
        // A worker never reads the main thread's input: it has an empty one of its own.
        let mut line = String::new();
        assert_eq!(first.stdin.unwrap().read_line(&mut line).unwrap(), 0);
        // The seed a task draws from depends on the main seed and the task's serial number, and
        // on nothing else: not on the worker, not on how many workers were built.
        assert_eq!(worker.seed(), Some(7));
        assert_eq!(task_seed(7, 3), task_seed(7, 3));
        assert_ne!(task_seed(7, 3), task_seed(7, 4));
        assert_ne!(task_seed(7, 3), task_seed(8, 3));
        let unseeded = WorkerStd::from_config(&StdConfig::default());
        assert_eq!(unseeded.seed(), None);
    }

    #[test]
    fn an_unseeded_generator_takes_its_seed_from_the_hosts_random_bytes() {
        let seed = seed_from(|bytes| bytes.copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]));
        assert_eq!(seed, 0x0807_0605_0403_0201);
        // The host's bytes decide the seed: different bytes, different sequence.
        let other = seed_from(|bytes| bytes.fill(9));
        assert_ne!(
            Rng::from_seed(seed).next_u64(),
            Rng::from_seed(other).next_u64()
        );
    }

    #[test]
    fn the_hosts_time_zone_offset_is_whole_seconds_within_a_day_or_utc() {
        assert_eq!(offset_from_host(3600.0), 3600);
        assert_eq!(offset_from_host(-19_800.0), -19_800);
        assert_eq!(offset_from_host(12_345.6), 12_346);
        for bad in [f64::NAN, f64::INFINITY, 86_400.0, -1e12] {
            assert_eq!(offset_from_host(bad), 0, "{bad}");
        }
    }

    #[test]
    fn milliseconds_become_nanoseconds_without_going_negative() {
        assert_eq!(ms_to_nanos(1.5), 1_500_000);
        assert_eq!(ms_to_nanos(-3.0), 0);
        assert_eq!(ms_to_nanos(f64::NAN), 0);
    }
}
