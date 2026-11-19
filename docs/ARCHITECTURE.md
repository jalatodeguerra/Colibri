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
      ListElem(Box<Expr>, Box<Expr>, String),  // list, index, source text of the list (guard messages)
      MapEntry(Box<Expr>, Box<Expr>, String),  // map, key, source text of the map
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

- **Slice 3 as built (classes, `Option`, `weak`, collections, places, drops, `defer`).**
  `TypeDefKind` gains `Class { fields }`, and `FieldDef.weak` marks a `weak T` field (its `ty` is
  `T`; reads give `T?`). `Program.types[0]` is the prelude `Option` enum (`OPTION_TYPE`; variants
  `none` = 0, `some` = 1, payload `Ty::Param(0)`), so `Ty::Option(T)` values, patterns, `Show`,
  `==` and exhaustiveness all use the ordinary enum machinery; user types start at index 1.
  New expression nodes: `Object { ty, fields }` (a class literal), `ListLit`, `MapLit`,
  `ToWeak` (a `T` or `T?` stored in a weak field or literal) and `FromWeak` (a weak field read as
  `T?`). An omitted weak field is `ToWeak(none)`. `Anchor::ListElem` and `MapEntry` carry the
  source text of the collection (the guard message needs it), `Anchor::ClassField` the object
  expression. `ForLoop { id, binding: ForBinding, source: ForSource, places, body }` replaces the
  range-only form: `ForSource` is `Range`, `List { list, name }`, `Map`, `Set`, `Enumerate` or
  `Chars`, and `ForBinding` is `None`, `Local` or `Pat` (a tuple pattern for `(k, v)`).
  `StmtKind::Defer(Rc<Stmt>)` registers a statement on the cleanup list. Methods and
  constructors of `str`, `List`, `Map` and `Set`, `Set(..)` and `x.clone()` are
  `Callee::Intrinsic(Intrinsic::Builtin(Builtin::..))`: the checker types them through the real
  unifier (a method signature is a table entry over the receiver's type arguments, so
  `xs.push(1)` on a `List[?T]` fixes `?T`), and the engine sees only the tag. `BinOp::Is` is
  identity; `Operand::Structural` now covers `Option` and the collections.

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
   `eval` *unless a second reference to it sits on `fiber.temps`*. Operands go through two
   helpers: `eval_temp` pushes a value and returns its index, and `eval_twin` pushes a clone and
   returns the value itself (so a Rust local can use it while the temporary stays rooted). The
   statement end releases down to its watermark in reverse order. This gives a defined drop
   order and roots that the leak detector and debugger can enumerate. *As built (slice 3):*
   because a callee leaves the caller's argument temporaries alone, the temps above an operand
   are not just that operand, so operands are addressed by index or kept as twins, never "the
   top of the stack".
