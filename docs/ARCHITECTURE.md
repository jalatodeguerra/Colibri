# Checker output (HIR) and interpreter architecture

Implementation notes for phase 1, written before the checker and interpreter exist so every
later feature (coroutines, hot reload, the bytecode VM) fits without a rewrite. DESIGN.md is the
source of truth for the language; this file is how the implementation realises it. Section
references (§) are to DESIGN.md.

## 1. HIR

HIR is one self-contained `Body` per function. Closures are lifted to their own `FnId`. Bodies
refer to each other only by id (`FnId`, `TypeId`, `GlobalId`, `ImplId`, `HostFnId`), and ids are
keyed by qualified name so they stay stable across hot reload (§16.3).

- **Locals:** flat slot indices per function frame, one slot per binding, no reuse. Each `Block`
  lists its locals in declaration order; that list is its drop list. Each local has
  `storage: Slot | Cell` (`Cell` for captured locals, see closures).
- **Temporaries:** HIR has no temp slots; it fixes the boundaries. Every statement is a temp
  scope. `match`, `if let` and `for` scrutinees live for the whole construct; `if` and `while`
  conditions die before the body. The engine keeps a temp stack (§2 below).
- **Places** are explicit, and the checker splits them at the last reference-typed step:

  ```rust
  struct Place { anchor: Anchor, path: Vec<ValueProj>, ty: Ty, display: Symbol } // display: "enemies" for guard messages
  enum Anchor {
      Local(LocalId), Global(GlobalId),
      ClassField(Box<Expr>, FieldIdx),   // Expr yields the object
      ListElem(Box<Expr>, Box<Expr>),    // list, index
      MapEntry(Box<Expr>, Box<Expr>),    // map, key
  }
  enum ValueProj { Field(FieldIdx), TupleIdx(u32) }   // struct and tuple steps only
  ```

  Collections and classes are references, so only the innermost one is the anchor and
  everything before it is an ordinary read: `a.b[i].c[j].x` anchors on `ListElem(a.b[i].c, j)`
  with path `[x]`.
- **Calls** are fully resolved; named and default arguments become positional (defaults are
  inserted at the call site). `Call { callee, type_args, args }` with
  `Callee::{Fn(FnId), Bound{iface, method}, Dyn{iface, method}, Value(expr), Host(HostFnId), Intrinsic(..)}`
  and `Arg::{Value(Expr), Place(Place)}` (`Place` for `var self` receivers). Field reads are
  `Read(Place)` or `Field(expr, idx)` on rvalues, never calls.
- **Constructors:** `Struct { ty, fields }` in declaration order with defaults filled,
  `Variant { enum, idx, args }`, `Tuple`, `ListLit`, `MapLit`.
- **Generics** keep type parameters and one uniform body. Every generic call records its
  `type_args` (which may mention the enclosing function's parameters); the interpreter passes them
  as a hidden per-frame vector. The checker marks functions that actually need them at runtime
  (`json.decode[T]`, `e.is[T]()`) so the VM can drop them elsewhere. `Bound` calls dispatch
  through `impls[(runtime type, iface)]`.
- **Implicit conversions are explicit `Coerce` nodes:** `ToDyn(ImplId)`, `F32ToFloat`,
  `IntoError(ImplId)`, `StrongToWeak`, `WeakToOption`.
- **Closures:** `Closure { fn, captures: Vec<(LocalId, ByValue | Cell | Weak)>, type_args }`. In
  phase 1 a local is a `Cell` iff it is captured, is `var`, and is assigned anywhere after its
  initialisation; otherwise it is captured by value. This is conservative and unobservable, so no
  escape analysis is needed (§6.2).
- **Integer operations carry their width** (`Add(IntTy::I32)`), so overflow traps are static.
  As built: `BinOp::Arith(ArithOp, Num::Int(IntTy) | Num::Float)`, `BinOp::Concat` and
  `BinOp::Cmp(CmpOp, Operand)`. While a literal is still untyped (`1 + x` before `x` is known)
  the checker cannot pick the operand kind, so a final pass over the body (`zonk`) resolves every
  type and re-derives each operator and literal from the resolved operand type. Engines only
  ever see resolved forms.
- **Slice 1 as built.** `Body { name, span, param_count, locals, block, ret }`: the first
  `param_count` locals are the parameters and head the function block's drop list. Module-level
  initialisers and the file's top-level statements are bodies too (`GlobalDef.init`,
  `Program.script`), so engines have exactly one way to run code. `Program` also carries
  `init_order`, the globals in dependency order. Large variants are boxed (`Read(Box<Place>)`,
  `Call(Box<Call>)`, `If`/`Block`/`Loop` blocks) so an `Expr` is 88 bytes; that matters because
  the recursive checker and evaluator pay for every `Expr` temporary on the Rust stack.
  `Callee` has `Fn` and `Intrinsic` (`Print`, `Show`, `Format(spec)`) so far; the other callees,
  anchors and coercions of this section are added by the slice that needs them.
