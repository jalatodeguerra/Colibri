//! The lexer (DESIGN §3, §12): source text to raw tokens, newlines included.

use crate::token::OPERATORS;
use crate::{Diagnostic, FileId, Span, StrLit, StrPart, Token, TokenKind};

/// Lexes `src` into raw tokens (with `Newline`, without implicit terminators).
///
/// Lexing never stops at an error: bad input is diagnosed and the lexer
/// carries on, emitting `Error` tokens where there is nothing better. The
/// result always ends with `Eof`.
pub fn lex(file: FileId, src: &str) -> (Vec<Token>, Vec<Diagnostic>) {
    let mut lexer = Lexer {
        file,
        src,
        pos: 0,
        tokens: Vec::new(),
        diags: Vec::new(),
        hole_depth: 0,
    };
    lexer.run();
    (lexer.tokens, lexer.diags)
}

fn is_ident_start(c: char) -> bool {
    c == '_' || c.is_alphabetic()
}

fn is_ident_continue(c: char) -> bool {
    c == '_' || c.is_alphanumeric()
}

/// A piece of a string body before escape processing; byte ranges refer to
/// the original source.
enum Seg {
    Text { start: usize, end: usize },
    Hole { expr: Span, spec: Option<String> },
}

/// Where an interpolation hole ends.
struct HoleEnd {
    expr_end: usize,
    spec: Option<(usize, usize)>,
}

struct Lexer<'a> {
    file: FileId,
    src: &'a str,
    /// Byte offset of the next unread character.
    pos: usize,
    tokens: Vec<Token>,
    diags: Vec<Diagnostic>,
    /// How many interpolation holes deep the scan is (strings inside holes
    /// inside strings ...).
    hole_depth: u32,
}

/// Holes nested deeper than this are reported as unterminated, so hostile
/// input cannot overflow the stack.
const MAX_HOLE_DEPTH: u32 = 64;

impl<'a> Lexer<'a> {
    // ---- cursor helpers ----

    fn span(&self, start: usize, end: usize) -> Span {
        Span::new(self.file, start as u32, end as u32)
    }

