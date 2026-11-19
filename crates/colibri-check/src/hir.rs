//! The typed, name-resolved intermediate representation (ARCHITECTURE §1).
//!
//! HIR is the contract between the checker and every engine: names are resolved (locals are
//! frame slots, globals and functions are ids), every expression carries its type, and sugar
//! is gone. Bodies refer to each other only by id.

use std::rc::Rc;

use colibri_syntax::Span;

use crate::types::{FnSig, IntTy, Ty, TypeId};

/// A function in [`Program::fns`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FnId(pub u32);

/// A module-level value in [`Program::globals`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GlobalId(pub u32);

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
    /// Every user-defined struct and enum, indexed by [`TypeId`].
    pub types: Vec<TypeDef>,
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
    pub kind: TypeDefKind,
}

#[derive(Clone, Debug)]
pub enum TypeDefKind {
    /// Fields in declaration order; their defaults live at the literals that omit them.
    Struct {
        fields: Vec<FieldDef>,
    },
    Enum {
        variants: Vec<VariantDef>,
    },
}

#[derive(Clone, Debug)]
pub struct FieldDef {
    /// For a positional enum payload, the position (`"0"`, `"1"`).
    pub name: String,
    pub ty: Ty,
    pub span: Span,
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
            TypeDefKind::Struct { .. } => &[],
        }
    }

    /// The fields of a struct (empty for an enum).
    pub fn fields(&self) -> &[FieldDef] {
        match &self.kind {
            TypeDefKind::Struct { fields } => fields,
            TypeDefKind::Enum { .. } => &[],
        }
    }

    pub fn is_enum(&self) -> bool {
        matches!(self.kind, TypeDefKind::Enum { .. })
    }
}

/// A function declaration with its body.
#[derive(Debug)]
pub struct FnDef {
    pub name: String,
    pub sig: FnSig,
    pub span: Span,
    pub body: Rc<Body>,
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

#[derive(Debug)]
pub enum Anchor {
    Local(LocalId),
    Global(GlobalId),
    /// A field of a class object (not produced yet).
    ClassField(Box<Expr>, FieldIdx),
    /// A list element: list, index (not produced yet).
    ListElem(Box<Expr>, Box<Expr>),
    /// A map entry: map, key (not produced yet).
    MapEntry(Box<Expr>, Box<Expr>),
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
    /// `for i in a..b`.
    For(Box<ForLoop>),
    /// `Player { .. }`: one value per field in declaration order (defaults already filled).
    Struct {
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
    /// A field or tuple element of a value that is not a place (`make().x`).
    Project(Box<Expr>, ValueProj),
    /// The first arm whose pattern matches (and whose guard holds) runs. The scrutinee lives
    /// for the whole construct. Checked exhaustive, so some arm always runs.
    Match {
        scrutinee: Box<Expr>,
        arms: Vec<Arm>,
    },
    /// Left behind after a diagnostic; never reaches an engine.
    Error,
}

/// A loop over an integer range. The binding is a fresh local per iteration: it is listed in
/// `body.locals`, so the body's scope exit releases it each time round.
#[derive(Debug)]
pub struct ForLoop {
    pub id: LoopId,
    /// `None` for `for _ in ..`.
    pub binding: Option<LocalId>,
    pub start: Expr,
    pub end: Expr,
    pub inclusive: bool,
    pub body: Block,
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
    Intrinsic(Intrinsic),
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
    /// Structs, enums and tuples: the derived structural `Eq`, and `Ord` for tuples and
    /// payload-free enums (DESIGN §7.4).
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