- **Loops.** Each loop has a `LoopId` unique within its body; `Break(LoopId)` and
  `Continue(LoopId)` name their target, so labels are resolved by the checker and `Ctl::Break`
  carries a `LoopId` rather than a label. `while c { body }` is
  `Loop { If { c, body, else: break } }`.
- **Slice 2 as built (structs, methods, enums, tuples, `match`, `for`).** `Program.types` is the
  type table (`TypeDef { name, kind: Struct { fields } | Enum { variants } }`, with field
  names and types); `Ty::Adt(TypeId, args)` refers into it, so `Ty`'s `Display` prints `type#N`
  and messages use `Ty::render(&types)` instead. New expression nodes: `Struct { ty, fields }`
  (declaration order, defaults filled), `Variant { ty, variant, args }`, `Tuple`, `For`,
  `Match { scrutinee, arms }`, and `Project(expr, ValueProj)` for a field or element of a value
  that is not a place (`make().x`; it replaces the planned `Field(expr, idx)`). Anywhere an
  argument list is not in source order (named arguments, struct fields written out of order),
  the hoisting of §6.13 applies, so a struct literal's `fields` are always in declaration order
  and still evaluate in source order.
  Patterns are `Pat { kind, ty, span }` with `Wild`, `Bind(local, Option<sub>)`, `Lit`,
  `Range`, `Const(global)`, `Variant`, `Struct`, `Tuple` and `Or`. An `Arm` is
  `{ pat, guard, body: Block }`; the pattern's bindings are the first locals of `body`, bound
  before the guard runs and released when the arm ends or the guard fails. The checker proves
  exhaustiveness (Maranget usefulness, `exhaust.rs`) and warns on unreachable arms at check
  time, so evaluation can assume some arm matches. `StmtKind::LetPat { pat, init }` is
  `let (a, b) = ...`; the pattern must be irrefutable. `For(ForLoop { id, binding, start, end,
  inclusive, body })` is only the integer range form so far; `binding` is a fresh local listed in
  `body.locals`, so each iteration releases and rebinds it. `Body.mutates_self` marks `var self`
  methods: the evaluator moves (a local) or copies (a global) the receiver in, and writes the
  final `self` back on every exit, including a panic. `Operand::Structural` is the derived
  `Eq` (structs, enums, tuples) and the derived `Ord` (tuples, payload-free enums).
  Methods are ordinary functions named `Type.method` with `self` as parameter 0; the call is a
  `Callee::Fn`, with an `Arg::Place` receiver for `var self`.

Desugared by the checker:

| Source | HIR |
|---|---|
| `?` | `Match` + `Return` + `Coerce(IntoError)` |
| `or`, handler blocks | `Match`, the handler as an ordinary arm block |
| `if let`, `while let`, `while` | `Match` / `Loop` + `Break` (`while let p = e { b }` is `Loop { Match e { p => b, _ => break } }`) |
| string interpolation | `StrConcat(Vec<Expr>)`, each part a resolved `Show` call |
| end of a `Result[()]` body | explicit `ok(())` |
| `.Variant`, `T?`, method syntax | resolved forms |

Kept as nodes: `Match` with patterns (the checker proves exhaustiveness; decision trees belong to
the IR); `Assign { place, op: Option<BinOp>, rhs }` (place operands evaluate exactly once);
`For { binding, var, kind: Range | List | Map | Set | Iter, body }` (guard, fresh binding per
iteration, write-back); `Defer`, `Go`, labelled `Break`/`Continue`.

## 2. Evaluator and future coroutines

