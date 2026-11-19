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
  `Callee::{Fn(FnId), Bound{iface, method}, Dyn{iface, method}, Value, Host(HostFnId), Intrinsic(..)}`
  (`Callee::Value` has no payload: the function value is `args[0]`, which keeps `Callee` `Clone`
  and `PartialEq` and lets the receiver-first evaluation order apply unchanged, slice 4b)
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
- **Closures:** `ExprKind::Closure(ClosureExpr { f, captures: Vec<Capture { outer, inner, mode }> })`
  with `mode` one of `ByValue | Cell | Weak`; the body is the lifted function `f`, and `inner` is
  the local of that body that holds the captured variable while the closure runs. In phase 1 a
  local is a `Cell` iff it is captured, is `var`, and is assigned anywhere after its
  initialisation; otherwise it is captured by value. This is conservative and unobservable, so no
  escape analysis is needed (§6.2). *As built (slice 4b):* "assigned" includes `x = ..`, `x op= ..`,
  assignments through a path (`p.x += 1`, `xs[0] = ..` where `xs` is a struct local) and `var
  self` calls on the local, and it is decided over the whole root body, closures included (a
  closure that assigns a captured variable marks the outer one, innermost first, so a chain of
  nested closures agrees). A capture takes its storage from the variable it captures; a `Weak`
  capture's inner local is always `Slot` (it holds a `T?` rebuilt on each call). `Body.locals[i].storage`
  tells the engine which locals are cells.
- **Function values:** `ExprKind::FnRef { f, type_args }` is a named function, static method or
  instantiated generic function used as a value (`Ty::Fn(params, ret)`); `ExprKind::Range`
  is a `Range[int]` value (`Ty::Range`).
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
  `==` and exhaustiveness all use the ordinary enum machinery; user types start at index 1 (index 10 since slice 4a, which reserves 1..9 for the built-in interfaces).
  New expression nodes: `Object { ty, fields }` (a class literal), `ListLit`, `MapLit`,
  `ToWeak` (a `T` or `T?` stored in a weak field or literal) and `FromWeak` (a weak field read as
  `T?`). An omitted weak field is `ToWeak(none)`. `Anchor::ListElem` and `MapEntry` carry the
  source text of the collection (the guard message needs it), `Anchor::ClassField` the object
  expression. `ForLoop { id, binding: ForBinding, source: ForSource, places, body }` replaces the
  range-only form: `ForSource` is `Range`, `List { list, name }`, `Map`, `Set` or (slice 4b)
  `Iter(expr)` (anything else iterable, as an iterator value), and `ForBinding` is `None`, `Local` or `Pat` (a tuple pattern for `(k, v)`).
  `StmtKind::Defer(Rc<Stmt>)` registers a statement on the cleanup list. Methods and
  constructors of `str`, `List`, `Map` and `Set`, `Set(..)` and `x.clone()` are
  `Callee::Intrinsic(Intrinsic::Builtin(Builtin::..))`: the checker types them through the real
  unifier (a method signature is a table entry over the receiver's type arguments, so
  `xs.push(1)` on a `List[?T]` fixes `?T`), and the engine sees only the tag. `BinOp::Is` is
  identity; `Operand::Structural` now covers `Option` and the collections.

