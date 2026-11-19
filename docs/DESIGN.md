# Colibri Language Design

**Status:** Draft v0.3, for review. Nothing here is implemented yet.
**Date:** 2026-11-19

This document describes what Colibri is, the decisions already agreed, the concrete proposals
that follow from them, and the open questions that still need a call. Every section is
tagged with one of three markers:

- **[Locked]**: agreed during the brainstorm. Changing it means re-opening the discussion.
- **[Proposed]**: a recommendation filling in details the brainstorm didn't cover. Review and push back.
- **[Open]**: a real choice that needs a decision. Listed again in [§18 Open questions](#18-open-questions).

### Changes in v0.3

santiago renamed the language to **Colibri** (from the working name Leo, §17.2), revised three
[Locked] decisions on 2026-11-19, and confirmed the reordered roadmap:

- **The bytecode VM is the product** (§1, §16.2). It runs scripts, hot reload, embedding, the web
  and shipped games: `colibri build` bundles the VM and the program's bytecode into one executable.
  Native AOT via Cranelift is a **stretch goal**, started only if a profile of a real program
  asks for it. There is no JIT.
- **Plain reference counting is the memory model** (§11): value-type structs, refcounted
  reference types and `weak`. Compiler elision of refcount operations is a later optimization;
  `own` and `borrow` stay reserved words, and the ownership checker is not designed now.
- **Game scripting and embedding come first**, general scripting second (§1). The embedding API
  is built from phase 1, and the scripting-oriented std (`http`, `regex`, subprocesses) moves to
  a later phase (§13.1, §17.5).

### Changes in v0.2

v0.2 answers the advisor review on PR #1. The main changes:

- **Memory safety** is now an explicit goal (§1), and the semantics that decide it are pinned
  down: a normative lifetime rule for drops (§11.2), places as copy-in/copy-out with a runtime
  guard (§11.6), tasks as isolates (§14.1), and one unwinding strategy for panics and
  cancellation (§10.4).
- **Roadmap reordered** (§17.5): the first game runs on the interpreter, then the bytecode VM,
  then Cranelift for shipping builds. The JIT is dropped. *(Confirmed by santiago; refined in v0.3.)*
- **Grammar rules** written down before the parser exists: newlines inside brackets and
  leading-dot chains (§3.2), brace ambiguities (§3.5), keywords (§3.3), and the capitalisation
  rule in patterns (§8.2).
- **One rule for function bodies**: `{ }` bodies use `return`, `=> expr` bodies return the
  expression. This also gives the short lambda (§6).
- **Numbers**: overflow traps in every mode; `f32` widens to `float` implicitly; vector-by-scalar
  arithmetic accepts a `float` (§5.1).
- **Modules**: a package is type-checked as one unit, import cycles are allowed inside a package,
  and the default visibility is the package (§13).
- **Coroutines** use the `go` keyword, thread tasks use `task.run(...)`, and `spawn`/`start` are
  ordinary identifiers again (§14).
- **`vec3` is 12 bytes** (§15.1). **No dylib for Rust bindings**; a built-in game kit instead
  (§15.5, §15.6).
- **New sections**: tuples, ranges and type aliases (§5.2), serialization and reflection (§7.5),
  iteration (§6.3), module-level values (§4.2), attributes and conditional compilation (§3.6),
  platform targets (§16.6), editions (§16.7), performance targets (§17.4).

---

## Table of contents

1. [Goals and non-goals](#1-goals-and-non-goals)
2. [Colibri at a glance](#2-colibri-at-a-glance)
3. [Lexical structure and grammar rules](#3-lexical-structure-and-grammar-rules)
4. [Bindings, mutability and module-level values](#4-bindings-mutability-and-module-level-values)
5. [Types](#5-types)
6. [Functions, closures and iteration](#6-functions-closures-and-iteration)
7. [Structs, classes and methods](#7-structs-classes-and-methods)
8. [Enums and pattern matching](#8-enums-and-pattern-matching)
9. [Interfaces and generics](#9-interfaces-and-generics)
10. [Error handling](#10-error-handling)
11. [Memory model](#11-memory-model)
12. [Strings](#12-strings)
13. [Modules and packages](#13-modules-and-packages)
14. [Concurrency: tasks and coroutines](#14-concurrency-tasks-and-coroutines)
15. [Game-dev features](#15-game-dev-features)
16. [Tooling and execution modes](#16-tooling-and-execution-modes)
17. [Implementation plan (Rust)](#17-implementation-plan-rust)
18. [Open questions](#18-open-questions)
19. [Prior art](#19-prior-art)

---

## 1. Goals and non-goals

### Goals [Locked, revised 2026-11-19]

- **Genuinely usable**, not a toy. Someone should be able to ship a small game, script a Rust
  game engine, or write a real automation script in Colibri.
- **Simple syntax**, in the spirit of Go and Python, with braces. Small keyword set, one
  obvious way to do common things.
- **Static types with inference.** Errors are caught before the program runs, but day-to-day
  code rarely needs type annotations inside function bodies.
- **Target domains, in priority order:** (1) **game scripting**, both standalone games and
  scripts **embedded in Rust engines**; (2) **general scripting**. When the two pull in
  different directions, games and embedding win.
- **The bytecode VM is the product.** One engine runs scripts (`colibri run`), hot reload,
  embedding, the web and shipped games (`colibri build` bundles the VM with the program's
  bytecode). Native AOT via Cranelift is a **stretch goal**, built only if a profile of a real
  program shows the VM can't meet a target in §17.4. No JIT.
- **Predictable memory without a tracing GC: plain reference counting**, with value-type
  structs, refcounted reference types and `weak` references for cycles. Compiler elision of
  refcount operations is a later optimization that may not change meaning; `own` and `borrow`
  are reserved words, and the ownership checker is not designed now.
- **Embeddable** in Rust game engines as a first-class use from phase 1, and **easy C interop**.

### Goals [Proposed]

- **Memory safe outside `unsafe` and FFI.** A Colibri program that uses no `unsafe` block and no
  `extern` declaration cannot read freed memory, write out of bounds, or race on memory. Every
  feature in this document (places, `borrow`, drop timing, thread sends, coroutines) is designed
  against this rule; where static checking is impractical, a runtime check panics instead.
- **The same program means the same thing in every mode.** The interpreter, the VM (standalone,
  embedded or on the web) and native code, if it is ever built, agree on results, panics and drop
  order. Only speed differs.

### Non-goals

- Audio-plugin / hard-real-time DSP as a first-class target (explicitly dropped).
- `async`/`await` (Colibri uses uncolored tasks and coroutines instead, §14).
- Inheritance, exceptions, macros, compile-time metaprogramming, and user-defined
  **arithmetic** operators in v1. (`==` and `<` are user-definable through `Eq` and `Ord`;
  arithmetic on structs through interfaces is planned after phase 1, Q12.)
- Competing with Rust or C++ on peak performance. "Fast enough for a 2D/3D indie game and
  much faster than Python" is the bar (numbers in §17.4).
- A JIT, at any point; M:N task scheduling in v1.
- Committing to native AOT or an ownership checker in v1. Both are stretch goals.
- A package registry in v1.

### Design principles [Proposed]

1. **Readable by a Python or Go programmer on day one.** If a feature needs a paragraph to
   explain, it needs a strong reason to exist.
2. **Performance is opt-in, never required.** Plain code is correct and reasonably fast. For the
   hot 5% the tools today are C-layout structs and moving hot code into Rust through the
   embedding API (§15.4); `own`/`borrow` are reserved for later. An annotation in a library
   never causes an error in a caller that didn't opt in.
3. **The optimizer must not change meaning.** Refcount elision, moves and backend choice are
   invisible to programs except through speed. Where an optimization *could* be observed (for
   example by shortening a lifetime), the language rule says it may only happen when it can't be.
4. **Great error messages are a feature**, budgeted for from the first parser.

---

## 2. Colibri at a glance

```colibri
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

fn greet(name: str) -> str => "Hello, {name}!"

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
    hero = apply(hero, .Move(vec2(1, 0)))     // `.Move`: the expected type is Command
    print("{hero.name} is at {hero.pos} with {hero.hp} hp")
}
```

---

## 3. Lexical structure and grammar rules

### 3.1 Source files [Proposed]

- UTF-8, extension `.colibri` (§17.2 has the naming details).
- Comments: `// line` and `/* block */` (nestable). Doc comments: `///`.

### 3.2 Statement termination [Proposed]

Braces delimit blocks; **newlines end statements**, with no semicolons in normal code. The rule
is Go's, plus two fixes for Go's most common "why doesn't this parse" moments:

1. **Go's rule:** a newline ends a statement if the last token on the line could end one (an
   identifier, literal, `)`, `]`, `}`, `return`, `break`, `continue`). So a line can continue
   after an operator or comma.
2. **Inside `( )` and `[ ]`, newlines never end a statement.** The innermost open bracket decides,
   so a `{ }` closure body inside a call still gets normal statement endings. Trailing commas
   in multi-line calls and literals are allowed but not required.
3. **A line that starts with `.`, `or`, `&&`, `||` or `else` continues the previous
   statement**, so leading-dot method chains work. The one exception is a `.` followed by a
   capitalised name, which starts a new line: `.Circle(r) =>` is a `match` arm using the variant
   shorthand of §8.2, and method names are lower-case.

```colibri
let total = price * qty +
            shipping                        // rule 1: line ended with `+`

let names = players
    .filter(fn(p) => p.alive())             // rule 3: leading `.`
    .map(fn(p) => p.name)

draw_text(
    "score: {score}",                        // rule 2: inside ( )
    x, y
)
```

Semicolons are allowed to put several statements on one line, and `colibri fmt` removes them
otherwise.

### 3.3 Keywords [Proposed]

```
fn let var const type return if else for in while break continue match
struct class enum interface impl import pub as is self
own borrow weak extern unsafe defer test go
some none ok err
or and not
true false
```

- `some`, `none`, `ok`, `err` are reserved because they appear in patterns (`ok(c) =>`,
  `none =>`). If they were ordinary names, `none =>` in a `match` would be indistinguishable
  from a binding that matches everything.
- Booleans use `&&`, `||` and `!` (Q1). `or` is the Result/Option fallback operator (§10.3).
  `and` and `not` are reserved only so that Python habits get a targeted error
  ("use `&&` instead of `and`").
- `is` is the identity test for reference types (`a is b`, §5.3).
- `go` starts a coroutine (§14.2). Thread tasks are a library call (`task.run`), so common
  identifiers like `spawn`, `start` and `run` stay free.

### 3.4 Literals [Proposed]

| Kind | Examples |
|------|----------|
| Integer | `42`, `1_000_000`, `0xFF`, `0b1010` |
| Float | `3.14`, `1e-9`, `2.0` |
| String | `"hello {name}"`, raw: `r"C:\path\{not interpolated}"` |
| Multi-line string | `"""..."""` (strips common indentation), raw: `r"""..."""` |
| Char | `'a'`, `'\n'` |
| List | `[1, 2, 3]` |
| Map | `{"a": 1, "b": 2}`, empty map `{:}` |
| Set | `Set([1, 2, 3])` (no dedicated literal) |
| Tuple | `(1, "a")`, unit `()` |
| Range | `0..10` (exclusive), `0..=10` (inclusive) |
| Bool | `true`, `false` |

Numeric literals are **untyped constants** (as in Go): `1` becomes `int`, `float`, `f32` or
`u8` depending on what it's used as, and defaults to `int`/`float` when nothing constrains it.
This is what makes `vec2(1, 0)` work without `vec2(1.0f, 0.0f)` noise. **Literal polymorphism
ends at the binding:** after `let x = 1`, `x` is an `int`.

### 3.5 Brace ambiguities [Proposed]

Written down now so the parser has one rule for each:

- **Block vs map literal.** `{` opens a **block** at the start of a statement and directly after
  `=>`, `else`, an `if`/`while`/`for`/`match` head, or a function header. Everywhere else it opens
  a **map literal**. `{}` is therefore an empty block in statement position; an empty map is
  written `{:}` (or `Map[K, V]()`). A `match` arm that yields a map writes `=> ({"a": 1})`.
- **Struct literal vs block in a condition.** No bare struct literal in the head of `if`,
  `while`, `for` or `match` (Rust's rule): `if p == (Player { name: "x" }) { ... }`.
- **Generic instantiation vs indexing.** `max[f32](a, b)` and `list[i](x)` parse to the same
  ambiguous AST node and are resolved after name resolution, as Go's parser does, instead of
  guessing in the parser.

### 3.6 Attributes and conditional compilation [Proposed]

`@name` or `@name(args)` before a declaration is the one general attribute syntax. Built-in
attributes: `@repr(C)` (§15.2), `@link("lib")` (§15.3), `@deprecated("msg")`, `@inline`, and
`@when(cond)` for conditional compilation:

```colibri
@when(os == "windows")
@link("user32")
extern "C" { fn MessageBoxA(...) -> i32 }
```

`@when` conditions can test `os`, `arch`, `debug`, and feature flags from `colibri.toml`. User-defined
attributes are not part of v1.

---

## 4. Bindings, mutability and module-level values

### 4.1 Local bindings [Proposed]

```colibri
let x = 10        // immutable binding
var y = 20        // mutable binding
y += 1
const MAX_HP = 100   // compile-time constant, module level or local
```

- `let` cannot be reassigned, and for value types (structs, vectors) its fields cannot be
  mutated either.
- `let` on a reference type (a `class`, a `List`) prevents rebinding but not mutating the
  object behind it, the same as `final` in Java or `const` in JavaScript. Deep immutability is
  not part of v1 (Q5).
- Shadowing is allowed in nested blocks, and is a warning in the same block.

### 4.2 Module-level values [Proposed]

```colibri
const GRAVITY = 9.81                  // compile-time constant
let config = load_defaults()          // initialised once, immutable binding
var score = 0                         // mutable module state
```

- **Initialisation order:** module-level `let` and `var` initialisers run once, before `main`
  (or before the script's first statement), in dependency order across the whole program. A
  cycle between initialisers is a compile error naming the chain, as in Go.
- **Threads:** non-constant module state belongs to the main isolate. Reading or writing it from a
  thread task (§14.1) is a compile error where the compiler can see it, and a panic otherwise.
  `const` values are shared freely.
- **Hot reload:** module-level values survive a reload and their initialisers do not re-run
  (§16.3).

---

## 5. Types

### 5.1 Primitive types and numeric rules [Proposed]

| Type | Meaning |
|------|---------|
| `int` | 64-bit signed integer (default integer) |
| `i8 i16 i32 i64`, `u8 u16 u32 u64` | sized integers, mainly for FFI and data formats (`i64` is the same type as `int`) |
| `float` | 64-bit float (default float) |
| `f32`, `f64` | sized floats (`f64` is the same type as `float`) |
| `bool` | `true` / `false` |
| `char` | a Unicode scalar value |
| `str` | immutable UTF-8 string (§12) |
| `vec2 vec3 vec4` | `f32` vectors (§15.1) |
| `()` | unit; functions without `-> T` return it |

**Conversions.** Only two conversions are implicit, both chosen for game code, where every `dt`,
`math.sin(t)` and time value meets an `f32` vector:

1. `f32` widens to `float` implicitly (lossless).
2. Vector-by-scalar arithmetic (`vec * s`, `s * vec`, `vec / s`) accepts a `float` scalar and
   narrows it to `f32`. So `self.pos += self.vel * dt` works with `dt: float`.

Everything else is explicit: between integer widths, from integer to float, and assigning a
`float` to an `f32` (`f32(x)`). Untyped literals (§3.4) adapt without a conversion.

**Overflow traps in every mode** (interpreter, VM, and native code if it is ever built), as in Swift, so
a program can't compute something different once built. `wrapping_add`, `wrapping_mul`,
`checked_add` (returns `Option`) and `saturating_add` exist for hashes, RNGs and fixed-point
code (Q6).

**Bitwise operators [Proposed].** On integer types, `&` (and), `|` (or), `^` (xor), `~x` (not),
`x << n` and `x >> n` work for flags, masks, hashes and random number generators. `& | ^`
need both operands of the same integer type; `~x` has the type of `x`. They bind tighter than
the comparisons and looser than `+ -`, in the order `|` < `^` < `&` < shifts < `+ -`, so
`x & 1 == 0` is `(x & 1) == 0` (unlike C) and `1 << n + 1` is `1 << (n + 1)`; the compound
forms `&= |= ^= <<= >>=` exist as well. The count of a shift may have any integer type; a count
that is negative or not below the bit width of `x` panics ("shift by 64 is out of range for
`int`"). Shifts are bit operations, not arithmetic, so bits shifted out of `<<` are lost
without a trap; `>>` is arithmetic (it keeps the sign) on signed types and logical on unsigned
ones. `&`, `|`, `^` and `~` do not work on `bool` (an error that suggests `&&`, `||`, `!=` or
`!`) or on floats.

**Conversion calls and constants [Proposed].** A number type's name is also its conversion
function: `u8(x)`, `int(x)`, `i32(x)`, `float(x)`, `f32(x)` take any number. Integer to integer
panics when the value does not fit ("value 300 does not fit in `u8`"); float to integer
truncates toward zero and panics on `nan` or a value out of range; integer to float rounds to
nearest. A bare literal argument takes the target type (`u8(256)` is a compile error). In a
binary operator or comparison, an `f32` meeting a `float` widens and the operation is a
`float` one; two `f32` operands are computed in `f32` and the result widens afterwards where a
`float` is wanted. `T.MAX` and `T.MIN` are the extreme values of an integer type (`u8.MAX`,
`int.MIN`); floats also have `T.EPSILON`, `T.INFINITY` and `T.NAN`. Integer types have the
methods `wrapping_add/sub/mul`, `checked_add/sub/mul/div` (returning `T?`),
`saturating_add/sub/mul`, `abs` (signed only; traps on `MIN`), `min`, `max` and `pow` (traps
on overflow); `float` and `f32` have `abs`, `sqrt`, `floor`, `ceil`, `round` (half away from
zero), `min`, `max` and `is_nan`. The rest comes with `math`.

**Integer division truncates toward zero**: `7 / 2` is `3`, not `3.5`. This will surprise
Python users, so the docs say it loudly and `colibri check` hints when an integer division result
flows into a `float`. `%` takes the sign of the dividend (`-7 % 3 == -1`), as in C, Go and
Rust; `math.floor_div` and `math.floor_mod` give Python's behaviour. Integer division by zero
panics; float division follows IEEE 754.

### 5.2 Built-in compound types [Proposed]

| Type | Semantics |
|------|-----------|
| `List[T]` | growable array, literal `[a, b]` |
| `Map[K, V]` | hash map, insertion-ordered, literal `{k: v}` |
| `Set[T]` | hash set, insertion-ordered |
| `T?` / `Option[T]` | `some(x)` or `none`; Colibri has **no null**. `T?` is sugar for `Option[T]` (Q2) |
| `Result[T]`, `Result[T, E]` | §10 |
| `(A, B)` | tuple, a value type; `let (a, b) = f()` destructures |
| `Range[T]` | `a..b`, `a..=b`; iterable, usable in patterns and slicing |
| `[T; N]` | fixed-size array, value type, used in C-layout structs |
| `fn(A, B) -> R` | function / closure type |

- **Multiple return values** are tuples: `fn bounds(xs: List[int]) -> (int, int)`, called as
  `let (lo, hi) = bounds(xs)`.
- **Type aliases:** `type Grid = Map[ivec2, Tile]`. An alias is the same type, not a new one.

### 5.3 Collection semantics [Proposed]

Per Q3, `List`, `Map` and `Set` are **reference types**: assignment shares them, as in Python and
Go. Consequences, written down:

- **A struct copy is shallow.** If `Player` had `inventory: List[Item]`, then
  `var next = p; next.inventory.push(x)` also changes `p.inventory`, since both structs point to
  the same list. This is the Go-slice gotcha. Every type gets a deep `.clone()` from the prelude
  (derived automatically), and the struct docs lead with this paragraph.
- **`==` on collections is structural**, like Python: two lists are equal if their elements are.
  `a is b` tests identity for any reference type.
- **`Hash`:** structs and enums derive it from their fields; classes hash by identity unless
  they implement `Hash`; mutable collections are not hashable (use a tuple, a struct or a frozen
  copy as a key).
- **Missing keys:** `m[k]` panics when `k` is missing; `m.get(k)` returns `V?`, so
  `m.get(k) or 0` reads well.

### 5.4 Type inference [Proposed]

- **Function signatures are annotated; bodies are inferred.** Parameters and return types are
  always written (except `-> ()`), local variables almost never are. Signatures are
  documentation and keep error messages local, and inference stays fast and predictable.
- Inference is bidirectional and local to a function: the expected type flows into
  expressions (`let v: List[f32] = []` works; `[]` alone with no later use is an error asking
  for an annotation).
- Closures can omit parameter types when the expected type is known:
  `items.map(fn(x) => x * 2)`.
- Generic type arguments are inferred at call sites: `max(3, 4)`, with `max[f32](a, b)` as
  the explicit form.

---

## 6. Functions, closures and iteration

### 6.1 Functions [Proposed]

There is **one rule for bodies** (Q4, Q4b): a `{ }` body is a statement block and uses
`return`; an expression body is written with `=>`, the token `match` arms already use.

```colibri
fn add(a: int, b: int) -> int {
    return a + b
}

fn area(self) -> float => math.PI * self.r * self.r

fn log(msg: str, level: str = "info") {     // default argument
    print("[{level}] {msg}")
}

log("starting")
log("disk full", level: "warn")              // named argument
```

- Default arguments and optional named arguments at call sites. No variadics in v1 except
  through `List[T]`.
- `if` and `match` are expressions. A block used as an `if`/`match` arm yields its trailing
  expression, which multi-statement arms need:
  `let s = if x > 0 { "pos" } else { log("neg"); "neg" }`.
- A function returning `Result[()]` (or `Result[(), E]`) that reaches the end of its body
  returns `ok(())`, so fallible procedures don't end in `return ok(())`.
- `pub` controls visibility (§13).
- `defer` runs a statement when the enclosing block exits, including on panic and coroutine
  cancellation (§10.4): `let f = fs.open(path)?` then `defer f.close()`.

### 6.2 Closures [Proposed]

```colibri
let double = fn(x: int) -> int => x * 2
let evens = nums.filter(fn(n) => n % 2 == 0)      // types inferred from `filter`
button.on_click(fn() {                             // block body: uses `return` if needed
    clicks += 1
})
```

- Closures capture variables **by reference** to the enclosing variable (like Go, Python, JS),
  so captured `var`s can be updated. A captured variable is moved into a refcounted cell only
  when a closure mutates it or escapes; otherwise it's captured directly.
- **A `var` struct captured by an escaping closure moves into a shared cell**, so through that
  closure the struct now has reference behaviour: the closure and the enclosing function see the
  same value. This is the expected behaviour; it's stated here so it isn't a surprise.
- **`for` introduces a fresh binding per iteration**, so closures created in a loop capture that
  iteration's value (the fix Go needed in 1.22).
- **Weak captures.** Callbacks are the main source of RC cycles
  (`button.on_click(fn() { self.count += 1 })` leaks when `self` owns `button`). A capture list
  makes a capture weak; inside the closure the name has type `T?`:

  ```colibri
  self.button.on_click(fn() use(weak self) {
      if let some(me) = self { me.count += 1 }
  })
  ```

  Exact spelling is Q19. The leak detector (§11.5) is on by default in `colibri run` and points at
  closures that close a cycle.

### 6.3 Loops and iteration [Proposed]

- `for x in xs` works on anything implementing `Iter[T]` (one method, `fn next(var self) -> T?`)
  or `Iterable[T]` (`fn iter(self) -> Iter[T]`). Lists, maps (yielding `(k, v)`), sets, ranges
  and strings (`s.chars()`, `s.bytes()`) are iterable.
- `for (i, x) in xs.enumerate()`, `for i in 0..n`, `for var e in enemies` (§11.6).
- **Mutating a collection while iterating it panics** ("list modified during iteration"), using
  the same mutation guard as places (§11.6). To remove while iterating, use `xs.retain(fn(x) => ...)`
  or iterate a copy.
- `while cond { }`, `while let some(x) = it.next() { }`, `break`, `continue`, and labelled
  `break 'outer` for nested loops.

---

## 7. Structs, classes and methods

The brainstorm locked "structs are value types (copied)". Games and scripts also need shared,
mutable objects with identity (a scene node, an entity referenced from several places), so Colibri
has two kinds of user-defined record type (Q3 option (a)).

### 7.1 `struct`: value types [Locked semantics, Proposed syntax]

```colibri
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

- Assignment, argument passing and returning copy the value (the compiler avoids physical
  copies when no one can observe the difference, §11.3).
- A struct can contain reference-typed fields (`str`, `List`, a `class`); copying the struct
  copies the reference, not the object (§5.3, shallow copies).
- Structs have no identity: `==` is structural.

### 7.2 `class`: reference types [Proposed]

```colibri
class Node {
    name: str
    children: List[Node] = []
    parent: weak Node              // weak back-reference: reads as `Node?` (§11.5)
}

let root = Node { name: "root" }
let child = Node { name: "child", parent: root }
root.children.push(child)          // root and the list share `child`
```

- Heap-allocated and reference-counted. Assignment shares; it does not copy.
- `==` on classes is identity unless the class implements `Eq`. `is` is always identity.
- **No inheritance.** Sharing behaviour across types is done with interfaces (§9) and
  composition.

### 7.3 Methods [Proposed]

Methods are declared in `impl` blocks, separate from the data:

```colibri
impl Particle {
    fn new(pos: vec2) -> Particle => Particle { pos: pos, vel: vec2(0, 0) }   // static

    fn alive(self) -> bool => self.life > 0             // read-only receiver

    fn step(var self, dt: float) {                      // mutating receiver (structs only)
        self.pos += self.vel * dt                       // vec2 * float: §5.1
        self.life -= dt
    }
}

var p = Particle.new(vec2(0, 0))
p.step(0.016)        // `p` must be `var`
```

- `self` is read-only; `var self` may mutate a struct in place, and the call site needs a
  mutable place (§11.6). On a `class`, `self` can always mutate fields, since the object is
  shared.
- Methods can be added to a type only in the package that defines it, except to implement an
  interface (§9).

### 7.4 Auto-derived interfaces [Proposed]

Structs, enums and tuples derive `Eq`, `Hash`, `Show`, `Clone` and the serialization support of
§7.5 automatically, whenever all their fields support them. No declaration is needed, so
`print("{hero.pos}")` and `if a == b` just work. A type can override any of them with an explicit
`impl`. `Ord` is derived only for enums without payloads and for tuples; structs implement it
explicitly. Classes derive `Show` (fields) and `Clone` (deep), but `Eq` and `Hash` stay identity
unless implemented.

### 7.5 Type descriptors, serialization and reflection [Proposed]

Colibri has no macros, but save games, configs and level data need `json.decode[Player](text)`.
The compiler emits a **type descriptor** for every type (name, fields with types and defaults,
variants, attributes). The same descriptors serve several features, so they're built once:

- `json.encode(x)` and `json.decode[T](text) -> Result[T]`, with `@rename("x")`, `@skip` and
  default-on-missing field handling. Other formats (TOML, a binary format) use the same API.
- Deep copy for thread sends (§14.1), `Show`, `Clone`, the leak detector (§11.5), the debugger
  (§16.5) and hot reload layout checks (§16.3).
- A small read-only reflection API (`reflect.fields[T]()`) for tools such as editors and
  inspectors. Reflection cannot bypass visibility.

---

## 8. Enums and pattern matching

### 8.1 Enums [Proposed]

Enums are tagged unions; variants may carry data:

```colibri
enum Shape {
    Circle(radius: float)
    Rect(w: float, h: float)
    Point
}

enum Dir { North, East, South, West }     // C-like enum, single line
```

Enums are value types. `Option[T]` and `Result[T, E]` are ordinary enums from the prelude.

### 8.2 `match` [Proposed]

```colibri
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
- Patterns: literals, ranges (`0..10`, `0..=9`), variants, struct destructuring
  (`Vec2i { x, y }`), tuples (`(a, b)`), `_`, bindings with `as` (`Move(d) as cmd`), guards with
  `if`.
- **Capitalisation rule in patterns.** A capitalised identifier in a pattern (`Quit`, `MAX_HP`)
  is a variant or constant and **must resolve**; a lower-case identifier is a new binding. So a
  typo like `Quti =>` is an error instead of a silent catch-all. `some`, `none`, `ok` and `err`
  are reserved words (§3.3) and work as variants.
- **Variant lookup** (Q11): bare variant names inside patterns; `.Variant` wherever the expected
  type is known (`apply(hero, .Move(d))`, `let s: Shape = .Circle(2)`); `Shape.Circle(2)`
  otherwise. Variants are not placed in module scope, since that collides with types (a
  `Shape.Circle` variant and a `struct Circle` can coexist).
- `if let some(x) = maybe { ... }` and `while let` for single-pattern tests.

---

## 9. Interfaces and generics

### 9.1 Generics [Locked syntax, Proposed details]

```colibri
fn max[T: Ord](a: T, b: T) -> T => if a > b { a } else { b }

struct Pair[A, B] {
    first: A
    second: B
}

enum Tree[T] {
    Leaf
    Node(left: Tree[T], value: T, right: Tree[T])
}
```

- Square brackets, as locked. Indexing also uses `[]`; see §3.5 for how the parser handles
  `max[f32](a, b)`.
- **Bounds** use interfaces: `T: Ord`, `T: Hash + Eq`.
- **Implementation strategy:** the interpreter and VM run generic code with a uniform value
  representation; native builds **monomorphize** (one copy per concrete type). Both are
  invisible to the user.
- Recursive enums like `Tree[T]` are boxed automatically (the compiler inserts an RC box for
  recursive fields), so users never write `Box`.
- **No variance:** `List[Circle]` is not a `List[Shape]`. Convert with
  `circles.map(fn(c) => c as Shape)`.

### 9.2 Interfaces [Proposed]

```colibri
interface Shape {
    fn area(self) -> float
    fn name(self) -> str => "shape"              // default method
}

struct Circle { r: float }

impl Shape for Circle {
    fn area(self) -> float => math.PI * self.r * self.r
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

- **Nominal, explicit** implementation (`impl Shape for Circle`), not Go-style structural (Q7).
  Explicit impls give better error messages and allow default methods.
- An interface name used as a type (`List[Shape]`) means a dynamically dispatched value. Used as
  a bound (`T: Shape`), it is statically dispatched.
- **A struct in an interface value stays a value.** After `let a: Shape = circle; var b = a`,
  `a` and `b` are independent: assigning an interface value that holds a struct copies it
  (implemented as copy-on-write on the box, so the copy is lazy). A `var self` method called
  through `b` changes only `b`. An interface value holding a class shares it, as classes always do.
- **Object safety:** an interface method with its own type parameters (`fn map[U](self, ...)`)
  can't be called through a dynamically dispatched value, because native builds monomorphize.
  Such methods are only available through a bound (`T: I`). The compiler explains this when it
  happens.
- Built-in interfaces in the prelude: `Eq`, `Ord`, `Hash`, `Show` (used by interpolation and
  `print`), `Clone`, `Iter[T]`, `Iterable[T]`, `IntoError` (§10), `Drop`.
- **`Drop` is limited to `class` types and host handles.** A copyable value type with a
  destructor would run it once per copy, which is the C++ copy-constructor problem Colibri otherwise
  avoids. Structs that own a resource hold it in a class field.
- Orphan rule: `impl I for T` must live in the package of `I` or of `T`.

---

## 10. Error handling

[Locked] Errors are values. There are no exceptions.

### 10.1 Types

- `Result[T, E]` is an enum: `ok(T)` or `err(E)`.
- `Result[T]` is shorthand for `Result[T, Error]`, where `Error` is a built-in type. Any type
  implementing `IntoError` converts to it with `?`.

```colibri
enum ParseError {
    UnexpectedEof
    BadToken(line: int, text: str)
}

impl IntoError for ParseError {}         // default message comes from `Show`

fn parse(src: str) -> Result[Ast, ParseError] { ... }

fn run(path: str) -> Result[()] {         // default error type
    let src = fs.read(path)?
    let ast = parse(src)?                 // ParseError converts to Error via IntoError
    eval(ast)?
}                                         // falls off the end: ok(()) (§6.1)
```

### 10.2 `Error` is inspectable [Proposed]

`Error` keeps the **original typed value**, so a caller can still tell errors apart after `?`:

```colibri
match load_level(path) {
    ok(level) => play(level)
    err(e) if e.is[fs.Error]() => match e.as[fs.Error]().expect("checked") {
        NotFound         => use_default_level()
        PermissionDenied => fail("can't read {path}")
        _                => fail("{e}")
    }
    err(e) => fail("{e}")
}

let text = fs.read(path).context("loading level {path}")?   // wrap with context
```

- `e.is[T]() -> bool` and `e.as[T]() -> T?` use the runtime type descriptors of §7.5.
- `e.context(msg)` wraps an error with a message; `e.cause()` walks the chain; `{e}` prints the
  whole chain. Debug builds capture a backtrace.
- The standard library returns real enums underneath (`fs.Error.NotFound`,
  `http.Error.Timeout`), never just strings.

### 10.3 Handling forms [Locked, Proposed details]

```colibri
let cfg = load_config()?                              // propagate
let cfg = load_config() or Config.default()           // fallback value
let cfg = load_config().expect("config is required")  // panic with message
match load_config() {
    ok(c)  => use(c)
    err(e) => log("bad config: {e}")
}
```

- `?` works in functions returning `Result` (converting the error through `IntoError`) and in
  functions returning `Option` (for `Option` values).
- **`or` with a handler block.** The handler is a block **of the enclosing function**, not a
  closure, so `return`, `break` and `continue` inside it act on the enclosing function or loop.
  The block must yield a value or diverge:

  ```colibri
  let cfg = load_config() or |e| { log("bad config: {e}"); return }

  for path in paths {
      let text = fs.read(path) or { continue }
      ...
  }
  ```

  `|e|` binds the error (it is omitted for `Option`, or when the error isn't needed). This is
  Zig's `catch |e| { ... }`.
- **Precedence:** postfix `?` binds tightest (with calls and field access). `or` binds looser
  than every other binary operator, including `||`, `&&` and comparisons, and tighter than
  assignment. It chains left to right: `a or b or c`.
- **Right-hand side types:** in `a or b` with `a: Result[T]` or `T?`, `b` may be a plain `T`
  (result type `T`) or itself a `Result[T]`/`T?` (result type is `b`'s type), so fallbacks chain:
  `cache.get(k) or disk.load(k) or fetch(k)?`.
- **Unused `Result` is a warning** (`let _ = f()` silences it explicitly).
- `main` may return `Result[()]`; an `err` prints the error chain and exits with code 1.

### 10.4 Panics and unwinding [Proposed]

Some failures aren't recoverable values: out-of-bounds index, integer overflow, `.expect()` on
`err`, integer division by zero, a failed mutation guard (§11.6). These **panic**: print a message
and stack trace and abort the current program, or the current task (surfacing as an `err` from
its join, §14.1), or, when embedded, return an `Err` to the host without crashing the host
process. Panics cannot be caught in Colibri code.

**Unwinding.** A panic, a coroutine cancellation (§14.2) and a hot-reload restart all abandon a
stack. All three must release every local on it and run every `defer`, or they leak and leave
`Drop` resources open. The strategy, identical in every engine:

- **Interpreter and VM:** walk the Colibri stack, which is a heap object, running cleanups.
- **Native code (only if the AOT stretch goal is built):** panics and cancellation are a **hidden error return**. Every call
  that can panic returns a flag the caller checks; on the slow path the caller runs its cleanups
  (drops and `defer`s) and returns the flag upward. No unwind tables or landing pads are needed,
  it works the same on every platform Cranelift targets, and coroutine cancellation reuses the
  same paths. The cost is one predictable branch after calls that can panic; calls proven not to
  panic skip it.
- Rust panics in host functions are caught at the boundary and turned into Colibri panics; Colibri panics
  never unwind through Rust or C frames.

---

## 11. Memory model

[Locked, revised 2026-11-19] **Plain reference counting is the model.** Structs are value types;
`str`, collections, classes and closures are refcounted; `weak` references break cycles; objects
are freed deterministically when their last owner goes. Compiler elision of refcount operations
(§11.3) is a later optimization that must not change meaning, and `own`/`borrow` (§11.4) are
reserved words with no checker designed now.

[Proposed] Everything in this section is designed against the memory-safety goal in §1: safe
code cannot observe freed memory, whatever a later optimizer does.

### 11.1 What is reference counted

| Kind | Representation |
|------|----------------|
| `int`, `float`, `bool`, `char`, `vecN`, C-layout structs of those | plain values, no RC |
| `struct`, tuple | plain value; its reference-typed fields are counted |
| `enum` | value; payloads that are reference types are counted |
| `str`, `List`, `Map`, `Set`, `class`, closures, interface values, recursive enum boxes | heap objects with a refcount header |

Refcounts are **non-atomic** (cheap). The header has one bit marking an object as
**thread-shared**; only those objects take the atomic path (§14.1).

### 11.2 The lifetime rule (normative) [Proposed]

Moving a free from scope end to a variable's last use is observable: through `Drop` guards
(`let batch = renderer.begin_batch()`), through `weak` references going `none` early, through
raw pointers handed to C, and through locals held across a coroutine `wait`. So **the lexical
rule is the language semantics**, in every engine and phase:

- A binding owns its value **until the end of its scope**. Bindings are dropped in reverse
  declaration order. Temporaries are dropped at the end of the statement that created them.
- Ownership leaves a binding only by an explicit transfer: a `return`, a store into a field,
  collection or captured cell, or a send to another task.
- `Drop` hooks therefore run at a predictable point, which is what makes deterministic
  destruction useful for games (textures, sounds and file handles are released when the last
  owner goes).

A future optimizer (§11.3) may remove increment/decrement pairs and reuse memory, but it may only shorten a
lifetime when **nothing can observe it**: no `Drop` anywhere in the type, and the object provably
has no weak references and no derived raw pointers.

The phase 1 interpreter is built to exactly this rule, and drop-order cases are part of the
golden tests (§17.3), so later engines and optimizations are checked against it.

### 11.3 Later: compiler-inferred elision [Proposed, not scheduled]

Not part of the v1 plan (§17.5). It is listed so the IR and runtime don't close the door on it.
Each pass is built only when profiles of real programs ask for it, and must obey §11.2. The
known techniques, all within that rule:

1. **Borrowed parameters** (Lobster, Swift's `guaranteed` convention). By default a function
   *borrows* its reference parameters: the caller keeps the reference alive, the callee does
   no RC unless it stores or returns the value. This shortens no lifetime.
2. **Moves into storage and returns.** When a binding's last use stores or returns it, the
   reference is transferred instead of incremented and decremented. This is an ownership
   transfer under §11.2, so it shortens no lifetime either.
3. **Reuse analysis** (Perceus / Koka FBIP). When a uniquely owned object dies and an object of
   the same size is allocated right after (e.g. `list.map` producing a new list), the memory is
   reused in place.
4. **Early frees** (last-use drops) only for objects that satisfy the "nothing can observe it"
   condition of §11.2.
5. **Struct copy elision.** A struct passed to a read-only parameter is passed by pointer;
   it is only physically copied when the callee or the caller could observe a difference.

### 11.4 `own` and `borrow` [Locked: reserved words only]

`own` and `borrow` are reserved keywords now, so adding them later is not a breaking change.
Their semantics and the checker are **not designed now**. Any future design must respect three
constraints from the review on PR #1:

- **`own` never causes an error in the caller.** If the caller uses a value again after passing
  it to an `own` parameter, the compiler keeps a reference instead of reporting a move error.
  No `move` keyword.
- **`borrow` is parameter-level only** (the callee may not store, return or capture the value).
  No local `borrow let`, which can't be checked when classes and collections alias freely.
- Code without annotations is always valid; annotations only *add* guarantees.

### 11.5 Cycles and `weak` [Locked]

RC can't free cycles. Colibri's stance:

- **`weak T` reads directly as `T?`**, with no `.get()` call, the way Swift's weak variables
  do: `if let some(p) = node.parent { ... }`. Assigning a `T` to a `weak T` field stores a weak
  reference.
- Common back-pointers (parent, owner, observer lists) should be `weak`, and closures can
  capture weakly (§6.2).
- Known cycle sources the docs call out: parent/child links, callbacks capturing their owner,
  and **suspended coroutines**, whose stacks hold strong references to their arguments and
  locals (§14.2).
- **Leak detector**, on by default in `colibri run` and available as `colibri run --leaks` elsewhere:
  on exit, and on demand via `mem.report_cycles()`, it reports objects kept alive only by cycles,
  with allocation sites. It is built as **trial deletion** (Python-style), which is a cycle
  collector that reports instead of freeing.
- No automatic cycle collector in v1 (Q9). Because the detector already is one, it can later be
  exposed as an explicit `mem.collect_cycles()` at level-load boundaries, where games can afford
  the pause.

### 11.6 Places: mutating values inside containers [Proposed]

Because structs are values, `list[i].hp -= 1` must update the element, not a copy. Indexing,
field access and `for var x in list` therefore produce **places** when used on the left of an
assignment, with a compound assignment, or as a `var self` receiver:

```colibri
for var e in enemies {     // `e` is a place in the list
    e.hp -= 1
}
enemies[0].pos += vec2(1, 0)
enemies[0].step(dt)        // `var self` method on a place
```

**Semantics: copy-in/copy-out** (Swift's `inout` model). The element is read, the body or
method runs on that value, and the result is written back. The meaning never depends on a
pointer into the buffer.

**Guard:** while a place into a collection is live, the collection's header has a "being
mutated in place" counter set. Anything that would resize, reorder or remove from that
collection during that time **panics** with a clear message ("enemies modified while an element
is being updated"):

```colibri
for var e in enemies {
    e.hp -= 1
    if e.hp < 10 { enemies.push(minion_at(e.pos)) }   // panics: push while `e` is live
}
```

The fix is to collect the new minions in a separate list and append after the loop; the panic
message suggests it. With the guard in place, the compiler may implement a place as a direct
pointer into the buffer as an optimization, which is safe because nothing can move the buffer
while the guard is set. The same counter implements "modified during iteration" (§6.3).

`for e in enemies` (no `var`) yields read-only copies (or, after optimization, read-only views).

### 11.7 Threads

Refcounts stay non-atomic because thread tasks are isolates: they don't share mutable
reference-typed objects. See §14.1.

---

## 12. Strings

[Locked] Interpolation: `"Hello, {name}!"`.

[Proposed]

- `str` is immutable, UTF-8 and reference-counted, and stored with a hidden trailing NUL so it
  converts to a C string for free (§15.3). Small strings may be stored inline later.
- Interpolation takes any expression: `"{a + b}"`, `"{p.pos}"`, including expressions with
  their own quotes: `"{m["key"]}"`. The value must implement `Show`. Format specs:
  `"{x:.2}"`, `"{n:>8}"`, `"{id:x}"` (Python-like subset).
- Literal braces are written `{{` and `}}`. Raw strings `r"..."` and raw multi-line strings
  `r"""..."""` don't interpolate, which suits shaders and JSON, full of braces.
- Indexing a `str` by integer is not allowed (it isn't O(1) in UTF-8); use `s.chars()`,
  `s.bytes()`, and slicing by byte range `s[0..4]` (panics on a non-boundary).
- **Slices copy** in v1: `s[0..4]` is a new string, so a small slice never keeps a huge string
  alive. A non-copying view type can come later if profiles ask for it.
- `StrBuilder` for efficient building; `+` concatenates strings.

---

## 13. Modules and packages

[Proposed, approved for the doc 2026-11-18, refined in v0.2] **One file is one module, and one
folder is one package**, like Go. The folder layout *is* the module tree: there are no header
files and no `mod` declarations.

```colibri
// game/player.colibri
pub struct Player {
    pub name: str
    hp: i32                  // package-visible, not exported
}

pub fn spawn(name: str) -> Player { ... }
```

```colibri
// main.colibri
import game.player                 // use as `player.spawn(...)`
import game.enemy as foe           // rename
import rapier.{World, Body}        // bring names in directly

let p = player.spawn("Ana")
```

- **Files are namespaces; packages are the unit of checking and visibility.** `game/player.colibri`
  is the module `game.player`, and its names are written `player.spawn`. But a package is
  type-checked as one unit, as in Go:
  - **Import cycles are allowed between files of the same package** and forbidden only between
    packages. Game code is mutually referential (`Player` mentions `World`, `World` holds `Enemy`,
    `Enemy` targets `Player`), and declarations are order-independent anyway. The only extra rule
    is initialisation order for module-level values (§4.2).
  - **Default visibility is the package.** Anything without `pub` is visible to every file in
    the same package. `pub` means exported from the package. So a three-file game needs no `pub`
    at all, and a library marks only its API. A struct with any non-exported field can only be
    constructed inside its package (use a `pub fn` constructor elsewhere).
- **Name resolution.** `import math` could match a local `math.colibri`, the standard `math`, or a
  dependency named `math`. Ambiguity is an error with the candidates listed; rename with
  `import std.math` or `as`.
- **Dependencies are listed in `colibri.toml`** and the `colibri` tool fetches them (git URLs or local
  paths in v1, a registry later). Resolved versions are pinned in **`colibri.lock`**. Rust crate
  bindings (§15.5) and capabilities (§15.3) are declared in the same file.
- **Scripts need no `colibri.toml`.** `colibri run script.colibri` treats the script's folder as an
  implicit package, so a script can `import helpers` from `helpers.colibri` next to it.
- **Top-level statements are allowed only in the entry file of a script** (Q15). Imported modules
  hold declarations only. In a package with `colibri.toml`, execution starts at `fn main()`.

### 13.1 Standard library [Proposed]

Built in two steps, following the domain priority in §1:

- **Core (phases 1–2):** `io`, `fs`, `path`, `os` (env, process exit), `math`, `rand`, `time`,
  `json` (with `encode`/`decode[T]`, §7.5), `str` helpers, `collections`, `task` (§14), `mem`,
  `reflect`, `test`, and the game kit (§15.6).
- **Scripting (phase 4):** `regex`, `http` (a blocking HTTP client), subprocesses with pipes,
  `datetime`, and `args` (CLI argument parsing). These are what people reach for Python to do;
  with a Rust runtime they are thin wrappers over existing crates. An HTTP server and sockets
  come later still.

---

## 14. Concurrency: tasks and coroutines

[Proposed, approved for the doc 2026-11-18, refined in v0.2] Colibri has **no `async`/`await` and no
function coloring**. Any function can pause, so libraries never split into sync and async
versions. There are two tools: **thread tasks** for parallel work such as loading and heavy
computation, and **coroutines** for game logic that runs over many frames on the current thread.

### 14.1 Thread tasks are isolates

```colibri
// background work
let t = task.run(fn() => load_level("lvl2.json"))   // runs on the runtime's worker pool
// ... keep rendering the loading screen with t.done() ...
let level = t.join()?                     // Task[Result[Level]] -> Result[Level]

// channels to talk between tasks
let ch = Channel[Msg]()
task.run(fn() => worker(ch))
ch.send(.Job(42))
let reply = ch.recv()
```

- `task.run(f)` runs the closure `f` on a worker thread and returns a `Task[T]`. The closure
  makes the thread boundary visible: everything it captures is sent across (rules below).
- `t.join()` waits for the result; a panic inside the task surfaces as an `err` from `join()`.
  `t.done()` and `t.try_join() -> T?` exist for polling from a frame loop.
- `Channel[T]` is a typed, optionally bounded queue (`Channel[Msg](capacity: 64)`) with `send`,
  `recv`, `try_recv` and `close`. A channel handle itself is thread-shared. `select` over several
  channels is a later addition.
- **Scheduling (v1):** a task is a job on a fixed worker pool (one thread per core), and channels
  are ordinary blocking queues. There is no M:N scheduler, preemption or network poller; games
  need a job pool and background loading, not tens of thousands of connections. The syntax
  doesn't change if M:N is ever added.
- **Blocking inside a coroutine:** `join()` and `recv()` called inside a coroutine (§14.2)
  **suspend the coroutine** and let `tick` resume it when the value is ready, so the frame never
  freezes. Outside a coroutine they block the thread.

**Isolation rules.** RC stays non-atomic because tasks are **isolates** (like Dart isolates and
JS workers): each task owns its own object graph.

- **A send is a structured deep copy**: captures of the `task.run` closure, values sent on a
  channel, and a task's result. To keep expensive copies visible, a send of a non-trivial
  reference value must be written explicitly as `ch.send(level.clone())` (Q10), unless the
  move optimization below applies.
  Plain values (numbers, vectors, structs of plain values) and `str` cross implicitly.
- **The move optimization** (sending without copying) applies only when the whole reachable graph
  is provably unshared: plain values, `str`, and collections of those whose count is 1. A root
  whose variable is dead is not enough, since other objects on the thread may still reference
  its contents.
- **Deep copy semantics:** cycles and shared substructure are preserved within the copied graph;
  `weak` references to objects inside the graph are remapped, and those to objects outside it
  become `none`; closures are copied with their captures; **opaque host handles refuse** (a
  compile error where the type is known, a panic otherwise), unless the host marks them
  thread-shareable.
- **Shared immutable data** uses Koka's approach: `Shared[T]` deep-freezes a graph and sets the
  thread-shared bit on every object in it, so those objects (and only those) use atomic counts.
  Frozen data crosses threads without copying, which is how a big level or asset table is shared
  with workers. Strings sent between threads can be frozen the same way instead of copied.
- **`Mutex[T]`** holds mutable shared state; it is thread-shared itself, and its contents are
  deep-copied in and out or accessed under a lock with a callback (`m.with(fn(var x) => ...)`).
- **Module-level state** belongs to the main isolate (§4.2).

### 14.2 Coroutines for game logic

```colibri
fn door_open(door: Door) {
    door.play("creak")
    wait(0.5)                                  // resume half a second later
    door.rotate(90, over: 1.0)                 // itself waits 1 second while animating
    wait_until(fn() => player.near(door))      // checked each frame until true
    door.close()
}

go door_open(front_door)                       // detached: runs until done or cancelled

class Door {
    co: CoGroup = CoGroup()                    // cancels its coroutines when the door is freed
    ...
}
door.co.go(door_open(door))                    // owned by the door
```

This is the big win for games: cutscenes, AI and animations read top to bottom instead of
becoming hand-written state machines.

- **Starting.** `go f(args)` creates a coroutine on the current thread, runs it until it first
  pauses, and returns a `Coroutine` handle (`cancel()`, `done()`, and `join()` from another
  coroutine). A bare `go` is **detached**: the scheduler keeps it alive until it finishes or is
  cancelled, and dropping the handle does not stop it.
- **Ownership.** A `CoGroup` owns the coroutines started through it and **cancels all of them
  when the group is dropped**. An entity holds a group as a field, so destroying the entity stops
  its coroutines, which removes the Unity/Godot failure mode of a coroutine outliving the object it
  animates. Note that a suspended coroutine holds strong references to its locals: a coroutine
  owned by its own subject keeps it alive until it finishes, so the group's owner should be the
  game's entity list or the subject should be `weak`.
- **Waiting.** `wait(seconds)`, `wait_frames(n)` and `wait_until(cond)` pause the coroutine.
  `wait_until` takes a closure that the scheduler re-checks every frame. For idle coroutines,
  **event waits** cost nothing per frame: `wait_for(signal)` on a `Signal` (`door.opened.emit()`)
  and `ch.recv()` on a channel.
- **Clock.** `wait(seconds)` counts the `dt` passed to `tick`, not wall time, so pausing the game
  (`tick(0)`) and slow motion (`tick(dt * 0.5)`) apply to coroutines. `wait_real(seconds)` uses
  wall time for menus.
- **The game loop resumes coroutines each frame.** The runtime has a per-thread coroutine
  scheduler, and the host drives it: a Colibri game calls `task.tick(dt)` once per frame (the built-in
  game kit does it for you, §15.6), and an engine embedding Colibri calls `vm.tick(dt)` (§15.4).
  Coroutines never run in parallel with each other, so they share `class` objects freely without
  locks.
- Any function can be called from a coroutine and may itself call `wait`; there is no special
  coroutine function type. Calling `wait` outside a coroutine panics with a clear message.
- **No suspending across a host frame.** If a host (Rust or C) function calls back into Colibri and
  that Colibri code tries to `wait`, it panics: the host's stack frame can't be suspended.
- **Cancellation** stops the coroutine at its current pause point and unwinds it (§10.4),
  running its `defer`s and drops, so cleanup still runs.

### 14.3 Implementation notes

- Uncolored functions require **stackful** coroutines.
- **Interpreter and VM:** the Colibri stack is a heap object, so coroutine stacks are naturally
  growable and cheap.
- **Native code (only if the AOT stretch goal is built):** coroutines get **fixed-size stacks with guard pages and lazily committed
  memory** (for example 256 KB of address space each, committed as touched). Compiled Colibri and
  C both run directly on the coroutine's stack, so raylib calls don't need a stack switch (the
  reason Go's cgo is slow). Games have hundreds or thousands of coroutines, not millions, so the
  address space is fine. Context switching uses an existing crate such as `corosensei` on the
  platforms Cranelift targets. A guard-page hit is a clean panic ("coroutine stack overflow").
- **Phasing:** coroutines arrive with the game slice on the interpreter (phase 2 in §17.5);
  thread tasks need the worker pool, thread-shared objects and deep copy, and come with the VM.

---

## 15. Game-dev features

### 15.1 Vector and matrix types [Locked: vec2/3/4, Proposed details]

- `vec2`, `vec3`, `vec4` (`f32`), `ivec2/3/4` (`i32`), `mat3`, `mat4`, `quat` are built-in
  value types.
- **`vec3` is three `f32`s: 12 bytes, alignment 4, everywhere.** That matches glam's default
  `Vec3`, Unity's `float3`, raylib's `Vector3`, GPU vertex formats and most file formats, so a
  `List[vec3]` can go to C or the GPU without repacking. Code generation uses SIMD registers
  opportunistically; there is no padded layout.
- Arithmetic operators work component-wise (`a + b`, `a * 2`, `-a`); vector-by-scalar accepts a
  `float` scalar (§5.1). `dot`, `cross`, `len`, `normalize`, `lerp` and swizzles (`v.xy`,
  `v.zyx`) are built in. The implementation uses glam internally.
- These are the only arithmetic operators on non-primitive types in phase 1. `Add`, `Sub`,
  `Mul` and `Neg` interfaces for user structs are planned after it (Q12), for `Color`,
  fixed-point numbers and `f64` vectors for large worlds; the built-in types will use the same
  machinery.

### 15.2 C-layout structs [Locked, Proposed syntax]

```colibri
@repr(C)
struct Vertex {
    pos: vec3
    uv: vec2
    color: [u8; 4]
}
```

`@repr(C)` structs follow the platform C ABI, may only contain C-compatible fields (numbers,
vectors, fixed arrays, other `@repr(C)` structs, raw pointers), and can be passed to C directly
or uploaded to the GPU as a byte buffer (`vertices.as_bytes()`).

### 15.3 C FFI [Locked, Proposed syntax]

```colibri
@link("raylib")
extern "C" {
    fn InitWindow(width: i32, height: i32, title: cstr)
    fn WindowShouldClose() -> bool
    fn DrawCircleV(center: vec2, radius: f32, color: Color)    // structs by value
}
```

- **FFI is a capability.** A package must declare `ffi = true` in `colibri.toml` to contain `extern`
  blocks, so a git dependency can't quietly link C. An embedding host can create a `Vm` with FFI,
  `fs`, `os` and `http` switched off (§15.4), which is what makes Colibri usable for mods. The risk in
  FFI is the declaration (a wrong signature corrupts memory as surely as a bad pointer), so the
  capability guards declarations, and calling a declared function needs no `unsafe`.
- **`unsafe { }` is required for raw pointer operations** (`ptr[T]` dereference, arithmetic,
  casts) (Q13). Safe code can pass and receive `ptr[T]` values but not use them.
- `cstr` is a borrowed NUL-terminated string; any `str` converts for free because strings carry a
  hidden NUL (§12).
- **Structs by value** (`Color`, `Vector2`, `Texture2D`, everywhere in raylib) are supported from
  day one, in the libffi path too.
- `colibri bindgen header.h` generates `extern` declarations from C headers.
- The interpreter and VM call C via `libffi`; native builds call directly.

### 15.4 Embedding in Rust [Locked, first-class from phase 1; Proposed API shape]

```rust
let mut vm = colibri::Vm::builder()
    .allow(colibri::Cap::Fs)                // capabilities: nothing beyond pure Colibri unless granted
    .instruction_budget(1_000_000)      // optional sandbox limits
    .memory_limit(64 << 20)
    .build();
vm.register_fn("spawn_enemy", |x: f32, y: f32| { /* engine code */ });
vm.write_interface("scripts/engine.cbi")?;   // declarations for `colibri check` and the editor
vm.load_file("scripts/ai.colibri")?;
let f = vm.get_fn::<(f32,), ()>("update")?;
f.call(&mut vm, (dt,))?;
vm.tick(dt)?;                    // resume Colibri coroutines for this frame, §14.2
vm.reload_changed()?;            // hot reload, §16.3
```

- **Offline checking.** `colibri check` and the editor need to know about host functions when the
  script isn't running inside the engine. The host writes an interface file (`.cbi`, the same
  format as Rust bindings in §15.5), and scripts are checked against it.
- **A `Vm` is single-threaded.** Several VMs can coexist; `colibri-runtime` has no global state.
- Rust types are exposed via `#[colibri::export]` (§15.5): `Copy` types as Colibri structs, others as
  opaque handles.
- **Host handles are generational.** A script holding a handle to a destroyed engine object gets
  a clean error (or `none` from a checked accessor) instead of a dangling pointer.
- **Capabilities and sandboxing:** no `fs`, `os`, `http` or FFI unless the host grants them;
  optional instruction budget and memory cap. Together these are the sandbox for embedded
  scripts and mods.
- Panics in Colibri code become `Err` on the Rust side; they never unwind into the host.

### 15.5 Using Rust crates from Colibri [Planned, Proposed design]

Rust's game ecosystem (rapier for physics, wgpu for graphics, kira for audio) would give Colibri a
lot. Colibri's runtime is already Rust, so the same binding layer that powers embedding (§15.4) can
expose Rust crates to Colibri code, the way PyO3 does for Python.

**Why not arbitrary `import some_crate`:** Rust has no stable ABI, and traits, lifetimes,
generic functions and borrowed references don't map one-to-one onto Colibri's type system. A crate
needs a thin, explicit Colibri-facing surface.

**Rust side.** A `colibri-bind` crate provides attribute macros that generate the glue:

```rust
use colibri_bind::prelude::*;

#[colibri::export]
pub struct RigidBody { handle: rapier3d::dynamics::RigidBodyHandle }   // opaque handle in Colibri

#[colibri::export]
impl World {
    pub fn new(gravity: Vec3) -> World { ... }
    pub fn add_ball(&mut self, pos: Vec3, radius: f32) -> RigidBody { ... }
    pub fn step(&mut self, dt: f32) { ... }
    pub fn position(&self, body: &RigidBody) -> Vec3 { ... }
}

#[colibri::module]
fn physics(m: &mut colibri::Module) {
    m.add_class::<World>();
    m.add_class::<RigidBody>();
}
```

- Type mapping: Rust numbers, `bool`, `String`/`&str`, `Vec<T>`, `Option<T>` and `Result<T, E>`
  map to their Colibri counterparts; `glam::Vec2/Vec3/Vec4` map to `vec2/3/4` with no conversion
  (same 8/12/16-byte layouts, §15.1); `#[colibri::export]` structs that are `Copy` become Colibri
  `struct`s; everything else becomes an RC'd opaque handle.
- Rust `Result::Err` becomes a Colibri `err(...)`; a Rust panic is caught at the boundary and
  becomes a Colibri panic.
- Generic Rust APIs are exported per concrete instantiation; trait-heavy APIs get a small
  hand-written wrapper.

**Colibri side.** The crate is declared in `colibri.toml`, and Colibri code imports it like any module:

```toml
[rust-dependencies]
rapier = { binding = "colibri-rapier", version = "0.1" }   # curated binding crate
mygame-native = { path = "native/" }                   # your own Rust code
```

```colibri
import rapier

let world = rapier.World.new(vec3(0, -9.81, 0))
let ball = world.add_ball(vec3(0, 10, 0), 0.5)
world.step(1.0 / 60.0)
print(world.position(ball))
```

**Build and run: static linking everywhere, no dylib.**

- When a project has `[rust-dependencies]`, `colibri run` builds (once, cached) a
  **project-specific runner executable**: the VM plus those bindings, statically linked. Hot
  reload still works, because only Colibri code reloads. `colibri build` ships the same runner with
  the program's bytecode.
- There is no dynamic library and no second ABI to design and keep stable. Using a binding needs a
  Rust toolchain (or a prebuilt runner per platform); the built-in game kit (§15.6) means most
  users never need either.
- The type checker reads a generated `.cbi` interface file for each binding, so code using a
  crate is fully type-checked and gets editor completion.

**Curated bindings.** No `glam` binding is needed, since the vector types are built in. The first
curated binding is `colibri-rapier` when the demo game needs physics, then `colibri-kira` and a renderer
on `wgpu` for projects that outgrow the game kit.

**One mechanism, two directions.** `#[colibri::export]` is the same API an engine uses to expose its
own types when embedding Colibri (§15.4): build it once, get both.

### 15.6 Built-in game kit [Proposed]

The stock `colibri` binary includes a small **game kit**, the way LÖVE does for Lua, so the first game
needs no Rust toolchain, no C toolchain and no bindings:

```colibri
import game

fn main() {
    game.window(800, 600, "Arena")
    var pos = vec2(400, 300)
    game.run(fn(dt) {                      // calls task.tick(dt) for coroutines, then this
        if game.key_down(.Right) { pos.x += 200 * dt }
        game.clear(game.BLACK)
        game.circle(pos, 16, game.RED)
    })
}
```

- Scope for v1: window and input (keyboard, mouse, gamepad), 2D drawing (shapes, sprites, text),
  audio playback, and asset loading. 3D is out of scope for the kit.
- The backend (statically linked raylib, or Rust crates such as winit, wgpu and kira) is Q20.
- The game kit is ordinary Colibri plus host functions, so it doubles as the reference example of the
  embedding API.

---

## 16. Tooling and execution modes

### 16.1 One binary [Locked, Proposed details]

```
colibri run file.colibri      # type-check, then run (on the tree-walker until the VM lands in phase 3)
colibri build               # one executable: the VM plus the program's bytecode and assets
colibri build --web         # the VM compiled to WebAssembly, plus bytecode and assets
colibri test                # runs `test "name" { ... }` blocks
colibri fmt                 # canonical formatter, no options
colibri check               # type-check only (fast, for editors/CI)
colibri lsp                 # language server
colibri repl                # interactive prompt
```

`colibri run` always type-checks first. There is no "dynamic mode"; every engine runs the same
checked program, with the same results, panics and drop order (§1). If the AOT stretch goal is
built, it becomes `colibri build --native`.

### 16.2 Execution pipeline [Proposed]

```
source → lexer → parser → AST → name resolution → type check → HIR (typed)
                                                                 │
                    ┌────────────────────────────────────────────┤
          tree-walking interpreter                      Colibri IR (SSA, explicit RC ops)
          (phases 1–2; then the reference)                       │
                                               ┌─────────────────┴────────────────┐
                                     bytecode VM: the product            Cranelift → native
                                     (run, build, embedding, hot         (stretch goal, only
                                      reload, web; phase 3)               if a profile asks)
```

- The **HIR** is the shared contract: fully typed, names resolved, sugar removed.
- **Colibri IR** makes RC operations explicit (`inc`, `dec`), so future elision passes (§11.3)
  have a place to live and would serve every backend.
- The **tree-walker** is the first engine and then remains the reference implementation: golden
  tests run on every engine and must agree.
- **The bytecode VM is the product.** It is what `colibri run`, `colibri build`, embedding, hot reload
  and the web use, so one engine is tuned, debugged and shipped.
- **Native AOT is a stretch goal.** It is started only when a profile of a real game shows the
  VM missing a §17.4 target, and only after the cheaper fix has been tried: moving the hot code
  into Rust through `#[colibri::export]` (§15.4–15.5). If it is built, §10.4 and §14.3 already say
  how panics and coroutines work in native code.
- **No JIT.** It would be one more engine to keep in agreement, and iOS and consoles forbid it.

### 16.3 Hot reload [Locked goal, Proposed semantics]

Goal: edit a `.colibri` file while the game runs and see the change without restarting.

- **Hot reload is a VM (and interpreter) feature**, so shipped games and embedded scripts can
  use it in development. Native AOT code, if it is ever built, doesn't have it.
- **Function-level patching** when only function bodies changed: the changed module plus every
  module that depends on it is re-checked against the running program, and new bodies replace
  old ones at the next call.
- **Automatic restart as the fallback** when anything else changed (a struct gains a field, an
  enum gains a variant, a signature changes). Since the VM starts fast, the program restarts
  in place, with an optional pair of hooks to carry state across:
  `fn save_state() -> Snapshot` and `fn restore_state(s: Snapshot)` (serialized through §7.5).
- **Rules:** adding an enum variant re-checks `match`es elsewhere; module-level initialisers do
  not re-run on a patch; a coroutine suspended inside an old function keeps running the old body
  until it returns, so long-lived `while true` coroutines pick up changes only on restart. The
  editor shows which coroutines are still on old code.
- After a panic in a development run, the program can be patched and resumed from the game loop.

### 16.4 Diagnostics [Proposed]

Errors show the source span, a short message, and a hint where possible (Rust/Elm style), using a
crate like `ariadne` or `miette`. The type checker keeps going after the first error to report
several at once.

### 16.5 Editor support, debugging and profiling [Proposed]

Editor support is most of what "usable" means today, so it moves early:

- **Day one:** a syntax-highlighting grammar (TextMate/tree-sitter) for VS Code and others.
- **Right after the type checker:** `colibri check` and a minimal LSP (diagnostics, hover with types,
  go-to-definition), then completion and rename.
- **With the VM:** a debugger over DAP (breakpoints, stepping, locals using the type descriptors
  of §7.5, coroutine list), and a sampling profiler (`colibri run --profile`) that writes a
  flamegraph and reports RC traffic hot spots.

### 16.6 Platform targets [Proposed]

| Platform | Development | Shipping |
|----------|-------------|----------|
| Windows, macOS, Linux | `colibri run` (interpreter, then VM) | `colibri build`: the VM plus bytecode in one executable |
| Web | `colibri run` on desktop | `colibri build --web`: the VM compiled to WebAssembly, plus bytecode |
| Embedded in a Rust engine | the host's dev build, with hot reload | the host's executable, with `colibri-runtime` linked in |
| iOS, Android | — | later; the VM (no JIT, so App Store rules are met) |
| Consoles | — | out of scope for v1 |

One engine serves every platform, so a game behaves the same on desktop, in the browser and
inside a host engine. Cranelift has no WebAssembly output, which is one more reason the VM is the
product.

### 16.7 Language editions [Proposed]

`colibri.toml` has an `edition = "2027"` field (scripts default to the latest). Syntax changes after
1.0 happen at edition boundaries, and `colibri fmt --migrate` rewrites code to the new edition, so
old code keeps compiling.

---

## 17. Implementation plan (Rust)

### 17.1 Workspace layout [Proposed]

```
colibri/
  Cargo.toml                 # workspace
  crates/
    colibri-syntax/              # lexer, parser, AST, spans
    colibri-check/               # name resolution, type inference/checking, HIR, type descriptors
    colibri-interp/              # tree-walking interpreter (the reference implementation)
    colibri-ir/                  # Colibri IR, bytecode lowering (later: RC elision passes)
    colibri-vm/                  # bytecode VM (native and wasm32)
    colibri-codegen/             # Cranelift AOT backend: stretch goal, not created until needed
    colibri-runtime/             # RC heap, strings, collections, coroutines, FFI, embedding API
    colibri-bind/                # #[colibri::export] macros for embedding and Rust crate bindings
    colibri-std/                 # standard library natives
    colibri-game/                # the built-in game kit (§15.6)
    colibri-lsp/                 # language server
    colibri-cli/                 # the `colibri` binary
  std/                       # .colibri sources of the standard library
  editors/                   # syntax grammars, VS Code extension
  tests/                     # golden tests: .colibri files with expected output/errors/drop order
  docs/DESIGN.md
```

### 17.2 Names [Proposed]

The language is **Colibri** (Q16, decided 2026-11-19), written without the accent in anything
typed: the command, file extensions, crate names and config files. A check of crates.io and
GitHub on 2026-11-19 found these constraints:

| Thing | Name | Notes |
|-------|------|-------|
| CLI binary | `colibri` | |
| CLI crate on crates.io | `colibri-lang` | The bare crate name `colibri` is taken by a Jitsi Meet library, so the CLI is published as `colibri-lang` with a binary named `colibri`. |
| Workspace crates | `colibri-syntax`, `colibri-check`, `colibri-interp`, `colibri-ir`, `colibri-vm`, `colibri-codegen`, `colibri-runtime`, `colibri-bind`, `colibri-std`, `colibri-game`, `colibri-lsp`, `colibri-cli` | The ones checked (`colibri-lang`, `-cli`, `-runtime`, `-bind`, `-vm`, `-std`, `-syntax`, `-check`) were free on 2026-11-19; reserve all of them before publishing anything. |
| Embedding crate | `colibri-runtime` | Hosts write `use colibri_runtime as colibri;`, so `colibri::Vm` and `#[colibri::export]` read as in §15.4–15.5. |
| Source files | `.colibri` | `.col` is taken by ColecoVision ROMs and other formats; `.coli` reads as E. coli. |
| Interface files | `.cbi` | Generated declarations for bindings and hosts (§15.4–15.5). |
| Project files | `colibri.toml`, `colibri.lock` | |
| Repository | `jalatodeguerra/leo` | Unchanged until santiago renames it. |

No existing programming language called Colibri turned up in two quick searches (a light check,
not a guarantee). Unrelated projects share the word: Jitsi's Colibri protocol, the Colibri Core
text-analysis tool and an AI inference engine. People will also mix up colibri, colibrí and
kolibri, so docs should say "Colibri language" and the website should own the misspellings.

### 17.3 Key technical choices [Proposed]

- **Parser:** hand-written recursive descent with Pratt parsing for expressions, implementing
  the rules of §3.2 and §3.5. Better error recovery and messages than a generator.
- **Type checker:** bidirectional checking with local unification for generics and closures
  (no global Hindley-Milner). Matches §5.4 and keeps errors local. Packages are checked as a
  unit (§13).
- **Testing:** golden-file tests from day one (`tests/*.colibri` with `// expect:` comments for
  output, `// error:` for diagnostics and `// drops:` for drop order, §11.2). Every feature
  lands with tests, and every engine runs the same suite.
- **Runtime values (interpreter):** a Rust `enum Value` with `Rc<...>` heap objects, built to
  follow §11.2 and §11.6 exactly (explicit scope-end drops, copy-in/copy-out places with the
  mutation guard). It must not rely on `RefCell` panics or accidental copies, which would hide
  the semantics native code has to reproduce.
- **Unwinding:** the VM unwinds its heap stack; native code, if ever built, uses hidden error
  returns (§10.4).
- **If the AOT stretch goal is started:** Cranelift has historically left inlining to the
  frontend (to be re-checked against its current version at that point), so Colibri IR has its own inliner
  before small generic and vector functions get fast.

### 17.4 Performance targets [Proposed]

So each phase has a finish line:

| Measure | Target |
|---------|--------|
| `colibri run hello.colibri` startup, including type check | under 50 ms |
| Type check | at least 100k lines per second |
| Hot-reload patch, edit to new code running | under 200 ms for a 10k-line game |
| Bytecode VM | within 1.5× of Lua 5.4 on standard benchmarks (binary-trees, n-body, fannkuch, spectral-norm) |
| Shipped game (`colibri build`) | a 2D game loop at 60 fps with no per-frame allocation required; executable under 15 MB |
| Embedding | calling a Colibri function from Rust in under 100 ns; `vm.tick` with 1,000 idle coroutines under 0.1 ms |
| Native AOT (stretch) | started only if a profiled game misses 60 fps on the VM; then within 3× of C on n-body and vector math |
| Interpreter | no target beyond "the demo game runs at 60 fps" |

### 17.5 Roadmap [Locked, revised 2026-11-19]

The original roadmap had three phases: (1) lexer, parser, type checker and tree-walking
interpreter; (2) bytecode VM, Cranelift and RC optimizations; (3) game bindings and the first
small game. santiago approved reordering it on 2026-11-19: the game comes before the fast
engine, because the game is what will force changes to places, struct/class, coroutines and
modules, and those changes are cheap with one engine. The VM is the product, embedding comes
first, and native AOT, RC elision and the ownership checker become stretch goals.

**Phase 1: language core on the tree-walker, embeddable from day one.** Built as **vertical
slices**: hello world end to end first (lexer → parser → checker → interpreter), then widen
feature by feature. `colibri-runtime` is a library from the first slice, and the `colibri` CLI is
just one host of it.

1. Syntax highlighting grammar; golden-test harness.
2. Slice 1: `print`, functions, `let`/`var`, ints, strings with interpolation, run from both the
   CLI and a Rust host (`Vm::load_file`, `get_fn`, `call`).
3. Structs, enums, `match` with exhaustiveness, `if`/`for`/`while`, ranges, tuples.
4. Classes, `List`/`Map`/`Set`, places with the mutation guard, drop order per §11.2.
5. `Result`/`Option`, `?`, `or` with handler blocks, `Error` with context.
6. Generics, interfaces, closures, iteration.
7. Embedding API: `register_fn`, `#[colibri::export]` for host types, generational handles,
   `.cbi` interface files, capabilities.
8. Packages and `import`; `colibri check` and a minimal LSP; `colibri test`, `colibri fmt`; core std
   (§13.1).
9. **Milestone:** a small Rust program with its own game loop that runs its gameplay logic from
   Colibri scripts.

**Phase 2: game slice on the interpreter**

1. Vector types as plain values; `@repr(C)` structs and the C FFI with structs by value.
2. Coroutines (`go`, `wait`, `wait_until`, `wait_for`, `CoGroup`), driven by `task.tick` and
   `vm.tick`.
3. The built-in game kit (§15.6).
4. Hot reload (patching plus automatic restart), for standalone games and embedded scripts.
5. The leak detector.
6. **Milestone:** a small, complete game (e.g. a top-down arena shooter) with hot reload, both
   standalone on the game kit and with its scripts embedded in a Rust host.

**Phase 3: the bytecode VM, the product engine**

1. Colibri IR with explicit RC ops; plain RC.
2. The bytecode VM replacing the tree-walker for `colibri run` and embedding; sandbox limits.
3. `colibri build` (one executable: VM plus bytecode and assets) and `colibri build --web`.
4. Thread tasks, channels, `Shared[T]` and deep copy.
5. Debugger over DAP; profiler.
6. **Milestone:** the demo game shipped with `colibri build` on all three desktop platforms and in
   the browser, with the VM within the §17.4 targets.

**Phase 4: general scripting**

1. The scripting std (§13.1): `regex`, `http`, subprocesses, `datetime`, `args`.
2. `colibri repl`, script ergonomics, and startup time tuned for one-off scripts.
3. Rust crate bindings through project-specific runners (§15.5); `colibri-rapier`.
4. **Milestone:** real automation scripts written in Colibri (a file-organising tool, a JSON/HTTP
   report generator).

**Stretch goals**, each started only when a profile of a real program asks for it:

- RC elision passes (§11.3).
- `own`/`borrow` semantics and checker (§11.4).
- Native AOT via Cranelift (§16.2), with the Colibri IR inliner and fixed-size native coroutine
  stacks (§14.3).

---

## 18. Open questions

None of these are decided. The **Recommendation** column reflects the advisor review on PR #1,
and the body of the doc is written as if each recommendation were adopted (marked [Proposed]), so
it reads consistently. Changing one means editing the sections it names.

| # | Question | Recommendation | Sections |
|---|----------|----------------|----------|
| Q1 | Boolean operators: `&&` `\|\|` `!` or `and` `or` `not` | `&&` `\|\|` `!`; `or` is the Result fallback; `and`/`not` reserved for a targeted error | §3.3 |
| Q2 | `T?` sugar for `Option[T]` | Yes; and a `weak T` field reads directly as `T?` | §5.2, §11.5 |
| Q3 | Reference types and collection semantics | `struct` values + `class` references; collections by reference; shallow struct copies with deep `.clone()`; structural `==` on collections | §5.3, §7 |
| Q4 | Implicit return | `{ }` bodies use `return`; `=> expr` bodies for functions and closures; `if`/`match` arm blocks yield their trailing expression | §6.1 |
| Q4b | Short lambda syntax | `fn(x) => x * 2` | §6.2 |
| Q5 | Deep immutability | Shallow `let` in v1 | §4.1 |
| Q6 | Integer overflow | Trap in every mode; `wrapping_*`/`checked_*`/`saturating_*` methods | §5.1 |
| Q7 | Interface conformance | Nominal `impl I for T` | §9.2 |
| Q8 | `own`/`borrow` rules | **Decided 2026-11-19:** reserved words only; semantics and checker not designed now (constraints in §11.4) | §11.4 |
| Q9 | Cycle collection | `weak` + leak detector built as trial deletion; maybe an explicit `mem.collect_cycles()` later | §11.5 |
| Q10 | What crosses a thread boundary | Isolates; sends are explicit deep copies (`.clone()`), implicit only for plain values and `str`; `Shared[T]` freezes and sets a shared bit | §14.1 |
| Q11 | Enum variant lookup | Bare in patterns, `.Variant` where the type is known, `Type.Variant` otherwise; capitalisation rule in patterns | §8.2 |
| Q12 | Operator overloading | Built-in only in phase 1; `Add`/`Sub`/`Mul`/`Neg` interfaces for structs after it | §15.1 |
| Q13 | `unsafe` | `unsafe { }` for raw pointer operations; FFI as a per-package and per-VM capability | §15.3 |
| Q14 | Hot reload semantics | Function-level patching, automatic restart (with optional save/restore hooks) on layout changes | §16.3 |
| Q15 | Script top-level code | Only in the entry file of a script; imported modules hold declarations only | §13 |
| Q16 | Name | **Decided 2026-11-19: Colibri**, written `colibri` in anything typed. Renamed from the working name Leo (which collides with Aleo's Leo), briefly Noble. Naming details in §17.2 are [Proposed] | everywhere |
| Q17 | Rust crate bindings | Static linking everywhere through project-specific runners; no dylib; no `glam` binding; `rapier` when the demo needs it | §15.5 |
| Q18 | Coroutine and task syntax | `go f()` keyword for coroutines; `task.run(fn() => ...)` for thread tasks; with `=>` lambdas `wait_until` needs no sugar. Spelling of `go` (vs `co`) still open | §14 |
| Q19 | Weak capture syntax | `fn() use(weak self) { ... }` capture list; inside, the name has type `T?` | §6.2 |
| Q20 | Game kit backend | Statically linked raylib (fastest to ship, C dependency) vs Rust crates (winit + wgpu + kira, pure Rust, more work) | §15.6 |

---

## 19. Prior art

| Language | What Colibri takes from it |
|----------|------------------------|
| **Go** | Simple syntax, newline-terminated statements, untyped constants, `defer`, packages as folders, one formatter |
| **Python** | Readability bar, scripting ergonomics, f-string style interpolation |
| **Rust** | `Result`/`?`, enums + exhaustive `match`, `impl` blocks, Cargo-style tooling, editions |
| **Swift** | Value structs vs reference classes, ARC with optimization, `weak`, `inout` copy-in/copy-out, trapping overflow |
| **C# / Unity** | The struct-vs-class split game developers already know; coroutine use cases (and pitfalls) |
| **Lobster** | Compile-time RC elision for a game-focused language; closest relative |
| **Koka / Perceus** | Precise RC with reuse analysis (FBIP); a shared bit for thread-shared objects |
| **Dart / JS workers** | Isolates with message passing |
| **Zig** | `catch \|e\| { ... }` error handler blocks |
| **Vale** | Ideas on generational references and region borrowing (for future work) |
| **Odin / Jai** | Game-dev ergonomics: vector types, C layout, easy FFI |
| **Lua / LÖVE** | The embedding experience to match or beat; a game kit in the stock binary |
| **GDScript / Wren** | Scripting languages built for games and hot reload |
