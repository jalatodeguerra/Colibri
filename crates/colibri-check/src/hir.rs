//! The typed, name-resolved intermediate representation (ARCHITECTURE §1).
//!
//! HIR is the contract between the checker and every engine: names are resolved (locals are
//! frame slots, globals and functions are ids), every expression carries its type, and sugar
//! is gone. Bodies refer to each other only by id.

use std::rc::Rc;

use colibri_syntax::Span;

use crate::types::{FnSig, IntTy, ParamId, Ty, TypeId};

/// A function in [`Program::fns`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FnId(pub u32);

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
    /// Globals in the order their initialisers must run (dependencies first).
    pub init_order: Vec<GlobalId>,
    /// The top-level statements of the file, in order.
    pub script: Rc<Body>,
    /// `fn main()` if the file defines it.
    pub main: Option<FnId>,
    /// Every struct, enum and class, indexed by [`TypeId`]. Index 0 is the prelude `Option`
    /// enum ([`crate::types::OPTION_TYPE`]); user types follow.
    pub types: Vec<TypeDef>,
    /// Every interface implementation: explicit `impl I for T` blocks and the derived
    /// implementations that were used (DESIGN §7.4). The engine dispatches `Callee::Bound`
    /// and `Callee::Dyn` calls through them.
    pub impls: Vec<ImplDef>,
}

/// What a runtime value's type looks like to interface dispatch: the head of a type, without
/// type arguments, since the engine runs generic code with one uniform representation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TypeKey {
    Int,
    Float,
    Bool,
    Char,
    Str,
    Unit,
    Tuple,
    List,
    Map,
    Set,
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
            .position(|f| f.name == name)
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
            | TypeDefKind::Interface { .. } => &[],
        }
    }

    /// The fields of a struct or class (empty for an enum).
    pub fn fields(&self) -> &[FieldDef] {
        match &self.kind {
            TypeDefKind::Struct { fields } | TypeDefKind::Class { fields } => fields,
            TypeDefKind::Enum { .. } | TypeDefKind::Interface { .. } => &[],
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

    pub fn is_class(&self) -> bool {
        matches!(self.kind, TypeDefKind::Class { .. })
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
    /// The value lives in a shared cell (captured by a closure; not produced yet).
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
    /// Left behind after a diagnostic; never reaches an engine.
    Error,
}

/// A conversion the checker made explicit (ARCHITECTURE §1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Coercion {
    /// A value of a type with a known implementation becomes an interface value (DESIGN §9.2).
    ToDyn(ImplId),
    /// The same, for a value whose type is a type parameter with this interface as a bound:
    /// the implementation is found from the value at runtime.
    ToDynBound(TypeId),
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
    /// `(index, element)` tuples of `list.enumerate()`.
    Enumerate(Expr),
    /// The chars of a `str`.
    Chars(Expr),
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
    Float(f64),
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
}

impl Callee {
    /// The built-in operation of an intrinsic callee.
    pub fn builtin(op: Builtin) -> Callee {
        Callee::Intrinsic(Intrinsic::Builtin(op))
    }
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
    /// `Set[T]()`
    SetNew,
    /// `Set(list)`
    SetFromList,
}

#[derive(Debug)]
pub enum Arg {
    Value(Expr),
    /// A mutable receiver (`var self`): the engine moves the value in and writes it back when
    /// the call ends, on every path.
    Place(Place),
}

/// Arithmetic operand kind: integer width or float.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Num {
    Int(IntTy),
    Float,
}

/// What a comparison compares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operand {
    Int(IntTy),
    Float,
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
    /// `str + str`
    Concat,
    Cmp(CmpOp, Operand),
    /// `a is b`: whether two references are the same object (DESIGN §5.3).
    Is,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnOp {
    Neg(Num),
    Not,
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
