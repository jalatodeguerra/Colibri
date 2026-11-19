# Optional game kit package (moved from DESIGN §15.6)

This note was DESIGN.md §15.6 until v0.5. The game kit is deferred: it is not in the stock
binary and not scheduled. It is kept here as a design sketch.

The game kit is **not in the stock binary** (revised 2026-11-19). A general-purpose language
should not carry a window, a renderer and an audio stack in every install. It becomes an
**optional package for later**, a separate `colibri-game` package (and crate), built after the
general scripting slice (§17.5). Until then there is no `game` module, and games use the C FFI
(§15.3) or the embedding API (§15.4).

The idea is unchanged, the way LÖVE is a kit for Lua: a game needs no Rust toolchain, no C
toolchain and no bindings beyond installing the package.

```colibri
import game
import vec

fn main() {
    game.window(800, 600, "Arena")
    var pos = vec.vec2(400, 300)
    game.run(fn(dt) {                      // calls task.tick(dt) for coroutines, then this
        if game.key_down(.Right) { pos.x += 200 * dt }
        game.clear(game.BLACK)
        game.circle(pos, 16, game.RED)
    })
}
```

- Scope for v1 of the package: window and input (keyboard, mouse, gamepad), 2D drawing (shapes,
  sprites, text), audio playback, and asset loading. 3D is out of scope.
- The backend (statically linked raylib, or Rust crates such as winit, wgpu and kira) is Q20,
  **deferred** until the package is scheduled.
- The kit is ordinary Colibri plus host functions, so it doubles as a reference example of the
  embedding API.
