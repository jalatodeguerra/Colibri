//! Matching a value against a HIR pattern (DESIGN §8.2).

use colibri_check::hir::{LocalId, Pat, PatKind};
use colibri_core::pattern::{bind_order, lit_matches, range_matches};

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
                range_matches(lo.as_ref(), hi.as_ref(), *inclusive, v)
            }
            (PatKind::Const(g), v) => match self.global(*g) {
                Some(constant) => {
                    let hit = equal_or_give_up(constant, v);
                    // The comparison gave up: the pattern is where it panics, before anything
                    // is bound on its answer (the caller raises it, see `pat_gave_up`).
                    if crate::compare::take_gave_up() {
                        if self.pat_gave_up.get().is_none() {
                            self.pat_gave_up.set(Some(pat.span));
                        }
                        return false;
                    }
                    hit
                }
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
                for (k, alt) in alts.iter().enumerate() {
                    if self.match_pat(alt, v, binds) {
                        if k > 0 {
                            // The bindings of an or-pattern are stored, and so dropped, in the
                            // order of its first alternative, whichever alternative matched.
                            let mut order = Vec::new();
                            bind_order(&alts[0], &mut order);
                            binds[mark..]
                                .sort_by_key(|(local, _)| order.iter().position(|l| l == local));
                        }
                        return true;
                    }
                    binds.truncate(mark);
                    if self.pat_gave_up.get().is_some() {
                        return false;
                    }
                }
                false
            }
            _ => false,
        }
    }
}
