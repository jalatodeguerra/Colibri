# Language examples corpus

The ```colibri blocks of the Colibri design notes, one per fenced block. The parser, formatter
and checker tests run over every block (`crates/colibri-syntax/tests/parser.rs`, `fmt.rs`,
`crates/colibri-check/tests/robustness.rs`). Add a block here when a new example should be covered.

```colibri
import fs
import str

struct Account {
    owner: str
    balance: int = 0
}

enum Command {
    Deposit(int)
    Withdraw(int)
    Close
}

fn greet(name: str) -> str => "Hello, {name}!"

fn apply(a: Account, cmd: Command) -> Result[Account] {
    let mut next = a // structs are values: this is a copy
    match cmd {
        Deposit(n) => next.balance += n
        Withdraw(n) => {
            if n > next.balance {
                return err("insufficient funds")
            }
            next.balance -= n
        }
        Close => next.balance = 0
    }
    return ok(next)
}

fn load_limit(path: str) -> Result[int] {
    let text = fs.read(path)? // propagate the error
    let limit = str.parse_int(text.trim())?
    return ok(limit)
}

fn main() {
    for name in ["Ana", "Leo"] {
        print(greet(name))
    }

    let limit = load_limit("limit.txt") or 500 // the file may be missing
    let mut acct = Account { owner: "Ana" }
    acct = apply(acct, .Deposit(120)) or acct
    print("{acct.owner} has {acct.balance} (limit {limit})")
    match apply(acct, .Withdraw(200)) {
        ok(a)  => print("now {a.balance}")
        err(e) => print("error: {e}")
    }
}
```

```colibri
let total = price * qty +
            shipping                        // rule 1: line ended with `+`

let names = users
    .filter(fn(u) => u.active)              // rule 3: leading `.`
    .map(fn(u) => u.name)

draw_text(
    "score: {score}",                        // rule 2: inside ( )
    x, y
)
```

```colibri
@when(os == "windows")
@link("user32")
extern "C" { fn MessageBoxA(...) -> i32 }
```

```colibri
let x = 10        // immutable binding
let mut y = 20    // mutable binding
y += 1
const MAX_HP = 100   // compile-time constant, module level
```

```colibri
const GRAVITY = 9.81                  // compile-time constant
let config = load_defaults()          // initialised once, immutable binding
let mut score = 0                     // mutable module state
```

```colibri
let mut items: List[int] = []
items.push(1)
let n = items.len()
print(n)                              // prints 1

print(total)                          // error: `total` is used before it is initialised
let total = 10                        // `total` is initialised here, further down
```

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

```colibri
let double = fn(x: int) -> int => x * 2
let evens = nums.filter(fn(n) => n % 2 == 0)      // types inferred from `filter`
button.on_click(fn() {                             // block body: uses `return` if needed
    clicks += 1
})
```

  ```colibri
  self.button.on_click(fn() use(weak self) {
      if let some(me) = self { me.count += 1 }
  })
  ```

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
let mut b = a                  // copy
b.x = 10                       // `a` is unchanged
```

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

```colibri
impl Particle {
    fn new(pos: vec2) -> Particle => Particle { pos: pos, vel: vec2(0, 0) }   // static

    fn alive(self) -> bool => self.life > 0             // read-only receiver

    fn step(mut self, dt: float) {                      // mutating receiver (structs only)
        self.pos += self.vel * dt                       // vec2 * float: §5.1
        self.life -= dt
    }
}

let mut p = Particle.new(vec2(0, 0))
p.step(0.016)        // `p` must be `let mut`
```

```colibri
enum Shape {
    Circle(radius: float)
    Rect(w: float, h: float)
    Point
}

enum Dir { North, East, South, West }     // C-like enum, single line
```

```colibri
fn area(s: Shape) -> float {
    return match s {
        Circle(r)    => math.PI * r * r
        Rect(w, h)   => w * h
        Point        => 0
    }
}

match cmd {
    Deposit(n) if n > 1000 => print("large deposit")    // guard
    Deposit(n)             => acct.balance += n
    Withdraw(n) | Fee(n)   => acct.balance -= n         // or-patterns
    _                      => {}
}
```

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
    let mut sum = 0.0
    for s in shapes { sum += s.area() }
    return sum
}

