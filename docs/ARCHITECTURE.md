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

*As built (§16):* `go` creates a `Fiber` plus a `corosensei` coroutine that runs the call; `wait`
yields to the scheduler; cancellation resumes the fiber with a cancel signal so `wait` returns
`Err(Ctl::Cancel)` and the ordinary error path unwinds (never a forced unwind). Each fiber counts
host frames; `wait` inside a host call or on the main fiber panics (§14.2), even though the
tree-walker could physically suspend, so it agrees with the VM. Each coroutine has its own
`stack_base` and native stack budget.

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
module initialisers and top-level statements); a second load is `Error::AlreadyLoaded`; the program is replaced by
hot reload (§17) instead. Compile errors come back as `Error::Compile(Report)`, where a `Report` holds the
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
- Host classes in `.cbi` produced a checker "not supported yet" error until part B (§10).
- `colibri_lang::vm_builder()` (used by goldens) grants nothing; the CLI `run` grants all and
  `check` grants all.

## 10. Slice 7b as built: exported host types, generational handles, `colibri-bind`

Part B of PHASE1 item 8: host classes. The checker, the interpreter, the runtime facade and a new
proc-macro crate each got a piece; the interface text from §9 stays the only thing the checker
sees.

**Checker.** A `@host class X {}` in the interface text becomes `TypeDefKind::HostClass` (no
fields; `TypeDef::is_class()` is true for it, so identity `==`/`is`/`Hash` and `Show` come with
the class rules, and `is_host_class()` singles it out). Host types are declared right after the
prelude, so their ids are `FIRST_USER_TYPE + index` in every program using the same interface
(part A). `TypeDef.sendable` is `false` for host classes and `true` otherwise; nothing reads it
yet (DESIGN §14.1 `task.run`). `impl X { .. }` blocks of the interface go through the ordinary
`collect_impl` while `collecting_host` is set, so each method is an inherent method
(`Checker.methods`) whose `FnInfo.host` is set by `add_fn` (functions and methods alike take their
`HostFnId` there, in text order). A method call lowers to `Callee::Host(id)` with the receiver as
`args[0]` (`call_fn_method`); `X.new(..)` goes through the existing static path. `&self` and
`&mut self` are both plain `self` in the text (`validate` refuses `var self`). Names are
`Class.method` (`HostFnDef.name`, the stub body, stack traces). `alive() -> bool` is an
`Intrinsic::HostAlive` on every host class (an exported method named `alive` is refused, in the
runtime and in `validate`). Compile errors with their own text: `.clone()` ("is a host class and
cannot be cloned"; `Clone` is not derived either, so a `T: Clone` bound refuses it), a struct
literal, field access, a struct pattern, `impl Drop`, a `weak` field or weak capture. A script may
add methods of its own to a host class with an ordinary `impl`.

**Interpreter.** `hostheap.rs` holds the slab: `Slot { generation, live, owner, any, name, cell }`
with `owner = Script(Weak<HostRef>) | Host` and `cell = Rc<HostCell { flag: Cell<isize>, holder,
obj: Cell<Option<Rc<dyn Any>>> }>`. A script holds `Value::Host(Rc<HostRef { ty, index,
generation }>)`; two values are equal when index and generation are. Borrowing is an explicit
flag, never a `RefCell` panic: `0` free, `n` shared guards, `-1` exclusive. An exclusive guard
*takes the object out of its cell* and puts it back (and resets the flag) when dropped; a shared
guard clones the `Rc`. `Option<Box<dyn Any>>` from EMBEDDING §3 became `Cell<Option<Rc<dyn
Any>>>` because two nested shared borrows need two handles to one object and a guard cannot hold
a reference into the slab while the `Ctx` is used. The cell records the host function that holds
the borrow (the `Rc<Body>` of the top frame, so no allocation), which is the `World.step` in the
conflict message. The host-owned objects need no script reference count; script-owned ones are
freed by `release`: `dismantle` frees the slot when the last `Rc<HostRef>` goes (`strong_count ==
1`) and the slot is script-owned, traces the type name (the `// drops:` and `trace_drops` label is
the class name, after the class hook and the fields of whatever held it, as for any object), and
runs the Rust `Drop` under `catch_unwind`; a panic there is reported like a panic of a `Drop`
hook (`dropping the host object `X` panicked: ..`). `Interp::drop` shuts the script down (roots,
then module values in reverse initialisation order, so hooks may still call host methods) and
then `drop_host_heap` drops what is left in index order; `Vm::shutdown` leaves the heap alone
because the host's own objects outlive a program. The hot path is unchanged: `is_container` and
`dismantle` gained one arm.

**Runtime.** `HostType { const NAME; fn describe(&mut TypeBuilder<Self>) }`. `TypeBuilder`
has `static_fn`, `method` (`&T`) and `method_mut` (`&mut T`); each takes the name, an array with
one name per parameter, and a closure. The closure forms are `HostFn` for static functions and
`HostMethod`/`HostMethodMut` for methods, all with an optional leading `&mut Ctx` (before the
receiver). The raw function a method becomes borrows the receiver (shared or exclusive) for the
call only, then converts the other arguments, calls, drops the guard and converts the result. A
Rust panic unwinds through the guard, so the flag is restored. `TypeRegistry::define_host_type`
declares the class (before describing its methods, so classes may mention each other and
themselves) and stores the methods; the printed text is `@host class X {}` and, directly after,
`impl X { .. }`. A type mentioned by a registered signature (`T` by value, `Handle<T>`,
`Ref<T>`) registers itself the same way. `register_type` and `register_fn` validate by printing
and parsing, as before, and roll back on failure.

- `Handle<T> { index, generation, vm }`: `Copy`, `!Send`/`!Sync` (a raw-pointer `PhantomData`).
  The field is `generation` (`gen` is reserved in edition 2024) and `vm` is the `u64` id of the
  `Vm`. `FromColibri`/`IntoColibri` convert it; `IntoColibri for Option<T>` asks the hidden
  `IntoColibri::is_live` so a stale `Handle` becomes `none`. A handle of another `Vm` is
  `Error::StaleHandle` on the host side and a panic if returned to a script.
- `Ref<T>` (`FromColibri`, shared guard) and `RefMut<T>` (host side only): `Deref`/`DerefMut`
  over the guard. `Vm`/`Ctx` have `insert`, `remove`, `get`, `get_mut`, `is_alive`.
  `remove` is `Result<Option<T>, Error>`: `Ok(None)` for a stale handle, `Err(Error::HostObject)`
  when borrowed ("cannot remove the host object `X`: it is in use by an enclosing call
  (`X.step`)") or script-owned. `Error::HostObject(message)` is also what `get` returns for a
  destroyed or busy object; the message is the text a script would see.
- `impl<T: HostType> IntoColibri for T` (by value: script-owned). There is no `FromColibri for
  T`: a host object is borrowed (`Ref<T>`) or named (`Handle<T>`), never moved out of a script.
- Panics with exact text travel as `HostFault::Message` (the trampoline does not prefix it):
  `` `Enemy` was destroyed by the host `` and ``host object `World` is already in use by an
  enclosing call (`World.step`)``. A call through the host side with nothing running says
  ``host object `World` is already borrowed by the host``.

**`colibri-bind`.** A proc-macro crate (`syn`/`quote`/`proc-macro2`), re-exported as
`colibri::export`. The expansion is plain `proc_macro2` in `expand.rs` and unit-tested on token
text; every path goes through `colibri_runtime::__private` (the default; `crate = path` renames
it). `export(value)` on a struct: all fields `pub`, no generics, named fields, a static `Copy`
assertion spanned at the type, `IntoColibri`/`FromColibri` through `define_struct` and
`Cx::new_struct`; on a payload-free enum without explicit numbers: `define_enum` and
`new_variant`. `export` on a struct writes `impl HostType` whose `describe` calls
`ExportedMethods::describe_methods`, which `#[export] impl X` writes (only `pub fn`s, `&self`,
`&mut self` or no receiver, no generics/async/unsafe/by-value `self`, plain parameter names;
a first `&mut Ctx` after the receiver is passed through and not named). A struct with no
exported impl says so through `#[diagnostic::on_unimplemented]`; `export(no_methods)` is the
explicit form. `#[export] fn f(..)` keeps the function and writes `mod f { pub fn register(vm) }`
(a module and a function share a name because they live in different namespaces), which calls
`Vm::register_fn_named` with the real parameter names, so scripts may name arguments. Errors are
`compile_error!` at the offending token; the item is emitted next to them.

Decisions the design documents did not fix:

- `Vm::register_fn_named(name, [..], f)` is public: closures keep `a, b, c` and `@positional`,
  and a named registration is what the macro needs.
- Methods and static functions of a class cannot require a capability yet (the interface text
  allows `@requires` on members and the checker enforces it; the registration side has no knob).
- `vm.insert` does not register the type; `register_type::<T>()` (or any signature that mentions
  it) must come first, or converting the handle panics with a message that says so.
- A `Handle<T>` to a script-owned object is allowed (the host may look at it with `get`), but
  `remove` refuses: the script owns its lifetime.
- Cloning a collection or a struct that holds host objects shares them (one object, many
  references); only a direct `.clone()` on the class is an error.
- Show is `<World #12>`, the number being the slab index; indexes are reused after a free, the
  generation is not shown.
- `Callee::Host` methods do not appear in `// drops:` goldens: the golden harness has no host,
  so the exact messages and the drop order are tested in `crates/colibri-runtime/tests`.
- A Rust `Drop` that panics is a Colibri panic at the next statement boundary (hook panics).

## 11. Slice 8 as built: packages and `import`

PHASE1 item 9, first half: DESIGN §13 (modules and packages) and §4.2 (initialisation order).
`colibri test`, `colibri fmt`, the language server and the standard library are later PRs; `std`
only has room reserved (see "Name resolution of an import").

### 11.1 The module tree

**One file is one module, one folder is one package.** Every import path is rooted at the *tree
root*, never relative to the importing file, so a file means the same thing from anywhere:

| Run as | Tree root | Entry |
|--------|-----------|-------|
| `colibri run script.colibri` | the script's folder | `script.colibri` |
| `colibri run` / `colibri run dir` | the folder with `colibri.toml` | `main.colibri` |
| `Vm::load_file(path)` | the file's folder | the file |
| `Vm::load_package(dir)` | `dir` | `dir/main.colibri` |
| `Vm::load_sources(&[(path, text)])` | the folder of the first path | the first file |
| `Vm::load_source(name, text)` | none: one file, no siblings | the source |

A *tree* is a root folder, a name and its dependencies (the entries of `[dependencies]` in its
`colibri.toml`, if there is one; `run script.colibri` reads a `colibri.toml` next to the script
only for its dependencies and name). A dependency is a tree of its own, with its own root.

`a/b/c.colibri` in a tree is the module `a.b.c`; its package is the folder `a/b`. The files
directly in the root form the *root package*, named after the manifest (or the folder). So with
`game/player.colibri`, `game/enemy.colibri` and `main.colibri`, the modules `game.player` and
`game.enemy` share the package `game`, and `main` is in the root package. A subfolder is a
separate package, **also for scripts without a manifest**: `import util.strings` finds
`util/strings.colibri` next to the script, and it can only use what that file marks `pub`
(the implicit package is the script's folder, not its subtree). Only modules reachable by
`import` from the entry are loaded and checked; a stray file nobody imports is ignored.

The loader (`colibri_check::package`) reads the files through a `SourceFs` (the real file system,
or the in-memory one `load_sources` uses), parses each module once into the shared `SourceMap`
(so every `Span` already names its file), resolves every `import` item to a module and returns
`Loaded { files, metas, packages }`. **Module order is initialisation order**: the post-order of
a depth-first walk over the imports starting at the entry (imports of a module are followed in
source order, a module already on the walk is skipped, so cycles terminate), so an imported
module comes before its importer and the entry is last. `FileId`s follow discovery order instead,
so the entry file is `FileId(0)`.

### 11.2 Name resolution of an import

`import a.b` resolves to one of these candidates; exactly one must exist:

1. **local**: `a/b.colibri` under the root of the importing module's tree;
2. **dependency**: `a` is the name of a dependency of that tree, and `b.colibri` exists under the
   dependency's root (`import foo` alone means `foo/lib.colibri`, the library's root module);
3. **host**: `a` is one named host module (`vm.module("physics")`, `.cbi` header `module =
   "physics"`) and there is no `b`;
4. **std**: the standard library. Nothing is registered yet (`std_module` returns none), but the
   resolver already asks it, so adding `std.math` later changes no rule.

Zero candidates is `no module `a.b``, with what was looked for, and, when `a` is a folder, its
modules. More than one is `` `import math` is ambiguous ``, listing every candidate (file path,
dependency name, `std.math`); rename the file or the dependency, or write `import std.math`.
`import a.b.X` (a name, not a module) says to write `import a.b.{X}`.

### 11.3 One check for the whole program

The checker takes the modules in initialisation order and produces **one HIR `Program`** for
all of them (`check_modules`; `check_with_host` is the one-module case, used by everything that
had one file before). Items from all modules live in the same tables (`fns`, `globals`, `types`,
...), so ids need no per-module space, hot reload patches by id as before and the engines do not
know modules exist. Per module the checker keeps a `Scope`:

```text
Scope { values, type_ids, aliases, poisoned_types, modules }   // name -> entity
```

Today's lookups (`self.values.get(name)`, `self.type_ids.get(name)`, ...) are untouched: the
`Checker` holds the tables of the module being checked in those fields and *swaps* them when it
moves to another module (`enter_module`, a handful of `mem::swap`s). Entering a module happens
wherever an item is looked at: collecting its declarations, checking a function, initialiser or
default argument, and resolving an alias lazily on first use from another module. A scope starts
as a copy of the *base* (prelude and default `host` module names), then gets:

1. the module's own declarations (types first, so every signature may name any of them);
2. `import a.b` binds the module under its last segment (`b`), `as c` under `c`. Every item the
   module exports is entered into the scope as the dotted name `b.name`; **a qualified name is
   a scope entry whose name contains a dot**, so `player.spawn(..)`, `player.Player { .. }`,
   `player.Shape.Circle(1)`, `x: player.Player` and patterns all resolve through the lookups
   that already exist, after the few places that read `Field(Ident(player), spawn)` turn it
   into the name `"player.spawn"` (`FnCtx::qualified`). A local variable called `player` wins
   over the module, as any local wins over a global;
3. `import a.b.{X, Y as Z}` enters those names (types, functions and values) directly.

An item's *exports* are recorded when it is declared (`Export { target, public }`, per module). A
name is entered into a scope if the item is `pub` or the importing module is in the same package.
A name that exists but is hidden still gets a precise message: `` `hp` is private to the package
`game` ``, with the hint to mark it `pub`.

**Ambiguity.** A name entered into a scope twice for different entities (an own item and an
import, two imports, a module alias and a value) is an error at the second entry, ``the name `X`
is ambiguous``, listing every candidate (`this module`, `game.player.X`, `rapier.X`). The same
entity imported twice is not a clash. A user declaration that collides with a prelude or default
host name keeps its older message (``is a host function and cannot be redefined``).

**Visibility.** `pub` on a function, type, interface, alias or module-level value exports it from
its package; everything else is visible to every module of the same package (and a file in another
package never sees it). A struct or class field without `pub` is *package-visible*: another
package cannot read or write it, match it in a pattern, or construct the type with a literal; the
error for a literal says which fields are not exported and points at the exported functions of the
type's module that return the type ("use a constructor: `player.spawn`"). Enum variants and their
payloads are as visible as the enum; a method or associated function of a type from another package
needs `pub` to be called from outside that package (`obj.hidden()` and `Type.make()` are errors
there), like a free function. Host items are all
exported. The prelude and `host` items carry no package.

**Cycles.** Imports between modules of one package may form cycles (the checker declares every
module before it resolves any import, so order never matters). Between *packages* a cycle is an
error found by the loader: ``import cycle between packages `a` -> `b` -> `a` ``, with a label on
each import of the cycle.

**Top-level statements** exist only in the entry module. Any other module that has one gets an
error at its first statement ("only the entry file may have top-level statements"); its
declarations are still checked. `fn main` is looked up in the entry module only. Function names
in the HIR (`FnDef.name`, stack traces, `Vm::get_fn`) are the plain name in the entry module and
the dotted module path before it in any other (`game.player.spawn`, `game.player.Player.update`);
host items are never prefixed.

### 11.4 Initialisation and drop order

Globals are numbered module by module in module order, in declaration order inside a module. The
existing dependency walk (`init_order`) starts from each global in that order and visits what it
uses first, through functions too, whatever module it is in, and reports a cycle through values
as before (naming the chain, now across files). So (§4.2): a value runs after every value it
reads; otherwise an imported module's values run before its importer's, and a module's own in
declaration order. The interpreter drops module values in reverse initialisation order
(§11.2 of DESIGN), which therefore holds across modules without any new code.

### 11.5 Diagnostics across files

A `Diagnostic` points at one file through its span, and its labels may point at others (a cycle,
"first defined here", "declared in"). The renderer builds one `ariadne` cache of every file a
report mentions. Parse errors of all loaded files are reported together and checking is skipped
if any file has one, as for a single file. The entry file is `FileId(0)` and the "in the host
interface" messages are anchored at its start. Stack frames and panics name the file of the code
that was running (`Span.file`), which the `SourceMap` knows by the path it was loaded from
(relative to what was given: `load_file("games/x/main.colibri")` shows `games/x/util.colibri`).

### 11.6 `colibri.toml`

A minimal TOML subset is parsed by hand (`colibri_check::package::toml`, about 200 lines; the
`toml` crate would bring `serde`, `indexmap` and `winnow` for four keys): comments, `[tables]`,
`key = "string"`, `key = { inline = "tables" }`, bare and quoted keys, and a clear error with
line and column for anything else. Understood:

```toml
[package]
name = "mygame"

[dependencies]
physics = { path = "../physics" }
```

`name` is required in package mode. A dependency needs `path`; `git = ".."`, `version = ".."`
and `registry` say ``not supported yet``. `colibri.lock`, capabilities and any other table or key
are errors, not silently ignored ("unknown key"). A dependency folder must have a `colibri.toml`
whose `name` equals the key it is listed under. Execution of a package starts at `fn main()`
of `main.colibri`, after the module values are initialised and the top-level statements of
`main.colibri` (if any) have run.

### 11.7 Named host modules

`vm.module("physics")` returns a `HostModule` with `register_fn`, `register_fn_named` and
`register_type`; `vm.register_fn(..)` is the same thing on the default `host` module, whose names
stay in scope everywhere without an import. `import physics` binds the module (`physics.step()`),
`import physics.{World}` brings names in. A module's interface (`.cbi`) is its own file with
`module = "physics"`; `HostEnv` merges files of the same module and `colibri check --interface`
may name several. Items are keyed by `(module, name)`: `HostFnEntry.module`, the decl list of the
`TypeRegistry` and `HostFnDef.module` in the HIR. A type is declared in the module that first
described it, and a module's signatures may mention types of their own module and of the default
one only (registering a function that mentions another named module's type is
`Error::Registration`), because that is what the `.cbi` text of a module can express. Type names
stay unique across a `Vm`.

### 11.8 Golden tests for packages

A directory under `tests/golden` that has a `main.colibri` is **one test**: the package rooted
there (`Vm::load_package`), not a test per file. Directives are read from every `.colibri` file in
the directory tree: an `// error:`, `// warning:` or `// panic:` line refers to the line of the
file it is in (the harness compares file and line); the expected stdout is the `// expect:`
lines of `main.colibri` followed by those of the other files in path order; `// drops:`, `//
panic:`, `// main-err:` and `// skip:` are read from `main.colibri` only. See PHASE1.md.

### 11.9 Decisions the design documents did not fix

- Imports are always rooted at the tree root; there are no relative imports and no `..`.
- A subfolder is its own package, scripts included; the entry's folder is the implicit package.
- `import foo` of a dependency means `foo/lib.colibri`; `import foo.x` is `foo/x.colibri`.
- `import a.b.{X}` binds only `X`, not `b`.
- A package that is not imported is not checked (there is no "check everything under here").
- `main.colibri` is the entry of a package, and may hold top-level statements, which run before
  `main`. `colibri run script.colibri` in a folder with a manifest takes its dependencies from it.
- Ambiguity is reported where a name is *entered* (import time), so it appears even when the name
  is never used.
- Methods and associated functions are exported with `pub`, like fields and free functions.
- Initialisation order between independent values follows module order (imports first).
- No orphan rule: a module may add methods or interface impls to a type of another package it
  can name. Methods added to a type from another package see only its exported fields.
- Only one level of qualification (`player.spawn`): modules are bound by their last segment.
- A module that gets two names (`import a.b` and `import a.b as c`) is allowed; both work.

## 12. Formatter (`colibri fmt`)

`colibri fmt` is the canonical formatter of DESIGN §16.1: no options. It lives in the module
`colibri_syntax::fmt` (`format_source(name, text) -> Result<String, Vec<Diagnostic>>`), not in a
crate of its own: it needs exactly what `colibri-syntax` has (tokens with spans, the parser,
`ast_dump`) and nothing else, so a new crate would only add a dependency edge. The CLI depends on
`colibri-syntax` directly; the subcommand's logic is `crates/colibri-cli/src/fmt.rs`.

### Design

- **AST to document to text.** `fmt/print.rs` turns the AST into a `Doc` (`fmt/doc.rs`: text,
  indent, `Line`/`SoftLine`/`HardLine`, `Group`, `IfBreak`, `LineSuffix`, plus `Either` and
  `Pad`), the algebra of Wadler's pretty printer as Prettier uses it. A group is printed flat when
  it fits in the rest of the line (width 100, measured in characters) and broken otherwise.
- **Comments are not in the AST.** The lexer drops `//` and `/* */`. The formatter finds them
  itself: everything between two significant tokens is whitespace and comments, and a string
  (with its `{..}` holes) is one token, so a `//` inside a string is never a comment. Comments are
  handed out in source order while the document is built, which keeps them in order by
  construction:
  - an own-line comment goes on its own line before the element that follows it (statements,
    items, fields, variants, arms, list elements, method-chain links, operands, `else`);
  - a comment after code on the same line stays at the end of that line, one space after it
    (`LineSuffix`; it forces the groups around it to break, since nothing may follow a `//`
    comment on a line);
  - a short `/* .. */` followed by code on its line stays inline;
  - a comment before a closing `}`, `)` or `]` stays before it, after the last element;
  - a comment anywhere else (inside an expression where no line break is possible, such as
    between an operator and its operand on one line) moves to the end of the line of the
    enclosing element. It is never dropped.
  A doc comment (`///`) is always on its own line before the next element, so it keeps attaching
  to the same item.
- **Safety net.** `format_source` re-parses its output and returns an "internal formatter error"
  instead of the text if the program (`ast_dump` equal, so also the doc comments) or the multiset
  of comments differs. The tests run it over every golden file, with comments injected at the
  end, before and inside every line, and over the DESIGN.md examples; the net never fires.
- **Refuses syntax errors.** A file the parser reports errors for is returned as its
  diagnostics and never touched.

### Rules

- 4-space indent, one statement per line, `;` removed, spaces around binary operators, `=`, `=>`,
  `->` and after `,` `:`; none around `..`/`..=`, `.`, unary operators, inside `( )` and `[ ]`
  and inside map literals `{"a": 1}`; one space inside `{ }` of struct literals and patterns. No
  trailing whitespace, one final newline, `\r\n` becomes `\n`.
- Blank lines: a run of them becomes one, kept between statements, items, fields, arms and
  comments; none at the start or end of a block or file. The formatter never adds one.
- **Lists** (arguments, parameters, generic parameters and arguments, list/map/struct literals,
  tuples, variant payloads, imports, struct patterns) are printed on one line if they fit; if
  not, one element per line with a trailing comma. *Magic trailing comma:* a list that has a
  trailing comma followed by a line break stays one element per line. A list with a trailing
  comma on the same line, or with the elements on separate lines but no trailing comma, is
  collapsed if it fits. A one-element tuple always keeps its comma.
- **Blocks.** `{}` when empty. A block with exactly one statement or tail expression that was
  written on one line stays on one line if it fits (`if x { 1 } else { 2 }`, `fn f() { g() }`);
  anything else is expanded, including a block whose single statement is itself a block
  construct (`if`, `match`, `for`, `while`, a bare block): `for x in xs { if a { f() } else { g() } }`
  expands the `for` body, and the inner `if` may then stay on one line. Blocks of expressions
  in expression position (`let a = if c { 1 } else { 2 }`, `fn() { g() }`) are not affected. A block never has `;` between statements. The one `;` that stays is
  after the last statement of a block when it is an expression statement, because it is what
  keeps the expression from being the value of the block (`{ g(); }` and `{ g() }` are
  different programs); likewise a statement that ends in an open range (`let a = 0..;`) and a
  `match` arm that does (`0..,`), since a line break after `..` does not end a statement.
- **Declarations.** `struct`/`class`/`enum` bodies are one field or variant per line, without
  commas; one written on one line and fitting stays `enum Dir { North, East }`. Attributes are
  one per line before the item. `impl`, `interface` and `extern` members are always expanded.
- **Long expressions** break at the places where DESIGN §3.2 makes it legal:
  - method chains with two or more method calls (`.name(..)` with a lower-case name) break
    before each `.`, indented once, when they do not fit on one line; a `.Name` with a capital
    never starts a line (§3.2 rule 3: it would read as a `match` arm);
  - chains of `&&`, `||` and `or` break before the operator (rule 3 again), one operand per line;
  - every other binary operator stays at the end of its line, which continues the statement
    under rule 1; operators of different precedence nest, so `a && b || c` breaks at the `||`
    first;
  - a last argument that is a closure with a block, a `match` or a call that ends in one stays
    on the line of the call (`xs.for_each(fn(x) {`, `print(match a {`), as does the last operand of a
    chain (`"a" + match x {`), provided the part before it fits.
- **`match` arms** are one per line, no separators; consecutive arms with a one-line body
  have their `=>` aligned (the longest pattern plus guard sets the column); a blank line, a
  comment or an arm with a multi-line body ends a run.
- **Strings** (including raw and multi-line strings) are copied as written. Interpolation holes
  are not reformatted: `"{x+1}"` stays. Numeric and char literals are copied as written
  (`0xFF`, `1_000`). `else` goes on the line of the closing `}` (unless a comment sits between,
  then it keeps its own line, which rule 3 allows).
- The parenthesised forms the parser keeps (`(a + b)`, `(weak Node)?`, `(fn(int) -> int)?`,
  `(a as n) | b`) keep their parentheses; the formatter adds none that are not needed and drops
  none.

### Decisions the design documents did not fix

- Alignment is done for `match` arms only (as in DESIGN's examples). Trailing comments are not
  aligned with each other: each is one space after its code. Struct fields are not aligned.
- No "last element may hug" for list literals, only for the constructs above.
- Method chains collapse onto one line when they fit, even if the source had them broken.
- `colibri fmt --check` is the one flag (CI); `colibri fmt -` filters stdin to stdout.

### Known limitations

- Comments inside an expression at a place where no line break is possible move to the end of the
  line of their statement (`f(a /* x */ + b)` becomes `f(a + /* x */ b)`; a `//` comment between
  `a` and `+` on one line goes to the end of the line). Their text and order are kept.
- Trailing whitespace inside multi-line comments and strings is kept: they are copied.
- A very long string, a long pattern or a long type is never broken; lines can exceed 100
  columns where there is no place to break.
- `.cbi` interface files are not formatted by the CLI (only `*.colibri`).

## 13. Standard library (phase 1 core std)

The standard library is the crate `colibri-std`. Every module in it is a *named host module*:
it is registered through the same `Vm` machinery a game uses for `vm.module("physics")`, its
`.cbi` interface text is generated from the registrations, and the checker sees it only through
that text. There is no special case in the checker for "std functions"; the few extensions the
std needed (constants, generic functions, `@typed`) are general `.cbi` features any host can use.

### Dependency direction

`syntax → check → interp → runtime → std → cli`. `colibri-std` depends on `colibri-runtime`
(and `libm`), never the other way round, so a host that wants no standard library links none and
the runtime stays free of file, clock and JSON code. The CLI depends on `colibri-std`; so does
any embedding that wants it (and the language server, which should call
`colibri_std::interfaces()` to check programs and `colibri_std::install(&mut vm)` to run them).
`colibri_std::install_with(&mut vm, StdConfig)` is the configurable form; `install` uses the
real process (system clock, process environment, real stdin). `interfaces()` gives the same
interfaces without a `Vm`, for `colibri check`.

### Names and resolution

A std module's interface name is `std.<name>` (`std.math`). `Vm::std_module(name)` is the only way
to create one; `Vm::module("std.x")` is refused as reserved, so a host cannot shadow or fake the
library. The loader (`package::loader`, `package::std_module`) maps the import paths `["math"]`
and `["std", "math"]` to `std.math` when the host provides it.

- `import std.math` always means the standard library: a path whose first segment is `std`
  skips local files, dependencies and host modules (`import std` alone is "no module `std`").
- `import math` follows DESIGN §13: when exactly one candidate fits (a local file, a
  dependency, a host module, or the standard library) it is that one; when more than one fits,
  the import is an error that lists them all and says to rename the file or dependency or to write
  `import std.math`. So a local `math.colibri` next to the std `math` is ambiguous, not
  shadowing. `tests/golden/modules/std_*` cover the cases.
- A missing std module says so: `the standard library has no module `x``, with the list of the
  ones provided. `task`, `game` and `reflect` (phase 2), `mem` (phase 3) give "the standard
  library module `x` is not available until phase N", and `collections` says `List`, `Map` and
  `Set` are built in (`LATER_STD_MODULES`). `import test` cannot be written: `test` is a keyword
  (a syntax error). There is no `test` module; `assert` and `assert_eq` are builtins (§15).

### `.cbi` extensions

- `const NAME: float = 3.14` / `const NAME: int = 7` in an interface: public module constants
  (`Registration`: `register_const_float`, `register_const_int`). Floats print as the shortest
  decimal that round-trips, always with a `.` and no exponent.
- `fn f[T](..)`: generic host functions without bounds; `Value` (the opaque host value) is
  `IntoColibri`/`FromColibri` with `TypeDesc::Param("T")`.
- `@typed fn decode[T](..)`: the host function needs the concrete type arguments. The checker
  rejects a call where an argument is still a type parameter (`needs to know its type at the
  call`), and the interpreter passes the call's type arguments to the host:
  `Ctx::type_args()`. `Ctx::type_defs()` gives every struct/enum/class (`colibri_runtime::reflect`),
  which is what a serializer reads (DESIGN §7.5). Registering `typed()` on a non-generic function
  is an error.
- A capability error names the module: "`fs.read` needs the `fs` capability, which this host did
  not grant".

### Runtime API added for std (usable by any host)

`Value::kind() -> ValueKind` (Unit, Bool, Int, U64, Float, F32, Char, Str, Struct, Tuple, Enum,
List, Map, Set, Other) and constructors (`Value::int`, `str`, `list`, `map`, `tuple`, `option`,
`new_struct`, `new_enum`, `ok`, `err`, ...); `Ctx::type_args`, `type_defs`, `write_stdout`
(the `Vm`'s stdout, where `print` writes), and `Ctx::raise(message) -> !`, which ends the
program with a Colibri panic of exactly that message from anywhere in a host function (the Rust
stack unwinds to the trampoline with a private payload; no panic message is printed; `defer`s and
`Drop` hooks run as for any panic).

### Configuration and hermetic runs

`StdConfig` holds what a module reads from the world: `clock` (`SystemClock`, or `ManualClock` a
test advances), `args`, `env` (`EnvSource::Process` or `Fixed`), `temp_dir`, `seed`, `stdin`, `local_offset`. The
golden harness installs a fixed one (clock 1_700_000_000.0, args `alpha beta`, env
`COLIBRI_GOLDEN=yes`, seed 1, local time zone +05:30, stdin `first line\nsecond line\n`, and a temp dir created per test
and removed after it). `os.set_env` writes an overlay in the `Vm`, never the process
environment (`set_var` is unsafe in edition 2024, and a library must not do that behind a
program's back).

### Capabilities

| Module | Needs `fs` | Needs `os` | Ungated |
|--------|-----------|------------|---------|
| `fs` | everything | | |
| `os` | | `env`, `vars`, `set_env`, `temp_dir`, `cwd`, `exit` | `args`, `platform` |
| `io` | | `read_line`, `read_all` | `write`, `eprint`, `eprintln` |
| `time` | | `unix`, `sleep` | `now`, `monotonic` |
| `path`, `math`, `rand`, `str`, `json`, `args` | | | everything |
| `process` | | everything | |
| `datetime` | | `now`, `now_local` | the rest |

The CLI grants all capabilities. `Cap::Os` now reads "processes, the environment, wall-clock
time and sleeping".

### Modules

- `io`: `write(text)` (no newline), `eprint`, `eprintln` (stderr), `read_line() -> Result[Option[str]]`
  (`none` at end of input), `read_all() -> Result[str]`. `print` stays a builtin.
- `fs`: `read`, `read_bytes`, `write`, `write_bytes`, `append`, `exists`, `is_file`, `is_dir`,
  `list` (sorted names), `create_dir` (like `mkdir -p`), `remove`, `remove_dir` (recursive), `copy`,
  `rename`, `size`. Failures are `err` with the message of the OS error and the path.
- `path`: pure string functions with `/` separators only: `join`, `file_name`, `stem`, `ext`,
  `with_ext`, `parent`, `is_absolute`, `normalize`, `split`.
- `os`: see the table; `args() -> List[str]`, `platform() -> str`, `env(name) -> Option[str]`,
  `vars() -> Map[str, str]` (sorted), `exit(code)`.
  `exit` ends the program through `Ctx::raise` ("the program exited with code N"), so `defer`s
  run; `colibri_std::exit_code(&mut vm)` returns the code, and the CLI uses it as the process
  exit status.
- `math`: constants `PI TAU E SQRT2 LN2 LN10`; float functions `sin cos tan asin acos atan sinh
  cosh tanh exp exp2 ln log2 log10 log(x, base) cbrt sqrt floor ceil round trunc fract radians
  degrees signf atan2 pow hypot lerp is_nan is_finite clampf`; integer functions `min max clamp
  abs sign floor_div floor_mod gcd`. Integer division by zero and an empty clamp range panic.
- `rand`: module functions on a global generator (`seed`, `int(lo, hi)` half-open, `float`,
  `bool`, `chance(p)`, `choice`, `shuffled`) and the host class `rand.Rng` (`Rng.new(seed)`,
  `int`, `float`, `bool`, `chance`). Seeding is SplitMix64 into xoshiro256**; `below` uses
  Lemire's unbiased method; floats use the top 53 bits. Same seed, same sequence on every
  platform (unit tests pin reference values checked against an independent implementation). The
  unseeded generator starts from the clock, so it is not deterministic until `rand.seed`.
  `choice` and `shuffled` are generic and do not mutate their argument.
- `time`: `now() -> float` (seconds since the program started, ungated, meant for frame timing),
  `monotonic() -> int` (nanoseconds), `unix() -> float`, `sleep(seconds)` (rejects negative and
  non-finite durations).
- `str`: only what is not already a method: `parse_int`, `parse_float`, `parse_bool` (all
  `Result`), `lines`, `repeat`, `pad_left`, `pad_right`, `trim_start`, `trim_end`, `strip_prefix`,
  `strip_suffix`.
- `json`: `encode[T](value) -> Result[str]`, `pretty[T]`, `@typed decode[T](text) -> Result[T]`.
  Mapping: `int`/sized ints, `float`, `bool`, `str`, `char` (one-character string), `List`,
  tuples (arrays), `Map[str, V]` (objects), `Option` (`null`; a missing field decodes to `none`),
  structs (objects by field name), payload-free enums (the variant name as a string). The
  parser is strict (no comments, trailing commas, duplicate keys; depth limit 128) and reports
  line and column. Decode errors say where (`$.items[2].hp`) and what was expected: missing
  field, wrong type, number out of range for the target, unknown enum variant. Unknown fields in
  the input are ignored. Not supported, with an error that says so: enums with payloads, classes,
  generic structs, maps with non-`str` keys, functions, non-finite floats.
- `collections`: intentionally absent (see above).

### `libm`

`f64::sin` and friends call the platform's C library, so digits differ between machines. The
std uses the `libm` crate (pure Rust port of musl, one new dependency, no transitive ones) for
`sin cos tan asin acos atan sinh cosh tanh exp exp2 ln log2 log10 pow cbrt hypot atan2`, so
output is identical everywhere and goldens can pin it. The results are within an ulp of the
true value, not always the nearest double: `math.exp(1.0)` is `2.7182818284590455`, one ulp
from `math.E`. `sqrt`, `floor`, `ceil`, `round`, `trunc` and arithmetic are exact IEEE operations
and use the hardware.

### Decisions the design documents did not fix

- Errors from `fs`, `io`, `json`, `str.parse_*` are message-only `Error`s, not typed enums.
- The `time` and `io` capability split in the table above; `os.args` and `os.platform` are open.
- Integer-only `min max clamp abs sign` (`clampf`, `signf` for floats); `rand.int` is half-open and
  panics on an empty range.
- Paths use `/` only; `fs.list` is sorted; `fs.create_dir` creates parents; `fs.remove_dir` is
  recursive; `os.vars` is sorted.
- Goldens get a fixed `StdConfig`, and `// caps: fs, os` grants capabilities per file
  (PHASE1.md).
- Not done: `@rename`/`@skip` and field defaults in `json` (no attribute support in the
  checked types yet), JSON for data enums, typed error enums.

### `regex` (phase 2, part B)

`regex.compile(pattern) -> Result[Regex]` on the Rust `regex` crate (linear-time matching: no
look-around, no back-references; Unicode on). An invalid pattern is an `err` carrying the crate's
multi-line message; patterns whose compiled program exceeds 10 MiB are refused the same way. No
capability. `Regex` and `Captures` are host classes (reference values: copies of a variable
share one object, like `rand.Rng`); `Match` is a struct `{ text: str, start: int, end: int }`.

- `Regex`: `is_match(s)`, `find(s) -> Match?`, `find_all(s) -> List[Match]`, `captures(s) ->
  Captures?`, `replace(s, replacement)` (first match), `replace_all`, `split(s) -> List[str]`,
  `pattern()`.
- `Captures`: `len()` (group 0 counts), `get(i) -> str?`, `named(name) -> str?`, `span(i)` and
  `named_span(name)` (`Match?`). A group that did not take part, an unknown name or an index out
  of range is `none`.
- Replacement text expands `$1`, `${1}`, `$name`, `${name}`; `$$` is a dollar sign. `$1a` is the
  group named `1a`, so write `${1}a`.
- **Offsets are byte offsets** into the UTF-8 text, the unit of `s[a..b]` (strings cannot be
  indexed by integer, DESIGN §7), so `s[m.start..m.end] == m.text` always holds.

### `http` (phase 2, part B)

A blocking client on `ureq` 3 with rustls (ring) and the bundled `webpki-roots`: `https://`
works with no system TLS library and no OpenSSL. Every function needs `Cap::Net` (`// caps: net`
in a golden; the CLI grants everything).

- `get(url)`, `post(url, body)` (`content-type: text/plain; charset=utf-8`),
  `post_as(url, body, content_type)` and `request(method, url, headers: Map[str, str], body)`,
  all `Result[Response]`; `set_timeout(seconds)`. Host functions have no optional parameters, so
  the "optional content type" is a second function (`post_as`) rather than a defaulted argument.
  `request` upper-cases the method and sends no body when `body` is `""`.
- `Response` is a struct `{ status: int, headers: Map[str, str], body: str }`. Header names are
  lowercased; a header that arrives several times is joined with `, `. Any status is `ok`
  (`404` is data); `err` means no response: bad URL, refused connection, TLS failure, timeout, a
  body over 64 MiB. Messages read `http: GET url failed: reason`.
- The body is text: bytes that are not UTF-8 are decoded lossily (U+FFFD). There is no byte
  body in v1. There is no `Response.json()`, because a struct has no methods and `json.decode`
  needs the target type at the call: write `json.decode[T](response.body)`.
- Redirects are followed (10 at most); `HTTP_PROXY`, `HTTPS_PROXY` and `NO_PROXY` are honoured.
- Timeout: 30 s for a whole request by default; `http.set_timeout(s)` changes it for the rest of
  the program (`0` is none, negative or non-finite panics). The setting lives in a state of its
  own in the `Vm`, not in `StdConfig`.
- **A call blocks the whole `Vm`, coroutines included**: a host function cannot wait (§16), so no
  other coroutine runs until the response is complete or the timeout fires.
- Tests: `crates/colibri-std/tests/http.rs` starts a hand-written HTTP/1.1 server on
  `127.0.0.1:0` in the test process (nothing leaves the machine); the golden
  `std/http_needs_capability` covers the refusal without `net`. There is no golden with a live
  server because the harness has no hook for a URL.
### 13.1 Scripting std, part A: `args`, `process`, `datetime`

Three more host modules in `colibri-std`, written like the others. `MODULES` and `install_with`
list them; their `.cbi` text, and so the language server's hover, comes from the registrations.

**Value structs from Rust.** `#[colibri::export(value)]` asserts `Copy`, and `process.Output`
holds strings, so `values.rs` has a small `value_struct!` macro that writes `IntoColibri` and
`FromColibri` for a plain Rust struct as the Colibri struct of the same name, field by field in
the order written. A host function that takes or returns one registers the type in its module.
A program may declare its own `Output` or `DateTime` (golden `process_names_do_not_clash`).

**`args`.** `os.args()` already holds what `colibri run file -- a b` passes (the CLI hands
everything after `--` to `StdConfig::args`), so `args` is a parser over it and needs no
capability. `list()` is `os.args()`. The grammar: `--` ends the options; `--name`, `-n`, and
groups `-vq` are flags; `--name=value`, `--name value`, `-n value` and `-n=value` give a value;
`-` and negative numbers are positional; the last occurrence wins. `flag(name) -> bool`,
`value(name) -> Option[str]`, `positional() -> List[str]`, `positional_with(value_options) ->
List[str]`. A word after an option is a value or a positional only if the parser is told which
options take one: `value("out")` knows, so it takes `--out file`; `positional()` does not, so it
treats `--out file` as an option followed by a positional. `positional_with(["out"])` names them.
There are no declarations, help text or subcommands; a script that needs more writes it.

**`process`.** Needs `Cap::Os` (no new capability: starting programs is machine state like the
environment). `run(program, args) -> Result[Output]` waits and captures; `Output { status: int,
stdout: str, stderr: str }` (text is decoded lossily). A program that cannot start (missing,
not executable, bad `cwd`) is `err`; one that exits non-zero is an `Output` with that `status`;
one killed by a signal has `-N` on Unix and `-1` when the system says nothing. The language has
no default arguments for host functions, so options are a struct: `process.options()` returns an
empty `Options { cwd: Option[str], env: Map[str, str], input: Option[str] }`, a script sets what
it needs, and `run_with(program, args, options)` / `spawn_with` take it. `output(program, args)
-> Result[str]` is the standard output of a program that succeeded (an `err` with its status and
standard error otherwise); `status(program, args) -> Result[int]` runs with the script's own
standard output and error (which the `Vm`'s buffer never sees); `shell(line)` is `run` through
`sh -c` (`cmd /C` on Windows). The child's environment is the process's, then what `os.set_env`
set, then `options.env`; `os.env` under a `Fixed` source does not restrict children. Its standard
input is closed unless `options.input` gives text, so a child never reads the script's stdin.
Output is drained on threads, so a program writing a lot cannot deadlock.

Blocking: host functions cannot suspend (waits panic inside host calls, §16), so `run`, `shell`,
`status` and `Child.join` block the whole `Vm`, every coroutine included. For concurrency,
`spawn` returns at once with a host class `Child` (`id()`, `try_wait() -> Result[Option[Output]]`,
`join() -> Result[Output]`, `kill() -> Result[Output]`); a coroutine polls `try_wait` with
`wait(0.01)` between calls (golden `process_polling_in_a_coroutine`). `try_wait` repeats its
answer once the child ended. The method is `join`, not `wait`, to keep `wait` for the builtin.
Dropping a `Child` that has not ended kills it, so a script cannot leak a process by forgetting
a handle (a script that wants a detached program starts it through `shell("... &")`).

**`datetime`.** No dependency for calendar math (proleptic Gregorian, days-from-civil, no leap
seconds, years 0 to 9999), so results are the same on every platform. `DateTime { year, month,
day, hour, minute, second, nanosecond, offset }` is a struct of ints; `offset` is the seconds
east of UTC the fields are written in. Equality is structural, so the same instant at two offsets
is not `==`; `diff(a, b) == 0.0` is the test for the instant. Functions: `now()` (UTC),
`now_local()` (both `Cap::Os`, from the clock of `StdConfig`), `new(y, mo, d, h, mi, s) ->
Result[DateTime]` (UTC, validated), `from_unix(seconds: float)`, `to_unix(dt) -> float`,
`to_utc`, `to_local`, `with_offset(dt, seconds)`, `add(dt, seconds: float)` (keeps the offset),
`diff(a, b) -> float` seconds, `weekday` (ISO: 1 Monday to 7 Sunday), `day_of_year`,
`is_leap_year`, `days_in_month`, `format`, `parse -> Result`, `to_iso`, `parse_iso -> Result`,
constants `MINUTE HOUR DAY WEEK`. Instants are floats like `time.unix()`; a float has no
nanosecond precision at today's date, so `from_unix` splits whole seconds from the fraction.
`format` patterns (strftime subset): `%Y %y %m %d %e %j %H %I %M %S %f` (nine digits) `%L`
(milliseconds) `%p %B %b %A %a %u %z %:z %Z %s %F %T %%`; an unknown one in `format` is a panic
(a bug in the program), in `parse` an `err` that says where the text stopped matching. `parse`
fields it does not read default to 1970-01-01 00:00:00 UTC; `%y` follows POSIX (69 to 99 are
1900s); `%I` needs `%p`. `parse_iso` takes `YYYY-MM-DD` with an optional `T` or space, `hh:mm`
`[:ss[.fraction]]`, and `Z` or `+hh[:mm]`; `to_iso` prints the fraction with trailing zeros cut.
No months/years arithmetic and no named zones (no tz database): offsets only.
Local time: `StdConfig::local_offset` (`Some(seconds)` fixed, golden harness +05:30) or the
machine's zone through `localtime_r` on Unix (the `libc` crate, one `unsafe` call; already in
the lockfile through libffi) and UTC on Windows.

Decisions the language owner may want to overrule: `process`/`datetime` as module names and
free functions rather than methods (value structs cannot have host methods yet); `Options` as a
struct to fill in; reusing `Cap::Os`; `join` instead of `wait`; kill-on-drop; the lossy decode of
output; weekday numbering; floats for instants and spans.

## 14. Language server (`colibri lsp`)

`colibri-lsp` is a library (the CLI's `lsp` subcommand calls `colibri_lsp::run_stdio`) speaking
LSP over stdio with the rust-analyzer crates `lsp-server` (synchronous message loop, no async
runtime) and `lsp-types`, plus `serde`/`serde_json` for the payloads. It depends on
`colibri-syntax` and `colibri-check` only: it reads `.cbi` files with `HostEnv::from_text` and
loads packages with `package::load`, so the runtime is not involved.

### What it does

- **Diagnostics.** On open, change and save of a document, the server re-checks the program the
  document belongs to, synchronously, and publishes the diagnostics of every file the loader
  read, an empty list for files that became clean or left the program. Unsaved buffers
  override the disk through an overlay `SourceFs` (`Overlay`: buffers first, then `DiskFs`), so
  an edit of `lib.colibri` shows up in the errors of `main.colibri`'s package. Which program:
  the package of the nearest `colibri.toml` above the document (`Entry::Package`), else the
  `main.colibri` of the document's folder, in each case only if that program reaches the
  document; otherwise the document alone (`Entry::File`, a script). Syntax errors stop at the
  loader, as in `colibri check`. A diagnostic's secondary labels become `relatedInformation`
  (labels in files the program does not hold, such as the prelude, are dropped) and its hint is
  appended to the message after `hint:`.
- **Positions** are UTF-16 code units (`convert::LineIndex`); the compiler's spans are byte
  offsets. A position inside a surrogate pair rounds to the end of the character.
- **Formatting.** `format_source` on the buffer; one edit replacing the whole text, none if it
  is already formatted, `null` if the file has syntax errors.
- **Hover and go to definition** come from `colibri_check::insight` (below). Hover shows a
  Markdown code block: a binding as `name: type` (`var` for mutable ones), a function or method
  as its signature, a field as `field: type`, any other expression as its type. Definition
  works for locals and parameters, functions, methods, struct and class literals (the type),
  variant constructors, fields (`p.x`, `self.x`), and module-qualified or imported names
  (across files). Declarations answer too, so hover on the name in `fn f(..)` gives the
  signature.
- **Host interfaces.** `.cbi` files come from the client's `initializationOptions`
  (`{"interfaces": ["engine.cbi"]}`, or the same under a `colibri` key; relative paths are
  taken from the workspace folder), and from the top level of a folder that has a
  `colibri.toml` (the workspace folder, and the document's package folder). All capabilities
  are granted, like `colibri check`. An unreadable `.cbi` is reported once with
  `window/logMessage` and skipped.

### The position query (`colibri_check::insight`)

Checking normally records nothing extra. `check_modules_with_insight` is `check_modules` that
also fills a table while it runs: the zonker, the last step of every body (and closure), has
the final types of every expression, so with a sink present it pushes a `RawFact` (span, type,
what the span names) for each expression and for each local at its declaration. After the whole
program is checked those become `Fact`s (`span`, hover `text`, `def` span of the declaration)
with types rendered by name, plus facts for the declarations of functions, globals, types, fields
and variants. Without a sink (`check`, `check_modules`) the only cost is a `None` test per
expression. The table is built even when the program has errors, since the zonker runs for every
body; only a syntax error (no checking at all) leaves the server without facts.

`Insight::at(file, offset, word)` returns the smallest fact whose span covers the offset, the
last recorded one on a tie. HIR has no span for the name inside a call or a field access, so
facts for those (callee, struct literal, field) are *named* and answer only when the word under
the cursor is that name; the call's type is still the fact of the whole call. For `p.x` the
local `p` gets its own fact over its name, the whole read is the field.

### Decisions the design documents did not fix

- Full document synchronisation (`TextDocumentSyncKind::FULL`): packages are small in phase 1
  and it avoids incremental UTF-16 edits. No debounce, and no cancellation: a check runs to
  the end (the checker's own nesting limit bounds it).
- The analysis behind a document is the one most recently run that contained its file, so
  hover after editing a sibling uses that sibling's check.
- Diagnostics of files that are not open are published too (that is the point for an error in
  another file); they are cleared when no open document's program holds them any more.
- Hover on the checked types of a program with a syntax error is empty (the loader stops
  before checking); the previous analysis is not reused because its spans would be stale.

### Known limitations

- Not implemented: completion, references, rename, document symbols, workspace diagnostics of
  files nobody opened, semantic tokens, incremental sync, `.cbi` hover for host functions'
  declarations (they have no source file; definition on a host function says nothing).
- Definition does not cover type names written in annotations (`let p: Point`), field names in
  struct literals or patterns, or built-in methods (`list.push`); hover on those shows the
  expression's type.
- Default argument values are re-checked at each call, so a hover inside one may show the type
  recorded at a call site.

The language server (§14) always checks against `colibri_std::interfaces()`, as `colibri check`
does, so `lsp` depends on `colibri-std`. `colibri run file -- a b` passes `a b` to `os.args()`
(`colibri_lang::build_vm(builder, args)`).

## 15. Tests (`test` blocks, `assert`, `colibri test`)

DESIGN §16.1 says `colibri test` runs `test "name" { ... }` blocks and §13.1 lists a `test`
module in the core std. This section is how phase 1 builds both; the departures from DESIGN
are collected at the end.

### Syntax and checking

`test "name" { ... }` is an item, allowed at the top of any module (not inside functions or
`impl`s). The name is a plain string literal (no `{}` holes). The lexer, parser (`TestDecl`) and
formatter already handled it; the checker used to answer "unsupported".

A test is checked as a function `() -> Result[()]` that nothing calls: it gets a `FnDef` in
`Program::fns` named `test "name"` (`game.player.test "name"` outside the entry module, so stack
traces say which test) and a `TestDef { name, module, root, f, span }` in `Program::tests`, in
source order (the entry module first, then the others in load order).

- **`Result[()]` return.** The body may use `?` on a `Result`: an `err` fails the test with the
  error chain. A body that reaches its end returns `ok(())` (DESIGN §6.1), and a bare `return`
  ends the test successfully (inside a test only; in an ordinary function it stays "needs a
  value"). `return ok(())` and `return err(..)` also work. A test cannot use `?` on an `Option`
  (the usual error).
- **Always checked, never run.** `colibri check`, `colibri run` and every load type-check the
  test blocks of all the modules they load, so tests cannot rot. `run` and `Vm::run_main` never
  execute them. The test bodies stay in the HIR (about one function body of memory each); they
  are not compiled out. Nothing runs them, so they cost nothing at run time, and keeping them in
  every program keeps one checker path (and one set of `FnId`s) for `check`, `run`, the language
  server and `test`. Stripping them in a release build is a later `colibri build` option.
- **Unique names.** A second `test "x"` in one module is an error ("the test "x" is defined more
  than once in this module", labelled with the first). Equal names in different modules are fine;
  the report names a test `module::name`.
- **Language server.** Nothing special: it calls the checker, which checks test bodies, so errors
  inside them are diagnostics and hover/definition work (`tests/lsp.rs`).

### Assertions: builtins, not a module

`assert(cond: bool, msg: str = "assertion failed")` and `assert_eq(a: T, b: T)` are builtins like
`print` and `panic`, available everywhere (not only in tests), and reserved names
(`BUILTIN_FNS`; a program or host function cannot redefine them).

- `assert_eq` needs both sides to have one type that supports `==` (`Eq`; derived for most
  types, a user `impl Eq` is honoured, a type parameter needs `T: Eq`). `Show` is not checked
  because every type has it. Mixed types are the usual "cannot apply `==`" error, a function
  value is the usual "cannot compare" error. There is no third `msg` argument.
- HIR: `Intrinsic::Assert` (condition, message) and `Intrinsic::AssertEq` (both values), type
  `()`. A failure is an ordinary panic at the call, so the location and stack are the panic's, and
  `defer`s run. The message of `assert_eq` is
  ```text
  assertion failed: `left == right`
    left: 3
   right: 4
  ```
  with values in their `Show` form (a user `impl Show` is used) and top-level strings quoted so
  that spaces and empty strings are visible.
- `assert` has an optional `msg` (positional or named `msg:`). It does not echo the source of the
  condition; the location points at it.

### Why not `import test`

`test` is a keyword, so `import test` is a syntax error (§13), and `test.assert(..)` cannot be
written. Builtins avoid renaming the keyword, need no import in every test file, and also serve
programs that want a plain `assert`. The std `test` module of DESIGN §13.1 stays unbuilt; extras
(approximate float comparison, `assert_ne`, `expect_panic`, fixtures) can be builtins or a module
under another name later.

### Loading for tests

`Vm::load_tests(path)` (and the loader option `package::LoadOptions { all_modules }`) loads a
script like `load_file`, or a package folder like `load_package` **plus every `.colibri` file
under the package root**, including ones nobody imports (skipping folders that start with `.` and
folders with their own `colibri.toml`, which are other packages). `import`ed modules of a script
are loaded as always; stray siblings of a script are not (they are not part of it, §11.1). The
extra modules come first in initialisation order, the entry last. Nothing runs at load: no
initialisers, no top-level statements. `colibri check` does *not* use this option, so a file
that nobody imports stays unchecked there, as before; `colibri test` checks it.

### Isolation: fresh module state per test

`Vm::run_test(id)` does, for each test: (1) release the module-level values in reverse
initialisation order (their `Drop` hooks run; `Interp::release_globals`), (2) run every module
initialiser again (`Interp::run_initialisers`, not the top-level statements), (3) call the test
function, (4) release the values again. Module initialisation is cheap (§11.4), so a test never
sees what another did, whatever its order or filter, and drop output belongs to the test that
caused it. Host state outside the `Vm` (a `Handle` store, `insert_state`) is the host's to
reset. Each of the four steps is its own entry from the host and gets its own instruction budget.
A panic in a `Drop` hook during step 4 is discarded (nobody can be told, as at shutdown).
A `Vm` that was loaded with `load_file` and has run its top-level code loses that state at the
first `run_test` (it is released and re-initialised without the statements), so use a separate
`Vm` for tests.

### Runtime API (usable by any host, EMBEDDING §9)

```rust
vm.load_tests(path)?;                        // check; run nothing
let tests: Vec<TestInfo> = vm.tests();       // id, name, module, root, file, line
vm.set_instruction_budget(Some(100_000_000));
match vm.run_test(tests[0].id) {             // Result<(), Error>
    Ok(()) => {}
    Err(Error::Panic { message, location, stack, .. }) => {}   // assert, panic, arithmetic
    Err(Error::TestReturnedErr { message }) => {}              // `err(e)` returned: the chain
    Err(Error::BudgetExhausted { .. }) => {}                   // an endless loop
    Err(_) => {}
}
```

`TestInfo::root` is false for the tests of a dependency (`colibri.toml` `[dependencies]`);
`tests()` lists them anyway, the runner skips them. `Error::TestReturnedErr` is new
(`#[non_exhaustive]` already). `Vm::tests()` is empty before a load, `run_test` before a load is
`Error::NotLoaded`, an id out of range `Error::FnNotFound`.

### `colibri test [path] [--filter TEXT] [--budget STEPS] [--nocapture]`

`path` is a script file or a package folder (the current folder by default), loaded with
`Vm::load_tests`. All capabilities are granted, as for `run`, and the standard library is
installed with the real process configuration (not the hermetic one goldens use). A compile
error is printed to stderr and exits 1 before any test runs; warnings go to stderr. Then, on
stdout:

```text
running 3 tests
test main::adds ... ok
test util.strings::trims ... FAILED
test main::loops ... FAILED

failures:

---- util.strings::trims ----
<what the test printed>
panic: assertion failed: `left == right`
  left: "x"
 right: "x "
  at test "trims" (util/strings.colibri:12:5)

---- main::loops ----
error: the instruction budget is exhausted
  at ...
hint: an endless loop? raise the limit with `--budget <steps>` (0 is unlimited)

1 passed, 2 failed
```

- Tests are those of the program's own modules, in source order; dependencies' are skipped.
  `--filter` keeps those whose `module::name` contains the text; the summary adds `(N filtered
  out)` when it dropped some.
- A panic, a failed assertion, an `err` and an exhausted budget fail only that test. The exit
  code is 1 if any test failed or the program does not compile, else 0 (also for zero tests).
  There is no separate code for panics (`run` uses 101): a failed test is the expected outcome
  of a test run.
- `--budget` is the instruction budget of one test (default 100 000 000 steps, `0` unlimited),
  see EMBEDDING §5. The setting is per entry, so it applies to initialisers and clean-up too.
- Output a test `print`s (and `io.write`) is held in memory and shown under `---- name ----` for
  failing tests only; `--nocapture` writes it as it happens (before the test's result line).
  `eprint` is not captured (it goes straight to stderr).
- `os.exit(n)` in a test ends that test as a failure ("the program exited with code n"); it does
  not end the run.

### Decisions the design documents did not fix

- Assertions are builtins (above): **a deviation from DESIGN §13.1**, which lists a std `test`
  module. Not recorded in DESIGN.md.
- `Result[()]` tests with `?`; bare `return` allowed in tests.
- Isolation by re-initialising module values per test, not by loading the program again.
- Tests stay in the HIR of non-test programs.
- Test names are unique per module, shown as `module::name`; the entry module's name is its file
  stem (`main`, `t`), another module's its dotted path.
- A package's tests include modules `main.colibri` does not import (§Loading for tests);
  a script's include only what it imports.
- Exit code 1 for failures; the report is on stdout.

### Known limitations

- `colibri check` does not look at modules a package's entry does not import.
- No parallel runs, no per-test timeout in time (only the step budget), no test attributes
  (`@skip`, `@should_panic`), no setup/teardown hooks other than module initialisers and
  `defer`, no `assert_ne`, no float tolerance helper, no source text in the `assert` message.
- A `Drop` panic during a test's final clean-up is not reported.
- Panics raised inside a host function that does not unwind through the interpreter are the
  host's; only Colibri panics are caught per test.

## 16. Coroutines as built

DESIGN §14.2 and §14.3, PHASE2 step 2. The language is general-purpose, so the text avoids game
wording: a *frame* below means one scheduler tick. Everything is in `colibri-interp/src/coro.rs`
unless said otherwise.

### Surface

- `go call(args)` is an **expression** of type `Coroutine[T]` (`T` is the call's type), so
  `let c = go worker(3)` works and a bare `go worker(3)` is an expression statement (the AST has
  `ExprKind::Go`; `StmtKind::Go` is gone; `fmt` and the AST dump print it the same). The operand
  must be a call of a function, a function value or closure, an interface method or a host
  function. A method that takes `var self` on a struct is a checker error ("`go` cannot start
  this call"): its receiver would have to be written back into the caller's variable while the
  coroutine runs. A class method (`go actor.act()`) is fine. HIR: `ExprKind::Go(Box<Expr>)`
  around the checked call.
- The arguments are evaluated on the caller's fiber (as `eval_twin` temporaries, like any call);
  the coroutine owns its own references. It then runs until its first pause and `go` returns.
  A bare `go` is detached: the scheduler keeps the coroutine, dropping the handle changes
  nothing. `go` inside a coroutine is allowed (the child runs nested on the parent's stack
  until its first pause; each coroutine has its own stack).
- `Coroutine[T]` is a **prelude class** (`class Coroutine[T] {}`, `COROUTINE_TYPE`, the type
  table's `FIRST_USER_TYPE` moved from 12 to 13), like `Error`: a reference type with built-in
  methods, identity equality, usable in fields and collections, no `Coroutine { }` construction.
  Its runtime value is a `ClassObj` with hidden fields `[id, status, result, name]`; the scheduler
  keeps only a `Weak` pointer to it, so a handle dies like any object and `// drops:` traces skip
  it. `Show` prints `<coroutine name>`.
  - `done() -> bool`: finished, panicked or cancelled.
  - `cancel()`: below.
  - `join() -> T`: **returns the callee's return value** (hence `Coroutine[T]`). Inside a
    coroutine it pauses until the target is finished, then returns the value (again and again:
    the value is kept in the handle). Joining a coroutine that panicked panics in the joiner
    ("the joined coroutine `f` panicked: ..", and the original failure is considered handled and
    not reported a second time); joining a cancelled one panics ("was cancelled"); joining
    itself panics. Outside a coroutine, `join` on an **unfinished** coroutine panics like `wait`
    does; on a finished one it just returns the value.
- Builtins (reserved names, `BUILTIN_FNS`, also reserved against host registration):
  `wait(seconds: float)`, `wait_frames(n: int)`, `wait_until(cond: fn() -> bool)`,
  `wait_real(seconds: float)`. They are HIR intrinsics (`Intrinsic::Wait` ..) of type `()`. Any
  function may call them; the check is at run time.
  - `wait(s)` counts **scheduler time**: the sum of the `dt` of the ticks (`vm.tick(dt)`,
    `task.tick(dt)`), and under the event loop (below) the real time that passed. A negative `s`
    is 0, NaN panics. The wake test has 1e-9 slack so five `wait(0.1)` equal a `wait(0.5)`.
    `wait(0)` pauses until the next tick (even a `tick(0.0)`).
  - `wait_frames(n)` waits `max(n, 1)` ticks, a frame being one scheduler tick (the counter
    advances at the start of every tick).
  - `wait_until(cond)`: the condition is evaluated **on the coroutine's own stack**: first at
    once (if true no pause happens), then once per tick (the coroutine is resumed every tick,
    runs `cond()` and pauses again if false). So a condition may do anything a closure can, and
    its panic is the coroutine's. The cost is one resumption per tick per such coroutine.
  - `wait_real(s)` uses the injectable `Clock` (default: wall clock). It is checked at ticks,
    so it needs ticks to happen (the event loop provides them).
- `task.tick(dt: float)` is **not** a named host module: `task` is a name the checker resolves
  specially (`check_task_member`) when no local, module-level value or imported module is called
  `task`, so DESIGN's examples work with no import and a host may still register a module
  `task` (it wins). Only `tick` exists; `task.run` etc. (thread tasks, §14.1) will extend the
  same place. A named module would have needed `import task` and a host-side `Vm` hook for
  something that is part of the language, so it is an intrinsic instead.

### Scheduler and ticks

One `Scheduler` per `Interp`: a `Vec<Entry>` in start order (ids grow, lookup is a binary
search), scheduler time, the tick counter, the clock. `tick` (Rust: `Vm::tick(dt)`, Colibri:
`task.tick(dt)`; both call `tick_core`) adds `dt`, bumps the counter, then walks the entries that
existed when it began, **in start order**, and resumes each due one exactly once. A coroutine
started during the tick is appended after that snapshot, so it first resumes in the next tick.
Finished entries are dropped at the end of the tick (and during `go` when many finished). A
suspended entry keeps a `Wake`: `Ready` (every tick), `Time`, `Frame`, `Real` or `Join(Weak)`.
Idle coroutines cost one comparison per tick and no context switch (1,000 idle coroutines tick in
about 10 microseconds even in a debug build: `tests/coroutines.rs`; DESIGN §17.4 asks for under
0.1 ms in release). `task.tick` from inside a coroutine, or from inside a tick, panics.

**Order inside a tick** is start order, not wake-time order. **A panic** in a coroutine ends only
that coroutine; the rest of the tick still runs; `tick` then returns the first failure (the others
become notes, "another coroutine also panicked"). From Rust this is `Err(Error::Panic)` (or
`BudgetExhausted`) and the `Vm` stays usable; from `task.tick` it is a panic of the caller. The
trace is the coroutine's own stack, so it names the coroutine's function at the bottom. A failure
while a coroutine is *starting* (before its first pause, inside `go`) or being cancelled is queued
and reported by the next tick, or by the event loop / `Vm::run_event_loop` at its end.

### Native stacks

`corosensei::DefaultStack` (guard page, lazily committed). Default **16 MiB** per coroutine
(`VmBuilder::coroutine_stack_size`), chosen after measuring recursion depth inside a coroutine:
the evaluator uses several KiB of native stack per Colibri call (roughly 4–8 KiB in a release
build, more in debug), so 1 MiB allowed fewer than 300 nested calls in release and fewer than
100 in debug, too shallow for a general-purpose language. 16 MiB gives about 1,000–2,000 in
release (comparable to Python's default recursion limit); the main fiber under the CLI keeps its
240 MB budget. The space is only reserved: 20,000 coroutines start in about 0.3 s in a release
build and commit only the pages they touch. The number of live coroutines is bounded by the OS
mapping limit (`vm.max_map_count`, about 30,000 on default Linux). The bytecode VM (phase 3)
removes the per-call native stack cost. Each resumption sets `Interp::native_stack_budget` to three
quarters of the stack (the rest is for built-ins and host functions, which are not measured) and
restores the resumer's on return, so nested resumption (`go` inside a coroutine) works; the
coroutine's own `Fiber` sets its `stack_base` when the body starts. Overflow is the usual clean
"stack overflow" panic.

### Soundness notes

- The `Interp` is boxed in the `Vm` (`Box<Interp>`) because suspended stacks hold `&mut Interp`
  into it; it never moves. Resume and suspend pass the pointer through the switch, which is
  enough for the compiler not to cache across it. No Rust panic crosses a coroutine boundary on
  purpose (host panics are caught in the trampoline as before); no coroutine is ever dropped
  while suspended (that would force an unwind): shutdown cancels first and leaks the stack of
  anything left (unreachable in practice).
- `wait` panics: on the main fiber ("can only pause a coroutine"), with `host_depth > 0` ("a
  host function's callback"), and inside a `Drop` hook. Suspending while a `for` loop holds a
  collection keeps its mutation guard held, as for any long loop.

### Instruction budget

A start shares its starter's budget (the first run is part of the starter's work). Every
resumption from a tick gets a **fresh allotment** (the budget is swapped in and out around the
resume), so one coroutine's endless loop is caught on its own: `BudgetExhausted` from `tick`,
with the usual grace for `defer`s. A cancellation at shutdown also gets one each.

### Cancellation

`c.cancel()` on a suspended coroutine **unwinds it at once, synchronously**: it is resumed with a
cancel signal, its pause returns `Err(Ctl::Cancel)`, and `exit_scope` runs its `defer`s and drops
in the usual order; when `cancel()` returns the coroutine is finished. Cancelling a finished
coroutine is a no-op, twice is fine. A coroutine cancelling **itself** stops immediately (`Ctl::Cancel`
from the `cancel()` call, defers run); cancelling a coroutine that is mid-run higher up the
resume chain (a parent that started this one) only marks it, and it stops at its next pause.
While being cancelled a coroutine cannot pause again: a `wait` in a `defer` returns `Cancel` at
once (so the rest of that deferred statement is skipped, the remaining defers still run).
`exit_scope` treats a `Cancel` coming out of a deferred statement as the cancellation continuing
(it no longer turns into a "cancelled" panic). A panic in a `defer` during cancellation is a
failure of that coroutine (queued as above). A cancelled coroutine's `join` panics.

### The end of the program

- **`colibri run` does not cancel coroutines when `main` returns.** It runs the **event loop**
  (`Vm::run_event_loop`) until no coroutine is left: tick with the time elapsed on the clock,
  then sleep until the next timed wake (`Time`/`Real`); coroutines that must be checked at every
  tick (`wait_until`, `wait_frames`, `join`) are polled every 1 ms (`run_event_loop_polling`).
  A coroutine panic ends the loop with that panic, so the program exits 101 like a panic in
  `main`. `fn main` returning does not end a program that still has coroutines; `os.exit` does.
  Scheduler time under the loop is real time, so `wait(1.0)` is one second there.
- Embedding hosts drive `vm.tick(dt)` themselves; `run_main` does not cancel or loop.
- **Shutdown** (`Vm::shutdown`, drop, a panic in `main`, a failed load, `release_globals`
  between tests) first **cancels every live coroutine, newest first** (reverse start order, the
  scope-exit convention), each unwinding with its defers and drops, **then** releases the roots,
  then the module-level values in reverse initialisation order. A `go` executed during that
  teardown starts nothing (its handle is already cancelled). `colibri test`'s per-test
  `release_globals` does it too, so no coroutine leaks into the next test; coroutine panics
  nobody collected are discarded there.
- The golden harness runs the same loop after `main`, with an injected fake clock whose `sleep`
  just moves it, so tests never sleep; `import testclock` gives `testclock.advance(seconds)`
  (a named host module registered only by the harness) to move the clock for `wait_real` under
  `task.tick`.

### Decisions the design documents did not fix

`go` is an expression; `Coroutine[T]` with a value-returning `join`; `task` is an intrinsic
namespace; `wait_until` checks at once and polls on the coroutine's stack; start-order ticks;
panics reported by `tick` and joiners; synchronous cancellation; self-cancel immediate; no waiting
while being cancelled; fresh budget per resumption; 16 MiB stacks; event loop after `main` in
`colibri run`; shutdown cancels newest first before module values die.

### Not built

Channels' `recv` pausing (DESIGN §14.2 lists it next to `wait_for`; it comes with channels).

## 16.1. `CoGroup`, `Signal` and `wait_for` as built

PHASE2 step 2, in `coro.rs` unless said otherwise.

### Types and surface

- `CoGroup` and `Signal` are **prelude classes** like `Coroutine[T]` (`class CoGroup {}`,
  `class Signal {}`; `COGROUP_TYPE` = 13, `SIGNAL_TYPE` = 14, `FIRST_USER_TYPE` = 15). Reference
  types with identity equality, usable in fields, parameters and collections; `Show` prints
  `<CoGroup>` and `<Signal>`; `// drops:` traces skip them. Each object has one hidden field, a
  unique id that the scheduler's tables are keyed by. `CoGroup()` and `Signal()` are checker
  special forms (`check_group_or_signal_new`, next to `List[T]()`); they take no arguments.
  Signals carry no payload (see decisions).
- `group.go(call(args))` is a checker special form (`check_group_go`, in `methods.rs` before
  inherent-method lookup): a method call named `go` on a `CoGroup` whose single argument is a
  **call expression**. It checks the call exactly as `go` does (`check_go_call`, same restrictions
  and errors, e.g. no `var self` receiver) and gives HIR `ExprKind::GoIn(group, call)` of type
  `Coroutine[T]`. The group is evaluated first, then the arguments, then the call runs to its first
  pause. `go` is a keyword, but the parser already accepts keywords as member names after a dot
  (`parse_member`), so `door.co.go(...)` needed no parser change. Anything else as the argument
  (`group.go(value)`, no argument, two arguments) is "`go` on a `CoGroup` needs a call to start"
  with a hint to pass the call itself and, to run arbitrary code, to wrap it in a closure.
- `group.cancel()`, `group.count() -> int` (live coroutines of the group), `group.join_all()`;
  `signal.emit()`; `wait_for(signal)` is a builtin function (`Intrinsic::WaitFor`, reserved name).

### Groups

The scheduler keeps `groups: HashMap<group id, Vec<coroutine id>>` (start order) and each `Entry`
knows its group. A coroutine that finishes, panics or is cancelled leaves its group
(`leave_group`), so `count()` is exact. **A group dies when its last reference does**: `dismantle`
(release.rs) calls `group_died` before anything else, which removes the group's list and cancels
its members **newest first** (reverse start order, the scope-exit convention), each unwinding with
its `defer`s and drops before the release that killed the group returns. A member that is running
higher up the resume chain (the group died inside it, or it started the code that dropped the
group) is only marked and stops at its next pause, as with `Coroutine.cancel`.

- `cancel()` does the same without killing the group: the group stays usable and new coroutines
  can be started on it afterwards. A member cancelling its own group cancels the others first,
  then stops itself at once (`Ctl::Cancel`).
- `join_all()` pauses the calling coroutine until **no other coroutine of the group is alive**,
  including ones started while it waits (it is polled once per tick, like `join`; `Wake::Group`).
  A member calling it waits for the rest, not for itself. If nobody else is alive it returns
  at once (also on the main fiber); otherwise outside a coroutine it panics like `wait`. It does
  not report members' panics (those surface through `tick` as ever).
- **Strong references** (DESIGN's warning): a suspended coroutine holds strong references to its
  arguments and locals. `door.co.go(open(door))` therefore keeps `door` (and so its group) alive
  until the coroutine ends, and the group's drop never fires. Pass what the coroutine needs
  (a name, a weak reference, a handle to a separately owned object), or keep the group in an owner
  that something else owns. `tests/golden/coroutines/group_owner_object.colibri` does the former.
- Starting during shutdown starts nothing (as for `go`); `cancel_all_coroutines` clears the tables.

### Signals and `wait_for`

- `wait_for(s)` pauses the running coroutine (same checks and panics as `wait`: not on the main
  fiber, not in a host callback, not in a `Drop` hook) with `Wake::Signal(id)` and appends the
  coroutine to `signals: HashMap<signal id, Vec<coroutine id>>`. The waiting coroutine keeps the
  signal object alive while it waits, so a waiter's signal cannot die under it.
- **Event waits cost nothing per tick.** A `Wake::Signal` coroutine is never due in `tick_core`,
  it is not resumed, and `next_wake` ignores it. The tick still walks the entry list (one
  comparison per entry, like a timed waiter), but no resumption, no closure call and no polling
  happens: `tests/coroutines.rs::signal_waiters_cost_nothing_per_tick` starts 1,000 waiters,
  ticks 100 times and asserts the resumption counter (`Vm::coroutine_resumptions`) did not move,
  then that one `emit` costs exactly 1,000 resumptions at the next tick.
- **`emit()` wakes at the next tick, not immediately.** It takes the signal's waiter list, marks
  each still-suspended waiter `Wake::Signaled` and appends it to `sched.signaled`, in wait order.
  Nothing runs inside `emit`, so there is no reentrancy: the emitter finishes its step, and a
  waiter that waits again waits for the *next* emit. At the start of the next `tick`, the
  `signaled` queue is drained first (resumption in emission order, then wait order inside one
  emit), then the remaining due coroutines in start order; a coroutine already resumed from the
  queue in that tick is not resumed again (`Entry::ticked`). An emit during a tick wakes at the
  following one. An `emit` with no waiter does nothing (signals do not remember emits), and a
  wait started after an emit does not see it.
- **Cancellation** of a waiter (`Coroutine.cancel`, group cancel or drop, shutdown) removes it
  from the signal's list in `resume_coro`; a coroutine cancelled after being woken but before
  it ran is cancelled and its queue entry is skipped.
- **The end of the program.** `run_event_loop` and `colibri run` treat coroutines that only wait
  for a signal as not keeping the program alive: `next_wake` returns `None` when every live
  coroutine is a signal waiter, so the loop returns, `main`'s program ends and shutdown cancels
  the waiters (newest first, unwinding their defers and drops). Nothing in a single-threaded
  program can emit then. A host that can emit from outside the program (a host function called
  between ticks, or a Colibri function it calls that does `s.emit()`) drives `vm.tick` itself, or
  calls `run_event_loop` again after emitting; `coroutine_count()` still counts signal waiters.
  A coroutine woken but not yet resumed (`Wake::Signaled`) does keep the loop going.

### Decisions the design documents did not fix

`Signal` has no payload (no `Signal[T]`: `emit(v)` returning the value from `wait_for` would need
a generic prelude class and a value slot per waiter; it can be added without changing the rest);
`emit` is next tick; woken waiters resume before other due coroutines, in emission order; a
signal does not buffer emits; `join_all` polls and ignores itself; group `cancel` leaves the
group reusable; the event loop does not wait for signal-only programs; hosts emit by calling a
Colibri function that emits (no Rust-side `Signal` handle yet).

## 17. Hot reload as built

PHASE2 step 5 (DESIGN §16.3, §15.4). The pieces: `colibri-runtime/src/reload.rs` (change
detection, fingerprints, outcome types), `Vm::reload_changed` and friends in `vm.rs`,
`Interp::patch` in `colibri-interp/src/eval.rs`, and `colibri run --watch` in
`colibri-cli/src/watch.rs`. The wording is general: any long-running program, server, tool or
embedded script can use it.

### Surface

```text
vm.reload_changed()                     -> Result<ReloadOutcome, Error>
vm.reload_changed_with(ReloadOptions { force_restart })
vm.run_event_loop_watching(poll, max_sleep, |vm| ..)   // a hook between ticks
enum ReloadOutcome { Unchanged, Patched { functions }, Restarted { reason, state_carried },
                     CheckFailed(Report) }
```

### Which files, and how a change is noticed

- Only programs loaded from disk (`load_file`, `load_package`, `load_path`) can reload;
  `load_source`/`load_sources`/`load_tests` programs report `Unchanged` forever. `Vm::shutdown`
  forgets the files.
- The files are exactly the ones the loader **read** (a `RecordingFs` wraps the `SourceFs`):
  every module file and every `colibri.toml`, dependencies included, as absolute paths with a
  hash of the text the checker saw. A load that fails (syntax or check errors, an unreadable
  file) still records them, so fixing the file loads the program.
- Polling is `std::fs::metadata` (mtime and length) per file; only a file whose metadata moved
  is read and hashed, and a changed hash is what counts (touching a file or saving it
  unchanged is `Unchanged`). Because timestamps are coarse, a file whose mtime was within 2 s
  of being read is compared by **content on every poll** until it has aged (git's "racy"
  rule); an edit of the same length in the same clock tick is therefore never missed. No
  file-watcher dependency; a new file that would make a failed import resolve is only seen
  when a watched file changes (directories are not watched).

### The decision: patch or restart

The new sources are loaded and checked as a whole program into a fresh `Program` (the old one
is untouched), then compared with the running program by a **fingerprint**: hashes of the
`Debug` text of the HIR with every `Span { .. }` removed (so moving code, comments and
reformatting change nothing), in parts:

1. functions: the table of `(name, signature, type parameter count, host id)` in order, and
   `main`;
2. structs, classes, enums and interfaces (`Program::types`);
3. interface implementations (`impls`);
4. module-level declarations: `(name, type, mutability)` of every global and the init order;
5. module-level initialisers and the top-level statements (their bodies);
6. the host interface (`host_fns`, `host_types`) and the `test` blocks.

All parts equal means the two programs allocate identical ids: `FnId`, `GlobalId` and
`TypeId` are positions in these tables, so a body that mentions `FnId(7)` means the same
function in both. Then the change is a **patch**: `Interp::patch` replaces `fns` (every body,
also the unchanged ones, so spans refer to the new `SourceMap`), the type and dispatch tables
and the stored init bodies, and the `Vm` replaces its `Program`, sources and warnings. The
epoch does not move, so `Fn` handles and callbacks stay valid. `functions` counts the bodies
whose own fingerprint changed; a patch with none (only comments or spacing) is reported as
`Unchanged`. Anything else, including a changed module-level initialiser or top-level
statement (it would not re-run), is a **restart**, and `reason` names the first differing
part.

Cost: one `Debug` pass per function body of the new program, hashed without building a big
string; the running program's fingerprint is computed at the first reload and kept.

### Patch semantics

- Module-level initialisers and top-level statements do not re-run; module values, class
  objects and `var`s keep their state.
- A call made after the patch uses the new body (the interpreter looks up `fns[FnId]` at call
  time). A coroutine suspended in an old body holds that `Rc<Body>` and finishes on it; calls
  it makes after resuming use the new table. A closure created before the patch keeps its old
  body.
- Stack traces of old, unchanged-in-place bodies still point at the old spans, resolved
  against the new sources, so a line number in a trace from a coroutine still on old code can
  be off. (DESIGN: "the editor shows which coroutines are still on old code" is not built.)

### Restart semantics

1. If the old program defines `fn save_state() -> str`, it is called first. A panic there, or a
   `save_state` with another signature, aborts the reload with `Err` and the old program keeps
   running (the edit counts as seen; edit again to retry).
2. All coroutines are cancelled (newest first, running `defer`s and drops), the roots and
   module values are released, exactly as `shutdown` does, and the epoch moves (handles and
   callbacks from before are `StaleHandle`).
3. The new program is installed: module initialisers and top-level statements run. A panic
   there is `Err(Panic)`; the `Vm` has no program until the next edit, which loads again.
4. If a state text was saved and the new program defines `fn restore_state(s: str)`, it is
   called; `state_carried` says so. Without a `save_state` in the old program, or a
   `restore_state` in the new one, the text is dropped (no error).
5. `main` is **not** called: `Restarted` tells the host, which decides. The CLI calls it.

The host's registered functions and types, capabilities, the instruction budget, the clock,
the output writer and `insert_state` values live in the `Vm`, not the program, so they all
stay in place.

### Check errors

If the new text does not parse or check, nothing changes and `CheckFailed(Report)` carries the
diagnostics. The attempt is remembered as seen: the same broken text is reported once, and the
next `Unchanged` comes until a file changes again. An I/O error (a file vanished while an
editor replaced it) is `Err(Error::Io)` and is **not** remembered, so the next call retries.

### `colibri run --watch`

`main`, then the event loop run as usual, but the loop is `run_event_loop_watching` with a hook
that calls `reload_changed` between ticks and never sleeps longer than 150 ms. A patch prints
`colibri: patched N functions`; a restart prints `colibri: restarted (reason[, state carried])`
and calls `main` again; check errors print the diagnostics and `keeping the old program`.
After `main` and the loop end, or after a panic (printed, exit code not used), the tool prints
`waiting for a change`, polls every 150 ms and **restarts** (forced) on the next edit, then
runs `main` again. `os.exit` still ends the process. A program that fails to check at start
waits the same way.

Safe points are therefore: between event-loop ticks, and when the program is idle. A `main`
that never returns and never yields to the scheduler cannot be reloaded: patching inside a
running call would need a re-entrant safepoint check from the instruction-budget machinery,
which is not built.

### Measured

A generated program of 1,000 functions (about 12,000 lines), one body edited, second reload
(running program already fingerprinted): about 50 ms in a release build, 170 ms unoptimised
(`tests/reload.rs`, DESIGN §17.4 asks for under 200 ms). The first reload costs about twice that
(it fingerprints the running program too).

### Decisions the design documents did not fix

- **Snapshot is text, not §7.5.** Reflection and serialization (DESIGN §7.5) are not built, so
  the hooks are `fn save_state() -> str` and `fn restore_state(s: str)`; programs encode with
  `json` or any format they like. The names and the "optional pair" semantics are DESIGN's.
  This is a deviation to revisit when §7.5 exists (a `Snapshot` type could then replace `str`
  without changing the rest).
- A module-level initialiser or top-level statement change is a restart (DESIGN says
  initialisers do not re-run; silently keeping the old value would be surprising).
- Check errors are an *outcome* (`CheckFailed`), not `Err`, so `vm.reload_changed()?` in a
  host's frame loop does not abort the host over a typo.
- `Restarted` does not call `main`; the CLI does. After an ended or panicked CLI run the reload
  is always a restart, so a re-run starts from fresh state.
- Fingerprints use `Debug` text with spans stripped, which needed no HIR changes. It is
  conservative: a difference it cannot tell apart from a real one leads to a restart, never to
  a wrong patch.

### Not built

Reload from inside a call (safepoint in the evaluator); a list of coroutines still on old code;
watching directories; reloading `load_sources`/`load_tests` programs; a `Snapshot` type
(§7.5).

## 18. Leak detector as built

PHASE2 step 6 (DESIGN §11.5, Q9), in `colibri-interp/src/leaks.rs`. Colibri counts references, so
a cycle of class objects, or of closures and the variables they capture, is never freed. The
detector finds such cycles by trial deletion (Bacon and Rajan, run synchronously). It is a
diagnostic: it reports and frees nothing.

### Surfaces

- `VmBuilder::track_leaks(true)` turns the registry on; `Vm::find_leaks() -> Vec<Leak>` can be
  called at any time (mid-run, after `shutdown`, between ticks). Without `track_leaks` the
  result is empty.
- `colibri run --leaks[=warn|error]`: at the end of the program (after `main` and the event
  loop, then `Vm::shutdown`: coroutines cancelled newest first, roots and module-level values
  released) the detector runs and the report goes to stderr. `--leaks` alone is `warn`: the exit
  code is unchanged. `--leaks=error` exits 1 when the program would otherwise have exited 0 (a
  panic keeps 101, `os.exit(n)` keeps `n`).
- `colibri test --leaks[=warn|error]`: after each test (which has already released its module
  values and cancelled its coroutines) the detector runs, and only the cycles that are new since
  the previous test are attributed to it. `warn` prints them to stderr; `error` fails that test
  with the report.
- Golden tests: every golden test runs with tracking on and must end with no leak, unless the
  test says `// leaks: Node, Node` (class names, `closure` for a closure, `Name xN` for N of
  them; compared as a multiset; read from `main.colibri` in a folder test). PHASE1 has the
  directive table. One existing test leaks on purpose
  (`closures/strong_self_capture_leaks_a_cycle`) and now says so.

### The registry

While tracking is on, every class object and closure is entered in a registry (a `Vec` of weak
references with the span of the literal or `.clone()` that made it) when it is created: the `Object`
expression, closures, and the copies a `clone()` makes. Cells, lists, maps and structs are not
entered: a cycle always passes through a class object or a closure, and everything else is found
by following edges. Host objects (`Value::Host`) and values the host holds are opaque, so they
count as outside references. Objects made before tracking is on are unknown, which is why the
switch is a `VmBuilder` option and not a runtime toggle.

A `Weak` keeps the allocation of a dead object, so the registry drops dead entries whenever it
doubles (amortised constant per object; the threshold follows the live count). `Rc::try_unwrap`
replaced `Rc::get_mut` when releasing a closure, because a weak reference makes `get_mut` fail.

With tracking off there is no registry: creating an object costs one `Option` test. Measured
on a release build allocating 1.5 million objects and closures in a loop: baseline (before this
change) 1.25-1.30 s, tracking off 1.22-1.28 s (no measurable difference), tracking on
1.32-1.36 s (about 5%).

### The algorithm

1. Every live registry entry becomes a node (one handle kept per node). Exploring from them adds
   every reference counted thing reachable: class objects, lists, maps, sets, closures, cells,
   iterators, and the structs, tuples, enums and interface boxes that can hold them. Each node
   records its outgoing strong edges (field, element, key, value, capture, payload, through).
   `weak` fields and weak captures hold a `Weak`, not an `Rc`, so they are not edges.
2. **Trial deletion.** For each node, `Rc::strong_count - 1` (the detector's own handle) minus
   the edges that come from other nodes is the number of references from *outside the graph*:
   frames, temporaries, globals, host handles and roots, suspended coroutines, Rust locals. A node
   with a positive remainder is live, and so is everything it reaches (a worklist from those).
3. What is not live is held only by other garbage. Tarjan's algorithm (iterative, so a long
   ring cannot overflow the stack) finds its strongly connected components; each component that
   has an edge cycle (two or more nodes, or one node with a self edge) is one `Leak`. The garbage
   that hangs off a cycle without being on one is `retained`, counted once, for the first cycle
   that reaches it.

No root set is enumerated. Anything the detector cannot see is an outside reference, so the
detector can *miss* a leak (for example while a Rust value holds a handle) but cannot report an
object that is still reachable. This is also why suspended coroutines need no special case: their
frames hold ordinary references, so a cycle a suspended coroutine uses is live, and it becomes
a leak once the coroutine is gone. The cost is linear in the number of live objects and edges
(a 10,000-object ring, whole program included, runs in about 60 ms in a release build; the golden tests `leaks/large_ring` and
`leaks/many_small_cycles` have 10,000 each).

### The report

`Leak { objects, path, sites, retained }`: the class of each object in the cycle (sorted),
one shortest cycle through the oldest object rendered as
`Node(name: "a") -> next -> Node(name: "b") -> next -> Node(name: "a")`, where the objects were
made (`file:line`, up to four), and how many more objects only this cycle keeps alive. Lists,
maps, structs and cells on the way are folded into the label (`children[0]`, `items["x"]`,
`captures button`); an object that has a `name: str` field is shown with it; an `Option` field
reads as the field. A path over ten hops is abbreviated (`... (9991 more) ...`).

### `weak`

A `weak` reference is not an edge: it does not keep its target alive in the reference counts,
so it neither forms nor joins a cycle, and it is never followed. A cycle broken with `weak` on
one link is not reported (`leaks/weak_breaks_the_cycle`). A weak capture is the same.

### Decisions the design documents did not fix

- Report only: no `collect`. Freeing a cycle means running `Drop` hooks on garbage in an order
  nobody chose, and closing the cycle by hand is a one-line fix once the report names it;
  `mem.collect_cycles()` stays a later option on top of this graph.
- Opt in. DESIGN §11.5 says the detector is on by default in `colibri run`; it is `--leaks`
  instead, because the registry costs about 5% of allocation-heavy code and a program that
  works should not start printing to stderr after an upgrade. Flipping the default is one
  line in `main.rs`.
- `--leaks` defaults to `warn`; `error` is opt in (exit 1 only when nothing else failed).
- The golden harness fails any test that leaks without declaring it.
- `mem.report_cycles()` (DESIGN §11.5) is not built; embedding hosts have `Vm::find_leaks`.
- Allocation sites are the span of the literal; a leaked object made by the host, a `Coroutine`
  or an `Error` has none. Only class objects and closures are named; a cycle made only of lists,
  maps and cells is not possible without one of them.
- Leaked cycles stay leaked until the process exits; their `Drop` hooks never run.

### Not built

`mem.report_cycles()`, `mem.collect_cycles()`, a leak report for cycles through host objects.

## 19. C FFI as built

PHASE2 step 7 (DESIGN §15.2, §15.3, Q13). A program can declare C functions in `extern "C"`
blocks and call them with numbers, strings, raw pointers and `@repr(C)` structs by value. C
calling Colibri (callbacks, function pointers) is not built, and neither is `colibri bindgen`.

```colibri
@repr(C)
struct DivT { quot: i32, rem: i32 }

@link("c")
extern "C" {
    fn div(num: i32, den: i32) -> DivT
    fn getenv(name: cstr) -> cstr?
    fn malloc(n: u64) -> ptr[u8]
    fn free(p: ptr[u8])
}
```

### Crates

- `colibri-ffi` (new): knows nothing of Colibri values. `CType`/`CStruct` (a type with the
  offsets, size and alignment the caller computed), `Val` (a value on its way in or out),
  `Library`/`Libraries` (dlopen, below) and `Function` (a symbol with a prepared libffi `Cif`).
  `Function::new` builds the libffi struct type and *checks the caller's layout against the one
  libffi computes*; a disagreement is an error at load time. Reading and writing raw memory
  (`read_memory`, `write_memory`, `read_cstring`) lives here too, so every `unsafe` block of the
  feature is in this crate and `colibri-interp/src/cffi.rs`.
- `colibri-ffi-testlib` (new, not published): `build.rs` compiles `c/testlib.c` into a shared
  library with the compiler the `cc` crate finds, and exports its path as `PATH`. The C library
  has scalars of every width, strings in and out, pointers, and structs chosen to exercise ABI
  classification (two ints in one register, two doubles, two floats in one SSE register, an
  int plus a double, a double plus a packed int/float/byte tail, 32 bytes through memory, a nested
  struct, padding, a ladder of growing widths) and `ct_layout(type, field)`, which reports
  `sizeof`, `alignof` and `offsetof` as the C compiler sees them.
- `colibri-check` gets `ffi.rs` (attributes, layout, `extern` blocks, `ptr` and `unsafe`);
  `colibri-interp` gets `cffi.rs` (binding, conversion, pointer operations); `colibri-runtime`
  binds the functions when a program loads.

### Dependencies and what they need to build

- `libffi` 5 with its bundled `libffi-sys`: builds libffi from its own sources with the `cc`
  crate. It needs a C compiler and **no** autotools, no system libffi and no `pkg-config`. (It was
  built here with gcc; `libtool` was not installed and did not matter.)
- `libloading` 0.8: `dlopen`/`LoadLibrary`.
- `cc` 1 (build dependency of `libffi-sys` and of the test library).
- Cargo feature `ffi`: `colibri-interp/ffi` (optional dependency on `colibri-ffi`),
  `colibri-runtime/ffi` (default on), `colibri-lang/ffi` (default on, forwards to the runtime).
  `colibri-std` and `colibri-lsp` depend on the runtime with `default-features = false`, so an
  embedder that does the same gets no libffi at all. Without the feature the interpreter's
  `bind_extern` returns an error that says so and the program fails to load (`Error::Ffi`); the
  checker still accepts `extern` blocks, because its job does not depend on the build.

### Types

- **`ptr[T]`** is `Ty::Ptr(T)`, a value `Value::Ptr(usize)`, shown as `0x7f...`. It has `==`,
  `Hash`, `Show`, can be stored in structs, collections and `@repr(C)` fields, and has no order.
  `T` is `()` (a `void *`), a C number, `bool`, another pointer, or a `@repr(C)` struct (which may
  be empty, to name an opaque handle: `@repr(C) struct Sqlite3 {}` and `ptr[Sqlite3]`).
- **A `ptr[T]` is never NULL.** NULL is `none`, in the type `ptr[T]?`. At the boundary (a
  parameter, a return, a `@repr(C)` field, a `read`) `ptr[T]?` maps NULL to `none`, and a NULL
  arriving where plain `ptr[T]` was declared is a panic that says to declare `ptr[T]?`. The
  language has no way to make a pointer out of an integer, so a valid-looking pointer only comes
  from C.
- **`cstr`** exists only in the signature of an `extern` function (`cstr` anywhere else is an
  error). It *is* a `str` to Colibri (`Ty::Str`); the declaration remembers it was written
  `cstr`. `cstr?` is `str?`: `none` is NULL, and a `str` is *not* implicitly `some` (Colibri has no
  such conversion), so a nullable parameter is called with `some("x")`.
- **Integers:** `i8..i64`, `u8..u64`, `int` (= `i64`), `float` (= `f64`), `f32`, `bool` map to the
  C types of the same width (`int` is `int64_t`, not C `int`: use `i32` for that). `size_t` is
  `u64` on the 64-bit targets Colibri runs on. `char` is not a C type. A unit return is `void`.
- **Not C types, with an error that says so:** `str` (write `cstr`), `char`, collections, tuples,
  options other than `ptr[T]?`, classes, plain structs (mark them `@repr(C)`), interface values,
  functions, generics. A fixed-size array `[T; N]` does not exist in the language yet
  ("fixed-size array type is not supported yet"), so DESIGN §15.2's `color: [u8; 4]` waits for it;
  declare the four fields, or a `ptr[u8]`. Vectors (`vec3`, §15.1) are not built either.

### `@repr(C)` structs

`@repr(C) struct` is an ordinary Colibri struct (literals, fields, copies, `impl`, derived `Eq`
and `Show`) plus a layout: the fields in order, each at the next multiple of its alignment, the
whole padded to the largest alignment (so `u8, u32, u16` is 12 bytes, align 4). Allowed fields are
the C types above, other `@repr(C)` structs by value, `ptr[T]` and `ptr[T]?`; anything else is an
error at the field. A `cstr` field is refused (it would outlive the copy made for a call): use
`ptr[u8]`. Not allowed: generics, classes, `@repr(packed)` or any other `@repr`. A struct with no
fields is allowed (size 0), cannot be passed by value, and is the way to name an opaque handle.
The layout is computed once by the checker (`Program.c_layouts`) and handed to libffi, which
re-computes it and must agree. `size_of[T]()` and `align_of[T]()` (builtins, folded to an `int`
literal at check time) show it; the tests compare them with the C compiler's.

### Extern blocks

`extern "C" { fn name(params) -> ret }` with optional `@link("lib")` on the block. The parser
already accepted the syntax; the checker now turns each function into a *host function with a C
signature*: it gets a `HostFnId` after the host's own, `HostFnDef.ext = Some(ExternDef { lib,
symbol, params, ret })`, requires `ffi`, and a call is an ordinary `Callee::Host`. So tracing,
call depth, the instruction budget, `go`, hot-reload fingerprints and the "a host function is not
a value" rule all apply with no new code. The function name is the C symbol (there is no
`@symbol` rename). Errors: a body, generics, defaults, `self`, an ABI other than `"C"`, a second
`@link`, any attribute but `@link`, a type that is not a C type, an empty struct by value.
Variadic functions (`printf`) are not supported.

**Calling needs no `unsafe`** (DESIGN §15.3: the capability guards the declaration). The
declaration is trusted: a wrong signature corrupts memory as it would in C.

### Capability

A program with an `extern` block needs both of:

1. **The host's `Cap::Ffi`** (`VmBuilder::allow(Cap::Ffi)`; the interface name is `ffi`). Without
   it the checker reports once, at the block: "`extern` blocks need the `ffi` capability, which
   this host did not grant". The functions of a refused block still exist (poisoned), so uses do
   not add errors. `colibri run`, `check`, `test` and the LSP grant everything.
2. **`ffi = true` in the `[package]` table of the package's `colibri.toml`**, for every package
   with an `extern` block, dependencies included: the program's own `ffi = true` does not cover
   them, and the error names the package ("the package `shim` cannot contain `extern` blocks: it
   has not declared the `ffi` capability"). A script with no manifest is the program itself and
   needs only the host's grant. (`ModuleMeta.ffi`; the TOML subset gained booleans, and
   `ffi = "yes"` is an error.) A manifest key was chosen over a `[capabilities]` table because
   DESIGN names exactly `ffi = true` and there is one capability; a table can come with a second.

### Libraries

`@link("name")` opens a library once per program (`Libraries`) and every function of the block
is looked up in it **at load time**, so a missing library or symbol is `Error::Ffi` from the
load and not a surprise on the first call (the cost: an `extern` block that is never used still
has to resolve; there is no conditional compilation yet to guard a platform-specific block).
Resolution, in order: a name with a `/`, a `\` or a library extension is a path, taken as is
(`@link("./libfoo.so")`); otherwise the platform's file name (`libNAME.so`, `libNAME.dylib` and
the Homebrew and `/usr/local` folders, `NAME.dll`), then on Linux the highest `libNAME.so.N` in
the usual library folders (so `libz.so.1` works without the development package), with `m`, `c`,
`dl` and `pthread` mapped to their versioned glibc names. For `c`, `m`, `dl`, `pthread`, `rt`
and `util` the symbols already in the process are the last resort, and an `extern` block with no
`@link` looks only in the running program, which is enough for libc. On Windows `c` and `m`
also try `ucrtbase.dll` and `msvcrt.dll`. Windows and macOS paths are written but untested here.

### Values and strings

- Calls copy: a `str` argument becomes a temporary NUL-terminated copy that lives for the call.
  **Deviation from DESIGN §12/§15.3:** strings do not carry a hidden NUL in the interpreter
  (`Value::Str(Rc<str>)`), so `cstr` is not free: it is one allocation and copy per string
  argument. A string with a NUL byte inside is a panic ("a string with a NUL character cannot be
  passed as a `cstr`").
- A `cstr` result is copied into a new `str` *before* the argument copies are freed (a function
  may hand its argument back). NULL is a panic for `cstr` and `none` for `cstr?`; bytes that are
  not UTF-8 are a panic ("the C string is not valid UTF-8"). The pointed-to memory is never freed:
  for a string C allocated, declare the result `ptr[u8]` and free it yourself.
- Structs by value go in and out through libffi (`Type::structure`), so classification (registers
  versus memory, the SSE/INTEGER eightbyte rules) is libffi's. Values are encoded into a byte
  buffer by the checker's offsets; returned integers are read through the full machine word
  libffi writes. A struct with a `cstr` field is refused.
- A C panic cannot be caught: a crash in C is a crash of the process. `print` output is flushed
  before each call so it comes before anything C writes, but C's own `stdout` buffer is separate
  (it flushes at exit when stdout is a pipe): call `fflush(none)` after `puts`.

### `unsafe` and the methods of `ptr[T]`

`unsafe { ... }` was already a keyword and a block expression; it is now an ordinary block that
raises `FnCtx.unsafe_depth`. A closure written inside starts again at zero (it may run later,
outside the block). The methods (a dispatch on `Ty::Ptr`, `Intrinsic::Ptr(PtrOp)`):

| Method | Needs `unsafe` | Meaning |
|---|---|---|
| `p.read() -> T` | yes | reads a `T` at the address (a struct too; `ptr[()]` has no `read`) |
| `p.write(v)` | yes | writes a `T` |
| `p.offset(n) -> ptr[T]` | yes | `n` elements (of `size_of[T]()` bytes) further; panics if the address would leave the address space or become 0 |
| `p.cast[U]() -> ptr[U]` | yes | the same address as another pointee type |
| `p.read_str() -> str` | yes | the NUL-terminated UTF-8 string at a `ptr[u8]` or `ptr[i8]` |
| `p.addr() -> u64` | no | the address |

Passing, returning, storing, comparing (`==`, `== none`) and printing a pointer are safe. There is
no address-of (`&x`) and no integer-to-pointer conversion, so pointers come from C: `malloc` and
`free` from `@link("c")` are how the tests make memory. There is no bounds checking and no use of
`ptr[T]` after `free` is detected; that is what `unsafe` says.

### Tests

- `crates/colibri-ffi/tests/testlib.rs`: `Function` against every C test function.
- `crates/colibri-runtime/tests/ffi.rs`: scripts against the test library (scalars, strings,
  pointers, every struct), `size_of`/`align_of` against `ct_layout` (size and alignment of
  twelve structs, offsets of the padded ones read back through raw memory), the capability checks
  (no `Cap::Ffi`, a package and a dependency without `ffi = true`, a script without a manifest),
  every declaration error, a missing library or symbol. `tests/no_ffi.rs` covers the build with
  the feature off (`cargo test -p colibri-runtime --no-default-features`).
- `tests/golden/ffi/`: libc and libm only (`strlen`, `abs`, `cos`, `sqrt`, `div` returning a
  struct, `malloc`/`calloc`/`memcpy`/`strchr` through `ptr[u8]`), the layout of `@repr(C)`
  structs, the errors, and a package with and without `ffi = true`. They avoid platform-specific
  symbols (`getpid`, `htons` and `strerror` text are the only ones that differ between systems
  and are used only through comparisons that hold on Linux and macOS); the `// skip:` directive
  is unconditional, so nothing is skipped and a Windows run would have to supply its own names.
- The LSP check (`colibri-lsp/tests/lsp.rs`) and `colibri fmt` need nothing new: attributes,
  `extern` blocks and `unsafe` already parsed and printed.

### Decisions the design documents did not fix

- Pointers are non-null, NULL is `ptr[T]?`'s `none` (above).
- `int` is `i64`, as everywhere; C `int` is `i32`, `long` is `i64` (LP64) and `size_t` is `u64`.
- Resolution is eager (at load); libraries are never unloaded.
- `ffi = true` lives in `[package]`; the host's capability and the package's declaration are both
  needed, and a script without a manifest is exempt from the second.
- `cstr` is a signature-only name for `str`; a returned `cstr` is copied (never owned or freed).
- `read`/`write`/`offset`/`cast`/`read_str`, `size_of` and `align_of` are the whole unsafe
  surface. There is no `offset_of` (the tests check offsets through memory) and no `alloc`.
- A C library's own state, threads and signal handlers are the program's business. The
  interpreter is single-threaded, and C is called on the thread that runs the script.

### Not built

Callbacks (a Colibri function as a C function pointer), function-pointer types, variadic
functions, `colibri bindgen`, unions, bit fields, `@repr(packed)`/`align`, fixed-size arrays in
structs (the language has none), `long double` and complex numbers, `@when(os == ..)` to guard a
platform-specific block, and a bytecode-VM path (the VM will call the same `colibri-ffi`).

## 20. Vector types as built

PHASE2 step 1 (DESIGN §15.1, §5.1). `vec2`, `vec3`, `vec4` (`f32` components) and `ivec2`,
`ivec3`, `ivec4` (`i32`) are plain values: constructors, component fields, swizzles,
component-wise operators and a small set of methods. `mat3`, `mat4` and `quat` are not built.

### Exposure: `import vec`

The types are built into the checker and the interpreter (`Ty::Vec(VecTy)`, `Value::Vec` and
`Value::IVec`), but their *names* live in the standard library module `vec`. Without `import vec`
the names `vec2` and friends are unknown (the error says to import the module), and they are not
reserved either: a program may declare its own `struct vec2`. `import vec` gives `vec.vec2`,
`import vec.{vec2, ivec2}` the bare names, `import vec as v` gives `v.vec2`. The mechanism:

- `std.vec` is an ordinary named host module (§11.7, §13), registered by `colibri_std::install`
  or, for hosts without the standard library, `colibri_runtime::install_vec`. It has two
  functions, `vec.from_angle(radians: f32) -> vec2` and `vec.angle(v: vec2) -> f32`; it exists
  so that there is a module to import.
- The checker adds the six types to the *type exports* of a host module called `std.vec`
  (`TypeEntity::Vec`), a third kind of type entity next to structs and aliases, with its own
  scope map `vec_types` (`"vec.vec2"`, or `"vec2"` after a names import). Imports, aliasing,
  clashes with user names and `can_see` need no other change.
- The text of a host interface has no imports, so while resolving a type in `HOST_FILE` the
  checker accepts `vec.vec2` .. `vec.ivec4` anywhere. `.cbi` files and `write_interface` therefore
  write `fn push(a: vec.vec3, b: vec.ivec2)`, and a script that never mentions `vec` can still call
  such a host function and use the result (`v.x`, `v + v` need no import; only *naming* the type
  does).
- A constructor call is checked in `check_constructor` when the callee is `vec.vec2` or a bare
  name bound in the scope's `vec_types` (and not shadowed by a local or a value).

### Representation

`Value::Vec(FVec { n, c: [f32; 4] })` and `Value::IVec(IVec { n, c: [i32; 4] })`, inline (the
`Value` stays 24 bytes), no reference count, no release work: a copy is a copy. HIR additions:
`Ty::Vec`, `TypeKey::Vec` (impls on vectors dispatch), `ValueProj::Comp(i)` (a component, always
the last step of a place path: `player.pos.x += 1.0` is an ordinary place with path
`[pos-field, Comp(0)]`, and `read_at`/`replace_at` of `place.rs` read and write inside the vector),
`BinOp::Vec(ArithOp, VecTy, VecShape)`, `UnOp::VecNeg`, `Coercion::FloatToF32`, and the
intrinsics `VecNew`, `VecSwizzle { from, lanes, len }` and `VecMethod`. `==` and `!=` are
`Operand::Structural`; ivecs are `Hash + Eq` (a `Map[ivec2, Tile]` works), float vectors only `Eq`;
none is ordered. All the arithmetic lives in `colibri-interp/src/vecmath.rs`.

### Decisions the design documents did not fix

- **Format.** `print(vec2(1, 2.5))` shows `vec2(1.0, 2.5)`: the type name and each component as
  `f32` shows (shortest text that reads back, always with a `.0`), `nan` and `inf` as for floats.
  `ivec2(3, -4)` shows integers. A format spec on a vector is an error (format the components).
- **Literals and scalars.** A component, the scalar of `v * s`, `s * v`, `v / s`, and `lerp`'s `t`
  take an untyped literal (it becomes `f32` or `i32`), an `f32` and, for float vectors, a `float`
  (narrowed with the new `Coercion::FloatToF32`, rounding to nearest, DESIGN §5.1). An `int`
  variable is not accepted for a `vec` (`vec2(n, 0)` needs `f32(n)`), and an `ivec` takes `i32`
  only (not `int`: `i32(n)`), because integer to float and integer narrowing stay explicit.
- **Constructors.** `vecN(x, y, ..)` with N components, `vecN(s)` (splat), and `vecN(w)` for `w`
  of the other kind and length: `ivec2(v)` truncates toward zero like `i32(x)` and panics on `nan`
  or a value out of range; `vec2(iv)` rounds to the nearest `f32`. The name of the target is the
  conversion, as for the number types, so there are no `to_ivec`/`to_vec` methods.
- **Operators.** `+ - * /` between two vectors of one type (component-wise, `*` is the Hadamard
  product), vector by scalar (`*` and `/`) and scalar times vector (`*`), unary `-`, and the
  compound forms. `%` is not defined, a scalar added to a vector is an error that suggests
  `v + vec2(1)`, and different vectors never mix. A compound assignment is a `Place` operation
  like any other.
- **`f32` precision.** Every float operation is done in `f32`, per component, in the written
  order, without fused multiply-add: `dot` is `((x*x' + y*y') + z*z')`, so a script gives the same
  digits on every machine. `len` is `f32::sqrt` of `len_sq`. `lerp(b, t)` is `a + (b - a) * t`
  (not `a * (1 - t) + b * t`; it hits `b` exactly only when the arithmetic allows).
- **`normalize` of the zero vector is the zero vector** (not `nan`s), tested as a length of exactly
  `0.0`. A vector of `nan` stays `nan`.
- **Methods** (the whole set): `dot(o)`, `cross(o)` (3-component only; the error for 2D says to
  write `a.x * b.y - a.y * b.x`), `len()`, `len_sq()`, `distance(o)`, `abs()`, `min(o)`, `max(o)`
  (component-wise), and, for float vectors only, `normalize()` and `lerp(b, t)`. On an `ivec`,
  `dot`, `len_sq`, `cross`, `abs` return `i32` vectors or scalars and trap on overflow (`abs` of
  `i32.MIN` too); `len` and `distance` return `f32` (computed exactly in 64 bits, then rounded).
- **Integer vectors** trap like `i32`: `+ - *`, `-v` and `/` on overflow ("integer overflow"),
  division by zero ("division by zero"); `/` truncates toward zero.
- **Fields and swizzles.** The component letters are `x y z w`; a vector has the first `n`. Reading
  `v.xz`, `v.zyx`, `v.xxyy` gives a vector of that length (2 to 4 letters, repeats allowed, of
  the same kind). A swizzle is an rvalue: assigning to one is an error that suggests assigning
  the components. A component of a `let` binding, a parameter or a temporary cannot be assigned
  (the ordinary place rules).
- **Embedding.** `glam::{Vec2, Vec3, Vec4, IVec2, IVec3, IVec4}` are `IntoColibri` and
  `FromColibri` (the checker and the VM do not depend on glam; only `colibri-runtime` does and
  re-exports it as `colibri_runtime::glam`). `TypeDesc::Vec(VecTy)` prints as `vec.vec3`.
- **C FFI.** `CTy::Vec(VecTy)`: `n` consecutive `f32` or `i32`, alignment 4, so `vec3` is 12
  bytes and an `@repr(C)` struct may hold vectors; `ptr[vec3]`, `size_of[vec3]()` and by-value
  passing work (libffi sees a struct of `n` scalars). Whether a platform ABI passes such a struct
  in the same registers as a SIMD `float3` is not verified beyond the platforms the tests run on.
- **No `vec` in `json`.** `json.encode` of a vector is an error like other unsupported types.

### Not built

`mat3`, `mat4`, `quat`, swizzle assignment, `f64` and `u32` vectors, `Add`/`Mul` interfaces for
user types (Q12), `clamp`, `floor`, `reflect`, `angle_between` and similar methods (add them to
`vec_sig` in `colibri-check/src/vector.rs` and `vecmath::method`), a splat on `+` (`v + 1`).
