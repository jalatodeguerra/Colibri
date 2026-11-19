//! The typed, name-resolved intermediate representation (ARCHITECTURE §1).
//!
//! HIR is the contract between the checker and every engine: names are resolved (locals are
//! frame slots, globals and functions are ids), every expression carries its type, and sugar
//! is gone. Bodies refer to each other only by id.

use std::rc::Rc;

use colibri_syntax::Span;

use crate::types::{FnSig, IntTy, ParamId, Ty, TypeId, VecTy};

/// A function in [`Program::fns`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FnId(pub u32);

/// A host function: an index into [`Program::host_fns`], which is the order of the
/// declarations in the host interface (EMBEDDING §1, §4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct HostFnId(pub u32);

/// A module-level value in [`Program::globals`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GlobalId(pub u32);

/// An interface implementation in [`Program::impls`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ImplId(pub u32);

/// A frame slot of one body: one per binding, never reused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LocalId(pub u32);

/// Identifies a loop within one body, so `break` and `continue` name their target.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LoopId(pub u32);

/// A field's index in declaration order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FieldIdx(pub u32);

/// A checked program.
#[derive(Debug)]
pub struct Program {
    pub fns: Vec<FnDef>,
    pub globals: Vec<GlobalDef>,
    /// Every global, in the order its initialiser runs: the ones ordered by dependency first,
    /// then (from [`Program::script_from`] on) the script's `let`/`var` in source order.
    pub init_order: Vec<GlobalId>,
    /// Where the globals the script initialises start in `init_order` (DESIGN §4.2): the script
    /// runs `StmtKind::InitGlobal` for each when it reaches its line. It is
    /// `init_order.len()` for a program with `fn main` (every value is initialised first).
    pub script_from: usize,
    /// The top-level statements of the file, in order.
    pub script: Rc<Body>,
    /// `fn main()` if the file defines it.
    pub main: Option<FnId>,
    /// Every struct, enum and class, indexed by [`TypeId`]. Index 0 is the prelude `Option`
    /// enum ([`crate::types::OPTION_TYPE`]), index 1 the prelude `Result`, then the built-in
    /// interfaces and `Error`; user types follow.
    pub types: Vec<TypeDef>,
    /// Every interface implementation: explicit `impl I for T` blocks and the derived
    /// implementations that were used (DESIGN §7.4). The engine dispatches `Callee::Bound`
    /// and `Callee::Dyn` calls through them.
    pub impls: Vec<ImplDef>,
    /// The host functions the program may call, indexed by [`HostFnId`].
    pub host_fns: Vec<HostFnDef>,
    /// How many types after the prelude's come from the host interface: the user's own types
    /// start at `FIRST_USER_TYPE + host_types`.
    pub host_types: u32,
    /// Every `test "name" { .. }` block of the program's own modules (not of the host or the
    /// standard library), in source order: the entry module first, then the other modules in
    /// load order (ARCHITECTURE §15). Their bodies are ordinary entries of [`Program::fns`].
    pub tests: Vec<TestDef>,
    /// The C layout of every `@repr(C)` struct (DESIGN §15.2), by type.
    pub c_layouts: std::collections::BTreeMap<TypeId, CLayout>,
}

/// A type at the C boundary (DESIGN §15.3): what an `extern` signature, a `@repr(C)` field or
/// the element of a `ptr[T]` may be.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CTy {
    /// No value: a function that returns nothing, or `ptr[()]` (`void *`).
    Void,
    Bool,
    Int(IntTy),
    F32,
    F64,
    /// `ptr[T]`, or `ptr[T]?` when `nullable` (NULL is `none`).
    Ptr {
        nullable: bool,
    },
    /// `cstr`, or `cstr?` when `nullable`. Only in an `extern` signature.
    CStr {
        nullable: bool,
    },
    /// A `@repr(C)` struct, by value.
    Struct(TypeId),
    /// A vector type: `n` consecutive `f32` or `i32` (12 bytes for a `vec3`, alignment 4), by
    /// value, as in glam's default layout (DESIGN §15.1).
    Vec(VecTy),
}

/// The layout of a `@repr(C)` struct on the platform's C ABI: sequential fields, each at the
/// next multiple of its alignment, the whole padded to the largest alignment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CLayout {
    pub fields: Vec<CTy>,
    pub offsets: Vec<usize>,
    pub size: usize,
    pub align: usize,
}

/// What an `extern "C"` function is: which library, which symbol, and its C signature.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExternDef {
    /// The `@link("name")` of its block; `None` looks in the running program.
    pub lib: Option<String>,
    pub symbol: String,
    pub params: Vec<CTy>,
    pub ret: CTy,
}

/// A `test "name" { .. }` block, compiled as a function `() -> Result[()]`.
#[derive(Clone, Debug)]
pub struct TestDef {
    /// The name written in the source.
    pub name: String,
    /// The module it is in: the file name without `.colibri` for the entry file, the module's
    /// import name otherwise.
    pub module: String,
    /// Whether the module belongs to the program's own package (not a dependency).
    pub root: bool,
    /// The function that runs the test.
    pub f: FnId,
    pub span: Span,
}

