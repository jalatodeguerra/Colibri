//! The parser: hand-written recursive descent, with Pratt parsing for
//! expressions (DESIGN §17.3).
//!
//! Errors are reported into a diagnostics list and parsing continues: a
//! failing construct returns `Err(Failed)` up to the nearest recovery point
//! (a statement, a list element or a top-level item), which skips ahead to a
//! safe token and leaves an `Error` placeholder node.

mod expr;
mod item;
mod pat;
mod stmt;
mod ty;

use crate::ast::{File, Ident, NodeId};
use crate::{Diagnostic, FileId, Span, Token, TokenKind};

/// Parses a whole source file. Lexer diagnostics are included, ordered by
/// position together with the parser's. Never panics; always returns a tree.
pub fn parse_file(file: FileId, src: &str) -> (File, Vec<Diagnostic>) {
    let (tokens, mut diags) = crate::tokenize(file, src);
    let mut parser = Parser::new(file, src, tokens);
    let ast = parser.parse_file();
    diags.append(&mut parser.diags);
    diags.sort_by_key(|d| d.span.start);
    (ast, diags)
}

/// Parses an interface file (`.cbi`, DESIGN §15.4 and EMBEDDING §4): ordinary Colibri
/// declarations in which functions and methods of `impl` blocks have no body.
pub fn parse_interface_file(file: FileId, src: &str) -> (File, Vec<Diagnostic>) {
    let (tokens, mut diags) = crate::tokenize(file, src);
    let mut parser = Parser::new(file, src, tokens);
    parser.interface_mode = true;
    let ast = parser.parse_file();
    diags.append(&mut parser.diags);
    diags.sort_by_key(|d| d.span.start);
    (ast, diags)
}

/// Marker for "an error was already reported".
#[derive(Debug)]
pub(crate) struct Failed;

pub(crate) type PResult<T> = Result<T, Failed>;

/// Saved parser state for speculative parsing.
pub(crate) struct Snapshot {
    pos: usize,
    diags: usize,
    last_error: Option<u32>,
}

pub(crate) struct Parser<'a> {
    file: FileId,
    src: &'a str,
    /// Always ends with `Eof`; doc comments are moved to `docs`.
    toks: Vec<Token>,
    /// Doc comment lines with the index of the token that follows them.
    docs: Vec<(usize, String)>,
    pos: usize,
    diags: Vec<Diagnostic>,
    next_id: u32,
    /// Set while parsing the head of `if`/`while`/`for`/`match`, where a
    /// struct literal is not allowed (DESIGN §3.5).
    no_struct: bool,
    /// Start of the last reported error, to keep one mistake to one message.
    last_error: Option<u32>,
    /// Current nesting of expressions, types, patterns and blocks.
    depth: u32,
    /// Parsing an interface file: functions and `impl` methods may omit their body.
    interface_mode: bool,
}

/// How deeply constructs may nest before the parser gives up, so that hostile
/// input cannot overflow the stack.
const MAX_DEPTH: u32 = 96;

/// How many operators or postfix operations one expression may chain, which
/// bounds the depth of the tree built for it.
pub(crate) const MAX_CHAIN: u32 = 1000;

impl<'a> Parser<'a> {
    pub(crate) fn new(file: FileId, src: &'a str, tokens: Vec<Token>) -> Self {
        let mut toks = Vec::with_capacity(tokens.len());
        let mut docs = Vec::new();
        for token in tokens {
            match token.kind {
                TokenKind::DocComment(text) => docs.push((toks.len(), text)),
                _ => toks.push(token),
            }
        }
        if toks.last().is_none_or(|t| t.kind != TokenKind::Eof) {
            let end = src.len() as u32;
            toks.push(Token {
                kind: TokenKind::Eof,
                span: Span::new(file, end, end),
            });
        }
        Parser {
            file,
            src,
            toks,
            docs,
            pos: 0,
            diags: Vec::new(),
            next_id: 0,
            no_struct: false,
            last_error: None,
            depth: 0,
            interface_mode: false,
        }
    }

    // ---- node bookkeeping ----

    pub(crate) fn id(&mut self) -> NodeId {
        let id = NodeId(self.next_id);
        self.next_id += 1;
        id
    }

    /// The span from `start` to the end of the last consumed token.
    pub(crate) fn span_from(&self, start: u32) -> Span {
        let end = self
            .pos
            .checked_sub(1)
            .map_or(start, |i| self.toks[i].span.end)
            .max(start);
        Span::new(self.file, start, end)
    }

