//! Attribute vocabulary: which attributes exist and the message for one that does not.

use colibri_syntax::Diagnostic;
use colibri_syntax::ast::{Attr, Item, ItemKind};

use crate::checker::Checker;

/// Where an attribute was written.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum AttrSite {
    /// A declaration at module level.
    Item,
    /// A method or interface member inside an `impl` or `interface`.
    Member,
    /// A field of a struct or class.
    Field,
    /// A function declared in an `extern` block.
    ExternFn,
}

impl Checker<'_> {
    /// Reports an attribute that does not exist (or not here), with the ones that do.
    pub(crate) fn unknown_attribute(&mut self, attr: &Attr, site: AttrSite) {
        let name = attr.name.name.as_str();
        let (what, known) = match site {
            AttrSite::Item => (
                "",
                "`@when(cond)` and `@deprecated(\"msg\")` on a declaration, `@repr(C)` on a struct, `@link(\"lib\")` on an `extern` block",
            ),
            AttrSite::Member => (" on a method", "`@deprecated(\"msg\")`"),
            AttrSite::Field => (
                " on a field",
                "`@rename(\"key\")`, `@skip` and `@deprecated(\"msg\")`",
            ),
            AttrSite::ExternFn => (" on an extern function", "`@deprecated(\"msg\")`"),
        };
        if name == "when" && site != AttrSite::Item {
            let hint = match site {
                AttrSite::Field => {
                    "a field cannot be left out of one build: the struct would have a different shape in each; put `@when` on the whole struct, or keep the field and decide its value in code"
                }
                AttrSite::ExternFn => {
                    "put the function in an `extern` block of its own that has the attribute"
                }
                _ => {
                    "put the method in its own `impl` block that has the attribute, or the code that needs the condition in a function that does"
                }
            };
            self.report(
                Diagnostic::error("`@when` applies to top-level items only", attr.span)
                    .with_hint(hint),
            );
            return;
        }
        let mut diag = Diagnostic::error(format!("unknown attribute `@{name}`{what}"), attr.span)
            .with_hint(format!("the attributes are {known}"));
        if name == "inline" {
            diag = diag.with_hint(format!(
                "`@inline` was removed: inlining is the engine's decision, so there is nothing to write; the attributes are {known}"
            ));
        }
        self.report(diag);
    }

    /// Checks the attributes of a method or interface member.
    pub(crate) fn check_member_attrs(&mut self, item: &Item) {
        for attr in &item.attrs {
            match (&item.kind, attr.name.name.as_str()) {
                (ItemKind::Fn(_), "deprecated") => {}
                _ => self.unknown_attribute(attr, AttrSite::Member),
            }
        }
    }
}
