//! The AST to [`Doc`] printer: the formatting rules themselves.
//!
//! Comments are not in the AST. The printer owns the list of comments found by
//! [`super::comments::scan`] and hands them out in source order as it builds the document:
//! before the element that follows them (`take_leading`), at the end of the line of the element
//! they trail (`take_trailing`), or before a closing bracket (`dangling`).

use super::comments::{Comment, Tok, scan};
use super::doc::{Doc, broken_group, concat, group, if_break, indent, join, render, shared, text};
use crate::ast::*;
use std::cell::Cell;
use std::rc::Rc;

use crate::{Span, TokenKind};

/// The line width.
pub(crate) const WIDTH: usize = 100;

pub(crate) fn format_file(src: &str, file: &File) -> String {
    let (toks, comments) = scan(src);
    let mut f = Fmt {
        src,
        toks,
        comments,
        next: 0,
        taken_end: 0,
        last_elem_end: 0,
    };
    let mut seq = Seq::new(None);
    let count = file.items.len();
    for (i, top) in file.items.iter().enumerate() {
        let last = i + 1 == count;
        match top {
            TopLevel::Item(item) => {
                f.seq_push(&mut seq, item.span.start, item.span.end, |f| {
                    let doc = f.item(item);
                    semi_if(doc, !last && item_open_end(item))
                });
            }
            TopLevel::Stmt(stmt) => {
                f.seq_push(&mut seq, stmt.span.start, stmt.span.end, |f| {
                    let doc = f.stmt(stmt);
                    semi_if(doc, !last && stmt_open_end(stmt))
                });
            }
        }
    }
    let dangling = f.dangling(src.len() as u32, seq.last_end);
    seq.parts.push(dangling);
    let mut out = render(&concat(seq.parts), WIDTH);
    out.truncate(out.trim_end().len());
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

/// Elements of a block, a file or a declaration body, with their separators.
struct Seq {
    /// Separators and elements, ready to place after the opening brace.
    parts: Vec<Doc>,
    /// Each element on its own (with its comments).
    elems: Vec<Doc>,
    /// Where the previous element (or its trailing comment) ended.
    last_end: Option<u32>,
    /// Comments on the line of the opening brace.
    open_comments: Doc,
    /// Written on one line, without comments: may stay on one line.
    one_line: bool,
}

impl Seq {
    fn new(open_comments: Option<Doc>) -> Seq {
        Seq {
            parts: Vec::new(),
            elems: Vec::new(),
            last_end: None,
            open_comments: open_comments.unwrap_or(Doc::Nil),
            one_line: false,
        }
    }
}

/// How a bracketed list is written.
#[derive(Clone, Copy)]
struct ListSpec {
    open: &'static str,
    close: &'static str,
    /// Spaces inside the brackets when printed flat (`{ a, b }`).
    pad: bool,
    /// The trailing comma is part of the syntax (one-element tuples).
    force_comma: bool,
}

const PAREN: ListSpec = ListSpec {
    open: "(",
    close: ")",
    pad: false,
    force_comma: false,
};
const BRACKET: ListSpec = ListSpec {
    open: "[",
    close: "]",
    pad: false,
    force_comma: false,
};
const BRACE: ListSpec = ListSpec {
    open: "{",
    close: "}",
    pad: false,
    force_comma: false,
};
const PADDED_BRACE: ListSpec = ListSpec {
    open: "{",
    close: "}",
    pad: true,
    force_comma: false,
};

struct ListParts {
    doc: Doc,
    magic: bool,
    dangling: bool,
}

struct Fmt<'a> {
    src: &'a str,
    toks: Vec<Tok>,
    comments: Vec<Comment>,
    /// Index of the next comment nobody has printed.
    next: usize,
    /// End of the last comment handed out.
    taken_end: u32,
    /// Where the last list element (with its trailing comments) ended.
    last_elem_end: u32,
}

fn semi_if(doc: Doc, yes: bool) -> Doc {
    if yes {
        concat(vec![doc, text(";")])
    } else {
        doc
    }
}

fn is_nil(doc: &Doc) -> bool {
    match doc {
        Doc::Nil => true,
        Doc::Concat(parts) => parts.iter().all(is_nil),
        _ => false,
    }
}

fn line_suffix(c: &Comment) -> Doc {
    let suffix = Doc::LineSuffix(format!(" {}", c.text));
    if c.block {
        suffix
    } else {
        concat(vec![suffix, Doc::BreakParent])
    }
}

fn binop_text(op: BinOp) -> &'static str {
    match op {
        BinOp::Add => "+",
        BinOp::Sub => "-",
        BinOp::Mul => "*",
        BinOp::Div => "/",
        BinOp::Rem => "%",
        BinOp::Eq => "==",
        BinOp::Ne => "!=",
        BinOp::Lt => "<",
        BinOp::Le => "<=",
        BinOp::Gt => ">",
        BinOp::Ge => ">=",
        BinOp::And => "&&",
        BinOp::Or => "||",
        BinOp::Is => "is",
        BinOp::BitOr => "|",
        BinOp::BitXor => "^",
        BinOp::BitAnd => "&",
        BinOp::Shl => "<<",
        BinOp::Shr => ">>",
    }
}

/// The binding power of a binary operator (the parser's table).
fn binop_power(op: BinOp) -> u8 {
    match op {
        BinOp::Or => 4,
        BinOp::And => 6,
        BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => 8,
        BinOp::Is => 10,
        BinOp::BitOr => 12,
        BinOp::BitXor => 14,
        BinOp::BitAnd => 16,
        BinOp::Shl | BinOp::Shr => 18,
        BinOp::Add | BinOp::Sub => 22,
        BinOp::Mul | BinOp::Div | BinOp::Rem => 24,
    }
}

/// The binding power of `a or b`.
const OR_POWER: u8 = 2;

/// A line that starts with these operators continues the previous statement (DESIGN §3.2), so
/// a long chain breaks before them. Every other operator stays at the end of its line: a line
/// ending with an operator continues too, in every context.
fn breaks_before(op: &str) -> bool {
    matches!(op, "&&" | "||" | "or")
}

fn assign_text(op: AssignOp) -> &'static str {
    match op {
        AssignOp::Assign => "=",
        AssignOp::Add => "+=",
        AssignOp::Sub => "-=",
        AssignOp::Mul => "*=",
        AssignOp::Div => "/=",
        AssignOp::Rem => "%=",
        AssignOp::BitAnd => "&=",
        AssignOp::BitOr => "|=",
        AssignOp::BitXor => "^=",
        AssignOp::Shl => "<<=",
        AssignOp::Shr => ">>=",
    }
}

fn bind_text(kind: BindKind) -> &'static str {
    match kind {
        BindKind::Let => "let",
        BindKind::Var => "var",
        BindKind::Const => "const",
    }
}

/// Whether an expression ends in a construct with a body that spans lines (a closure with a
/// block, a `match`), so that a call whose last argument it is can keep it on the line of the
/// call: `print(xs.any(fn(x) {`.
fn huggable(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Closure(c) => matches!(*c.body, FnBody::Block(_)),
        ExprKind::Match { .. } | ExprKind::Block(_) | ExprKind::Unsafe(_) => true,
        ExprKind::Call { args, .. } => args.last().is_some_and(|a| huggable(&a.value)),
        ExprKind::Binary { rhs, .. } | ExprKind::Or { rhs, .. } => huggable(rhs),
        _ => false,
    }
}

