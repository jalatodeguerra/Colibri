//! A headless job runner whose job logic is written in Colibri.
//!
//! The host (this file) owns the jobs, the worker, the timers and the clock. The script decides
//! what happens: when jobs are submitted, how their load drains, what finishing a job means and
//! what it bills. The scheduler advances in fixed ticks (60 per second of simulated time), with
//! no wall clock and no randomness, so the log is the same on every run (the tests compare it
//! with a golden file).

use std::path::{Path, PathBuf};

use colibri::{Callback, Ctx, Handle, Vm};
use colibri_runtime as colibri;

/// Scheduler ticks per second of simulated time.
pub const TICKS_PER_SECOND: u64 = 60;
/// How many ticks `main` runs unless told otherwise: ten seconds.
pub const DEFAULT_TICKS: u64 = 600;
/// The steps one entry from the host (`update`, a timer, a hook) may take.
pub const STEP_BUDGET: u64 = 20_000;

/// The worker picks up the most urgent job every `WORK_EVERY` ticks, if its load is within
/// `WORK_RANGE` of the idle baseline, and does `WORK_UNITS` units of work on it.
const WORK_EVERY: u64 = 30;
const WORK_RANGE: f32 = 8.0;
const WORK_UNITS: i64 = 25;

/// The resources a job asks for. A copy type: a Colibri `struct` that crosses by value.
#[colibri::export(value)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Load {
    pub cpu: f32,
    pub io: f32,
}

impl Load {
    fn gap(self, other: Load) -> f32 {
        ((self.cpu - other.cpu).powi(2) + (self.io - other.io).powi(2)).sqrt()
    }

    fn ease(self, target: Load, step: f32) -> Load {
        let d = self.gap(target);
        if d <= step {
            return target;
        }
        Load {
            cpu: self.cpu + (target.cpu - self.cpu) / d * step,
            io: self.io + (target.io - self.io) / d * step,
        }
    }
}

/// A job. The host owns it: scripts hold a handle, and `finish` destroys the object.
#[colibri::export]
pub struct Job {
    id: i64,
    load: Load,
    remaining: i64,
}

#[colibri::export]
impl Job {
    pub fn id(&self) -> i64 {
        self.id
    }

    pub fn load(&self) -> Load {
        self.load
    }

    pub fn set_load(&mut self, l: Load) {
        self.load = l;
    }

    pub fn remaining(&self) -> i64 {
        self.remaining
    }
}

struct Timer {
    due: u64,
    callback: Callback<(), ()>,
}

/// Everything the host functions reach through `Ctx`: the Vm keeps it.
struct Queue {
    tick: u64,
    baseline: Load,
    next_id: i64,
    jobs: Vec<Handle<Job>>,
    timers: Vec<Timer>,
    log: Vec<String>,
}

impl Queue {
    fn new() -> Queue {
        Queue {
            tick: 0,
            baseline: Load { cpu: 0.0, io: 0.0 },
            next_id: 1,
            jobs: Vec::new(),
            timers: Vec::new(),
            log: Vec::new(),
        }
    }

    fn say(&mut self, message: impl std::fmt::Display) {
        self.log.push(format!("[{:04}] {message}", self.tick));
    }
}

/// The four load templates a job can start from.
const TEMPLATES: [Load; 4] = [
    Load { cpu: 12.0, io: 0.0 },
    Load { cpu: 0.0, io: 12.0 },
    Load {
        cpu: -12.0,
        io: 0.0,
    },
    Load {
        cpu: 0.0,
        io: -12.0,
    },
];

/// The hooks a script may define. Only `update` is required.
struct Hooks {
    init: Option<colibri::Fn<(), ()>>,
    update: colibri::Fn<(f32,), ()>,
    on_progress: Option<colibri::Fn<(Handle<Job>, i64), ()>>,
    balance: Option<colibri::Fn<(), i64>>,
}

/// One run of the example: a `Vm` with the host API registered, and a script to drive.
pub struct JobHost {
    vm: Vm,
    hooks: Option<Hooks>,
    max_steps: u64,
    worked: u64,
    script_errors: u64,
}

/// A script test's name and how it ended.
pub type TestOutcome = (String, Result<(), colibri::Error>);

/// The directory with the example's scripts and `host.cbi`.
pub fn scripts_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts")
}