/// A function provided by the host program.
#[derive(Clone, Debug)]
pub struct HostFnDef {
    pub name: String,
    /// The host module the function belongs to (`host` for the default one).
    pub module: String,
    /// The function's entry in [`Program::fns`], which has an empty body and carries the
    /// signature and the name shown in stack traces.
    pub f: FnId,
    /// The capability the function needs (`fs`, `os`, `net`, `ffi`), if any.
    pub requires: Option<String>,
    /// For a function declared in an `extern "C"` block: the C function it calls. The host
    /// does not provide it; the runtime binds it to the library at load time.
    pub ext: Option<ExternDef>,
}

/// What a runtime value's type looks like to interface dispatch: the head of a type, without
/// type arguments, since the engine runs generic code with one uniform representation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TypeKey {
    Int(IntTy),
    Float,
    F32,
    Bool,
    Char,
    Str,
    Unit,
    Tuple,
    List,
    Map,
    Set,
    Range,
    Ptr,
    /// A vector type (`vec2`, `ivec3`, ..).
    Vec(VecTy),
    /// A struct, enum or class (`Option` is the enum at [`crate::types::OPTION_TYPE`]).
    Adt(TypeId),
}

/// `impl I for T`, or the derived implementation of a built-in interface.
#[derive(Clone, Debug)]
pub struct ImplDef {
    pub iface: TypeId,
    pub iface_args: Vec<Ty>,
    /// The implementing type; it may mention the impl's own type parameters.
    pub target: Ty,
    /// The head of `target`, which the engine dispatches on.
    pub key: TypeKey,
    /// One entry per interface method, in the interface's declaration order: the function
    /// that runs (the impl's own, or the interface's default). `None` for the methods of a
    /// derived built-in implementation, which the engine provides itself.
    pub methods: Vec<Option<FnId>>,
    /// Whether this is a derived implementation of a built-in interface.
    pub derived: bool,
    pub span: Span,
}

impl Program {
    /// The function with this source name.
    pub fn fn_by_name(&self, name: &str) -> Option<FnId> {
        self.fns
            .iter()
            .position(|f| f.name == name && f.host.is_none())
            .map(|i| FnId(i as u32))
    }
}

/// A user-defined type (ARCHITECTURE §1: bodies refer to types only by id).
#[derive(Clone, Debug)]
pub struct TypeDef {
    pub name: String,
    pub span: Span,
    /// The type parameters, in order; `Ty::Adt(id, args)` gives one argument for each.
    pub params: Vec<ParamId>,
    pub kind: TypeDefKind,
    /// Whether values of the type may move to another thread (`task.run`, DESIGN §14.1). Only
    /// the host's classes say no for now: the flag is recorded and nothing reads it yet.
    pub sendable: bool,
}

#[derive(Clone, Debug)]
pub enum TypeDefKind {
    /// Fields in declaration order; their defaults live at the literals that omit them.
    Struct {
        fields: Vec<FieldDef>,
    },
    /// A reference type with identity (DESIGN §7.2); same field layout as a struct.
    Class {
        fields: Vec<FieldDef>,
    },
    /// A class of the embedding program (EMBEDDING §2): an opaque object with identity, no
    /// fields and no literal; only the host creates one, and its methods are the host's.
    HostClass,
    Enum {
        variants: Vec<VariantDef>,
    },
    /// An interface (DESIGN §9.2); as a type it means a dynamically dispatched value.
    Interface {
        methods: Vec<IfaceMethod>,
    },
}

/// A method of an interface. `Callee::Bound` and `Callee::Dyn` name it by its index.
#[derive(Clone, Debug)]
pub struct IfaceMethod {
    pub name: String,
    pub span: Span,
    /// `Some(mutable)` (`var self` is `Some(true)`); every interface method has a receiver.
    pub receiver: Option<bool>,
    /// The default implementation, if the interface provides one.
    pub default: Option<FnId>,
}

#[derive(Clone, Debug)]
pub struct FieldDef {
    /// For a positional enum payload, the position (`"0"`, `"1"`).
    pub name: String,
    /// The declared type; for a `weak T` field this is `T` (reads give `T?`).
    pub ty: Ty,
    pub span: Span,
    /// A `weak` class field (DESIGN §11.5): it stores a weak reference, and reading it gives
    /// `ty?`.
    pub weak: bool,
}

#[derive(Clone, Debug)]
pub struct VariantDef {
    pub name: String,
    pub span: Span,
    /// The payload, empty for a unit variant.
    pub fields: Vec<FieldDef>,
    /// Whether the payload fields have names (`Circle(radius: float)`).
    pub named: bool,
}

impl TypeDef {
    /// The variants of an enum (empty for a struct).
    pub fn variants(&self) -> &[VariantDef] {
        match &self.kind {
            TypeDefKind::Enum { variants } => variants,
            TypeDefKind::Struct { .. }
            | TypeDefKind::Class { .. }
            | TypeDefKind::HostClass
            | TypeDefKind::Interface { .. } => &[],
        }
    }