/// Whether an expression ends in an open range (`a..`). A line break after `..` does not end a
/// statement, so such a statement keeps a `;` (or, in a `match`, a `,`).
fn open_end(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Range { end: None, .. } => true,
        ExprKind::Range { end: Some(x), .. } => open_end(x),
        ExprKind::Binary { rhs, .. } | ExprKind::Or { rhs, .. } => open_end(rhs),
        ExprKind::Unary { expr, .. } => open_end(expr),
        ExprKind::Closure(c) => matches!(&*c.body, FnBody::Expr(b) if open_end(b)),
        _ => false,
    }
}

fn stmt_open_end(s: &Stmt) -> bool {
    match &s.kind {
        StmtKind::Let(l) => open_end(&l.init),
        StmtKind::Assign { value, .. } => open_end(value),
        StmtKind::Expr(e) | StmtKind::Go(e) => open_end(e),
        StmtKind::Return(Some(e)) => open_end(e),
        StmtKind::Break { value: Some(e), .. } => open_end(e),
        StmtKind::Defer(inner) => stmt_open_end(inner),
        _ => false,
    }
}

fn item_open_end(item: &Item) -> bool {
    match &item.kind {
        ItemKind::Binding(l) => open_end(&l.init),
        ItemKind::Fn(FnDecl {
            body: Some(FnBody::Expr(e)),
            ..
        }) => open_end(e),
        _ => false,
    }
}

/// One operand and the operator that precedes it, flattened from a left-nested chain.
struct Operand<'e> {
    op: &'static str,
    expr: &'e Expr,
    /// Where the operator is (for comments before it).
    op_pos: u32,
}

impl<'a> Fmt<'a> {
    // ---- source and token helpers ----