impl JobHost {
    /// A host with the host API registered and no script yet.
    pub fn new() -> Result<JobHost, colibri::Error> {
        let mut vm = Vm::builder().instruction_budget(STEP_BUDGET).build();
        vm.insert_state(Queue::new());
        // A script test gets fresh module state, but the queue lives in the `Vm`: put it back
        // to empty before every test, and destroy the jobs the last test left behind.
        vm.on_test_start(|vm, _test| {
            let old = std::mem::replace(
                vm.state_mut::<Queue>()
                    .expect("the queue is inserted in `new`"),
                Queue::new(),
            );
            for job in old.jobs {
                let _ = vm.remove(job);
            }
        });
        register_host_api(&mut vm)?;
        Ok(JobHost {
            vm,
            hooks: None,
            max_steps: 0,
            worked: 0,
            script_errors: 0,
        })
    }

    /// The `.cbi` text of the host API: what `colibri check --interface` reads.
    pub fn interface_text(&self) -> String {
        self.vm.interface_text()
    }

    /// Writes the interface file unless it is already current.
    pub fn write_interface(&self, path: impl AsRef<Path>) -> Result<bool, colibri::Error> {
        self.vm.write_interface(path)
    }

    pub fn load_file(&mut self, path: impl AsRef<Path>) -> Result<(), colibri::Error> {
        self.vm.load_file(path)?;
        self.find_hooks()
    }

    pub fn load_source(&mut self, name: &str, source: &str) -> Result<(), colibri::Error> {
        self.vm.load_source(name, source)?;
        self.find_hooks()
    }

    /// Loads the script's `test` blocks from `path` (a script file or a folder) and runs each
    /// one against the host API, returning the name of every test with its outcome. The queue is
    /// reset before each test, so the tests do not see each other's jobs or ids.
    pub fn run_script_tests(
        &mut self,
        path: impl AsRef<Path>,
    ) -> Result<Vec<TestOutcome>, colibri::Error> {
        self.vm.load_tests(path)?;
        let mut results = Vec::new();
        for test in self.vm.tests() {
            results.push((test.name.clone(), self.vm.run_test(test.id)));
        }
        Ok(results)
    }

    fn find_hooks(&mut self) -> Result<(), colibri::Error> {
        self.hooks = Some(Hooks {
            init: self.vm.find_fn("init")?,
            update: self.vm.get_fn("update")?,
            on_progress: self.vm.find_fn("on_progress")?,
            balance: self.vm.find_fn("get_balance")?,
        });
        Ok(())
    }

    /// Runs `ticks` ticks and returns the log, ending with a summary line.
    pub fn run(&mut self, ticks: u64) -> Vec<String> {
        let mut lines = Vec::new();
        let init = self.hooks.as_ref().and_then(|h| h.init);
        if let Some(init) = init {
            let result = init.call(&mut self.vm, ());
            self.after_entry("init", result);
        }
        for _ in 0..ticks {
            self.tick();
        }
        lines.append(&mut self.queue().log);
        let balance = self.script_balance();
        let queue = self.queue();
        let pending = queue.jobs.len();
        lines.push(format!(
            "summary: ticks={ticks} submitted={} pending={pending} worked={} balance={balance} script_errors={} max_steps={}",
            queue.next_id - 1,
            self.worked,
            self.script_errors,
            self.max_steps
        ));
        lines
    }

    fn queue(&mut self) -> &mut Queue {
        self.vm
            .state_mut::<Queue>()
            .expect("the queue is inserted in `new`")
    }

    fn script_balance(&mut self) -> i64 {
        let balance = self.hooks.as_ref().and_then(|h| h.balance);
        match balance {
            Some(f) => f.call(&mut self.vm, ()).unwrap_or(0),
            None => 0,
        }
    }

    /// Records the steps of one entry into the script, and turns a failure (a panic, or the
    /// budget running out) into a log line: the host goes on.
    fn after_entry(&mut self, what: &str, result: Result<(), colibri::Error>) {
        self.max_steps = self.max_steps.max(self.vm.steps_used());
        if let Err(e) = result {
            self.script_errors += 1;
            let first = e.to_string();
            let first = first.lines().next().unwrap_or("");
            self.queue().say(format!("script error in {what}: {first}"));
        }
    }

    /// Advances the scheduler by one tick: due timers fire, then the script's `update`, then the
    /// worker gets its turn.
    fn tick(&mut self) {
        self.queue().tick += 1;
        self.fire_timers();
        let update = self.hooks.as_ref().map(|h| h.update);
        if let Some(update) = update {
            let result = update.call(&mut self.vm, (1.0 / TICKS_PER_SECOND as f32,));
            self.after_entry("update", result);
        }
        self.work();
    }

