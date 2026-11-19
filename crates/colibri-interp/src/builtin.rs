//! The methods and constructors of `str`, `List`, `Map` and `Set`, and `clone`.
//!
//! The checker has already typed every call, so an operand of the wrong shape is an internal
//! error, not a user error. Values removed from a collection are released here, after the
//! collection's own borrow has ended (ARCHITECTURE §3).

use std::rc::Rc;

use colibri_check::hir::Builtin;
use colibri_syntax::Span;

use crate::clone::Cloner;
use crate::compare::values_equal;
use crate::ctl::{Ctl, Eval};
use crate::eval::Interp;
use crate::fiber::Fiber;
use crate::heap::{ListError, ListObj, MapObj, OutOfBounds, SetObj, Violation};
use crate::ordered::OrderedMap;
use crate::show::show;
use crate::value::Value;

/// The receiver of a list method.
fn list_of(v: &Value) -> Option<Rc<ListObj>> {
    match v {
        Value::List(l) => Some(Rc::clone(l)),
        _ => None,
    }
}

fn map_of(v: &Value) -> Option<Rc<MapObj>> {
    match v {
        Value::Map(m) => Some(Rc::clone(m)),
        _ => None,
    }
}

fn set_of(v: &Value) -> Option<Rc<SetObj>> {
    match v {
        Value::Set(s) => Some(Rc::clone(s)),
        _ => None,
    }
}

fn int_of(v: &Value) -> Option<i64> {
    match v {
        Value::Int(i) => Some(*i),
        _ => None,
    }
}

/// Moves argument `i` out, leaving `()` behind.
fn take(args: &mut [Value], i: usize) -> Value {
    args.get_mut(i)
        .map_or(Value::Unit, |a| std::mem::replace(a, Value::Unit))
}

impl Interp {
    /// Runs one built-in operation. `args[0]` is the receiver, if the operation has one. The
    /// arguments are references to values the caller still owns on its temporaries, so
    /// releasing what is left of them here only drops a reference.
    pub(crate) fn builtin(
        &mut self,
        fiber: &mut Fiber,
        op: Builtin,
        mut args: Vec<Value>,
        span: Span,
    ) -> Eval {
        let result = self.builtin_inner(fiber, op, &mut args, span);
        self.release_all(args);
        result
    }

    fn builtin_inner(
        &mut self,
        fiber: &mut Fiber,
        op: Builtin,
        args: &mut [Value],
        span: Span,
    ) -> Eval {
        let bad =
            |this: &Self, fiber: &mut Fiber| this.internal(fiber, span, "ill-typed built-in call");
        let recv = args.first().cloned().unwrap_or(Value::Unit);
        Ok(match op {
            Builtin::Clone => return self.deep_clone(fiber, &recv, span),
            Builtin::StrLen => match &recv {
                Value::Str(s) => Value::Int(s.len() as i64),
                _ => return Err(bad(self, fiber)),
            },
            Builtin::StrIsEmpty => match &recv {
                Value::Str(s) => Value::Bool(s.is_empty()),
                _ => return Err(bad(self, fiber)),
            },
            Builtin::SetNew => Value::Set(SetObj::new(OrderedMap::new())),
            Builtin::SetFromList => {
                let Some(list) = list_of(&recv) else {
                    return Err(bad(self, fiber));
                };
                let set = SetObj::new(OrderedMap::new());
                for item in list.snapshot() {
                    if let Ok((_, Some(spare))) = set.add(item) {
                        self.release(spare);
                    }
                }
                Value::Set(set)
            }
            Builtin::ListLen
            | Builtin::ListIsEmpty
            | Builtin::ListPush
            | Builtin::ListPop
            | Builtin::ListInsert
            | Builtin::ListRemove
            | Builtin::ListClear
            | Builtin::ListContains
            | Builtin::ListIndexOf
            | Builtin::ListFirst
            | Builtin::ListLast
            | Builtin::ListReverse
            | Builtin::ListExtend
            | Builtin::ListGet => {
                let Some(list) = list_of(&recv) else {
                    return Err(bad(self, fiber));
                };
                return self.list_op(fiber, op, &list, args, span);
            }
            Builtin::MapLen
            | Builtin::MapIsEmpty
            | Builtin::MapGet
            | Builtin::MapContainsKey
            | Builtin::MapInsert
            | Builtin::MapRemove
            | Builtin::MapKeys
            | Builtin::MapValues
            | Builtin::MapClear => {
                let Some(map) = map_of(&recv) else {
                    return Err(bad(self, fiber));
                };
                return self.map_op(fiber, op, &map, args, span);
            }
            Builtin::SetLen
            | Builtin::SetIsEmpty
            | Builtin::SetAdd
            | Builtin::SetRemove
            | Builtin::SetContains
            | Builtin::SetClear => {
                let Some(set) = set_of(&recv) else {
                    return Err(bad(self, fiber));
                };
                return self.set_op(fiber, op, &set, args, span);
            }
        })
    }