3. **One `exit_scope` on every path.** `eval_block` is
   `let mark = scope_mark(); let r = run(); self.exit_scope(mark, r)`, so normal exit, `break`,
   `return`, panic and cancellation run the same defers and drops in the same order (§10.4).
   *As built:* each frame has one LIFO `cleanups: Vec<Cleanup>` (`Slot(local)` or `Defer(stmt)`).
   Binding a local pushes it and executing a `defer` pushes its statement, so drops and defers
   interleave in exactly the order they were registered (a local whose `let` never ran is never
   released). `exit_scope` pops to the scope's mark and folds a panic raised by a cleanup into
   the result (§6.5). `Block.locals` is no longer what drops locals; it stays as the declaration
   list for tools.
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
struct ListObj  { items: RefCell<Vec<Value>>, guard: Rc<Guard> }
struct ClassObj { ty: TypeId, fields: RefCell<Box<[Value]>>, released: Cell<bool> }
struct Guard    { iterations: Cell<u32>, places: Cell<u32>, place_name: RefCell<Rc<str>>, what: &'static str }
```

Slice 1's `Value` has `Unit`, `Bool`, `Int`, `Float`, `Char` and `Str(Rc<str>)`; the other
variants join as their features land. Slice 2 adds `Struct`, `Tuple` and `Enum`. Slice 3 adds
`List`, `Map`, `Set`, `Obj` and `Weak` (`Weak<ClassObj>`; a dead or empty weak reference reads as
`none`). `Value`'s `PartialEq` is a Rust-side convenience (reference types by identity); Colibri's
`==` is `values_equal`. `Map` and `Set` share one self-written insertion-ordered hash map
(`ordered.rs`: entries in a `Vec<Option<(k, v)>>` with tombstones, a `HashMap<u64, Bucket>` index
by key hash, structural key equality, compaction when tombstones outnumber live entries), so
lookup, insert and remove are O(1) and `List` push and index are O(1).

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
- **Mutation guard:** each `List`, `Map` and `Set` has an `Rc<Guard>` counting live iterations and
  live places, held by a Rust RAII `GuardToken`. The token holds the `Rc<Guard>`, not the
  collection, so dropping a token can never drop a collection (safe, since every exit is an `Err`
  return). Every structural operation and every user write to an element checks the guard first
  and, when it is held, hands its arguments back with a `Violation`; the interpreter turns that
  into a panic (message formats in §6.28). A write-back through a place is unguarded because the
  place holds the guard itself.
- **Drops are explicit.** `interp.release(v)` is called wherever an owned value dies: scope exit,
  temp end, overwritten store, container removal. It maps one-to-one onto the future IR's `dec`.
  When `v` is the last reference (`Rc::strong_count == 1`) to a class object: run its `Drop` hook
  (none exist yet: no interfaces), record the trace event, then take the fields out and release
  them in declaration order. A collection releases its elements in index (or insertion) order,
  a struct, tuple or enum its parts in order, so a death is a depth-first pre-order walk. It is
  done with an explicit worklist, so a million-node chain cannot overflow the native stack. A
  value that is not the last reference is just dropped (the count falls), which is why returning
  a binding's value, or storing it, needs no explicit "move": the slot's release only decrements.
  A debug-only `impl Drop for ClassObj` asserts the fields were already taken (`released`),
  catching any path that bypassed `release`; no test may trip it. A strong cycle never reaches
  count 1, so it is never destroyed or traced (the leak detector of §11.5 is not built yet).
- **`// drops:` tracing:** `Interp.trace: Option<Vec<String>>`, appended when a class object is
  destroyed, before its fields. The label is the object's `name: str` field if it has one, else
  the class name. `VmBuilder::trace_drops(bool)` turns it on and `Vm::take_drop_trace()` takes it
  (both `#[doc(hidden)]`). Module-level values are released after `main` returns, in reverse
  initialisation order, by `Vm::shutdown` (also run when the `Vm` is dropped, when it loads
  another program, and after a panic while loading), and that is part of the trace.

## 4. Places

- **`enemies[i].pos.x += 1`:** evaluate the anchor operands left to right (`enemies`, then `i`)
  into a `Resolved` anchor whose objects stay on the temps stack, then the right-hand side,
  then read, combine and write as separate steps; the read and the write each bounds-check, and
  the write walks the path with `make_mut`. An overflow leaves the element untouched. *As built:*
  the write is *guarded* (it panics while the collection is iterated or has a live place), even
  though no user code can run between read and write, because `xs[i] = v` during `for x in xs`
  is the same mistake as `xs.push(v)` there (§6.2).
- **`p.step(dt)` on a local:** a plain slot moves its value into the callee's `self` and back on
  every exit; a `Cell` local copies in and out.
- **`enemies[i].step(dt)`:** resolve and bounds-check the anchor, take a `GuardToken`, copy the
  element in, run, and `heap.set` it back on every exit including `Err`. The guard keeps the index
  valid.
- **`obj.pos.step(dt)` (class field):** the same, without a guard. **Map entries:** guard on the
  map, write back by key.
- **`for var e in enemies`:** one `GuardToken` for the whole loop (plain `for` takes one too). Per
  iteration a fresh slot gets `items[i].clone()`, the body runs, and the value is written back on
  every exit from the iteration, skipped when `Rc::ptr_eq` shows it unchanged. *As built:* the
  loop holds the *place* guard (`for var`) or the *iteration* guard (`for`), the final value of the
  binding is taken before the iteration's scope exits, scalars (which have no pointer to compare)
  are always written back, and the source list stays on the temps stack for the whole loop.

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
22. **`for`.** Over `a..b` / `a..=b` (integers; each end evaluated once), a `List`, a `Map`
    (`(key, value)` tuples, usually destructured: `for (k, v) in m`), a `Set`,
    `list.enumerate()` (`(index, item)`) and `s.chars()`. The binding is immutable and fresh per
    iteration; `for var x in list` makes it a place (§6.28). An open-ended range, a range outside
    a `for`, a `str` iterated directly, and any other iterable report "not supported yet" or a
    hint. Methods on generic user types report the same.
23. **`Option`.** `T?` is `Ty::Option(T)`, built in as the prelude enum of §1 (slice 3). `some(x)`
    is only valid applied; `some` alone is an error with a hint. `none` needs an expected type
    (from an annotation, a parameter, a return type, a field or the other branch); otherwise it
    is "cannot infer the type of this optional value". Matching, `if let`, `while let`, `==`,
    `!=` and `Show` (`some(3)`, `none`, `some("hi")`) work; `?` and `or` still report "not
    supported yet", and `Option` has no methods yet.
24. **Classes (DESIGN §7.2).** A class literal is a struct literal (`Node { name: "n" }`), with
    the same defaults and field rules. Assignment shares. `==` is identity, `is` is identity
    for classes and collections and an error on anything else (on `str` the hint says to use
    `==`). A class method's `self` can always change fields (the object is shared), so
    `var self` on a class is an error with that hint; a field can be assigned through a `let`
    binding. A class may refer to its own type directly (it is a reference), unlike a struct. `Show` prints like a struct, and an object that is already being printed prints as
    `Name {...}`, so a cycle terminates. A struct pattern on a class works.
25. **`weak` (DESIGN §11.5).** Only a field of a class, whose type is a class, may be `weak`
    (otherwise an error). Reading the field gives `T?` (`some` while the object lives);
    assigning accepts `T`, `T?` (`none` clears it); a literal may give `T`, `none`, or omit the
    field, which means `none` (the §7.2 example builds a `root` with no parent); a default is
    written as `T?`. A weak reference does not keep its target alive, so
    `Node { parent: Node { .. } }` is `none` at once.
26. **Collections (DESIGN §5.2, §5.3).** `[a, b]`, `{k: v}` (a repeated key keeps the first
    position and the last value), `List[T]()`, `Map[K, V]()`, `Set[T]()`, `Set(list)`. `[]` and
    `{:}` take their type from context (annotation, parameter, field, assignment, later use);
    with none the error is "cannot infer the element type of this list" with an annotation
    example, reported once per body and not at all when the body already has another error.
    List methods: `len is_empty push pop insert remove clear contains index_of first last
    reverse extend clone get`; Map: `len is_empty get contains_key insert remove keys values
    clear clone` and `m[k]`, `m[k] = v`; Set: `len is_empty add remove contains clear clone`;
    `str` has `len` (bytes) and `is_empty`. `pop`, `get`, `first`, `last`, `index_of`, and
    `Map.get`/`remove` return `Option`; `Set.remove` returns whether it was present;
    `List.remove` panics out of bounds. `keys` and `values` return new lists. `enumerate()` and
    `chars()` exist only directly as the iterable of a `for`. Methods that take a closure
    (`map`, `filter`, ...) and slicing report "not supported yet". `contains` and `index_of`
    need `==` on the element.
    Iteration order is insertion order. `==` on collections is structural (a map ignores order);
    `is` is identity. `Show` is `[1, 2]`, `{"a": 1}`, `{:}`, `Set([1, 2])`; strings are quoted
    inside.
27. **Hashability.** A key (`Map` key, `Set` element) must be hashable: `int`, `bool`, `char`,
    `str`, `()`, tuples, structs, enums and `Option` of hashable parts (structurally), and class
    objects (by identity). `List`, `Map` and `Set` are not hashable (mutable; the §5.3 hint says
    to use a tuple, a struct or its `Show` text), and **`float` is not hashable** (a deliberate
    choice the docs left open: `nan != nan` and `0.0 == -0.0` break the hash/equality contract;
    key on a scaled integer or the bits instead). A type that becomes known only by inference is
    checked after inference.
28. **Indexing, places and the guard (DESIGN §11.6).** `xs[i]` panics out of range or negative:
    `index 3 out of bounds for list of length 3`; `insert` says `insert index ..`; a missing map
    key is `key "k" not found in map` (reads, and `m[k].f += 1`; plain `m[k] = v` inserts).
    A place's operands evaluate once, left to right, then the right-hand side, then the bounds
    check. A guarded operation on a held collection panics with
    `` `enemies` modified while an element is being updated; collect the changes in a separate
    list and apply them afterwards `` (a live place; the name is the collection's source text),
    or `list modified during iteration` (also `map`, `set`). Both `for` and `for var` hold the
    collection for the loop; a `var self` call on an element holds it for the call. Element
    writes `xs[i] = v` are guarded too, so they panic during any iteration. `for var` is only
    allowed over a list for now; a `var self` receiver may be any place, including elements of a
    list held in a `let` (the list is a shared object, so `let` does not freeze its contents).
29. **`clone` (DESIGN §5.3).** `x.clone()` exists on every type and is deep. It keeps a memo from
    each original object to its copy, so a substructure shared inside the cloned graph stays
    shared and a cycle stays a cycle; objects outside the graph are not reached. Class objects
    are copied (new identity); a `weak` reference whose target was copied points at the copy,
    otherwise it is kept. `str` is immutable and shared. Recursion is bounded by the native stack
    budget and then panics with "this value nests too deeply to clone".
30. **Lifetimes and temporaries as built (DESIGN §11.2).** Bindings die in reverse declaration
    order at the end of their scope; temporaries at the end of their statement, in reverse
    creation order (the base of `make().x`, call arguments, operands of `==`). A callee gets its
    own references to the arguments and the caller keeps its own on its temps, so a temporary
    argument outlives the call it was passed to. The condition of `if` and `while` is released
    before the body; a `match`, `if let` or `for` source lives for the whole construct; a
    block's tail temporaries die when it has produced its value. An overwritten value dies at the
    store; a returned or stored value is not released because the count only falls. Parameters
    die with the callee's locals (a `var self` receiver goes back to the caller instead).
31. **`defer` (DESIGN §6.1).** `defer stmt` takes a call or an assignment (or an `if`/block
    written as an expression statement); it registers when executed and runs, with the state of
    the variables at that moment, when the enclosing block exits by any path, last in first out
    with the drops. `return`, `break` and `continue` are errors inside a `defer`. A panic in a
    deferred statement does not stop the remaining cleanups: the first panic is reported and the
    later ones are attached as notes (`PanicInfo.notes`, `Error::Panic.notes`, rendered as
    `note:` lines). When there is no earlier panic, the first deferred panic is the result.
32. **Golden harness additions.** `// drops:` is compared only when present (an empty one
    expects no destruction); several `// panic:` lines must all be contained in the message,
    and notes are appended to it as `note: ..` lines. The harness shuts the `Vm` down after
    `main` and includes the module-value drops in the trace.