    pub(crate) fn start(&self) -> u32 {
        self.toks[self.pos].span.start
    }

    /// The doc comment lines directly before the token at `index`.
    pub(crate) fn docs_before(&self, index: usize) -> Option<String> {
        let first = self.docs.partition_point(|(i, _)| *i < index);
        let lines: Vec<&str> = self.docs[first..]
            .iter()
            .take_while(|(i, _)| *i == index)
            .map(|(_, text)| text.as_str())
            .collect();
        (!lines.is_empty()).then(|| lines.join("\n"))
    }

    // ---- cursor ----

    pub(crate) fn tok(&self) -> &Token {
        &self.toks[self.pos]
    }

    pub(crate) fn kind(&self) -> &TokenKind {
        &self.toks[self.pos].kind
    }

    /// The kind `n` tokens ahead (the final `Eof` repeats).
    pub(crate) fn nth(&self, n: usize) -> &TokenKind {
        let i = (self.pos + n).min(self.toks.len() - 1);
        &self.toks[i].kind
    }

    pub(crate) fn at(&self, kind: &TokenKind) -> bool {
        self.kind() == kind
    }

    pub(crate) fn at_eof(&self) -> bool {
        self.at(&TokenKind::Eof)
    }

    /// Consumes and returns the current token (`Eof` is never consumed).
    pub(crate) fn bump(&mut self) -> Token {
        let token = self.toks[self.pos].clone();
        if self.pos + 1 < self.toks.len() {
            self.pos += 1;
        }
        token
    }

    pub(crate) fn eat(&mut self, kind: &TokenKind) -> bool {
        let found = self.at(kind);
        if found {
            self.bump();
        }
        found
    }

    pub(crate) fn skip_semis(&mut self) {
        while self.at(&TokenKind::Semi) {
            self.bump();
        }
    }

    /// Whether the previous token is `kind`.
    fn prev_is(&self, kind: &TokenKind) -> bool {
        self.pos > 0 && self.toks[self.pos - 1].kind == *kind
    }

    pub(crate) fn snapshot(&self) -> Snapshot {
        Snapshot {
            pos: self.pos,
            diags: self.diags.len(),
            last_error: self.last_error,
        }
    }

    /// Rewinds to `snapshot`, dropping the diagnostics reported since.
    pub(crate) fn restore(&mut self, snapshot: Snapshot) {
        self.pos = snapshot.pos;
        self.diags.truncate(snapshot.diags);
        self.last_error = snapshot.last_error;
    }

    /// Runs `f` one nesting level deeper, failing past [`MAX_DEPTH`].
    pub(crate) fn nested<T>(&mut self, f: impl FnOnce(&mut Self) -> PResult<T>) -> PResult<T> {
        if self.depth >= MAX_DEPTH {
            let diag = Diagnostic::error("this is nested too deeply", self.found_span())
                .with_hint("split the code into smaller pieces");
            return self.fail(diag);
        }
        self.depth += 1;
        let result = f(self);
        self.depth -= 1;
        result
    }

    /// Runs `f` with struct literals allowed (`allow`) or forbidden.
    pub(crate) fn with_struct_mode<T>(
        &mut self,
        no_struct: bool,
        f: impl FnOnce(&mut Self) -> T,
    ) -> T {
        let saved = std::mem::replace(&mut self.no_struct, no_struct);
        let result = f(self);
        self.no_struct = saved;
        result
    }

    // ---- errors ----

    /// The span to point at for the current token. Implicit terminators and
    /// the end of file have no text, so they point at the line break.
    pub(crate) fn found_span(&self) -> Span {
        let span = self.tok().span;
        if span.is_empty() {
            let end = self.src.len() as u32;
            let start = span.start.min(end);
            Span::new(self.file, start, (start + 1).min(end))
        } else {
            span
        }
    }

    /// Records an error unless one was already reported at the same place.
    pub(crate) fn report(&mut self, diag: Diagnostic) {
        if diag.is_error() {
            if self.last_error == Some(diag.span.start) {
                return;
            }
            self.last_error = Some(diag.span.start);
        }
        self.diags.push(diag);
    }

    pub(crate) fn fail<T>(&mut self, diag: Diagnostic) -> PResult<T> {
        self.report(diag);
        Err(Failed)
    }