- **Slice 4a as built (generics, interfaces, interface values, `Drop`).**
  *Types.* `Ty::Param(ParamId, name)` is a type parameter (program-wide ids; the prelude
  `Option` uses `ParamId(0)`) and `Ty::Dyn(TypeId, args)` is an interface value of
  `iface[args]`. `TypeDef.params` lists the parameters of a generic struct, enum, class or
  interface, and `TypeDefKind::Interface { methods }` holds the method names, receivers and
  defaults. `Ty::subst` instantiates a signature or a field type. The built-in interfaces are
  declared by a prelude written in Colibri (`prelude.rs`) so their `TypeId`s are constants
  (`EQ`, `ORD`, `HASH`, `SHOW`, `CLONE`, `DROP`, `ITER`, `ITERABLE`, `INTO_ERROR` = 1..9, user
  types from `FIRST_USER_TYPE` = 10; `OPTION_TYPE` stays 0).
  *Calls.* `Call.type_args` holds one resolved type per type parameter in scope of the callee
  (the `impl`'s or interface's first, then the function's own), also for method calls;
  `FnDef.type_params` says how many. They may mention the caller's own parameters. The
  interpreter does not need them (generic code runs with one uniform representation and `Bound`
  calls dispatch on the receiver's runtime type), so `Body.needs_type_args` is `false` for every
  function; it is the hook for `json.decode[T]` and friends. `Callee::Bound { iface, method }` is
  a method called through a bound or a derived implementation, `Callee::Dyn { iface, method }` a
  method called on an interface value; `method` is the index in the interface's declaration
  order. A call of a method that an `impl` provides (or an interface default) on a concrete
  receiver is an ordinary `Callee::Fn`.
  *Implementations.* `Program.impls: Vec<ImplDef>` has every `impl I for T` and every derived
  implementation that something used (as a `ToDyn` target). `ImplDef.methods` has one entry per
  interface method: the impl's function, the interface's default, or `None` for a derived
  built-in (the engine supplies it). `TypeKey` is the head of a type without arguments; the
  engine finds `impls[(TypeKey of the runtime value, iface)]` for `Bound` calls.
  *Conversions.* `ExprKind::Coerce(expr, Coercion::ToDyn(ImplId))` boxes a value of a concrete
  type as an interface value; `ToDynBound(iface)` does it for a value of a type parameter whose
  implementation is only known at run time. Both implicit coercions (arguments, `let` with an
  annotation, returns, fields, list and map literals) and `value as Interface` produce them.
  *Generic checking.* A generic body is checked once with its parameters opaque; `T`'s bounds
  are everything known about it (`param_bounds`). A call instantiates every parameter with a
  fresh inference variable (explicit `f[int](..)` unifies the variables first), and each bound
  becomes an `Obligation` decided when the body is finished, when all variables are resolved
  (`obligations.rs`). `Map` keys and `Set` elements are obligations too (`Hash + Eq`), so the
  hashability check and the bound check are one mechanism.

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
    Closure(Rc<ClosureObj>),    // { f, captures: Vec<Captured { inner, mode, value }> }
    Dyn(Rc<DynVal>),            // { impl_id, val }
    Fn(FnId, Rc<str>),          // a function used as a value; the name is for Show
    Cell(Rc<CellObj>),          // the slot content of a captured, reassigned local
    Range(Rc<RangeVal>),        // { start, end, inclusive }; boxed to keep Value at 24 bytes
    Iter(Rc<IterObj>),          // built-in iterator (collections, ranges, chars, bytes, enumerate)
    Host(HostHandle),           // HostHandle: type, index, generation
}
struct ListObj  { items: RefCell<Vec<Value>>, guard: Rc<Guard> }
struct ClassObj { ty: TypeId, fields: RefCell<Box<[Value]>>, released: Cell<bool>, hooked: Cell<bool> }
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

- **Interface values (slice 4a).** `Value::Dyn(Rc<DynVal { impl_id, val }>)` boxes any value with
  the implementation that dispatches it. A struct inside stays an `Rc<StructVal>`, so copying the
  `Dyn` is a pointer bump and a `var self` method through it changes a private copy that is
  written back as a new `Dyn` (lazy copy-on-write); a class inside is shared as always. `Show`
  and `clone` look through the box; there is no `==` on interface values (`Eq` mentions `Self`,
  so it is not object safe). `release` releases the contents.
- **Closures, cells and iterators (slice 4b).** A closure is `ClosureObj { f, captures }`; each
  capture holds a copy (`ByValue`), a shared `Value::Cell` (`Cell`) or a `Value::Weak` (`Weak`).
  A local whose `storage` is `Cell` keeps a `Value::Cell(Rc<CellObj>)` in its frame slot; `bind_local`
  makes a *fresh* cell each time it binds, which is what gives every `for` iteration, `while let`
  trip and block entry its own variable. Places resolve through the cell
  (`Resolved::Cell`), so reads, writes, `+=` and `var self` calls need no other change; a
  `var self` call on a cell copies the value in and writes it back (never moves it out, because a
  closure may still read the cell). Calling a closure installs its captures in the capture locals
  of the lifted body, which are cleaned up with the parameters (a weak capture becomes a fresh
  `T?` from `Weak::upgrade` for the length of the call). `Interp::release` handles all three new
  containers: a closure releases its captures in capture order when its last reference goes, a cell
  its content, an iterator its collection and guard, so drop order is exact (a weak capture holds
  nothing, which is the point). `Value::Iter` is the engine's own `Iter[T]`; user iterators are
  `Value::Dyn` boxes as before, and `Interp::iter_next` advances either, which is the one thing
  `for` over an `Iter` does.
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
  (slice 4a; §6.33), record the trace event, then take the fields out and release
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
10. **Integers.** `int` is `i64` and `i64` is the same type (as `float` is `f64`). Every
    integer operation traps on overflow of its own type ("integer overflow"): `+ - *`, unary
    `-`, `/` and `%` (`MIN / -1` and `MIN % -1` trap too); `/` and `%` by zero panic ("division
    by zero", "modulo by zero"). Float arithmetic is IEEE, `f32` arithmetic is done in `f32`.
    A negated integer literal is folded, so `-9223372036854775808` is a valid literal; whether
    a literal fits its final type is decided after inference (see §8).
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
    (`(key, value)` tuples, usually destructured: `for (k, v) in m`), a `Set`, a `Range` value,
    and (slice 4b) anything iterable: a type that implements `Iter[T]` or `Iterable[T]`, a type
    parameter bounded by one of them, or an iterator such as `xs.iter()`, `s.chars()`,
    `s.bytes()` or `it.enumerate()`. The binding is immutable and fresh per iteration; `for var x
    in list` makes it a place (§6.28). An open-ended range in a `for`, and a `str` iterated
    directly (hint: `s.chars()`), are errors ("cannot loop over a value of type ..").
    *Iteration protocol (slice 4b).* `Iterable.iter` returns the interface type `Iter[T]`, as
    DESIGN writes it, so an `iter()` hands back a dynamic interface value (the engine's own
    iterators are such values too, which lets them mix freely with user iterators). `for x in v`
    calls `iter()` only when `v` is not already an `Iter`. If an `Iter` is a *struct*, `for` runs
    over a private copy: the variable is unchanged afterwards and a second loop starts again
    (value semantics, §6.20); a class iterator is shared and keeps its position. A built-in
    iterator over a `List`, `Map` or `Set` holds that collection's iteration guard from creation
    until its `next` first returns `none` or the iterator dies, so `xs.push(..)` while an
    iterator over `xs` is live panics ("list modified during iteration"), exactly as inside a
    `for`; a `for` over `xs.iter()` leaves the guard when the loop ends. `enumerate()` is
    general: it works on any `Iter` or `Iterable` and yields `(int, T)`. `Range` is a value of
    type `Range[int]` (`contains len is_empty iter rev enumerate`); only `int` ranges exist, and an
    open-ended range (`1..`) is not a value.
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
    `str` has `len` (bytes), `is_empty`, `chars bytes contains starts_with ends_with split trim
    to_upper to_lower replace find` (slice 4b). `pop`, `get`, `first`, `last`, `index_of`, and
    `Map.get`/`remove` return `Option`; `Set.remove` returns whether it was present;
    `List.remove` panics out of bounds. `keys` and `values` return new lists. `contains` and
    `index_of` need `==` on the element. Slice 4b adds, on `List`: `iter map filter retain any all
    find find_index fold for_each sort sort_by sort_by_key min max sum join enumerate`, and on
    `Map`/`Set`: `iter enumerate`. `sort` is stable and needs `T: Ord`; `sort_by` takes
    `fn(a, b) -> int` (negative, zero, positive); `sort_by_key` needs an `Ord` key; `min` and `max`
    return the *first* extreme element as an `Option`; `sum` is for `int` or `float` (an empty
    list sums to `0` / `0.0`); `join` is for `List[str]`. `map`, `filter`, `retain`, `any`, `all`,
    `find`, `find_index`, `fold`, `for_each` and the sorts hold the list's *iteration guard* while
    the closure runs, so a closure that changes the list panics ("list modified during
    iteration"); nothing borrows the list's `RefCell` across user code (elements are read one at a
    time). A sort orders a snapshot (a merge sort over positions, own code), and writes it back
    only when finished: a comparator that panics leaves the list untouched, and an inconsistent
    comparator (`fn(a, b) => 1`) just gives some order, never a corrupt list. `retain` decides
    every element first and then removes, so the removed objects die together, in order, after
    the last closure call. A `nan` in a `sort`, `min` or `max` panics ("these values are not
    ordered").
    *Slicing.* `xs[a..b]`, `xs[..b]`, `xs[a..]`, `xs[..]` and the `..=` forms are a *copy* (a
    new list sharing the element objects, as every list copy does) and not a place: `xs[0..2] = ..`
    is an error. `s[a..b]` on a `str` is a *byte* range and panics when an end is out of range or
    inside a multi-byte character ("byte 2 is not on a character boundary of the string").
    Out-of-range slices say `slice 1..9 out of range for string of length 3`; a start after the
    end says `slice 4..2 starts after it ends`. `split` and `replace` with an empty separator or
    pattern panic.
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
33. **`Drop` hooks (DESIGN §11.2; slice 4a).** Only a `class` can implement `Drop` (a struct or
    enum is copied freely, so a destructor would run once per copy). When the last reference to
    an object goes away, `release` runs its `drop(self)` first, then the trace event, then the
    fields in declaration order (item 4). The hook runs on a fresh `Fiber`, so it never sees (or
    unwinds) the frames of whatever released the object; nesting is limited to 64 hooks inside
    hooks (then "Drop hooks are nested too deeply" is a panic). *Resurrection:* if the hook
    stores `self` somewhere, the object stays alive and is not traced; its hook never runs again
    (`ClassObj.hooked`), so when it dies for real only the trace event and the field releases
    happen. *Panics:* a hook cannot return an error to `release`, so a panic is parked in
    `Interp.hook_panics` and folded into the result of the enclosing statement or scope exit: if
    that result was fine, the hook's panic is the result; if it already was a panic (unwinding),
    the hook's becomes a note, `a `Drop` hook also panicked: ..` (item 5). A hook that panics
    while the program shuts down (module-level values) is discarded. Hooks run for objects that
    die inside collections, fields and interface values like any other; a strong cycle never
    dies, so its hooks never run.
34. **Method lookup (DESIGN §9.2).** `recv.name(args)` is resolved in this order: (1) an inherent
    method of the receiver's own `struct`/`enum`/`class` (an `impl Type { .. }`); (2) a method of
    a built-in type (`str`, `List`, `Map`, `Set`); (3) the methods of interfaces the receiver
    implements: for a type parameter the interfaces in its bounds, for an interface value its own
    interface, for any other type its explicit `impl`s and the derived `Eq`/`Ord`/`Hash`. So an
    inherent method *wins* over an interface method of the same name (`inherent_method_wins`
    golden); the interface one stays reachable through a bound or an interface value. Two
    different interfaces offering the same name is an error that asks for `(value as I).name()`.
    `x.clone()` and `x.show()` exist on every type (the universal `Clone` and `Show`) unless an
    explicit impl or an inherent method takes the name.
35. **Built-in interfaces and their method signatures** (declared in `prelude.rs`):
    `Eq { fn eq(self, other: Self) -> bool }`, `Ord { fn cmp(self, other: Self) -> int }`
    (negative, zero or positive; there is no `Ordering` enum yet), `Hash { fn hash(self) -> int }`,
    `Show { fn show(self) -> str }`, `Clone { fn clone(self) -> Self }`, `Drop { fn drop(self) }`,
    `Iter[T] { fn next(var self) -> T? }`, `Iterable[T] { fn iter(self) -> Iter[T] }` and
    `IntoError { fn message(self) -> str => "{self}" }`. `Iter`, `Iterable` and `IntoError` are
    declared but nothing uses them yet (`for` over a user iterator and `?` come later). `Ord`
    implies `Eq` for bounds (`T: Ord` may use `==`). An `impl` writes the interface's methods
    with the type substituted for `Self` (`fn eq(self, other: P) -> bool`); a signature that
    differs is reported as expected versus found.
36. **Derived implementations (DESIGN §7.4) are real.** The bound checker asks the same question
    as `==`, `<` and the `Map`/`Set` key check: `Env::implements(type, interface, args)`, which
    answers "an explicit impl", "derived" or names the part of the type that fails
    (`Pair[int, List[int]]` is not `Hash` because `List[int]` is not hashable). `Eq` is derived
    for every type made of derivable parts (a class by identity); `Ord` for scalars, tuples and
    enums without data; `Hash` for everything but `float`, `List`, `Map` and `Set`; `Show` and
    `Clone` for every type. An explicit `impl` *replaces* the derived one for that type
    everywhere: `==`/`!=`, `<` and friends (also inside tuples, lists and other structs),
    `contains`/`index_of`, `Map` and `Set` lookups, `print` and interpolation (also nested) and
    `.clone()`. The engine consults the user's method through the `Overrides` hook of the
    structural functions (`hooks.rs`), and takes the plain fast path when the program has no
    such impl. *Map keys with a user `Eq`:* a lookup scans the stored keys with the user's `eq`
    (O(n)) and then uses the matching stored key; a user `Hash` is accepted and type-checked but
    not consulted, since a hash is not observable (iteration is insertion order). The checker
    requires `Hash + Eq` of every key regardless.
37. **Implementations are unique per head type.** One type has at most one `impl` of an
    interface, whatever the type arguments: `impl Container[int] for Bag` and
    `impl Container[str] for Bag` conflict, and so do `impl Describe for List[int]` and
    `... for List[str]`. This is what lets the engine dispatch on the runtime type alone (values
    carry no type arguments). The same holds for inherent methods: they are keyed by the type's
    name, so `impl Tree[int] { fn f }` and `impl Tree[str] { fn f }` conflict.
38. **Orphan rule.** `impl BuiltinInterface for BuiltinType` (`impl Show for int`) is an error;
    every other combination is allowed, because all user code is one package for now (the package
    boundary comes with imports). `impl Drop` also requires a class.
39. **Object safety (DESIGN §9.2).** A method cannot be called on an interface value if it has
    type parameters of its own or mentions `Self` outside its receiver (`other: Self`, `-> Self`).
    The interface itself is still legal and usable as a bound; the error is reported at the call
    on the interface value, with the reason, not at the interface declaration. Methods of
    built-in `Eq`/`Ord`/`Clone` are therefore only reachable through bounds and concrete types.
    Interface methods need a `self` receiver (no static functions in interfaces yet), and a
    type parameter has no static functions either (`T.default()` is "a type parameter, not a
    value").
40. **Receivers.** An interface method declared `var self` may be implemented with plain `self`
    by a class (a class's `self` can always change its object); a struct must match exactly. A
    `var self` method called through an interface value changes a private copy of the boxed
    struct and writes a new box back (item "interface values" in §3).
41. **Implicit conversion sites.** A value is converted to an interface value wherever the
    expected type is an interface: arguments, `let x: I = ..`, assignment, `return`, struct and
    enum payload fields, list, map and set literal elements, `some(..)`. `value as I` is the
    explicit form (any other `as` is an error; numbers convert with `u8(x)`, §8). An interface value never converts to
    another interface, and `==` on interface values is an error. Generic code may be
    instantiated with an interface type (`total_area[Shape]`): its `Bound` calls dispatch
    through the box.
42. **Inference of type arguments (DESIGN §5.4).** Unification is local to one body and has no
    defaults except the integer/float literal rule: `max(1, 2.5)` is a mismatch, `id(1)` is
    `id[int]`. A type argument that is still unknown at the end of the body is "cannot infer
    `T`; write `f[int](...)`" at the call (or the literal `Box { }`, `Tree.Leaf`) that created
    it. `f[int](x)` and `Type[int].f(x)` give arguments explicitly; `x[i]` versus `f[T]` is
    decided by the base: a generic function (or method name after a dot) means instantiation,
    anything else is an index (DESIGN §3.5). Bounds on a *type's* own parameters
    (`struct Box[T: Ord]`) are not supported yet: write them on the `impl` blocks and functions.
43. **Recursive generic types** (`enum Tree[T] { Leaf, Node(Tree[T], T, Tree[T]) }`) need no
    boxing in the interpreter: every composite is already behind an `Rc`. The native-code engine
    will box the recursive position; nothing in the HIR depends on it.
44. **Function types and values (slice 4b; DESIGN §5.2, §6.2).** `fn(A, B) -> R` and `fn(A)`
    (result `()`) are types, `Ty::Fn(params, ret)`. A named function, a static method
    (`Particle.new`) and a generic function (`max[int]`, or `max` where the expected type decides
    `T`) are values; they show as `<fn name>`, a closure as `<closure>`, and a function value has
    no `==` (error with a hint). A function value takes *all* its parameters: defaults are not
    part of the type and are not filled in, and named arguments are an error. A method that
    takes `self` is not a value (`P.get` is an error with the closure to write instead): the
    receiver would need a binding rule DESIGN does not give. `callee(args)` where `callee` is not
    a function name is a call of a value (`Callee::Value`, the function is `args[0]`).
    `recv.name(args)` tries a *method* `name` first (inherent, then interface, §6.34) and only
    when the receiver's type has no method of that name does it look for a *field* `name` of
    function type, so `self.on_hit(dmg)` calls the field and a method can never be hidden by a
    field. To call a field whose name is also a method, bind it first (`let f = t.run`).
45. **Closures (slice 4b; DESIGN §6.2).** `fn(x: int) -> int => x * 2`; a `{ }` body is a
    statement block and needs `return` for a value (§6.12); `fn(var x: int)` makes a mutable
    parameter. Parameter and result types come from the *expected* function type (an annotation,
    a parameter of the callee, a field, a return type), also through a generic callee: the
    checker unifies a generic call's arguments left to right, but checks the *closure arguments
    last* so every other argument has already fixed type parameters (`apply(21, fn(n) => n * 2)`,
    `xs.fold(0, fn(acc, x) => ..)`, `compose(f, fn(b) => ..)`). With no expected type an
    unannotated parameter is an error asking for a type. A written type wins over the expected
    one and a mismatch is reported where the closure is used. A `=>` body must have exactly the
    result type; a block body that can fall off the end with a non-`()` result is an error.
    Closures capture by reference (§1): see the `Cell` rule. A `var` struct captured by a closure
    that assigns it (or any part of it) lives in a cell, so the closure and the enclosing function
    share it; a closure that only reads it gets a copy (nothing can observe the difference).
    `for` and `while let` bind a fresh variable per iteration, `let`/`var` inside a loop body a
    fresh one per trip, but a `var` declared *before* a `while` is one variable (the classic
    `while` closure sees its final value; the test `captured_loop_counter_is_one_variable` pins
    it). Closures can capture `self` (a class's or a struct method's) and each other; a closure
    cannot refer to itself by name (there is no binding yet when its body is checked).
46. **Weak captures (DESIGN §6.2).** `fn() use(weak self) { .. }` and `use(weak a, weak b)`: the
    listed names must be locals (or `self`) of class type, are captured as `Weak` and have type
    `T?` inside; every other capture stays implicit. Naming a non-class or an unknown name is an
    error. The closure holds no strong reference, so a button that stores a closure over its own
    panel does not keep the panel alive and the leak that DESIGN §6.2 describes does not
    happen (test `weak_self_button_example`, against `strong_self_capture_leaks_a_cycle`).
47. **Generic inference through bounds.** A type parameter that appears only in another
    parameter's *bound* (`fn collect[T, I: Iter[T]](it: I)`) is not inferred from that bound:
    write `[int, Evens]` or use `I: Iter[int]`. (Unchanged from §6.42; it surfaces more now that
    `Iter[T]` is common.)

## 7. Slice 5 as built: `Result`, `Error`, `?` and `or`

- **Type table.** `Option`=0, `Result`=1 (generic enum, `Ty::Adt(RESULT_TYPE, [T, E])`; the old
  `Ty::Result` is gone), interfaces `Eq`..`Iterable`=2..9, `IntoError`=10, `Error`=11 (prelude
  `class Error {}`), first user type 12. `ok`=variant 0, `err`=1. `Result[T]` is `Result[T, Error]`
  and prints that way.
- **Error value.** A `ClassObj` of `Error` with fields `[value, impl_id, context, cause]`. A root
  keeps the original typed value (so `as[T]` returns it intact); `context` makes a wrapper
  (`value=()`, `cause`=inner). `is`/`as` look at the root. `{e}` is `outer: inner: root`;
  `message()` is the outermost layer only; `cause()` the wrapped error or `none`. The root message is
  the user `IntoError.message`, else the `Show` of the value. `str` and `Error` implement
  `IntoError` by derivation. Error objects are not in the drop trace; the kept payload is.
- **HIR.** `Coercion::IntoError(ErrorConv)` / `ResultIntoError(ErrorConv)` (`Impl(id)` when the
  type is known, `Bound` when it is a type parameter resolved at run time);
  `Intrinsic::{Panic, ErrorIs(TypeKey), ErrorAs(TypeKey)}`; `Builtin` variants for the Option,
  Result and Error methods. `e?` and `a or b` lower to `Match` + `Return`; handler blocks are
  checked inline in the enclosing function so `return`/`break`/`continue` act on it.
- **Runtime.** `Vm::run_main` returns `Error::MainReturnedErr { message }` when `main` returns
  `err`; the CLI prints `error: ..` and exits 1. Harness directive `// main-err:`.
- **Decisions beyond DESIGN.** (1) Explicit conversion is `Error.from(x)`; there is no
  `.into_error()`. (2) `err(x)` where `Error` is expected converts `x`, so `err("msg")` works.
  (3) `?` converts errors only to `Error`. (4) `or` first tries a plain `T` fallback, then a
  `Result`/`Option` of `T`. (5) `is[T]`/`as[T]` compare a static type key, so generic arguments are
  not distinguished and a type-parameter `T` is rejected. (6) Backtraces on errors are not
  recorded (skipped). (7) `unwrap`, `unwrap_or`, `map_err`, `ok_or` are not provided. (8)
  `panic(msg) -> Never` is a built-in added here (not in DESIGN); the name is reserved. (9) User
  types named `Error`/`Result` are rejected; `Error {}` cannot be built. (10) The unused-`Result`
  warning is skipped inside `defer`. (11) A bare `return` in a `Result[()]` function is still an
  error; falling off the end returns `ok(())`. (12) `main` may return `Result[()]` or
  `Result[(), E]`. (13) A top-level `let _ = ..` is parsed as a script statement.

## 8. Slice 6a as built: the numeric tower

- **Types.** `Ty::Int(IntTy)` for `i8 i16 i32 i64 u8 u16 u32 u64` (`int` and `i64` are one
  type, `IntTy::I64`), `Ty::Float` (`float` = `f64`) and the new `Ty::F32`. The names are
  resolved by `Ty::numeric_by_name`; a local or module value of the same name shadows the
  conversion call and the constants.
- **Literals.** `Lit::Int(i64)`, `Lit::U64(u64)` (only above `i64::MAX`, or a `u64` literal
  after zonking), `Lit::Float(f64)` and `Lit::F32(f32)`. The type of an integer literal is an
  `IntLit` inference variable that may become any number type, a float literal's a `FloatLit`
  variable that may become `float` or `f32`. Zonking fixes the literal's final form and reports
  "literal 256 does not fit in `u8`" (also for patterns and for the folded negative literals).
  A float literal reaches an `f32` through `as f32` of the parsed `f64`, which can differ from
  parsing the text as `f32` in the last bit for exotic spellings.
- **Operators.** `BinOp::Arith(ArithOp, Num)` with `Num::{Int(IntTy), F32, Float}`,
  `BinOp::Bit(BitOp, IntTy)` (the type is the left operand's; the count of a shift may be any
  integer), `UnOp::Neg(Num)` (unsigned types are rejected by the checker), `UnOp::BitNot(IntTy)`
  and `Operand::{Int(IntTy), F32, Float, ..}` for comparisons. The checker puts `int`
  placeholders in while a literal is unresolved; `zonk` re-derives each operator from the
  resolved operand type.
- **`f32` to `float`.** The only implicit conversion is `Coercion::F32ToFloat`. It is inserted
  where a `float` is expected (`coerce_to_expected`, which every checked expression with an
  expected type goes through) and in a binary operator or comparison whose operands are an
  `f32` and a `float` (`unify_operands`). An arithmetic expression is not given a `float` hint,
  so `let x: float = a * b` with `f32` operands multiplies in `f32` and widens the product.
- **Values.** `Value::Int(i64)` holds every integer type except `u64`, which is
  `Value::U64(u64)`; `Value::F32(f32)` is single precision. The operator names the width, so
  `colibri-interp/src/num.rs` widens both operands to `i128`, computes exactly and checks the
  result against the type's range. A `u64` therefore needs no bit-pattern tricks, `Show`,
  comparison, hashing and sorting of a `List[u64]` work on the value alone, and the cost is one
  extra variant. The `int` (`I64`) fast path in `eval.rs` is kept. `TypeKey::Int(IntTy)` and
  `TypeKey::F32` exist for interface dispatch; since a `Value::Int` carries no width, a runtime
  lookup on one finds the `int` implementation, so `impl I for i8|i16|i32|u8|u16|u32` is
  reported as not supported yet (an interface value built by a static `ToDyn` is unaffected, and
  `u64`, `f32` and `int` work everywhere).
- **Intrinsics.** `Intrinsic::Convert { from, to }` for `u8(x)` and friends (panics: "value 300
  does not fit in `u8`", "cannot convert `nan` to `int`") and `Intrinsic::NumMethod(NumMethod,
  Num)` for the methods. `T.MAX`, `T.MIN` and the float constants are literals folded by the
  checker. `pow` takes an `int` exponent (a negative one panics); `round` is half away from
  zero; float `min`/`max` ignore a `nan`; `abs` of a signed `MIN` traps; `checked_*` return
  `T?`.
- **Shifts.** `x << n` and `x >> n` panic unless `0 <= n < bits` ("shift by 64 is out of range
  for `int`"); `<<` drops bits without trapping (two's complement, so `1 << 63` is `int.MIN`),
  `>>` is arithmetic on signed and logical on unsigned types.
- **Ranges.** A `for` loop accepts a range of any integer type except `u64` (the loop variable
  has that type; the values are `Value::Int`); `Range[int]` values and slices stay `int`.
  `sum` accepts `int` and `float` lists only. `Map`/`Set` keys of any integer type work;
  floats (both) are not hashable.
- **Embedding.** `IntoColibri`/`FromColibri` exist for `i8 i16 i32 i64 u8 u16 u32 u64 f32 f64`;
  `get_fn` compares the Rust signature with the script's (`i64` is `int`, `f64` is `float`).
  Superseded by section 9, which describes the context-taking traits and `TypeDesc`.

## 9. Slice 7a as built: the embedding API, host functions

Part A of PHASE1 item 8: everything except host classes (`#[colibri::export]`, `Handle<T>`,
`Ref<T>`, the host heap), which is part B. `TypeDesc::Host(name)` and `.cbi` `@host class` /
body-less `impl` methods already parse, so B only has to add the runtime half.

**Host input to the checker is text.** `register_fn` derives a signature (`SigDesc`) from the Rust
closure type; the `Vm` prints all signatures as canonical `.cbi` text and re-parses that text
into a `colibri_check::HostEnv`. The checker never sees a Rust type. A `.cbi` file read by
`colibri check --interface` takes the same path (`HostEnv::from_text`), so the CLI and a live
`Vm` check identically. `register_fn` validates by that print-and-parse round trip and rolls
the registration back on failure (name clash, unparsable type). The `HostEnv` items are declared
after the prelude and before user items, so host `TypeId`s are `FIRST_USER_TYPE + index`.
Host fns become `FnInfo`s without a body (`host: Some(HostFnInfo)`), calls lower to
`Callee::Host(HostFnId)`, and `Program.fns` holds an empty stub `Body` per host fn so that
traces, depth and fn ids need no special case. `Program.host_fns` lists `name`, an
`f: HostFnId` slot and `requires`. A host fn used as a value is a checker error ("wrap it in a
closure"); a script item with a host name is "`x` is a host function and cannot be redefined".
Diagnostics whose span is in the host text (`HOST_FILE`) are moved to the start of the user file
with the prefix "in the host interface:"; labels in prelude or host text are dropped.

**Interface text.** `// Generated ...` comment, then `@interface(format = 1, module = "host",
hash = "...", caps = [...])`, then one item per line. The hash is FNV-1a 64 over everything after
the header line, so a hand edit is detected when reading (`Error::Interface`) and a stale file
when verifying (`Vm::verify_interface` -> `Error::InterfaceMismatch`). `write_interface`
writes only when the text changed and returns whether it did. Only the `host` module exists;
several files merge into it. The header line is parsed by hand and blanked (same length, so
spans stay right) before the body goes to `parse_interface_file`, which allows body-less `fn`
and methods. Attributes: `@requires(cap)`, `@positional` and `@host`.

**Runtime ABI.** `HostFn<Marker>` is implemented for closures of arity 0..=8, with or without a
leading `&mut Ctx`, and erased to the non-generic `RawHostFn(&mut Ctx, &[Value]) -> Result<Value,
HostFault>` indexed by `HostFnId`. The interpreter trampoline pushes a pseudo frame (the stub
body) so traces name the host fn, bumps `fiber.host_depth`, converts `Vec<Value>` to the opaque
`colibri::Value` borrowing the arguments, wraps the call in `catch_unwind(AssertUnwindSafe)` and
releases the arguments afterwards. A Rust panic becomes a Colibri panic whose message is
``host function `name` panicked: ..``. `Ctx` is `{interp, fiber}`; `Caller` is implemented for
`Vm` and `Ctx`, so `Fn::call` and `Callback::call` work from inside a host fn and run the
script on the same fiber. Host state is a `TypeId -> Box<dyn Any>` map in the interpreter.

**Capabilities.** `Cap` is `#[non_exhaustive]`; `CapSet` a bit set. The host grants with
`VmBuilder::allow`; `Registration::requires` tags a function. The checker reports an ungranted
call at the use site ("`read_save` needs the `fs` capability, which this host did not grant")
and `extern` blocks without `ffi` are an error. An ungranted function is never installed: its
slot stays a trap, which cannot be reached because the checker refused every call.

**Instruction budget.** Interp `budget.rs`. `steps_left: i64` is decremented with one branch per
statement, call (script and host), loop iteration, and per element in built-ins that walk a
collection or string (`work_of`). Each outermost entry from the host gets a fresh allotment
(`begin_entry`/`end_entry`); a reentrant entry shares it. Exhaustion unwinds like a panic
(`PanicInfo.budget`) and wins over an ordinary panic when both happen (`merge_failure`). It
grants 10,000 grace steps so `defer` and `Drop` hooks can run; after the grace, defers and hooks
are skipped, though plain releases still happen, so the heap stays consistent. The result is
`Error::BudgetExhausted { location, stack }`; the `Vm` stays usable. A script that swallows the
unwinding with `or` still ends in the error at the outermost entry. Release and shutdown entries
do not record the budget outcome.

**Callbacks.** The interpreter keeps a root slab (generation + epoch handles). `Vm::find_fn`
returns a `Callback<A, R>` that roots the function value; dropping it pushes the handle on a
drop queue (`Rc<Cell<Vec<..>>>`) that is drained at the start of every outermost entry, so no
release happens at an arbitrary point. After `shutdown` or a reload the epoch moved, so a
callback returns `Err(StaleHandle)`.

Decisions the design documents did not fix:

- `@positional` marks registered functions whose parameter names are placeholders (`a`, `b`,
  `c`, ...), so a named-argument call is rejected ("`x` takes positional arguments only").
- `register_fn` returns `Result<Registration, Error>`; conflicts are immediate errors. After
  load it is `Error::AlreadyLoaded`.
- Host fn parameters cannot be `&str` (no `unsafe` means no lifetime-erased borrow): use
  `String`. `&[T]` is accepted for `get_fn`-style arguments only.
- `HashMap` converts out only, sorted by key (`K: Ord`) so output is deterministic; `BTreeMap`
  both ways. `usize`/`isize` are range-checked in and panic out above `i64::MAX`.
- `Option[T]` prints as `Option[T]` in `.cbi` text.
- `Vm::shutdown` unloads the program and bumps the epoch; the `Vm` can load again.
- A panic raised inside a reentrant call shows the whole active stack including the host frame.
- Host classes in `.cbi` produce a checker "not supported yet" error until part B.
- `colibri_lang::vm_builder()` (used by goldens) grants nothing; the CLI `run` grants all and
  `check` grants all.
