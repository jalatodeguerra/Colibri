//! `@deprecated("message")` (DESIGN §3.6): every use of a deprecated function, method, type,
//! alias, constant, module-level value or field outside the item's own code and outside other
//! deprecated items is a warning. The warnings are lints: they never fail a check or a run, and
//! a dependency's code is not linted.

use colibri_syntax::ast;
use colibri_syntax::{Diagnostic, Span};

use crate::checker::Checker;
use crate::hir::{FnId, GlobalId};
use crate::modules::AliasId;
use crate::types::TypeId;

/// What a deprecation is attached to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum DeprKey {
    Fn(FnId),
    Global(GlobalId),
    Type(TypeId),
    Alias(AliasId),
    /// A field, by its position in the type.
    Field(TypeId, u32),
    /// A method of an interface, by its position in the interface: what a call through an
    /// interface value or a type parameter's bound finds.
    IfaceMethod(TypeId, u32),
}

/// The display name and the optional message of a deprecated item.
#[derive(Clone, Debug)]
pub(crate) struct Deprecation {
    pub name: String,
    pub message: String,
}

impl Checker<'_> {
    /// The `@deprecated` attribute among `attrs`: `Some(message)` (empty when none is given)
    /// when the item is deprecated. A malformed attribute is reported.
    pub(crate) fn deprecation_attr(&mut self, attrs: &[ast::Attr]) -> Option<String> {
        let mut found: Option<String> = None;
        for attr in attrs.iter().filter(|a| a.name.name == "deprecated") {
            let message = match attr.args.as_slice() {
                [] => String::new(),
                [arg] => match crate::ffi::plain_string(arg) {
                    Some(m) => m,
                    None => {
                        self.report(
                            Diagnostic::error(
                                "`@deprecated` takes its message as a plain string",
                                attr.span,
                            )
                            .with_hint("write `@deprecated(\"use new_name instead\")`"),
                        );
                        String::new()
                    }
                },
                _ => {
                    self.report(
                        Diagnostic::error("`@deprecated` takes at most one argument", attr.span)
                            .with_hint("write `@deprecated(\"use new_name instead\")`"),
                    );
                    String::new()
                }
            };
            if found.is_some() {
                self.report(Diagnostic::error(
                    "an item has one `@deprecated`",
                    attr.span,
                ));
            }
            found = Some(message);
        }
        found
    }

    /// Records that `key` is deprecated (when `attrs` say so).
    pub(crate) fn declare_deprecation(&mut self, key: DeprKey, name: &str, attrs: &[ast::Attr]) {
        if let Some(message) = self.deprecation_attr(attrs) {
            self.deprecated.insert(
                key,
                Deprecation {
                    name: name.to_string(),
                    message,
                },
            );
        }
    }

    /// Whether the code being checked is part of a deprecated item (or of the item `key`
    /// itself), where uses of deprecated things are not warned about.
    fn in_deprecated_item(&self) -> bool {
        self.depr_ctx
            .iter()
            .any(|k| self.deprecated.contains_key(k))
    }

    /// A use of `key` at `span`: a warning if it is deprecated and the use is somewhere the
    /// program's author can change.
    pub(crate) fn note_deprecated_use(&mut self, key: DeprKey, span: Span) {
        if self.deprecated.is_empty() || self.depr_mute > 0 {
            return;
        }
        let Some(dep) = self.deprecated.get(&key) else {
            return;
        };
        if self.in_deprecated_item() || !self.mods[self.cur_mod.0 as usize].root {
            return;
        }
        let mut diag = Diagnostic::warning(format!("`{}` is deprecated", dep.name), span);
        if !dep.message.is_empty() {
            diag.message = format!("`{}` is deprecated: {}", dep.name, dep.message);
        }
        self.report_lint(diag);
    }

    /// The deprecated declarations by the span of their names, for hover and completion.
    pub(crate) fn deprecated_spans(&self) -> std::collections::HashMap<Span, String> {
        self.deprecated
            .iter()
            .map(|(key, dep)| {
                let span = match *key {
                    DeprKey::Fn(f) => self.fns[f.0 as usize].name_span,
                    DeprKey::Global(g) => self.globals[g.0 as usize].name_span,
                    DeprKey::Type(t) => self.types[t.0 as usize].span,
                    DeprKey::Alias(a) => self.alias_infos[a.0 as usize].name_span,
                    DeprKey::Field(t, i) => self.types[t.0 as usize].fields()[i as usize].span,
                    DeprKey::IfaceMethod(t, i) => self.ifaces[&t].methods[i as usize].name_span,
                };
                (span, dep.message.clone())
            })
            .collect()
    }

    /// Runs `f` as the code of the item `key` (the keys of the items it belongs to, such as the
    /// type of a method, are pushed too).
    pub(crate) fn with_depr_ctx<R>(
        &mut self,
        keys: &[DeprKey],
        f: impl FnOnce(&mut Self) -> R,
    ) -> R {
        let mark = self.depr_ctx.len();
        self.depr_ctx.extend_from_slice(keys);
        let r = f(self);
        self.depr_ctx.truncate(mark);
        r
    }
}
