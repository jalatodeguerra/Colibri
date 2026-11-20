//! `Option`, `Result` and `Error` (DESIGN §10): their methods and the `Error` value.
//!
//! An `Error` is a class object of the prelude type [`ERROR_TYPE`] with four fields:
//!
//! | field | root error | error made by `context` |
//! |-------|-----------|--------------------------|
//! | 0 value | the original typed value (kept as it was, so `as[T]` returns it intact) | `()` |
//! | 1 impl | the index of its `IntoError` implementation, or `-1` | `-1` |
//! | 2 context | `()` | the context message |
//! | 3 cause | `()` | the wrapped `Error` |
//!
//! So an error is a chain from the outermost `context` down to the root, which holds the
//! value. `{e}` shows the messages joined by `: `, `message()` the outermost one, `cause()`
//! the next link, and `is`/`as` look at the value of the root. Objects die through
//! `Interp::release` like every class object, which releases the kept value.

use std::rc::Rc;

use colibri_check::hir::{Builtin, ErrorConv, Intrinsic};
use colibri_check::types::{
    ERROR_TYPE, INTO_ERROR, RESULT_TYPE, VARIANT_ERR, VARIANT_OK, VARIANT_SOME,
};
use colibri_syntax::Span;

use crate::ctl::Eval;
use crate::eval::Interp;
use crate::fiber::Fiber;
use crate::heap::ClassObj;
use crate::value::Value;

const VALUE: usize = 0;
const IMPL: usize = 1;
const CONTEXT: usize = 2;
const CAUSE: usize = 3;

/// The `Error` object a value is, if it is one.
pub(crate) fn error_obj(v: &Value) -> Option<&Rc<ClassObj>> {
    match v {
        Value::Obj(o) if o.ty() == ERROR_TYPE => Some(o),
        _ => None,
    }
}

/// What one link of an error chain contributes.
pub(crate) enum Link {
    /// A message added by `context`, and the error it wraps.
    Context(Rc<str>, Rc<ClassObj>),
    /// The root: the kept value, the index of its `IntoError` implementation (`-1`: none) and
    /// the message the host gave it, if any (a host error keeps a typed value, an enum, and
    /// says what happened in words: the message wins over the `Show` of the value).
    Root(Value, i64, Option<Rc<str>>),
}

/// Reads one link of an error object.
pub(crate) fn link(obj: &ClassObj) -> Link {
    let field = |i| obj.field(i).unwrap_or(Value::Unit);
    match (field(CONTEXT), field(CAUSE)) {
        (Value::Str(message), Value::Obj(cause)) => Link::Context(message, cause),
        (Value::Str(message), Value::Unit) => Link::Root(field(VALUE), -1, Some(message)),
        _ => {
            let imp = match field(IMPL) {
                Value::Int(i) => i,
                _ => -1,
            };
            Link::Root(field(VALUE), imp, None)
        }
    }
}

/// The value the root of the chain keeps.
fn root_value(obj: &Rc<ClassObj>) -> Value {
    let mut cur = Rc::clone(obj);
    loop {
        match link(&cur) {
            Link::Context(_, cause) => cur = cause,
            Link::Root(value, ..) => return value,
        }
    }
}

fn is_variant(v: &Value, variant: u32) -> Option<&Value> {
    match v {
        Value::Enum(e) if e.variant == variant => e.payload.first(),
        _ => None,
    }
}

/// The payload of `some(x)` or `ok(x)`.
fn payload_of(v: &Value, variant: u32) -> Option<Value> {
    is_variant(v, variant).cloned()
}

fn ok_value(v: Value) -> Value {
    Value::variant(RESULT_TYPE, VARIANT_OK, vec![v])
}

fn err_value(v: Value) -> Value {
    Value::variant(RESULT_TYPE, VARIANT_ERR, vec![v])
}

impl Interp {
    // ---- making errors ----

