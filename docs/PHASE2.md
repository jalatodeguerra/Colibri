# Phase 2 implementation plan

This file tracks how phase 2 is being built. DESIGN.md is the source of truth for the language;
ARCHITECTURE.md records how things are built. When they disagree, DESIGN.md wins and this file
is fixed.

**Status (2026-11-19):** santiago redirected Colibri to be a general-purpose language rather than
one aimed at game development. Phase 2 was planned as the game slice of DESIGN §17.5; the plan
below replaces it, and santiago approved it on 2026-11-19 together with the matching DESIGN
changes (v0.4: §1 goals, §14.2 wording, §15.1 vectors as a std module, §15.6 game kit as an
optional package, §17.5 roadmap). The game kit and the game milestone are paused.

## PR sequence

Each step is one or more small PRs. Every PR lands with golden tests and an ARCHITECTURE.md
section ("as built") for the decisions it had to make.

1. **Coroutines** (DESIGN §14.2, ARCHITECTURE §2): `go f(args)` and the `Coroutine[T]` handle
   (`cancel`, `done`, `join`), `wait`, `wait_frames`, `wait_until`, `wait_real`, a per-`Vm`
   scheduler driven by `vm.tick(dt)`, `task.tick(dt)` or the event loop `colibri run` runs after
   `main`, and cancellation that unwinds through `defer`s and drops. Built: ARCHITECTURE §16.
2. **`CoGroup`, `Signal` and `wait_for`** (DESIGN §14.2): groups that cancel their coroutines
   when they die, `group.go(call)`, `join_all`, and signals with no per-tick cost. Built:
   ARCHITECTURE §16.1.
3. **Scripting std**, moved up from phase 4: `args`, subprocesses, `datetime`, `regex`, and an
   `http` client behind the `net` capability. Built: part A (`args`, `process`, `datetime`,
   ARCHITECTURE §13.1) and part B (`regex`, `http`, ARCHITECTURE §13).
4. **`colibri repl`** and startup time tuned for one-off scripts. Built: ARCHITECTURE §21
   (`colibri run hello.colibri` went from 58 ms to 19 ms by making host registration linear).
5. **Hot reload** (DESIGN §16.3) for long-running programs and embedded scripts:
   `vm.reload_changed()`, `colibri run --watch`. Built: ARCHITECTURE §17 (snapshot hooks pass
   text, `save_state() -> str`, until §7.5 exists).
6. **The leak detector** (DESIGN §11.5, Q9), built as trial deletion: `colibri run --leaks`,
   `colibri test --leaks`, `VmBuilder::track_leaks` and `Vm::find_leaks`, and the golden
   directive `// leaks:`. Report only. Built: ARCHITECTURE §18.
7. **`@repr(C)` structs and the C FFI** (DESIGN §15.2, §15.3): `extern "C"` blocks with
   `@link`, structs by value through libffi, `ptr[T]` and `unsafe`, `ffi = true` in
   `colibri.toml`, `Cap::Ffi` and the `ffi` cargo feature. Built: ARCHITECTURE §19.
8. **Vector types as the std module `vec`** (DESIGN §15.1): `import vec` gives `vec.vec2`,
   `vec.vec3`, `vec.vec4` and `vec.ivec2..4`, plain values with component-wise operators, fields
   and swizzles. Built: ARCHITECTURE §20.
9. **Milestone:** real automation scripts written in Colibri (a file organiser, a JSON/HTTP
   report generator). Built: [examples/scripts](../examples/scripts/README.md), with the
   language findings in [FINDINGS.md](../examples/scripts/FINDINGS.md); also added
   `fs.modified`.


## Notes

- GitHub Actions is blocked by a billing limit, so PRs are merged on local checks:
  `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings` and
  `cargo test --workspace` on Linux.
