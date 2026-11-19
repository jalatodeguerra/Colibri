//! Type expressions (DESIGN §5): `List[T]`, `T?`, `weak T`, `(A, B)`,
//! `[T; N]` and `fn(A) -> R`.

use super::{PResult, Parser};
use crate::ast::*;
use crate::{Diagnostic, TokenKind};

impl Parser<'_> {
    pub(crate) fn make_type(&mut self, start: u32, kind: TypeKind) -> TypeExpr {
        TypeExpr {
            id: self.id(),
            span: self.span_from(start),
            kind,
        }
    }

    /// A type with its postfix `?` suffixes.
    pub(crate) fn parse_type(&mut self) -> PResult<TypeExpr> {
        self.nested(|p| p.parse_type_inner())
    }

    fn parse_type_inner(&mut self) -> PResult<TypeExpr> {
        let start = self.start();
        if self.eat(&TokenKind::Weak) {
            let inner = self.parse_type()?;
            return Ok(self.make_type(start, TypeKind::Weak(Box::new(inner))));
        }
        let mut ty = self.parse_type_atom()?;
        while self.eat(&TokenKind::Question) {
            ty = self.make_type(start, TypeKind::Optional(Box::new(ty)));
        }
        Ok(ty)
    }

    fn parse_type_atom(&mut self) -> PResult<TypeExpr> {
        let start = self.start();
        match self.kind() {
            TokenKind::Ident(_) => self.parse_named_type(),
            TokenKind::LParen => {
                self.bump();
                self.with_struct_mode(false, |p| {
                    // `(A)` is just `A`; `(A,)` is a one-element tuple.
                    let mut trailing_comma = false;
                    let mut items = Vec::new();
                    while !p.at(&TokenKind::RParen) {
                        items.push(p.parse_type()?);
                        trailing_comma = p.eat(&TokenKind::Comma);
                        if !trailing_comma {
                            break;
                        }
                    }
                    p.expect(&TokenKind::RParen)?;
                    if items.len() == 1 && !trailing_comma {
                        let mut only = items.remove(0);
                        only.span = p.span_from(start);
                        return Ok(only);
                    }
                    Ok(p.make_type(start, TypeKind::Tuple(items)))
                })
            }
            TokenKind::LBracket => {
                self.bump();
                self.with_struct_mode(false, |p| {
                    let elem = p.parse_type()?;
                    if !p.at(&TokenKind::Semi) {
                        let message = format!(
                            "expected `;` and a length in an array type, found {}",
                            p.describe()
                        );
                        let diag = Diagnostic::error(message, p.found_span())
                            .with_hint("fixed-size arrays are written `[T; N]`; use `List[T]` for a growable list");
                        return p.fail(diag);
                    }
                    p.bump();
                    let len = p.parse_expr()?;
                    p.expect(&TokenKind::RBracket)?;
                    let kind = TypeKind::Array {
                        elem: Box::new(elem),
                        len: Box::new(len),
                    };
                    Ok(p.make_type(start, kind))
                })
            }
            TokenKind::Fn => {
                self.bump();
                self.expect(&TokenKind::LParen)?;
                let params = self.with_struct_mode(false, |p| {
                    p.parse_comma_list(&TokenKind::RParen, |p| p.parse_type())
                })?;
                let ret = if self.eat(&TokenKind::Arrow) {
                    Some(Box::new(self.parse_type()?))
                } else {
                    None
                };
                Ok(self.make_type(start, TypeKind::Fn { params, ret }))
            }
            _ => self.unexpected("a type"),
        }
    }

    /// `a.b.Name[Args]`
    fn parse_named_type(&mut self) -> PResult<TypeExpr> {
        let start = self.start();
        let mut path = vec![self.expect_ident("a type name")?];
        while self.at(&TokenKind::Dot) && matches!(self.nth(1), TokenKind::Ident(_)) {
            self.bump();
            path.push(self.expect_ident("a type name")?);
        }
        let mut args = Vec::new();
        if self.eat(&TokenKind::LBracket) {
            args = self.with_struct_mode(false, |p| {
                p.parse_comma_list(&TokenKind::RBracket, |p| p.parse_type())
            })?;
        }
        Ok(self.make_type(start, TypeKind::Named { path, args }))
    }
}
