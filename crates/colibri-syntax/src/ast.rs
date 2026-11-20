//! The Colibri syntax tree.
//!
//! The parser produces this tree and does no name resolution or type checking.
//! Every [`Expr`], [`Pat`], [`Stmt`], [`Item`] and [`TypeExpr`] has a [`Span`] and a
//! [`NodeId`] that is unique within one parse. Error recovery leaves
//! `Error` placeholder nodes so later stages can keep going.

use crate::Span;

/// Identifies a node within one parsed file (assigned by the parser, in
/// creation order).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(pub u32);

/// A name with the span of its source text.
#[derive(Clone, Debug, PartialEq)]
pub struct Ident {
    pub name: String,
    pub span: Span,
}

impl Ident {
    /// Whether the name starts with an uppercase letter. The pattern
    /// capitalisation rule (DESIGN §8.2) is enforced by the checker.
    pub fn is_capitalised(&self) -> bool {
        self.name.chars().next().is_some_and(char::is_uppercase)
    }
}

/// A loop label such as `'outer` (stored without the quote).
#[derive(Clone, Debug, PartialEq)]
pub struct Label {
    pub name: String,
    pub span: Span,
}

// ---- files and items ----

/// A parsed source file.
#[derive(Clone, Debug, PartialEq)]
pub struct File {
    pub items: Vec<TopLevel>,
    pub span: Span,
}

/// Files hold items and statements interleaved (DESIGN §13, Q15); the checker
/// decides where top-level statements are allowed.
#[derive(Clone, Debug, PartialEq)]
pub enum TopLevel {
    Item(Item),
    Stmt(Stmt),
}

/// A declaration, with its attributes, doc comment and visibility.
#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub id: NodeId,
    pub span: Span,
    pub attrs: Vec<Attr>,
    /// `///` lines directly before the item, joined with `\n`.
    pub doc: Option<String>,
    /// Whether the item was marked `pub`.
    pub public: bool,
    pub kind: ItemKind,
}