    fn rest(&self) -> &'a str {
        &self.src[self.pos..]
    }

    fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    fn peek_nth(&self, n: usize) -> Option<char> {
        self.rest().chars().nth(n)
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += c.len_utf8();
        Some(c)
    }

    fn push(&mut self, kind: TokenKind, start: usize) {
        let span = self.span(start, self.pos);
        self.tokens.push(Token { kind, span });
    }

    fn error(&mut self, d: Diagnostic) {
        self.diags.push(d);
    }

    // ---- main loop ----

    fn run(&mut self) {
        while let Some(c) = self.peek() {
            let start = self.pos;
            match c {
                '\n' => {
                    self.pos += 1;
                    self.push(TokenKind::Newline, start);
                }
                '\r' => {
                    self.pos += 1;
                    if self.peek() == Some('\n') {
                        self.pos += 1;
                        self.push(TokenKind::Newline, start);
                    }
                }
                c if c.is_whitespace() => self.pos += c.len_utf8(),
                '/' if self.rest().starts_with("//") => self.line_comment(),
                '/' if self.rest().starts_with("/*") => self.block_comment(),
                '0'..='9' => self.number(),
                '"' => self.string(start, false),
                'r' if self.rest().starts_with("r\"") => {
                    self.pos += 1;
                    self.string(start, true);
                }
                'r' if self.raw_hashes().is_some() => {
                    let hashes = self.raw_hashes().unwrap_or(0);
                    self.pos += 1 + hashes;
                    self.hashed_raw_string(start, hashes);
                }
                '\'' => self.quote(),
                c if is_ident_start(c) => self.ident(),
                _ => self.operator(),
            }
        }
        let end = self.src.len();
        self.pos = end;
        self.push(TokenKind::Eof, end);
    }

    // ---- comments ----

    fn line_comment(&mut self) {
        let start = self.pos;
        let line = &self.rest()[..self.rest().find('\n').unwrap_or(self.rest().len())];
        self.pos += line.len();
        // `///` is a doc comment, `////` is an ordinary comment.
        if let Some(doc) = line.strip_prefix("///")
            && !doc.starts_with('/')
        {
            let doc = doc.strip_suffix('\r').unwrap_or(doc);
            let doc = doc.strip_prefix(' ').unwrap_or(doc);
            self.push(TokenKind::DocComment(doc.to_string()), start);
        }
    }

    fn block_comment(&mut self) {
        let start = self.pos;
        self.pos += 2;
        let mut depth = 1;
        let mut has_newline = false;
        while depth > 0 {
            if self.rest().starts_with("/*") {
                depth += 1;
                self.pos += 2;
            } else if self.rest().starts_with("*/") {
                depth -= 1;
                self.pos += 2;
            } else if let Some(c) = self.bump() {
                has_newline |= c == '\n';
            } else {
                let hint = if depth > 1 {
                    "block comments nest: every `/*` needs its own `*/`"
                } else {
                    "add `*/` to close the comment"
                };
                self.error(
                    Diagnostic::error("unterminated block comment", self.span(start, start + 2))
                        .with_hint(hint),
                );
                break;
            }
        }
        // A block comment spanning lines acts as a line break (as in Go).
        if has_newline {
            self.push(TokenKind::Newline, start);
        }
    }

    // ---- identifiers, operators ----

    fn ident(&mut self) {
        let start = self.pos;
        while matches!(self.peek(), Some(c) if is_ident_continue(c)) {
            self.bump();
        }
        let text = &self.src[start..self.pos];
        let kind = if text == "_" {
            TokenKind::Underscore
        } else {
            TokenKind::keyword(text).unwrap_or_else(|| TokenKind::Ident(text.to_string()))
        };
        self.push(kind, start);
    }

    fn operator(&mut self) {
        let start = self.pos;
        let rest = self.rest();
        if let Some((text, kind)) = OPERATORS.iter().find(|(text, _)| rest.starts_with(text)) {
            self.pos += text.len();
            self.push(kind.clone(), start);
        } else if let Some(c) = self.bump() {
            self.error(Diagnostic::error(
                format!("unexpected character `{}`", c.escape_debug()),
                self.span(start, self.pos),
            ));
            self.push(TokenKind::Error, start);
        }
    }

    // ---- numbers ----

    fn number(&mut self) {
        let start = self.pos;
        let radix = match (self.peek(), self.peek_nth(1)) {
            (Some('0'), Some('x' | 'X')) => 16,
            (Some('0'), Some('b' | 'B')) => 2,
            (Some('0'), Some('o' | 'O')) => 8,
            _ => 10,
        };
        if radix == 10 {
            self.decimal(start);
        } else {
            self.radix_int(start, radix);
        }
        self.number_suffix();
    }

    fn eat_digits(&mut self) {
        while matches!(self.peek(), Some('0'..='9' | '_')) {
            self.pos += 1;
        }
    }

    fn decimal(&mut self, start: usize) {
        // After a `.` we are in `t.0` field access: never a float, so `t.0.1`
        // is two field accesses.
        let after_dot = matches!(self.tokens.last(), Some(t) if t.kind == TokenKind::Dot);
        self.eat_digits();
        let mut is_float = false;
        if !after_dot && self.peek() == Some('.') && matches!(self.peek_nth(1), Some('0'..='9')) {
            self.pos += 1;
            self.eat_digits();
            is_float = true;
        }
        if !after_dot && matches!(self.peek(), Some('e' | 'E')) {
            let sign = usize::from(matches!(self.peek_nth(1), Some('+' | '-')));
            if matches!(self.peek_nth(1 + sign), Some('0'..='9')) {
                self.pos += 1 + sign;
                self.eat_digits();
                is_float = true;
            }
        }
        let span = self.span(start, self.pos);
        let text: String = self.src[start..self.pos]
            .chars()
            .filter(|&c| c != '_')
            .collect();
        if is_float {
            let value: f64 = text.parse().unwrap_or(0.0);
            if !value.is_finite() {
                self.error(Diagnostic::error("float literal is too large", span));
            }
            self.push(TokenKind::Float(value), start);
        } else {
            let value = text.parse::<u64>().unwrap_or_else(|_| {
                self.diags.push(too_large(span));
                0
            });
            self.push(TokenKind::Int(value), start);
        }
    }

    fn radix_int(&mut self, start: usize, radix: u32) {
        let name = match radix {
            16 => "hexadecimal",
            2 => "binary",
            _ => "octal",
        };
        self.pos += 2;
        let mut value: u64 = 0;
        let mut digits = 0;
        let mut bad = false;
        let mut overflow = false;
        while let Some(c) = self.peek() {
            if c == '_' {
                self.pos += 1;
            } else if let Some(d) = c.to_digit(radix) {
                let next = value
                    .checked_mul(u64::from(radix))
                    .and_then(|v| v.checked_add(u64::from(d)));
                overflow |= next.is_none();
                value = next.unwrap_or(value);
                digits += 1;
                self.pos += 1;
            } else if c.is_alphanumeric() {
                let at = self.pos;
                self.pos += c.len_utf8();
                bad = true;
                self.error(Diagnostic::error(
                    format!("invalid digit `{c}` in {name} literal"),
                    self.span(at, self.pos),
                ));
            } else {
                break;
            }
        }
        let span = self.span(start, self.pos);
        if digits == 0 && !bad {
            self.error(Diagnostic::error(
                format!("expected digits after `{}`", &self.src[start..start + 2]),
                span,
            ));
        }
        if overflow {
            self.error(too_large(span));
        }
        self.push(TokenKind::Int(value), start);
    }

    /// Letters glued to a number (`12px`, `0xFFu8`) are an error; consume them
    /// so one mistake gives one diagnostic.
    fn number_suffix(&mut self) {
        let start = self.pos;
        if !matches!(self.peek(), Some(c) if c.is_alphabetic()) {
            return;
        }
        while matches!(self.peek(), Some(c) if is_ident_continue(c)) {
            self.bump();
        }
        let suffix = &self.src[start..self.pos];
        self.error(
            Diagnostic::error(
                format!("invalid suffix `{suffix}` on number literal"),
                self.span(start, self.pos),
            )
            .with_hint("number literals take their type from context; annotate the binding (`let x: u8 = 255`) or convert with a call such as `u8(x)`"),
        );
    }

    // ---- chars and labels ----

    /// Handles a `'`: a char literal (`'a'`, `'\n'`) or a label (`'outer`).
    fn quote(&mut self) {
        let start = self.pos;
        self.pos += 1;
        match self.peek() {
            Some('\\') => self.escaped_char(start),
            Some('\'') => {
                self.pos += 1;
                self.error(
                    Diagnostic::error("empty char literal", self.span(start, self.pos))
                        .with_hint("a char literal holds exactly one character, like 'a'"),
                );
                self.push(TokenKind::Error, start);
            }
            Some(c) if self.peek_nth(1) == Some('\'') => {
                self.pos += c.len_utf8() + 1;
                self.push(TokenKind::Char(c), start);
            }
            Some(c) if is_ident_start(c) => {
                let name_start = self.pos;
                while matches!(self.peek(), Some(c) if is_ident_continue(c)) {
                    self.bump();
                }
                if self.peek() == Some('\'') {
                    self.pos += 1;
                    self.too_many_chars(start);
                } else {
                    let name = self.src[name_start..self.pos].to_string();
                    self.push(TokenKind::Label(name), start);
                }
            }
            _ => self.malformed_char(start),
        }
    }

    fn escaped_char(&mut self, start: usize) {
        let backslash = self.pos;
        match parse_escape(&self.src[backslash + 1..]) {
            Ok((value, len)) => {
                self.pos = backslash + 1 + len;
                if self.peek() == Some('\'') {
                    self.pos += 1;
                    self.push(TokenKind::Char(value), start);
                } else {
                    self.malformed_char(start);
                }
            }
            Err(msg) => {
                let len = self.src[backslash + 1..]
                    .chars()
                    .next()
                    .map_or(0, char::len_utf8);
                self.error(escape_error(msg, self.span(backslash, backslash + 1 + len)));
                // Resynchronize on the closing quote, if the line has one.
                self.pos = backslash + 1 + len;
                let line_end = self.pos + self.rest().find('\n').unwrap_or(self.rest().len());
                if let Some(i) = self.src[self.pos..line_end].find('\'') {
                    self.pos += i + 1;
                }
                self.push(TokenKind::Error, start);
            }
        }
    }

    /// A `'` that begins neither a char literal nor a label.
    fn malformed_char(&mut self, start: usize) {
        let line_end = self.pos + self.rest().find('\n').unwrap_or(self.rest().len());
        match self.src[self.pos..line_end].find('\'') {
            Some(i) => {
                self.pos += i + 1;
                self.too_many_chars(start);
            }
            None => {
                self.error(
                    Diagnostic::error(
                        "unterminated char literal",
                        self.span(start, self.pos.max(start + 1)),
                    )
                    .with_hint("close it with `'`, e.g. 'a'"),
                );
                self.push(TokenKind::Error, start);
            }
        }
    }

    fn too_many_chars(&mut self, start: usize) {
        let inner = &self.src[start + 1..self.pos - 1];
        self.error(
            Diagnostic::error(
                "a char literal holds one character",
                self.span(start, self.pos),
            )
            .with_hint(format!(
                "use double quotes for strings: \"{}\"",
                inner.escape_debug()
            )),
        );
        self.push(TokenKind::Error, start);
    }

    // ---- strings ----

    /// Lexes a string whose prefix (`r`) is already consumed; `pos` is at the
    /// opening quote. `start` is where the token began.
    fn string(&mut self, start: usize, raw: bool) {
        let multi = self.rest().starts_with("\"\"\"");
        self.pos += if multi { 3 } else { 1 };
        let segs = if raw {
            self.scan_raw(start, multi)
        } else {
            self.scan_body(start, multi)
        };
        let parts = self.build_parts(segs, multi, raw);
        self.push(TokenKind::Str(StrLit { parts }), start);
    }

    /// At an `r`: the number of `#` in a `r#"` / `r##"` opener, if this is one.
    fn raw_hashes(&self) -> Option<usize> {
        let rest = self.rest().strip_prefix('r')?;
        let n = rest.len() - rest.trim_start_matches('#').len();
        (n > 0 && rest[n..].starts_with('"')).then_some(n)
    }

    /// Lexes `r#"..."#` (any number of `#`): `pos` is at the opening quote.
    /// The text is verbatim: no escapes, no interpolation, may span lines and
    /// holds `"` freely; it ends at a `"` followed by the same number of `#`.
    fn hashed_raw_string(&mut self, start: usize, hashes: usize) {
        self.pos += 1;
        let text_start = self.pos;
        let closer = format!("\"{}", "#".repeat(hashes));
        let (end, closed) = match self.rest().find(&closer) {
            Some(i) => (self.pos + i, true),
            None => (self.src.len(), false),
        };
        self.pos = end;
        if closed {
            self.pos += closer.len();
        } else {
            self.error(
                Diagnostic::error(
                    "unterminated raw string literal",
                    self.span(start, start + 2 + hashes),
                )
                .with_hint(format!(
                    "the string started here; close it with `\"{}`",
                    "#".repeat(hashes)
                )),
            );
        }
        let segs: Vec<Seg> = text(text_start, end).into_iter().collect();
        let parts = self.build_parts(segs, false, true);
        self.push(TokenKind::Str(StrLit { parts }), start);
    }

    fn unterminated_string(&mut self, start: usize, multi: bool) {
        let opening = if multi { 3 } else { 1 } + usize::from(self.src[start..].starts_with('r'));
        let hint = if multi {
            "the string started here; close it with `\"\"\"`"
        } else {
            "the string started here; a `\"` string must end on the same line (use `\"\"\"` for multi-line text)"
        };
        self.error(
            Diagnostic::error(
                "unterminated string literal",
                self.span(start, start + opening),
            )
            .with_hint(hint),
        );
    }

    fn scan_raw(&mut self, start: usize, multi: bool) -> Vec<Seg> {
        let text_start = self.pos;
        loop {
            let here = self.pos;
            let closes = if multi {
                self.rest().starts_with("\"\"\"")
            } else {
                self.peek() == Some('"')
            };
            if closes {
                self.pos += if multi { 3 } else { 1 };
                return text(text_start, here).into_iter().collect();
            }
            match self.peek() {
                None => {}
                Some('\n') if !multi => {}
                Some(c) => {
                    self.pos += c.len_utf8();
                    continue;
                }
            }
            self.unterminated_string(start, multi);
            return text(text_start, here).into_iter().collect();
        }
    }

    /// Scans a non-raw string body up to and including the closing quote(s),
    /// splitting it into text and interpolation holes.
    fn scan_body(&mut self, start: usize, multi: bool) -> Vec<Seg> {
        let mut segs = Vec::new();
        let mut text_start = self.pos;
        loop {
            let here = self.pos;
            let Some(c) = self.peek() else {
                segs.extend(text(text_start, here));
                self.unterminated_string(start, multi);
                return segs;
            };
            match c {
                '"' if !multi => {
                    segs.extend(text(text_start, here));
                    self.pos += 1;
                    return segs;
                }
                '"' if self.rest().starts_with("\"\"\"") => {
                    segs.extend(text(text_start, here));
                    self.pos += 3;
                    return segs;
                }
                '\n' if !multi => {
                    segs.extend(text(text_start, here));
                    self.unterminated_string(start, multi);
                    return segs;
                }
                '\\' => self.skip_escape(),
                '{' if self.rest().starts_with("{{") => self.pos += 2,
                '{' => {
                    segs.extend(text(text_start, here));
                    self.pos += 1;
                    let expr_start = self.pos;
                    let Some(end) = self.scan_hole(multi) else {
                        self.error(
                            Diagnostic::error(
                                "unterminated interpolation",
                                self.span(here, here + 1),
                            )
                            .with_hint("close the hole with `}`, or write `{{` for a literal `{`"),
                        );
                        return segs;
                    };
                    let expr = self.span(expr_start, end.expr_end);
                    if self.src[expr_start..end.expr_end].trim().is_empty() {
                        self.error(
                            Diagnostic::error("empty interpolation", self.span(here, self.pos))
                                .with_hint("put an expression inside `{}`, or write `{{}}` for literal braces"),
                        );
                    } else {
                        let spec = end
                            .spec
                            .map(|(a, b)| self.src[a..b].to_string())
                            .filter(|s| !s.is_empty());
                        segs.push(Seg::Hole { expr, spec });
                    }
                    text_start = self.pos;
                }
                '}' if self.rest().starts_with("}}") => self.pos += 2,
                '}' => {
                    self.pos += 1;
                    self.error(
                        Diagnostic::error("unmatched `}` in string", self.span(here, self.pos))
                            .with_hint("write `}}` for a literal `}`"),
                    );
                }
                _ => self.pos += c.len_utf8(),
            }
        }
    }

    /// Skips a backslash escape without interpreting it, so that `\"` and
    /// `\{` do not end the string or open a hole.
    fn skip_escape(&mut self) {
        self.pos += 1;
        match self.peek() {
            Some('u') => {
                self.pos += 1;
                if self.peek() == Some('{') {
                    self.pos += 1;
                    while matches!(self.peek(), Some(c) if c.is_ascii_hexdigit()) {
                        self.pos += 1;
                    }
                    if self.peek() == Some('}') {
                        self.pos += 1;
                    }
                }
            }
            Some('\n') | None => {}
            Some(c) => self.pos += c.len_utf8(),
        }
    }

    /// Scans an interpolation hole; `pos` is just after the `{`. Returns where
    /// the expression and the format spec end, with `pos` after the closing
    /// `}`, or `None` if the hole never closes.
    fn scan_hole(&mut self, multi: bool) -> Option<HoleEnd> {
        if self.hole_depth >= MAX_HOLE_DEPTH {
            return None;
        }
        self.hole_depth += 1;
        let end = self.scan_hole_inner(multi);
        self.hole_depth -= 1;
        end
    }

    fn scan_hole_inner(&mut self, multi: bool) -> Option<HoleEnd> {
        let mut depth = 0usize;
        loop {
            let here = self.pos;
            match self.peek()? {
                '(' | '[' | '{' => {
                    depth += 1;
                    self.pos += 1;
                }
                ')' | ']' => {
                    depth = depth.saturating_sub(1);
                    self.pos += 1;
                }
                '}' if depth == 0 => {
                    self.pos += 1;
                    return Some(HoleEnd {
                        expr_end: here,
                        spec: None,
                    });
                }
                '}' => {
                    depth -= 1;
                    self.pos += 1;
                }
                '"' => self.skip_nested_string(multi)?,
                '\'' => self.skip_char_literal(),
                ':' if depth == 0 => {
                    // The rest up to the closing `}` is the format spec.
                    self.pos += 1;
                    let spec_start = self.pos;
                    loop {
                        match self.peek()? {
                            '}' => break,
                            '\n' if !multi => return None,
                            c => self.pos += c.len_utf8(),
                        }
                    }
                    let spec_end = self.pos;
                    self.pos += 1;
                    return Some(HoleEnd {
                        expr_end: here,
                        spec: Some((spec_start, spec_end)),
                    });
                }
                '\n' if !multi => return None,
                c => self.pos += c.len_utf8(),
            }
        }
    }

    /// Skips a string literal inside a hole (`pos` at its opening `"`),
    /// including holes nested in it. `None` if it never closes.
    fn skip_nested_string(&mut self, multi: bool) -> Option<()> {
        self.pos += 1;
        loop {
            match self.peek()? {
                '"' => {
                    self.pos += 1;
                    return Some(());
                }
                '\\' => self.skip_escape(),
                '{' if self.rest().starts_with("{{") => self.pos += 2,
                '{' => {
                    self.pos += 1;
                    self.scan_hole(multi)?;
                }
                '\n' if !multi => return None,
                c => self.pos += c.len_utf8(),
            }
        }
    }

    /// Skips a char literal inside a hole so `'"'` and `'}'` don't confuse the
    /// scan. A `'` that is not a char literal (a label) is skipped alone.
    fn skip_char_literal(&mut self) {
        let start = self.pos;
        self.pos += 1;
        match self.peek() {
            Some('\\') => {
                self.skip_escape();
                if self.peek() == Some('\'') {
                    self.pos += 1;
                }
            }
            Some(c) if c != '\'' && self.peek_nth(1) == Some('\'') => {
                self.pos += c.len_utf8() + 1;
            }
            _ => self.pos = start + 1,
        }
    }

    // ---- building string parts ----

    fn build_parts(&mut self, mut segs: Vec<Seg>, multi: bool, raw: bool) -> Vec<StrPart> {
        let mut indent = 0;
        if multi {
            self.trim_multiline(&mut segs);
            indent = self.common_indent(&segs);
        }
        let mut parts = Vec::new();
        let mut buf = String::new();
        for (i, seg) in segs.into_iter().enumerate() {
            match seg {
                Seg::Text { start, end } => {
                    self.append_text(&mut buf, start, end, i == 0, indent, raw);
                }
                Seg::Hole { expr, spec } => {
                    if !buf.is_empty() {
                        parts.push(StrPart::Lit(std::mem::take(&mut buf)));
                    }
                    parts.push(StrPart::Interp { expr, spec });
                }
            }
        }
        if !buf.is_empty() {
            parts.push(StrPart::Lit(buf));
        }
        parts
    }

    /// Removes the line break right after the opening `"""` and the
    /// whitespace-only line before the closing `"""` (with its line break).
    fn trim_multiline(&self, segs: &mut [Seg]) {
        let src = self.src;
        if let Some(Seg::Text { start, end }) = segs.last_mut() {
            let body = &src[*start..*end];
            if let Some(nl) = body.rfind('\n')
                && body[nl + 1..].chars().all(|c| c == ' ' || c == '\t')
            {
                let cut = if body[..nl].ends_with('\r') {
                    nl - 1
                } else {
                    nl
                };
                *end = *start + cut;
            }
        }
        if let Some(Seg::Text { start, end }) = segs.first_mut() {
            let body = &src[*start..*end];
            if body.starts_with("\r\n") {
                *start += 2;
            } else if body.starts_with('\n') {
                *start += 1;
            }
        }
    }

    /// The whitespace prefix shared by all non-blank lines of the body.
    fn common_indent(&self, segs: &[Seg]) -> usize {
        // A body that opens with a hole has a non-blank first line at column 0.
        let mut min = if matches!(segs.first(), Some(Seg::Hole { .. })) {
            0
        } else {
            usize::MAX
        };
        for (i, seg) in segs.iter().enumerate() {
            let Seg::Text { start, end } = seg else {
                continue;
            };
            let followed_by_hole = i + 1 < segs.len();
            let lines: Vec<&str> = self.src[*start..*end].split('\n').collect();
            for (j, line) in lines.iter().enumerate() {
                // Only the first piece of the first segment, and pieces after
                // a line break, begin a source line.
                if j == 0 && i > 0 {
                    continue;
                }
                let indent = line
                    .bytes()
                    .take_while(|&b| b == b' ' || b == b'\t')
                    .count();
                let rest = line[indent..].trim_end_matches('\r');
                let ends_at_hole = j + 1 == lines.len() && followed_by_hole;
                if !rest.is_empty() || ends_at_hole {
                    min = min.min(indent);
                }
            }
        }
        if min == usize::MAX { 0 } else { min }
    }

    /// Appends the text in `start..end` to `buf`, stripping up to `indent`
    /// whitespace bytes at each line start and processing escapes.
    fn append_text(
        &mut self,
        buf: &mut String,
        start: usize,
        end: usize,
        first: bool,
        indent: usize,
        raw: bool,
    ) {
        let src = self.src;
        let mut offset = start;
        for (j, line) in src[start..end].split_inclusive('\n').enumerate() {
            let strip = if j > 0 || first {
                line.bytes()
                    .take(indent)
                    .take_while(|&b| b == b' ' || b == b'\t')
                    .count()
            } else {
                0
            };
            self.unescape(buf, offset + strip, offset + line.len(), raw);
            offset += line.len();
        }
    }

    /// Appends `src[start..end]` to `buf`, processing escapes, `{{`/`}}` and
    /// CRLF (unless `raw`, which only normalizes CRLF).
    fn unescape(&mut self, buf: &mut String, start: usize, end: usize, raw: bool) {
        let src = self.src;
        let text = &src[start..end];
        let mut i = 0;
        while let Some(c) = text[i..].chars().next() {
            let rest = &text[i + c.len_utf8()..];
            match c {
                '\r' if rest.starts_with('\n') => i += 1,
                '\\' if !raw => match parse_escape(rest) {
                    Ok((value, len)) => {
                        buf.push(value);
                        i += 1 + len;
                    }
                    Err(msg) => {
                        let len = rest.chars().next().map_or(0, char::len_utf8);
                        let span = self.span(start + i, start + i + 1 + len);
                        self.error(escape_error(msg, span));
                        i += 1 + len;
                    }
                },
                '{' | '}' if !raw && rest.starts_with(c) => {
                    buf.push(c);
                    i += 2;
                }
                _ => {
                    buf.push(c);
                    i += c.len_utf8();
                }
            }
        }
    }
}