    fn sl(&self, a: u32, b: u32) -> &'a str {
        if a >= b {
            return "";
        }
        &self.src[a as usize..b as usize]
    }

    fn slice(&self, span: Span) -> &'a str {
        self.sl(span.start, span.end)
    }

    /// Index of the first token that starts at or after `pos`.
    fn tok_from(&self, pos: u32) -> usize {
        self.toks.partition_point(|t| t.start < pos)
    }

    fn find_close(&self, open: u32) -> Option<u32> {
        let mut depth = 0i32;
        for tok in &self.toks[self.tok_from(open)..] {
            match tok.kind {
                TokenKind::LParen | TokenKind::LBracket | TokenKind::LBrace => depth += 1,
                TokenKind::RParen | TokenKind::RBracket | TokenKind::RBrace => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(tok.start);
                    }
                }
                _ => {}
            }
        }
        None
    }

    fn find_open(&self, close: u32) -> u32 {
        let mut depth = 0i32;
        let end = self.tok_from(close) + 1;
        for tok in self.toks[..end.min(self.toks.len())].iter().rev() {
            match tok.kind {
                TokenKind::RParen | TokenKind::RBracket | TokenKind::RBrace => depth += 1,
                TokenKind::LParen | TokenKind::LBracket | TokenKind::LBrace => {
                    depth -= 1;
                    if depth == 0 {
                        return tok.start;
                    }
                }
                _ => {}
            }
        }
        close
    }

    /// The bracket of `kind` that is the first token at or after `pos`: (open, close).
    fn bracket_after(&self, pos: u32, kind: &TokenKind) -> Option<(u32, u32)> {
        let tok = self.toks.get(self.tok_from(pos))?;
        if tok.kind != *kind {
            return None;
        }
        Some((tok.start, self.find_close(tok.start)?))
    }

    /// Whether the list closed at `close` has a trailing comma followed by a line break (the
    /// "magic trailing comma": such a list stays one element per line).
    fn magic(&self, close: u32) -> bool {
        let i = self.tok_from(close);
        if i == 0 || self.toks.get(i).is_none_or(|t| t.start != close) {
            return false;
        }
        let comma = &self.toks[i - 1];
        comma.kind == TokenKind::Comma && self.sl(comma.end, close).contains('\n')
    }

    fn only_separators(&self, a: u32, b: u32) -> bool {
        self.toks[self.tok_from(a)..]
            .iter()
            .take_while(|t| t.start < b)
            .all(|t| matches!(t.kind, TokenKind::Comma | TokenKind::Semi))
    }

    /// Whether a line break (or the end of the file) comes before the next token after `pos`.
    fn ends_line(&self, pos: u32) -> bool {
        match self.toks.get(self.tok_from(pos)) {
            Some(tok) => self.sl(pos, tok.start).contains('\n'),
            None => true,
        }
    }

    fn gap_blank(&self, a: u32, b: u32) -> bool {
        self.sl(a, b).matches('\n').count() >= 2
    }

    fn has_comment_between(&self, open: u32, close: u32) -> bool {
        let i = self.comments.partition_point(|c| c.start <= open);
        self.comments.get(i).is_some_and(|c| c.start < close)
    }

    // ---- comments ----

    fn next_before(&self, pos: u32) -> Option<&Comment> {
        self.comments.get(self.next).filter(|c| c.start < pos)
    }

    fn take(&mut self) -> Comment {
        let c = self.comments[self.next].clone();
        self.next += 1;
        self.taken_end = self.taken_end.max(c.end);
        c
    }

    /// The comments before `pos`, each on its own line (or inline, for short block comments);
    /// the document that follows continues after them. The flag says one is on its own line.
    fn take_leading_ex(&mut self, pos: u32) -> (Doc, bool) {
        let (suffixes, lines, own) = self.take_leading_parts(pos);
        (concat(vec![suffixes, lines]), own)
    }

    /// Like `take_leading_ex`, with the comments that go to the end of the line (placed before
    /// a line break) apart from those that go on lines of their own.
    fn take_leading_parts(&mut self, pos: u32) -> (Doc, Doc, bool) {
        let mut parts = Vec::new();
        let mut suffixes = Vec::new();
        let mut own = false;
        while self.next_before(pos).is_some() {
            let c = self.take();
            let following = self.next_before(pos).map_or(pos, |n| n.start);
            let gap = self.sl(c.end, following);
            if c.block && !c.text.contains('\n') && !gap.contains('\n') {
                parts.push(text(c.text));
                parts.push(text(" "));
            } else if c.own_line || c.doc {
                own = true;
                parts.push(text(c.text));
                parts.push(Doc::HardLine);
                if gap.matches('\n').count() >= 2 {
                    parts.push(Doc::HardLine);
                }
            } else {
                suffixes.push(line_suffix(&c));
            }
        }
        (concat(suffixes), concat(parts), own)
    }

    fn take_leading(&mut self, pos: u32) -> Doc {
        self.take_leading_ex(pos).0
    }

    /// Comments on the same line as the element that ended at `end`.
    fn take_trailing(&mut self, end: u32) -> Doc {
        let mut parts = Vec::new();
        let mut cursor = end;
        while let Some(c) = self.comments.get(self.next) {
            if c.own_line
                || c.doc
                || !self.only_separators(cursor, c.start)
                || !self.ends_line(c.end)
            {
                break;
            }
            let c = self.take();
            cursor = c.end;
            parts.push(line_suffix(&c));
        }
        concat(parts)
    }

    /// Comments inside an element that nothing printed: moved to the end of its line.
    fn flush_inside(&mut self, end: u32) -> Doc {
        let mut parts = Vec::new();
        while self.next_before(end).is_some() {
            let c = self.take();
            parts.push(line_suffix(&c));
        }
        concat(parts)
    }

    /// The comments before the closing bracket at `close`. `last_end` is where the previous
    /// element ended (`None` when this is the first thing in the body).
    fn dangling(&mut self, close: u32, last_end: Option<u32>) -> Doc {
        let mut parts = Vec::new();
        let mut prev = last_end;
        while self.next_before(close).is_some() {
            let c = self.take();
            if c.own_line || c.doc {
                if let Some(p) = prev {
                    if self.sl(p, c.start).contains('\n') {
                        parts.push(Doc::HardLine);
                        if self.gap_blank(p, c.start) {
                            parts.push(Doc::HardLine);
                        }
                    } else {
                        parts.push(text(" "));
                    }
                }
                parts.push(text(c.text.clone()));
                parts.push(Doc::BreakParent);
            } else {
                parts.push(line_suffix(&c));
            }
            prev = Some(c.end);
        }
        concat(parts)
    }

    /// An element of a list: its leading and trailing comments around `build`.
    fn element(&mut self, start: u32, end: u32, build: impl FnOnce(&mut Self) -> Doc) -> Doc {
        let lead = self.take_leading(start);
        let doc = build(self);
        let inside = self.flush_inside(end);
        let trail = self.take_trailing(end);
        self.last_elem_end = end.max(self.taken_end);
        concat(vec![lead, doc, inside, trail])
    }

    // ---- sequences (blocks, files, declaration bodies) ----

    fn new_seq(&mut self, open: u32, close: u32) -> Seq {
        let one_line =
            !self.sl(open, close).contains('\n') && !self.has_comment_between(open, close);
        let open_comments = self.take_trailing(open + 1);
        let mut seq = Seq::new(Some(open_comments));
        seq.one_line = one_line;
        seq
    }

    fn seq_push(
        &mut self,
        seq: &mut Seq,
        start: u32,
        end: u32,
        build: impl FnOnce(&mut Self) -> Doc,
    ) {
        if let Some(prev) = seq.last_end {
            let group_start = self
                .comments
                .get(self.next)
                .filter(|c| c.start < start)
                .map_or(start, |c| c.start);
            seq.parts.push(Doc::HardLine);
            if self.gap_blank(prev, group_start) {
                seq.parts.push(Doc::HardLine);
            }
        }
        let elem = shared(self.element(start, end, build));
        seq.parts.push(elem.clone());
        seq.elems.push(elem);
        seq.last_end = Some(end.max(self.taken_end));
    }

    /// `{ .. }` around a finished sequence. `one_element` allows `{ x }` on one line for a
    /// single element; `many` allows `{ a, b }` for declaration bodies.
    fn seq_braces(&mut self, seq: Seq, close: u32, many: bool) -> Doc {
        let dangling = self.dangling(close, seq.last_end);
        let quiet = is_nil(&dangling) && is_nil(&seq.open_comments);
        if seq.elems.is_empty() && quiet {
            return text("{}");
        }
        let one_line = seq.one_line && quiet && !seq.elems.is_empty();
        let single = if one_line && seq.elems.len() == 1 && !many {
            Some(seq.elems[0].clone())
        } else {
            None
        };
        let flat_many = if one_line && many {
            Some(join(&text(", "), seq.elems.clone()))
        } else {
            None
        };
        let mut body = seq.parts;
        body.push(dangling);
        let expanded = if body.iter().all(is_nil) {
            // Only a comment after the `{`.
            concat(vec![text("{"), seq.open_comments, Doc::HardLine, text("}")])
        } else {
            concat(vec![
                text("{"),
                seq.open_comments,
                indent(concat(vec![Doc::HardLine, concat(body)])),
                Doc::HardLine,
                text("}"),
            ])
        };
        if let Some(elem) = single {
            return group(concat(vec![
                text("{"),
                indent(concat(vec![Doc::Line, elem])),
                Doc::Line,
                text("}"),
            ]));
        }
        if let Some(flat) = flat_many {
            let first = concat(vec![text("{ "), flat, text(" }")]);
            return Doc::Either(Box::new(first), Box::new(expanded));
        }
        expanded
    }

    // ---- lists ----

    fn open_comments(&mut self, close: u32) -> Doc {
        let open = self.find_open(close);
        self.take_trailing(open + 1)
    }

    fn list_parts(
        &mut self,
        spec: ListSpec,
        close: u32,
        open_comments: Doc,
        items: Vec<Doc>,
    ) -> ListParts {
        let last_end = if items.is_empty() {
            None
        } else {
            Some(self.last_elem_end)
        };
        let dangling = self.dangling(close, last_end);
        let magic = self.magic(close);
        let has_dangling = !is_nil(&dangling);
        if items.is_empty() && !has_dangling {
            return ListParts {
                doc: concat(vec![text(spec.open), open_comments, text(spec.close)]),
                magic,
                dangling: false,
            };
        }
        let edge = if spec.pad { Doc::Line } else { Doc::SoftLine };
        let comma = if items.is_empty() {
            Doc::Nil
        } else if spec.force_comma {
            text(",")
        } else {
            if_break(text(","), Doc::Nil)
        };
        let sep = concat(vec![text(","), Doc::Line]);
        let inner = concat(vec![
            text(spec.open),
            open_comments,
            indent(concat(vec![
                edge.clone(),
                join(&sep, items),
                comma,
                dangling,
            ])),
            edge,
            text(spec.close),
        ]);
        let doc = if magic {
            broken_group(inner)
        } else {
            group(inner)
        };
        ListParts {
            doc,
            magic,
            dangling: has_dangling,
        }
    }

    fn list(&mut self, spec: ListSpec, close: u32, items: Vec<Doc>, open_comments: Doc) -> Doc {
        self.list_parts(spec, close, open_comments, items).doc
    }

    // ---- items ----

    fn item(&mut self, item: &Item) -> Doc {
        let mut parts = Vec::new();
        for (i, attr) in item.attrs.iter().enumerate() {
            if i > 0 {
                parts.push(self.take_leading(attr.span.start));
            }
            parts.push(self.attr(attr));
            parts.push(self.take_trailing(attr.span.end));
            parts.push(Doc::HardLine);
        }
        if let Some(last) = item.attrs.last()
            && let Some(tok) = self.toks.get(self.tok_from(last.span.end))
        {
            let pos = tok.start;
            parts.push(self.take_leading(pos));
        }
        if item.public {
            parts.push(text("pub "));
        }
        parts.push(self.item_kind(item));
        concat(parts)
    }

    fn attr(&mut self, attr: &Attr) -> Doc {
        let mut parts = vec![text(format!("@{}", attr.name.name))];
        if !attr.args.is_empty() {
            let close = attr.span.end - 1;
            let open_c = self.open_comments(close);
            let items = attr
                .args
                .iter()
                .map(|a| self.element(a.span.start, a.span.end, |f| f.expr(a)))
                .collect();
            parts.push(self.list(PAREN, close, items, open_c));
        }
        concat(parts)
    }

    /// A string token at or after `from`, as written.
    fn string_after(&self, from: u32) -> String {
        self.toks[self.tok_from(from)..]
            .iter()
            .find(|t| matches!(t.kind, TokenKind::Str(_)))
            .map_or_else(String::new, |t| self.sl(t.start, t.end).to_string())
    }

    fn item_kind(&mut self, item: &Item) -> Doc {
        let close = item.span.end.saturating_sub(1);
        match &item.kind {
            ItemKind::Fn(decl) => self.fn_decl(decl),
            ItemKind::Struct(s) => {
                let kw = if s.kind == StructKind::Class {
                    "class"
                } else {
                    "struct"
                };
                let mut parts = vec![text(format!("{kw} {}", s.name.name))];
                parts.push(self.generics(s.name.span.end, &s.generics));
                parts.push(text(" "));
                let open = self.find_open(close);
                let mut seq = self.new_seq(open, close);
                for field in &s.fields {
                    self.seq_push(&mut seq, field.span.start, field.span.end, |f| {
                        f.field(field)
                    });
                }
                parts.push(self.seq_braces(seq, close, true));
                concat(parts)
            }
            ItemKind::Enum(e) => {
                let mut parts = vec![text(format!("enum {}", e.name.name))];
                parts.push(self.generics(e.name.span.end, &e.generics));
                parts.push(text(" "));
                let open = self.find_open(close);
                let mut seq = self.new_seq(open, close);
                for variant in &e.variants {
                    self.seq_push(&mut seq, variant.span.start, variant.span.end, |f| {
                        f.variant(variant)
                    });
                }
                parts.push(self.seq_braces(seq, close, true));
                concat(parts)
            }
            ItemKind::Interface(i) => {
                let mut parts = vec![text(format!("interface {}", i.name.name))];
                parts.push(self.generics(i.name.span.end, &i.generics));
                parts.push(text(" "));
                parts.push(self.members(&i.items, close));
                concat(parts)
            }
            ItemKind::Impl(i) => {
                let mut parts = vec![text("impl")];
                let after_kw = self.toks[self.tok_from(item.span.start)..]
                    .iter()
                    .find(|t| t.kind == TokenKind::Impl)
                    .map_or(item.span.start, |t| t.end);
                parts.push(self.generics(after_kw, &i.generics));
                parts.push(text(" "));
                if let Some(iface) = &i.interface {
                    parts.push(self.ty(iface));
                    parts.push(text(" for "));
                }
                parts.push(self.ty(&i.target));
                parts.push(text(" "));
                parts.push(self.members(&i.items, close));
                concat(parts)
            }
            ItemKind::TypeAlias(t) => {
                let mut parts = vec![text(format!("type {}", t.name.name))];
                parts.push(self.generics(t.name.span.end, &t.generics));
                parts.push(text(" = "));
                parts.push(self.ty(&t.ty));
                concat(parts)
            }
            ItemKind::Binding(l) => self.let_stmt(l),
            ItemKind::Import(i) => self.import(i, close),
            ItemKind::Extern(e) => {
                let from = item.attrs.last().map_or(item.span.start, |a| a.span.end);
                let mut parts = vec![text(format!("extern {} ", self.string_after(from)))];
                parts.push(self.members(&e.items, close));
                concat(parts)
            }
            ItemKind::Test(t) => {
                let from = item.attrs.last().map_or(item.span.start, |a| a.span.end);
                let mut parts = vec![text(format!("test {} ", self.string_after(from)))];
                parts.push(self.block(&t.body));
                concat(parts)
            }
            ItemKind::Error => text(self.slice(item.span)),
        }
    }

    /// The `fn` members of an `interface`, `impl` or `extern` block.
    fn members(&mut self, items: &[Item], close: u32) -> Doc {
        let open = self.find_open(close);
        let mut seq = self.new_seq(open, close);
        for item in items {
            self.seq_push(&mut seq, item.span.start, item.span.end, |f| f.item(item));
        }
        seq.one_line = false;
        self.seq_braces(seq, close, false)
    }

    fn field(&mut self, field: &Field) -> Doc {
        let mut parts = Vec::new();
        if field.public {
            parts.push(text("pub "));
        }
        parts.push(text(format!("{}: ", field.name.name)));
        parts.push(self.ty(&field.ty));
        if let Some(default) = &field.default {
            parts.push(text(" = "));
            parts.push(self.expr(default));
        }
        concat(parts)
    }

    fn variant(&mut self, variant: &Variant) -> Doc {
        let mut parts = vec![text(variant.name.name.clone())];
        let close = variant.span.end.saturating_sub(1);
        match &variant.payload {
            VariantPayload::Unit => {}
            VariantPayload::Positional(types) => {
                let open_c = self.open_comments(close);
                let items = types
                    .iter()
                    .map(|t| self.element(t.span.start, t.span.end, |f| f.ty(t)))
                    .collect();
                parts.push(self.list(PAREN, close, items, open_c));
            }
            VariantPayload::Named(fields) => {
                let open_c = self.open_comments(close);
                let items = fields
                    .iter()
                    .map(|field| {
                        let span = field.name.span.to(field.ty.span);
                        self.element(span.start, span.end, |f| {
                            concat(vec![
                                text(format!("{}: ", field.name.name)),
                                f.ty(&field.ty),
                            ])
                        })
                    })
                    .collect();
                parts.push(self.list(PAREN, close, items, open_c));
            }
        }
        concat(parts)
    }

    fn import(&mut self, import: &Import, close: u32) -> Doc {
        let path: Vec<&str> = import.path.iter().map(|s| s.name.as_str()).collect();
        let mut parts = vec![text(format!("import {}", path.join(".")))];
        match &import.kind {
            ImportKind::Plain => {}
            ImportKind::Alias(alias) => parts.push(text(format!(" as {}", alias.name))),
            ImportKind::Names(names) => {
                parts.push(text("."));
                let open_c = self.open_comments(close);
                let items = names
                    .iter()
                    .map(|n| {
                        let end = n.alias.as_ref().map_or(n.name.span.end, |a| a.span.end);
                        self.element(n.name.span.start, end, |_| match &n.alias {
                            Some(a) => text(format!("{} as {}", n.name.name, a.name)),
                            None => text(n.name.name.clone()),
                        })
                    })
                    .collect();
                parts.push(self.list(BRACE, close, items, open_c));
            }
        }
        concat(parts)
    }

    /// `[T: A + B, U]` after a name that ends at `after`, if the source has one.
    fn generics(&mut self, after: u32, params: &[GenericParam]) -> Doc {
        let Some((_, close)) = self.bracket_after(after, &TokenKind::LBracket) else {
            return Doc::Nil;
        };
        if params.is_empty() {
            return Doc::Nil;
        }
        let open_c = self.open_comments(close);
        let items = params
            .iter()
            .map(|p| {
                let end = p.bounds.last().map_or(p.name.span.end, |b| b.span.end);
                self.element(p.name.span.start, end, |f| f.generic_param(p))
            })
            .collect();
        self.list(BRACKET, close, items, open_c)
    }

    fn generic_param(&mut self, p: &GenericParam) -> Doc {
        let mut parts = vec![text(p.name.name.clone())];
        if !p.bounds.is_empty() {
            parts.push(text(": "));
            let bounds = p.bounds.iter().map(|b| self.ty(b)).collect();
            parts.push(join(&text(" + "), bounds));
        }
        concat(parts)
    }

    fn fn_decl(&mut self, decl: &FnDecl) -> Doc {
        let mut parts = vec![text(format!("fn {}", decl.name.name))];
        parts.push(self.generics(decl.name.span.end, &decl.generics));
        // The parameter list is the first `(` after the name and its generics.
        let mut pos = decl.name.span.end;
        if let Some((_, close)) = self.bracket_after(pos, &TokenKind::LBracket) {
            pos = close + 1;
        }
        let params = self
            .bracket_after(pos, &TokenKind::LParen)
            .map(|(_, close)| self.params(decl.receiver.as_ref(), &decl.params, close));
        parts.push(params.unwrap_or_else(|| text("()")));
        if let Some(ret) = &decl.ret {
            parts.push(text(" -> "));
            parts.push(self.ty(ret));
        }
        match &decl.body {
            Some(FnBody::Block(b)) => {
                parts.push(text(" "));
                parts.push(self.block(b));
            }
            Some(FnBody::Expr(e)) => {
                parts.push(text(" => "));
                parts.push(self.expr(e));
            }
            None => {}
        }
        concat(parts)
    }

    fn params(&mut self, receiver: Option<&Receiver>, params: &[Param], close: u32) -> Doc {
        let open_c = self.open_comments(close);
        let mut items = Vec::new();
        if let Some(r) = receiver {
            items.push(self.element(r.span.start, r.span.end, |_| {
                text(if r.mutable { "var self" } else { "self" })
            }));
        }
        for p in params {
            items.push(self.element(p.span.start, p.span.end, |f| f.param(p)));
        }
        self.list(PAREN, close, items, open_c)
    }

    fn param(&mut self, p: &Param) -> Doc {
        let mut parts = Vec::new();
        if p.mutable {
            parts.push(text("var "));
        }
        parts.push(text(p.name.name.clone()));
        if let Some(ty) = &p.ty {
            parts.push(text(": "));
            parts.push(self.ty(ty));
        }
        if let Some(default) = &p.default {
            parts.push(text(" = "));
            parts.push(self.expr(default));
        }
        concat(parts)
    }

    // ---- types ----

    fn ty(&mut self, t: &TypeExpr) -> Doc {
        match &t.kind {
            TypeKind::Named { path, args } => {
                let names: Vec<&str> = path.iter().map(|s| s.name.as_str()).collect();
                let mut parts = vec![text(names.join("."))];
                if !args.is_empty() {
                    let after = path.last().map_or(t.span.start, |s| s.span.end);
                    if let Some((_, close)) = self.bracket_after(after, &TokenKind::LBracket) {
                        let open_c = self.open_comments(close);
                        let items = args
                            .iter()
                            .map(|a| self.element(a.span.start, a.span.end, |f| f.ty(a)))
                            .collect();
                        parts.push(self.list(BRACKET, close, items, open_c));
                    }
                }
                concat(parts)
            }
            TypeKind::Optional(inner) => {
                let doc = self.ty(inner);
                if matches!(inner.kind, TypeKind::Fn { .. } | TypeKind::Weak(_)) {
                    concat(vec![text("("), doc, text(")?")])
                } else {
                    concat(vec![doc, text("?")])
                }
            }
            TypeKind::Weak(inner) => concat(vec![text("weak "), self.ty(inner)]),
            TypeKind::Tuple(items) => {
                if items.is_empty() {
                    return text("()");
                }
                let close = t.span.end - 1;
                let open_c = self.open_comments(close);
                let docs = items
                    .iter()
                    .map(|a| self.element(a.span.start, a.span.end, |f| f.ty(a)))
                    .collect();
                let spec = ListSpec {
                    force_comma: items.len() == 1,
                    ..PAREN
                };
                self.list(spec, close, docs, open_c)
            }
            TypeKind::Array { elem, len } => concat(vec![
                text("["),
                self.ty(elem),
                text("; "),
                self.expr(len),
                text("]"),
            ]),
            TypeKind::Fn { params, ret } => {
                let mut parts = vec![text("fn")];
                if let Some((_, close)) = self.bracket_after(t.span.start + 2, &TokenKind::LParen) {
                    let open_c = self.open_comments(close);
                    let items = params
                        .iter()
                        .map(|a| self.element(a.span.start, a.span.end, |f| f.ty(a)))
                        .collect();
                    parts.push(self.list(PAREN, close, items, open_c));
                } else {
                    parts.push(text("()"));
                }
                if let Some(ret) = ret {
                    parts.push(text(" -> "));
                    parts.push(self.ty(ret));
                }
                concat(parts)
            }
            TypeKind::Error => text(self.slice(t.span)),
        }
    }

    // ---- statements ----

    fn block(&mut self, b: &Block) -> Doc {
        let open = b.span.start;
        let close = b.span.end.saturating_sub(1);
        let mut seq = self.new_seq(open, close);
        let count = b.stmts.len();
        for (i, s) in b.stmts.iter().enumerate() {
            // An explicit `;` after the last expression statement is what keeps it from being
            // the value of the block, so it stays.
            let last_expr =
                i + 1 == count && b.tail.is_none() && matches!(s.kind, StmtKind::Expr(_));
            let semi = last_expr || (stmt_open_end(s) && (i + 1 < count || b.tail.is_some()));
            self.seq_push(&mut seq, s.span.start, s.span.end, |f| {
                let doc = f.stmt(s);
                semi_if(doc, semi)
            });
        }
        if let Some(tail) = &b.tail {
            self.seq_push(&mut seq, tail.span.start, tail.span.end, |f| f.expr(tail));
        }
        // A single statement that is itself a block construct makes the block expand.
        let nested = |e: &Expr| {
            matches!(
                e.kind,
                ExprKind::If { .. }
                    | ExprKind::IfLet { .. }
                    | ExprKind::Match { .. }
                    | ExprKind::Block(_)
                    | ExprKind::Unsafe(_)
            )
        };
        let nested_stmt = |s: &Stmt| match &s.kind {
            StmtKind::Expr(e) => nested(e),
            StmtKind::For { .. } | StmtKind::While { .. } | StmtKind::WhileLet { .. } => true,
            StmtKind::Defer(inner) => matches!(&inner.kind, StmtKind::Expr(e) if nested(e)),
            _ => false,
        };
        if b.stmts.iter().any(nested_stmt) || b.tail.as_deref().is_some_and(nested) {
            seq.one_line = false;
        }
        self.seq_braces(seq, close, false)
    }

    fn let_stmt(&mut self, l: &LetStmt) -> Doc {
        let mut parts = vec![text(format!("{} ", bind_text(l.kind))), self.pat(&l.pat)];
        if let Some(ty) = &l.ty {
            parts.push(text(": "));
            parts.push(self.ty(ty));
        }
        parts.push(text(" = "));
        parts.push(self.expr(&l.init));
        concat(parts)
    }

    fn label(label: &Option<Label>) -> Doc {
        label
            .as_ref()
            .map_or(Doc::Nil, |l| text(format!("'{}: ", l.name)))
    }

    fn stmt(&mut self, s: &Stmt) -> Doc {
        match &s.kind {
            StmtKind::Let(l) => self.let_stmt(l),
            StmtKind::Assign { op, target, value } => {
                let target = self.expr(target);
                let value = self.expr(value);
                concat(vec![target, text(format!(" {} ", assign_text(*op))), value])
            }
            StmtKind::Expr(e) => self.expr(e),
            StmtKind::Return(value) => match value {
                Some(v) => concat(vec![text("return "), self.expr(v)]),
                None => text("return"),
            },
            StmtKind::Break { label, value } => {
                let mut parts = vec![text("break")];
                if let Some(l) = label {
                    parts.push(text(format!(" '{}", l.name)));
                }
                if let Some(v) = value {
                    parts.push(text(" "));
                    parts.push(self.expr(v));
                }
                concat(parts)
            }
            StmtKind::Continue { label } => match label {
                Some(l) => text(format!("continue '{}", l.name)),
                None => text("continue"),
            },
            StmtKind::Defer(inner) => concat(vec![text("defer "), self.stmt(inner)]),
            StmtKind::Go(e) => concat(vec![text("go "), self.expr(e)]),
            StmtKind::For {
                label,
                mutable,
                pat,
                iter,
                body,
            } => concat(vec![
                Self::label(label),
                text(if *mutable { "for var " } else { "for " }),
                self.pat(pat),
                text(" in "),
                self.expr(iter),
                text(" "),
                self.block(body),
            ]),
            StmtKind::While { label, cond, body } => concat(vec![
                Self::label(label),
                text("while "),
                self.expr(cond),
                text(" "),
                self.block(body),
            ]),
            StmtKind::WhileLet {
                label,
                pat,
                scrutinee,
                body,
            } => concat(vec![
                Self::label(label),
                text("while let "),
                self.pat(pat),
                text(" = "),
                self.expr(scrutinee),
                text(" "),
                self.block(body),
            ]),
            StmtKind::Error => text(self.slice(s.span)),
        }
    }

    // ---- patterns ----

    fn path(path: &PatPath) -> Doc {
        let names: Vec<&str> = path.segments.iter().map(|s| s.name.as_str()).collect();
        let dot = if path.leading_dot { "." } else { "" };
        text(format!("{dot}{}", names.join(".")))
    }

    fn pat(&mut self, p: &Pat) -> Doc {
        match &p.kind {
            PatKind::Wild => text("_"),
            PatKind::Ident(id) => text(id.name.clone()),
            PatKind::Lit(l) => text(self.slice(l.span)),
            PatKind::Range { lo, hi, inclusive } => {
                let lo = lo.as_ref().map_or("", |l| self.slice(l.span));
                let hi = hi.as_ref().map_or("", |l| self.slice(l.span));
                text(format!("{lo}{}{hi}", if *inclusive { "..=" } else { ".." }))
            }
            PatKind::Variant { path, args } => {
                let mut parts = vec![Self::path(path)];
                if let Some(args) = args {
                    let close = p.span.end - 1;
                    let open_c = self.open_comments(close);
                    let items = args
                        .iter()
                        .map(|a| self.element(a.span.start, a.span.end, |f| f.pat(a)))
                        .collect();
                    parts.push(self.list(PAREN, close, items, open_c));
                }
                concat(parts)
            }
            PatKind::Some(inner) => concat(vec![text("some("), self.pat(inner), text(")")]),
            PatKind::None => text("none"),
            PatKind::Ok(inner) => concat(vec![text("ok("), self.pat(inner), text(")")]),
            PatKind::Err(inner) => concat(vec![text("err("), self.pat(inner), text(")")]),
            PatKind::Tuple(items) => {
                if items.is_empty() {
                    return text("()");
                }
                let close = p.span.end - 1;
                let open_c = self.open_comments(close);
                let docs = items
                    .iter()
                    .map(|a| self.element(a.span.start, a.span.end, |f| f.pat(a)))
                    .collect();
                let spec = ListSpec {
                    force_comma: items.len() == 1,
                    ..PAREN
                };
                self.list(spec, close, docs, open_c)
            }
            PatKind::Struct { path, fields, rest } => {
                let close = p.span.end - 1;
                let open_c = self.open_comments(close);
                let mut items: Vec<Doc> = fields
                    .iter()
                    .map(|fp| {
                        let end = fp.pat.span.end.max(fp.name.span.end);
                        self.element(fp.name.span.start, end, |f| {
                            if fp.shorthand {
                                text(fp.name.name.clone())
                            } else {
                                concat(vec![text(format!("{}: ", fp.name.name)), f.pat(&fp.pat)])
                            }
                        })
                    })
                    .collect();
                if *rest {
                    items.push(text(".."));
                }
                let list = self.list(PADDED_BRACE, close, items, open_c);
                concat(vec![Self::path(path), text(" "), list])
            }
            PatKind::Or(alts) => {
                let docs = alts
                    .iter()
                    .map(|a| {
                        let doc = self.pat(a);
                        if matches!(a.kind, PatKind::Bind { .. } | PatKind::Or(_)) {
                            concat(vec![text("("), doc, text(")")])
                        } else {
                            doc
                        }
                    })
                    .collect();
                join(&text(" | "), docs)
            }
            PatKind::Bind { pat, name } => {
                concat(vec![self.pat(pat), text(format!(" as {}", name.name))])
            }
            PatKind::Error => text(self.slice(p.span)),
        }
    }

    // ---- expressions ----

    fn arg(&mut self, a: &Arg) -> Doc {
        let start = a.name.as_ref().map_or(a.value.span.start, |n| n.span.start);
        self.element(start, a.value.span.end, |f| {
            let value = f.expr(&a.value);
            match &a.name {
                Some(n) => concat(vec![text(format!("{}: ", n.name)), value]),
                None => value,
            }
        })
    }

    /// `(a, b)` of a call. A last argument that is a closure with a block body stays on the
    /// line of the call (`f(a, fn(x) {`) when the start of the call fits.
    fn call_args(&mut self, args: &[Arg], close: u32) -> Doc {
        let open_c = self.open_comments(close);
        let items: Vec<Doc> = args.iter().map(|a| shared(self.arg(a))).collect();
        let hug_candidate = args.last().is_some_and(|a| huggable(&a.value))
            && items.last().is_some_and(Doc::forced)
            && items[..items.len() - 1].iter().all(|d| !d.forced())
            && is_nil(&open_c);
        let hug = hug_candidate
            .then(|| concat(vec![text("("), join(&text(", "), items.clone()), text(")")]));
        let parts = self.list_parts(PAREN, close, open_c, items);
        match hug {
            Some(hug) if !parts.magic && !parts.dangling => {
                Doc::Either(Box::new(hug), Box::new(parts.doc))
            }
            _ => parts.doc,
        }
    }

    fn index_args(&mut self, args: &[IndexArg], close: u32) -> Doc {
        let open_c = self.open_comments(close);
        let items = args
            .iter()
            .map(|a| match a {
                IndexArg::Expr(e) => self.element(e.span.start, e.span.end, |f| f.expr(e)),
                IndexArg::Type(t) => self.element(t.span.start, t.span.end, |f| f.ty(t)),
            })
            .collect();
        self.list(BRACKET, close, items, open_c)
    }

    fn postfix(&mut self, op: &Expr) -> Doc {
        let close = op.span.end.saturating_sub(1);
        match &op.kind {
            ExprKind::Call { args, .. } => self.call_args(args, close),
            ExprKind::Field { name, .. } => text(format!(".{}", name.name)),
            ExprKind::TupleIndex { index, .. } => text(format!(".{index}")),
            ExprKind::Index { args, .. } => self.index_args(args, close),
            ExprKind::Try(_) => text("?"),
            _ => unreachable!("postfix is only called on postfix operations"),
        }
    }

    /// A chain of calls, fields, indexes and `?`. With two or more method calls it may break
    /// before each `.method(..)`, indented, when it does not fit on one line.
    fn chain(&mut self, e: &Expr) -> Doc {
        let mut ops: Vec<&Expr> = Vec::new();
        let mut cur = e;
        while let ExprKind::Call { callee: base, .. }
        | ExprKind::Field { base, .. }
        | ExprKind::TupleIndex { base, .. }
        | ExprKind::Index { base, .. }
        | ExprKind::Try(base) = &cur.kind
        {
            ops.push(cur);
            cur = base;
        }
        ops.reverse();
        let base = self.expr(cur);
        // A `.name(` can start a line, unless `.Name` would be read as a match arm.
        let breakable = |i: usize| {
            matches!(&ops[i].kind, ExprKind::Field { name, .. } if !name.name.starts_with(char::is_uppercase))
                && matches!(ops.get(i + 1).map(|o| &o.kind), Some(ExprKind::Call { .. }))
        };
        let starts: Vec<usize> = (0..ops.len()).filter(|&i| breakable(i)).collect();
        if starts.len() < 2 {
            let mut parts = vec![base];
            for op in &ops {
                parts.push(self.postfix(op));
            }
            return concat(parts);
        }
        let mut head = vec![base];
        for op in &ops[..starts[0]] {
            head.push(self.postfix(op));
        }
        let mut links: Vec<Doc> = Vec::new();
        let mut leading: Vec<Doc> = Vec::new();
        let mut any_comment = false;
        for (k, &start) in starts.iter().enumerate() {
            let end = starts.get(k + 1).copied().unwrap_or(ops.len());
            let before = self.next;
            let lead = match &ops[start].kind {
                ExprKind::Field { name, .. } => self.take_leading(name.span.start),
                _ => Doc::Nil,
            };
            any_comment |= self.next != before;
            leading.push(lead);
            let mut docs: Vec<Doc> = ops[start..end].iter().map(|op| self.postfix(op)).collect();
            // A trailing comment after the last link belongs to the whole expression.
            if end < ops.len() {
                let link_end = ops[end - 1].span.end;
                docs.push(self.flush_inside(link_end));
                docs.push(self.take_trailing(link_end));
            }
            links.push(shared(concat(docs)));
        }
        let head = shared(concat(head));
        let flat_ok =
            !any_comment && !head.forced() && links[..links.len() - 1].iter().all(|l| !l.forced());
        let flat = concat(
            std::iter::once(head.clone())
                .chain(links.iter().cloned())
                .collect(),
        );
        let broken = group(concat(vec![
            head,
            indent(concat(
                links
                    .into_iter()
                    .zip(leading)
                    .map(|(link, lead)| concat(vec![Doc::SoftLine, lead, link]))
                    .collect(),
            )),
        ]));
        if flat_ok {
            Doc::Either(Box::new(flat), Box::new(broken))
        } else {
            broken
        }
    }

    /// Splits a left-nested chain of operators of one precedence into its operands.
    fn operands<'e>(&self, e: &'e Expr) -> (&'e Expr, Vec<Operand<'e>>) {
        let power = match &e.kind {
            ExprKind::Binary { op, .. } => binop_power(*op),
            _ => OR_POWER,
        };
        let mut rest: Vec<Operand<'e>> = Vec::new();
        let mut cur = e;
        loop {
            let (op, lhs, rhs) = match &cur.kind {
                ExprKind::Binary { op, lhs, rhs } if binop_power(*op) == power => {
                    (binop_text(*op), lhs, rhs)
                }
                ExprKind::Or { lhs, rhs } if power == OR_POWER => ("or", lhs, rhs),
                _ => break,
            };
            let op_pos = self.toks[self.tok_from(lhs.span.end)..]
                .first()
                .map_or(rhs.span.start, |t| t.start);
            rest.push(Operand {
                op,
                expr: rhs,
                op_pos,
            });
            cur = lhs;
        }
        rest.reverse();
        (cur, rest)
    }

    fn binary_chain(&mut self, e: &Expr) -> Doc {
        let (first, rest) = self.operands(e);
        let first_doc = shared(self.element(first.span.start, first.span.end, |f| f.expr(first)));
        let count = rest.len();
        let mut broken = Vec::new();
        let mut flat = vec![first_doc.clone()];
        let mut inline_ok = !first_doc.forced();
        for (i, operand) in rest.iter().enumerate() {
            let before = breaks_before(operand.op);
            let (suffixes, lead, _) = self.take_leading_parts(if before {
                operand.op_pos
            } else {
                operand.expr.span.start
            });
            // A trailing comment after the last operand belongs to the whole expression.
            let rhs = shared(if i + 1 == count {
                self.expr(operand.expr)
            } else {
                self.operand(operand.expr)
            });
            inline_ok &= is_nil(&lead) && is_nil(&suffixes) && (i + 1 == count || !rhs.forced());
            flat.push(text(format!(" {} ", operand.op)));
            flat.push(rhs.clone());
            broken.push(if before {
                concat(vec![
                    suffixes,
                    Doc::Line,
                    lead,
                    text(format!("{} ", operand.op)),
                    rhs,
                ])
            } else {
                concat(vec![
                    text(format!(" {}", operand.op)),
                    suffixes,
                    Doc::Line,
                    lead,
                    rhs,
                ])
            });
        }
        let broken = group(concat(vec![first_doc, indent(concat(broken))]));
        // `a + match x { .. }`: when only the last operand spans lines, it stays on the line of
        // the operator.
        if inline_ok && broken.forced() {
            Doc::Either(Box::new(concat(flat)), Box::new(broken))
        } else {
            broken
        }
    }

    fn operand(&mut self, e: &Expr) -> Doc {
        self.element(e.span.start, e.span.end, |f| f.expr(e))
    }

    fn opt_expr(&mut self, e: &Option<Box<Expr>>) -> Doc {
        match e {
            Some(e) => self.expr(e),
            None => Doc::Nil,
        }
    }

    fn expr(&mut self, e: &Expr) -> Doc {
        match &e.kind {
            ExprKind::Int(_) | ExprKind::Float(_) | ExprKind::Char(_) | ExprKind::Str(_) => {
                text(self.slice(e.span))
            }
            ExprKind::Bool(b) => text(if *b { "true" } else { "false" }),
            ExprKind::None => text("none"),
            ExprKind::Ident(id) => text(id.name.clone()),
            ExprKind::SelfValue => text("self"),
            ExprKind::List(items) => {
                let close = e.span.end - 1;
                let open_c = self.open_comments(close);
                let docs = items
                    .iter()
                    .map(|a| self.element(a.span.start, a.span.end, |f| f.expr(a)))
                    .collect();
                self.list(BRACKET, close, docs, open_c)
            }
            ExprKind::Map(entries) => {
                if entries.is_empty() {
                    return text("{:}");
                }
                let close = e.span.end - 1;
                let open_c = self.open_comments(close);
                let docs = entries
                    .iter()
                    .map(|(k, v)| {
                        self.element(k.span.start, v.span.end, |f| {
                            concat(vec![f.expr(k), text(": "), f.expr(v)])
                        })
                    })
                    .collect();
                self.list(BRACE, close, docs, open_c)
            }
            ExprKind::Tuple(items) => {
                if items.is_empty() {
                    return text("()");
                }
                let close = e.span.end - 1;
                let open_c = self.open_comments(close);
                let docs = items
                    .iter()
                    .map(|a| self.element(a.span.start, a.span.end, |f| f.expr(a)))
                    .collect();
                let spec = ListSpec {
                    force_comma: items.len() == 1,
                    ..PAREN
                };
                self.list(spec, close, docs, open_c)
            }
            ExprKind::Paren(inner) => {
                let lead = self.take_leading(inner.span.start);
                let doc = self.expr(inner);
                let tail = concat(vec![
                    self.flush_inside(inner.span.end),
                    self.take_trailing(inner.span.end),
                ]);
                group(concat(vec![
                    text("("),
                    indent(concat(vec![Doc::SoftLine, lead, doc, tail])),
                    Doc::SoftLine,
                    text(")"),
                ]))
            }
            ExprKind::DotVariant { name, args } => {
                let mut parts = vec![text(format!(".{}", name.name))];
                if let Some(args) = args {
                    parts.push(self.call_args(args, e.span.end - 1));
                }
                concat(parts)
            }
            ExprKind::StructLit { ty, fields } => {
                let close = e.span.end - 1;
                let ty = self.ty(ty);
                let open_c = self.open_comments(close);
                let docs = fields
                    .iter()
                    .map(|fi| {
                        let end = fi.value.span.end.max(fi.name.span.end);
                        self.element(fi.name.span.start, end, |f| {
                            if fi.shorthand {
                                text(fi.name.name.clone())
                            } else {
                                concat(vec![text(format!("{}: ", fi.name.name)), f.expr(&fi.value)])
                            }
                        })
                    })
                    .collect();
                let list = self.list(PADDED_BRACE, close, docs, open_c);
                concat(vec![ty, text(" "), list])
            }
            ExprKind::Unary { op, expr } => {
                let sign = match op {
                    UnOp::Neg => "-",
                    UnOp::Not => "!",
                    UnOp::BitNot => "~",
                };
                concat(vec![text(sign), self.expr(expr)])
            }
            ExprKind::Binary { .. } | ExprKind::Or { .. } => self.binary_chain(e),
            ExprKind::Range {
                start,
                end,
                inclusive,
            } => concat(vec![
                self.opt_expr(start),
                text(if *inclusive { "..=" } else { ".." }),
                self.opt_expr(end),
            ]),
            ExprKind::Cast { expr, ty } => concat(vec![self.expr(expr), text(" as "), self.ty(ty)]),
            ExprKind::OrHandler {
                lhs,
                binding,
                handler,
            } => {
                let mut parts = vec![self.expr(lhs), text(" or ")];
                if let Some(b) = binding {
                    parts.push(text(format!("|{}| ", b.name)));
                }
                parts.push(self.block(handler));
                concat(parts)
            }
            ExprKind::Call { .. }
            | ExprKind::Field { .. }
            | ExprKind::TupleIndex { .. }
            | ExprKind::Index { .. }
            | ExprKind::Try(_) => self.chain(e),
            ExprKind::If { .. } | ExprKind::IfLet { .. } => self.if_expr(e),
            ExprKind::Match { scrutinee, arms } => self.match_expr(e, scrutinee, arms),
            ExprKind::Closure(c) => self.closure(e, c),
            ExprKind::Unsafe(b) => concat(vec![text("unsafe "), self.block(b)]),
            ExprKind::Block(b) => self.block(b),
            ExprKind::Error => text(self.slice(e.span)),
        }
    }

    fn if_expr(&mut self, e: &Expr) -> Doc {
        let (head, then, else_) = match &e.kind {
            ExprKind::If { cond, then, else_ } => {
                (concat(vec![text("if "), self.expr(cond)]), then, else_)
            }
            ExprKind::IfLet {
                pat,
                scrutinee,
                then,
                else_,
            } => (
                concat(vec![
                    text("if let "),
                    self.pat(pat),
                    text(" = "),
                    self.expr(scrutinee),
                ]),
                then,
                else_,
            ),
            _ => unreachable!("if_expr is called on if expressions"),
        };
        let mut parts = vec![head, text(" "), self.block(then)];
        if let Some(else_) = else_ {
            let else_pos = self
                .toks
                .get(self.tok_from(then.span.end))
                .map_or(else_.span.start, |t| t.start);
            let (comments, own_line) = self.take_leading_ex(else_pos);
            if own_line {
                parts.push(Doc::HardLine);
            } else {
                parts.push(text(" "));
            }
            parts.push(comments);
            parts.push(text("else "));
            parts.push(self.expr(else_));
        }
        concat(parts)
    }

    fn match_expr(&mut self, e: &Expr, scrutinee: &Expr, arms: &[MatchArm]) -> Doc {
        let head = concat(vec![text("match "), self.expr(scrutinee), text(" ")]);
        let close = e.span.end.saturating_sub(1);
        let open = self.find_open(close);
        let mut seq = self.new_seq(open, close);
        // Arms with a short pattern and a one-line body in a row have their `=>` aligned.
        let mut run: Option<Rc<Cell<usize>>> = None;
        let mut prev_end = open;
        for (i, arm) in arms.iter().enumerate() {
            let last_arm = i + 1 == arms.len();
            let joined = run.is_some()
                && !self.gap_blank(prev_end, arm.span.start)
                && !self.has_comment_between(prev_end, arm.span.start);
            if !joined {
                run = None;
            }
            let mut current = run.take();
            self.seq_push(&mut seq, arm.span.start, arm.span.end, |f| {
                let (doc, aligned) = f.arm(arm, &mut current);
                if !aligned {
                    current = None;
                }
                let open = match &arm.body {
                    ArmBody::Expr(e) => open_end(e),
                    ArmBody::Stmt(s) => stmt_open_end(s),
                };
                if open && !last_arm {
                    concat(vec![doc, text(",")])
                } else {
                    doc
                }
            });
            run = current;
            prev_end = arm.span.end;
        }
        seq.one_line = false;
        concat(vec![head, self.seq_braces(seq, close, false)])
    }

    /// An arm; `run` is the alignment of the arms before it, taken over when the arm joins it.
    fn arm(&mut self, arm: &MatchArm, run: &mut Option<Rc<Cell<usize>>>) -> (Doc, bool) {
        let mut lhs = vec![self.pat(&arm.pat)];
        if let Some(guard) = &arm.guard {
            lhs.push(text(" if "));
            lhs.push(self.expr(guard));
        }
        let lhs = concat(lhs);
        let body = match &arm.body {
            ArmBody::Expr(e) => self.expr(e),
            ArmBody::Stmt(s) => self.stmt(s),
        };
        let flat = render(&lhs, usize::MAX / 2);
        if body.forced() || lhs.forced() || flat.contains('\n') {
            return (concat(vec![lhs, text(" => "), body]), false);
        }
        let width = flat.chars().count();
        let widest = run.get_or_insert_with(|| Rc::new(Cell::new(0))).clone();
        widest.set(widest.get().max(width));
        let doc = concat(vec![lhs, Doc::Pad(widest, width), text(" => "), body]);
        (doc, true)
    }

    fn closure(&mut self, e: &Expr, c: &Closure) -> Doc {
        let mut parts = vec![text("fn")];
        match self.bracket_after(e.span.start + 2, &TokenKind::LParen) {
            Some((_, close)) => parts.push(self.params(None, &c.params, close)),
            None => parts.push(text("()")),
        }
        if let Some(ret) = &c.ret {
            parts.push(text(" -> "));
            parts.push(self.ty(ret));
        }
        if !c.captures.is_empty() {
            let names: Vec<String> = c
                .captures
                .iter()
                .map(|cap| {
                    if cap.weak {
                        format!("weak {}", cap.name.name)
                    } else {
                        cap.name.name.clone()
                    }
                })
                .collect();
            parts.push(text(format!(" use({})", names.join(", "))));
        }
        match &*c.body {
            FnBody::Block(b) => {
                parts.push(text(" "));
                parts.push(self.block(b));
            }
            FnBody::Expr(body) => {
                parts.push(text(" => "));
                parts.push(self.expr(body));
            }
        }
        concat(parts)
    }
}