    /// The fields of a struct or class (empty for an enum).
    pub fn fields(&self) -> &[FieldDef] {
        match &self.kind {
            TypeDefKind::Struct { fields } | TypeDefKind::Class { fields } => fields,
            TypeDefKind::Enum { .. } | TypeDefKind::HostClass | TypeDefKind::Interface { .. } => {
                &[]
            }
        }
    }

    /// The methods of an interface (empty for anything else).
    pub fn iface_methods(&self) -> &[IfaceMethod] {
        match &self.kind {
            TypeDefKind::Interface { methods } => methods,
            _ => &[],
        }
    }

    pub fn is_interface(&self) -> bool {
        matches!(self.kind, TypeDefKind::Interface { .. })
    }

    pub fn is_enum(&self) -> bool {
        matches!(self.kind, TypeDefKind::Enum { .. })
    }

    /// A reference type with identity: a `class`, or a class of the host.
    pub fn is_class(&self) -> bool {
        matches!(
            self.kind,
            TypeDefKind::Class { .. } | TypeDefKind::HostClass
        )
    }

    /// A class of the embedding program.
    pub fn is_host_class(&self) -> bool {
        matches!(self.kind, TypeDefKind::HostClass)
    }
}

/// A function declaration with its body.
#[derive(Debug)]
pub struct FnDef {
    pub name: String,
    pub sig: FnSig,
    pub span: Span,
    pub body: Rc<Body>,
    /// How many type parameters the function has in scope (its own plus those of its `impl`
    /// or interface). Calls record one type argument for each in [`Call::type_args`].
    pub type_params: u32,
    /// For a host function, its id; the body is empty and never runs.
    pub host: Option<HostFnId>,
}

/// A module-level `const`, `let` or `var`.
#[derive(Debug)]
pub struct GlobalDef {
    pub name: String,
    pub ty: Ty,
    pub mutable: bool,
    pub span: Span,
    /// Computes the initial value; runs once, before the script's first statement.
    pub init: Rc<Body>,
}

/// Everything the engine needs to run one function, initialiser or script.
#[derive(Debug)]
pub struct Body {
    /// Shown in stack traces.
    pub name: String,
    pub span: Span,
    /// The first `param_count` locals are the parameters, in order.
    pub param_count: u32,
    pub locals: Vec<LocalInfo>,
    /// The function's top-level block. Its drop list starts with the parameters.
    pub block: Block,
    pub ret: Ty,
    /// A `var self` method: its first parameter is an in/out receiver. When the call ends,
    /// the engine takes the final value of local 0 back out and the caller writes it to the
    /// receiver place (ARCHITECTURE §4).
    pub mutates_self: bool,
    /// The body reads its hidden type-argument vector at runtime (ARCHITECTURE §1). Nothing
    /// does yet (no `json.decode[T]`, no `e.is[T]()`), so the engines may drop `type_args`.
    pub needs_type_args: bool,
}

/// How a local's slot is stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Storage {
    /// The value lives in the frame slot.
    Slot,
    /// The value lives in a shared cell: the local is captured by a closure, is `var` and is
    /// assigned after its initialisation (ARCHITECTURE §1). A fresh cell is made each time the
    /// local is bound, so a loop iteration or a block entered twice gets its own.
    Cell,
}

#[derive(Clone, Debug)]
pub struct LocalInfo {
    pub name: String,
    pub ty: Ty,
    pub mutable: bool,
    pub storage: Storage,
    pub span: Span,
}

/// A braced scope. `locals` lists the bindings declared directly in it, in declaration order,
/// which is also the order they are dropped in reverse.
#[derive(Debug)]
pub struct Block {
    pub locals: Vec<LocalId>,
    pub stmts: Vec<Stmt>,
    /// The block's value; without it the block is `()` (or `!` if it cannot finish).
    pub tail: Option<Box<Expr>>,
    pub ty: Ty,
    pub span: Span,
}

/// A statement; every statement is a temporary scope.
#[derive(Debug)]
pub struct Stmt {
    pub span: Span,
    pub kind: StmtKind,
}

#[derive(Debug)]
pub enum StmtKind {
    /// Initialises a local declared by the enclosing block.
    Let {
        local: LocalId,
        init: Expr,
    },
    /// Evaluates for effect and drops the value.
    Expr(Expr),
    /// `place = rhs`, or `place op= rhs` when `op` is set (the place is read, combined and
    /// written once).
    Assign {
        place: Place,
        op: Option<BinOp>,
        rhs: Expr,
    },
    /// `let (a, b) = init`: destructures with an irrefutable pattern, binding locals declared
    /// by the enclosing block.
    LetPat {
        pat: Pat,
        init: Expr,
    },
    Return(Option<Expr>),
    Break(LoopId),
    Continue(LoopId),
    /// The line of a script's module-level `let`/`var`: runs its initialiser and stores the
    /// value in the global (DESIGN §4.2). Only in [`Program::script`].
    InitGlobal {
        global: GlobalId,
        init: Rc<Body>,
    },
    /// `defer stmt`: registers `stmt` on the enclosing block's cleanup list; it runs when the
    /// block exits, in last-in first-out order with the drops of the block's bindings
    /// (DESIGN §6.1, ARCHITECTURE §6).
    Defer(Rc<Stmt>),
}

