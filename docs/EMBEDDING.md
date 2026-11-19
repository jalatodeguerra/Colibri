# Embedding API design (phase 1, DESIGN §15.4–15.5)

Implementation design for the embedding slice (DESIGN §17.5 phase 1 item 7), from an advisor
review. DESIGN.md is the source of truth for behaviour; this file fixes the shapes. Section
references (§) are to DESIGN.md.

## 0. Decisions that are expensive to change once the bytecode VM exists

- **A. Conversion traits take a context and borrow.**
  ```rust
  pub trait IntoColibri { fn ty(reg: &mut TypeRegistry) -> TypeDesc; fn into_colibri(self, cx: &mut Cx<'_>) -> Value; }
  pub trait FromColibri: Sized { fn ty(reg: &mut TypeRegistry) -> TypeDesc; fn from_colibri(v: &Value, cx: &mut Cx<'_>) -> Result<Self, Error>; }
  ```
  Host struct types need a per-Vm type lookup, the runtime has no global state, and the VM will
  allocate through its own heap. `from_colibri` borrows because heap objects must die only through
  the interpreter's `release` (Drop hooks, drop trace); arguments stay on the caller's temps and
  the trampoline passes `&[Value]`.
- **B. A structural `TypeDesc`** (scalars, `List`, `Option`, `Result`, `Tuple`, `Map`,
  `Fn(params, ret)`, `Host(name)`, value types) shared by `get_fn` and `register_fn`.
- **C. One non-generic host-call ABI:** `Rc<dyn Fn(&mut Ctx<'_>, &[Value]) -> Result<Value, HostFault>>`,
  indexed by `HostFnId`. Macro output uses only `colibri::__private`, so the VM can reimplement it.
- **D. Public handles are `(index, generation, vm id)`**, never `Rc` pointers.
- **E. The checker's only host input is a `HostEnv` built from parsed `.cbi` text** (§4 below).

## 1. `register_fn`

`register_fn` returns `Result<Registration, Error>`: a name clash or an unusable type is an error at
registration, and `Error::AlreadyLoaded` after `load_*`. Closures have no parameter names, so their
parameters are `a, b, c, ...` in the interface (marked `@positional`) and a call with named
arguments is a checker error.

```rust
vm.register_fn("spawn_enemy", |x: f32, y: f32| -> Enemy { ... })?;
vm.register_fn("damage", |cx: &mut colibri::Ctx, e: Handle<Enemy>, n: i32| -> Result<(), MyErr> { ... })?;
pub trait HostFn<Marker>: 'static { fn sig(reg: &mut TypeRegistry) -> SigDesc; fn into_raw(self) -> RawHostFn; }
```

- `HostFn` is implemented by macro for arities 0..=8, each with and without a leading `&mut Ctx`
  (marker types disambiguate). The signature is computed once at registration.
- Returns: `R: IntoColibri`. `Result<T, E: Display>` becomes Colibri `Result[T]` with the `Error`
  message from `E` (§15.5). A Rust panic becomes a Colibri panic. `Err` never becomes a panic.
- **Host state lives in the Vm**, reached through `Ctx`: `vm.insert_state(world)`,
  `vm.state_mut::<World>()` between calls, `cx.state_mut::<World>()` inside host functions. `Ctx`
  wraps the interpreter, fiber, host heap and state map, so a host function cannot hold
  `cx.state_mut()` across a reentrant `cx.call(..)` — checked by the borrow checker, not at runtime.
  Closures may still capture what they like (`'static`, not `Send`).
- **Reentry:** `trait Caller` implemented by `Vm` and `Ctx`; `Fn::call` and `Callback::call` take
  `&mut impl Caller`. `fiber.host_depth` is bumped around host calls now, so `wait` can panic in
  phase 2 (§14.2).
- **Namespacing:** host items are keyed by `(module, name)` everywhere. This slice ships the default
  module `host`, whose names are in scope without an import (the §15.4 example). Named modules
  (`vm.module("physics")` + `import physics`) are built (ARCHITECTURE §11.7), with no format
  change: each module has its own `.cbi`, and `colibri check --interface` takes several. A
  script definition colliding with a host name is an error. Host functions as values: checker
  error for now ("wrap it in a closure").
- Parameter names: closures get `a, b, c…` and reject named-argument calls; `#[colibri::export] fn`
  keeps real names.