    /// `x.clone()`: a deep copy that keeps shared substructure shared and cycles cyclic.
    fn deep_clone(&mut self, fiber: &mut Fiber, v: &Value, span: Span) -> Eval {
        let mut cloner = Cloner::new(fiber.stack_base, self.native_stack_budget());
        match cloner.clone_value(v) {
            Ok(copy) => Ok(copy),
            Err(_) => {
                self.abandon_copies(cloner.into_copies());
                Err(self.panic(fiber, span, "this value nests too deeply to clone"))
            }
        }
    }

    /// Takes a half-made copy apart without tracing its objects as destroyed.
    fn abandon_copies(&mut self, copies: Vec<Value>) {
        let mut pending = Vec::new();
        for copy in &copies {
            match copy {
                Value::Obj(o) => pending.extend(o.take_fields()),
                Value::List(l) => pending.extend(l.take_all()),
                Value::Map(m) => pending.extend(m.take_all().into_iter().flat_map(|(k, v)| [k, v])),
                Value::Set(s) => pending.extend(s.take_all()),
                _ => {}
            }
        }
        drop(copies);
        self.release_all(pending);
    }

    fn violation(&self, fiber: &mut Fiber, span: Span, v: Violation) -> Ctl {
        self.panic(fiber, span, v.0)
    }

    pub(crate) fn bounds_panic(
        &self,
        fiber: &mut Fiber,
        span: Span,
        what: &str,
        e: OutOfBounds,
    ) -> Ctl {
        self.panic(
            fiber,
            span,
            format!(
                "{what} {} out of bounds for list of length {}",
                e.index, e.len
            ),
        )
    }

    pub(crate) fn list_error(
        &self,
        fiber: &mut Fiber,
        span: Span,
        what: &str,
        e: ListError,
    ) -> Ctl {
        match e {
            ListError::Guard(v) => self.violation(fiber, span, v),
            ListError::Bounds(b) => self.bounds_panic(fiber, span, what, b),
        }
    }

    fn list_op(
        &mut self,
        fiber: &mut Fiber,
        op: Builtin,
        list: &Rc<ListObj>,
        args: &mut [Value],
        span: Span,
    ) -> Eval {
        let index = |args: &[Value]| args.get(1).and_then(int_of).unwrap_or(0);
        Ok(match op {
            Builtin::ListLen => Value::Int(list.len() as i64),
            Builtin::ListIsEmpty => Value::Bool(list.is_empty()),
            Builtin::ListPush => match list.push(take(args, 1)) {
                Ok(()) => Value::Unit,
                Err(rejected) => {
                    self.release(rejected.args);
                    return Err(self.violation(fiber, span, rejected.violation));
                }
            },
            Builtin::ListPop => match list.pop() {
                Ok(v) => Value::option(v),
                Err(v) => return Err(self.violation(fiber, span, v)),
            },
            Builtin::ListInsert => match list.insert(index(args), take(args, 2)) {
                Ok(()) => Value::Unit,
                Err((e, item)) => {
                    self.release(item);
                    return Err(self.list_error(fiber, span, "insert index", e));
                }
            },
            Builtin::ListRemove => match list.remove(index(args)) {
                Ok(v) => v,
                Err(e) => return Err(self.list_error(fiber, span, "index", e)),
            },
            Builtin::ListClear => match list.clear() {
                Ok(items) => {
                    self.release_all(items);
                    Value::Unit
                }
                Err(v) => return Err(self.violation(fiber, span, v)),
            },
            Builtin::ListContains => {
                let item = take(args, 1);
                let found =
                    list.with(|xs| xs.iter().any(|x| values_equal(x, &item).unwrap_or(false)));
                self.release(item);
                Value::Bool(found)
            }
            Builtin::ListIndexOf => {
                let item = take(args, 1);
                let at = list.with(|xs| {
                    xs.iter()
                        .position(|x| values_equal(x, &item).unwrap_or(false))
                });
                self.release(item);
                Value::option(at.map(|i| Value::Int(i as i64)))
            }
            Builtin::ListFirst => Value::option(list.first()),
            Builtin::ListLast => Value::option(list.last()),
            Builtin::ListReverse => match list.reverse() {
                Ok(()) => Value::Unit,
                Err(v) => return Err(self.violation(fiber, span, v)),
            },
            Builtin::ListExtend => {
                let more = match args.get(1).and_then(list_of) {
                    Some(other) => other.snapshot(),
                    None => return Err(self.internal(fiber, span, "ill-typed built-in call")),
                };
                match list.extend(more) {
                    Ok(()) => Value::Unit,
                    Err(rejected) => {
                        self.release_all(rejected.args);
                        return Err(self.violation(fiber, span, rejected.violation));
                    }
                }
            }
            Builtin::ListGet => Value::option(list.get(index(args)).ok()),
            _ => return Err(self.internal(fiber, span, "ill-typed built-in call")),
        })
    }