/// An assignable location: an anchor plus a path of value projections.
#[derive(Debug)]
pub struct Place {
    pub anchor: Anchor,
    pub path: Vec<ValueProj>,
    pub ty: Ty,
    /// The source text of the place, for guard messages.
    pub display: String,
}

/// Where a place starts. Collections and classes are references, so only the innermost one is
/// the anchor and everything before it is an ordinary read (ARCHITECTURE §1).
#[derive(Debug)]
pub enum Anchor {
    Local(LocalId),
    Global(GlobalId),
    /// A field of a class object: the expression yields the object.
    ClassField(Box<Expr>, FieldIdx),
    /// A list element: list, index, and the source text of the list for guard messages.
    ListElem(Box<Expr>, Box<Expr>, String),
    /// A map entry: map, key, and the source text of the map for guard messages.
    MapEntry(Box<Expr>, Box<Expr>, String),
}

/// A step through a value type (struct field or tuple element).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueProj {
    Field(FieldIdx),
    TupleIdx(u32),
    /// Component `i` of a vector (`v.x` is `Comp(0)`). Always the last step of a path: a
    /// component is a number, not a place that can be walked into.
    Comp(u8),
}

#[derive(Debug)]
pub struct Expr {
    pub kind: ExprKind,
    pub ty: Ty,
    pub span: Span,
}

#[derive(Debug)]
pub enum ExprKind {
    Lit(Lit),
    /// Reads a place.
    Read(Box<Place>),
    Unary(UnOp, Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    /// `a && b`; `b` runs only when `a` is true.
    And(Box<Expr>, Box<Expr>),
    /// `a || b`; `b` runs only when `a` is false.
    Or(Box<Expr>, Box<Expr>),
    Call(Box<Call>),
    /// `go call(..)` (DESIGN §14.2): the operand is an [`ExprKind::Call`] whose callee is
    /// a function, a function value, an interface method or a host function (never a `var self`
    /// receiver or an intrinsic). The arguments are evaluated on the caller's fiber, the call
    /// itself runs on a new coroutine, and the value is its `Coroutine[T]` handle, where `T`
    /// is the call's type.
    Go(Box<Expr>),
    /// `group.go(call(..))` (DESIGN §14.2): like [`ExprKind::Go`], and the coroutine is owned
    /// by the `CoGroup` value of the first operand, which is evaluated first.
    GoIn(Box<Expr>, Box<Expr>),
    /// Concatenates its string parts (the desugaring of string interpolation).
    StrConcat(Vec<Expr>),
    If {
        cond: Box<Expr>,
        then: Box<Block>,
        else_: Option<Box<Block>>,
    },
    Block(Box<Block>),
    /// Runs `body` until a `break` to this loop (`while` is a `Loop` around an `If`).
    Loop {
        id: LoopId,
        body: Box<Block>,
    },
    /// `for i in a..b`, or over a list, map, set or string.
    For(Box<ForLoop>),
    /// `Player { .. }`: one value per field in declaration order (defaults already filled).
    Struct {
        ty: TypeId,
        fields: Vec<Expr>,
    },
    /// `Node { .. }` for a class: allocates an object with one value per field in declaration
    /// order (defaults filled; a `weak` field holds a [`ExprKind::ToWeak`]).
    Object {
        ty: TypeId,
        fields: Vec<Expr>,
    },
    /// `Shape.Circle(2.0)`: the payload values in declaration order.
    Variant {
        ty: TypeId,
        variant: u32,
        args: Vec<Expr>,
    },
    Tuple(Vec<Expr>),
    /// `[a, b]`; `[]` is the empty list.
    ListLit(Vec<Expr>),
    /// `{k: v}`; `{:}` is the empty map. Insertion-ordered; a repeated key keeps the last value.
    MapLit(Vec<(Expr, Expr)>),
    /// A strong reference (or an `Option` of one) stored as a weak one: `StrongToWeak` /
    /// `OptionToWeak` of ARCHITECTURE §1.
    ToWeak(Box<Expr>),
    /// The value of a weak field read as `T?`: `WeakToOption`.
    FromWeak(Box<Expr>),
    /// A field or tuple element of a value that is not a place (`make().x`).
    Project(Box<Expr>, ValueProj),
    /// The first arm whose pattern matches (and whose guard holds) runs. The scrutinee lives
    /// for the whole construct. Checked exhaustive, so some arm always runs.
    Match {
        scrutinee: Box<Expr>,
        arms: Vec<Arm>,
    },
    /// An implicit or explicit (`as`) conversion.
    Coerce(Box<Expr>, Coercion),
    /// A function used as a value (`let f = add`, `max[int]`, `Particle.new`). `type_args` are
    /// the function's type arguments, as in [`Call::type_args`].
    FnRef {
        f: FnId,
        type_args: Vec<Ty>,
    },
    /// A closure expression: builds a closure object that captures variables of the enclosing
    /// body (ARCHITECTURE §1, DESIGN §6.2).
    Closure(Box<ClosureExpr>),
    /// `a..b` / `a..=b` as a value of type `Range[int]`.
    Range {
        start: Box<Expr>,
        end: Box<Expr>,
        inclusive: bool,
    },
    /// Left behind after a diagnostic; never reaches an engine.
    Error,
}

/// A closure expression. The body is the lifted function `f`; each capture copies a local of
/// the enclosing body into a local of the closure's body when the closure is created.
#[derive(Debug)]
pub struct ClosureExpr {
    pub f: FnId,
    pub captures: Vec<Capture>,
}

/// One captured variable (ARCHITECTURE §1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capture {
    /// The local of the enclosing body.
    pub outer: LocalId,
    /// The local of the closure's body that holds it while the closure runs.
    pub inner: LocalId,
    pub mode: CaptureMode,
}