- Registration after a load is `Error::AlreadyLoaded`.
- **Panics:** `catch_unwind(AssertUnwindSafe(..))` in the trampoline. Sound for the Vm because no
  interpreter `RefCell` borrow is held across a host call and host-object borrow flags are RAII.
  Message: ``host function `spawn_enemy` panicked: ..``. Documented limits: host state may be
  logically inconsistent afterwards; `panic = "abort"` defeats the catch.
- **Cost:** dispatch is a `Vec` index by `HostFnId`, no name hashing; args as a slice.

## 2. Exported host types (`colibri-bind`, re-exported as `colibri::export`)

A proc macro can't see whether a type is `Copy`, so it's explicit:

- `#[colibri::export(value)]` on a struct → a Colibri `struct` (static assert `Copy`, all `pub`
  fields, converted field by field on each crossing). On a fieldless enum → a Colibri enum.
- `#[colibri::export]` alone → an opaque host class. `#[colibri::export] impl T { ... }` exports
  methods (`&self`, `&mut self`, static constructors).

```rust
impl colibri::HostType for World {
    const NAME: &'static str = "World";
    fn describe(d: &mut colibri::TypeBuilder<Self>) {
        d.static_fn("new", ["gravity"], |g: Vec3| World::new(g));  // also: a leading `cx: &mut Ctx`
        d.method_mut("step", ["dt"], |this: &mut World, dt: f32| this.step(dt));
        d.method("position", ["body"], |this: &World, b: colibri::Ref<RigidBody>| this.position(&b));
    }
}
```

- `vm.register_type::<World>()`; a type referenced in a registered signature is registered
  automatically.
- Checker: host types are declared before user types (stable `TypeId`s), as
  `TypeDefKind::HostClass`: no literal construction, no fields, identity `==`/`is`/`Hash`, Show
  `<World #12>`, `.clone()` a compile error. Methods are inherent methods lowering to
  `Callee::Host(id)` with the receiver in `args[0]`. Both `&self` and `&mut self` are plain `self`
  in Colibri (class rule).
- **`&mut self` safety:** each slab slot has a borrow flag (`Cell<isize>`), shared for `&self`,
  exclusive for `&mut self`, via RAII guards. A conflict is a defined Colibri panic:
  ``host object `World` is already in use by an enclosing call (`World.step`)`` (golden-tested so
  the VM reproduces it).
- Exported methods take anything `FromColibri`: scalars, strings, `Vec<T>`, `Option<T>`, value
  types, `Ref<T>`/`Handle<T>`, `Callback<A, R>`, and `&mut Ctx` first.

## 3. Generational handles

- One `HostHeap` slab per Vm: `Slot { generation: u32, borrow, obj: Option<Rc<dyn Any>>, owner: Script | Host }`
  (`Rc` so that nested shared borrows work; see ARCHITECTURE §10).
- Host side: `Handle<T> { index: u32, generation: u32, vm }`, `Copy`, `!Send`. Script side:
  `Value::Host(Rc<HostRef { ty, index, gen }>)`.
- **Two ownership modes:** returning `T` by value moves it into the slab as script-owned; it is
  freed (Rust `Drop` runs) when the last Colibri reference is released, where a class `Drop` hook
  would run. `vm.insert(obj) -> Handle<T>` is host-owned: script references never free it;
  `vm.remove(h) -> Option<T>` (also on `Ctx`) destroys it and bumps the generation (`Err` if it is
  borrowed at that moment).
- **Scripts observe destruction:** a method call on a dead handle panics
  (`` `Enemy` was destroyed by the host ``); every host class has a built-in `alive() -> bool`;
  Rust returning `Option<Handle<T>>` gives `T?` with stale handles turned into `none`.
- Vm drop order: roots, then module values (Drop hooks may still call host methods), then
  remaining slab objects in index order.
- Host `TypeDef`s record `sendable: false` for the later `task.run` check (§14.1).

## 4. `.cbi` interface files

Colibri syntax, whole file, every function body-less; methods in `impl` blocks as in ordinary
source:

```colibri
// Generated by colibri-runtime; do not edit.
@interface(format = 1, module = "host", hash = "9f2c41d07a3be518", caps = ["fs"])

struct Vec3 { x: f32, y: f32, z: f32 }
@host class World {}
impl World {
    fn new(gravity: Vec3) -> World
    fn step(self, dt: f32)
}
fn spawn_enemy(a: f32, b: f32) -> Enemy
@requires(fs) fn read_save(path: str) -> Result[str]
```