fn text(start: usize, end: usize) -> Option<Seg> {
    (end > start).then_some(Seg::Text { start, end })
}

fn too_large(span: Span) -> Diagnostic {
    Diagnostic::error("integer literal is too large", span)
        .with_hint("integer literals must fit in 64 bits (at most 18446744073709551615)")
}

fn escape_error(msg: String, span: Span) -> Diagnostic {
    Diagnostic::error(msg, span)
        .with_hint("valid escapes are \\n \\r \\t \\\\ \\' \\\" \\0 \\{ \\} and \\u{hex}")
}

/// Parses the escape after a backslash (`rest` starts just after it).
/// Returns the character and how many bytes of `rest` it used.
fn parse_escape(rest: &str) -> Result<(char, usize), String> {
    let Some(c) = rest.chars().next() else {
        return Err("unfinished escape sequence".to_string());
    };
    let value = match c {
        'n' => '\n',
        'r' => '\r',
        't' => '\t',
        '\\' => '\\',
        '\'' => '\'',
        '"' => '"',
        '0' => '\0',
        '{' => '{',
        '}' => '}',
        'u' => return parse_unicode_escape(&rest[1..]).map(|(ch, len)| (ch, len + 1)),
        other => return Err(format!("unknown escape `\\{other}`")),
    };
    Ok((value, c.len_utf8()))
}

/// Parses `{hex}` after `\u`.
fn parse_unicode_escape(rest: &str) -> Result<(char, usize), String> {
    let Some(body) = rest.strip_prefix('{') else {
        return Err("expected `{` after `\\u`".to_string());
    };
    let close = body
        .char_indices()
        .take(8)
        .find_map(|(i, c)| (c == '}').then_some(i))
        .ok_or("unterminated `\\u{...}` escape")?;
    let hex = &body[..close];
    if hex.is_empty() || hex.len() > 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("`\\u{...}` takes 1 to 6 hex digits".to_string());
    }
    let value = u32::from_str_radix(hex, 16).map_err(|e| e.to_string())?;
    let c = char::from_u32(value).ok_or("`\\u{...}` is not a valid Unicode character")?;
    Ok((c, 1 + close + 1))
}