    /// The text of the identifier after the current token, or `x`: for hints that show the
    /// replacement of a construct written with the old `var` keyword.
    pub(crate) fn name_after_keyword(&self) -> String {
        match self.nth(1) {
            TokenKind::Ident(name) => name.clone(),
            _ => "x".to_string(),
        }
    }

    /// Like [`Parser::name_after_keyword`], when the keyword was already consumed.
    pub(crate) fn name_here(&self) -> String {
        match self.kind() {
            TokenKind::Ident(name) => name.clone(),
            _ => "x".to_string(),
        }
    }

    /// Reports the old `var` keyword at the current token; the caller reads it as the new form.
    pub(crate) fn report_old_var(&mut self, hint: &str) {
        let span = self.found_span();
        self.report_old_var_at(span, hint);
    }

    pub(crate) fn report_old_var_at(&mut self, span: Span, hint: &str) {
        let diag = Diagnostic::error(
            "`var` is not a Colibri keyword; mutable bindings are written `let mut`",
            span,
        )
        .with_hint(hint);
        self.report(diag);
    }

    /// "expected `what`, found `token`" at the current token.
    pub(crate) fn unexpected<T>(&mut self, what: &str) -> PResult<T> {
        let message = format!("expected {what}, found {}", self.describe());
        let diag = Diagnostic::error(message, self.found_span());
        self.fail(diag)
    }

    /// A description of the current token for messages.
    pub(crate) fn describe(&self) -> String {
        let token = self.tok();
        match &token.kind {
            TokenKind::Semi if token.span.is_empty() => "end of line".to_string(),
            TokenKind::Semi => "`;`".to_string(),
            TokenKind::Eof => "end of file".to_string(),
            TokenKind::Ident(name) => format!("identifier `{name}`"),
            TokenKind::Int(_) | TokenKind::Float(_) => {
                format!("number `{}`", self.slice(token.span))
            }
            TokenKind::Char(_) => "character literal".to_string(),
            TokenKind::Str(_) => "string literal".to_string(),
            TokenKind::Label(name) => format!("label `'{name}`"),
            TokenKind::Error => "an invalid token".to_string(),
            kind if is_keyword(kind) => {
                format!("keyword `{}`", kind.fixed_text().unwrap_or_default())
            }
            kind => format!("`{}`", kind.fixed_text().unwrap_or("?")),
        }
    }

