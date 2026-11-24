//! The scalar tests of patterns, shared by the engines (DESIGN §8.2).

use std::cmp::Ordering;

use colibri_check::hir::{Lit, LocalId, Pat, PatKind};

use crate::value::Value;

/// Whether the literal pattern `lit` matches `v`. An integer literal also matches a `u64`.
#[inline]
pub fn lit_matches(lit: &Lit, v: &Value) -> bool {
    match (lit, v) {
        (Lit::Int(a), Value::Int(b)) => a == b,
        (Lit::Int(a), Value::U64(b)) => u64::try_from(*a) == Ok(*b),
        (Lit::Char(a), Value::Char(b)) => a == b,
        (Lit::Bool(a), Value::Bool(b)) => a == b,
        (Lit::Str(a), Value::Str(b)) => a == b,
        (Lit::Unit, Value::Unit) => true,
        _ => false,
    }
}

/// How `v` compares with a range bound, `None` when they are not comparable.
#[inline]
pub fn lit_cmp(v: &Value, lit: &Lit) -> Option<Ordering> {
    match (v, lit) {
        (Value::Int(a), Lit::Int(b)) => Some(a.cmp(b)),
        (Value::U64(a), Lit::Int(b)) => Some(i128::from(*a).cmp(&i128::from(*b))),
        (Value::Char(a), Lit::Char(b)) => Some(a.cmp(b)),
        _ => None,
    }
}

/// Whether `v` lies in the range pattern `lo..hi` (`lo..=hi` when `inclusive`); a missing bound
/// is open.
#[inline]
pub fn range_matches(lo: Option<&Lit>, hi: Option<&Lit>, inclusive: bool, v: &Value) -> bool {
    let above =
        lo.is_none_or(|lo| matches!(lit_cmp(v, lo), Some(Ordering::Greater | Ordering::Equal)));
    let below = hi.is_none_or(|hi| match lit_cmp(v, hi) {
        Some(Ordering::Less) => true,
        Some(Ordering::Equal) => inclusive,
        _ => false,
    });
    above && below
}

/// The locals a pattern binds, in the order its bindings are stored (and so the reverse of the
/// order they die in): the order the names first appear, a binding with a sub-pattern after the
/// bindings of that sub-pattern, an or-pattern as its first alternative. Whichever alternative
/// matched, both engines store and drop the bindings in this order.
pub fn bind_order(p: &Pat, out: &mut Vec<LocalId>) {
    match &p.kind {
        PatKind::Bind(local, inner) => {
            if let Some(inner) = inner {
                bind_order(inner, out);
            }
            out.push(*local);
        }
        PatKind::Variant { args: items, .. }
        | PatKind::Struct { fields: items, .. }
        | PatKind::Tuple(items) => items.iter().for_each(|i| bind_order(i, out)),
        PatKind::Or(alts) => {
            if let Some(first) = alts.first() {
                bind_order(first, out);
            }
        }
        PatKind::Wild | PatKind::Lit(_) | PatKind::Range { .. } | PatKind::Const(_) => {}
    }
}
