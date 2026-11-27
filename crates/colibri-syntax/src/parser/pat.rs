//! Patterns (DESIGN §8.2).

use super::{PResult, Parser};
use crate::ast::*;
use crate::{Diagnostic, StrPart, TokenKind};

enum FieldOrRest {
    Field(FieldPat),
    Rest,
}

impl Parser<'_> {
    pub(crate) fn make_pat(&mut self, start: u32, kind: PatKind) -> Pat {
        Pat {
            id: self.id(),
            span: self.span_from(start),
            kind,
        }
    }

    /// A full pattern: or-patterns with an optional `as name` binding.
    ///
    /// TODO(design): `a | b as n` binds the whole or-pattern; DESIGN only
    /// shows `Move(d) as cmd`.
    pub(crate) fn parse_pat(&mut self) -> PResult<Pat> {
        let start = self.start();
        let mut pat = self.parse_pat_or()?;
        while self.eat(&TokenKind::As) {
            let name = self.expect_ident("a name after `as`")?;
            pat = self.make_pat(
                start,
                PatKind::Bind {
                    pat: Box::new(pat),
                    name,
                },
            );
        }
        Ok(pat)
    }

    fn parse_pat_or(&mut self) -> PResult<Pat> {
        let start = self.start();
        let first = self.parse_pat_primary()?;
        if !self.at(&TokenKind::Pipe) {
            return Ok(first);
        }
        let mut alternatives = vec![first];
        while self.eat(&TokenKind::Pipe) {
            alternatives.push(self.parse_pat_primary()?);
        }
        Ok(self.make_pat(start, PatKind::Or(alternatives)))
    }

    fn parse_pat_primary(&mut self) -> PResult<Pat> {
        self.nested(|p| p.parse_pat_primary_inner())
    }

    fn parse_pat_primary_inner(&mut self) -> PResult<Pat> {
        let start = self.start();
        match self.kind().clone() {
            TokenKind::Underscore => {
                self.bump();
                Ok(self.make_pat(start, PatKind::Wild))
            }
            TokenKind::None => {
                self.bump();
                Ok(self.make_pat(start, PatKind::None))
            }
            TokenKind::Some | TokenKind::Ok | TokenKind::Err => self.parse_wrapper_pat(),
            TokenKind::Minus
            | TokenKind::Int(_)
            | TokenKind::Float(_)
            | TokenKind::Char(_)
            | TokenKind::Str(_)
            | TokenKind::True
            | TokenKind::False => self.parse_lit_or_range_pat(),
            TokenKind::DotDot | TokenKind::DotDotEq => self.parse_lit_or_range_pat(),
            TokenKind::LParen => self.parse_tuple_pat(),
            TokenKind::Dot | TokenKind::Ident(_) => self.parse_path_pat(),
            TokenKind::Mut => {
                let message = format!("expected a pattern, found {}", self.describe());
                let diag = Diagnostic::error(message, self.found_span()).with_hint(
                    "a pattern cannot mark a single binding `mut` (`if let`, `while let` and \
                     `match` patterns cannot be `mut`): bind the value, then write \
                     `let mut x = x`; in a `let`, `let mut` covers the whole pattern",
                );
                self.fail(diag)
            }
            _ => self.unexpected("a pattern"),
        }
    }

    /// `some(p)`, `ok(p)`, `err(p)`
    fn parse_wrapper_pat(&mut self) -> PResult<Pat> {
        let start = self.start();
        let word = self.bump().kind;
        self.expect(&TokenKind::LParen)?;
        let inner = Box::new(self.parse_pat()?);
        self.eat(&TokenKind::Comma);
        self.expect(&TokenKind::RParen)?;
        let kind = match word {
            TokenKind::Some => PatKind::Some(inner),
            TokenKind::Ok => PatKind::Ok(inner),
            _ => PatKind::Err(inner),
        };
        Ok(self.make_pat(start, kind))
    }

    fn parse_tuple_pat(&mut self) -> PResult<Pat> {
        let start = self.start();
        self.bump();
        let mut items = Vec::new();
        let mut trailing_comma = false;
        while !self.at(&TokenKind::RParen) {
            items.push(self.parse_pat()?);
            trailing_comma = self.eat(&TokenKind::Comma);
            if !trailing_comma {
                break;
            }
        }
        self.expect(&TokenKind::RParen)?;
        if items.len() == 1 && !trailing_comma {
            // `(p)` is just `p`.
            let mut only = items.remove(0);
            only.span = self.span_from(start);
            return Ok(only);
        }
        Ok(self.make_pat(start, PatKind::Tuple(items)))
    }

    /// A literal (possibly negative) in a pattern.
    fn parse_pat_lit(&mut self) -> PResult<PatLit> {
        let start = self.start();
        let neg = self.eat(&TokenKind::Minus);
        let token = self.tok().clone();
        let lit = match token.kind {
            TokenKind::Int(n) => Lit::Int(n),
            TokenKind::Float(x) => Lit::Float(x),
            TokenKind::Char(c) if !neg => Lit::Char(c),
            TokenKind::True if !neg => Lit::Bool(true),
            TokenKind::False if !neg => Lit::Bool(false),
            TokenKind::Str(ref s) if !neg => {
                let mut text = String::new();
                for part in &s.parts {
                    match part {
                        StrPart::Lit(t) => text.push_str(t),
                        StrPart::Interp { .. } => {
                            let diag = Diagnostic::error(
                                "a string pattern cannot contain `{...}` holes",
                                token.span,
                            );
                            return self.fail(diag);
                        }
                    }
                }
                Lit::Str(text)
            }
            _ => return self.unexpected("a literal"),
        };
        self.bump();
        Ok(PatLit {
            span: self.span_from(start),
            neg,
            lit,
        })
    }

    /// A literal pattern, or a range `lo..hi`, `lo..=hi`, `lo..`, `..hi`.
    fn parse_lit_or_range_pat(&mut self) -> PResult<Pat> {
        let start = self.start();
        let lo = if matches!(self.kind(), TokenKind::DotDot | TokenKind::DotDotEq) {
            None
        } else {
            Some(self.parse_pat_lit()?)
        };
        if !matches!(self.kind(), TokenKind::DotDot | TokenKind::DotDotEq) {
            let lit = lo.unwrap_or_else(|| unreachable!("a missing lower bound needs `..`"));
            return Ok(self.make_pat(start, PatKind::Lit(lit)));
        }
        let inclusive = self.bump().kind == TokenKind::DotDotEq;
        let hi = if matches!(
            self.kind(),
            TokenKind::Minus
                | TokenKind::Int(_)
                | TokenKind::Float(_)
                | TokenKind::Char(_)
                | TokenKind::Str(_)
        ) {
            Some(self.parse_pat_lit()?)
        } else {
            None
        };
        // `..=` ends at its upper bound; with none it would say "up to and including nothing",
        // and a bare `..=` would quietly match everything like `_`.
        if inclusive && hi.is_none() {
            return self.unexpected("an upper bound after `..=`");
        }
        Ok(self.make_pat(start, PatKind::Range { lo, hi, inclusive }))
    }

    /// `x`, `Circle`, `Shape.Circle`, `.Circle`, with optional `(args)` or
    /// `{ fields }`.
    fn parse_path_pat(&mut self) -> PResult<Pat> {
        let start = self.start();
        let leading_dot = self.eat(&TokenKind::Dot);
        let mut segments = vec![self.expect_ident("a name")?];
        while self.at(&TokenKind::Dot) && matches!(self.nth(1), TokenKind::Ident(_)) {
            self.bump();
            segments.push(self.expect_ident("a name")?);
        }
        let path = PatPath {
            leading_dot,
            segments,
        };
        if self.eat(&TokenKind::LParen) {
            let args = self.parse_comma_list(&TokenKind::RParen, |p| p.parse_pat())?;
            let kind = PatKind::Variant {
                path,
                args: Some(args),
            };
            return Ok(self.make_pat(start, kind));
        }
        if self.at(&TokenKind::LBrace) {
            let mut rest = false;
            let items = self.parse_braced("fields", |p| p.parse_field_pat())?;
            let mut fields = Vec::new();
            for item in items {
                match item {
                    FieldOrRest::Field(f) => fields.push(f),
                    FieldOrRest::Rest => rest = true,
                }
            }
            let kind = PatKind::Struct { path, fields, rest };
            return Ok(self.make_pat(start, kind));
        }
        if !path.leading_dot && path.segments.len() == 1 {
            let name = path
                .segments
                .into_iter()
                .next()
                .unwrap_or_else(|| unreachable!());
            return Ok(self.make_pat(start, PatKind::Ident(name)));
        }
        Ok(self.make_pat(start, PatKind::Variant { path, args: None }))
    }

    /// `x`, `x: pat` or `..` in a struct pattern.
    fn parse_field_pat(&mut self) -> PResult<FieldOrRest> {
        if self.eat(&TokenKind::DotDot) {
            return Ok(FieldOrRest::Rest);
        }
        let start = self.start();
        let name = self.expect_ident("a field name")?;
        if self.eat(&TokenKind::Colon) {
            let pat = self.parse_pat()?;
            Ok(FieldOrRest::Field(FieldPat {
                name,
                shorthand: false,
                pat,
            }))
        } else {
            let pat = self.make_pat(start, PatKind::Ident(name.clone()));
            Ok(FieldOrRest::Field(FieldPat {
                name,
                shorthand: true,
                pat,
            }))
        }
    }
}