The interpreter is a **recursive evaluator**. In phase 2 each coroutine runs it on its own
stackful context (`corosensei`), so nothing is rewritten. That only works if phase 1 follows these
rules from the start:

1. **One control channel.** `type Eval<T = Value> = Result<T, Ctl>` with
   `enum Ctl { Break(Label, Value), Continue(Label), Return(Value), Panic(Box<PanicInfo>), Cancel }`.
   No Rust panic is ever part of the semantics.
2. **All Colibri state lives in a heap `Fiber`:** `frames: Vec<Frame { body: Rc<Body>, slots, type_args, cleanups }>`
   and `temps: Vec<Value>` (slice 1 frames hold `body`, `slots` and the current source `span`,
   which gives stack traces their line numbers; `type_args` and `cleanups` arrive with generics
   and `defer`). Rust locals never own a droppable Colibri value across a nested
   `eval`. Operands that are not ownership transfers go through one helper, `eval_temp`, which
   pushes onto `fiber.temps`; the statement end releases down to its watermark in reverse order.
   This gives a defined drop order and roots that the leak detector and debugger can enumerate.
3. **One `exit_scope` on every path.** `eval_block` is `let r = run(); self.exit_scope(); r`, so
   normal exit, `break`, `return`, panic and cancellation run the same defers and drops in the same
   order (§10.4).
4. **Interpreter methods take `&mut Fiber`**; there is no global "current frame".
5. **Calls look up `fns[FnId] -> Rc<Body>` at call time** and the frame holds the `Rc`. That is all
   function-level patching needs, including "a suspended coroutine keeps running the old body".
6. **A language-level call-depth limit** gives a deterministic "stack overflow" panic, so engines
   agree and the Rust stack never overflows. The CLI runs `main` on a large-stack thread.
   *As built, a second guard backs it up:* the depth limit alone cannot keep a host safe, because
   each call costs a variable amount of Rust stack (a function with deeply nested blocks costs
   far more than a flat one, and an unoptimised build far more than a release build) and the
   interpreter cannot know how big the host thread's stack is. So `eval` also compares the
   current stack position (the address of a local) with where the host called in, and panics
   with "stack overflow" past a configurable *native stack budget* (default 1 MiB, which a
   default test thread can afford; the CLI runs a 256 MB thread with a 240 MB budget). The
   checker has the same guard for deeply nested expressions. Both are safe code. The language
   limit is what defines behaviour; the budget only turns a would-be crash into the same panic.

Phase 2 then adds: `go` creates a `Fiber` plus a coroutine running `call_fn`; `wait` yields to the
scheduler; cancellation resumes the fiber with a cancel signal so `wait` returns `Err(Ctl::Cancel)`
and the ordinary error path unwinds (never a forced unwind). Each fiber counts host frames;
`wait` inside a host call or on the main fiber panics (§14.2), even though the tree-walker could
physically suspend, so it agrees with the VM.

## 3. Values

```rust
enum Value {
    Unit, Bool(bool), Int(i64) /* all integer widths */, Float(f64), F32(f32), Char(char),
    Str(Rc<str>),
    Struct(Rc<StructVal>),      // { ty, fields: Box<[Value]> }; value semantics via Rc::make_mut
    Tuple(Rc<[Value]>),         // same
    Enum(Rc<EnumVal>),          // { ty, variant, payload }; immutable, so sharing is unobservable
    List(Rc<ListObj>), Map(Rc<MapObj>), Set(Rc<SetObj>),
    Obj(Rc<ClassObj>), Weak(Weak<ClassObj>),
    Closure(Rc<ClosureObj>),    // { fn, captures, type_args }
    Dyn(Rc<DynVal>),            // { impl_id, val }
    Fn(FnId), Host(HostHandle), // HostHandle: type, index, generation
}
struct ListObj  { items: RefCell<Vec<Value>>, guard: Cell<u32> }
struct ClassObj { ty: TypeId, fields: RefCell<Box<[Value]>> }
```

Slice 1's `Value` has `Unit`, `Bool`, `Int`, `Float`, `Char` and `Str(Rc<str>)`; the other
variants join as their features land. Slice 2 adds `Struct`, `Tuple` and `Enum`.

