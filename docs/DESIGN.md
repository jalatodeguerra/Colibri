# Leo Language Design

**Status:** Draft v0.1, for review. Nothing here is implemented yet.
**Date:** 2026-11-18

This document describes what Leo is, the decisions already agreed, the concrete proposals
that follow from them, and the open questions that still need a call. Every section is
tagged with one of three markers:

- **[Locked]** — agreed during the brainstorm. Changing it means re-opening the discussion.
- **[Proposed]** — my recommendation, filling in details the brainstorm didn't cover. Review and push back.
- **[Open]** — a real choice that needs a decision. Listed again in [§17 Open questions](#17-open-questions).

---

## Table of contents

1. [Goals and non-goals](#1-goals-and-non-goals)
2. [Leo at a glance](#2-leo-at-a-glance)
3. [Lexical structure](#3-lexical-structure)
4. [Bindings and mutability](#4-bindings-and-mutability)
5. [Types](#5-types)
6. [Functions and closures](#6-functions-and-closures)
7. [Structs, classes and methods](#7-structs-classes-and-methods)
8. [Enums and pattern matching](#8-enums-and-pattern-matching)
9. [Interfaces and generics](#9-interfaces-and-generics)
10. [Error handling](#10-error-handling)
11. [Memory model](#11-memory-model)
12. [Strings](#12-strings)
13. [Modules and packages](#13-modules-and-packages)
14. [Game-dev features](#14-game-dev-features)
15. [Tooling and execution modes](#15-tooling-and-execution-modes)
16. [Implementation plan (Rust)](#16-implementation-plan-rust)
17. [Open questions](#17-open-questions)
18. [Prior art](#18-prior-art)

---

## 1. Goals and non-goals

### Goals [Locked]

- **Genuinely usable**, not a toy. Someone should be able to ship a small game or a real
  automation script in Leo.
- **Simple syntax**, in the spirit of Go and Python, with braces. Small keyword set, one
  obvious way to do common things.
- **Static types with inference.** Errors are caught before the program runs, but day-to-day
  code rarely needs type annotations inside function bodies.
- **Two target domains:** scripting, and game development.
- **Two run modes from one language:** a fast-starting interpreter (later JIT) for scripts and
  hot reload, and native builds via Cranelift.
- **Predictable memory without a tracing GC:** reference counting by default, with the compiler
  removing most of the counting overhead.
- **Embeddable** in Rust game engines, and **easy C interop**.

### Non-goals

- Audio-plugin / hard-real-time DSP as a first-class target (explicitly dropped).
- Inheritance, exceptions, macros, operator overloading for arbitrary types (see §14 for the
  narrow exception on math types), and compile-time metaprogramming in v1.
- Competing with Rust or C++ on peak performance. "Fast enough for a 2D/3D indie game and
  much faster than Python" is the bar.
- A package registry in v1.

### Design principles [Proposed]

1. **Readable by a Python or Go programmer on day one.** If a feature needs a paragraph to
   explain, it needs a strong reason to exist.
2. **Performance is opt-in, never required.** Plain code is correct and reasonably fast;
   `own`/`borrow`, C-layout structs and SIMD exist for the hot 5%.
3. **The optimizer must not change meaning.** Refcount elision, moves and the JIT are
   invisible to programs except through speed.
4. **Great error messages are a feature**, budgeted for from the first parser.

---

## 2. Leo at a glance

```leo
import math
import fs

struct Player {
    name: str
    pos: vec2
    hp: int = 100
}

enum Command {
    Move(vec2)
    Heal(int)
    Quit
}

fn greet(name: str) -> str {
    return "Hello, {name}!"
}

fn apply(p: Player, cmd: Command) -> Player {
    var next = p                       // structs are values: this is a copy
    match cmd {
        Move(dir) => next.pos += dir
        Heal(n)   => next.hp = math.min(next.hp + n, 100)
        Quit      => {}
    }
    return next
}

fn load_level(path: str) -> Result[Level] {
    let text = fs.read(path)?          // propagate the error
    let level = parse_level(text)?
    return ok(level)
}

fn main() {
    let people = ["Ana", "Leo"]
    for p in people {
        print(greet(p))
    }

    let level = load_level("level1.txt") or default_level()
    var hero = Player { name: "Ana", pos: vec2(0, 0) }
    hero = apply(hero, Move(vec2(1, 0)))
    print("{hero.name} is at {hero.pos} with {hero.hp} hp")
}
```

---

## 3. Lexical structure

### 3.1 Source files [Proposed]

- UTF-8, extension `.leo`.
- Comments: `// line` and `/* block */` (nestable). Doc comments: `///`.

### 3.2 Statement termination [Proposed]

Braces delimit blocks; **newlines end statements** — no semicolons in normal code. Leo uses
Go's rule: a newline ends a statement if the last token on the line could end one (an
identifier, literal, `)`, `]`, `}`, `return`, `break`, `continue`). This means a line can
continue after an operator or comma:

```leo
let total = price * qty +
            shipping            // OK: line ended with `+`
```

Semicolons are allowed to put several statements on one line, and `leo fmt` removes them
otherwise.

### 3.3 Keywords [Proposed]

```
fn let var const return if else for in while break continue match
struct class enum interface impl import pub as
own borrow weak extern defer test
true false
```

`ok`, `err`, `some`, `none` are ordinary built-in functions/constructors, not keywords.
`or` is an operator keyword (§10). Logical operators are `and`, `or`, `not`? **[Open]** —
see Q1. Using `or` for both boolean OR and Result fallback is the one real collision; the
proposed resolution is to use `&&`, `||`, `!` for booleans and reserve `or` for fallback.

### 3.4 Literals [Proposed]

| Kind | Examples |
|------|----------|
| Integer | `42`, `1_000_000`, `0xFF`, `0b1010` |
| Float | `3.14`, `1e-9`, `2.0` |
| String | `"hello {name}"`, raw: `r"C:\path\{not interpolated}"` |
| Multi-line string | `"""..."""` (strips common indentation) |
| Char | `'a'`, `'\n'` |
| List | `[1, 2, 3]` |
| Map | `{"a": 1, "b": 2}` |
| Bool | `true`, `false` |

Numeric literals are **untyped constants** (as in Go): `1` becomes `int`, `float`, `f32` or
`u8` depending on what it's used as, and defaults to `int`/`float` when nothing constrains it.
This is what makes `vec2(1, 0)` work without `vec2(1.0f, 0.0f)` noise.

---

## 4. Bindings and mutability

[Proposed]

```leo
let x = 10        // immutable binding
var y = 20        // mutable binding
y += 1
const MAX_HP = 100   // compile-time constant, module level or local
```

- `let` cannot be reassigned, and for value types (structs, vectors) its fields cannot be
  mutated either.
- `let` on a reference type (a `class`, a `List`) prevents rebinding but not mutating the
  object behind it — same as `final` in Java or `const` in JavaScript. Whether Leo should offer
  deep immutability is deferred (Q5).
- Shadowing is allowed in nested blocks, and is a warning in the same block.

---

## 5. Types

### 5.1 Primitive types [Proposed]

| Type | Meaning |
|------|---------|
| `int` | 64-bit signed integer (default integer) |
| `i8 i16 i32 i64`, `u8 u16 u32 u64` | sized integers, mainly for FFI and data formats |
| `float` | 64-bit float (default float) |
| `f32`, `f64` | sized floats |
| `bool` | `true` / `false` |
| `char` | a Unicode scalar value |
| `str` | immutable UTF-8 string (§12) |
| `vec2 vec3 vec4` | `f32` vectors, SIMD-backed (§14) |
| `()` | unit; functions without `-> T` return it |

Integer overflow traps in debug builds and in `leo run`, wraps in `leo build --release`?
**[Open]** — see Q6.

No implicit numeric conversions between named types: `let a: i32 = b` where `b: int` is an
error; use `i32(b)`. (Untyped literals from §3.4 are the exception.)

### 5.2 Built-in generic types [Proposed]

| Type | Semantics |
|------|-----------|
| `List[T]` | growable array, literal `[a, b]` |
| `Map[K, V]` | hash map, insertion-ordered, literal `{k: v}` |
| `Set[T]` | hash set |
| `Option[T]` | `some(x)` or `none` — Leo has **no null** |
| `Result[T]`, `Result[T, E]` | §10 |
| `[T; N]` | fixed-size array, value type, used in C-layout structs |
| `fn(A, B) -> R` | function / closure type |

Whether `List`/`Map` have reference semantics or copy-on-write value semantics is a key open
question (Q3). This draft assumes **reference semantics**, which is what Python and Go users
expect.

Whether `T?` should be sugar for `Option[T]` is open (Q2).

### 5.3 Type inference [Proposed]

- **Function signatures are annotated; bodies are inferred.** Parameters and return types are
  always written (except `-> ()`), local variables almost never are. This is the Rust/Go/Swift
  balance: signatures are documentation and keep error messages local, and inference stays
  fast and predictable.
- Inference is bidirectional and local to a function: the expected type flows into
  expressions (`let v: List[f32] = []` works, `[]` alone with no later use is an error asking
  for an annotation).
- Closures can omit parameter types when the expected type is known:
  `items.map(fn(x) { x * 2 })`.
- Generic type arguments are inferred at call sites: `max(3, 4)`, with `max[f32](a, b)` as
  the explicit form.

---

## 6. Functions and closures

### 6.1 Functions [Proposed]

```leo
fn add(a: int, b: int) -> int {
    return a + b
}

fn log(msg: str, level: str = "info") {     // default argument
    print("[{level}] {msg}")
}

log("starting")
log("disk full", level: "warn")              // named argument
```

- Default arguments and optional named arguments at call sites. No variadics in v1 except
  through `List[T]`.
- `return` is required to return a value from a function. `if` and `match` can be used as
  expressions (`let s = if x > 0 { "pos" } else { "neg" }`). Whether a block's last
  expression is its implicit value everywhere (Rust-style) is open (Q4).
- `pub fn` exports from the module (§13).
- `defer` runs a statement when the enclosing block exits, Go-style, for cleanup:
  `let f = fs.open(path)?` then `defer f.close()`.

### 6.2 Closures [Proposed]

```leo
let double = fn(x: int) -> int { return x * 2 }
let evens = nums.filter(fn(n) { n % 2 == 0 })   // single-expression body, types inferred
```

Closures capture variables **by reference** to the enclosing variable (like Go, Python, JS),
so captured `var`s can be updated. Captured variables are boxed into a refcounted cell only
when a closure actually mutates them or escapes; otherwise they're captured directly.

A shorter lambda syntax (e.g. `|n| n % 2 == 0` or `n => n % 2 == 0`) is open (Q4b).

---

## 7. Structs, classes and methods

The brainstorm locked "structs are value types (copied)". Games and scripts also need shared,
mutable objects with identity (a scene node, an entity referenced from several places). This
draft therefore proposes two kinds of user-defined record type.

### 7.1 `struct` — value types [Locked semantics, Proposed syntax]

```leo
struct Vec2i {
    x: int
    y: int
}

struct Particle {
    pos: vec2
    vel: vec2
    life: float = 1.0          // field default
}

let a = Vec2i { x: 1, y: 2 }
var b = a                      // copy
b.x = 10                       // `a` is unchanged
```

- Assignment, argument passing and returning copy the value (the compiler avoids actual
  copies when it can prove no one observes the difference, §11.3).
- A struct can contain reference-typed fields (`str`, `List`, a `class`); copying the struct
  copies the reference and bumps its count.
- Structs have no identity: `==` is structural, auto-derived when all fields support it.

### 7.2 `class` — reference types [Proposed]

```leo
class Node {
    name: str
    children: List[Node] = []
    parent: weak Node?          // weak back-reference, see §11.5
}

let root = Node { name: "root" }
let child = Node { name: "child", parent: root }
root.children.push(child)       // root and the list share `child`
```

- Heap-allocated and reference-counted. Assignment shares; it does not copy.
- `==` on classes is identity unless the class implements `Eq`.
- **No inheritance.** Sharing behaviour across types is done with interfaces (§9) and
  composition.

The keyword (`class` vs `ref struct` vs `box`) and whether to have two record kinds at all are
open (Q3).

### 7.3 Methods [Proposed]

Methods are declared in `impl` blocks, separate from the data:

```leo
impl Particle {
    fn new(pos: vec2) -> Particle {          // no `self`: a static function
        return Particle { pos: pos, vel: vec2(0, 0) }
    }

    fn alive(self) -> bool {                 // read-only receiver
        return self.life > 0
    }

    fn step(var self, dt: float) {           // mutating receiver (structs only)
        self.pos += self.vel * dt
        self.life -= dt
    }
}

var p = Particle.new(vec2(0, 0))
p.step(0.016)        // `p` must be `var`
```

- `self` is read-only; `var self` may mutate a struct in place, and the call site needs a
  mutable place. On a `class`, `self` can always mutate fields, since the object is shared.
- Methods can be added to a type only in the module that defines it, except to implement an
  interface (§9).

---

## 8. Enums and pattern matching

### 8.1 Enums [Proposed]

Enums are tagged unions; variants may carry data:

```leo
enum Shape {
    Circle(radius: float)
    Rect(w: float, h: float)
    Point
}

enum Dir { North, East, South, West }     // C-like enum, single line
```

Enums are value types. `Option[T]` and `Result[T, E]` are ordinary enums from the prelude.

### 8.2 `match` [Proposed]

```leo
fn area(s: Shape) -> float {
    return match s {
        Circle(r)    => math.PI * r * r
        Rect(w, h)   => w * h
        Point        => 0
    }
}

match cmd {
    Move(d) if d.len() > 10 => print("big move")       // guard
    Move(d)                 => hero.pos += d
    Heal(n) | Regen(n)      => hero.hp += n             // or-patterns
    _                       => {}
}
```

- `match` is **exhaustive**: missing a variant is a compile error.
- Patterns: literals, ranges (`0..10`), variants, struct destructuring (`Vec2i { x, y }`),
  tuples, `_`, bindings with `as` (`Move(d) as cmd`), guards with `if`.
- Variant names are usable unqualified inside `match` on a value of that enum type; elsewhere
  they're written `Shape.Circle(2)` unless imported. (Swift-style contextual lookup:
  `let s: Shape = .Circle(2)` is a possible addition, Q11.)
- `if let some(x) = maybe { ... }` and `while let` for single-pattern tests.

---

## 9. Interfaces and generics

### 9.1 Generics [Locked syntax, Proposed details]

```leo
fn max[T: Ord](a: T, b: T) -> T {
    return if a > b { a } else { b }
}

struct Pair[A, B] {
    first: A
    second: B
}

enum Tree[T] {
    Leaf
    Node(left: Tree[T], value: T, right: Tree[T])
}
```

- Square brackets, as locked. Indexing also uses `[]`; the parser disambiguates by position
  (type position vs expression position) and by the explicit-instantiation form
  `max[f32](a, b)`. This is the same situation as Go 1.18 and is known to be workable.
- **Bounds** use interfaces: `T: Ord`, `T: Hash + Eq`.
- **Implementation strategy:** the interpreter runs generic code once with uniform value
  representation; native builds **monomorphize** (one copy per concrete type), as Rust does.
  Both strategies are invisible to the user.
- Recursive enums like `Tree[T]` are boxed automatically (the compiler inserts an RC box for
  recursive fields), so users never write `Box`.

### 9.2 Interfaces [Proposed]

```leo
interface Shape {
    fn area(self) -> float
    fn name(self) -> str { return "shape" }   // default method
}

struct Circle { r: float }

impl Shape for Circle {
    fn area(self) -> float { return math.PI * self.r * self.r }
}

fn total_area(shapes: List[Shape]) -> float {   // dynamic dispatch
    var sum = 0.0
    for s in shapes { sum += s.area() }
    return sum
}

fn print_area[T: Shape](s: T) {                 // static dispatch
    print(s.area())
}
```

- **Nominal, explicit** implementation (`impl Shape for Circle`), not Go-style structural.
  Explicit impls give better error messages and allow default methods. Structural typing
  is open (Q7).
- An interface name used as a type (`List[Shape]`) means a dynamically dispatched value
  (an RC box with a vtable). Used as a bound (`T: Shape`), it is statically dispatched.
- Built-in interfaces in the prelude: `Eq`, `Ord`, `Hash`, `Show` (used by string
  interpolation and `print`), `Iter[T]`,
  `Drop` (cleanup hook, runs when the last reference goes away).
- Orphan rule: `impl I for T` must live in the module of `I` or of `T`.

---

## 10. Error handling

[Locked] Errors are values. There are no exceptions.

### 10.1 Types

- `Result[T, E]` is an enum: `ok(T)` or `err(E)`.
- `Result[T]` is shorthand for `Result[T, Error]`, where `Error` is a built-in type: a
  message string, an optional cause (another `Error`), and a captured location/backtrace in
  debug builds. Any type implementing the `IntoError` interface converts to it with `?`.

```leo
enum ParseError {
    UnexpectedEof
    BadToken(line: int, text: str)
}

fn parse(src: str) -> Result[Ast, ParseError] { ... }

fn run(path: str) -> Result[()] {             // default error type
    let src = fs.read(path)?
    let ast = parse(src)?                     // ParseError converts to Error via IntoError
    eval(ast)?
    return ok(())
}
```

### 10.2 Handling forms [Locked]

```leo
let cfg = load_config()?                              // propagate
let cfg = load_config() or Config.default()           // fallback value
let cfg = load_config().expect("config is required")  // crash with message
match load_config() {
    ok(c)  => use(c)
    err(e) => log("bad config: {e}")
}
```

- `?` works in functions returning `Result` (converting the error) and in functions
  returning `Option` (for `Option` values).
- `or` evaluates its right-hand side only on `err`/`none`. A block form gives access to the
  error: `load() or fn(e) { log(e); return default() }`. **[Open]** exact syntax, tied to Q4b.
- **Unused `Result` is a warning** (`let _ = f()` silences it explicitly).
- `main` may return `Result[()]`; an `err` prints the error and exits with code 1.

### 10.3 Panics [Proposed]

Some failures aren't recoverable values: out-of-bounds index, integer overflow (debug),
`.expect()` on `err`, division by zero. These **panic**: print a message and stack trace, and
abort the current program (or, when embedded, return an error to the host without crashing the
host process). Panics cannot be caught in Leo code. Hot reload (§15.3) can resume after a
panic in development builds.

---

## 11. Memory model

[Locked] Hybrid: reference counting by default; the compiler infers moves and borrows to
elide refcount operations; structs are value types; optional `own`/`borrow` for hot code;
`weak` references for cycles. Ship plain RC first; add optimizations later without changing
user code.

### 11.1 What is reference counted

| Kind | Representation |
|------|----------------|
| `int`, `float`, `bool`, `char`, `vecN`, C-layout structs of those | plain values, no RC |
| `struct` | plain value; its reference-typed fields are counted |
| `enum` | value; payloads that are reference types are counted |
| `str`, `List`, `Map`, `Set`, `class`, closures, interface values, recursive enum boxes | heap objects with a refcount header |

Refcounts are **non-atomic** (cheap) by default. See §11.7 for threads.

### 11.2 Phase 1: plain RC

The first implementation increments on every copy of a reference and decrements on every drop.
This is slow-ish but simple, deterministic, and correct. Objects are freed immediately when
their count reaches zero; `Drop` hooks run at that point. Deterministic destruction is a
feature for games (textures, sounds and file handles are released when the last owner goes).

### 11.3 Phase 2: compiler-inferred elision [Locked intent, Proposed approach]

The compiler eliminates most counting without user annotations, using known techniques:

1. **Last-use moves** (Perceus, Lobster). If a variable's last use passes it somewhere, it's a
   move: no increment at the call site and no decrement at scope end.
2. **Borrowed parameters** (Lobster, Swift's `guaranteed` convention). By default a function
   *borrows* its reference parameters: the caller keeps the reference alive, the callee does
   no RC unless it stores or returns the value.
3. **Reuse analysis** (Perceus / Koka FBIP). When a uniquely owned object dies and an
   object of the same size is allocated right after (e.g. `list.map` producing a new list),
   the memory is reused in place.
4. **Uniqueness-based in-place mutation.** If a `List` has refcount 1, mutations don't need
   copy-on-write checks (only relevant if Q3 picks value semantics).
5. **Struct copy elision.** A struct passed to a read-only parameter is passed by pointer;
   it is only physically copied when the callee or the caller would observe a difference.

None of these change program behaviour; only timing of frees within a function can move
earlier (to a variable's last use instead of scope end). `Drop` hooks with side effects are
therefore documented as "runs no later than scope end".

### 11.4 `own` and `borrow` [Locked keywords, Proposed semantics]

Optional annotations for hot code that make the compiler's choice explicit and **checked**:

```leo
fn consume(own buf: List[u8]) { ... }         // callee takes ownership; caller's variable is moved
fn inspect(borrow mesh: Mesh) -> int { ... }  // guaranteed zero RC traffic; cannot store `mesh`

let data = load()
consume(data)          // `data` is moved; using it afterwards is a compile error
```

- `own` on a parameter: the caller must give up the value (last use, or an explicit
  `move x`?). Using it afterwards is an error. **[Open]** whether an explicit `move` keyword is
  needed at call sites (Q8).
- `borrow` on a parameter: the callee may not store, return or capture the value in an escaping
  closure. Violations are compile errors with a hint.
- `borrow` on a local (`borrow let m = scene.mesh`) is a non-owning view valid while the source
  is alive and unmodified; the checker is deliberately simple (lexical, Rust-2015-style) and
  rejects anything it can't prove. Exact rules are open (Q8).
- Code without annotations is always valid; annotations only *add* guarantees.

### 11.5 Cycles and `weak` [Locked]

RC can't free cycles. Leo's stance:

- `weak T` holds a non-owning reference; reading it gives `Option[T]` (`none` if freed):
  `if let some(p) = node.parent.get() { ... }`.
- Common back-pointers (parent, owner, observer lists) should be `weak`.
- **Debug-mode leak detector**: on exit (and on demand via `leo run --leaks`) the runtime
  reports objects still alive that are only reachable through cycles, with allocation sites.
- A backup cycle collector (Python-style trial deletion) is **not** planned for v1. Open
  whether to add it later (Q9).

### 11.6 Mutation and aliasing of value types inside containers

Because structs are values, `list[i].hp -= 1` must mutate the element in place, not a copy.
The rule: indexing, field access and `for var x in list` produce **places** (lvalues) when used
on the left of an assignment or as a `var self` receiver:

```leo
for var e in enemies {     // e is a place inside the list
    e.hp -= 1
}
enemies[0].pos += vec2(1, 0)
```

`for e in enemies` (no `var`) yields copies (or, after optimization, read-only views).

### 11.7 Concurrency [Open]

Not part of v1 language design, but the memory model must leave room. Proposal: Leo programs
are single-threaded by default; concurrency comes later as **isolated workers** with message
passing (values are deep-copied or moved across), so RC can stay non-atomic. Data-parallel
helpers for games (`parallel_for` over a `List` of structs) can be offered on top. See Q10.

---

## 12. Strings

[Locked] Interpolation: `"Hello, {name}!"`.

[Proposed]

- `str` is immutable, UTF-8, reference-counted (small strings may be stored inline later).
- Interpolation takes any expression: `"{a + b}"`, `"{p.pos}"`. The value must implement
  `Show`. Format specs: `"{x:.2}"`, `"{n:>8}"`, `"{id:x}"` (Python-like, subset).
- Literal braces are written `{{` and `}}`. Raw strings `r"..."` don't interpolate.
- Indexing a `str` by integer is not allowed (it's not O(1) in UTF-8); use `s.chars()`,
  `s.bytes()`, slicing by byte range `s[0..4]` (panics on a non-boundary).
- `StrBuilder` for efficient building; `+` concatenates strings.

---

## 13. Modules and packages

[Proposed]

- One file is one module. A directory with a `leo.toml` is a package.
- `import math`, `import game.physics`, `import game.physics as phys`,
  `import game.physics.{Body, World}`.
- Everything is private to its module unless marked `pub`.
- A script (`leo run script.leo`) needs no `leo.toml`. Top-level statements in a script run in
  order, so `print("hi")` is a valid one-line program. In a package, execution starts at
  `fn main()`.
- Standard library v1: `io`, `fs`, `path`, `os` (args, env, process), `math`, `rand`,
  `time`, `json`, `str` helpers, `collections`, `test`. Networking is later.
- Dependencies in `leo.toml` from git URLs or paths; no registry in v1.

---

## 14. Game-dev features

### 14.1 Vector and matrix types [Locked: vec2/3/4, Proposed details]

- `vec2`, `vec3`, `vec4` (`f32`), `ivec2/3/4` (`i32`), `mat3`, `mat4`, `quat` are built-in
  value types, laid out for SIMD (vec3 is padded to 16 bytes in memory, packed in C-layout
  structs only when asked).
- Arithmetic operators work component-wise (`a + b`, `a * 2`, `-a`); `dot`, `cross`, `len`,
  `normalize`, `lerp`, swizzles (`v.xy`, `v.zyx`) as built-ins.
- This is the **only** operator overloading in v1: user types can't define `+`. Open whether
  to allow it via an interface later (Q12).

### 14.2 C-layout structs [Locked, Proposed syntax]

```leo
@repr(C)
struct Vertex {
    pos: vec3
    uv: vec2
    color: [u8; 4]
}
```

`@repr(C)` structs follow the platform C ABI, may only contain C-compatible fields (numbers,
vectors, fixed arrays, other `@repr(C)` structs, raw pointers), and can be passed to C
directly or uploaded to the GPU as a byte buffer (`vertices.as_bytes()`).

### 14.3 C FFI [Locked, Proposed syntax]

```leo
@link("raylib")
extern "C" {
    fn InitWindow(width: i32, height: i32, title: cstr)
    fn WindowShouldClose() -> bool
    fn DrawCircleV(center: vec2, radius: f32, color: Color)
}
```

- `cstr` is a borrowed NUL-terminated string; passing a `str` literal converts automatically.
- `ptr[T]` is a raw pointer type, only usable in `unsafe` blocks? **[Open]** whether Leo has an
  `unsafe` marker or treats all of `extern` as "you're on your own" (Q13).
- `leo bindgen header.h` generates `extern` declarations from C headers (phase 3).
- The interpreter calls C via `libffi`; native builds call directly.

### 14.4 Embedding in Rust [Locked, Proposed API shape]

```rust
let mut vm = leo::Vm::new();
vm.register_fn("spawn_enemy", |x: f32, y: f32| { /* engine code */ });
vm.load_file("scripts/ai.leo")?;
let f = vm.get_fn::<(f32,), ()>("update")?;
f.call(&mut vm, (dt,))?;
vm.reload_changed()?;            // hot reload, §15.3
```

- `leo-runtime` is a regular Rust crate with no global state; several VMs can coexist.
- Rust types are exposed to Leo via a derive macro (`#[derive(leo::Struct)]`) for value types
  or as opaque handles (`leo::Handle<T>`) for engine-owned objects.
- Panics in Leo code become `Err` on the Rust side; they never unwind into the host.
- Resource limits for embedding (instruction budget, memory cap) are optional VM settings.

### 14.5 Using Rust crates from Leo [Planned, Proposed design]

Rust's game ecosystem (rapier for physics, glam for math, wgpu for graphics, kira for audio)
would give Leo a lot on day one. Leo's runtime is already Rust, so the same binding layer
that powers embedding (§14.4) can expose Rust crates to Leo code, the way PyO3 does for
Python.

**Why not arbitrary `import some_crate`:** Rust has no stable ABI, and traits, lifetimes,
generic functions and borrowed references don't map one-to-one onto Leo's type system. A
crate needs a thin, explicit Leo-facing surface.

**Rust side.** A `leo-bind` crate provides attribute macros that generate the glue:

```rust
use leo_bind::prelude::*;

#[leo::export]
pub struct RigidBody { handle: rapier3d::dynamics::RigidBodyHandle }   // opaque handle in Leo

#[leo::export]
impl World {
    pub fn new(gravity: Vec3) -> World { ... }
    pub fn add_ball(&mut self, pos: Vec3, radius: f32) -> RigidBody { ... }
    pub fn step(&mut self, dt: f32) { ... }
    pub fn position(&self, body: &RigidBody) -> Vec3 { ... }
}

#[leo::module]
fn physics(m: &mut leo::Module) {
    m.add_class::<World>();
    m.add_class::<RigidBody>();
}
```

- Type mapping: Rust numbers, `bool`, `String`/`&str`, `Vec<T>`, `Option<T>` and
  `Result<T, E>` map to their Leo counterparts; `glam::Vec2/3/4` map to `vec2/3/4` with no
  conversion cost (same layout); `#[leo::export]` structs that are `Copy` become Leo
  `struct`s; everything else becomes an RC'd opaque `class` handle.
- Rust `Result::Err` becomes a Leo `err(...)`; a Rust panic is caught at the boundary and
  becomes a Leo panic, never unwinding into Leo frames.
- Generic Rust APIs are exported per concrete instantiation; trait-heavy APIs get a small
  hand-written wrapper.

**Leo side.** The crate is declared in `leo.toml`, and Leo code imports it like any module:

```toml
[rust-dependencies]
rapier = { binding = "leo-rapier", version = "0.1" }   # curated binding crate
mygame-native = { path = "native/" }                   # your own Rust code
```

```leo
import rapier

let world = rapier.World.new(vec3(0, -9.81, 0))
let ball = world.add_ball(vec3(0, 10, 0), 0.5)
world.step(1.0 / 60.0)
print(world.position(ball))
```

**Build and run.**

- `leo build` compiles the binding crates with `cargo` and statically links them into the
  native executable.
- `leo run` loads them as a prebuilt dynamic library (built once, cached per crate version and
  Leo runtime version), so scripts and hot reload keep working. The Leo-facing ABI is a
  C-compatible function table generated by `leo-bind`, which sidesteps Rust's unstable ABI.
- The type checker reads a generated `.leoi` interface file for each binding, so Leo code
  using a crate is fully type-checked and gets editor completion.

**Curated bindings.** The project ships and maintains ready-made bindings for the popular game
crates (`leo-glam`, `leo-rapier`, `leo-kira`, and a higher-level renderer on `wgpu`) so most
users never write Rust.

**One mechanism, two directions.** `#[leo::export]` is the same API an engine uses to expose
its own types when embedding Leo (§14.4): build it once, get both.

Open details are Q17 in §17.

---

## 15. Tooling and execution modes

### 15.1 One binary [Locked]

```
leo run file.leo        # type-check, then interpret (later: JIT)
leo build               # native executable via Cranelift
leo test                # runs `test "name" { ... }` blocks
leo fmt                 # canonical formatter, no options
leo check               # type-check only (fast, for editors/CI)   [Proposed]
leo repl                # interactive prompt                        [Proposed]
leo lsp                 # language server                            [Proposed, phase 2+]
```

`leo run` always type-checks first. There is no "dynamic mode"; the interpreter runs the same
checked program a native build would.

### 15.2 Execution pipeline [Proposed]

```
source → lexer → parser → AST → name resolution → type check → HIR (typed)
                                                                  │
                     ┌────────────────────────────────────────────┼─────────────────┐
           phase 1: tree-walking interpreter        phase 2: Leo IR (SSA-ish, explicit RC ops)
                                                                  │
                                                ┌─────────────────┴──────────┐
                                          bytecode VM (+ JIT)        Cranelift → native
```

- The **HIR** is the shared contract: fully typed, names resolved, sugar removed.
- **Leo IR** makes RC operations explicit (`inc`, `dec`, `move`), so the RC optimizations of
  §11.3 are IR passes shared by the VM and the native backend.
- The JIT, when it comes, is the same Cranelift backend driven at runtime per function.

### 15.3 Hot reload [Locked goal, Open semantics]

Goal: edit a `.leo` file while the game runs and see the change without restarting.

Proposed v1 semantics: **function-level reload**. When a file changes, it is re-checked; if it
type-checks against the running program, new function bodies replace old ones at the next
call. If a struct's layout changed, reload is refused with a message (or, opt-in, a
`fn migrate(old) -> New` hook). Global state survives reloads. Exact rules are Q14.

### 15.4 Diagnostics [Proposed]

Errors show the source span, a short message, and a hint where possible (Rust/Elm style),
using a crate like `ariadne` or `miette`. The type checker keeps going after the first error
to report several at once.

---

## 16. Implementation plan (Rust)

### 16.1 Workspace layout [Proposed]

```
leo/
  Cargo.toml                 # workspace
  crates/
    leo-syntax/              # lexer, parser, AST, spans
    leo-check/               # name resolution, type inference/checking, HIR
    leo-interp/              # phase 1 tree-walking interpreter
    leo-ir/                  # phase 2 IR + RC optimization passes
    leo-vm/                  # phase 2 bytecode VM
    leo-codegen/             # phase 2 Cranelift backend (AOT + JIT)
    leo-runtime/             # RC heap, strings, collections, FFI, embedding API
    leo-bind/                # #[leo::export] macros for embedding and Rust crate bindings (§14.5)
    leo-std/                 # standard library (Leo + Rust natives)
    leo-cli/                 # the `leo` binary
  std/                       # .leo sources of the standard library
  tests/                     # golden tests: .leo files with expected output/errors
  docs/DESIGN.md
```

### 16.2 Key technical choices [Proposed]

- **Parser:** hand-written recursive descent with Pratt parsing for expressions. Better error
  recovery and messages than a generator.
- **Type checker:** bidirectional checking with local unification for generics and closures
  (no global Hindley-Milner). Matches §5.3 and keeps errors local.
- **Testing:** golden-file tests from day one (`tests/*.leo` with `// expect:` comments for
  output and `// error:` for diagnostics). Every feature lands with tests.
- **Runtime values (phase 1):** a Rust `enum Value` with `Rc<RefCell<...>>` for heap objects.
  Simple, and good enough to validate the language. Phase 2 replaces it with a custom heap
  and explicit RC.

### 16.3 Roadmap [Locked phases, Proposed milestones]

**Phase 1 — the language works** (lexer, parser, type checker, tree-walking interpreter)

1. Lexer + parser for the full grammar, with `leo fmt`-quality spans. Golden tests.
2. Type checker: primitives, functions, structs, enums, `match` exhaustiveness.
3. Interpreter: run checked programs; `print`, strings with interpolation, `List`, `Map`.
4. Generics, interfaces, closures.
5. `Result`/`Option`, `?`, `or`, unused-Result warning.
6. Modules and `import`; minimal std (`fs`, `math`, `time`, `os`).
7. `leo test`, `leo fmt`. **Milestone:** write real scripts in Leo (e.g. a file-renaming
   tool, a JSON report generator).

**Phase 2 — fast** (IR, bytecode VM / Cranelift, RC optimizations)

1. Leo IR with explicit RC ops, plain RC.
2. Bytecode VM replacing the tree-walker for `leo run`.
3. Cranelift AOT for `leo build`.
4. RC optimizations: last-use moves, borrowed params, reuse, struct copy elision.
5. `own` / `borrow` checking.
6. Benchmarks vs Python, Lua and LuaJIT; regressions tracked in CI.

**Phase 3 — games** (bindings + first demo)

1. C FFI + `@repr(C)` + vector types fully SIMD-backed.
2. raylib bindings (simplest API to start), SDL later.
3. Hot reload.
4. Rust embedding API and `leo-bind` (`#[leo::export]`), shared by both directions.
5. Rust crate bindings via `leo.toml`, with the first curated bindings (`leo-glam`,
   `leo-rapier`, then `leo-kira`).
6. **Milestone:** a small, complete game (e.g. a top-down arena shooter) written in Leo, run
   both with hot reload and as a native build.

---

## 17. Open questions

These need a decision before or during phase 1. Each has a recommendation; none are decided.

| # | Question | Options | Recommendation |
|---|----------|---------|----------------|
| Q1 | Boolean operators | `&&` `\|\|` `!` vs `and` `or` `not` | `&&` `\|\|` `!`, so `or` is free for Result fallback without ambiguity |
| Q2 | Optional shorthand | `T?` sugar for `Option[T]`, or always `Option[T]` | Add `T?`; it's the most common type in real code and reads well (`parent: weak Node?`) |
| Q3 | Reference types and collection semantics | (a) `struct` values + `class` references, collections by reference; (b) everything is a value with copy-on-write (Swift); (c) only structs, explicit `Rc[T]` for sharing | (a). Matches Python/Go intuition, keeps the RC story simple. (b) is cleaner but surprises scripters and needs uniqueness checks everywhere |
| Q4 | Implicit return | Last expression of a block is its value (Rust) vs explicit `return` only (Go) | Explicit `return` in functions; `if`/`match` usable as expressions; single-expression closures return implicitly |
| Q4b | Short lambda syntax | `fn(x) { x * 2 }` only, or also `\|x\| x * 2` / `x => x * 2` | Only `fn(x) { ... }` in v1; revisit after writing real code |
| Q5 | Deep immutability | `let` is shallow vs `let` freezes reference types too | Shallow in v1 |
| Q6 | Integer overflow | Trap always / trap in debug, wrap in release / wrap always | Trap in debug and `leo run`, wrap in release, explicit `wrapping_add` etc. available |
| Q7 | Interface conformance | Nominal `impl I for T` vs structural (Go) | Nominal |
| Q8 | `own`/`borrow` rules | Need explicit `move x` at call sites? How strict is local `borrow`? | Infer moves (no `move` keyword); keep `borrow` lexical and conservative; finalize in phase 2 when we have real profiles |
| Q9 | Cycle collection | `weak` + leak detector only, or add a backup cycle collector | `weak` + leak detector for v1; revisit after the game demo |
| Q10 | Concurrency model | Isolated workers + messages, shared-memory threads with atomic RC, async/await | Isolated workers; defer design to after phase 2 |
| Q11 | Enum variant lookup | Always qualified, contextual `.Variant`, or unqualified in `match` only | Unqualified in `match`, plus contextual `.Variant` where the type is known |
| Q12 | Operator overloading | Built-in math types only vs via interfaces (`Add`, `Mul`) | Built-in only in v1 |
| Q13 | `unsafe` | Mark raw pointer use with `unsafe { }` or not | Yes, an `unsafe` block, only needed for `ptr[T]` operations; plain `extern` calls don't need it |
| Q14 | Hot reload semantics | Function-level only, or also struct layout migration, or full state reset | Function-level, refuse layout changes in v1 |
| Q15 | Script top-level code | Allow statements at top level in scripts, or always require `main` | Allow in scripts, require `main` in packages |
| Q16 | Name | "Leo" is a working name; there are existing projects called Leo (e.g. the Aleo smart-contract language) | Decide before publishing anything; file extension `.leo` is also used by Aleo |
| Q17 | Rust crate bindings details | Static link only vs also dynamic loading for `leo run`; who maintains curated bindings; how much of a crate's API to expose | Static for `leo build`, cached dynamic library for `leo run`; start with glam and rapier only, expanding as the demo game needs |

---

## 18. Prior art

| Language | What Leo takes from it |
|----------|------------------------|
| **Go** | Simple syntax, newline-terminated statements, untyped constants, `defer`, one formatter |
| **Python** | Readability bar, scripting ergonomics, f-string style interpolation |
| **Rust** | `Result`/`?`, enums + exhaustive `match`, `impl` blocks, Cargo-style tooling |
| **Swift** | Value structs vs reference classes, ARC with optimization, `weak` |
| **Lobster** | Compile-time RC elision for a game-focused language; closest relative |
| **Koka / Perceus** | Precise RC with reuse analysis (FBIP) |
| **Vale** | Ideas on generational references and region borrowing (for future work) |
| **Odin / Jai** | Game-dev ergonomics: vector types, C layout, easy FFI |
| **Lua** | The embedding experience Leo should match or beat |
| **GDScript / Wren** | Scripting languages built for games and hot reload |