/// `@name` or `@name(args)`.
#[derive(Clone, Debug, PartialEq)]
pub struct Attr {
    pub span: Span,
    pub name: Ident,
    pub args: Vec<Expr>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ItemKind {
    Fn(FnDecl),
    /// `struct` and `class`.
    Struct(StructDecl),
    Enum(EnumDecl),
    Interface(InterfaceDecl),
    Impl(ImplDecl),
    TypeAlias(TypeAlias),
    /// Module-level `const`, `let` or `var`.
    Binding(LetStmt),
    Import(Import),
    Extern(ExternBlock),
    Test(TestDecl),
    /// An item that failed to parse.
    Error,
}

/// A generic parameter with its bounds: `T: Hash + Eq`.
#[derive(Clone, Debug, PartialEq)]
pub struct GenericParam {
    pub name: Ident,
    pub bounds: Vec<TypeExpr>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FnDecl {
    pub name: Ident,
    pub generics: Vec<GenericParam>,
    pub receiver: Option<Receiver>,
    pub params: Vec<Param>,
    pub ret: Option<TypeExpr>,
    /// `None` for interface method signatures and `extern` declarations.
    pub body: Option<FnBody>,
}

/// `self` or `var self`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Receiver {
    pub mutable: bool,
    pub span: Span,
}

/// A function or closure parameter. `ty` is optional only in closures.
#[derive(Clone, Debug, PartialEq)]
pub struct Param {
    pub id: NodeId,
    pub span: Span,
    /// `var x`, as in `fn(var x) => ...`.
    pub mutable: bool,
    pub name: Ident,
    pub ty: Option<TypeExpr>,
    pub default: Option<Expr>,
}

/// `{ block }` bodies use `return`; `=> expr` bodies return the expression.
#[derive(Clone, Debug, PartialEq)]
pub enum FnBody {
    Block(Block),
    Expr(Expr),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StructKind {
    Struct,
    Class,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StructDecl {
    pub kind: StructKind,
    pub name: Ident,
    pub generics: Vec<GenericParam>,
    pub fields: Vec<Field>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Field {
    pub id: NodeId,
    pub span: Span,
    pub doc: Option<String>,
    /// `@rename("x")` and `@skip` before the field (DESIGN §7.5).
    pub attrs: Vec<Attr>,
    pub public: bool,
    pub name: Ident,
    /// `weak T` appears here as [`TypeKind::Weak`].
    pub ty: TypeExpr,
    pub default: Option<Expr>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EnumDecl {
    pub name: Ident,
    pub generics: Vec<GenericParam>,
    pub variants: Vec<Variant>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Variant {
    pub id: NodeId,
    pub span: Span,
    pub doc: Option<String>,
    pub name: Ident,
    pub payload: VariantPayload,
}

#[derive(Clone, Debug, PartialEq)]
pub enum VariantPayload {
    /// `Quit`
    Unit,
    /// `Move(vec2)`
    Positional(Vec<TypeExpr>),
    /// `Circle(radius: float)`
    Named(Vec<VariantField>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct VariantField {
    pub name: Ident,
    pub ty: TypeExpr,
}

/// An `interface`; its members are `Item`s of kind [`ItemKind::Fn`], with an
/// optional default body.
#[derive(Clone, Debug, PartialEq)]
pub struct InterfaceDecl {
    pub name: Ident,
    pub generics: Vec<GenericParam>,
    pub items: Vec<Item>,
}

/// `impl Type { }`, `impl Iface for Type { }` and `impl[T] Iface for List[T] { }`.
#[derive(Clone, Debug, PartialEq)]
pub struct ImplDecl {
    pub generics: Vec<GenericParam>,
    pub interface: Option<TypeExpr>,
    pub target: TypeExpr,
    /// Items of kind [`ItemKind::Fn`].
    pub items: Vec<Item>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TypeAlias {
    pub name: Ident,
    pub generics: Vec<GenericParam>,
    pub ty: TypeExpr,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Import {
    /// `a.b` in `import a.b`.
    pub path: Vec<Ident>,
    pub kind: ImportKind,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ImportKind {
    /// `import a.b`
    Plain,
    /// `import a.b as c`
    Alias(Ident),
    /// `import a.{B, C as D}`
    Names(Vec<ImportName>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct ImportName {
    pub name: Ident,
    pub alias: Option<Ident>,
}

/// `extern "C" { fn ...(...) -> T }`. Items are body-less `fn`s.
#[derive(Clone, Debug, PartialEq)]
pub struct ExternBlock {
    pub abi: String,
    pub items: Vec<Item>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TestDecl {
    pub name: String,
    pub body: Block,
}

// ---- types ----

#[derive(Clone, Debug, PartialEq)]
pub struct TypeExpr {
    pub id: NodeId,
    pub span: Span,
    pub kind: TypeKind,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TypeKind {
    /// `List[T]`, `jobs.Worker`, `ptr[T]`: a path with optional generic
    /// arguments on the last segment.
    Named {
        path: Vec<Ident>,
        args: Vec<TypeExpr>,
    },
    /// `T?`
    Optional(Box<TypeExpr>),
    /// `weak T`
    Weak(Box<TypeExpr>),
    /// `(A, B)`; `()` is the unit type.
    Tuple(Vec<TypeExpr>),
    /// `[T; N]`
    Array {
        elem: Box<TypeExpr>,
        len: Box<Expr>,
    },
    /// `fn(A, B) -> R`
    Fn {
        params: Vec<TypeExpr>,
        ret: Option<Box<TypeExpr>>,
    },
    /// `!`, the type of a function that never returns (a return type only).
    Never,
    Error,
}

// ---- statements ----

#[derive(Clone, Debug, PartialEq)]
pub struct Block {
    pub id: NodeId,
    pub span: Span,
    pub stmts: Vec<Stmt>,
    /// The last statement when it is an expression not ended by an explicit
    /// `;`: the value of the block (DESIGN §6.1).
    pub tail: Option<Box<Expr>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Stmt {
    pub id: NodeId,
    pub span: Span,
    pub kind: StmtKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BindKind {
    Let,
    Var,
    Const,
}

/// `let`, `var` or `const` with a pattern, an optional type and an initialiser.
/// Also the payload of module-level [`ItemKind::Binding`].
#[derive(Clone, Debug, PartialEq)]
pub struct LetStmt {
    pub kind: BindKind,
    pub pat: Pat,
    pub ty: Option<TypeExpr>,
    pub init: Expr,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AssignOp {
    Assign,
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
}

#[derive(Clone, Debug, PartialEq)]
pub enum StmtKind {
    Let(LetStmt),
    /// Assignment is a statement, not an expression.
    Assign {
        op: AssignOp,
        target: Expr,
        value: Expr,
    },
    Expr(Expr),
    Return(Option<Expr>),
    Break {
        label: Option<Label>,
        value: Option<Expr>,
    },
    Continue {
        label: Option<Label>,
    },
    /// `defer stmt`: an assignment, an expression or a block.
    Defer(Box<Stmt>),
    For {
        label: Option<Label>,
        /// `for var e in xs`
        mutable: bool,
        pat: Pat,
        iter: Expr,
        body: Block,
    },
    While {
        label: Option<Label>,
        cond: Expr,
        body: Block,
    },
    WhileLet {
        label: Option<Label>,
        pat: Pat,
        scrutinee: Expr,
        body: Block,
    },
    Error,
}

// ---- expressions ----

#[derive(Clone, Debug, PartialEq)]
pub struct Expr {
    pub id: NodeId,
    pub span: Span,
    pub kind: ExprKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnOp {
    Neg,
    Not,
    /// `~x`, bitwise not (DESIGN §5.1).
    BitNot,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    /// `&&`
    And,
    /// `||`
    Or,
    /// Identity test `a is b` (DESIGN §5.3).
    Is,
    /// `|`, `^`, `&`, `<<`, `>>`: bitwise operators on integers (DESIGN §5.1).
    BitOr,
    BitXor,
    BitAnd,
    Shl,
    Shr,
}

/// A call argument, positional or named (`level: "warn"`).
#[derive(Clone, Debug, PartialEq)]
pub struct Arg {
    pub name: Option<Ident>,
    pub value: Expr,
}

/// An argument in `base[...]`: the same node serves indexing and generic
/// instantiation, and the checker decides which it is (DESIGN §3.5).
#[derive(Clone, Debug, PartialEq)]
pub enum IndexArg {
    Expr(Expr),
    /// Only produced for arguments that do not parse as expressions
    /// (`fn(int) -> int`, `[u8; 4]`, `weak Node`).
    Type(TypeExpr),
}

#[derive(Clone, Debug, PartialEq)]
pub enum StrSeg {
    Lit(String),
    Interp { expr: Expr, spec: Option<String> },
}

#[derive(Clone, Debug, PartialEq)]
pub struct FieldInit {
    pub name: Ident,
    /// `Vec2i { x, y }` has `shorthand: true` and a value that is the
    /// identifier `x`.
    pub shorthand: bool,
    pub value: Expr,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MatchArm {
    pub id: NodeId,
    pub span: Span,
    pub pat: Pat,
    pub guard: Option<Expr>,
    pub body: ArmBody,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ArmBody {
    /// An expression or a `{ block }`.
    Expr(Expr),
    /// An assignment, `return`, `break` or `continue`.
    Stmt(Box<Stmt>),
}

/// One entry of a closure capture list: `use(weak self, other)`.
#[derive(Clone, Debug, PartialEq)]
pub struct Capture {
    pub weak: bool,
    pub name: Ident,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Closure {
    pub params: Vec<Param>,
    pub ret: Option<TypeExpr>,
    pub captures: Vec<Capture>,
    pub body: Box<FnBody>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ExprKind {
    Int(u64),
    Float(f64),
    Char(char),
    Bool(bool),
    None,
    /// A string with interpolation holes.
    Str(Vec<StrSeg>),
    List(Vec<Expr>),
    /// `{k: v}`; `{:}` is the empty map.
    Map(Vec<(Expr, Expr)>),
    /// `(a, b)`, `(a,)` and `()`.
    Tuple(Vec<Expr>),
    Paren(Box<Expr>),
    /// A name. Also `some`, `ok` and `err` (reserved words that are used as
    /// constructors, `ok(level)`).
    Ident(Ident),
    SelfValue,
    /// `.Variant` or `.Variant(args)` (DESIGN §8.2); `args` is `None` without
    /// parentheses.
    DotVariant {
        name: Ident,
        args: Option<Vec<Arg>>,
    },
    StructLit {
        ty: TypeExpr,
        fields: Vec<FieldInit>,
    },
    Unary {
        op: UnOp,
        expr: Box<Expr>,
    },
    Binary {
        op: BinOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    /// `a..b`, `a..=b` and the open forms `a..`, `..b`, `..`.
    Range {
        start: Option<Box<Expr>>,
        end: Option<Box<Expr>>,
        inclusive: bool,
    },
    /// `expr as Type`: a cast, a conversion or an interface conversion.
    Cast {
        expr: Box<Expr>,
        ty: TypeExpr,
    },
    /// `a or b` (fallback value).
    Or {
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    /// `a or { ... }` and `a or |e| { ... }` (DESIGN §10.3). The handler is a
    /// block of the enclosing function.
    OrHandler {
        lhs: Box<Expr>,
        binding: Option<Ident>,
        handler: Block,
    },
    /// Postfix `?`.
    Try(Box<Expr>),
    /// `go call(...)`: starts a coroutine and has type `Coroutine[T]`. The operand is always a
    /// call (anything else is a syntax error).
    Go(Box<Expr>),
    /// Function and method calls (a method call is a `Call` of a `Field`).
    Call {
        callee: Box<Expr>,
        args: Vec<Arg>,
    },
    Field {
        base: Box<Expr>,
        name: Ident,
    },
    /// `t.0`
    TupleIndex {
        base: Box<Expr>,
        index: u32,
    },
    /// Indexing or generic instantiation: `xs[i]`, `max[f32]`, `Map[str, int]`.
    Index {
        base: Box<Expr>,
        args: Vec<IndexArg>,
    },
    If {
        cond: Box<Expr>,
        then: Block,
        /// A [`ExprKind::Block`], [`ExprKind::If`] or [`ExprKind::IfLet`].
        else_: Option<Box<Expr>>,
    },
    IfLet {
        pat: Box<Pat>,
        scrutinee: Box<Expr>,
        then: Block,
        else_: Option<Box<Expr>>,
    },
    Match {
        scrutinee: Box<Expr>,
        arms: Vec<MatchArm>,
    },
    Closure(Closure),
    Unsafe(Block),
    Block(Block),
    Error,
}

// ---- patterns ----

#[derive(Clone, Debug, PartialEq)]
pub struct Pat {
    pub id: NodeId,
    pub span: Span,
    pub kind: PatKind,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Lit {
    Int(u64),
    Float(f64),
    Char(char),
    Bool(bool),
    Str(String),
}

/// A literal in a pattern, possibly negated (`-1`).
#[derive(Clone, Debug, PartialEq)]
pub struct PatLit {
    pub span: Span,
    pub neg: bool,
    pub lit: Lit,
}

/// A path in a pattern: `Circle`, `Shape.Circle` or `.Circle`.
#[derive(Clone, Debug, PartialEq)]
pub struct PatPath {
    pub leading_dot: bool,
    pub segments: Vec<Ident>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FieldPat {
    pub name: Ident,
    /// `Vec2i { x }`: the pattern is the binding `x`.
    pub shorthand: bool,
    pub pat: Pat,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PatKind {
    /// `_`
    Wild,
    /// A single name: a binding when lower-case, a variant or constant when
    /// capitalised (checked later, DESIGN §8.2). See [`Ident::is_capitalised`].
    Ident(Ident),
    Lit(PatLit),
    /// `0..10`, `0..=9`, `'a'..='z'`, and the open forms `0..` and `..9`.
    Range {
        lo: Option<PatLit>,
        hi: Option<PatLit>,
        inclusive: bool,
    },
    /// `Shape.Circle`, `.Circle` (no arguments) or `Circle(r)`, `.Circle(r)`
    /// (with `args`).
    Variant {
        path: PatPath,
        args: Option<Vec<Pat>>,
    },
    Some(Box<Pat>),
    None,
    Ok(Box<Pat>),
    Err(Box<Pat>),
    /// `(a, b)`, `(a,)` and `()`.
    Tuple(Vec<Pat>),
    /// `Vec2i { x, y: _, .. }`
    Struct {
        path: PatPath,
        fields: Vec<FieldPat>,
        rest: bool,
    },
    /// `a | b`
    Or(Vec<Pat>),
    /// `pat as name`
    Bind {
        pat: Box<Pat>,
        name: Ident,
    },
    Error,
}