- **Deep values.** A recursive enum can nest as deep as the program builds it, and Rust's
  recursive `Drop`, `Show` and `==` would overflow the native stack. `StructVal` and `EnumVal`
  therefore have a custom `Drop`: past a nesting depth of 256 (counted in a thread-local) it
  switches to an iterative worklist. `Show` stops at depth 1000 and prints `...`; `==` and `<`
  stop at depth 2000 and panic with "these values nest too deeply to compare".

- **Structs and tuples:** a copy is an `Rc` bump and a write uses `Rc::make_mut`, in place when
  unique. That is exact value semantics, not an accidental copy. An interface value holding a
  struct gets the lazy copy-on-write box of §9.2 the same way.
- **Borrows are confined.** Every `RefCell` borrow happens inside one `heap` module whose
  functions never call the evaluator: `get(anchor) -> Value`, `set(anchor, v) -> Value /* old */`,
  and `update(anchor, f)` where `f` runs no Colibri code. Old values are handed back and released
  after the borrow ends, because a release can run `Drop` hooks. Borrow panics are therefore
  impossible, not merely unobserved.
- **Mutation guard:** `guard: Cell<u32>` on `List`, `Map` and `Set` counts live places and
  iterations, held by a Rust RAII `GuardToken` (safe, since every exit is an `Err` return). Every
  structural operation checks `guard == 0` and otherwise panics with the place's `display` name
  (§11.6).
- **Drops are explicit.** `interp.release(v)` is called wherever an owned value dies: scope exit,
  temp end, overwritten store, container removal. It maps one-to-one onto the future IR's `dec`.
  When `v` is the last reference to a class object: run its `Drop` hook, record the trace event,
  then take the fields out and release them in declaration order. A collection releases its
  elements in index (or insertion) order. A debug-only `impl Drop for ClassObj` asserts the fields
  were already taken, catching any path that bypassed `release`.
- **`// drops:` tracing:** `Heap.trace: Option<Vec<DropEvent>>`, appended when a class object or
  host handle is destroyed, before its fields. The label is the object's `name: str` field if it
  has one, else the type name.

## 4. Places

- **`enemies[i].pos.x += 1`:** evaluate the anchor operands left to right (`enemies`, then `i`),
  then the right-hand side, then one `heap.update` that bounds-checks, walks the path with
  `make_mut` and applies the checked operation. No user code runs between read and write
  (operators are built in during phase 1), so no guard is needed and an overflow leaves the element
  untouched.
- **`p.step(dt)` on a local:** a plain slot moves its value into the callee's `self` and back on
  every exit; a `Cell` local copies in and out.
- **`enemies[i].step(dt)`:** resolve and bounds-check the anchor, take a `GuardToken`, copy the
  element in, run, and `heap.set` it back on every exit including `Err`. The guard keeps the index
  valid.
- **`obj.pos.step(dt)` (class field):** the same, without a guard. **Map entries:** guard on the
  map, write back by key.
- **`for var e in enemies`:** one `GuardToken` for the whole loop (plain `for` takes one too). Per
  iteration a fresh slot gets `items[i].clone()`, the body runs, and the value is written back on
  every exit from the iteration, skipped when `Rc::ptr_eq` shows it unchanged.

## 5. Embedding boundary

- **`Vm`** owns the checked program (HIR plus a `HostEnv`), the engine and an output writer. No
  interpreter type appears in its public API. Values cross through public `IntoColibri` /
  `FromColibri` traits that carry their Colibri type and convert via an opaque `colibri::Value`.
- **Function handles:** `Fn<A, R> { id: FnId, vm: u64, epoch: u32 }`, `Copy`, not borrowing the
  `Vm` (`vm` is a per-`Vm` id, so a handle used on another `Vm` is a `StaleHandle` error).
  `get_fn` checks `A` and `R` against the signature once; `call(&mut vm, args) -> Result<R, Error>`.
  `A` is a tuple of up to six `IntoColibri` types (trait `Args`).
  Patching keeps the id; a restart bumps the epoch, and stale handles return `Err`.
- **Host functions:** `register_fn` derives a signature from the Rust closure type and stores
  `(name, Sig)` in the `HostEnv`; registration precedes `load_file`. The checker's only source of
  host declarations is a `&HostEnv`, produced either by live registration or by parsing a `.cbi`
  file (Colibri-syntax declarations; `write_interface` prints the same `HostEnv`). In HIR a host
  call is `Callee::Host(HostFnId)`.
