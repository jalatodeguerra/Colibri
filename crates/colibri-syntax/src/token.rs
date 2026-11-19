//! Tokens produced by the lexer (DESIGN §3).

use std::fmt;

use crate::Span;

/// A token and where it came from.
#[derive(Clone, Debug, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

/// A string literal after escape processing, split into literal text and
/// interpolation holes.
#[derive(Clone, Debug, PartialEq)]
pub struct StrLit {
    pub parts: Vec<StrPart>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum StrPart {
    /// Literal text with escapes (and `{{` / `}}`) already processed.
    Lit(String),
    /// An interpolation hole `{expr}` or `{expr:spec}`.
    Interp {
        /// The expression text inside the braces, as a span of the original
        /// file (the parser re-lexes and parses it).
        expr: Span,
        /// The format spec after a top-level `:`, e.g. `.2` in `{x:.2}`.
        spec: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub enum TokenKind {
    // Keywords (DESIGN §3.3). `use` is deliberately not one.
    Fn,
    Let,
    Var,
    Const,
    Type,
    Return,
    If,
    Else,
    For,
    In,
    While,
    Break,
    Continue,
    Match,
    Struct,
    Class,
    Enum,
    Interface,
    Impl,
    Import,
    Pub,
    As,
    Is,
    SelfKw,
    Own,
    Borrow,
    Weak,
    Extern,
    Unsafe,
    Defer,
    Test,
    Go,
    Some,
    None,
    Ok,
    Err,
    Or,
    And,
    Not,
    True,
    False,

    // Names and literals.
    Ident(String),
    /// A lone `_`.
    Underscore,
    Int(u64),
    Float(f64),
    Char(char),
    /// A loop label such as `'outer` (stored without the quote).
    Label(String),
    Str(StrLit),

    // Brackets and separators.
    LParen,
    RParen,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    Comma,
    Dot,
    DotDot,
    DotDotEq,
    Colon,
    /// An explicit `;` or an implicit terminator inserted at a newline.
    Semi,
    Arrow,
    FatArrow,

    // Operators.
    Eq,
    EqEq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    PlusEq,
    MinusEq,
    StarEq,
    SlashEq,
    PercentEq,
    AndAnd,
    OrOr,
    Bang,
    Question,
    At,
    Pipe,
    Amp,
    Caret,
    Tilde,
    Shl,
    Shr,

    /// A `///` doc comment (text after the slashes, one leading space removed).
    DocComment(String),
    /// A raw line break. Removed by [`crate::insert_terminators`].
    Newline,
    /// A character the lexer could not make sense of (already diagnosed).
    Error,
    Eof,
}

/// Reserved words, in no particular order.
pub(crate) const KEYWORDS: &[(&str, TokenKind)] = &[
    ("fn", TokenKind::Fn),
    ("let", TokenKind::Let),
    ("var", TokenKind::Var),
    ("const", TokenKind::Const),
    ("type", TokenKind::Type),
    ("return", TokenKind::Return),
    ("if", TokenKind::If),
    ("else", TokenKind::Else),
    ("for", TokenKind::For),
    ("in", TokenKind::In),
    ("while", TokenKind::While),
    ("break", TokenKind::Break),
    ("continue", TokenKind::Continue),
    ("match", TokenKind::Match),
    ("struct", TokenKind::Struct),
    ("class", TokenKind::Class),
    ("enum", TokenKind::Enum),
    ("interface", TokenKind::Interface),
    ("impl", TokenKind::Impl),
    ("import", TokenKind::Import),
    ("pub", TokenKind::Pub),
    ("as", TokenKind::As),
    ("is", TokenKind::Is),
    ("self", TokenKind::SelfKw),
    ("own", TokenKind::Own),
    ("borrow", TokenKind::Borrow),
    ("weak", TokenKind::Weak),
    ("extern", TokenKind::Extern),
    ("unsafe", TokenKind::Unsafe),
    ("defer", TokenKind::Defer),
    ("test", TokenKind::Test),
    ("go", TokenKind::Go),
    ("some", TokenKind::Some),
    ("none", TokenKind::None),
    ("ok", TokenKind::Ok),
    ("err", TokenKind::Err),
    ("or", TokenKind::Or),
    ("and", TokenKind::And),
    ("not", TokenKind::Not),
    ("true", TokenKind::True),
    ("false", TokenKind::False),
];

/// Punctuation and operators, longest spelling first so that scanning the
/// table in order gives maximal munch.
pub(crate) const OPERATORS: &[(&str, TokenKind)] = &[
    ("..=", TokenKind::DotDotEq),
    ("..", TokenKind::DotDot),
    ("->", TokenKind::Arrow),
    ("=>", TokenKind::FatArrow),
    ("==", TokenKind::EqEq),
    ("!=", TokenKind::NotEq),
    ("<=", TokenKind::LtEq),
    (">=", TokenKind::GtEq),
    ("<<", TokenKind::Shl),
    (">>", TokenKind::Shr),
    ("+=", TokenKind::PlusEq),
    ("-=", TokenKind::MinusEq),
    ("*=", TokenKind::StarEq),
    ("/=", TokenKind::SlashEq),
    ("%=", TokenKind::PercentEq),
    ("&&", TokenKind::AndAnd),
    ("||", TokenKind::OrOr),
    ("(", TokenKind::LParen),
    (")", TokenKind::RParen),
    ("[", TokenKind::LBracket),
    ("]", TokenKind::RBracket),
    ("{", TokenKind::LBrace),
    ("}", TokenKind::RBrace),
    (",", TokenKind::Comma),
    (".", TokenKind::Dot),
    (":", TokenKind::Colon),
    (";", TokenKind::Semi),
    ("=", TokenKind::Eq),
    ("<", TokenKind::Lt),
    (">", TokenKind::Gt),
    ("+", TokenKind::Plus),
    ("-", TokenKind::Minus),
    ("*", TokenKind::Star),
    ("/", TokenKind::Slash),
    ("%", TokenKind::Percent),
    ("!", TokenKind::Bang),
    ("?", TokenKind::Question),
    ("@", TokenKind::At),
    ("|", TokenKind::Pipe),
    ("&", TokenKind::Amp),
    ("^", TokenKind::Caret),
    ("~", TokenKind::Tilde),
];

impl TokenKind {
    /// The keyword spelled `word`, if it is one.
    pub fn keyword(word: &str) -> Option<TokenKind> {
        KEYWORDS
            .iter()
            .find(|(text, _)| *text == word)
            .map(|(_, kind)| kind.clone())
    }

    /// The fixed source text of a keyword or punctuation token.
    pub fn fixed_text(&self) -> Option<&'static str> {
        KEYWORDS
            .iter()
            .chain(OPERATORS)
            .find(|(_, kind)| kind == self)
            .map(|(text, _)| *text)
    }
}

/// A compact rendering for diagnostics and tests: `fn`, `(`, `Ident(x)`,
/// `Int(42)`, `Semi`, ...
impl fmt::Display for TokenKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(text) = self.fixed_text() {
            // `;` is spelled `Semi` because the same kind is also inserted
            // implicitly at newlines.
            return if *self == TokenKind::Semi {
                f.write_str("Semi")
            } else {
                f.write_str(text)
            };
        }
        match self {
            TokenKind::Ident(s) => write!(f, "Ident({s})"),
            TokenKind::Underscore => f.write_str("_"),
            TokenKind::Int(n) => write!(f, "Int({n})"),
            TokenKind::Float(x) => write!(f, "Float({x:?})"),
            TokenKind::Char(c) => write!(f, "Char({c:?})"),
            TokenKind::Label(s) => write!(f, "Label('{s})"),
            TokenKind::Str(lit) => {
                f.write_str("Str(")?;
                for part in &lit.parts {
                    match part {
                        StrPart::Lit(s) => write!(f, "{s:?}")?,
                        StrPart::Interp { expr, spec: None } => {
                            write!(f, "{{{}..{}}}", expr.start, expr.end)?
                        }
                        StrPart::Interp {
                            expr,
                            spec: Some(spec),
                        } => write!(f, "{{{}..{}:{spec}}}", expr.start, expr.end)?,
                    }
                }
                f.write_str(")")
            }
            TokenKind::DocComment(s) => write!(f, "Doc({s:?})"),
            TokenKind::Newline => f.write_str("Newline"),
            TokenKind::Error => f.write_str("Error"),
            TokenKind::Eof => f.write_str("Eof"),
            _ => unreachable!("fixed-text tokens are handled above"),
        }
    }
}
