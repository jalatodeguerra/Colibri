# Built-in types and functions

The types and functions on this page need no `import`. The [standard library
reference](std/README.md) covers the modules you import. In the tables, `T` is the element type,
`K` and `V` are the key and value types of a map, and `U` is a type the method picks for itself.
An optional result, `T?`, is `none` when there is nothing to return.

- [Functions you can always call](#functions-you-can-always-call)
- [Numbers](#numbers)
- [`str`](#str)
- [`StrBuilder`](#strbuilder)
- [`List[T]`](#listt)
- [`Map[K, V]`](#mapk-v)
- [`Set[T]`](#sett)
- [Ranges](#ranges)
- [`Bytes` and `BytesBuilder`](#bytes-and-bytesbuilder)
- [Optionals, `Result` and `Error`](#optionals-result-and-error)
- [`Coroutine`, `CoGroup` and `Signal`](#coroutine-cogroup-and-signal)

## Functions you can always call

| Function | What it does |
|----------|--------------|
| `print(value)` | writes the value, shown as text, and a newline to standard output |
| `panic(message) -> !` | ends the program (or the coroutine, or the test) with a message and a stack trace |
| `assert(condition)`, `assert(condition, message)` | panics when the condition is false |
| `assert_eq(left, right)` | panics when the two are not equal, and shows both |
| `compare(a, b) -> int` | -1, 0 or 1 for two values of an ordered type (tuples work too) |
| `error(message) -> Error` | an `Error` made from a message |
| `some(x)`, `none`, `ok(x)`, `err(e)` | the cases of `T?` and `Result` |
| `int(x)`, `float(x)`, `u8(x)`, `i32(x)`, `f32(x)`, ... | convert between number types |
| `wait(seconds)`, `wait_real(seconds)`, `wait_until(condition)`, `wait_for(signal)` | pause a coroutine |
| `Map[K, V]()`, `Set[T]()`, `StrBuilder()`, `BytesBuilder()`, `Bytes(list)`, `CoGroup()`, `Signal()` | make a value of a built-in type |

```colibri
fn main() {
    assert(1 < 2)
    assert_eq(1 + 1, 2)
    print(compare(1, 2))
    print(compare("b", "a"))
    print(compare((1, "z"), (1, "a")))
    let e = error("bad input").context("while testing")
    print(e)
}
```

```output
-1
1
1
while testing: bad input
```

## Numbers

A number type's name converts to it: `int(3.9)` is `3`, and `u8(n)` panics when `n` does not fit.
`T.MAX` and `T.MIN` are the limits of an integer type, and `float.INFINITY`, `float.NAN` and
`float.EPSILON` exist. The module [`math`](std/math.md) has the rest of the maths.

| Method | On | What it does |
|--------|----|--------------|
| `abs()` | signed integers, floats | absolute value (panics for the smallest integer) |
| `min(other)`, `max(other)` | all numbers | the smaller or the larger of the two |
| `pow(exp: int)` | integers | power; panics on overflow |
| `wrapping_add(o)`, `wrapping_sub(o)`, `wrapping_mul(o)` | integers | wrap around instead of panicking |
| `saturating_add(o)`, `saturating_sub(o)`, `saturating_mul(o)` | integers | stop at the limit |
| `checked_add(o)`, `checked_sub(o)`, `checked_mul(o)`, `checked_div(o)` | integers | `T?`, `none` on overflow or division by zero |
| `sqrt()`, `floor()`, `ceil()`, `round()` | floats | `round` rounds half away from zero |
| `is_nan()` | floats | whether the value is not a number |

```colibri
fn main() {
    print(2.pow(10))
    print((-3).abs())
    print(u8(250).saturating_add(10))
    print(u8(250).wrapping_add(10))
    print(int.MAX.checked_add(1))
    print((2.5).round())
    print(float.NAN.is_nan())
    print(7.min(9))
}
```

```output
1024
3
255
4
none
3.0
true
7
```

## `str`

A `str` is immutable UTF-8 text. Indexes and lengths count bytes, and `s[a..b]` slices by bytes
(it panics inside a character).

| Method | What it does |
|--------|--------------|
| `len()`, `is_empty()` | the length in bytes, and whether it is 0 |
| `chars()`, `bytes()` | iterators over the characters and over the bytes |
| `contains(sub)`, `starts_with(prefix)`, `ends_with(suffix)` | tests |
| `find(sub)`, `rfind(sub)` | the byte index of the first or last match, `int?` |
| `split(sep)` | `List[str]` |
| `split_once(sep)`, `rsplit_once(sep)` | `(str, str)?`, split at the first or last `sep` |
| `trim()`, `to_upper()`, `to_lower()` | a new string |
| `replace(from, to)` | a new string with every match replaced |
| `pad_left(width)`, `pad_right(width)` | pad with spaces to a width |
| `pad_left_with(width, fill)`, `pad_right_with(width, fill)` | pad with a `char` |
| `to_bytes()` | the UTF-8 bytes, as `Bytes` |

The module [`str`](std/str.md) adds `parse_int`, `parse_float`, `lines`, `repeat` and more.

```colibri
fn main() {
    let s = "a-b-c"
    print(s.split("-"))
    print(s.split_once("-"))
    print(s.rsplit_once("-"))
    print(s.find("b"))
    print(s.replace("-", "+"))
    print("x".pad_left_with(3, '0'))
    print("héllo".len())
    print("  hi ".trim().to_upper())
    print(s.starts_with("a-"))
}
```

```output
["a", "b", "c"]
some(("a", "b-c"))
some(("a-b", "c"))
some(2)
a+b+c
00x
6
HI
true
```

## `StrBuilder`

Builds text without copying it at every step. It is a reference type, like a list.

| Method | What it does |
|--------|--------------|
| `push(s)`, `push_char(c)` | append a string or a character |
| `write(value)` | append anything that can be shown |
| `len()`, `is_empty()`, `clear()` | size and reset |
| `to_str()` | the text |

```colibri
fn main() {
    let b = StrBuilder()
    b.push("a")
    b.push_char('b')
    b.write(42)
    print(b.to_str())
    print(b.len())
}
```

```output
ab42
4
```

## `List[T]`

A growable array. Assigning a list shares it. `xs[i]` panics when `i` is out of range. A few
methods need something of `T`: `sort`, `min`, `max` need `Ord`, `contains`, `index_of` and
`dedup` need `Eq`, `sum` needs numbers and `join` needs strings.

| Method | What it does |
|--------|--------------|
| `len()`, `is_empty()`, `clear()` | size and reset |
| `push(item)`, `pop() -> T?` | add or take the last element |
| `insert(index, item)`, `remove(index) -> T` | add or take at a position |
| `extend(other)` | append another list |
| `first() -> T?`, `last() -> T?`, `get(index) -> T?` | safe reads |
| `contains(item)`, `index_of(item) -> int?` | search by `==` |
| `find(f) -> T?`, `find_index(f) -> int?`, `any(f)`, `all(f)` | search by a test |
| `map(f) -> List[U]`, `filter(f)`, `flat_map(f)` | new lists |
| `retain(f)` | keep the elements `f` accepts, in place |
| `fold(init, f) -> U`, `for_each(f)` | walk the list |
| `sort()`, `sort_by(f)`, `sort_by_key(f)` | sort in place (stable); `f` of two elements gives an `int`, a key function gives anything ordered |
| `sorted()`, `sorted_by(f)` | a sorted copy |
| `reverse()`, `dedup()` | in place; `dedup` drops neighbours that are equal |
| `min() -> T?`, `max() -> T?`, `sum() -> T`, `join(sep) -> str` | reductions |
| `zip(other) -> List[(T, U)]` | pairs |
| `chunks(size)`, `windows(size)` | `List[List[T]]` |
| `iter()`, `enumerate()` | iterate; `enumerate` gives `(int, T)` |

```colibri
fn main() {
    var xs = [3, 1, 2]
    xs.push(4)
    xs.insert(1, 9)
    print(xs)
    print(xs.remove(0))
    xs.sort()
    print(xs)
    print(xs.sorted_by(fn(a, b) => compare(b, a)))
    print(xs.map(fn(x) => x * 2).filter(fn(x) => x > 4))
    print(xs.find(fn(x) => x > 2))
    print(xs.find_index(fn(x) => x > 2))
    print(xs.index_of(9))
    print(xs.any(fn(x) => x > 8))
    print(xs.all(fn(x) => x > 1))
    print(xs.fold(0, fn(sum, x) => sum + x))
    print(xs.sum())
    print(xs.min())
    print(xs.max())
    print(["b", "a"].join("+"))
    print(xs.zip(["a", "b", "c", "d"]))
    print(xs.chunks(3))
    print(xs.windows(3))
    print([[1], [2, 3]].flat_map(fn(x) => x))
    var d = [1, 1, 2, 2, 1]
    d.dedup()
    print(d)
    xs.retain(fn(x) => x != 9)
    xs.reverse()
    print(xs)
    xs.extend([7])
    print(xs.last())
    print(xs.get(10))
    xs.for_each(fn(x) {
        print(x)
    })
}
```

```output
[3, 9, 1, 2, 4]
3
[1, 2, 4, 9]
[9, 4, 2, 1]
[8, 18]
some(4)
some(2)
some(3)
true
false
16
16
some(1)
some(9)
b+a
[(1, "a"), (2, "b"), (4, "c"), (9, "d")]
[[1, 2, 4], [9]]
[[1, 2, 4], [2, 4, 9]]
[1, 2, 3]
[1, 2, 1]
[4, 2, 1]
some(7)
none
4
2
1
7
```

## `Map[K, V]`

A hash map that keeps the order in which keys were first added. `K` must be hashable. Assigning
a map shares it. `m[k]` panics when the key is missing; `m[k] = v` adds or replaces.

| Method | What it does |
|--------|--------------|
| `len()`, `is_empty()`, `clear()` | size and reset |
| `get(key) -> V?`, `get_or(key, default) -> V` | reads that do not panic |
| `contains_key(key)` | whether the key is there |
| `insert(key, value)` | the same as `m[key] = value` |
| `remove(key) -> V?` | removes the key and gives its value |
| `keys() -> List[K]`, `values() -> List[V]` | copies, in order |
| `entries() -> List[(K, V)]` | the pairs, in order |
| `iter()` | iterate pairs; `for (k, v) in m` does the same |

```colibri
fn main() {
    var m = {"a": 1}
    m.insert("b", 2)
    m["c"] = 3
    print(m)
    print(m.get("a"))
    print(m.get_or("z", 0))
    print(m.contains_key("b"))
    print(m.remove("a"))
    print(m.keys())
    print(m.values())
    print(m.entries())
    for (k, v) in m {
        print("{k}={v}")
    }
    m.clear()
    print(m.is_empty())
}
```

```output
{"a": 1, "b": 2, "c": 3}
some(1)
0
true
some(1)
["b", "c"]
[2, 3]
[("b", 2), ("c", 3)]
b=2
c=3
true
```

## `Set[T]`

A hash set that keeps insertion order. `T` must be hashable.

| Method | What it does |
|--------|--------------|
| `len()`, `is_empty()`, `clear()` | size and reset |
| `add(item)`, `remove(item) -> bool`, `contains(item)` | change and test |
| `union(o)`, `intersection(o)`, `difference(o)`, `symmetric_difference(o)` | a new set |
| `is_subset(o)`, `is_superset(o)`, `is_disjoint(o)` | tests |
| `iter()` | iterate; `for x in set` does the same |

```colibri
fn main() {
    let a = Set[int]()
    a.add(1)
    a.add(2)
    a.add(2)
    let b = Set[int]()
    b.add(2)
    b.add(3)
    print(a.len())
    print(a.union(b))
    print(a.intersection(b))
    print(a.difference(b))
    print(a.symmetric_difference(b))
    print(a.is_subset(b))
    print(a.contains(1))
    print(a.remove(1))
}
```

```output
2
Set([1, 2, 3])
Set([2])
Set([1])
Set([1, 3])
false
true
true
```

## Ranges

`a..b` stops before `b` and `a..=b` includes it. Ranges are iterable, work in patterns and slice
lists and strings.

| Method | What it does |
|--------|--------------|
| `contains(value)` | membership |
| `len()`, `is_empty()` | size |
| `iter()`, `rev()` | iterate forward or backward |
| `to_list()` | a list of the values |

```colibri
fn main() {
    let r = 1..5
    print(r.len())
    print(r.contains(5))
    print(r.to_list())
    for i in (0..3).rev() {
        print(i)
    }
}
```

```output
4
false
[1, 2, 3, 4]
2
1
0
```

## `Bytes` and `BytesBuilder`

`Bytes` is an immutable string of bytes. It is a value, can be a map key, and `b[i]` is a `u8`.
`BytesBuilder` builds one a piece at a time, and is a reference type.

| `Bytes` method | What it does |
|----------------|--------------|
| `len()`, `is_empty()` | size |
| `get(index) -> u8?`, `slice(start, end)` | reads; `b[a..b]` slices too |
| `iter()`, `to_list() -> List[u8]` | the bytes |
| `to_str() -> Result[str]`, `to_str_lossy() -> str` | decode UTF-8, or replace what is not |
| `starts_with(b)`, `ends_with(b)`, `contains(b)`, `find(b) -> int?` | search |

`BytesBuilder` has `push(byte)`, `push_bytes(b)`, `push_str(s)`, `len()`, `is_empty()`,
`clear()` and `to_bytes()`.

```colibri
fn main() {
    let b = Bytes([104, 105, 255])
    print(b)
    print(b.len())
    print(b[0])
    print(b.to_str().is_err())
    print(b.to_str_lossy())
    print(b.find(Bytes([105])))
    let out = BytesBuilder()
    out.push(0xCA)
    out.push_str("hi")
    out.push_bytes(b)
    print(out.to_bytes())
}
```

```output
b"hi\xff"
3
104
true
hi�
some(1)
b"\xcahihi\xff"
```

## Optionals, `Result` and `Error`

| On | Method | What it does |
|----|--------|--------------|
| `T?` | `is_some()`, `is_none()` | tests |
| `T?` | `expect(msg) -> T` | the value, or a panic with the message |
| `T?` | `map(f) -> U?`, `and_then(f) -> U?` | change the value; `f` of `and_then` returns an optional |
| `Result[T]` | `is_ok()`, `is_err()` | tests |
| `Result[T]` | `expect(msg) -> T` | the value, or a panic with the message |
| `Result[T]` | `map(f)`, `and_then(f)` | change the value; `f` of `and_then` returns a `Result` |
| `Result[T]` | `ok() -> T?`, `err() -> Error?` | one side as an optional |
| `Result[T]` | `context(msg) -> Result[T]` | wrap an error with a message |
| `Error` | `message() -> str` | the message of this error alone, without the errors it wraps (`"{e}"` shows the whole chain) |
| `Error` | `context(msg) -> Error` | wrap with a message |
| `Error` | `cause() -> Error?` | the error that was wrapped |
| `Error` | `is[T]() -> bool`, `as[T]() -> T?` | whether the error holds a `T`, and the `T` |

```colibri
fn main() {
    let o: int? = some(2)
    print(o.map(fn(x) => x + 1))
    print(o.and_then(fn(x) => some(x * 5)))
    print(o.is_some())
    let r: Result[int] = ok(3)
    print(r.map(fn(x) => x + 1))
    print(r.and_then(fn(x) => ok(x * 2)))
    print(r.ok())
    let bad: Result[int] = err("boom")
    print(bad.err())
    print(bad.is_err())
    let e = error("inner").context("outer")
    print(e.message())
    print(e.cause())
    print(e.is[str]())
}
```

```output
some(3)
some(10)
true
ok(4)
ok(6)
some(3)
some(boom)
true
outer
some(inner)
true
```

## `Coroutine`, `CoGroup` and `Signal`

`go f(args)` gives a `Coroutine[T]`, where `T` is what `f` returns.

| Type | Method | What it does |
|------|--------|--------------|
| `Coroutine[T]` | `join() -> T` | wait for the result |
| `Coroutine[T]` | `join_result() -> Result[T]` | wait; a panic or a cancel is an `err` |
| `Coroutine[T]` | `cancel()`, `done() -> bool` | stop it at its next pause; whether it ended |
| `CoGroup` | `go(call)` | start a coroutine that the group owns, as in `group.go(work(1))` |
| `CoGroup` | `join_all()`, `cancel()`, `count() -> int` | wait for all, cancel all, how many are running |
| `Signal` | `emit()` | wake every coroutine waiting on it with `wait_for(signal)` |

```colibri
fn work(n: int) -> int {
    wait(0.01)
    return n * 2
}

fn waiter(s: Signal) {
    wait_for(s)
    print("woken")
}

fn main() {
    let c = go work(4)
    print(c.join())
    print(c.done())
    let group = CoGroup()
    group.go(work(1))
    group.go(work(2))
    print(group.count())
    group.join_all()
    let s = Signal()
    go waiter(s)
    wait(0.01)
    s.emit()
    wait(0.01)
}
```

```output
8
true
2
woken
```
