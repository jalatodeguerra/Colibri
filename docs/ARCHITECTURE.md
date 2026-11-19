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

Desugared by the checker:

| Source | HIR |
|---|---|
| `?` | `Match` + `Return` + `Coerce(IntoError)` |
| `or`, handler blocks | `Match`, the handler as an ordinary arm block |
| `if let`, `while let`, `while` | `Match` / `Loop` + `Break` |
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
   and `temps: Vec<Value>`. Rust locals never own a droppable Colibri value across a nested
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
- **Function handles:** `Fn<A, R> { id: FnId, epoch: u32 }`, `Copy`, not borrowing the `Vm`.
  `get_fn` checks `A` and `R` against the signature once; `call(&mut vm, args) -> Result<R, Error>`.
  Patching keeps the id; a restart bumps the epoch, and stale handles return `Err`.
- **Host functions:** `register_fn` derives a signature from the Rust closure type and stores
  `(name, Sig)` in the `HostEnv`; registration precedes `load_file`. The checker's only source of
  host declarations is a `&HostEnv`, produced either by live registration or by parsing a `.cbi`
  file (Colibri-syntax declarations; `write_interface` prints the same `HostEnv`). In HIR a host
  call is `Callee::Host(HostFnId)`.
- **Panics:** `Ctl::Panic` unwinds through `exit_scope` to the `call` boundary and becomes
  `Err(Error::Panic { message, span, stack })`. A Rust panic in a host function is caught with
  `catch_unwind` and converted to `Ctl::Panic`; budget exhaustion takes the same path. After an
  `Err` the `Vm` stays usable, with module state intact.

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