    fn map_op(
        &mut self,
        fiber: &mut Fiber,
        op: Builtin,
        map: &Rc<MapObj>,
        args: &mut [Value],
        span: Span,
    ) -> Eval {
        Ok(match op {
            Builtin::MapLen => Value::Int(map.len() as i64),
            Builtin::MapIsEmpty => Value::Bool(map.is_empty()),
            Builtin::MapGet => Value::option(args.get(1).and_then(|k| map.get(k))),
            Builtin::MapContainsKey => {
                Value::Bool(args.get(1).is_some_and(|k| map.contains_key(k)))
            }
            Builtin::MapInsert => match map.insert(take(args, 1), take(args, 2)) {
                Ok(replaced) => {
                    if let Some(old) = replaced.old {
                        self.release(old);
                    }
                    if let Some(spare) = replaced.spare_key {
                        self.release(spare);
                    }
                    Value::Unit
                }
                Err(rejected) => {
                    let (k, v) = rejected.args;
                    self.release(k);
                    self.release(v);
                    return Err(self.violation(fiber, span, rejected.violation));
                }
            },
            Builtin::MapRemove => {
                let Some(key) = args.get(1) else {
                    return Err(self.internal(fiber, span, "ill-typed built-in call"));
                };
                match map.remove(key) {
                    Ok(Some((k, v))) => {
                        self.release(k);
                        Value::option(Some(v))
                    }
                    Ok(None) => Value::option(None),
                    Err(v) => return Err(self.violation(fiber, span, v)),
                }
            }
            Builtin::MapKeys => Value::list(map.keys()),
            Builtin::MapValues => Value::list(map.values()),
            Builtin::MapClear => match map.clear() {
                Ok(entries) => {
                    for (k, v) in entries {
                        self.release(k);
                        self.release(v);
                    }
                    Value::Unit
                }
                Err(v) => return Err(self.violation(fiber, span, v)),
            },
            _ => return Err(self.internal(fiber, span, "ill-typed built-in call")),
        })
    }

    fn set_op(
        &mut self,
        fiber: &mut Fiber,
        op: Builtin,
        set: &Rc<SetObj>,
        args: &mut [Value],
        span: Span,
    ) -> Eval {
        Ok(match op {
            Builtin::SetLen => Value::Int(set.len() as i64),
            Builtin::SetIsEmpty => Value::Bool(set.is_empty()),
            Builtin::SetContains => Value::Bool(args.get(1).is_some_and(|x| set.contains(x))),
            Builtin::SetAdd => match set.add(take(args, 1)) {
                Ok((_, spare)) => {
                    if let Some(spare) = spare {
                        self.release(spare);
                    }
                    Value::Unit
                }
                Err(rejected) => {
                    self.release(rejected.args);
                    return Err(self.violation(fiber, span, rejected.violation));
                }
            },
            Builtin::SetRemove => {
                let Some(item) = args.get(1) else {
                    return Err(self.internal(fiber, span, "ill-typed built-in call"));
                };
                match set.remove(item) {
                    Ok(Some(old)) => {
                        self.release(old);
                        Value::Bool(true)
                    }
                    Ok(None) => Value::Bool(false),
                    Err(v) => return Err(self.violation(fiber, span, v)),
                }
            }
            Builtin::SetClear => match set.clear() {
                Ok(items) => {
                    self.release_all(items);
                    Value::Unit
                }
                Err(v) => return Err(self.violation(fiber, span, v)),
            },
            _ => return Err(self.internal(fiber, span, "ill-typed built-in call")),
        })
    }

    /// The `Show` text of a value, for panic messages.
    pub(crate) fn shown(&self, v: &Value) -> String {
        show(&self.types, v).into_owned()
    }
}
