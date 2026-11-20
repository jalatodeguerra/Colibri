//! Matching a value against a HIR pattern (DESIGN §8.2).

use std::cmp::Ordering;

use colibri_check::hir::{Lit, LocalId, Pat, PatKind};

use crate::compare::equal_or_give_up;
use crate::eval::Interp;
use crate::value::Value;

/// The bindings a successful match produced, to be stored in their slots.
pub(crate) type Bindings = Vec<(LocalId, Value)>;

impl Interp {
    /// Whether `value` matches `pat`; on a match `binds` holds the pattern's bindings. On a
    /// mismatch `binds` may hold partial results and must be discarded.
    pub(crate) fn match_pat(&self, pat: &Pat, value: &Value, binds: &mut Bindings) -> bool {
        match (&pat.kind, value) {
            (PatKind::Wild, _) => true,
            (PatKind::Bind(local, inner), v) => {
                if let Some(inner) = inner
                    && !self.match_pat(inner, v, binds)
                {
                    return false;
                }
                binds.push((*local, v.clone()));
                true
            }
            (PatKind::Lit(lit), v) => lit_matches(lit, v),
            (PatKind::Range { lo, hi, inclusive }, v) => {
                let above = lo.as_ref().is_none_or(|lo| {
                    matches!(lit_cmp(v, lo), Some(Ordering::Greater | Ordering::Equal))
                });
                let below = hi.as_ref().is_none_or(|hi| match lit_cmp(v, hi) {
                    Some(Ordering::Less) => true,
                    Some(Ordering::Equal) => *inclusive,
                    _ => false,
                });
                above && below
            }
            (PatKind::Const(g), v) => match self.global(*g) {
                Some(constant) => equal_or_give_up(constant, v),
                None => false,
            },
            (
                PatKind::Variant {
                    variant, args, ty, ..
                },
                Value::Enum(e),
            ) => {
                e.ty == *ty
                    && e.variant == *variant
                    && args.len() == e.payload.len()
                    && args
                        .iter()
                        .zip(e.payload.iter())
                        .all(|(p, v)| self.match_pat(p, v, binds))
            }
            (PatKind::Struct { fields, .. }, Value::Struct(s)) => {
                fields.len() == s.fields.len()
                    && fields
                        .iter()
                        .zip(s.fields.iter())
                        .all(|(p, v)| self.match_pat(p, v, binds))
            }
            (PatKind::Struct { fields, .. }, Value::Obj(o)) => {
                let values = o.snapshot();
                fields.len() == values.len()
                    && fields
                        .iter()
                        .zip(values.iter())
                        .all(|(p, v)| self.match_pat(p, v, binds))
            }
            (PatKind::Tuple(items), Value::Tuple(values)) => {
                items.len() == values.len()
                    && items
                        .iter()
                        .zip(values.iter())
                        .all(|(p, v)| self.match_pat(p, v, binds))
            }
            (PatKind::Tuple(items), Value::Unit) => items.is_empty(),
            (PatKind::Or(alts), v) => {
                let mark = binds.len();
                for alt in alts {
                    if self.match_pat(alt, v, binds) {
                        return true;
                    }
                    binds.truncate(mark);
                }
                false
            }
            _ => false,
        }
    }
}

fn lit_matches(lit: &Lit, v: &Value) -> bool {
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

fn lit_cmp(v: &Value, lit: &Lit) -> Option<Ordering> {
    match (v, lit) {
        (Value::Int(a), Lit::Int(b)) => Some(a.cmp(b)),
        (Value::U64(a), Lit::Int(b)) => Some(i128::from(*a).cmp(&i128::from(*b))),
        (Value::Char(a), Lit::Char(b)) => Some(a.cmp(b)),
        _ => None,
    }
}