fn print_area[T: Shape](s: T) {                 // static dispatch
    print(s.area())
}
```

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

```colibri
match load_config(path) {
    ok(cfg) => run(cfg)
    err(e) if e.is[fs.Error]() => match e.as[fs.Error]().expect("checked") {
        NotFound         => run(default_config())
        PermissionDenied => fail("can't read {path}")
        _                => fail("{e}")
    }
    err(e) => fail("{e}")
}

let text = fs.read(path).context("loading config {path}")?   // wrap with context
```

```colibri
let cfg = load_config()?                              // propagate
let cfg = load_config() or Config.default()           // fallback value
let cfg = load_config().expect("config is required")  // panic with message
match load_config() {
    ok(c)  => use(c)
    err(e) => log("bad config: {e}")
}
```

  ```colibri
  let cfg = load_config() or |e| { log("bad config: {e}"); return }

  for path in paths {
      let text = fs.read(path) or { continue }
      ...
  }
  ```

```colibri
for mut e in enemies {     // `e` is a place in the list
    e.hp -= 1
}
enemies[0].pos += vec2(1, 0)
enemies[0].step(dt)        // `mut self` method on a place
```

```colibri
for mut e in enemies {
    e.hp -= 1
    if e.hp < 10 { enemies.push(minion_at(e.pos)) }   // panics: push while `e` is live
}
```

```colibri
// shop/cart.colibri
pub struct Cart {
    pub owner: str
    items: int               // package-visible, not exported
}

pub fn new_cart(owner: str) -> Cart { ... }
```

```colibri
// main.colibri
import shop.cart                   // use as `cart.new_cart(...)`
import shop.order as ord           // rename
import json.{encode, decode}       // bring names in directly

let c = cart.new_cart("Ana")
```

```colibri
// background work
let t = task.run(fn() => parse_report("big.json"))   // runs on the runtime's worker pool
// ... keep the program responsive, polling t.done() ...
let level = t.join()?                     // Task[Result[Report]] -> Result[Report]

// channels to talk between tasks
let ch = Channel[Msg]()
task.run(fn() => worker(ch))
ch.send(.Job(42))
let reply = ch.recv()
```

```colibri
fn fetch_with_retry(url: str) -> str {
    for attempt in 1..=3 {
        let r = http.get(url, timeout: 5.0)
        if r.ok() { return r.body }
        wait(1.0 * attempt)                    // back off, resume later; other coroutines run
    }
    panic("giving up on {url}")
}

fn refresh_every(seconds: float, cache: Cache) {
    while true {                               // a periodic job
        cache.reload()
        wait(seconds)
    }
}

go refresh_every(60.0, cache)                  // detached: runs until done or cancelled
let page = go fetch_with_retry("https://example.org")   // a handle; page.join() gives the str
```

```colibri
fn upload(job: Job) {
    job.log("starting")
    wait(0.5)
    job.send_chunks(over: 1.0)                 // itself waits 1 second while sending
    wait_until(fn() => queue.is_empty())       // re-checked at each scheduler tick until true
    job.finish()
}
job.co.go(upload(job))                         // owned by the job: a CoGroup field
```

```colibri
@repr(C)
struct Vertex {
    pos: vec3
    uv: vec2
    color: [u8; 4]
}
```

```colibri
@link("raylib")
extern "C" {
    fn InitWindow(width: i32, height: i32, title: cstr)
    fn WindowShouldClose() -> bool
    fn DrawCircleV(center: vec2, radius: f32, color: Color)    // structs by value
}
```

```colibri
import rapier

let world = rapier.World.new(vec3(0, -9.81, 0))
let ball = world.add_ball(vec3(0, 10, 0), 0.5)
world.step(1.0 / 60.0)
print(world.position(ball))
```

```colibri
// Generated by colibri-runtime; do not edit.
@interface(format = 1, module = "host", hash = "9f2c41d07a3be518", caps = ["fs"])

struct Load { cpu: f32, io: f32 }
@host class Job {}
impl Job {
    fn id(self) -> int
    fn load(self) -> Load
    fn set_load(self, l: Load)
}
@positional fn submit(a: Load) -> Job
@requires(fs) fn read_save(path: str) -> Result[str]
```