    /// A new root error that keeps `value`, converted through the implementation `impl_id`.
    pub(crate) fn new_error(value: Value, impl_id: i64) -> Value {
        Value::Obj(ClassObj::new(
            ERROR_TYPE,
            vec![value, Value::Int(impl_id), Value::Unit, Value::Unit],
        ))
    }

    /// A new root error that keeps the typed `value` (so `e.is[T]()` and `e.as[T]()` see it)
    /// and shows as `message`.
    pub(crate) fn new_typed_error(value: Value, message: &str) -> Value {
        Value::Obj(ClassObj::new(
            ERROR_TYPE,
            vec![value, Value::Int(-1), Value::str(message), Value::Unit],
        ))
    }

    /// `context`: a new error with the message `message` around `cause`.
    fn wrap_error(cause: Value, message: Rc<str>) -> Value {
        Value::Obj(ClassObj::new(
            ERROR_TYPE,
            vec![Value::Unit, Value::Int(-1), Value::Str(message), cause],
        ))
    }

    /// A value converted to an `Error` (an `Error` stays as it is).
    pub(crate) fn convert_to_error(&mut self, value: Value, conv: ErrorConv) -> Value {
        if error_obj(&value).is_some() {
            return value;
        }
        let impl_id = match conv {
            ErrorConv::Impl(id) => i64::from(id.0),
            ErrorConv::Bound => self.impl_for(&value, INTO_ERROR).map_or(-1, i64::from),
            // No implementation: the message is the `Show` of the variant.
            ErrorConv::OpenEnum => -1,
        };
        Self::new_error(value, impl_id)
    }

    /// `Result[T, E]` to `Result[T, Error]`.
    pub(crate) fn result_into_error(&mut self, value: Value, conv: ErrorConv) -> Value {
        match payload_of(&value, VARIANT_ERR) {
            Some(e) => err_value(self.convert_to_error(e, conv)),
            None => value,
        }
    }

    /// The user-written `message` method of an `IntoError` implementation, if it has one.
    pub(crate) fn error_message_fn(&self, impl_id: i64) -> Option<colibri_check::hir::FnId> {
        let imp = self.dispatch.impls.get(usize::try_from(impl_id).ok()?)?;
        if imp.derived {
            return None;
        }
        imp.methods.first().copied().flatten()
    }

    /// The message of the root of an error: its `IntoError` `message`, or the `Show` of the
    /// value.
    fn root_message(
        &mut self,
        fiber: &mut Fiber,
        value: &Value,
        impl_id: i64,
        span: Span,
    ) -> Eval<String> {
        if let Some(f) = self.error_message_fn(impl_id) {
            let Some(body) = self.fns.get(f.0 as usize).cloned() else {
                return Err(self.internal(fiber, span, "unknown function"));
            };
            return match self.call_body(fiber, body, vec![value.clone()])? {
                Value::Str(s) => Ok(s.to_string()),
                other => {
                    self.release(other);
                    Ok(String::new())
                }
            };
        }
        self.show_deep(fiber, value, span)
    }

    /// The message of the outermost link (`e.message()`).
    fn outer_message(&mut self, fiber: &mut Fiber, e: &Rc<ClassObj>, span: Span) -> Eval<String> {
        match link(e) {
            Link::Context(message, _) => Ok(message.to_string()),
            Link::Root(_, _, Some(message)) => Ok(message.to_string()),
            Link::Root(value, impl_id, None) => self.root_message(fiber, &value, impl_id, span),
        }
    }

    // ---- Option, Result and Error methods ----