/// How a closure holds a captured variable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureMode {
    /// The value is copied into the closure (a reference is shared, a struct is copied).
    ByValue,
    /// The variable lives in a shared cell that the enclosing body and the closure both see.
    Cell,
    /// A weak reference to a class object: inside the closure the name is `T?`.
    Weak,
}

/// A conversion the checker made explicit (ARCHITECTURE §1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Coercion {
    /// A value of a type with a known implementation becomes an interface value (DESIGN §9.2).
    ToDyn(ImplId),
    /// The same, for a value whose type is a type parameter with this interface as a bound:
    /// the implementation is found from the value at runtime.
    ToDynBound(TypeId),
    /// A value of a type that implements `IntoError` becomes an `Error` that keeps the value
    /// (DESIGN §10.1); an `Error` stays as it is.
    IntoError(ErrorConv),
    /// `Result[T, E]` becomes `Result[T, Error]`: the payload of `err` is converted as by
    /// [`Coercion::IntoError`].
    ResultIntoError(ErrorConv),
    /// An `f32` becomes a `float` (DESIGN §5.1); lossless.
    F32ToFloat,
    /// A `float` becomes an `f32` (rounded to nearest): the scalar of vector arithmetic and the
    /// components of a vector constructor accept a `float` (DESIGN §5.1, §15.1).
    FloatToF32,
}

/// How a value is turned into an `Error`: the engine records the implementation with the error
/// so that its message can be asked for later.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorConv {
    /// The `IntoError` implementation of the value's type, known statically.
    Impl(ImplId),
    /// The type is a parameter bounded by `IntoError`: the implementation is found from the
    /// value at runtime.
    Bound,
}

/// A `for` loop. The binding is fresh per iteration: its locals are listed in `body.locals`,
/// so the body's scope exit releases them each time round.
#[derive(Debug)]
pub struct ForLoop {
    pub id: LoopId,
    pub binding: ForBinding,
    pub source: ForSource,
    /// `for var x in list`: the element is a place; the final value of the binding is written
    /// back after each iteration (DESIGN §11.6). Only lists.
    pub places: bool,
    pub body: Block,
}

/// What a `for` loop binds each iteration.
#[derive(Debug)]
pub enum ForBinding {
    /// `for _ in ..`
    None,
    Local(LocalId),
    /// `for (k, v) in map`: an irrefutable pattern whose bindings are locals of the body.
    Pat(Pat),
}

/// What a `for` loop iterates. Collection sources hold the mutation guard for the whole loop.
#[derive(Debug)]
pub enum ForSource {
    Range {
        start: Expr,
        end: Expr,
        inclusive: bool,
    },
    /// Elements of a list; `name` is its source text for guard messages.
    List {
        list: Expr,
        name: String,
    },
    /// `(key, value)` tuples in insertion order.
    Map(Expr),
    Set(Expr),
    /// Anything else iterable: the expression is an iterator (`Iter[T]`, a built-in iterator
    /// object or a boxed user implementation); each trip calls its `next`.
    Iter(Expr),
}

/// A `match` arm. `body.locals` starts with the pattern's bindings, which are bound before the
/// guard runs and dropped when the arm ends (or when the guard fails).
#[derive(Debug)]
pub struct Arm {
    pub pat: Pat,
    pub guard: Option<Expr>,
    pub body: Block,
}

/// A pattern, typed. Patterns stay patterns in HIR; engines match them directly.
#[derive(Debug)]
pub struct Pat {
    pub kind: PatKind,
    pub ty: Ty,
    pub span: Span,
}

#[derive(Debug)]
pub enum PatKind {
    Wild,
    /// A binding, optionally with a sub-pattern: `x` or `pat as x`.
    Bind(LocalId, Option<Box<Pat>>),
    /// An integer, char, string or bool literal (a negative integer is already folded).
    Lit(Lit),
    /// An integer or char range; a missing bound is open.
    Range {
        lo: Option<Lit>,
        hi: Option<Lit>,
        inclusive: bool,
    },
    /// A module-level `const`, compared by equality when the pattern is tried.
    Const(GlobalId),
    /// An enum variant with one sub-pattern per payload field.
    Variant {
        ty: TypeId,
        variant: u32,
        args: Vec<Pat>,
    },
    /// A struct with one sub-pattern per field in declaration order (`_` for omitted ones).
    Struct {
        ty: TypeId,
        fields: Vec<Pat>,
    },
    Tuple(Vec<Pat>),
    /// Alternatives that bind the same locals.
    Or(Vec<Pat>),
}