    fn fire_timers(&mut self) {
        let queue = self.queue();
        let now = queue.tick;
        let (mut due, later): (Vec<Timer>, Vec<Timer>) = std::mem::take(&mut queue.timers)
            .into_iter()
            .partition(|t| t.due <= now);
        queue.timers = later;
        due.sort_by_key(|t| t.due);
        for timer in due {
            let result = timer.callback.call(&mut self.vm, ());
            self.after_entry("timer", result);
        }
    }

    /// The worker takes the most urgent job in range every `WORK_EVERY` ticks, and tells the
    /// script's `on_progress` hook, if it has one.
    fn work(&mut self) {
        if !self.queue().tick.is_multiple_of(WORK_EVERY) {
            return;
        }
        let Some(target) = self.most_urgent_in_range() else {
            return;
        };
        let (id, remaining) = {
            let mut job = self.vm.get_mut(target).expect("the queue lists live jobs");
            job.remaining -= WORK_UNITS;
            (job.id, job.remaining)
        };
        self.worked += 1;
        self.queue().say(format!(
            "host: worked #{id} for {WORK_UNITS}, remaining {remaining}"
        ));
        let on_progress = self.hooks.as_ref().and_then(|h| h.on_progress);
        if let Some(on_progress) = on_progress {
            let result = on_progress.call(&mut self.vm, (target, WORK_UNITS));
            self.after_entry("on_progress", result);
        }
    }

    fn most_urgent_in_range(&mut self) -> Option<Handle<Job>> {
        let baseline = self.queue().baseline;
        let handles = self.queue().jobs.clone();
        let mut best: Option<(f32, Handle<Job>)> = None;
        for h in handles {
            let d = self.vm.get(h).ok()?.load.gap(baseline);
            // `jobs` is in submission order, so a tie keeps the oldest.
            if d <= WORK_RANGE && best.is_none_or(|(bd, _)| d < bd) {
                best = Some((d, h));
            }
        }
        best.map(|(_, h)| h)
    }
}

fn register_host_api(vm: &mut Vm) -> Result<(), colibri::Error> {
    vm.register_type::<Load>()?;
    vm.register_type::<Job>()?;

    vm.register_fn_named("log", ["message"], |cx: &mut Ctx, message: String| {
        cx.state_mut::<Queue>()
            .expect("queue")
            .say(format!("script: {message}"));
    })?;
    vm.register_fn_named("baseline", [], |cx: &mut Ctx| {
        cx.state_mut::<Queue>().expect("queue").baseline
    })?;
    vm.register_fn_named(
        "template",
        ["index"],
        |index: i64| -> Result<Load, String> {
            usize::try_from(index)
                .ok()
                .and_then(|i| TEMPLATES.get(i).copied())
                .ok_or_else(|| format!("there is no load template {index}"))
        },
    )?;
    vm.register_fn_named(
        "ease",
        ["from", "to", "step"],
        |from: Load, to: Load, step: f32| from.ease(to, step),
    )?;
    vm.register_fn_named("gap", ["a", "b"], |a: Load, b: Load| a.gap(b))?;
    vm.register_fn_named("submit", ["load"], |cx: &mut Ctx, load: Load| {
        let queue = cx.state_mut::<Queue>().expect("queue");
        let id = queue.next_id;
        queue.next_id += 1;
        let handle = cx.insert(Job {
            id,
            load,
            remaining: 50 + 25 * (id % 3),
        });
        let queue = cx.state_mut::<Queue>().expect("queue");
        queue.jobs.push(handle);
        queue.say(format!(
            "host: submitted #{id} with load ({:.1}, {:.1})",
            load.cpu, load.io
        ));
        handle
    })?;
    vm.register_fn_named("jobs", [], |cx: &mut Ctx| {
        cx.state_mut::<Queue>().expect("queue").jobs.clone()
    })?;
    vm.register_fn_named(
        "finish",
        ["job"],
        |cx: &mut Ctx, job: Handle<Job>| -> Result<(), String> {
            let removed = cx
                .remove(job)
                .map_err(|e| e.to_string())?
                .ok_or("that job is already gone")?;
            let queue = cx.state_mut::<Queue>().expect("queue");
            queue.jobs.retain(|h| *h != job);
            queue.say(format!("host: finished #{}", removed.id));
            Ok(())
        },
    )?;
    vm.register_fn_named(
        "on_timer",
        ["secs", "f"],
        |cx: &mut Ctx, secs: f32, f: Callback<(), ()>| {
            let queue = cx.state_mut::<Queue>().expect("queue");
            let ticks = ((secs * TICKS_PER_SECOND as f32).round() as u64).max(1);
            let due = queue.tick + ticks;
            queue.timers.push(Timer { due, callback: f });
        },
    )?;
    Ok(())
}