- **Panics:** `Ctl::Panic` unwinds through `exit_scope` to the `call` boundary and becomes
  `Err(Error::Panic { message, location, stack })` (locations are `file:line:column`, resolved
  against the loaded sources). A Rust panic in a host function is caught with
  `catch_unwind` and converted to `Ctl::Panic`; budget exhaustion takes the same path. After an
  `Err` the `Vm` stays usable, with module state intact.

Slice 1 details. A `Vm` loads one program (`load_source`/`load_file`: parse, check, run the
module initialisers and top-level statements); a second load is `Error::AlreadyLoaded` until hot
reload exists. Compile errors come back as `Error::Compile(Report)`, where a `Report` holds the
diagnostics and the `SourceMap` they point into and renders itself (`Error::render(color)`, with
`ariadne`, which therefore lives in `colibri-runtime`). If the top-level code panics, the program
is discarded, because its module state is half built. `colibri::check_source` / `check_file`
check without running. `stdout` defaults to the process's stdout; `SharedBuffer` is a clonable
in-memory writer for tests and hosts that capture output.

## 6. Semantics DESIGN.md leaves open, as implemented

These are implementation choices for details DESIGN.md doesn't pin down. Each is small; if one
turns out to matter, it moves into DESIGN.md.

1. `defer` and drops in one scope share one LIFO cleanup list in execution order; a `defer`
   registers when it executes.
2. While a place into a collection is live, reads of that collection see the old value, and any
   other write to it (including `xs[j] = v`) panics. Write-back happens on every exit, including
   unwinding.
3. `if`/`while` condition temporaries die before the body; `match`/`if let`/`for` scrutinees live
   for the construct. Arguments stay owned by the caller's statement.
4. An object is destroyed `Drop` hook first, then fields in declaration order, list elements by
   index. If the hook resurrects the object it stays alive and the hook doesn't run again.
5. A panic inside a `defer` or `Drop` during unwinding doesn't stop the remaining cleanups; the
   first panic is reported, the second attached as a note.
6. Assignment evaluates place operands, then the right-hand side, then bounds-checks and writes.
   Arguments evaluate in source order, named ones included.
7. Enum payloads and interface-value contents are not places in v1; mutate by rebuilding. Stack
   overflow is a call-depth limit, identical in every engine.
8. Host functions live in an implicit `host` prelude module; a script definition with the same
   name is an error. Module state survives a panic.
9. **Entry points (Q15).** A file may mix declarations and top-level statements. Running it is:
   module-level initialisers in dependency order, then the top-level statements in source order,
   then `fn main()` if it exists (no parameters, returns `()`; `Result[()]` comes with `Result`).
   Module-level `let`/`var`/`const` are always declarations, not statements, so they initialise
   first wherever they are written. `Vm::load_*` runs the first two phases; `Vm::run_main` the
   third (the CLI does both).
10. **Integers.** `int` is `i64`; `+ - *` and unary `-` trap on overflow ("integer overflow");
    `/` and `%` by zero panic ("division by zero", "modulo by zero"); `i64::MIN / -1` and
    `i64::MIN % -1` panic as overflow. Float arithmetic is IEEE. A negated integer literal is
    folded, so `-9223372036854775808` is a valid literal.
11. **Show and format specs.** Floats show as Rust's shortest round-trip `{:?}`: `1.5`, `2.0`,
    `-0.0`, `1e-9`, `1e16` (exponent form below `1e-4` and from `1e16`), plus `inf`, `-inf`,
    `nan`. A spec is `[[fill]align][0][width][.precision][type]` with `align` in `<>^`, `type` in
    `x X b o` (integers; negative numbers print a sign and the magnitude). On a float `.N` means
    N digits after the point (Python's bare `.N` means significant digits, which is never what
    `{dt:.2}` intends); on a `str` it truncates. The `0` flag zero-pads numbers after the sign.
    `char` and `bool` take width, fill and align like strings. No `+`, `#` or `,` flags. Specs
    are validated against the static type at compile time.
12. **Function bodies.** A `{ }` body is a statement block (DESIGN §6.1, "one rule for bodies"):
    a trailing expression is evaluated and dropped, so the function must `return`. Only `=>`
    bodies and blocks used as `if` arms yield their last expression. An `if` in statement
    position drops its branch values (they need not agree); an `if` without `else` is `()`.
    A body that can fall off its end without a `return` is an error unless every path returns.