/// A resolved call. Named and default arguments are already positional.
#[derive(Debug)]
pub struct Call {
    pub callee: Callee,
    pub type_args: Vec<Ty>,
    pub args: Vec<Arg>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Lit {
    Unit,
    Bool(bool),
    Int(i64),
    /// An integer above `i64::MAX`: only a `u64` literal can be one (the checker turns every
    /// literal of type `u64` into this form).
    U64(u64),
    Float(f64),
    /// A literal of type `f32`, already rounded.
    F32(f32),
    Char(char),
    Str(Rc<str>),
}

/// Who is called.
#[derive(Clone, Debug, PartialEq)]
pub enum Callee {
    Fn(FnId),
    /// A method of an interface called on a value of a type parameter or a derived
    /// implementation: the engine finds the implementation from the runtime type of the
    /// receiver (`args[0]`), through `impls[(runtime type, iface)]`.
    Bound {
        iface: TypeId,
        method: u32,
    },
    /// A method called on an interface value: dispatched through the implementation stored
    /// in the value (DESIGN §9.2).
    Dyn {
        iface: TypeId,
        method: u32,
    },
    Intrinsic(Intrinsic),
    /// A function provided by the host program (EMBEDDING §1).
    Host(HostFnId),
    /// A call of a function value (`f(x)`, `self.on_hit(dmg)` on a field): `args[0]` is the
    /// function or closure, the rest are its arguments.
    Value,
}

impl Callee {
    /// The built-in operation of an intrinsic callee.
    pub fn builtin(op: Builtin) -> Callee {
        Callee::Intrinsic(Intrinsic::Builtin(op))
    }
}

/// A method of `ptr[T]`. Every one but `Addr` needs an `unsafe` block, which the checker has
/// already enforced.
#[derive(Clone, Debug, PartialEq)]
pub enum PtrOp {
    /// `p.read() -> T`
    Read(CTy),
    /// `p.write(v)`
    Write(CTy),
    /// `p.offset(n) -> ptr[T]`: `n` elements of `size` bytes further.
    Offset { size: usize },
    /// `p.cast[U]() -> ptr[U]`: the same address.
    Cast,
    /// `p.addr() -> u64`
    Addr,
    /// `p.read_str() -> str`: the NUL-terminated string at the address.
    ReadStr,
}

/// Operations built into the compiler.
#[derive(Clone, Debug, PartialEq)]
pub enum Intrinsic {
    /// `print(x)`: writes the `Show` form of the argument and a newline.
    Print,
    /// The `Show` form of the argument, as a `str`.
    Show,
    /// The argument formatted with a spec, as a `str`.
    Format(Box<FormatSpec>),
    /// A method or constructor of a built-in type; the receiver is the first argument.
    Builtin(Builtin),
    /// `panic(msg)`: stops the program with a message (it has type `!`).
    Panic,
    /// `assert(cond, msg)`: panics with `msg` when `cond` is false (arguments: the condition,
    /// the message as a `str`). Type `()`.
    Assert,
    /// `assert_eq(a, b)`: panics when `a == b` is false, showing both values (ARCHITECTURE §15).
    AssertEq,
    /// `compare(a, b) -> int`: `-1`, `0` or `1` by the `Ord` of the two values (same type); a
    /// `nan` anywhere in the comparison panics, as in `sort`.
    Compare,
    /// `e.is[T]()`: whether the value an `Error` keeps has this runtime type (DESIGN §10.2).
    ErrorIs(TypeKey),
    /// `e.as[T]() -> T?`: the value an `Error` keeps, if it has this runtime type.
    ErrorAs(TypeKey),
    /// A method of `ptr[T]` (DESIGN §15.3); the receiver is the first argument.
    Ptr(PtrOp),
    /// `object.alive()` on a class of the host: whether the host still has the object. The
    /// receiver is the only argument.
    HostAlive,
    /// A conversion call `u8(x)`, `float(x)`, `f32(x)`: from the argument's type to the named
    /// one (DESIGN §5.1). Integer conversions that lose the value panic.
    Convert { from: Num, to: Num },
    /// A method of a number type (`x.wrapping_add(y)`, `x.sqrt()`); the receiver is the first
    /// argument and `Num` is its type.
    NumMethod(NumMethod, Num),
    /// `wait(seconds: float)`: pauses the running coroutine for scheduler time.
    Wait,
    /// `wait_frames(n: int)`: pauses for `n` ticks.
    WaitFrames,
    /// `wait_until(cond: fn() -> bool)`: pauses until `cond()` is true, checked at every tick.
    WaitUntil,
    /// `wait_real(seconds: float)`: pauses for wall-clock time.
    WaitReal,
    /// `task.tick(dt: float)`: resumes the coroutines that are due.
    TaskTick,
    /// `wait_for(signal: Signal)`: pauses until the signal is next emitted.
    WaitFor,
    /// `CoGroup()`
    NewCoGroup,
    /// `Signal()`
    NewSignal,
    /// `vec2(x, y)`, `ivec3(1)`, `vec2(iv)`: builds a vector of this type from its components
    /// (already `f32` or `i32`), one scalar (every component), or one vector of the other kind
    /// with the same length (converted component-wise).
    VecNew(VecTy),
    /// `v.xz`, `v.zyx`: a vector of `len` components, `lanes[i]` being the source component of
    /// result component `i`. `from` is the type of the argument.
    VecSwizzle {
        from: VecTy,
        lanes: [u8; 4],
        len: u8,
    },
    /// A method of a vector type; the receiver is the first argument and `VecTy` its type.
    VecMethod(VecMethod, VecTy),
}

/// The methods of the vector types (DESIGN §15.1, ARCHITECTURE "Vector types as built").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VecMethod {
    Dot,
    /// `vec3` and `ivec3` only.
    Cross,
    Len,
    LenSq,
    Normalize,
    Lerp,
    Distance,
    Abs,
    Min,
    Max,
}

