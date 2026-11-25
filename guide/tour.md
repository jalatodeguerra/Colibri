# A tour of the language

This page is for programmers who know a few languages already. It is one short section for each
feature, with a sample that runs. For a gentler path that builds a program, read the
[tutorial](tutorial.md); for the library, see the [standard library reference](std/README.md).

Colibri is statically typed with local inference, uses braces and has no semicolons. It is
reference counted (no garbage collector), has no exceptions and no null, and has coroutines
instead of `async`. It is early and pre-release, and what is planned but not built is marked
**planned**.

- [Programs and scripts](#programs-and-scripts)
- [Bindings and constants](#bindings-and-constants)
- [Types and numbers](#types-and-numbers)
- [Strings](#strings)
- [Tuples, ranges and type aliases](#tuples-ranges-and-type-aliases)
- [Functions](#functions)
- [Closures](#closures)
- [Control flow](#control-flow)
- [Pattern matching](#pattern-matching)
- [Structs](#structs)
- [Classes and weak references](#classes-and-weak-references)
- [Enums](#enums)
- [Methods and impl blocks](#methods-and-impl-blocks)
- [Derived behaviour](#derived-behaviour)
- [Interfaces](#interfaces)
- [Generics](#generics)
- [Operators on your own types](#operators-on-your-own-types)
- [Optionals](#optionals)
- [Errors: `Result`, `?` and `or`](#errors-result--and-or)
- [Panics and `defer`](#panics-and-defer)
- [Lists, maps and sets](#lists-maps-and-sets)
- [Iterators](#iterators)
- [Memory and `Drop`](#memory-and-drop)
- [Modules and packages](#modules-and-packages)
- [Visibility](#visibility)
- [Conditional code and deprecation](#conditional-code-and-deprecation)
- [Coroutines](#coroutines)
- [Bytes and JSON](#bytes-and-json)
- [Tests](#tests)
- [Tools](#tools)
- [Embedding and FFI](#embedding-and-ffi)

## Programs and scripts

A file with `fn main()` is a program. A file with no `main` is a script, whose top-level
statements run from the top down. `main` may return `Result[()]`; an `err` prints the error and
exits with code 1. After `main` returns, the run lasts until every coroutine has finished.

```colibri
fn main() -> Result[()] {
    print("a program")
    return ok(())
}
```

```output
a program
```

```colibri
let greeting = "a script"
print(greeting)
```

```output
a script
```

## Bindings and constants

`let` is immutable and `let mut` is mutable. `const` is a compile-time constant and
only exists at the top level of a file; inside a function use `let`. A name can be shadowed in
an inner block. Types are written only where they cannot be inferred, mainly in
function signatures.

```colibri
const LIMIT = 3

fn main() {
    let a = 10
    let mut b = 20
    b += a
    let c: float = 1.5
    {
        let a = "shadowed"
        print(a)
    }
    print("{a} {b} {c} {LIMIT}")
}
```

```output
shadowed
10 30 1.5 3
```

`let` stops rebinding, not mutation of what a class or a list holds (see
[classes](#classes-and-weak-references)).

## Types and numbers

| Type | Meaning |
|------|---------|
| `int`, `float` | 64-bit signed integer, 64-bit float |
| `i8 i16 i32 i64`, `u8 u16 u32 u64`, `f32` | sized numbers (`i64` is `int`) |
| `bool`, `char`, `str` | truth value, Unicode scalar value, UTF-8 text |
| `Bytes` | immutable byte string |
| `()` | unit, the type of a function that returns nothing |
| `T?` | optional |
| `(A, B)` | tuple |
| `List[T]`, `Map[K, V]`, `Set[T]` | collections |
| `fn(A) -> R` | function |

Numbers convert only when you say so, with the type's name as the function. Integer division
truncates toward zero, `%` takes the sign of the left side, and overflow panics in every build.
Bitwise operators bind tighter than comparisons, so `x & 1 == 0` means `(x & 1) == 0`.

```colibri
fn main() {
    print(7 / 2)
    print(-7 / 2)
    print(-7 % 3)
    print(float(7) / 2.0)
    print(int(3.99))
    print(u8(200) + u8(50))
    print(u8.MAX)
    print(0xFF & 0b1010)
    print(1 << 10)
    print(5.max(9))
    print(7.checked_add(1))
    print(int.MAX.checked_add(1))
}
```

```output
3
-3
-1
3.5
3
250
255
10
1024
9
some(8)
none
```

## Strings

A `str` is immutable UTF-8. `{expr}` interpolates any value that can be shown, with an optional
format after `:`. `{{` and `}}` are literal braces. Raw strings (`r"..."`, `r#"..."#`) do not
interpolate or escape, and `"""` strings span lines. Indexing a string by integer is not
allowed; slice by byte range or iterate `chars()`.

```colibri
fn main() {
    let name = "héllo"
    print("{name.len()} bytes")
    print(name[0..1])
    print("{3.14159:.2} [{42:>6}] [{"ab":<5}] {255:x} {{literal}}")
    print(r#"{"json": true}"#)
    print("a,b,,c".split(","))
    print("  pad ".trim().to_upper())
    print("x=1".split_once("="))
    let b = StrBuilder()
    for i in 0..3 {
        b.push("{i};")
    }
    print(b.to_str())
}
```

```output
6 bytes
h
3.14 [    42] [ab   ] ff {literal}
{"json": true}
["a", "b", "", "c"]
PAD
some(("x", "1"))
0;1;2;
```

Use `StrBuilder` to build long text: `s += x` in a loop copies the string every time.

## Tuples, ranges and type aliases

```colibri
type Grid = Map[str, List[int]]

fn bounds(xs: List[int]) -> (int, int) => (xs.min() or 0, xs.max() or 0)

fn main() {
    let (lo, hi) = bounds([4, 9, 2])
    let pair = (1, "one")
    print("{lo} {hi} {pair.0} {pair.1}")
    let r = 1..=4
    print(r.to_list())
    print(r.contains(4))
    print((0..10).len())
    let g: Grid = {"row": [1, 2]}
    print(g)
}
```

```output
2 9 1 one
[1, 2, 3, 4]
true
10
{"row": [1, 2]}
```

An alias is the same type under another name.

## Functions

Parameter and result types are written. `{ }` bodies use `return`, and `=>` makes the body one
expression. Parameters can have defaults and arguments can be named. `-> !` marks a function that
never returns.

```colibri
fn area(w: int, h: int = 1) -> int => w * h

fn log(msg: str, level: str = "info") {
    print("[{level}] {msg}")
}

fn die(msg: str) -> ! {
    panic(msg)
}

fn main() {
    print(area(3))
    print(area(3, h: 4))
    log("ready")
    log("careful", level: "warn")
    if area(1) > 5 {
        die("never")
    }
}
```

```output
3
12
[info] ready
[warn] careful
```

## Closures

`fn(x) => x * 2` and `fn(x) { ... }` are values with function types. A closure captures variables
by reference, so it can change them. Each iteration of a `for` has its own binding, so closures
made in a loop capture that iteration's value.

```colibri
fn make_counter() -> fn() -> int {
    let mut n = 0
    return fn() -> int {
        n += 1
        return n
    }
}

fn main() {
    let next = make_counter()
    print(next())
    print(next())
    let mut fs: List[fn() -> int] = []
    for i in 0..3 {
        fs.push(fn() => i * 10)
    }
    print(fs.map(fn(f) => f()))
}
```

```output
1
2
[0, 10, 20]
```

## Control flow

`if` and `match` are expressions. Conditions have no parentheses. `while`, `for`, `break` and
`continue` work as usual, and a loop label (`'outer`) lets `break` and `continue` name an outer
loop.

```colibri
fn main() {
    let n = 7
    let kind = if n % 2 == 0 { "even" } else { "odd" }
    print(kind)
    'outer: for a in 0..4 {
        for b in 0..4 {
            if b == 2 {
                continue 'outer
            }
            if a == 2 {
                break 'outer
            }
            print("{a}{b}")
        }
    }
    let mut xs = [1, 2, 3]
    while let some(x) = xs.pop() {
        print(x)
    }
}
```

```output
odd
00
01
10
11
3
2
1
```

## Pattern matching

`match` is exhaustive. Patterns are literals, ranges, tuples, enum cases, struct fields, `_`, `|`
alternatives, `as` bindings, and each arm can have an `if` guard. A capitalised name in a pattern
must be an existing case or constant, so a typo is an error and not a silent catch-all.

```colibri
struct P {
    x: int
    y: int
}

enum Cmd {
    Move(dx: int, dy: int)
    Say(text: str)
    Quit
}

fn run(c: Cmd, p: P) -> str {
    return match (c, p) {
        (Move(0, 0), _)                         => "stay"
        (Move(dx, _), P { x, y }) if x + dx < 0 => "wall at y={y}"
        (Say(t), _)                             => "say {t}"
        (Quit | Move(_, _), _)                  => "other"
    }
}

fn main() {
    let p = P { x: 1, y: 2 }
    print(run(.Move(dx: 0, dy: 0), p))
    print(run(.Move(dx: -5, dy: 0), p))
    print(run(.Say(text: "hi"), p))
    print(run(.Quit, p))
    if let some(x) = [5, 6].first() {
        print(x)
    }
}
```

```output
stay
wall at y=2
say hi
other
5
```

## Structs

A struct is a value type: assignment, passing and returning copy it. Fields can have defaults,
and a struct with a reference-typed field copies the reference, not the object (a shallow copy;
`.clone()` is deep).

```colibri
struct Item {
    name: str
    qty: int = 1
}

fn main() {
    let a = Item { name: "pen" }
    let mut b = a
    b.qty = 10
    print(a)
    print(b)
    print(a == b)
}
```

```output
Item { name: "pen", qty: 1 }
Item { name: "pen", qty: 10 }
false
```

## Classes and weak references

A class is a heap object with identity. It is shared when assigned, reference counted, and freed
when the last reference goes. `==` on classes is identity unless the class implements `Eq`. There
is no inheritance.

A reference cycle is never freed, so back-pointers are `weak`. A `weak T` field reads as `T?`.

```colibri
class Node {
    name: str
    parent: weak Node
    children: List[Node] = []
}

fn main() {
    let root = Node { name: "root" }
    let kid = Node { name: "kid", parent: root }
    root.children.push(kid)
    if let some(p) = kid.parent {
        print("{kid.name}'s parent is {p.name}")
    }
    let alias = root
    alias.name = "renamed"
    print(root.name)
}
```

```output
kid's parent is root
renamed
```

`colibri run --leaks` reports cycles that were never freed.

## Enums

An enum is a tagged union whose cases may carry named or positional data. Where the expected
type is known, `.Case(...)` is enough. `Option` and `Result` are ordinary enums whose cases are
written `some`/`none` and `ok`/`err`. Enums are values.

```colibri
enum Shape {
    Circle(r: float)
    Rect(w: float, h: float)
    Dot
}

enum Level {
    Low
    Mid(n: int)
    High
}

fn main() {
    let s: Shape = .Circle(r: 2.0)
    print(s)
    print(Shape.Dot)
    print(Level.Low < Level.High)
    print(Level.Mid(5) < Level.Mid(7))
}
```

```output
Circle(r: 2.0)
Dot
true
true
```

## Methods and impl blocks

Methods are declared in `impl` blocks, apart from the data. `self` reads, and `mut self` mutates
a struct in place (the value it is called on must be declared with `let mut`). A function
without `self` is called on the type.
You cannot add methods to the built-in types (`List`, `str`, the numbers, ...). Today an `impl`
can be written for any other type your program can name, including a type from a dependency;
the rules about who may add what to a type are expected to get stricter.

```colibri
struct Counter {
    n: int
}

impl Counter {
    fn new() -> Counter => Counter { n: 0 }

    fn get(self) -> int => self.n

    fn bump(mut self) {
        self.n += 1
    }
}

fn main() {
    let mut c = Counter.new()
    c.bump()
    c.bump()
    print(c.get())
}
```

```output
2
```

## Derived behaviour

Structs, enums and tuples get `Eq`, `Hash`, `Show`, `Clone` and `Ord` from their fields when each
field has them, so they can be printed, compared, used as map keys and sorted with no code.
Structs compare field by field and enums by case first. Classes derive `Show` and `Clone`; they
compare and hash by identity unless you implement `Eq` and `Hash`, and are not ordered unless
you implement `Ord`.

```colibri
struct Version {
    major: int
    minor: int
}

fn main() {
    let mut vs = [Version { major: 1, minor: 10 }, Version { major: 1, minor: 2 }]
    vs.sort()
    print(vs)
    let mut seen = Set[Version]()
    seen.add(vs[0])
    print(seen.contains(Version { major: 1, minor: 2 }))
    print(compare(vs[0], vs[1]))
}
```

```output
[Version { major: 1, minor: 2 }, Version { major: 1, minor: 10 }]
true
-1
```

## Interfaces

An interface lists methods, and can give defaults. Types implement it explicitly. Used as a type
(`List[Shape]`) it is dispatched at run time; used as a bound (`T: Shape`) it is checked at
compile time. An interface of your own can be implemented for any type. A built-in interface (`Show`, `Eq`, ...)
cannot be implemented for a built-in type or a type of the standard library, only for types
written in Colibri outside it.

```colibri
interface Shape {
    fn area(self) -> float
    fn name(self) -> str => "shape"
}

struct Square {
    side: float
}

impl Shape for Square {
    fn area(self) -> float => self.side * self.side
}

fn report[T: Shape](s: T) {
    print("{s.name()} of area {s.area()}")
}

fn main() {
    report(Square { side: 3.0 })
    let shapes: List[Shape] = [Square { side: 1.0 }]
    print(shapes.len())
}
```

```output
shape of area 9.0
1
```

The built-in interfaces are `Eq`, `Ord`, `Hash`, `Show`, `Clone`, `Iter[T]`, `Iterable[T]`,
`IntoError`, `Drop` and the operator interfaces.

## Generics

Type parameters go in square brackets, with bounds after a colon (`T: Ord`, `T: Hash + Eq`).
Functions, structs, classes, enums, interfaces and aliases can be generic, and arguments are
inferred at the call. There is no variance: a `List[Circle]` is not a `List[Shape]`.

```colibri
import math

fn largest[T: Ord](items: List[T]) -> T? {
    let mut best = items.first()?
    for x in items {
        if x > best {
            best = x
        }
    }
    return some(best)
}

struct Pair[A, B] {
    first: A
    second: B
}

enum Tree[T] {
    Leaf
    Node(left: Tree[T], value: T, right: Tree[T])
}

fn depth[T](t: Tree[T]) -> int {
    return match t {
        Leaf          => 0
        Node(l, _, r) => 1 + math.max(depth(l), depth(r))
    }
}

fn main() {
    print(largest([3, 9, 4]))
    print(Pair { first: 1, second: "a" })
    let t = Tree.Node(left: .Leaf, value: 1, right: .Node(left: .Leaf, value: 2, right: .Leaf))
    print(depth(t))
}
```

```output
some(9)
Pair { first: 1, second: "a" }
2
```

## Operators on your own types

`+ - * /` and unary `-` are the interfaces `Add`, `Sub`, `Mul`, `Div` and `Neg`. `Add[R, O]` has a
right-hand type and a result type, both defaulting to `Self`. Comparison uses `Eq` and `Ord`.
`a += b` means `a = a + b`.

```colibri
struct Money {
    cents: int
}

impl Add for Money {
    fn add(self, other: Money) -> Money => Money { cents: self.cents + other.cents }
}

impl Mul[int] for Money {
    fn mul(self, k: int) -> Money => Money { cents: self.cents * k }
}

fn main() {
    let mut total = Money { cents: 250 }
    total += Money { cents: 50 }
    print((total * 3).cents)
}
```

```output
900
```

## Optionals

`T?` is `some(x)` or `none`, and there is no null. `?` returns `none` early from a function that
returns an optional. `or` supplies a default (or a block that returns or continues), `if let`
opens a value, and `.expect(msg)` panics with a message.

```colibri
fn find_user(id: int) -> str? {
    if id == 1 {
        return some("ana")
    }
    return none
}

fn shout(id: int) -> str? {
    let name = find_user(id)?
    return some(name.to_upper())
}

fn main() {
    print(shout(1))
    print(shout(2))
    print(shout(2) or "nobody")
    if let some(name) = shout(1) {
        print("hello {name}")
    }
    let n: int? = none
    print(n.is_none())
}
```

```output
some("ANA")
none
nobody
hello ANA
true
```

## Errors: `Result`, `?` and `or`

`Result[T]` is `ok(T)` or `err(Error)`, where `Error` is the one error type the language has: it
keeps the original value, so a caller can ask what it was (`e.is[fs.Error]()`, `e.as[T]()`), add
a message (`e.context("...")`) and read the chain (`"{e}"`). `Result[T, E]` fixes the error type;
`?` converts an `E` that implements `IntoError`.

```colibri
import fs
import str

fn load(path: str) -> Result[int] {
    let text = fs.read(path).context("loading {path}")?
    return ok(str.parse_int(text.trim())?)
}

fn main() {
    print(load("nope") or -1)
    let n = load("nope") or |e| {
        print("error: {e}")
        return
    }
    print(n)
}
```

```output
-1
error: loading nope: cannot read `nope`: no such file or directory
```

Postfix `?` binds tightest, and `or` binds looser than every other binary operator and chains:
`cache.get(k) or disk.load(k) or 0`. An unused `Result` is a warning; `let _ = f()` says it is
on purpose. A function that returns `Result[()]` and reaches its end returns `ok(())`.

## Panics and `defer`

A panic is for bugs: an index out of range, a division by zero, overflow, a failed `.expect`.
It prints a message and a stack trace and exits with code 101. It runs every `defer` and every
`Drop` on its way out. A coroutine's panic can be received as a value (see
[coroutines](#coroutines)).

```colibri
fn main() {
    defer print("first deferred")
    defer print("second deferred")
    print("body")
}
```

```output
body
second deferred
first deferred
```

`defer` runs a statement when the enclosing block exits, last one first, and also on `return`,
`?` and panics. A panic shows like this, and exits with code 101:

```console
$ colibri run oops.colibri
panic: index 5 out of bounds for list of length 3
  at main (oops.colibri:3:11)
```

## Lists, maps and sets

`List`, `Map` and `Set` are reference types: assignment shares them. `==` compares contents.
Maps and sets keep the order of insertion. `m[k]` and `xs[i]` panic when missing, and `get`
gives an optional.

```colibri
fn main() {
    let mut xs = [3, 1, 2]
    xs.push(4)
    xs.sort()
    print(xs)
    print(xs.map(fn(x) => x * x).filter(fn(x) => x > 1))
    print(xs.fold(0, fn(a, b) => a + b))
    print(xs[1..3])
    let mut m = Map[str, int]()
    m["a"] = 1
    m["b"] = m.get_or("b", 0) + 5
    print(m)
    for (k, v) in m {
        print("{k}={v}")
    }
    let s = Set[int]()
    s.add(1)
    s.add(1)
    print(s.len())
    let mut e = [Version { n: 2 }, Version { n: 1 }]
    e.sort_by_key(fn(v) => v.n)
    print(e)
}

struct Version {
    n: int
}
```

```output
[1, 2, 3, 4]
[4, 9, 16]
10
[2, 3]
{"a": 1, "b": 5}
a=1
b=5
1
[Version { n: 1 }, Version { n: 2 }]
```

`for mut x in xs` gives a place you can change. Changing a collection while you iterate it
panics ("list modified during iteration"); use `retain` or iterate a copy. The
[built-in types](builtins.md) page lists their methods.

## Iterators

`for` runs over anything with `fn next(mut self) -> T?` (`Iter[T]`) or `fn iter(self) -> Iter[T]`
(`Iterable[T]`): lists, maps, sets, ranges, strings and your own types.

```colibri
struct Countdown {
    n: int
}

impl Iter[int] for Countdown {
    fn next(mut self) -> int? {
        if self.n == 0 {
            return none
        }
        self.n -= 1
        return some(self.n + 1)
    }
}

fn main() {
    for x in (Countdown { n: 3 }) {
        print(x)
    }
    for (i, c) in "ab".chars().enumerate() {
        print("{i}{c}")
    }
}
```

```output
3
2
1
0a
1b
```

## Memory and `Drop`

Memory is reference counted and freed the moment the last reference goes, in a predictable order:
locals are released in reverse order at the end of their scope. A class can implement `Drop` to
release a resource. Structs cannot, because a copy would run it twice.

```colibri
class Guard {
    name: str
}

impl Drop for Guard {
    fn drop(self) {
        print("release {self.name}")
    }
}

fn main() {
    let a = Guard { name: "a" }
    {
        let b = Guard { name: "b" }
        print("inner {b.name}")
    }
    print("outer {a.name}")
}
```

```output
inner b
release b
outer a
release a
```

## Modules and packages

One file is one module and one folder is a package. `import store.cart` (the file
`store/cart.colibri`) makes `cart` available; `import store.cart as c` renames it and
`import store.cart.{Cart, new_cart}` brings names in directly. `import std.math` names the standard library when a local module has the same name.
A folder with a `colibri.toml` is a package that starts at `fn main()`, and the manifest lists
dependencies from a path or from git:

```toml
[package]
name = "shop"

[dependencies]
utils = { path = "../utils" }
greet = { git = "https://example.com/me/greet.git", tag = "v1.2.0" }
```

Imports between files of one package may form cycles, between packages they may not. The
exact commits of git dependencies go into `colibri.lock`. There is no package registry, and none is planned for v1. The
[tutorial](tutorial.md#10-a-real-program-counting-words) builds a package.

## Visibility

Visibility is decided per package, and **a folder is a package**: the files in one folder see each
other's names whether or not they say `pub`. A subfolder is another package, so a name it wants
to share with its parent needs `pub`. A struct with a private field can only be built inside its
own package, so a library offers a `pub fn` to build it.

Here `main.colibri` is in the package at the root, and `store/cart.colibri` is a module of the
package `store` (the import path is the folder and the file):

```toml,pkg=shop,file=colibri.toml
[package]
name = "shop"
```

```colibri,pkg=shop,file=store/cart.colibri
pub struct Cart {
    pub owner: str
    items: int
}

pub fn new_cart(owner: str) -> Cart => Cart { owner: owner, items: 0 }

fn secret() -> int => 42

impl Cart {
    pub fn describe(self) -> str => "{self.owner}: {self.items} items"
}
```

```colibri,pkg=shop,file=main.colibri
import store.cart

fn main() {
    let c = cart.new_cart("Ana")
    print(c.describe())
}
```

```output
Ana: 0 items
```

From the root package, `cart.Cart { owner: "Ana", items: 3 }` and `cart.secret()` do not compile:

```toml,pkg=shopbad,file=colibri.toml
[package]
name = "shopbad"
```

```colibri,pkg=shopbad,file=store/cart.colibri
pub struct Cart {
    pub owner: str
    items: int
}

fn secret() -> int => 42
```

```colibri,error,pkg=shopbad,file=main.colibri
import store.cart

fn main() {
    let c = cart.Cart { owner: "Ana", items: 3 }
    print(c.owner)
    print(cart.secret())
}
```

```output
the field `items` is not exported from the package `store`
`cart.secret` is private to the package `store`
```

## Conditional code and deprecation

`@when(condition)` keeps an item only where the condition holds. It can test `os`, `arch`,
`debug` and feature flags declared in `colibri.toml`, with `!`, `&&`, `||` and parentheses. A
condition with a misspelt name, or a value that no system has (`os == "win"`), is an error and not a quiet `false`. `@deprecated("message")` makes
each use of an item warn.

```colibri
@when(os == "windows")
fn platform() -> str => "windows"

@when(os != "windows")
fn platform() -> str => "unix-like"

@when(debug)
fn log(msg: str) {
    print("debug: {msg}")
}

@when(!debug)
fn log(msg: str) {}

fn main() {
    log("starting")
    print(platform() == "unix-like" || platform() == "windows")
}
```

```output
debug: starting
true
```

`colibri run --release` makes `debug` false, and `--feature NAME` turns a feature on.

## Coroutines

`go f(args)` starts a coroutine on the current thread and returns a handle at once. Any
function can pause with `wait(seconds)`; there is no `async` or `await`. `join()` waits for the
result, `join_result()` gives a `Result` so that a panic or a cancel does not end the joiner (see the
condition below), `cancel()` stops it at its next pause (running its `defer`s), and `done()` says whether it ended.
A `CoGroup` starts several and `join_all()` waits for them, and cancels what is left when dropped. A
`Signal` wakes coroutines that `wait_for` it.

```colibri
fn job(name: str, secs: float) -> str {
    wait(secs)
    return "{name} done"
}

fn boom() -> int {
    panic("no good")
}

fn waiter(s: Signal) {
    wait_for(s)
    print("signalled")
}

fn main() {
    let slow = go job("slow", 0.2)
    let fast = go job("fast", 0.1)
    print(fast.join())
    print(slow.join())
    match (go boom()).join_result() {
        ok(v)  => print(v)
        err(e) => print(e.message())
    }
    let s = Signal()
    go waiter(s)
    wait(0.01)
    s.emit()
    wait(0.01)
}
```

```output
fast done
slow done
the coroutine `boom` panicked: no good
signalled
```

`go` runs the new coroutine at once, up to its first pause. A panic that no `join_result` has
received ends the program (exit code 101) as soon as the code that started the coroutine pauses.
So `join_result` receives a panic when it is already waiting for that coroutine, or, for a
coroutine that panics before its first pause, when you call it before you pause again after the
`go`. A coroutine that panics later, while you wait for something else, ends the program.

`wait(seconds)` works anywhere, in `main` too. `wait_until`, `wait_for` and `wait_real` panic
when called outside a coroutine started with `go`, `main` included.

Coroutines never run at the same time, so they share objects without locks, and a coroutine runs
until it waits. `net`, `http`, `process` and `fs.File` calls pause only the calling coroutine. The
whole-file functions (`fs.read`, `fs.write`, ...) and `time.sleep` hold the thread until they
finish. Thread tasks and channels are **planned**.

## Bytes and JSON

`Bytes` is an immutable byte string that needs no import, and `BytesBuilder` builds one. `json`
encodes any value and decodes into a type, with `@rename` and `@skip` attributes for the shape of
the document; `json.Value` is for a document of unknown shape.

```colibri
import json

struct Item {
    name: str
    qty: int = 1
}

fn main() {
    let data = "héllo".to_bytes()
    print(data.len())
    print(data.to_str() or "not text")
    print(json.encode(Item { name: "pen" }) or "")
    let item = json.decode[Item](r#"{"name": "cup", "qty": 2}"#) or {
        return
    }
    print(item)
    let loose = json.parse(r#"{"a": [1, 2]}"#) or {
        return
    }
    print(loose.get("a") or json.Value.null())
}
```

```output
6
héllo
{"name":"pen","qty":1}
Item { name: "cup", qty: 2 }
[1,2]
```

## Tests

`test "name" { ... }` blocks live beside the code, are skipped by `colibri run`, and run with
`colibri test`. `assert(cond)` and `assert_eq(a, b)` fail a test and show both sides.

```colibri,test
fn clamp(x: int, lo: int, hi: int) -> int {
    if x < lo {
        return lo
    }
    if x > hi {
        return hi
    }
    return x
}

test "clamp keeps values in range" {
    assert_eq(clamp(5, 0, 10), 5)
    assert_eq(clamp(-3, 0, 10), 0)
    assert_eq(clamp(42, 0, 10), 10)
}
```

```output
running 1 test
test sample::clamp keeps values in range ... ok

1 passed, 0 failed
```

## Tools

| Command | What it does |
|---------|--------------|
| `colibri run [file or folder]` | check, then run; `--watch` reloads on every save |
| `colibri check [file or folder]` | type-check only |
| `colibri test [file or folder]` | run the tests (`--filter`, `--nocapture`) |
| `colibri fmt [paths]` | format in place (`--check` only reports) |
| `colibri build [file or folder] [-o FILE]` | one executable that holds the program |
| `colibri new <name>` | create a package |
| `colibri update` | re-resolve git dependencies |
| `colibri repl` | interactive session |
| `colibri lsp` | language server for editors |

`colibri run --leaks` reports reference cycles that were never freed. `colibri build` writes
one executable that holds the program and runs it like `colibri run`; it is a copy of `colibri`
with the sources appended, for the platform that built it.

## Embedding and FFI

A Rust program can load Colibri scripts, call their functions with typed arguments, and expose
Rust functions and types to them through the `colibri-runtime` crate (`cargo doc -p
colibri-runtime --open`); the [README](../README.md#embedding-in-rust) has an example, and
`examples/host_jobs` is a runnable one. `extern "C"` blocks call C libraries, and `@repr(C)`
structs are passed by value.