- One path: live registration prints canonical `.cbi` text from the `SigDesc`s and parses it into
  the `HostEnv` the checker uses, exactly like the prelude. `Vm::write_interface(path)` writes the
  same text (only if it changed). Declaration order defines `HostFnId`s.
- Hash: FNV-1a 64 over the canonical body (not `DefaultHasher`, which isn't stable).
- `colibri check --interface engine.cbi game.colibri` (repeatable; a `colibri.toml` key later).
- A live load always rechecks against the live `HostEnv`, so a stale `.cbi` can't be unsound.
  `vm.interface_hash()` and `vm.verify_interface(path)` (→ `Error::InterfaceMismatch`) for CI.

## 5. Capabilities and sandbox

- `#[non_exhaustive] enum Cap { Fs, Os, Net, Ffi }` and a `CapSet`; `VmBuilder::allow(Cap)`; the
  default is none; the CLI grants all.
- `@requires(cap)` on host items (set with `vm.register_fn(..).requires(Cap::Fs)`); a checker error
  at the use site: `` `read_save` needs the `fs` capability, which this host did not grant ``.
  Ungranted functions are not installed (trap slot). `extern` blocks are an error without `Ffi`
  (and, per package, without `ffi = true`; see below). The std slice then only tags its modules.
- **`Cap::Ffi` and the `ffi` cargo feature** (ARCHITECTURE §19). `extern "C"` blocks need both
  halves: the host grants `Cap::Ffi` (`VmBuilder::allow(Cap::Ffi)`; a `Vm` without it rejects a
  program that has an `extern` block when it is checked: "`extern` blocks need the `ffi`
  capability, which this host did not grant") and the package declares `ffi = true` in its
  `colibri.toml` (a script without a manifest needs only the host's grant). The host's grant
  covers the program's own code, and every dependency still has to say it wants C itself. Nothing
  else changes for a host that does not grant it, which is what makes Colibri usable for mods.
  `colibri-runtime`'s `ffi` feature (on by default) brings in libffi and `dlopen`; an embedder
  that wants neither builds with `default-features = false`, and a program with an `extern`
  block then fails to load with `Error::Ffi("... this build of Colibri has no C FFI ...")`.
  `colibri-std` and `colibri-lsp` do not enable the feature themselves, so
  `default-features = false` on the runtime really turns it off. A library that cannot be
  opened, or a symbol it lacks, is also `Error::Ffi`, at load time and not at the first call.
- **Instruction budget:** one step per statement executed, per call (Colibri or host), per loop
  iteration, per element in looping built-ins. Approximate and engine-specific; the only guarantee
  is that unbounded work exhausts it. `.instruction_budget(n)` is an allotment per outermost host
  entry (`call`, `load`, `run_main`, later `tick`); reentrant calls share it;
  `vm.set_instruction_budget(Option<u64>)`, `vm.steps_used()`. Exhaustion unwinds like a panic
  (defers and drops run) with a grace of 10,000 steps for cleanups, after which remaining defers and
  Drop hooks are skipped and plain releases still happen. Surfaces as
  `Err(Error::BudgetExhausted { location, stack })`; the Vm stays usable.

  Step counts (`steps_used` and the budget) are defined by the current engine, the tree-walking
  interpreter, and will change when the bytecode VM replaces it. Hosts should not hard-code
  numbers tuned to exact counts: treat the budget as a coarse safety limit and leave headroom.
- **Memory limit: phase 3.** Byte counts in an `Rc`/`Vec` tree-walker would be meaningless, and an
  accepted-but-unenforced sandbox knob is worse than none.

## 6. Values across the boundary

| Rust | Colibri | Note |
|---|---|---|
| `i8..u64`, `f32`, `f64`, `bool`, `char` | same-named types (`i64` = `int`, `f64` = `float`) | `usize`/`isize` map to `int` with a range check |
| `String`; `&str` into Colibri only (arguments of `Fn::call`, results) | `str` | copied; a host fn parameter cannot be `&str` (no `unsafe`, so no lifetime-erased borrow): take `String` |
| `Vec<T>`, `&[T]` (in) | `List[T]` | copied each crossing (O(n), documented) |
| `Option<T>`, `Result<T, E>`, tuples up to 4 | `T?`, `Result[T]`, tuples | |
| `BTreeMap<K, V>` in; `HashMap`/`BTreeMap` out | `Map[K, V]` | no `HashMap` into Colibri: random order would make iteration nondeterministic; `HashMap` out is sorted by key (`K: Ord`) |
| `#[export(value)]` struct/enum | struct / enum | |
| `T`, `Ref<T>`, `Handle<T>` | host class | |
| `Callback<A, R>` | `fn(A..) -> R` | |
| `glam::Vec2/3/4` (also `IVec2/3/4`) | `vec.vec2/3/4` (`vec.ivec2/3/4`) | plain values, no conversion cost beyond copying 8 to 16 bytes; `colibri_runtime::glam` is the version to use. The module is registered by `colibri_std::install`, or by `colibri_runtime::install_vec(&mut vm)` in a host without the standard library. A `.cbi` writes them `vec.vec3` and needs no import for that |

**Callbacks:** `Callback<A, R>` is `Clone`, `!Send`, and holds a root id into a Vm root slab that
keeps the closure alive. Its Rust `Drop` pushes the id onto a shared queue that the Vm drains at
every entry (mlua's registry scheme). `cb.call(&mut impl Caller, args)`; after Vm shutdown or a
reload, `Err(StaleHandle)`.

## 7. Milestone support (DESIGN §17.5 phase 1 item 9)

The slice provides state-in-Vm with `Ctx`, `Handle<T>` with `insert`/`remove`/`get_mut`, value
types, `Callback`, `vm.find_fn::<A, R>(name) -> Result<Option<Fn>, Error>` for optional hooks, host
function names in stack traces, `write_interface`, and the step budget. The milestone example is a
headless Rust game loop (`examples/host_game`, fixed 60-tick loop, deterministic output, run as a
test): the host owns entities; scripts define `init()`, `update(dt: f32)`, `on_hit(e, dmg)`; host
functions `spawn_enemy`, `enemies()`, `Enemy.pos`/`set_pos`/`alive`, `despawn`, `on_timer(secs, fn())`.

## 8. DESIGN.md points resolved here

1. "Copy types as structs" (§15.4): explicit `#[colibri::export(value)]`.
2. Bare `spawn_enemy` (§15.4) vs `import rapier` (§15.5): default `host` module in scope, named
   modules need an import.
3. "Clean error" for dead handles: panic on method call, `alive()`, `none` through `Option<Handle<T>>`.
4. Ownership of host objects: by-value is script-owned, `Handle` is host-owned.
5. `instruction_budget`: approximate steps per outermost entry, distinct `BudgetExhausted`, bounded
   cleanup grace.
6. `memory_limit` (shown in §15.4): moved to phase 3 with the other sandbox limits.
7. `.cbi` format: Colibri syntax with an `@interface(...)` header.
8. Host `Result<T, E>`: `E: Display` becomes the `Error` message; `&mut self` reentrancy is a
   defined borrow-flag panic.

## 9. Running a script's tests from a host

`test "name" { .. }` blocks are checked with every program and never run by `load_*` or
`run_main`. A host that wants to run them (a game's own test command, an editor) does:

```rust
let mut vm = colibri::Vm::builder().stdout(Box::new(out.clone())).build();
// register host functions and modules here, as for the game
vm.load_tests("scripts/")?;                // a script file or a package folder; runs nothing
vm.set_instruction_budget(Some(100_000_000));
for t in vm.tests() {                      // TestInfo { id, name, module, root, file, line }
    match vm.run_test(t.id) {              // fresh module state for every test
        Ok(()) => {}
        Err(e) => eprintln!("{}::{} failed:\n{}", t.module, t.name, e.render(false)),
    }
}
```

`run_test` releases the module-level values, initialises them again (not the top-level statements),
runs the test and releases them again; failures are `Error::Panic` (including failed `assert`s),
`Error::TestReturnedErr` (the test returned `err`) and `Error::BudgetExhausted`. Details and the
reasoning are in ARCHITECTURE §15. Host state (handles, `insert_state`) is not reset.

## 10. Coroutines from a host

`go f(args)` in a script starts a coroutine (DESIGN §14.2). The host drives them:

```rust
vm.load_file("game.colibri")?;
vm.run_main()?;                     // `main` may start coroutines with `go`; it does not wait
loop {
    vm.tick(dt)?;                   // advance scheduler time by dt, resume the due coroutines once
    // ... draw, input, ...
}
```

- `vm.tick(dt) -> Result<(), Error>` adds `dt` seconds to scheduler time (what `wait(s)` counts;
  `tick(0.0)` pauses it), advances the tick counter, and resumes every due
  coroutine once in the order the coroutines were started. A coroutine started during a tick is
  resumed from the next one. A panic in a coroutine ends only that coroutine and is returned as
  `Error::Panic` (further ones are notes); running out of the instruction budget is
  `Error::BudgetExhausted`. The `Vm` stays usable. Every resumption has its own instruction budget.
- `vm.coroutine_count()` is how many coroutines are alive; `vm.cancel_coroutines()` cancels them
  all, newest first (also done by `shutdown` and `Drop`), running their `defer`s and drops.
- `vm.run_event_loop()` is what `colibri run` does after `main`: tick with clock time until no
  coroutine is left (sleeping until the next timed wake, polling `wait_until` conditions every
  1 ms). Use it for programs with no loop of their own.
- Scripts own coroutines with `CoGroup` (cancelled when the group dies) and wake them with
  `Signal` / `wait_for` (ARCHITECTURE §16.1). A coroutine waiting for a signal costs nothing per
  tick, and `run_event_loop` returns when only such waiters are left (they stay alive: emit and
  call `run_event_loop` or `tick` again, or cancel them). A host emits by calling a script
  function that does `s.emit()`; woken coroutines resume at the next `tick`.
  `vm.coroutine_resumptions()` counts resumptions, for tests.
- `VmBuilder::coroutine_stack_size(bytes)` (default 16 MiB, reserved and lazily committed; the evaluator
  panics with "stack overflow" at three quarters of it) and `VmBuilder::clock(impl Clock)`
  (the `Clock` trait: `now()` and `sleep()`; default the wall clock) for `wait_real`, the event
  loop and deterministic tests.
- A script `wait` (any of the four) called from a host function's callback into Colibri panics:
  the host's Rust frame cannot be suspended. The callback sees it as `Err(Error::Panic)` from
  `Fn::call`. A `wait(seconds)`, `join` or `join_all` on the main program (not inside `go`) runs the event loop instead of panicking (§10.1); the other waits still panic there.
- The `Vm` is boxed internally; moving a `Vm` value while coroutines are suspended is fine.
- Measured: `vm.tick` with 1,000 idle coroutines takes about 10 microseconds in a debug build
  (`tests/coroutines.rs`); a `wait_until` coroutine costs a resumption per tick.


### 10.1 Waiting on the main fiber, and background operations

- **Main-fiber waits.** `wait(s)`, `c.join()` and `group.join_all()` called from the program's
  main fiber (not inside a coroutine) run the same event loop as `vm.run_event_loop()` until
  the time has passed or the awaited coroutine or group has finished, with the clock of
  `VmBuilder::clock` (the wall clock under `colibri run`, the injected one in tests). They panic
  when they cannot: inside a host function's callback, a `Drop` hook, or a tick. A host that
  drives `vm.tick` itself and does not want this keeps scripts from calling them.
- **`Ctx::run_async(work) -> R`** is the host API for blocking work that must not stop the
  other coroutines. Inside a host function, `cx.run_async(move || do_blocking_thing(args))`
  runs the closure (`Send + 'static`, no Colibri values, no `Vm`) on a background thread (an idle one
  of the `Vm`'s pool, else a new one: there is no limit, and an operation never waits behind
  another; idle threads beyond four end after a few seconds) and
  suspends the calling coroutine until it finishes; the scheduler resumes it on a later tick,
  and the event loop sleeps until a timer or a completion instead of polling. On the main
  fiber the call runs the event loop meanwhile; inside a callback or a `Drop` hook it just
  blocks. Rules: do not hold a borrow of host objects across the call (other coroutines run);
  if the coroutine is cancelled while waiting, the host function is unwound by a Rust panic
  that the trampoline turns into a clean cancellation (the result is discarded; work that has not started yet never runs); a panic of the work is a Colibri panic of the caller. Work that can
  stop early takes `cx.run_async_cancellable(|cancelled| ..)`, which hands it a flag that is
  set on cancellation (`process.run` kills its program that way); work that blocks for a long
  time holds its thread meanwhile, which costs a stack and nothing else. A host driving `vm.tick(dt)` itself gets
  the completions at its ticks. `http` and `process` are built on it (ARCHITECTURE §22).
- The `Clock` trait has a third method with a default, `sleep_interruptible(seconds, wake)`,
  which the event loop calls while operations are in flight; a custom clock for tests can keep
  the default.

## 11. Hot reload from a host

`vm.reload_changed() -> Result<ReloadOutcome, Error>` (DESIGN §15.4, §16.3; as built in
ARCHITECTURE §17) re-reads the source files of a program loaded with `load_file`,
`load_package` or `load_path`, and puts changes to work. Call it at a safe point of your own
loop (between frames, requests or `vm.tick` calls), never from inside a script call.

```rust
vm.load_file("scripts/main.colibri")?;
vm.run_main()?;
loop {
    match vm.reload_changed()? {
        ReloadOutcome::Unchanged => {}
        ReloadOutcome::Patched { functions } => log::info!("patched {functions} functions"),
        ReloadOutcome::Restarted { reason, .. } => {
            log::info!("restarted: {reason}");
            vm.run_main()?;                    // the host decides whether to run main again
        }
        ReloadOutcome::CheckFailed(report) => eprintln!("{}", report.render(true)),
    }
    vm.tick(dt)?;
}
```

- **Patch**: only function bodies changed. `Fn` handles and `Callback`s stay valid, module state
  is kept, initialisers do not run again, coroutines suspended in an old body finish on it.
- **Restart**: anything else (signatures, types, module-level declarations or initialisers).
  Coroutines are cancelled, module values released, the new program initialised, and handles
  and callbacks from before are `StaleHandle`. State carries across if the old program has
  `fn save_state() -> str` and the new one `fn restore_state(s: str)` (text, until
  serialization exists, DESIGN §7.5). `main` is not called.
- **Check errors** do not change anything and come back as `CheckFailed(report)`; the next call
  says `Unchanged` until a file changes again. `Err` is for I/O trouble (retried next call) and
  panics in `save_state`, in the new initialisers or in `restore_state`.
- Registered host functions and types, capabilities, the instruction budget, clock, output and
  `insert_state` values live in the `Vm` and survive a restart. Register everything before the
  first load, as always.
- `reload_changed_with(ReloadOptions { force_restart: true })` restarts even when a patch
  would do. `run_event_loop_watching(poll, max_sleep, hook)` is `run_event_loop` with a hook
  between ticks (what `colibri run --watch` uses).
- Polling is `std::fs::metadata` per file, cheap enough to call every frame; a file is read
  and hashed only when its modification time or size moved.

## 12. Finding leaks from a host

Reference counting never frees a cycle of class objects or closures. The leak detector
(ARCHITECTURE §18) finds them:

```rust
let mut vm = colibri::Vm::builder().track_leaks(true).build();   // before loading
vm.load_file("game.colibri")?;
vm.run_main()?;
// ... later, at a level boundary, or after vm.shutdown() ...
for leak in vm.find_leaks() {
    eprintln!("{leak}");   // leak: a cycle of 2 objects: Node(name: "a") -> next -> ...
}
```

- `VmBuilder::track_leaks(true)` records every class object and closure made from then on (weakly,
  with the source line that made it). It is off by default and costs one `Option` test per object
  when off, about 5% on allocation-heavy code when on. Objects made before it is on are not known.
- `Vm::find_leaks() -> Vec<Leak>` works at any moment and changes nothing. Mid-run it reports the
  cycles that nothing reaches: a cycle a global, a frame, a temporary, a suspended coroutine,
  a host-held handle or a value the host holds still reaches is live and not reported. After
  `Vm::shutdown` (module values released, coroutines cancelled) every cycle that is left is a
  leak. A reference the detector cannot see counts as a root, so it can miss a leak but never
  reports a live object.
- `Leak { objects, path, sites, retained }` (all public, `Display` renders them): the classes in
  the cycle, one path through it, where the objects were made, and how many more objects the
  cycle alone keeps alive.
- `weak` fields and weak captures are not edges. The detector reports and frees nothing.
- The cost is linear in the live objects. Call it between frames or at level changes, not every tick.