    /// The methods of `Option`, `Result` and `Error`; `args[0]` is the receiver.
    pub(crate) fn optres_op(
        &mut self,
        fiber: &mut Fiber,
        op: Builtin,
        args: &mut [Value],
        span: Span,
    ) -> Eval {
        let arg = |args: &[Value], i: usize| args.get(i).cloned().unwrap_or(Value::Unit);
        let recv = arg(args, 0);
        let bad =
            |this: &Self, fiber: &mut Fiber| this.internal(fiber, span, "ill-typed built-in call");
        Ok(match op {
            Builtin::OptionIsSome => Value::Bool(is_variant(&recv, VARIANT_SOME).is_some()),
            Builtin::OptionIsNone => Value::Bool(is_variant(&recv, VARIANT_SOME).is_none()),
            Builtin::ResultIsOk => Value::Bool(is_variant(&recv, VARIANT_OK).is_some()),
            Builtin::ResultIsErr => Value::Bool(is_variant(&recv, VARIANT_ERR).is_some()),
            Builtin::ResultOk => Value::option(payload_of(&recv, VARIANT_OK)),
            Builtin::ResultErr => Value::option(payload_of(&recv, VARIANT_ERR)),
            Builtin::OptionExpect => match payload_of(&recv, VARIANT_SOME) {
                Some(v) => v,
                None => {
                    let msg = self.text_arg(args, 1);
                    return Err(self.panic(fiber, span, msg));
                }
            },
            Builtin::ResultExpect => match (
                payload_of(&recv, VARIANT_OK),
                payload_of(&recv, VARIANT_ERR),
            ) {
                (Some(v), _) => v,
                (None, Some(e)) => {
                    let msg = self.text_arg(args, 1);
                    let why = self.show_deep(fiber, &e, span)?;
                    return Err(self.panic(fiber, span, format!("{msg}: {why}")));
                }
                (None, None) => return Err(bad(self, fiber)),
            },
            Builtin::OptionMap => match payload_of(&recv, VARIANT_SOME) {
                Some(v) => {
                    let mapped = self.call_value(fiber, &arg(args, 1), vec![v], span)?;
                    Value::option(Some(mapped))
                }
                None => recv,
            },
            Builtin::OptionAndThen => match payload_of(&recv, VARIANT_SOME) {
                Some(v) => self.call_value(fiber, &arg(args, 1), vec![v], span)?,
                None => recv,
            },
            Builtin::ResultMap => match payload_of(&recv, VARIANT_OK) {
                Some(v) => {
                    let mapped = self.call_value(fiber, &arg(args, 1), vec![v], span)?;
                    ok_value(mapped)
                }
                None => recv,
            },
            Builtin::ResultAndThen => match payload_of(&recv, VARIANT_OK) {
                Some(v) => self.call_value(fiber, &arg(args, 1), vec![v], span)?,
                None => recv,
            },
            Builtin::ResultContext => match payload_of(&recv, VARIANT_ERR) {
                Some(e) => err_value(Self::wrap_error(e, self.message_arg(args, 1))),
                None => recv,
            },
            Builtin::ErrorMessage => {
                let Some(e) = error_obj(&recv) else {
                    return Err(bad(self, fiber));
                };
                let text = self.outer_message(fiber, e, span)?;
                Value::Str(Rc::from(text))
            }
            Builtin::ErrorContext => {
                if error_obj(&recv).is_none() {
                    return Err(bad(self, fiber));
                }
                Self::wrap_error(recv, self.message_arg(args, 1))
            }
            Builtin::ErrorCause => match error_obj(&recv).map(|e| link(e)) {
                Some(Link::Context(_, cause)) => Value::option(Some(Value::Obj(cause))),
                Some(Link::Root(..)) => Value::option(None),
                None => return Err(bad(self, fiber)),
            },
            _ => return Err(bad(self, fiber)),
        })
    }

    /// Calls the function value `f` with `args` (which the callee owns).
    fn call_value(&mut self, fiber: &mut Fiber, f: &Value, args: Vec<Value>, span: Span) -> Eval {
        if let Some(frame) = fiber.frames.last_mut() {
            frame.span = span;
        }
        self.call_function_value(fiber, f, args, span)
    }

    fn message_arg(&self, args: &[Value], i: usize) -> Rc<str> {
        match args.get(i) {
            Some(Value::Str(s)) => Rc::clone(s),
            _ => Rc::from(""),
        }
    }

    fn text_arg(&self, args: &[Value], i: usize) -> String {
        self.message_arg(args, i).to_string()
    }

    // ---- is, as and panic ----

