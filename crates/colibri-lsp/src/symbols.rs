//! `textDocument/documentSymbol`: the outline of a file, from its syntax tree.
//!
//! The buffer is parsed on its own (the parser recovers from errors), so the outline works
//! while the file does not check, and while it does not even parse.

use colibri_syntax::ast::{
    BindKind, EnumDecl, FnDecl, Item, ItemKind, LetStmt, Pat, PatKind, StructKind, TopLevel,
    VariantPayload,
};
use colibri_syntax::{FileId, Span, parse_file};
use lsp_types::{DocumentSymbol, SymbolKind};

use crate::convert::LineIndex;

/// The outline of `text`: one symbol per declaration, with fields, variants and methods as
/// children.
pub fn document_symbols(text: &str) -> Vec<DocumentSymbol> {
    let (file, _) = parse_file(FileId(0), text);
    let cx = Cx {
        text,
        index: LineIndex::new(text),
    };
    let mut out = Vec::new();
    for top in &file.items {
        if let TopLevel::Item(item) = top {
            cx.item(item, &mut out);
        }
    }
    out
}

struct Cx<'a> {
    text: &'a str,
    index: LineIndex<'a>,
}

impl Cx<'_> {
    fn symbol(
        &self,
        name: &str,
        detail: Option<String>,
        kind: SymbolKind,
        range: Span,
        selection: Span,
        children: Vec<DocumentSymbol>,
    ) -> DocumentSymbol {
        // The selection must lie inside the range.
        let selection = if selection.start >= range.start && selection.end <= range.end {
            selection
        } else {
            range
        };
        #[allow(deprecated)]
        DocumentSymbol {
            name: if name.is_empty() {
                "<unnamed>".to_string()
            } else {
                name.to_string()
            },
            detail,
            kind,
            tags: None,
            deprecated: None,
            range: self.index.range(range.start as usize, range.end as usize),
            selection_range: self
                .index
                .range(selection.start as usize, selection.end as usize),
            children: (!children.is_empty()).then_some(children),
        }
    }

    fn slice(&self, from: u32, to: u32) -> &str {
        self.text
            .get(from as usize..to as usize)
            .unwrap_or_default()
            .trim()
    }

    /// `(a: int) -> int`: the source between the name and the body.
    fn signature(&self, item: &Item, f: &FnDecl) -> Option<String> {
        let end = match &f.body {
            Some(colibri_syntax::ast::FnBody::Block(b)) => b.span.start,
            Some(colibri_syntax::ast::FnBody::Expr(e)) => {
                // `=> expr`: stop before the arrow.
                let head = self
                    .text
                    .get(f.name.span.end as usize..e.span.start as usize)?;
                f.name.span.end + head.rfind("=>").unwrap_or(head.len()) as u32
            }
            None => item.span.end,
        };
        if end < f.name.span.end {
            return None;
        }
        let text = self.slice(f.name.span.end, end);
        (!text.is_empty()).then(|| text.to_string())
    }

    fn function(&self, item: &Item, f: &FnDecl, method: bool) -> DocumentSymbol {
        // An associated function without a receiver is a plain function.
        let kind = if method && f.receiver.is_some() {
            SymbolKind::METHOD
        } else {
            SymbolKind::FUNCTION
        };
        self.symbol(
            &f.name.name,
            self.signature(item, f),
            kind,
            item.span,
            f.name.span,
            Vec::new(),
        )
    }

    fn item(&self, item: &Item, out: &mut Vec<DocumentSymbol>) {
        match &item.kind {
            ItemKind::Fn(f) => out.push(self.function(item, f, false)),
            ItemKind::Struct(s) => {
                let children = s
                    .fields
                    .iter()
                    .map(|f| {
                        let ty = self.slice(f.ty.span.start, f.ty.span.end).to_string();
                        self.symbol(
                            &f.name.name,
                            Some(ty),
                            SymbolKind::FIELD,
                            f.span,
                            f.name.span,
                            Vec::new(),
                        )
                    })
                    .collect();
                let kind = match s.kind {
                    StructKind::Struct => SymbolKind::STRUCT,
                    StructKind::Class => SymbolKind::CLASS,
                };
                out.push(self.symbol(&s.name.name, None, kind, item.span, s.name.span, children));
            }
            ItemKind::Enum(e) => out.push(self.enumeration(item, e)),
            ItemKind::Interface(i) => {
                let children = i
                    .items
                    .iter()
                    .filter_map(|m| match &m.kind {
                        ItemKind::Fn(f) => Some(self.function(m, f, true)),
                        _ => None,
                    })
                    .collect();
                out.push(self.symbol(
                    &i.name.name,
                    None,
                    SymbolKind::INTERFACE,
                    item.span,
                    i.name.span,
                    children,
                ));
            }
            ItemKind::Impl(i) => {
                let target = self.slice(i.target.span.start, i.target.span.end);
                let name = match &i.interface {
                    Some(iface) => format!(
                        "impl {} for {target}",
                        self.slice(iface.span.start, iface.span.end)
                    ),
                    None => format!("impl {target}"),
                };
                let children = i
                    .items
                    .iter()
                    .filter_map(|m| match &m.kind {
                        ItemKind::Fn(f) => Some(self.function(m, f, true)),
                        _ => None,
                    })
                    .collect();
                out.push(self.symbol(
                    &name,
                    None,
                    SymbolKind::OBJECT,
                    item.span,
                    i.target.span,
                    children,
                ));
            }
            ItemKind::TypeAlias(a) => out.push(self.symbol(
                &a.name.name,
                Some(self.slice(a.ty.span.start, a.ty.span.end).to_string()),
                SymbolKind::CLASS,
                item.span,
                a.name.span,
                Vec::new(),
            )),
            ItemKind::Binding(b) => self.binding(item, b, out),
            ItemKind::Extern(block) => {
                for m in &block.items {
                    if let ItemKind::Fn(f) = &m.kind {
                        out.push(self.function(m, f, false));
                    }
                }
            }
            ItemKind::Test(t) => {
                let head = self.slice(item.span.start, t.body.span.start);
                out.push(self.symbol(
                    &format!("test \"{}\"", t.name),
                    None,
                    SymbolKind::FUNCTION,
                    item.span,
                    Span::new(
                        item.span.file,
                        item.span.start,
                        item.span.start + head.len() as u32,
                    ),
                    Vec::new(),
                ));
            }
            ItemKind::Import(_) | ItemKind::Error => {}
        }
    }

    fn enumeration(&self, item: &Item, e: &EnumDecl) -> DocumentSymbol {
        let children = e
            .variants
            .iter()
            .map(|v| {
                let detail = match &v.payload {
                    VariantPayload::Unit => None,
                    _ => Some(self.slice(v.name.span.end, v.span.end).to_string()),
                };
                self.symbol(
                    &v.name.name,
                    detail,
                    SymbolKind::ENUM_MEMBER,
                    v.span,
                    v.name.span,
                    Vec::new(),
                )
            })
            .collect();
        self.symbol(
            &e.name.name,
            None,
            SymbolKind::ENUM,
            item.span,
            e.name.span,
            children,
        )
    }

    /// A module-level `let`, `var` or `const`: one symbol for each name the pattern binds.
    fn binding(&self, item: &Item, b: &LetStmt, out: &mut Vec<DocumentSymbol>) {
        let kind = match b.kind {
            BindKind::Const => SymbolKind::CONSTANT,
            BindKind::Let | BindKind::Var => SymbolKind::VARIABLE,
        };
        let detail =
            b.ty.as_ref()
                .map(|t| self.slice(t.span.start, t.span.end).to_string());
        let mut names = Vec::new();
        match &b.pat.kind {
            // `const MAX = 1`: a capitalised name is still the binding's name.
            PatKind::Ident(id) => names.push((id.name.clone(), id.span)),
            _ => pattern_names(&b.pat, &mut names),
        }
        for (name, span) in names {
            out.push(self.symbol(&name, detail.clone(), kind, item.span, span, Vec::new()));
        }
    }
}

/// The names a pattern binds, with their spans.
pub fn pattern_names(pat: &Pat, out: &mut Vec<(String, Span)>) {
    match &pat.kind {
        PatKind::Ident(id) if !id.is_capitalised() => out.push((id.name.clone(), id.span)),
        PatKind::Some(p) | PatKind::Ok(p) | PatKind::Err(p) => pattern_names(p, out),
        PatKind::Tuple(ps) => ps.iter().for_each(|p| pattern_names(p, out)),
        // Every alternative binds the same names.
        PatKind::Or(ps) => ps.iter().take(1).for_each(|p| pattern_names(p, out)),
        PatKind::Variant { args, .. } => {
            for p in args.iter().flatten() {
                pattern_names(p, out);
            }
        }
        PatKind::Struct { fields, .. } => fields.iter().for_each(|f| pattern_names(&f.pat, out)),
        PatKind::Bind { pat, name } => {
            pattern_names(pat, out);
            out.push((name.name.clone(), name.span));
        }
        _ => {}
    }
}
