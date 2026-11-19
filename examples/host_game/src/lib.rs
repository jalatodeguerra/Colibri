//! A headless game loop whose gameplay is written in Colibri.
//!
//! The host (this file) owns the world: the enemies, the player, the timers and the clock. The
//! script decides what happens: when enemies appear, how they move, what a hit means and what
//! counts as a point. The loop runs a fixed 60 ticks per second of simulated time, with no wall
//! clock and no randomness, so the log is the same on every run (the tests compare it with a
//! golden file).

use std::path::{Path, PathBuf};

use colibri::{Callback, Ctx, Handle, Vm};
use colibri_runtime as colibri;

/// Ticks per second of simulated time.
pub const TICKS_PER_SECOND: u64 = 60;
/// How many ticks `main` runs unless told otherwise: ten seconds.
pub const DEFAULT_TICKS: u64 = 600;
/// The steps one entry from the host (`update`, a timer, a hook) may take.
pub const STEP_BUDGET: u64 = 20_000;

/// The turret at the player's position fires this often, this far, for this much damage.
const FIRE_EVERY: u64 = 30;
const FIRE_RANGE: f32 = 8.0;
const FIRE_DAMAGE: i64 = 25;

/// A point in the plane. A copy type: a Colibri `struct` that crosses by value.
#[colibri::export(value)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Vec2 {
    pub x: f32,
    pub y: f32,
}

impl Vec2 {
    fn distance(self, other: Vec2) -> f32 {
        ((self.x - other.x).powi(2) + (self.y - other.y).powi(2)).sqrt()
    }

    fn toward(self, target: Vec2, step: f32) -> Vec2 {
        let d = self.distance(target);
        if d <= step {
            return target;
        }
        Vec2 {
            x: self.x + (target.x - self.x) / d * step,
            y: self.y + (target.y - self.y) / d * step,
        }
    }
}

/// An enemy. The host owns it: scripts hold a handle, and `despawn` destroys the object.
#[colibri::export]
pub struct Enemy {
    id: i64,
    pos: Vec2,
    hp: i64,
}

#[colibri::export]
impl Enemy {
    pub fn id(&self) -> i64 {
        self.id
    }

    pub fn pos(&self) -> Vec2 {
        self.pos
    }

    pub fn set_pos(&mut self, p: Vec2) {
        self.pos = p;
    }

    pub fn hp(&self) -> i64 {
        self.hp
    }
}

struct Timer {
    due: u64,
    callback: Callback<(), ()>,
}

/// Everything the host functions reach through `Ctx`: the Vm keeps it.
struct World {
    tick: u64,
    player: Vec2,
    next_id: i64,
    enemies: Vec<Handle<Enemy>>,
    timers: Vec<Timer>,
    log: Vec<String>,
}

impl World {
    fn say(&mut self, message: impl std::fmt::Display) {
        self.log.push(format!("[{:04}] {message}", self.tick));
    }
}

/// The four places enemies can come from.
const SPAWN_POINTS: [Vec2; 4] = [
    Vec2 { x: 12.0, y: 0.0 },
    Vec2 { x: 0.0, y: 12.0 },
    Vec2 { x: -12.0, y: 0.0 },
    Vec2 { x: 0.0, y: -12.0 },
];

/// The hooks a script may define. Only `update` is required.
struct Hooks {
    init: Option<colibri::Fn<(), ()>>,
    update: colibri::Fn<(f32,), ()>,
    on_hit: Option<colibri::Fn<(Handle<Enemy>, i64), ()>>,
    score: Option<colibri::Fn<(), i64>>,
}

/// One run of the example: a `Vm` with the host API registered, and a script to drive.
pub struct Game {
    vm: Vm,
    hooks: Option<Hooks>,
    max_steps: u64,
    hits: u64,
    script_errors: u64,
}

/// The directory with the example's scripts and `host.cbi`.
pub fn scripts_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts")
}