/// The methods of the numeric types (DESIGN §5.1, Q6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NumMethod {
    WrappingAdd,
    WrappingSub,
    WrappingMul,
    CheckedAdd,
    CheckedSub,
    CheckedMul,
    CheckedDiv,
    SaturatingAdd,
    SaturatingSub,
    SaturatingMul,
    /// Integers and floats.
    Abs,
    Min,
    Max,
    /// Integers: `x.pow(n)` traps on overflow.
    Pow,
    Sqrt,
    Floor,
    Ceil,
    Round,
    IsNan,
}

/// Methods and constructors of `str`, `List`, `Map` and `Set` (DESIGN §5.2, §5.3), plus the
/// deep `clone` every type has. The checker types each call through the receiver's type
/// arguments; the engine only sees these tags.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Builtin {
    /// `x.clone()` on any type: a deep copy (DESIGN §5.3).
    Clone,
    StrLen,
    StrIsEmpty,
    ListLen,
    ListIsEmpty,
    ListPush,
    ListPop,
    ListInsert,
    ListRemove,
    ListClear,
    ListContains,
    ListIndexOf,
    ListFirst,
    ListLast,
    ListReverse,
    ListExtend,
    ListGet,
    MapLen,
    MapIsEmpty,
    MapGet,
    /// `m.get_or(key, default)`
    MapGetOr,
    /// `m.entries() -> List[(K, V)]`
    MapEntries,
    MapContainsKey,
    MapInsert,
    MapRemove,
    MapKeys,
    MapValues,
    MapClear,
    SetLen,
    SetIsEmpty,
    SetAdd,
    SetRemove,
    SetContains,
    SetClear,
    SetUnion,
    SetIntersection,
    SetDifference,
    SetSymmetricDifference,
    SetIsSubset,
    SetIsSuperset,
    SetIsDisjoint,
    /// `Set[T]()`
    SetNew,
    /// `StrBuilder()`
    StrBuilderNew,
    StrBuilderPush,
    StrBuilderPushChar,
    StrBuilderLen,
    StrBuilderIsEmpty,
    StrBuilderClear,
    StrBuilderToStr,
    /// `Set(list)`
    SetFromList,
    // ---- closure-taking and ordering list methods (DESIGN §6.3, §9.2) ----
    ListMap,
    ListFilter,
    ListRetain,
    ListAny,
    ListAll,
    ListFind,
    ListFindIndex,
    ListFold,
    ListForEach,
    ListSort,
    ListSortBy,
    ListSortByKey,
    ListMin,
    ListMax,
    ListSum,
    ListJoin,
    /// `xs.zip(ys)`: pairs up to the shorter list.
    ListZip,
    ListFlatMap,
    /// `xs.sorted()`: a sorted copy.
    ListSorted,
    ListSortedBy,
    /// `xs.dedup()`: removes consecutive equal elements in place.
    ListDedup,
    ListChunks,
    ListWindows,
    /// `xs[lo..hi]`: `args` are the list, `lo` and `hi` as `int?` (`none` for an open end) and
    /// whether `hi` is inclusive.
    ListSlice,
    // ---- strings ----
    StrChars,
    StrBytes,
    StrContains,
    StrStartsWith,
    StrEndsWith,
    StrSplit,
    StrTrim,
    StrToUpper,
    StrToLower,
    StrReplace,
    StrFind,
    /// `s.rfind(sub)`: the byte offset of the last match, as `int?`.
    StrRFind,
    /// `s.split_once(sep)`: `(before, after)` at the first `sep`, as `(str, str)?`.
    StrSplitOnce,
    /// `s.rsplit_once(sep)`: the same at the last `sep`.
    StrRSplitOnce,
    /// `s.pad_left(width)`, or with a fill `char`: pad on the left up to `width` characters.
    StrPadLeft,
    /// `s.pad_right(width)`, or with a fill `char`.
    StrPadRight,
    /// `s[lo..hi]`, with the arguments of [`Builtin::ListSlice`].
    StrSlice,
    // ---- ranges and iterators ----
    RangeContains,
    RangeLen,
    RangeIsEmpty,
    /// `r.rev()`: an iterator that counts down.
    RangeRev,
    /// `r.to_list()`: the integers of the range as a `List[int]`.
    RangeToList,
    /// `x.iter()` on a `List`, `Set`, `Map` or `Range`: an iterator object (`Iter[T]`).
    Iter,
    /// `it.enumerate()` on an iterator: `Iter[(int, T)]`. The receiver is already an `Iter`.
    IterEnumerate,
    // ---- Option (DESIGN §5.2) ----
    OptionIsSome,
    OptionIsNone,
    /// `opt.expect(msg)`: the value, or a panic with `msg`.
    OptionExpect,
    OptionMap,
    OptionAndThen,
    // ---- Result (DESIGN §10) ----
    ResultIsOk,
    ResultIsErr,
    /// `r.expect(msg)`: the value, or a panic with `msg` and the error.
    ResultExpect,
    ResultMap,
    ResultAndThen,
    /// `r.ok() -> T?`
    ResultOk,
    /// `r.err() -> E?`
    ResultErr,
    /// `r.context(msg)`: the receiver is already a `Result[T, Error]` (the checker converts
    /// it with [`Coercion::ResultIntoError`]).
    ResultContext,
    // ---- Error (DESIGN §10.2) ----
    /// `e.message() -> str`: the outermost message, without the chain.
    ErrorMessage,
    /// `e.context(msg) -> Error`
    ErrorContext,
    /// `e.cause() -> Error?`
    ErrorCause,
    // ---- Coroutine[T] (DESIGN §14.2) ----
    /// `c.cancel()`
    CoroutineCancel,
    /// `c.done() -> bool`
    CoroutineDone,
    /// `c.join() -> T`
    CoroutineJoin,
    // ---- CoGroup and Signal (DESIGN §14.2) ----
    /// `g.cancel()`
    CoGroupCancel,
    /// `g.count() -> int`
    CoGroupCount,
    /// `g.join_all()`
    CoGroupJoinAll,
    /// `s.emit()`
    SignalEmit,
}

