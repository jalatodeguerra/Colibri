//! The methods of `str` beyond `len` and `is_empty`, and string slicing (DESIGN §5.1).
//!
//! A `str` is UTF-8 and positions are byte offsets, so `s[0..4]` is a byte range: it panics
//! when an end is out of range or falls inside a multi-byte character.

use std::rc::Rc;

use colibri_check::hir::Builtin;
use colibri_syntax::Span;

use crate::ctl::Eval;
use crate::eval::Interp;
use crate::fiber::Fiber;
use crate::iter::IterObj;
use crate::value::Value;

impl Interp {
    /// A `str` method; `args[0]` is the receiver.
    pub(crate) fn str_op(
        &mut self,
        fiber: &mut Fiber,
        op: Builtin,
        args: &[Value],
        span: Span,
    ) -> Eval {
        let Some(Value::Str(s)) = args.first() else {
            return Err(self.internal(fiber, span, "a string method on a non-string"));
        };
        let text: &str = s;
        let other = match args.get(1) {
            Some(Value::Str(o)) => Some(Rc::clone(o)),
            _ => None,
        };
        let need = |this: &Self, fiber: &mut Fiber| match &other {
            Some(o) => Ok(Rc::clone(o)),
            None => Err(this.internal(fiber, span, "ill-typed string method")),
        };
        Ok(match op {
            Builtin::StrChars => Value::Iter(IterObj::chars(Rc::clone(s))),
            Builtin::StrBytes => Value::Iter(IterObj::bytes(Rc::clone(s))),
            Builtin::StrContains => Value::Bool(text.contains(&*need(self, fiber)?)),
            Builtin::StrStartsWith => Value::Bool(text.starts_with(&*need(self, fiber)?)),
            Builtin::StrEndsWith => Value::Bool(text.ends_with(&*need(self, fiber)?)),
            Builtin::StrFind => Value::option(
                text.find(&*need(self, fiber)?)
                    .map(|i| Value::Int(i as i64)),
            ),
            Builtin::StrSplit => {
                let sep = need(self, fiber)?;
                if sep.is_empty() {
                    return Err(self.panic(
                        fiber,
                        span,
                        "`split` needs a separator that is not empty",
                    ));
                }
                Value::list(text.split(&*sep).map(Value::str).collect())
            }
            Builtin::StrTrim => Value::str(text.trim()),
            Builtin::StrToUpper => Value::str(&text.to_uppercase()),
            Builtin::StrToLower => Value::str(&text.to_lowercase()),
            Builtin::StrReplace => {
                let (Some(from), Some(Value::Str(to))) = (other.as_ref(), args.get(2)) else {
                    return Err(self.internal(fiber, span, "ill-typed `replace`"));
                };
                if from.is_empty() {
                    return Err(self.panic(
                        fiber,
                        span,
                        "`replace` needs a pattern that is not empty",
                    ));
                }
                Value::str(&text.replace(&**from, to))
            }
            Builtin::StrSlice => {
                let (lo, hi) = self.slice_bounds(fiber, args, text.len(), "string", span)?;
                if !text.is_char_boundary(lo) || !text.is_char_boundary(hi) {
                    let bad = if text.is_char_boundary(lo) { hi } else { lo };
                    return Err(self.panic(
                        fiber,
                        span,
                        format!("byte {bad} is not on a character boundary of the string"),
                    ));
                }
                Value::str(&text[lo..hi])
            }
            _ => return Err(self.internal(fiber, span, "ill-typed built-in call")),
        })
    }

    /// `Range` methods and the iterator constructors; `args[0]` is the receiver.
    pub(crate) fn range_op(
        &mut self,
        fiber: &mut Fiber,
        op: Builtin,
        args: &[Value],
        span: Span,
    ) -> Eval {
        match (op, args.first(), args.get(1)) {
            (Builtin::RangeContains, Some(Value::Range(r)), Some(Value::Int(x))) => {
                Ok(Value::Bool(r.contains(*x)))
            }
            (Builtin::RangeLen, Some(Value::Range(r)), _) => {
                Ok(Value::Int(i64::try_from(r.len()).unwrap_or(i64::MAX)))
            }
            (Builtin::RangeIsEmpty, Some(Value::Range(r)), _) => Ok(Value::Bool(r.is_empty())),
            (Builtin::RangeRev, Some(Value::Range(r)), _) => {
                Ok(Value::Iter(IterObj::over_range(**r, true)))
            }
            (Builtin::Iter, Some(v), _) => match self.iterator_of(v) {
                Some(it) => Ok(it),
                None => Err(self.internal(fiber, span, "`iter` of something not iterable")),
            },
            (Builtin::IterEnumerate, Some(v @ (Value::Iter(_) | Value::Dyn(_))), _) => {
                Ok(Value::Iter(IterObj::enumerate(v.clone())))
            }
            _ => Err(self.internal(fiber, span, "ill-typed built-in call")),
        }
    }
}