impl Game {
    /// A game with the host API registered and no script yet.
    pub fn new() -> Result<Game, colibri::Error> {
        let mut vm = Vm::builder().instruction_budget(STEP_BUDGET).build();
        vm.insert_state(World {
            tick: 0,
            player: Vec2 { x: 0.0, y: 0.0 },
            next_id: 1,
            enemies: Vec::new(),
            timers: Vec::new(),
            log: Vec::new(),
        });
        register_host_api(&mut vm)?;
        Ok(Game {
            vm,
            hooks: None,
            max_steps: 0,
            hits: 0,
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

    fn find_hooks(&mut self) -> Result<(), colibri::Error> {
        self.hooks = Some(Hooks {
            init: self.vm.find_fn("init")?,
            update: self.vm.get_fn("update")?,
            on_hit: self.vm.find_fn("on_hit")?,
            score: self.vm.find_fn("get_score")?,
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
        lines.append(&mut self.world().log);
        let score = self.script_score();
        let world = self.world();
        let alive = world.enemies.len();
        lines.push(format!(
            "summary: ticks={ticks} spawned={} alive={alive} hits={} score={score} script_errors={} max_steps={}",
            world.next_id - 1,
            self.hits,
            self.script_errors,
            self.max_steps
        ));
        lines
    }

    fn world(&mut self) -> &mut World {
        self.vm
            .state_mut::<World>()
            .expect("the world is inserted in `new`")
    }

    fn script_score(&mut self) -> i64 {
        let score = self.hooks.as_ref().and_then(|h| h.score);
        match score {
            Some(f) => f.call(&mut self.vm, ()).unwrap_or(0),
            None => 0,
        }
    }

    /// Records the steps of one entry into the script, and turns a failure (a panic, or the
    /// budget running out) into a log line: the game goes on.
    fn after_entry(&mut self, what: &str, result: Result<(), colibri::Error>) {
        self.max_steps = self.max_steps.max(self.vm.steps_used());
        if let Err(e) = result {
            self.script_errors += 1;
            let first = e.to_string();
            let first = first.lines().next().unwrap_or("");
            self.world().say(format!("script error in {what}: {first}"));
        }
    }

    fn tick(&mut self) {
        self.world().tick += 1;
        self.fire_timers();
        let update = self.hooks.as_ref().map(|h| h.update);
        if let Some(update) = update {
            let result = update.call(&mut self.vm, (1.0 / TICKS_PER_SECOND as f32,));
            self.after_entry("update", result);
        }
        self.turret();
    }

    fn fire_timers(&mut self) {
        let world = self.world();
        let now = world.tick;
        let (mut due, later): (Vec<Timer>, Vec<Timer>) = std::mem::take(&mut world.timers)
            .into_iter()
            .partition(|t| t.due <= now);
        world.timers = later;
        due.sort_by_key(|t| t.due);
        for timer in due {
            let result = timer.callback.call(&mut self.vm, ());
            self.after_entry("timer", result);
        }
    }

    /// The turret shoots the nearest enemy in range every `FIRE_EVERY` ticks, and tells the
    /// script's `on_hit` hook, if it has one.
    fn turret(&mut self) {
        if !self.world().tick.is_multiple_of(FIRE_EVERY) {
            return;
        }
        let Some(target) = self.nearest_in_range() else {
            return;
        };
        let (id, hp) = {
            let mut enemy = self
                .vm
                .get_mut(target)
                .expect("the world lists live enemies");
            enemy.hp -= FIRE_DAMAGE;
            (enemy.id, enemy.hp)
        };
        self.hits += 1;
        self.world()
            .say(format!("host: hit #{id} for {FIRE_DAMAGE}, hp {hp}"));
        let on_hit = self.hooks.as_ref().and_then(|h| h.on_hit);
        if let Some(on_hit) = on_hit {
            let result = on_hit.call(&mut self.vm, (target, FIRE_DAMAGE));
            self.after_entry("on_hit", result);
        }
    }

    fn nearest_in_range(&mut self) -> Option<Handle<Enemy>> {
        let player = self.world().player;
        let handles = self.world().enemies.clone();
        let mut best: Option<(f32, Handle<Enemy>)> = None;
        for h in handles {
            let d = self.vm.get(h).ok()?.pos.distance(player);
            // `enemies` is in spawn order, so a tie keeps the oldest.
            if d <= FIRE_RANGE && best.is_none_or(|(bd, _)| d < bd) {
                best = Some((d, h));
            }
        }
        best.map(|(_, h)| h)
    }
}

fn register_host_api(vm: &mut Vm) -> Result<(), colibri::Error> {
    vm.register_type::<Vec2>()?;
    vm.register_type::<Enemy>()?;

    vm.register_fn_named("log", ["message"], |cx: &mut Ctx, message: String| {
        cx.state_mut::<World>()
            .expect("world")
            .say(format!("script: {message}"));
    })?;
    vm.register_fn_named("player", [], |cx: &mut Ctx| {
        cx.state_mut::<World>().expect("world").player
    })?;
    vm.register_fn_named(
        "spawn_point",
        ["index"],
        |index: i64| -> Result<Vec2, String> {
            usize::try_from(index)
                .ok()
                .and_then(|i| SPAWN_POINTS.get(i).copied())
                .ok_or_else(|| format!("there is no spawn point {index}"))
        },
    )?;
    vm.register_fn_named(
        "move_toward",
        ["from", "to", "step"],
        |from: Vec2, to: Vec2, step: f32| from.toward(to, step),
    )?;
    vm.register_fn_named("distance", ["a", "b"], |a: Vec2, b: Vec2| a.distance(b))?;
    vm.register_fn_named("spawn_enemy", ["pos"], |cx: &mut Ctx, pos: Vec2| {
        let world = cx.state_mut::<World>().expect("world");
        let id = world.next_id;
        world.next_id += 1;
        let handle = cx.insert(Enemy {
            id,
            pos,
            hp: 50 + 25 * (id % 3),
        });
        let world = cx.state_mut::<World>().expect("world");
        world.enemies.push(handle);
        world.say(format!(
            "host: spawned #{id} at ({:.1}, {:.1})",
            pos.x, pos.y
        ));
        handle
    })?;
    vm.register_fn_named("enemies", [], |cx: &mut Ctx| {
        cx.state_mut::<World>().expect("world").enemies.clone()
    })?;
    vm.register_fn_named(
        "despawn",
        ["e"],
        |cx: &mut Ctx, e: Handle<Enemy>| -> Result<(), String> {
            let enemy = cx
                .remove(e)
                .map_err(|e| e.to_string())?
                .ok_or("that enemy is already gone")?;
            let world = cx.state_mut::<World>().expect("world");
            world.enemies.retain(|h| *h != e);
            world.say(format!("host: despawned #{}", enemy.id));
            Ok(())
        },
    )?;
    vm.register_fn_named(
        "on_timer",
        ["secs", "f"],
        |cx: &mut Ctx, secs: f32, f: Callback<(), ()>| {
            let world = cx.state_mut::<World>().expect("world");
            let ticks = ((secs * TICKS_PER_SECOND as f32).round() as u64).max(1);
            let due = world.tick + ticks;
            world.timers.push(Timer { due, callback: f });
        },
    )?;
    Ok(())
}
