# A tour of the language

This page is for programmers who know a few languages already. It is one short section for each
feature, with a sample that runs. For a gentler path that builds a program, read the
[tutorial](tutorial.md); for the library, see the [standard library reference](std/README.md).

Colibri is statically typed with local inference, uses braces and has no semicolons. It is
reference counted (no garbage collector), has no exceptions and no null, and has coroutines
instead of `async`. It is early and pre-release, and what is planned but not built is marked
**planned**.

A statement ends at the end of its line, so a line that starts with an operator is a new
statement: `let c = a` followed by `- b` on the next line is `let c = a` and an unused `-b`. The
checker warns about a statement that only computes a plain value and drops it; end the first
line with the operator (`let c = a -`) or wrap the expression in parentheses to continue it.

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
- [Thread tasks](#thread-tasks)
- [Channels](#channels)
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
[classes](#classes-and-weak-references)). So `let mut` is needed only to assign to the variable
(`x = 1`, `x += 1`) or to change a struct held in it (`p.x = 1`, a `mut self` method). A list, map or
set is a reference, so `let xs = []` followed by `xs.push(1)` is fine, and a `let mut xs` that is
never assigned gets an `unused mut` warning: remove the `mut`. The same goes for a `mut` parameter
that the function never changes.

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

A parameter cannot be assigned to, like a `let`. Write `mut` before its name (`fn f(mut n: int)`)
to get a local copy that the body may change. The caller's variable never changes: a number or a
struct is copied in, so `n = n - 1` and `p.x = 0` stay inside the function. A list, map, set or
class object is a reference, so `xs.push(1)` still reaches the caller's list, while `xs = []` only
rebinds the parameter. `mut` is part of the body, not of the signature, so an `impl` may add or
leave out the `mut` that its interface method has, and it has no effect on the type of the function.

```colibri
struct Point {
    x: int
    y: int
}

fn countdown(mut n: int) -> int {
    let mut steps = 0
    while n > 0 {
        n -= 1
        steps += 1
    }
    return steps
}

fn moved(mut p: Point, dx: int) -> Point {
    p.x += dx
    return p
}

fn main() {
    let start = Point { x: 1, y: 2 }
    print(countdown(3))
    print(moved(start, 10))
    print(start)
}
```

```output
3
Point { x: 11, y: 2 }
Point { x: 1, y: 2 }
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
    let fs: List[fn() -> int] = []
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
    let xs = [1, 2, 3]
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

A pattern sees a weak field the same way: `let Node { parent, .. } = kid` binds `parent` as a
`Node?`, and so does `Node { parent: p, .. }` in a `match`. Write `some(p)` or `none` there to
test it.

```colibri
class Node {
    name: str
    parent: weak Node
}

fn main() {
    let root = Node { name: "root" }
    let kid = Node { name: "kid", parent: root }
    let Node { name, parent } = kid
    print("{name} has a parent: {parent.is_some()}")
    match kid {
        Node { parent: some(p), .. } => print("under {p.name}")
        Node { parent: none, .. }    => print("an orphan")
    }
}
```

```output
kid has a parent: true
under root
```

`colibri run --leaks` reports cycles that were never freed. A cycle is reported when it goes
through a class object or a closure; a cycle made only of lists, maps and sets is not.

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
    let vs = [Version { major: 1, minor: 10 }, Version { major: 1, minor: 2 }]
    vs.sort()
    print(vs)
    let seen = Set[Version]()
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
    let xs = [3, 1, 2]
    xs.push(4)
    xs.sort()
    print(xs)
    print(xs.map(fn(x) => x * x).filter(fn(x) => x > 1))
    print(xs.fold(0, fn(a, b) => a + b))
    print(xs[1..3])
    let m = Map[str, int]()
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
    let e = [Version { n: 2 }, Version { n: 1 }]
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

An import is private to the file that writes it: `pub import` is an error, because nothing is
re-exported, and another file that needs the module imports it too.

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
finish. Thread tasks, channels, `Shared` and `Mutex` are in the next sections.

## Thread tasks

`task.run(f)` runs a closure on a worker thread and gives a `Task[T]`, where `T` is what the
closure returns. The workers are a pool with one thread for each core, so tasks run in parallel with
the program and with each other. `join()` waits for the result as a `Result[T]`, `done()` says
whether the task ended, and `try_join()` gives the result if it is there (`Result[T]?`) and does
not wait.

```colibri
fn sum_to(n: int) -> int {
    let mut total = 0
    for i in 1..=n {
        total += i
    }
    return total
}

fn divide(a: int, b: int) -> int {
    return a / b
}

fn main() -> Result[()] {
    let a = task.run(fn() => sum_to(1000))
    let b = task.run(fn() => sum_to(2000))
    print("{a.join()?} {b.join()?}")

    // A panic in a task is an `err` for whoever joins it; the program goes on.
    let bad = task.run(fn() => divide(1, 0))
    match bad.join() {
        ok(v)  => print("value {v}")
        err(e) => print("failed: {e.message()}")
    }
    return ok(())
}
```

```output
500500 2001000
failed: the task `<closure in main>` panicked: division by zero
```

The error says which task panicked and why, and `e.cause()` is the stack of the task. A task that
runs out of the instruction budget or the memory limit of its `Vm` gives an `err` as well, and does
not stop the program.

A task is an **isolate**: it has its own objects. Everything the closure captures is copied to the
task when it starts, and the result is copied back by `join`. So a task cannot change what the
program holds, and the program cannot change what the task holds, and no locks are needed. The
price is that **a write to a captured `let mut` inside the closure is lost**, because the closure
runs on a copy:

```colibri
fn main() -> Result[()] {
    let mut n = 0
    task.run(fn() { n += 1 }).join()?
    print(n)
    return ok(())
}
```

```output
0
```

To get a value out of a task, return it. A copy is a structural one: shared parts and cycles inside
what is copied stay shared and cyclic in the copy, a `weak` reference to something outside it is
`none`, and `Drop` runs for the copy as for any other object. What cannot be copied is refused,
with the way to the part: a host handle such as a `net.Conn`, an iterator, a `Coroutine`,
`CoGroup`, `Signal` or `Task`. The checker says so at the `task.run` where it can see the type
(`` `closure.conn` is a `net.Conn`, which cannot be sent to another thread ``) and the run time
says it where it cannot (inside a generic function).

Module-level state belongs to the main thread. A task may read a `const`, which every thread
builds for itself, and call the functions that use only those; a `let` or `let mut` of the module,
read or written from a task, is a compile error where the checker follows the calls and a panic
that names the variable where it cannot (a `Drop` that runs implicitly, say). Host functions work
only if the host registered them for workers.

`join` waits in the way that fits where it is called. In a coroutine it pauses that coroutine and
the others go on; on the main fiber it runs the event loop until the task is done; where neither is
possible (a `Drop` hook, a callback of the host) it blocks the thread. Its result can be taken
once: a second `join` or `try_join` of the same task gives an `err` that says it was joined
already. A task may start tasks and join them. When the pool's threads are all busy, a worker that
joins a task that has not started runs it itself, so nested tasks cannot deadlock a pool of any
size. The task runs as it would on a thread of its own: it has its own instruction budget, its own
memory-limit standing, the tasks it starts are numbered from it, it has the scheduler to itself (the
coroutines of its joiner wait until it ends, and it can start and join coroutines of its own, even
when the joiner is a coroutine), and the coroutines it leaves are cancelled when it ends. The one difference is the stack: a deep chain of tasks that each join the
next, all run this way, shares one thread's stack and can hit its limit (a clean panic that says
so) where separate threads would not. A `Task` has one owner: `t.clone()` is a compile error.

Dropping the `Task` without joining it **detaches** it: the task goes on, and its result is thrown
away. There is no `cancel()`. A task that has not ended when the program does is stopped (at its
next safepoint, with its `defer`s run), so `join` what the program needs. The random numbers of a
task (`rand`) come from a generator of its own, seeded from the program's seed and from where
the task was started (which task started it, and as its how-manyth), so a run repeats whichever
thread runs the task, nested tasks included. A task also starts with a clock of its own. Thread tasks do not exist in the web build: `task.run`
is a compile error there.

## Channels

A `Channel[T]` carries values of type `T` from one place to another, and is how tasks and
coroutines talk to each other. `Channel[T]()` holds any number of values; `Channel[T](capacity: n)`
holds at most `n` (at least 1), and a `send` that finds it full waits until a `recv` makes room.

| Call | What it does |
|------|--------------|
| `ch.send(v)` | copy `v` into the channel; waits while a bounded channel is full; a panic if the channel is closed |
| `ch.recv() -> T?` | the next value, waiting for one; `none` once the channel is closed and empty |
| `ch.try_recv() -> T?` | the next value if there is one, `none` otherwise (it never waits) |
| `ch.close()` | no more values will be sent; what is queued can still be received; closing twice is fine |
| `for x in ch` | receives until the channel is closed and empty |

```colibri
fn produce(ch: Channel[int]) {
    for i in 1..=5 {
        ch.send(i * i)
    }
    ch.close()
}

fn main() -> Result[()] {
    let ch = Channel[int](capacity: 2)
    let t = task.run(fn() => produce(ch))
    let mut sum = 0
    for v in ch {
        sum += v
    }
    t.join()?
    print("sum = {sum}")
    return ok(())
}
```

```output
sum = 55
```

The channel itself is not copied when a closure that captures it is sent to a task, or when it is
sent through another channel: every holder has a handle to the one channel, from any thread, and
dropping a handle closes nothing, so call `close()` when the senders are done. A value that is sent
is **copied**, as the closure of a task is (the same rules say what can be copied and what is
refused), and the receiver gets the copy; a send of something that is more than plain data has to
be written as a visible copy, `ch.send(x.clone())`, and that clone is the copy (the value is not
copied a second time unless the program has an `impl Clone` or an `impl Drop` of its own, which then
run as for any `.clone()`). A `Channel` can be a value of a channel: `Channel[Channel[str]]` is how
a request carries the channel that its answer comes back on.

Waiting fits where it happens, as `join` does: a coroutine that waits pauses and the others go on,
the main fiber runs the event loop, and anywhere else the thread blocks. Several receivers are
served in the order they began to wait, and a value goes straight to a receiver that is waiting. A
coroutine that is cancelled while it waits does not take a value with it. Things to know:

- A task that waits on a channel (`recv`, or a `send` on a full channel) keeps its worker thread,
  so give the pool enough threads for the tasks that wait on each other. A `join` is different:
  the main fiber of a task with no coroutines runs the task it joins itself if nobody has started
  it, and a coroutine that joins, or a task that has coroutines, waits for it instead and goes on
  running them, because the task may be waiting for one of them (or for the fiber that is joining).
  If every thread of the pool is busy then, the pool lends a temporary thread for the task, which
  ends when it has nothing to run. At most 256 such threads exist at once; a program that nests
  more joins than that waits for ever.
- A `recv` that nothing will ever satisfy is not detected: a coroutine that waits on a channel
  nobody closes keeps the event loop running.
- The order of values from several senders is not fixed, but the values of one sender arrive in the
  order it sent them.
- A `send` on a closed channel panics with `cannot send on a closed channel`, also for a sender
  that was waiting when the channel was closed.
- A value that does not fit in the memory limit when it is copied into the channel, or rebuilt out
  of it, is that limit failing, as for a task. The receive that fails has taken the value out of
  the channel, so the value is lost with the receiver that could not hold it.

## Shared

`Shared(x)` copies `x` once into a frozen value that every thread task can read, and gives a
`Shared[T]` handle for it. The handle is sent to tasks like a `Channel`: it is not copied, and
`.clone()` of it is the same handle. `s.get()` gives the calling thread its own copy of the value,
built the first time that thread asks and kept after that, so a second `get()` is the same object
(a class compares by identity) and costs nothing.

```colibri
class Table {
    rows: List[int]
}

fn total(s: Shared[Table]) -> int {
    let mut sum = 0
    for r in s.get().rows {
        sum += r
    }
    return sum
}

fn main() {
    let s = Shared(Table { rows: [1, 2, 3, 4] })
    let t = task.run(fn() => total(s))
    print("total {t.join().expect("task")}")
    print("same copy twice: {s.get() == s.get()}")
    let r = task.run(fn() => s.get().rows.push(5)).join()
    print(r.is_err())
}
```

```output
total 10
same copy twice: true
true
```

The copy is **frozen**: lists, maps, sets and class objects in it cannot be written, in the main
thread as much as in a task. Every way of changing them (an index or field assignment, `+=`, `push`,
`pop`, `insert`, `remove`, `clear`, `sort`, `reverse`, `for mut`, a `mut` method of an element, a
`StrBuilder` or `BytesBuilder` that is in it, ...) panics with ``cannot modify a frozen value (it
came from `Shared`)``. Reading is free. A struct is a value, so a struct you took out of the copy
is yours to change, as it is anywhere.

To change the data, make a new value from it. A `clone()` of a frozen value, a value that you send
back through a task result or a channel, and a result of `sorted`, `map` and the like are ordinary
copies and are not frozen. The copy is built by the same rules as a task's closure, so a `Shared`
can hold what a task can be sent. One more thing is refused: a closure that captures a `let mut`
variable that is written somewhere (a `let mut` that is never written is fine), because that
variable would have to be changed by every copy.
Strings are copied, not frozen, as they are never changed in place.

Things to know:

- Each thread that calls `get()` holds its own copy of the whole value, so a `Shared` of N bytes costs
  about N bytes for each thread that reads it, plus the one frozen original. The copies count in the
  memory limit of the program.
- The copies are freed when the last handle is gone. A worker thread that is idle keeps its copy
  until its next task begins, and the end of the program frees everything.
- A type with an `impl Drop` in a `Shared` has its hook run once for each copy that is freed, not
  once for the value. The hook may write the fields of its own object (it is dying, and nothing else
  can see it), but what the object holds stays frozen: a hook that pushes to a list in a field
  panics. That panic is the result of `main` when the copy is main's, and is lost when the copy is
  a worker's.
- `Shared` is for data that is read. `Mutex`, for data that tasks change, is the next section.

## Mutex

`Mutex(x)` holds a value that threads change one at a time, and gives a `Mutex[T]` handle. Like a
`Channel` or a `Shared`, the handle is sent to tasks as it is, and `.clone()` of it is the same
handle. The value is kept in a form that belongs to no thread: every operation copies it out of the
mutex or into it, so a thread never holds an object that another thread can see.

- `m.get()` gives a copy of the value.
- `m.set(v)` puts a copy of `v` in.
- `m.with(fn(mut x) { ... })` takes the lock, gives the callback a copy of the value, and puts the
  copy back when the callback returns. What the callback returns is the result of `with`.

```colibri
fn add(m: Mutex[int], n: int) {
    for _ in 0..n {
        m.with(fn(mut x) { x += 1 })
    }
}

fn main() {
    let m = Mutex(0)
    let a = task.run(fn() => add(m, 500))
    let b = task.run(fn() => add(m, 500))
    a.join().expect("a")
    b.join().expect("b")
    print("count {m.get()}")
    let old = m.with(fn(mut x) {
        let before = x
        x = 7
        return before
    })
    print("was {old}, now {m.get()}")
}
```

```output
count 1000
was 1000, now 7
```

`get` followed by `set` is two operations, and another task can run between them; use `with` for a
change that depends on the value it replaces.

Things to know:

- **The callback may not use any `Mutex`.** A `get`, `set` or `with` of the same mutex or another
  one, inside a `with`, panics with a message that says so. A thread that holds one lock and asks for
  another is how locks deadlock, and this rule leaves no way to do it.
- **The callback may not wait, and may not call what could.** `wait`, `join` of any task, `recv` of
  any channel (and `for x in ch`), and `send` on a channel that has a capacity panic inside a
  `with`. They panic whether or not they would have had to wait at that moment, so a program does
  not pass on one run and fail on another. `done`, `try_join`, `try_recv` and `send` on a channel
  without a capacity do not wait, and work. A lock is never held while a thread waits for anything.
  Joining a coroutine (`join`, `join_result`, `join_all`) panics the same way, even for one that has
  ended. (`go` is fine: the new coroutine runs on a fiber of its own and may pause on a `wait`, a
  `recv` or a `send`, which holds nothing, and the `with` goes on.)
- **A panic in the callback leaves the value as it was.** The lock is released and nothing is
  poisoned: the copy the callback changed is dropped, and the next `get` sees the value from before
  the call. The panic goes on to the caller of `with`, in a task it is the task's result. The same
  holds when the program is stopped while the callback runs.
- A value is copied by the rules of a task's closure, so a `Mutex` can hold what a task can be sent,
  and a closure that captures a `let mut` that is written somewhere is refused.
- A struct in a `Mutex` is a value as ever: the `mut x` of the callback is a copy that is stored
  back. A class object is copied too (with what it holds), and identity does not survive the trip.
- Each operation costs a copy of the whole value. Keep the value small, or keep many small mutexes.
- A type with an `impl Drop` in a `Mutex` has its hook run once for each copy that is freed: the
  copy `with` hands to its function, the copy it takes back, and each `get`, not once for the value.
  The hooks of the copy the function leaves behind run after the lock is let go, so such a hook may
  use a `Mutex` (to count live objects, say). A copy that dies inside the function (a parameter that
  is reassigned, a local) runs its hook under the lock, where using a `Mutex` panics.
- A `Shared` that is inside a `Mutex`, or inside a message that is never received, may keep its copies
  in idle worker threads until those workers run another task: they are freed when the last handle
  that is a variable of a program dies, and at the latest at the end of the program.

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
| `colibri build --web [file or folder]` | a folder for a static web server instead: the program runs in the page |
| `colibri new <name>` | create a package |
| `colibri update` | re-resolve git dependencies |
| `colibri repl` | interactive session |
| `colibri lsp` | language server for editors |
| `colibri debug` | debug adapter (DAP) on stdin and stdout, for an editor to set breakpoints and step |
| `colibri run --profile[=FILE]` | sample the program and write a flame graph (`--profile-rc` counts reference-count traffic) |

`colibri run --leaks` reports reference cycles that were never freed. `colibri build` writes
one executable that holds the program and runs it like `colibri run`; it is a copy of `colibri`
with the sources appended, for the platform that built it.

## Embedding and FFI

A Rust program can load Colibri scripts, call their functions with typed arguments, and expose
Rust functions and types to them through the `colibri-runtime` crate (`cargo doc -p
colibri-runtime --open`); the [README](../README.md#embedding-in-rust) has an example, and
`examples/host_jobs` is a runnable one. `extern "C"` blocks call C libraries, and `@repr(C)`
structs are passed by value.