#[derive(Debug)]
pub enum Arg {
    Value(Expr),
    /// A mutable receiver (`var self`): the engine moves the value in and writes it back when
    /// the call ends, on every path.
    Place(Place),
}

/// A numeric type as an operation sees it: integer width, `f32` or `float`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Num {
    Int(IntTy),
    F32,
    Float,
}

impl Num {
    /// `int`.
    pub const INT: Num = Num::Int(IntTy::I64);

    /// The numeric type `ty` is, if it is one.
    pub fn of(ty: &Ty) -> Option<Num> {
        match ty {
            Ty::Int(i) => Some(Num::Int(*i)),
            Ty::F32 => Some(Num::F32),
            Ty::Float => Some(Num::Float),
            _ => None,
        }
    }

    /// The source name of the type.
    pub fn name(self) -> &'static str {
        match self {
            Num::Int(i) => i.name(),
            Num::F32 => "f32",
            Num::Float => "float",
        }
    }
}

/// What a comparison compares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operand {
    Int(IntTy),
    Float,
    F32,
    Bool,
    Char,
    Str,
    Unit,
    /// Structs, enums, tuples, `Option` and the collections: the derived structural `Eq`, and
    /// `Ord` for tuples and payload-free enums (DESIGN §7.4, §5.3). A class inside is compared
    /// by identity.
    Structural,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
}

/// A bitwise operator (DESIGN §5.1). `Shl` and `Shr` take any integer type as the count.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BitOp {
    And,
    Or,
    Xor,
    Shl,
    Shr,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

/// A binary operator resolved to its operand type. Integer operations carry their width, so
/// overflow traps are static (DESIGN §5.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinOp {
    Arith(ArithOp, Num),
    /// `& | ^ << >>` on integers of this type (the left operand's).
    Bit(BitOp, IntTy),
    /// `str + str`
    Concat,
    Cmp(CmpOp, Operand),
    /// Component-wise arithmetic on vectors of this type (`+ - * /`); the shape says where the
    /// scalar is, if there is one.
    Vec(ArithOp, VecTy, VecShape),
    /// `a is b`: whether two references are the same object (DESIGN §5.3).
    Is,
}

/// The operands of vector arithmetic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VecShape {
    /// `a + b`: two vectors of the type.
    VecVec,
    /// `v * s`, `v / s`: the right operand is a scalar (`f32` or `i32`).
    VecScalar,
    /// `s * v`: the left operand is a scalar.
    ScalarVec,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnOp {
    Neg(Num),
    /// `-v` on a vector of this type.
    VecNeg(VecTy),
    Not,
    /// `~x` on an integer of this type.
    BitNot(IntTy),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Align {
    Left,
    Right,
    Center,
}

/// The conversion letter of a format spec.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormatType {
    LowerHex,
    UpperHex,
    Binary,
    Octal,
}

/// A validated format spec (DESIGN §12): `[[fill]align][0][width][.precision][type]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FormatSpec {
    pub fill: char,
    pub align: Option<Align>,
    /// The `0` flag: pad numbers with zeros after the sign.
    pub zero: bool,
    pub width: usize,
    pub precision: Option<usize>,
    pub ty: Option<FormatType>,
}