    /// `e.is[T]()`, `e.as[T]()` and `panic(msg)`.
    pub(crate) fn error_intrinsic(
        &mut self,
        fiber: &mut Fiber,
        intrinsic: &Intrinsic,
        arg: &Value,
        span: Span,
    ) -> Eval {
        match intrinsic {
            Intrinsic::Panic => {
                let msg = match arg {
                    Value::Str(s) => s.to_string(),
                    other => crate::show::show(&self.types, other).into_owned(),
                };
                Err(self.panic(fiber, span, msg))
            }
            Intrinsic::ErrorIs(key) | Intrinsic::ErrorAs(key) => {
                let Some(e) = error_obj(arg) else {
                    return Err(self.internal(fiber, span, "`is` or `as` on a non-error"));
                };
                let value = root_value(e);
                let matches = crate::dispatch::runtime_key(&value) == Some(*key);
                Ok(match intrinsic {
                    Intrinsic::ErrorIs(_) => Value::Bool(matches),
                    _ => Value::option(matches.then_some(value)),
                })
            }
            _ => Err(self.internal(fiber, span, "not an error intrinsic")),
        }
    }

    /// `assert(cond, msg)` and `assert_eq(a, b)`: a panic at the call when the assertion fails
    /// (ARCHITECTURE §15). Equality honours a user-written `impl Eq`, the shown values a
    /// user-written `impl Show`.
    pub(crate) fn assertion(
        &mut self,
        fiber: &mut Fiber,
        intrinsic: &Intrinsic,
        args: Vec<Value>,
        span: Span,
    ) -> Eval {
        let failure = match (intrinsic, args.as_slice()) {
            (Intrinsic::Assert, [Value::Bool(true), _]) => Ok(None),
            (Intrinsic::Assert, [Value::Bool(false), Value::Str(msg)]) => Ok(Some(msg.to_string())),
            (Intrinsic::AssertEq, [a, b]) => match self.eq_deep(fiber, a, b, span) {
                Ok(true) => Ok(None),
                Ok(false) => self.show_both(fiber, a, b, span).map(Some),
                Err(ctl) => Err(ctl),
            },
            _ => Err(self.internal(fiber, span, "ill-typed assertion")),
        };
        self.release_all(args);
        match failure {
            Ok(None) => Ok(Value::Unit),
            Ok(Some(msg)) => Err(self.panic(fiber, span, msg)),
            Err(ctl) => Err(ctl),
        }
    }

    /// `compare(a, b)`: -1, 0 or 1, honouring a user-written `impl Ord`. A `nan` panics.
    pub(crate) fn compare_values(
        &mut self,
        fiber: &mut Fiber,
        args: Vec<Value>,
        span: Span,
    ) -> Eval {
        let result = match args.as_slice() {
            [a, b] => self.cmp_deep(fiber, a, b, span).and_then(|o| match o {
                Some(o) => Ok(Value::Int(o as i64)),
                None => Err(self.not_ordered(fiber, span)),
            }),
            _ => Err(self.internal(fiber, span, "ill-typed `compare`")),
        };
        self.release_all(args);
        result
    }

    /// The message of a failed `assert_eq`: both values, strings quoted so that spaces show.
    fn show_both(&mut self, fiber: &mut Fiber, a: &Value, b: &Value, span: Span) -> Eval<String> {
        let mut shown = Vec::new();
        for v in [a, b] {
            shown.push(match v {
                Value::Str(s) => format!("{:?}", s.as_ref()),
                other => self.show_deep(fiber, other, span)?,
            });
        }
        Ok(format!(
            "assertion failed: `left == right`\n  left: {}\n right: {}",
            shown[0], shown[1]
        ))
    }

    /// What `main` returning `err(e)` reports: the whole chain of `e`.
    pub fn error_report(&mut self, fiber: &mut Fiber, v: &Value) -> String {
        let span = Span::new(colibri_syntax::FileId(0), 0, 0);
        self.show_deep(fiber, v, span)
            .unwrap_or_else(|_| "an error whose message could not be shown".to_string())
    }
}
