# Colibri

Colibri is a statically typed programming language for **game scripting and general scripting**,
implemented in Rust. It aims for syntax as simple as Go or Python (with braces), memory
management by plain reference counting, and one bytecode VM that runs scripts, hot reload,
embedding in Rust engines, the web and shipped games.

```colibri
fn greet(name: str) -> str {
    return "Hello, {name}!"
}

let people = ["Ana", "Leo"]
for p in people {
    print(greet(p))
}
```

## Status

Design phase. Nothing is implemented yet. The language design is in
[docs/DESIGN.md](docs/DESIGN.md) and is open for review, including the list of
[open questions](docs/DESIGN.md#18-open-questions).

## Highlights (planned)

- Game scripting first: coroutines for game logic, `vec2`/`vec3`/`vec4`, a built-in game kit, hot reload
- Embeddable in Rust engines from day one, with easy C FFI
- One engine, the bytecode VM, for scripts, embedding, the web and shipped games
- Static types with local inference, generics (`List[T]`, `fn max[T](a: T, b: T)`)
- Plain reference counting with value-type structs and `weak` refs; no tracing GC
- Errors as values: `Result[T]`, `?`, `or` fallback, no exceptions
- Enums with exhaustive `match`, interfaces, no inheritance
- One `colibri` binary: `run`, `build`, `test`, `fmt`, `check`, `lsp`

## Roadmap

1. Language core on a tree-walking interpreter, embeddable in Rust from the start
2. Game slice: vector types, coroutines, a built-in game kit, hot reload, and a first small game
3. The bytecode VM, `colibri build` for desktop and the web
4. General scripting: `regex`, `http`, subprocesses, a REPL

Stretch goals: native builds via Cranelift, RC elision, an ownership checker.