13. **Calls.** Named and default arguments become positional at the call site. Arguments
    evaluate in source order and defaults after them in parameter order; if that differs from
    parameter order and more than one argument is not a literal, the checker evaluates them into
    hidden locals first (a wrapper block), since HIR has no temp slots. A default is re-checked
    at each call in the callee's module-level scope (it cannot see the caller's locals), with
    its diagnostics reported once at the declaration.
14. **`const`.** For now a module-level or local `const` is an immutable binding like `let`;
    "compile-time constant" is not enforced, and it initialises in dependency order like `let`.
15. **Names.** Functions and module-level values share one namespace; `print` is a built-in
    function and defining it is an error. Parameters live in an outer scope of the body, so a
    `let` that reuses a parameter's name is not a same-block shadowing warning. Unsupported
    syntax reports "`X` is not supported yet" at its span, never a crash; the declared names
    of unsupported types are remembered so uses do not cascade into "unknown type".
16. **Initialisation order.** Dependencies are collected per body (globals read, functions
    called) and ordered by depth-first search from each initialiser, so a dependency through a
    function counts. A cycle that passes through a module-level value is an error naming the
    chain (`a -> f() -> b -> a`); recursion among functions alone is fine. An unannotated
    module-level value is typed on demand when another body first uses it.
17. **Show of composites (DESIGN §7.4).** `print(x)` and `"{x}"` work for every value. At the
    top level a `str` and a `char` print bare; inside a composite they are quoted and escaped
    (the quote, `\\`, `\n`, `\r`, `\t` and `\0` as backslash escapes, other control characters as
    `\u{..}`).
    A struct is `Player { name: "Ana", hp: 100 }`, an enum variant is `Move(3)`, `Circle(radius:
    2.0)` for named payloads and `Quit` without one; a tuple is `(1, "a")`, the unit value `()`.
    A format spec on a composite is a compile error.
18. **Derived interfaces.** `==` and `!=` work on structs, enums and tuples whose parts all
    support them (structurally, recursively); `<`, `<=`, `>`, `>=` only on tuples of orderable
    parts and on enums whose variants carry no data (declaration order). Structs and enums with
    data are an error with a hint. Floats compare by IEEE, so a struct holding `nan` is not equal
    to itself.
19. **Struct literals and variants.** Fields may be written in any order; they evaluate in
    source order and are stored in declaration order. A missing field without a default, an
    unknown field and a duplicate are separate errors. `Type.Variant(args)` accepts named
    payload arguments for variants declared with names (`Rect(w: 1.0, h: 2.0)`); a variant
    without parentheses is a value only when it carries no data. `.Variant` needs an expected
    type; without one the error suggests `Type.Variant`.
20. **`var self` receivers.** The receiver must be a place rooted in a `var` local, a module
    `var` or a field/element path from one (`g.score.inc()`); a `let`, a parameter, read-only
    `self` or a temporary is an error. A local is moved into the callee and moved back, a module
    value is copied in (the callee cannot see it change halfway, and it is written back on every
    exit, so a panic still keeps the changes made before it).
21. **Patterns.** A capitalised identifier must be a variant of the scrutinee's enum or a
    module-level `const` (a local `const` is not usable in a pattern yet); a lower-case one binds.
    Float literals are an error. Ranges are compared by identity during exhaustiveness
    (`0..5` and `0..=4` do not subsume each other), so an integer or char `match` always needs a
    catch-all arm. Or-patterns must bind the same names with the same types. A variant that
    carries data needs its parentheses in a pattern (`Move(_)`), a bare name is an error.
    Struct patterns on named-payload variants are not supported yet. Exhaustiveness is skipped
    after a pattern error, so one mistake does not produce a second, misleading one. The
    analysis has a step budget (200 000); when it runs out the match is accepted without
    diagnostics rather than stalling the checker.
22. **`for`.** Only `for x in a..b` and `a..=b` over integers; each end is evaluated once, the
    binding is immutable and fresh per iteration, and a range anywhere else (or an open-ended
    one) reports "not supported yet". Methods on built-in types and generic types report the
    same.
