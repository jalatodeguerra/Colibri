# Learn Colibri by building things

This tutorial takes you from an empty machine to a small, tested program and a web service. It
assumes you have written code before, in any language, and nothing else. Type the samples in as
you go, or copy them: every sample on this page is checked, and where it shows output, run, by
the project's tests.

If you already know several languages and want the whole language on a few pages, read the
[language tour](tour.md) instead. When you need to know what a module offers, the
[standard library reference](std/README.md) lists every function with its signature.

Colibri is early and pre-release. The language and the tools described here work today, and
things that are planned and not built are said to be planned.

## Contents

1. [Install it](#1-install-it)
2. [Hello, world, and a package](#2-hello-world-and-a-package)
3. [Values, variables and types](#3-values-variables-and-types)
4. [Functions](#4-functions)
5. [Control flow](#5-control-flow)
6. [Structs, classes and enums](#6-structs-classes-and-enums)
7. [Interfaces and generics](#7-interfaces-and-generics)
8. [Errors and optionals](#8-errors-and-optionals)
9. [Lists and maps](#9-lists-and-maps)
10. [A real program: counting words](#10-a-real-program-counting-words)
11. [Tests](#11-tests)
12. [Coroutines](#12-coroutines)
13. [A small HTTP service](#13-a-small-http-service)
14. [Where to go next](#14-where-to-go-next)

## 1. Install it

Colibri is built from source today. You need a recent stable [Rust toolchain](https://rustup.rs)
and a C compiler.

```sh
git clone https://github.com/jalatodeguerra/Colibri colibri
cd colibri
cargo install --path crates/colibri-cli
colibri --version
```

That puts one program, `colibri`, in `~/.cargo/bin`. It runs programs, checks them without
running them, runs tests, formats source, starts a REPL and a language server. You will meet
each of those below.

## 2. Hello, world, and a package

Save this as `hello.colibri`:

```colibri
fn main() {
    print("Hello, world!")
}
```

```output
Hello, world!
```

Run it with `colibri run hello.colibri`. A file is a program when it has a `fn main()`. A file
with no `main` is a script: its top-level statements run from the top down, like Python.

```colibri
let name = "Colibri"
print("Hello, {name}!")
```

```output
Hello, Colibri!
```

Anything inside `{ }` in a string is an expression and its value is put in the text. To write a
brace itself, double it: `"{{"`.

Real programs are packages, a folder with a `colibri.toml`. `colibri new` makes one that
already runs and has a test:

```console
$ colibri new hello
Created the package in `hello`.
Try:  cd hello && colibri run && colibri test
$ cd hello
$ colibri run
Hello, world!
$ colibri test
running 1 test
test main::greeting names who it greets ... ok

1 passed, 0 failed
```

Two more commands are worth knowing from the first day:

- `colibri check` type-checks without running, so it is quick and has no effects. Editors run
  the same checker as you type.
- `colibri fmt` formats your files. There is one style and no options, so it ends arguments about
  style. `colibri fmt --check` only reports.

The `colibri repl` command opens an interactive session if you want to try an expression:

```console
$ colibri repl
colibri> 21 * 2
= 42 : int
```

## 3. Values, variables and types

`let` makes a binding that cannot be reassigned, and `var` one that can. You almost never write
a type: the compiler infers it from what you assign.

```colibri
fn main() {
    let language = "Colibri"
    var year = 2026
    year += 1
    let pi = 3.14159
    let ready = true
    print("{language} {year} {pi} {ready}")
}
```

```output
Colibri 2027 3.14159 true
```

The types you will use most are `int` (64-bit), `float` (64-bit), `bool`, `str` (UTF-8 text) and
`char`. Sized numbers (`i8` to `u64`, `f32`) exist for file formats and C code. Numbers never
convert silently: you write `float(n)` or `int(x)` when you mean it.

```colibri
fn main() {
    print(7 / 2)
    print(7.0 / 2.0)
    let n = 7
    print(float(n) / 2.0)
    print(int(3.99))
    print(-7 % 3)
}
```

```output
3
3.5
3.5
3
-1
```

Integer division truncates toward zero, as in C and Go, so `7 / 2` is `3`. Integer arithmetic
that overflows panics instead of wrapping around, so a wrong answer cannot sneak through.

Strings interpolate expressions and can be formatted with a `:` after the value:

```colibri
fn main() {
    let price = 4.5
    let qty = 3
    print("{qty} x {price:.2} = {price * float(qty):.2}")
    print("[{qty:>5}] [{"ab":<5}] {255:x}")
}
```

```output
3 x 4.50 = 13.50
[    3] [ab   ] ff
```

The checker is there to help. It reports every error it finds, with the place and often a hint:

```colibri,error
fn main() {
    let count: int = "five"
    print(count)
}
```

```output
expected `int`, found `str`
```

A tuple groups a few values, and you take it apart with a pattern. (This sample uses a list, a
`for` loop and an `if`, which the next sections explain; it should read well enough now.)

```colibri
fn min_max(xs: List[int]) -> (int, int) {
    var lo = xs[0]
    var hi = xs[0]
    for x in xs {
        if x < lo {
            lo = x
        }
        if x > hi {
            hi = x
        }
    }
    return (lo, hi)
}

fn main() {
    let (lo, hi) = min_max([4, 9, 1, 7])
    print("{lo}..{hi}")
}
```

```output
1..9
```

## 4. Functions

A function states its parameter and result types, and the body is inferred. A block body uses
`return`. A body that is one expression is written with `=>`:

```colibri
fn add(a: int, b: int) -> int {
    return a + b
}

fn square(x: int) -> int => x * x

fn log(msg: str, level: str = "info") {
    print("[{level}] {msg}")
}

fn main() {
    print(add(2, 3))
    print(square(add(1, 3)))
    log("starting")
    log("disk almost full", level: "warn")
}
```

```output
5
16
[info] starting
[warn] disk almost full
```

A parameter can have a default, and a call can name an argument. A function that returns nothing
says nothing after the parameter list.

Functions are values. An anonymous function is `fn(x) => x * 2`, and it can use the variables
around it, even change them:

```colibri
fn apply_twice(f: fn(int) -> int, x: int) -> int {
    return f(f(x))
}

fn main() {
    print(apply_twice(fn(n) => n * 3, 2))

    var total = 0
    let add = fn(n: int) {
        total += n
    }
    add(5)
    add(6)
    print(total)

    let words = ["kiwi", "fig", "banana"]
    print(words.map(fn(w) => w.len()))
}
```

```output
18
11
[4, 3, 6]
```

`defer` runs a statement when the block ends, however it ends. It is how you close things:

```colibri
fn main() {
    defer print("last")
    print("first")
    print("second")
}
```

```output
first
second
last
```

## 5. Control flow

`if` is an expression, so it can sit on the right of a `let`. There are no parentheses around the
condition and the braces are required.

```colibri
fn classify(n: int) -> str {
    if n < 0 {
        return "negative"
    } else if n == 0 {
        return "zero"
    }
    return "positive"
}

fn main() {
    let n = 7
    let kind = if n % 2 == 0 { "even" } else { "odd" }
    print("{n} is {kind} and {classify(n)}")
}
```

```output
7 is odd and positive
```

Loops are `while` and `for`. A `for` walks a range, a list, a map, the characters of a string,
and anything else that can be iterated. `a..b` stops before `b` and `a..=b` includes it:

```colibri
fn main() {
    for i in 0..3 {
        print(i)
    }
    var n = 1
    while n < 20 {
        n *= 3
    }
    print(n)
    for (i, fruit) in ["fig", "plum"].enumerate() {
        print("{i}: {fruit}")
    }
    for c in "hé".chars() {
        print(c)
    }
}
```

```output
0
1
2
27
0: fig
1: plum
h
é
```

`break` and `continue` work as you expect, and a label (`'outer`) lets them name an outer loop.

`match` takes a value apart. It must cover every case, so adding a case to an enum later makes the
compiler show you each `match` to fix. Patterns can be literals, ranges, tuples, enum cases, and
can have an `if` guard:

```colibri
fn describe(n: int) -> str {
    return match n {
        0          => "zero"
        1..=9      => "a digit"
        x if x < 0 => "negative"
        _          => "big"
    }
}

fn main() {
    for n in [0, 5, -3, 40] {
        print("{n}: {describe(n)}")
    }
}
```

```output
0: zero
5: a digit
-3: negative
40: big
```

## 6. Structs, classes and enums

A `struct` is a record and a value: assigning one makes a copy.

```colibri
struct Point {
    x: int
    y: int
}

impl Point {
    fn origin() -> Point => Point { x: 0, y: 0 }

    fn dist2(self) -> int => self.x * self.x + self.y * self.y

    fn shift(var self, dx: int, dy: int) {
        self.x += dx
        self.y += dy
    }
}

fn main() {
    var a = Point.origin()
    a.shift(3, 4)
    var b = a
    b.x = 100
    print(a)
    print(b)
    print(a.dist2())
}
```

```output
Point { x: 3, y: 4 }
Point { x: 100, y: 4 }
25
```

Methods live in an `impl` block next to the type. `self` reads the value and `var self` may
change it, so the call needs a `var` to change. A function in the block with no `self`
(`origin`) is called on the type. Printing, `==`, hashing and ordering of a struct need no code:
the compiler provides them from the fields.

A `class` is a record with identity. It is shared, not copied, and reference counted, so it is
freed as soon as nothing uses it:

```colibri
class Account {
    owner: str
    balance: int = 0
}

impl Account {
    fn deposit(self, amount: int) {
        self.balance += amount
    }
}

fn main() {
    let acct = Account { owner: "Ana" }
    let same = acct
    same.deposit(50)
    print("{acct.owner} has {acct.balance}")
}
```

```output
Ana has 50
```

`let` stops you from rebinding `acct` and does not stop the object from changing: a `let` on a
class is like `final` in Java. Choose `struct` for plain data and `class` for things with identity
and a changing state. There is no inheritance; you share behaviour with interfaces.

An `enum` is one of several cases, and a case can carry data:

```colibri
enum Shape {
    Circle(r: float)
    Rect(w: float, h: float)
    Dot
}

fn area(s: Shape) -> float {
    return match s {
        Circle(r)  => 3.14159 * r * r
        Rect(w, h) => w * h
        Dot        => 0.0
    }
}

fn main() {
    let shapes = [Shape.Circle(r: 1.0), .Rect(w: 2.0, h: 3.0), .Dot]
    for s in shapes {
        print(area(s))
    }
}
```

```output
3.14159
6.0
0.0
```

Where the type is known, `.Rect(...)` is enough. Forget a case in the `match` and the compiler
tells you which one:

```colibri,error
enum Color {
    Red
    Green
    Blue
}

fn name(c: Color) -> str {
    return match c {
        Red   => "red"
        Green => "green"
    }
}

fn main() {
    print(name(.Red))
}
```

```output
non-exhaustive `match`: missing case: Blue
```

## 7. Interfaces and generics

An `interface` lists methods. A type implements it with an explicit `impl ... for`, and an
interface method can have a default body:

```colibri
interface Shape {
    fn area(self) -> float
    fn name(self) -> str => "shape"
}

struct Square {
    side: float
}

struct Circle {
    r: float
}

impl Shape for Square {
    fn area(self) -> float => self.side * self.side
    fn name(self) -> str => "square"
}

impl Shape for Circle {
    fn area(self) -> float => 3.0 * self.r * self.r
}

fn total_area(shapes: List[Shape]) -> float {
    var sum = 0.0
    for s in shapes {
        sum += s.area()
    }
    return sum
}

fn main() {
    let shapes: List[Shape] = [Square { side: 2.0 }, Circle { r: 1.0 }]
    print(total_area(shapes))
    for s in shapes {
        print(s.name())
    }
}
```

```output
7.0
square
shape
```

An interface used as a type, like `List[Shape]`, holds any implementing value and calls are
dispatched at run time. Used as a bound on a type parameter, it is checked at compile time.

Generics use square brackets. (`items.first()?` below is explained in the next section: it
returns `none` from the function when the list is empty.) A function or a type takes type parameters, and a bound says what
the parameter must be able to do:

```colibri
fn largest[T: Ord](items: List[T]) -> T? {
    var best = items.first()?
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

class Stack[T] {
    items: List[T] = []
}

impl[T] Stack[T] {
    fn push(self, x: T) {
        self.items.push(x)
    }

    fn pop(self) -> T? => self.items.pop()
}

fn main() {
    print(largest([3, 8, 1]))
    print(largest(["pear", "apple"]))
    print(Pair { first: 1, second: "one" })
    let s = Stack[int] {}
    s.push(1)
    s.push(2)
    print(s.pop())
}
```

```output
some(8)
some("pear")
Pair { first: 1, second: "one" }
some(2)
```

`Ord` is one of the interfaces that come with the language, with `Eq`, `Hash`, `Show` (what
`print` and `"{x}"` use), `Clone` and a few more. Structs and enums get `Eq`, `Hash`, `Show`,
`Clone` and `Ord` for free when their fields have them. To print a type your own way, implement
`Show`:

```colibri
struct Money {
    cents: int
}

impl Show for Money {
    fn show(self) -> str {
        let cents = "{self.cents % 100}".pad_left_with(2, '0')
        return "${self.cents / 100}.{cents}"
    }
}

fn main() {
    print(Money { cents: 1250 })
    print(Money { cents: 705 })
}
```

```output
$12.50
$7.05
```

## 8. Errors and optionals

There are no exceptions and no null. A function that can fail returns a `Result[T]`: either
`ok(value)` or `err(error)`. A value that may be absent is an optional, `T?`: either `some(value)`
or `none`.

```colibri
import str

fn parse_port(text: str) -> Result[int] {
    let n = str.parse_int(text)?
    if n < 1 || n > 65535 {
        return err("port out of range: {n}")
    }
    return ok(n)
}

fn main() {
    print(parse_port("8080"))
    print(parse_port("http"))
    print(parse_port("70000"))
}
```

```output
ok(8080)
err(cannot read `http` as an int: it has a character that is not a digit)
err(port out of range: 70000)
```

The `?` after a call means: if this is an error, return it from the current function, and
otherwise give me the value. That is the whole error-propagation mechanism. The function that
`?` is in must itself return a `Result` (or an optional, for an optional).

At the place where you decide what an error means, you have three tools.

`or` gives a fallback value:

```colibri
import str

fn main() {
    let port = str.parse_int("http") or 80
    print(port)
}
```

```output
80
```

`or` with a block runs code and may leave the function or the loop. Add `|e|` to see the error:

```colibri
import str

fn main() {
    let inputs = ["10", "x", "30"]
    var total = 0
    for text in inputs {
        let n = str.parse_int(text) or |e| {
            print("skipping `{text}`: {e}")
            continue
        }
        total += n
    }
    print(total)
}
```

```output
skipping `x`: cannot read `x` as an int: it has a character that is not a digit
40
```

`match` looks at both cases when you want to handle them differently, and `.expect("message")`
stops the program with a message when the error cannot happen in a correct program:

```colibri
import fs
import str

fn main() {
    match fs.read("missing.txt") {
        ok(text) => print(text)
        err(e)   => print("could not read: {e}")
    }
    let n = str.parse_int("42").expect("a literal number")
    print(n + 1)
}
```

```output
could not read: cannot read `missing.txt`: no such file or directory
43
```

You can add what you were doing to an error with `.context(...)`, and the error keeps its
original kind. Standard library errors are enums such as `fs.Error`, which you can ask for:

```colibri
import fs

fn load(path: str) -> Result[str] {
    return fs.read(path).context("loading the settings")
}

fn main() {
    match load("settings.txt") {
        ok(text) => print(text)
        err(e) => {
            print(e)
            print(e.is[fs.Error]())
        }
    }
}
```

```output
loading the settings: cannot read `settings.txt`: no such file or directory
true
```

You can make your own error types. An `enum` is a good fit, and `impl IntoError` lets `?` convert
it. The message comes from its `Show`:

```colibri
import str

enum ConfigError {
    Missing(key: str)
    Invalid(key: str, value: str)
}

impl Show for ConfigError {
    fn show(self) -> str {
        return match self {
            Missing(key)        => "missing setting `{key}`"
            Invalid(key, value) => "setting `{key}` has a bad value: {value}"
        }
    }
}

impl IntoError for ConfigError {}

fn lookup(settings: Map[str, str], key: str) -> Result[str, ConfigError] {
    return match settings.get(key) {
        some(v) => ok(v)
        none    => err(ConfigError.Missing(key: key))
    }
}

fn threads(settings: Map[str, str]) -> Result[int] {
    let text = lookup(settings, "threads")?
    let n = str.parse_int(text) or {
        return err(ConfigError.Invalid(key: "threads", value: text))
    }
    return ok(n)
}

fn main() {
    print(threads({"threads": "8"}))
    print(threads({"threads": "many"}))
    print(threads({"colour": "red"}))
}
```

```output
ok(8)
err(setting `threads` has a bad value: many)
err(missing setting `threads`)
```

Optionals work the same way. `?` on an optional returns `none` early, `or` supplies a default,
and `if let` opens the value:

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
}
```

```output
some("ANA")
none
nobody
hello ANA
```

Some failures are bugs, not conditions to handle: an index out of range, a division by zero, an
overflow, a failed `.expect`. They are panics. A panic prints a message and a stack trace and ends
the program with code 101. Inside a coroutine it can be received as a value, as you will see in
section 12.

## 9. Lists and maps

A `List[T]` is a growable array and a `Map[K, V]` is a hash map that keeps the order in which keys
were added. Both are shared, not copied, when you assign them. Literals use `[ ]` and `{ : }`.

```colibri
fn main() {
    var xs = [5, 3, 9]
    xs.push(1)
    xs.sort()
    print(xs)
    print(xs.len())
    print(xs[0])
    print(xs[1..3])
    print(xs.filter(fn(x) => x > 3).map(fn(x) => x * 10))
    print(xs.contains(9))
    print(xs.first())
    print(xs.get(10))
    print(xs.fold(0, fn(sum, x) => sum + x))
    print(["b", "a"].join("-"))
}
```

```output
[1, 3, 5, 9]
4
1
[3, 5]
[50, 90]
true
some(1)
none
18
b-a
```

An index that is out of range is a panic, and `get` gives an optional when you are not sure.
`for var x in xs` lets you change each element in place.

```colibri
fn main() {
    let ages = {"ana": 30, "bo": 25}
    var stock = Map[str, int]()
    stock["pens"] = 12
    stock["ink"] = 3
    stock["pens"] += 1
    print(stock)
    print(stock.get("pens"))
    print(stock.get_or("paper", 0))
    print(stock.contains_key("ink"))
    stock.remove("ink")
    print(stock.len())
    for (name, age) in ages {
        print("{name} is {age}")
    }
    print(stock.keys())
}
```

```output
{"pens": 13, "ink": 3}
some(13)
0
true
1
ana is 30
bo is 25
["pens"]
```

`stock["pens"]` panics when the key is missing, and `get` and `get_or` do not. There is also a
`Set[T]` with `add`, `contains` and the usual set operations. All three hold any type that can be
hashed (for `Map` keys and `Set` elements) or anything at all (for list elements), and structs
and enums can be hashed without any code.

Sorting by a key and counting are everyday tasks:

```colibri
struct Entry {
    word: str
    count: int
}

fn main() {
    var counts = Map[str, int]()
    for w in "to be or not to be".split(" ") {
        counts[w] = counts.get_or(w, 0) + 1
    }
    var entries = counts.entries().map(fn(e) => Entry { word: e.0, count: e.1 })
    entries.sort_by_key(fn(e) => -e.count)
    for e in entries {
        print("{e.word}: {e.count}")
    }
}
```

```output
to: 2
be: 2
or: 1
not: 1
```

The sort is stable, so words with the same count stay in the order they were first seen.

## 10. A real program: counting words

Now we put these pieces together in a package. It has three files: a manifest, a module with the
counting logic, and the entry file. Make a folder for it (or run `colibri new wordfreq` and
replace the files):

```toml,pkg=wordfreq,file=colibri.toml
[package]
name = "wordfreq"
```

Each file is a module and each folder is a package. Names are private to their package unless
they say `pub`. This module is `text.words`, because it is the file `text/words.colibri`:

```colibri,pkg=wordfreq,file=text/words.colibri
pub struct Entry {
    pub word: str
    pub count: int
}

// Splits text into lower-case words and counts them, most frequent first. Ties keep the
// order of first appearance.
pub fn count(text: str) -> List[Entry] {
    var counts = Map[str, int]()
    for raw in text.to_lower().split(" ") {
        let word = raw.trim()
        if word != "" {
            counts[word] = counts.get_or(word, 0) + 1
        }
    }
    var entries = counts.entries().map(fn(e) => Entry { word: e.0, count: e.1 })
    entries.sort_by_key(fn(e) => -e.count)
    return entries
}
```

The entry file imports it with the name of its path. Imports bring the module in as a name:
`import text.words` is used as `words.count(...)`.

```colibri,pkg=wordfreq,file=main.colibri
import fs
import text.words

fn report(text: str) {
    for e in words.count(text) {
        print("{e.word}: {e.count}")
    }
}

fn main() -> Result[()] {
    fs.write("notes.txt", "the cat and the dog and the bird")?
    let text = fs.read("notes.txt").context("reading notes.txt")?
    report(text)
}
```

```output
the: 3
and: 2
cat: 1
dog: 1
bird: 1
```

`colibri run` in that folder runs it. Three things are new here:

- `main` returns a `Result[()]`. If it ends with an `err`, the program prints the error chain
  and exits with code 1. When a function returning `Result[()]` reaches the end of its body, it
  returns `ok(())` without being told to.
- `fs.write` and `fs.read` return results, so `?` passes failures up. We wrote the file first so
  that the sample is self-contained. A real program would read the path from `os.args()`, or
  parse flags with the `args` module.
- The standard library is imported by name, `import fs`. If a module of your own is also called
  `fs`, write `import std.fs`.

A package can depend on other packages in `colibri.toml`, from a folder on disk or from git:

```toml
[dependencies]
utils = { path = "../utils" }
greet = { git = "https://example.com/me/greet.git", tag = "v1.2.0" }
```

`colibri run`, `check` and `test` fetch what is missing and write the exact commits to
`colibri.lock`, which you commit. `colibri update` moves them forward.

## 11. Tests

A test is a block next to the code it tests. Add this to the end of `text/words.colibri`, or a
file of its own. Tests are not part of the program when you `run` it:

```colibri,test,pkg=wordfreq,file=text/words_test.colibri
import text.words

test "the most frequent word comes first" {
    let entries = words.count("a b a c a b")
    assert_eq(entries[0].word, "a")
    assert_eq(entries[0].count, 3)
    assert_eq(entries.len(), 3)
}

test "case and spacing do not matter" {
    let entries = words.count("  Go go  GO ")
    assert_eq(entries.len(), 1)
    assert_eq(entries[0].count, 3)
}

test "no text, no words" {
    assert(words.count("").is_empty())
}
```

```output
running 3 tests
test text.words_test::the most frequent word comes first ... ok
test text.words_test::case and spacing do not matter ... ok
test text.words_test::no text, no words ... ok

3 passed, 0 failed
```

`colibri test` finds every test in the folder, runs each one on its own, and does not stop at the
first failure. `assert(condition)` and `assert_eq(left, right)` fail a test with a message that
shows both sides. A single file works too, so you can try a test without a package:

```colibri,test
fn double(n: int) -> int => n * 2

test "doubling" {
    assert_eq(double(2), 4)
}
```

```output
running 1 test
test sample::doubling ... ok

1 passed, 0 failed
```

If the test said `assert_eq(double(2), 5)`, the run would end with exit code 1 and this report:

```console
running 1 test
test t::doubling ... FAILED

failures:

---- t::doubling ----
panic: assertion failed: `left == right`
  left: 4
 right: 5
  at test "doubling" (t.colibri:4:5)

0 passed, 1 failed
```

Use `colibri test --filter double` to run the tests whose name contains some text, and
`--nocapture` to see what the tests print as they run (otherwise only the failing ones show it).

## 12. Coroutines

Programs spend much of their time waiting for a file, a network answer or a timer. A coroutine is
a function that can be paused at such a point while another one runs, on the same thread. You
start one with `go`, and there is no `async` and no `await`: any function can wait, so nothing
is split into two kinds.

```colibri
fn fetch(name: str, secs: float) -> str {
    wait(secs)
    return "{name} done"
}

fn main() {
    let slow = go fetch("slow", 0.2)
    let fast = go fetch("fast", 0.1)
    print(fast.join())
    print(slow.join())
}
```

```output
fast done
slow done
```

`go fetch(...)` starts the call and gives back a handle at once. `join()` waits for the answer.
Both functions wait at the same time, so the program takes 0.2 seconds, not 0.3. `wait` pauses
only the coroutine that calls it. Calls on the network (`net`, `http`), on subprocesses
(`process`) and on open file handles (`fs.File`) pause only the coroutine that makes them too, so
while one waits for an answer, others run. Not everything does: the whole-file functions
(`fs.read`, `fs.write`, ...) and `time.sleep` hold the thread until they finish and give no one else
a turn. Inside a coroutine, use `wait` to pause and `fs.File` for big files.

Coroutines never run in parallel with each other, so they can share a `class` object with no
locks. A coroutine runs until it waits, so keep long computations out of the ones that serve
requests.

What if the coroutine panics? `join` would panic too. `join_result()` returns a `Result`
instead, so you can decide. Here each coroutine is joined as soon as it is started:

```colibri
fn risky(n: int) -> int {
    if n == 0 {
        panic("zero is not allowed")
    }
    return 100 / n
}

fn main() {
    print((go risky(5)).join_result())
    match (go risky(0)).join_result() {
        ok(v)  => print("value {v}")
        err(e) => print("failed: {e.message()}")
    }
}
```

```output
ok(20)
failed: the coroutine `risky` panicked: zero is not allowed
```

There is a condition. `go` runs the new coroutine at once, up to its first pause. A panic that no
`join_result` has received ends the program, with exit code 101, as soon as the code that started
the coroutine pauses (with `wait`, a `join`, or a call that waits). So `join_result` receives a
panic when it is already waiting for that coroutine, or, for a coroutine that panics before its
first pause, when you call it before you pause again after the `go`. Here the panic happens inside
`go`, and `wait` comes before the `join_result`, so the program ends:

```colibri,panics
fn risky() -> int {
    panic("zero is not allowed")
}

fn main() {
    print("started")
    let handle = go risky()
    wait(0.1)
    print("not reached")
    print(handle.join_result())
}
```

```output
started
```

So join a coroutine straight after you start it, or start it from a coroutine that waits for it. That is how `httpd` survives a handler that panics: it starts
each handler and waits for it at once. To run several things and wait for all of
them, use a `CoGroup`. It also cancels whatever is still running when it is dropped:

```colibri
fn work(name: str, secs: float) {
    wait(secs)
    print("{name} finished")
}

fn main() {
    let group = CoGroup()
    group.go(work("a", 0.1))
    group.go(work("b", 0.2))
    group.join_all()
    print("all done")
}
```

```output
a finished
b finished
all done
```

After `main` returns, `colibri run` keeps going until every coroutine has finished, so a
program that only starts coroutines needs no loop of its own. `cancel()` stops a coroutine at its
next wait, running its `defer`s.

## 13. A small HTTP service

The standard library has an HTTP server, `httpd`. It is written in Colibri on top of the `net`
module, and every connection is a coroutine. We will write a notes service with two routes.

A handler is a function from a `httpd.Request` to a `Result[httpd.Response]`. An `err(httpd.fail(
404, "..."))` answers with that status, and any other error is a 500. Put the service in
`service.colibri`:

```toml,pkg=notes,file=colibri.toml
[package]
name = "notes"
```

```colibri,test,pkg=notes,file=service.colibri
import httpd

pub struct Note {
    pub id: int
    pub text: str
}

pub class Store {
    pub notes: List[Note] = []
}

fn list_notes(store: Store, r: httpd.Request) -> Result[httpd.Response] {
    return httpd.json(200, store.notes)
}

fn add_note(store: Store, r: httpd.Request) -> Result[httpd.Response] {
    let text = r.text()?.trim()
    if text == "" {
        return err(httpd.fail(400, "a note needs some text"))
    }
    let note = Note { id: store.notes.len() + 1, text: text }
    store.notes.push(note)
    return httpd.json(201, note)
}

fn get_note(store: Store, r: httpd.Request) -> Result[httpd.Response] {
    let id = r.param("id") or { "" }
    for note in store.notes {
        if "{note.id}" == id {
            return httpd.json(200, note)
        }
    }
    return err(httpd.fail(404, "there is no note {id}"))
}

pub fn router(store: Store) -> httpd.Router {
    let router = httpd.Router()
    router.get("/notes", fn(r) => list_notes(store, r))
    router.post("/notes", fn(r) => add_note(store, r))
    router.get("/notes/:id", fn(r) => get_note(store, r))
    return router
}

fn call(handler: httpd.Handler, method: str, target: str, body: str) -> httpd.Response {
    let request = httpd.test_request(method, target, body: body.to_bytes())
    return handler(request) or |e| {
        panic("the handler failed: {e}")
    }
}

test "a posted note can be read back" {
    let handler = router(Store {}).handler()
    let created = call(handler, "POST", "/notes", "buy milk")
    assert_eq(created.status, 201)
    let found = call(handler, "GET", "/notes/1", "")
    assert_eq(found.status, 200)
    match found.body {
        Text(text) => assert_eq(text, r#"{"id":1,"text":"buy milk"}"#)
        _          => panic("expected a text body")
    }
}

test "an empty note is refused with a 400" {
    let handler = router(Store {}).handler()
    let result = handler(httpd.test_request("POST", "/notes", body: "  ".to_bytes()))
    match result {
        ok(_)  => panic("expected an error")
        err(e) => assert_eq(e.as[httpd.Failure]().expect("a Failure").status, 400)
    }
}
```

```output
running 2 tests
test service::a posted note can be read back ... ok
test service::an empty note is refused with a 400 ... ok

2 passed, 0 failed
```

Look at what was not needed: no socket, no port. `httpd.test_request` builds a request, and the
router is a function you call, so a handler is tested like any other function. The router answers
404 for an unknown path and 405 (with an `Allow` header) for a known path and a wrong method on
its own.

Now the entry file. A server needs a client to talk to it. Rather than ask you to open a second
terminal, this `main` starts the server on a port the system picks (`:0`), starts a client
coroutine that makes three requests and then stops the server:

```colibri,pkg=notes,file=main.colibri
import http
import httpd
import service

fn client(base: str, server: httpd.Server) {
    defer server.stop()
    let created = http.post("{base}/notes", "buy milk") or |e| {
        print("request failed: {e}")
        return
    }
    print("{created.status} {created.body}")
    let missing = http.get("{base}/notes/7") or |e| {
        print("request failed: {e}")
        return
    }
    print("{missing.status} {missing.body.trim()}")
    let all = http.get("{base}/notes") or |e| {
        print("request failed: {e}")
        return
    }
    print("{all.status} {all.body}")
}

fn main() -> Result[()] {
    let server = httpd.listen("127.0.0.1:0")?
    let router = service.router(service.Store {})
    go client("http://{server.addr()}", server)
    return server.serve(router.handler())
}
```

```output
201 {"id":1,"text":"buy milk"}
404 there is no note 7
200 [{"id":1,"text":"buy milk"}]
```

To run a real service, drop the client and serve on an address you choose. `serve` runs until
`stop` is called. The package now has two files with a `main`; `colibri run` runs the one named
`main.colibri`, and `colibri run serve.colibri` runs this one:

```colibri,no-run,pkg=notes,file=serve.colibri
import httpd
import service

fn main() -> Result[()] {
    let router = service.router(service.Store {})
    return httpd.serve("127.0.0.1:8080", router.handler())
}
```

While it runs, `curl -d 'buy milk' http://127.0.0.1:8080/notes` creates a note and
`curl http://127.0.0.1:8080/notes/1` reads it back.

Before you put a server like this on a network, read the limits in the
[`httpd` reference](std/httpd.md): `httpd.Options` bounds the size of a request, the time a
client may take and the number of connections, and a request that breaks the HTTP syntax is
refused. TLS (https), HTTP/2 and WebSockets are not built; put a reverse proxy in front for
https. For a bigger example with a typed JSON body, a streamed response and a graceful stop,
read [`examples/scripts/notes`](../examples/scripts/notes/notes.colibri).

## 14. Where to go next

- The [language tour](tour.md) is one short section for each feature, for looking things up.
- The [standard library reference](std/README.md) lists every module: files and paths, JSON,
  TOML and CSV, regular expressions, subprocesses, dates, an HTTP client, TCP sockets, and more.
- `examples/scripts` has four larger programs, and `examples/host_jobs` shows Colibri embedded in
  a Rust program.
- `colibri run --watch` reloads your program every time you save, and `colibri lsp` gives your
  editor diagnostics, hover, completion and rename. The VS Code extension is in `editors/vscode`.
- The [README](../README.md) lists what is planned and what is not.
