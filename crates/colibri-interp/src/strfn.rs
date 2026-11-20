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
use crate::heap::ClassObj;
use crate::iter::IterObj;
use crate::value::Value;

pub(crate) use colibri_core::builders::builder_text;

/// The most integers `range.to_list()` builds.
pub(crate) const MAX_RANGE_LIST: u64 = 1 << 27;

impl Interp {
    /// A `StrBuilder` method; `args[0]` is the builder.
    pub(crate) fn str_builder_op(
        &mut self,
        fiber: &mut Fiber,
        op: Builtin,
        args: &[Value],
        span: Span,
    ) -> Eval {
        let Some(Value::Obj(obj)) = args.first() else {
            return Err(self.internal(fiber, span, "a string builder method on a non-builder"));
        };
        let Some(Value::List(chunks)) = obj.field(0) else {
            return Err(self.internal(fiber, span, "a malformed string builder"));
        };
        let total = |obj: &ClassObj| match obj.field(1) {
            Some(Value::Int(n)) => n,
            _ => 0,
        };
        let text = match (op, args.get(1)) {
            (Builtin::StrBuilderPush, Some(Value::Str(s))) => Some(Rc::clone(s)),
            (Builtin::StrBuilderPushChar, Some(Value::Char(c))) => {
                Some(Rc::from(c.encode_utf8(&mut [0; 4]) as &str))
            }
            _ => None,
        };
        if let Some(piece) = text {
            if !piece.is_empty() {
                let n = total(obj) + piece.len() as i64;
                if chunks.push(Value::Str(piece)).is_err() {
                    return Err(self.internal(fiber, span, "a string builder was held"));
                }
                let _ = obj.set_field(1, Value::Int(n));
            }
            return Ok(Value::Unit);
        }
        Ok(match op {
            Builtin::StrBuilderLen => Value::Int(total(obj)),
            Builtin::StrBuilderIsEmpty => Value::Bool(total(obj) == 0),
            Builtin::StrBuilderClear => {
                let old = chunks.replace_all(Vec::new());
                self.release_all(old);
                let _ = obj.set_field(1, Value::Int(0));
                Value::Unit
            }
            Builtin::StrBuilderToStr => {
                if chunks.len() == 1
                    && let Some(only @ Value::Str(_)) = chunks.at(0)
                {
                    return Ok(only);
                }
                // Join once and keep the joined text as the only chunk, so asking again (or
                // appending a little more) does not walk the pieces a second time.
                let joined: Rc<str> = Rc::from(builder_text(obj));
                if !joined.is_empty() {
                    let old = chunks.replace_all(vec![Value::Str(Rc::clone(&joined))]);
                    self.release_all(old);
                }
                Value::Str(joined)
            }
            _ => return Err(self.internal(fiber, span, "ill-typed built-in call")),
        })
    }

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
            Builtin::StrRFind => Value::option(
                text.rfind(&*need(self, fiber)?)
                    .map(|i| Value::Int(i as i64)),
            ),
            Builtin::StrSplitOnce | Builtin::StrRSplitOnce => {
                let sep = need(self, fiber)?;
                if sep.is_empty() {
                    return Err(self.panic(
                        fiber,
                        span,
                        "`split_once` needs a separator that is not empty",
                    ));
                }
                let found = if op == Builtin::StrSplitOnce {
                    text.split_once(&*sep)
                } else {
                    text.rsplit_once(&*sep)
                };
                Value::option(found.map(|(a, b)| Value::tuple(vec![Value::str(a), Value::str(b)])))
            }
            Builtin::StrPadLeft | Builtin::StrPadRight => {
                let Some(Value::Int(width)) = args.get(1) else {
                    return Err(self.internal(fiber, span, "ill-typed `pad_left`"));
                };
                let fill = match args.get(2) {
                    Some(Value::Char(c)) => *c,
                    _ => ' ',
                };
                let Ok(width) = usize::try_from(*width) else {
                    return Err(self.panic(
                        fiber,
                        span,
                        format!("the width {width} is not allowed"),
                    ));
                };
                let len = text.chars().count();
                if width > (1 << 30) {
                    return Err(self.panic(fiber, span, "the width is more than a gigabyte"));
                }
                let pad: String = std::iter::repeat_n(fill, width.saturating_sub(len)).collect();
                Value::str(&if op == Builtin::StrPadLeft {
                    format!("{pad}{text}")
                } else {
                    format!("{text}{pad}")
                })
            }
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
            (Builtin::RangeToList, Some(Value::Range(r)), _) => {
                if r.len() > MAX_RANGE_LIST {
                    // `len` saturates at `u64::MAX`: only the full inclusive range gets there.
                    let n = u128::from(r.len()) + u128::from(r.len() == u64::MAX);
                    return Err(self.panic(
                        fiber,
                        span,
                        format!("a range of {n} integers is too long to turn into a list"),
                    ));
                }
                let items = if r.inclusive {
                    (r.start..=r.end).map(Value::Int).collect()
                } else {
                    (r.start..r.end).map(Value::Int).collect()
                };
                Ok(Value::list(items))
            }
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
