# Leo

Leo is a statically typed programming language for **scripting and game development**,
implemented in Rust. It aims for syntax as simple as Go or Python (with braces), memory
management by compiler-optimized reference counting, and two ways to run the same code: an
interpreter with hot reload for fast iteration, and native builds via Cranelift.

```leo
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
[open questions](docs/DESIGN.md#17-open-questions).

## Highlights (planned)

- Static types with local inference, generics (`List[T]`, `fn max[T](a: T, b: T)`)
- Value-type structs, reference counting with compile-time elision, `weak` refs, optional `own`/`borrow`
- Errors as values: `Result[T]`, `?`, `or` fallback, no exceptions
- Enums with exhaustive `match`, interfaces, no inheritance
- Game-dev built-ins: `vec2`/`vec3`/`vec4`, C-layout structs, easy C FFI
- Embeddable in Rust engines, and Rust crates usable from Leo via a binding layer
- One `leo` binary: `run`, `build`, `test`, `fmt`

## Roadmap

1. Lexer, parser, type checker, tree-walking interpreter
2. Bytecode VM and Cranelift native backend, RC optimizations
3. Game bindings (raylib, Rust crates) and a first small game