    pub(crate) fn slice(&self, span: Span) -> &'a str {
        self.src
            .get(span.start as usize..span.end as usize)
            .unwrap_or("")
    }

    pub(crate) fn expect(&mut self, kind: &TokenKind) -> PResult<Token> {
        if self.at(kind) {
            Ok(self.bump())
        } else {
            let what = format!("`{}`", kind.fixed_text().unwrap_or("?"));
            self.unexpected(&what)
        }
    }

    pub(crate) fn expect_ident(&mut self, what: &str) -> PResult<Ident> {
        match self.kind().clone() {
            TokenKind::Ident(name) => {
                let span = self.bump().span;
                Ok(Ident { name, span })
            }
            ref kind if is_keyword(kind) => {
                let word = kind.fixed_text().unwrap_or_default();
                let message = format!("expected {what}, found keyword `{word}`");
                let diag = Diagnostic::error(message, self.found_span()).with_hint(format!(
                    "`{word}` is a reserved word and cannot be used as a name"
                ));
                self.fail(diag)
            }
            _ => self.unexpected(what),
        }
    }

    // ---- recovery ----

    /// Skips to the end of the current statement: past the next `Semi`, or up
    /// to (not including) the `}` that closes the enclosing block. Brackets
    /// opened while skipping are matched. At the top level, stops before a
    /// token that starts an item.
    pub(crate) fn sync_stmt(&mut self, top_level: bool) {
        let mut open = Brackets::default();
        let mut first = true;
        loop {
            let kind = self.kind().clone();
            match kind {
                TokenKind::Eof => return,
                TokenKind::Semi if open.is_empty() => {
                    self.bump();
                    return;
                }
                TokenKind::RBrace if !open.has_brace() => {
                    if !top_level {
                        return;
                    }
                    open.clear();
                }
                _ if top_level && open.is_empty() && !first && self.starts_item() => return,
                _ => open.track(&kind),
            }
            first = false;
            self.bump();
        }
    }

    /// Skips to the next separator (`,` or `Semi`, consumed) or to `close`
    /// (not consumed), matching brackets, so a bad list element costs one
    /// diagnostic.
    pub(crate) fn sync_list(&mut self, close: &TokenKind) {
        let mut open = Brackets::default();
        loop {
            let kind = self.kind().clone();
            match kind {
                TokenKind::Eof => return,
                TokenKind::Comma | TokenKind::Semi if open.is_empty() => {
                    self.bump();
                    return;
                }
                _ if open.is_empty() && kind == *close => return,
                // A `}` that closes nothing we opened ends the enclosing block.
                TokenKind::RBrace if !open.has_brace() => return,
                _ => open.track(&kind),
            }
            self.bump();
        }
    }

    /// Parses `{ elem (sep elem)* }` where separators are `,` or line breaks,
    /// trailing separators are allowed, and a bad element is skipped.
    /// `what` names the elements for the missing-separator message.
    pub(crate) fn parse_braced<T>(
        &mut self,
        what: &str,
        mut elem: impl FnMut(&mut Self) -> PResult<T>,
    ) -> PResult<Vec<T>> {
        let open = self.expect(&TokenKind::LBrace)?;
        let saved = std::mem::replace(&mut self.no_struct, false);
        let mut out = Vec::new();
        loop {
            while matches!(self.kind(), TokenKind::Semi | TokenKind::Comma) {
                self.bump();
            }
            if self.at(&TokenKind::RBrace) {
                break;
            }
            if self.at_eof() {
                self.no_struct = saved;
                return self.fail(
                    Diagnostic::error("unclosed `{`", open.span)
                        .with_hint("add a matching `}` at the end"),
                );
            }
            let before = self.pos;
            match elem(self) {
                Ok(value) => out.push(value),
                Err(Failed) => {
                    self.sync_list(&TokenKind::RBrace);
                    if self.pos == before {
                        self.bump();
                    }
                    continue;
                }
            }
            let separated = matches!(
                self.kind(),
                TokenKind::Semi | TokenKind::Comma | TokenKind::RBrace
            );
            if !separated && !self.prev_is(&TokenKind::RBrace) {
                let message = format!(
                    "expected `,` or a new line between {what}, found {}",
                    self.describe()
                );
                self.report(Diagnostic::error(message, self.found_span()));
                self.sync_list(&TokenKind::RBrace);
            }
        }
        self.no_struct = saved;
        self.expect(&TokenKind::RBrace)?;
        Ok(out)
    }

    /// Parses `( elem, elem, )` with the opening `(` already consumed, up to
    /// and including `close`.
    pub(crate) fn parse_comma_list<T>(
        &mut self,
        close: &TokenKind,
        mut elem: impl FnMut(&mut Self) -> PResult<T>,
    ) -> PResult<Vec<T>> {
        let mut out = Vec::new();
        while !self.at(close) {
            out.push(elem(self)?);
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        if !self.at(close) {
            let what = format!("`,` or `{}`", close.fixed_text().unwrap_or("?"));
            return self.unexpected(&what);
        }
        self.bump();
        Ok(out)
    }
}

/// Brackets opened while skipping tokens during error recovery. A `}` closes
/// the innermost `{` even past unclosed `(` and `[`, and a closer that matches
/// nothing open is ignored, so one unbalanced bracket doesn't swallow the file.
#[derive(Default)]
struct Brackets(Vec<TokenKind>);

impl Brackets {
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn has_brace(&self) -> bool {
        self.0.contains(&TokenKind::LBrace)
    }

    fn clear(&mut self) {
        self.0.clear();
    }

    fn track(&mut self, kind: &TokenKind) {
        match kind {
            TokenKind::LParen | TokenKind::LBracket | TokenKind::LBrace => {
                self.0.push(kind.clone())
            }
            TokenKind::RParen if self.0.last() == Some(&TokenKind::LParen) => {
                self.0.pop();
            }
            TokenKind::RBracket if self.0.last() == Some(&TokenKind::LBracket) => {
                self.0.pop();
            }
            TokenKind::RBrace => while self.0.pop().is_some_and(|k| k != TokenKind::LBrace) {},
            _ => {}
        }
    }
}

/// Whether `kind` is a reserved word.
pub(crate) fn is_keyword(kind: &TokenKind) -> bool {
    kind.fixed_text()
        .is_some_and(|text| TokenKind::keyword(text).is_some())
}
