# Debugging

`colibri debug` is a debugger for Colibri programs. It speaks the [Debug Adapter
Protocol](https://microsoft.github.io/debug-adapter-protocol/), so an editor starts it and shows
breakpoints, the call stack and variables. The VS Code extension in `editors/vscode` does that;
this page is about what you can do once it runs. The samples are ordinary programs, with a
comment where a breakpoint would go.

- [Starting a session](#starting-a-session)
- [Breakpoints](#breakpoints)
- [Logpoints and hit counts](#logpoints-and-hit-counts)
- [Stepping](#stepping)
- [Stopping at a panic](#stopping-at-a-panic)
- [Variables](#variables)
- [Coroutines](#coroutines)
- [Editing while it runs](#editing-while-it-runs)
- [Ending a session](#ending-a-session)
- [What it does not do](#what-it-does-not-do)

## Starting a session

In VS Code, open a `.colibri` file and press F5, or add a configuration to
`.vscode/launch.json` (the snippets "Colibri: Launch ..." write one):

```text
{
  "type": "colibri",
  "request": "launch",
  "name": "Colibri: launch the current file",
  "program": "${file}",
  "cwd": "${workspaceFolder}"
}
```

`program` is a script or a package folder. The other attributes are optional:

| Attribute | What it does |
|-----------|--------------|
| `args` | the arguments the program sees as `os.args()` |
| `cwd` | the folder the program runs in |
| `features` | feature flags for `@when` conditions; fixed by the first launch of a session |
| `stopOnEntry` | stop at the first statement |
| `noDebug` | run without the debugger ("Run Without Debugging") |
| `watch` | reload the program when its files change (see [below](#editing-while-it-runs)) |

The extension starts `colibri debug` from the `colibri.server.path` setting, the same
executable as the language server. Any other editor that speaks the protocol can start
`colibri debug` and send the same `launch` request. The program runs in debug mode, with the
`debug` condition true, every capability allowed and no standard input (the editor's input is
the protocol). What it prints shows in the debug console.

## Breakpoints

A breakpoint is a line. The program stops before the first statement that starts on it, each
time it gets there.

```colibri
fn main() {
    let mut total = 0
    for i in 1..4 {
        total += i // breakpoint: stops three times, with i = 1, 2, 3
    }
    print(total)
}
```

```output
6
```

A breakpoint can sit in a function that has not been called yet. The editor first shows it
as not verified, because the function is not compiled; when the program gets there, the
adapter sends the editor a change that marks it verified, just before the program stops.

```colibri
fn later(n: int) -> int {
    let doubled = n * 2 // breakpoint: not verified until `later` is first called
    return doubled
}

fn main() {
    print("start")
    print(later(21))
}
```

```output
start
42
```

A line with no statement on it (a comment, a closing brace) never verifies.

## Logpoints and hit counts

A logpoint writes a message to the debug console instead of stopping. `{name}` in the message
is the local `name` of the frame; `{{` and `}}` are braces.

```colibri
fn main() {
    for i in 1..4 {
        let square = i * i // logpoint: "i is {i}, square will be {square}"
        print(square)
    }
}
```

```output
1
4
9
```

A breakpoint can have a hit count instead of a condition: `3` stops at the third time only,
`>= 3` at the third and after, `% 2` at every second time. Conditions on values are not
supported yet; a hit count with a logpoint or an ordinary breakpoint covers most of what they
are used for.

```colibri
fn main() {
    let mut n = 0
    while n < 10 {
        n += 1 // breakpoint with hit count 5: stops with n = 4
    }
    print(n)
}
```

```output
10
```

## Stepping

Step over runs a line, calls included. Step in goes into the next call, and into the closure
that `map`, `filter` and the like call: the code of those built-ins is hidden, so you never
land in it. Step out runs until the function returns.

```colibri
fn double(x: int) -> int {
    return x * 2
}

fn main() {
    let xs = [1, 2, 3]
    let doubled = xs.map(fn(x) => double(x)) // step in: the closure, then `double`
    print(doubled)
}
```

```output
[2, 4, 6]
```

## Stopping at a panic

With the "panic" exception filter on, the program stops where a panic is raised, before any
`defer` runs, with the locals of that frame.

```colibri panics
fn average(xs: List[int]) -> int {
    let total = 0
    let count = xs.len()
    return total / count // stops here when xs is empty: division by zero
}

fn main() {
    print(average([]))
}
```

```output
```

The filter stops at every new panic, also one that a `join_result` will turn into an `err`.

## Variables

The Variables view shows the locals of the frame, and a Module scope with the module's values.
Structs, classes, enums, lists, maps, sets and tuples expand, with their types. Showing a value
never runs your code: a `Show` you wrote is not called, so the debugger cannot change what the
program prints.

```colibri
struct Point {
    x: int
    y: int
}

enum Shape {
    Circle(radius: float)
    Dot(at: Point)
}

fn main() {
    let shape = Shape.Dot(at: Point { x: 1, y: 2 })
    let names = ["a", "b"]
    print(names.len()) // breakpoint: shape expands to Dot(at: Point { x: 1, y: 2 })
    match shape {
        Shape.Dot(at)        => print(at.x)
        Shape.Circle(radius) => print(radius)
    }
}
```

```output
2
1
```

The debug console reads paths: `shape`, `names[1]` or `shape.at.x`. It does not evaluate
expressions, so `names.len()` is refused.

## Coroutines

Every coroutine is a thread of the debugger, named for the function it started and its number:
`go fetch (#12)`. The main computation is thread 1, `main`. When a coroutine stops, it is the
thread of the stop event, and the paused coroutines are listed with their frames and locals,
like the ones that are running.

```colibri
fn worker(name: str, n: int) {
    let local = n * 10
    wait(0.01)
    print("{name} {local}") // breakpoint: threads `go worker (#..)`, one for each worker
}

fn main() {
    let a = go worker("a", 1)
    let b = go worker("b", 2)
    a.join()
    b.join()
}
```

```output
a 10
b 20
```

While a coroutine is the one that stopped, the computation that started or is waiting for it
has no frames to read: thread 1 is listed, and its stack is empty until it is the one that
runs again. Stepping stays with the thread you step.

A coroutine that panics does not stop the others by itself; with the panic filter on, the stop
is on the thread that panicked.

```colibri panics
fn crash(n: int) {
    let xs: List[int] = []
    print(xs[n])
}

fn main() {
    let c = go crash(3)
    c.join()
}
```

```output
```

## Editing while it runs

With `"watch": true` the debugger reloads the program the way `colibri run --watch` does: while
the coroutines run, between two ticks, changed files are looked at. A change that only touches
function bodies is patched in, and the next call of each function runs the new body. Anything
else (a new field, a changed signature) restarts the program, and `main` runs again.

Breakpoints stay where you put them. A breakpoint finds its file again by name after a reload,
so a restart that renumbers the files keeps it, and it stops in the new body. If you move the
code, move the breakpoint: it stays on its line number.

```colibri no-run
fn price(n: int) -> int {
    return n * 2 // breakpoint; edit this line while the program runs
}

fn main() {
    go ticker()
}

fn ticker() {
    let mut i = 0
    while true {
        print("price {price(i)}")
        wait(1.0)
        i += 1
    }
}
```

A program stopped at a breakpoint cannot reload until it goes on, and a `main` that never
returns and never waits cannot be reloaded at all.

## Ending a session

Stopping the session (the red square, or closing the editor) ends the program: the `defer`s run, and
no breakpoint stops during that, so cleanup code finishes (see the limits below).

```colibri
fn main() {
    defer print("cleanup runs even when the debugger ends the program")
    print("working") // stop here, then press Stop
}
```

```output
working
cleanup runs even when the debugger ends the program
```

A program that is blocked in a call (`time.sleep`, a read that never returns) or idle in the
event loop cannot be paused or stopped at a safepoint. The adapter ends it after a moment; if
the editor has gone away, the adapter exits after half a minute even if a `defer` is still running.

## What it does not do

- It does not evaluate expressions: `evaluate` reads paths (`point.x`, `xs[2]`, `m["key"]`).
- Conditions on values are refused; hit counts and logpoints work.
- It does not change variables (`setVariable`).
- A breakpoint in a module's top-level code that runs while the program loads is set too late
  to hit.
- A coroutine that is running (it started or resumed the one that stopped) shows no frames.
- Debug mode compiles one step for each statement, so a debugged program is slower than a
  normal run. Its output, and the steps it is charged, are the same.
- Output of a program that `process.status` starts goes to the adapter's standard error, not to
  the debug console. C code called through `extern` that writes to standard output on Windows
  still reaches the protocol stream.
