# Phase 2 implementation plan

This file tracks how phase 2 of [DESIGN.md §17.5](DESIGN.md#175-roadmap-locked-revised-2026-11-19)
(the game slice on the interpreter) is being built. DESIGN.md is the source of truth for the
language; ARCHITECTURE.md records how things are built. When they disagree, DESIGN.md wins and
this file is fixed.

## PR sequence

Each step is one or more small PRs. Every PR lands with golden tests and an ARCHITECTURE.md
section ("as built") for the decisions it had to make.

1. **Vector types** (DESIGN §15.1): `vec2`, `vec3`, `vec4` (`f32`) and `ivec2`, `ivec3`, `ivec4`
   (`i32`) as plain values: constructors, component fields, component-wise operators, vector by
   scalar, swizzles, `dot`, `cross`, `len`, `normalize`, `lerp` and friends, `==`, formatting,
   embedding conversions to and from glam. `mat3`, `mat4` and `quat` follow when a game needs
   them.
2. **Coroutines** (DESIGN §14.2, ARCHITECTURE §2): `go f(args)` and the `Coroutine` handle
   (`cancel`, `done`, `join`), `wait`, `wait_frames`, `wait_until`, `wait_real`, a per-`Vm`
   scheduler driven by `task.tick(dt)` and `vm.tick(dt)`, and cancellation that unwinds through
   `defer`s and drops. Each coroutine is a heap `Fiber` running the recursive evaluator on its
   own `corosensei` stack. Built: ARCHITECTURE §16.
3. **`CoGroup`, `Signal` and `wait_for`** (DESIGN §14.2).
4. **The game kit** (DESIGN §15.6), crate `colibri-game`, named host module `game`: window,
   input, 2D drawing, text, sprites, audio and assets behind a `Backend` trait. A headless
   backend records frames and replays scripted input, so golden tests and CI never open a
   window.
5. **A real backend** for the game kit (Q20, chosen by santiago), on by default in the `colibri`
   binary.
6. **Hot reload** (DESIGN §16.3): `vm.reload_changed()` with function-level patching, automatic
   restart with `save_state`/`restore_state` hooks, and file watching in `colibri run` for game
   kit programs.
7. **The leak detector** (DESIGN §11.5, Q9), built as trial deletion.
8. **`@repr(C)` structs and the C FFI** (DESIGN §15.2, §15.3).
9. **Milestone:** a small top-down arena shooter with hot reload, both standalone on the game
   kit and with its scripts embedded in a Rust host.

## Notes

- GitHub Actions is blocked by a billing limit, so PRs are merged on local checks:
  `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings` and
  `cargo test --workspace` on Linux.
