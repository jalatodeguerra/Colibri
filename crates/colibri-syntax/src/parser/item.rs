//! Items: functions, types, impls, imports, `extern` blocks, tests and
//! attributes (DESIGN §3.6, §6, §7, §8, §9, §13, §15).

use super::{Failed, PResult, Parser};
use crate::ast::*;
use crate::{Diagnostic, Span, StrPart, TokenKind};

/// Whether a function needs a body.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Body {
    Required,
    /// Interface method signatures and `extern` declarations.
    Optional,
}

/// Where a member item is being parsed.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Member {
    Impl,
    Interface,
    Extern,
}

impl Member {
    fn body(self) -> Body {
        match self {
            Member::Impl => Body::Required,
            Member::Interface | Member::Extern => Body::Optional,
        }
    }

    fn describe(self) -> &'static str {
        match self {
            Member::Impl => "`impl` blocks",
            Member::Interface => "`interface` blocks",
            Member::Extern => "`extern` blocks",
        }
    }
}

impl Parser<'_> {
    /// Whether the current token starts an item that is not a plain statement.
    /// `const`, `let` and `let mut` are not listed: they are items at module level
    /// and statements elsewhere.
    pub(crate) fn starts_item(&self) -> bool {
        match self.kind() {
            TokenKind::Struct
            | TokenKind::Class
            | TokenKind::Enum
            | TokenKind::Interface
            | TokenKind::Impl
            | TokenKind::Import
            | TokenKind::Extern
            | TokenKind::Type
            | TokenKind::At
            | TokenKind::Pub => true,
            TokenKind::Fn => matches!(self.nth(1), TokenKind::Ident(_)),
            TokenKind::Test => matches!(self.nth(1), TokenKind::Str(_)),
            _ => false,
        }
    }

    /// The whole file: items and statements interleaved.
    pub(crate) fn parse_file(&mut self) -> File {
        let mut items = Vec::new();
        loop {
            self.skip_semis();
            if self.at_eof() {
                break;
            }
            if self.at(&TokenKind::RBrace) {
                let diag = Diagnostic::error("unmatched `}`", self.found_span())
                    .with_hint("remove it, or add the `{` it closes");
                self.report(diag);
                self.bump();
                continue;
            }
            let before = self.pos;
            let start = self.start();
            match self.parse_top_level() {
                Ok(top) => items.push(top),
                Err(Failed) => {
                    self.sync_stmt(true);
                    if self.pos == before {
                        self.bump();
                    }
                    let span = self.span_from(start);
                    let id = self.id();
                    let kind = StmtKind::Error;
                    items.push(TopLevel::Stmt(Stmt { id, span, kind }));
                }
            }
        }
        File {
            items,
            span: Span::new(self.file, 0, self.src.len() as u32),
        }
    }

    fn parse_top_level(&mut self) -> PResult<TopLevel> {
        let first = self.pos;
        let start = self.start();
        let attrs = self.parse_attrs()?;
        let public = self.eat(&TokenKind::Pub);
        let has_prefix = public || !attrs.is_empty();
        let item_start = self.starts_item()
            || matches!(
                self.kind(),
                TokenKind::Const | TokenKind::Let | TokenKind::Var
            );
        if item_start {
            let doc = self.docs_before(first);
            let item = self.parse_item_rest(start, attrs, doc, public)?;
            self.finish_stmt(false)?;
            // `let _ = expr` binds nothing: it is a statement of the script, so it runs in order
            // with the other top-level statements instead of as a module-level value (also
            // for `const _`).
            return Ok(match item {
                Item {
                    id,
                    span,
                    kind: ItemKind::Binding(binding),
                    public: false,
                    ref attrs,
                    ..
                } if attrs.is_empty() && matches!(binding.pat.kind, PatKind::Wild) => {
                    TopLevel::Stmt(Stmt {
                        id,
                        span,
                        kind: StmtKind::Let(binding),
                    })
                }
                item => TopLevel::Item(item),
            });
        }
        if has_prefix {
            return self.unexpected("an item after the attributes or `pub`");
        }
        let (stmt, blocklike) = self.parse_stmt()?;
        self.finish_stmt(blocklike)?;
        Ok(TopLevel::Stmt(stmt))
    }

    /// Parses `@name` and `@name(args)` attributes (skipping the line break
    /// after each).
    pub(crate) fn parse_attrs(&mut self) -> PResult<Vec<Attr>> {
        let mut attrs = Vec::new();
        while self.at(&TokenKind::At) {
            let start = self.start();
            self.bump();
            let name = self.expect_ident("an attribute name after `@`")?;
            let mut args = Vec::new();
            if self.eat(&TokenKind::LParen) {
                args = self.with_struct_mode(false, |p| {
                    p.parse_comma_list(&TokenKind::RParen, |p| p.parse_expr())
                })?;
            }
            let span = self.span_from(start);
            // `@when(a, b)` is two conditions, an error the loader reports; `@when(a,)` has
            // one and the comma means nothing, which the formatter could not keep.
            if name.name == "when"
                && !args.is_empty()
                && self.pos >= 2
                && self.toks[self.pos - 2].kind == TokenKind::Comma
            {
                let comma = self.toks[self.pos - 2].span;
                self.report(
                    Diagnostic::error(
                        "`@when` takes one condition, without a trailing comma",
                        comma,
                    )
                    .with_hint("write `@when(debug)`; to join conditions use `&&` or `||`"),
                );
            }
            attrs.push(Attr { span, name, args });
            self.skip_semis();
        }
        Ok(attrs)
    }

    /// Parses the item that starts at the current token (after attributes
    /// and `pub`).
    fn parse_item_rest(
        &mut self,
        start: u32,
        attrs: Vec<Attr>,
        doc: Option<String>,
        public: bool,
    ) -> PResult<Item> {
        let kind = match self.kind() {
            TokenKind::Fn => {
                let rule = if self.interface_mode {
                    Body::Optional
                } else {
                    Body::Required
                };
                ItemKind::Fn(self.parse_fn_decl(rule)?)
            }
            TokenKind::Struct | TokenKind::Class => ItemKind::Struct(self.parse_struct()?),
            TokenKind::Enum => ItemKind::Enum(self.parse_enum()?),
            TokenKind::Interface => ItemKind::Interface(self.parse_interface()?),
            TokenKind::Impl => ItemKind::Impl(self.parse_impl()?),
            TokenKind::Type => ItemKind::TypeAlias(self.parse_type_alias()?),
            TokenKind::Import => ItemKind::Import(self.parse_import()?),
            TokenKind::Extern => ItemKind::Extern(self.parse_extern()?),
            TokenKind::Test => ItemKind::Test(self.parse_test()?),
            TokenKind::Const | TokenKind::Let | TokenKind::Var => {
                ItemKind::Binding(self.parse_let(false)?)
            }
            _ => return self.unexpected("an item"),
        };
        Ok(self.make_item(start, attrs, doc, public, kind))
    }

    fn make_item(
        &mut self,
        start: u32,
        attrs: Vec<Attr>,
        doc: Option<String>,
        public: bool,
        kind: ItemKind,
    ) -> Item {
        Item {
            id: self.id(),
            span: self.span_from(start),
            attrs,
            doc,
            public,
            kind,
        }
    }

    /// Parses an item in a block, only to report that it does not belong
    /// there and to skip it cleanly.
    pub(crate) fn skip_local_item(&mut self) -> PResult<()> {
        let at = self.found_span();
        let (what, hint) = match self.kind() {
            TokenKind::Fn => (
                "functions cannot be declared inside a block".to_string(),
                "declare it at module level, or bind a closure: `let name = fn(x) => ...`",
            ),
            TokenKind::At | TokenKind::Pub => (
                "attributes and `pub` are only allowed on module-level items".to_string(),
                "move the declaration out of the block",
            ),
            kind => (
                format!(
                    "`{}` declarations are only allowed at module level",
                    kind.fixed_text().unwrap_or("item")
                ),
                "move the declaration out of the block",
            ),
        };
        self.report(Diagnostic::error(what, at).with_hint(hint));
        let start = self.start();
        let attrs = self.parse_attrs()?;
        let public = self.eat(&TokenKind::Pub);
        self.parse_item_rest(start, attrs, None, public)?;
        Ok(())
    }

    // ---- functions ----

    fn parse_fn_decl(&mut self, body_rule: Body) -> PResult<FnDecl> {
        self.expect(&TokenKind::Fn)?;
        let name = self.expect_ident("a function name")?;
        let generics = self.parse_generics_opt()?;
        self.expect(&TokenKind::LParen)?;
        let (receiver, params) = self.parse_params(false)?;
        let ret = if self.eat(&TokenKind::Arrow) {
            Some(self.parse_type()?)
        } else {
            None
        };
        let body = if self.at(&TokenKind::LBrace) {
            Some(FnBody::Block(self.parse_block()?))
        } else if self.eat(&TokenKind::FatArrow) {
            Some(FnBody::Expr(
                self.with_struct_mode(false, |p| p.parse_expr())?,
            ))
        } else if body_rule == Body::Optional {
            None
        } else {
            let message = format!(
                "expected `{{` or `=>` to start the body of `{}`, found {}",
                name.name,
                self.describe()
            );
            let diag = Diagnostic::error(message, self.found_span())
                .with_hint("write a block `{ ... }` or an expression body `=> expr`");
            return self.fail(diag);
        };
        Ok(FnDecl {
            name,
            generics,
            receiver,
            params,
            ret,
            body,
        })
    }

    /// Parses parameters after `(`, through `)`. Closures may omit types and
    /// write `mut x`; functions may start with `self` or `mut self`.
    pub(crate) fn parse_params(
        &mut self,
        closure: bool,
    ) -> PResult<(Option<Receiver>, Vec<Param>)> {
        let mut receiver = None;
        let mut params = Vec::new();
        let mut first = true;
        while !self.at(&TokenKind::RParen) {
            let start = self.start();
            let is_self = self.at(&TokenKind::SelfKw);
            let is_mut_self = matches!(self.kind(), TokenKind::Mut | TokenKind::Var)
                && *self.nth(1) == TokenKind::SelfKw;
            if first && !closure && (is_self || is_mut_self) {
                let mutable = is_mut_self;
                if self.at(&TokenKind::Var) {
                    self.report_old_var("write `mut self`");
                }
                if mutable {
                    self.bump();
                }
                self.bump();
                receiver = Some(Receiver {
                    mutable,
                    span: self.span_from(start),
                });
            } else {
                params.push(self.parse_param(closure)?);
            }
            first = false;
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        if !self.at(&TokenKind::RParen) {
            return self.unexpected("`,` or `)` in the parameter list");
        }
        self.bump();
        Ok((receiver, params))
    }

    fn parse_param(&mut self, closure: bool) -> PResult<Param> {
        let start = self.start();
        let mut mutable = self.eat(&TokenKind::Mut);
        // `var self` in a closure only gets the error about `self`.
        if !mutable && self.at(&TokenKind::Var) && *self.nth(1) != TokenKind::SelfKw {
            let hint = match (self.name_after_keyword(), closure) {
                (Some(name), true) => format!("write `mut {name}`"),
                (Some(name), false) => format!("write `mut {name}: Type`"),
                (None, _) => "write `mut` instead of `var`".to_string(),
            };
            self.report_old_var(&hint);
            self.bump();
            mutable = true;
        } else if !mutable && self.at(&TokenKind::Var) {
            self.bump();
        }
        let name = self.expect_ident("a parameter name")?;
        let ty = if self.eat(&TokenKind::Colon) {
            Some(self.parse_type()?)
        } else {
            if !closure {
                let diag =
                    Diagnostic::error(format!("parameter `{}` needs a type", name.name), name.span)
                        .with_hint(format!(
                            "function signatures are always annotated: `{}: Type`",
                            name.name
                        ));
                self.report(diag);
            }
            None
        };
        let default = if self.eat(&TokenKind::Eq) {
            Some(self.with_struct_mode(false, |p| p.parse_expr())?)
        } else {
            None
        };
        Ok(Param {
            id: self.id(),
            span: self.span_from(start),
            mutable,
            name,
            ty,
            default,
        })
    }

    /// Optional `[T: Bound + Bound, U, R = Self]`.
    fn parse_generics_opt(&mut self) -> PResult<Vec<GenericParam>> {
        if !self.eat(&TokenKind::LBracket) {
            return Ok(Vec::new());
        }
        self.parse_generic_params()
    }

    /// Generic parameters after `[`, through `]`.
    fn parse_generic_params(&mut self) -> PResult<Vec<GenericParam>> {
        self.parse_comma_list(&TokenKind::RBracket, |p| {
            let name = p.expect_ident("a generic parameter name")?;
            let mut bounds = Vec::new();
            if p.eat(&TokenKind::Colon) {
                loop {
                    bounds.push(p.parse_type()?);
                    if !p.eat(&TokenKind::Plus) {
                        break;
                    }
                }
            }
            let default = if p.eat(&TokenKind::Eq) {
                Some(p.parse_type()?)
            } else {
                None
            };
            Ok(GenericParam {
                name,
                bounds,
                default,
            })
        })
    }

    // ---- types ----

    fn parse_struct(&mut self) -> PResult<StructDecl> {
        let kind = if self.bump().kind == TokenKind::Class {
            StructKind::Class
        } else {
            StructKind::Struct
        };
        let name = self.expect_ident("a type name")?;
        let generics = self.parse_generics_opt()?;
        let fields = self.parse_braced("fields", |p| p.parse_field())?;
        Ok(StructDecl {
            kind,
            name,
            generics,
            fields,
        })
    }

    fn parse_field(&mut self) -> PResult<Field> {
        let first = self.pos;
        let start = self.start();
        let doc = self.docs_before(first);
        let attrs = self.parse_attrs()?;
        let public = self.eat(&TokenKind::Pub);
        let name = self.expect_ident("a field name")?;
        if !self.at(&TokenKind::Colon) {
            let message = format!(
                "expected `:` and a type after field `{}`, found {}",
                name.name,
                self.describe()
            );
            let diag = Diagnostic::error(message, self.found_span())
                .with_hint(format!("fields are written `{}: Type`", name.name));
            return self.fail(diag);
        }
        self.bump();
        let ty = self.parse_type()?;
        let default = if self.eat(&TokenKind::Eq) {
            Some(self.parse_expr()?)
        } else {
            None
        };
        Ok(Field {
            id: self.id(),
            span: self.span_from(start),
            doc,
            attrs,
            public,
            name,
            ty,
            default,
        })
    }

    fn parse_enum(&mut self) -> PResult<EnumDecl> {
        self.bump();
        let name = self.expect_ident("an enum name")?;
        let generics = self.parse_generics_opt()?;
        let variants = self.parse_braced("variants", |p| p.parse_variant())?;
        Ok(EnumDecl {
            name,
            generics,
            variants,
        })
    }

    fn parse_variant(&mut self) -> PResult<Variant> {
        let first = self.pos;
        let start = self.start();
        let doc = self.docs_before(first);
        if self.at(&TokenKind::At) {
            let at = self.found_span();
            let hint = match self.nth(1) {
                TokenKind::Ident(name) if name == "when" => {
                    "a variant cannot be left out of one build: the enum would change shape; put `@when` on the whole enum"
                }
                TokenKind::Ident(name) if name == "deprecated" => {
                    "`@deprecated` goes on the whole enum"
                }
                _ => "attributes go on a declaration or a field, not on a variant",
            };
            let diag = Diagnostic::error("attributes are not supported on enum variants", at)
                .with_hint(hint);
            return self.fail(diag);
        }
        let name = self.expect_ident("a variant name")?;
        let payload = if self.eat(&TokenKind::LParen) {
            let named =
                matches!(self.kind(), TokenKind::Ident(_)) && *self.nth(1) == TokenKind::Colon;
            if named {
                VariantPayload::Named(self.parse_comma_list(&TokenKind::RParen, |p| {
                    let name = p.expect_ident("a field name")?;
                    p.expect(&TokenKind::Colon)?;
                    let ty = p.parse_type()?;
                    Ok(VariantField { name, ty })
                })?)
            } else {
                VariantPayload::Positional(
                    self.parse_comma_list(&TokenKind::RParen, |p| p.parse_type())?,
                )
            }
        } else {
            VariantPayload::Unit
        };
        Ok(Variant {
            id: self.id(),
            span: self.span_from(start),
            doc,
            name,
            payload,
        })
    }

    fn parse_interface(&mut self) -> PResult<InterfaceDecl> {
        self.bump();
        let name = self.expect_ident("an interface name")?;
        let generics = self.parse_generics_opt()?;
        let items = self.parse_members(Member::Interface)?;
        Ok(InterfaceDecl {
            name,
            generics,
            items,
        })
    }

    /// `impl Type`, `impl Iface for Type` and `impl[T] Iface for List[T]`.
    ///
    /// A `[` directly after `impl` always opens generic parameters.
    fn parse_impl(&mut self) -> PResult<ImplDecl> {
        self.bump();
        let generics = self.parse_generics_opt()?;
        let first = self.parse_type()?;
        let (interface, target) = if self.eat(&TokenKind::For) {
            (Some(first), self.parse_type()?)
        } else {
            (None, first)
        };
        let items = self.parse_members(Member::Impl)?;
        Ok(ImplDecl {
            generics,
            interface,
            target,
            items,
        })
    }

    /// The `fn` members of an `impl`, `interface` or `extern` block.
    fn parse_members(&mut self, member: Member) -> PResult<Vec<Item>> {
        self.parse_braced("methods", |p| {
            let first = p.pos;
            let start = p.start();
            let attrs = p.parse_attrs()?;
            let doc = p.docs_before(first);
            let public = p.eat(&TokenKind::Pub);
            if !p.at(&TokenKind::Fn) {
                let message = format!(
                    "only functions can be declared in {}, found {}",
                    member.describe(),
                    p.describe()
                );
                return p.fail(Diagnostic::error(message, p.found_span()));
            }
            let rule = if p.interface_mode && member == Member::Impl {
                Body::Optional
            } else {
                member.body()
            };
            let decl = p.parse_fn_decl(rule)?;
            Ok(p.make_item(start, attrs, doc, public, ItemKind::Fn(decl)))
        })
    }

    fn parse_type_alias(&mut self) -> PResult<TypeAlias> {
        self.bump();
        let name = self.expect_ident("a type name")?;
        let generics = self.parse_generics_opt()?;
        self.expect(&TokenKind::Eq)?;
        let ty = self.parse_type()?;
        Ok(TypeAlias { name, generics, ty })
    }

    // ---- modules ----

    /// `import a.b`, `import a.b as c`, `import a.{B, C as D}`.
    fn parse_import(&mut self) -> PResult<Import> {
        self.bump();
        let mut path = vec![self.expect_ident("a module name after `import`")?];
        let mut kind = ImportKind::Plain;
        while self.eat(&TokenKind::Dot) {
            if self.at(&TokenKind::LBrace) {
                let names = self.parse_braced("imported names", |p| {
                    let name = p.expect_ident("a name to import")?;
                    let alias = if p.eat(&TokenKind::As) {
                        Some(p.expect_ident("a name after `as`")?)
                    } else {
                        None
                    };
                    Ok(ImportName { name, alias })
                })?;
                kind = ImportKind::Names(names);
                break;
            }
            path.push(self.expect_ident("a module name after `.`")?);
        }
        if kind == ImportKind::Plain && self.eat(&TokenKind::As) {
            kind = ImportKind::Alias(self.expect_ident("a name after `as`")?);
        }
        Ok(Import { path, kind })
    }

    /// `extern "C" { fn f(...) -> T }`
    fn parse_extern(&mut self) -> PResult<ExternBlock> {
        self.bump();
        let abi = self.parse_plain_string("an ABI string such as `\"C\"` after `extern`")?;
        let items = self.parse_members(Member::Extern)?;
        Ok(ExternBlock { abi, items })
    }

    /// `test "name" { ... }`
    fn parse_test(&mut self) -> PResult<TestDecl> {
        self.bump();
        let name = self.parse_plain_string("a test name in quotes after `test`")?;
        let body = self.parse_block()?;
        Ok(TestDecl { name, body })
    }

    /// A string literal without interpolation holes.
    fn parse_plain_string(&mut self, what: &str) -> PResult<String> {
        let TokenKind::Str(lit) = self.kind().clone() else {
            return self.unexpected(what);
        };
        let span = self.bump().span;
        let mut text = String::new();
        for part in &lit.parts {
            match part {
                StrPart::Lit(s) => text.push_str(s),
                StrPart::Interp { .. } => {
                    return self.fail(Diagnostic::error(
                        "this string cannot contain `{...}` holes",
                        span,
                    ));
                }
            }
        }
        Ok(text)
    }
}
