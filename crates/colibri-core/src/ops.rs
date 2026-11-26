//! The operators on values (DESIGN §5): unary and binary arithmetic, bitwise, concatenation,
//! identity and comparison, and the value of a literal.
//!
//! They live here, generic over the engine, because a fault in them is a panic that an engine
//! builds (with its own stack trace), and a binary operator releases its operands through the
//! engine. The checks of integer width and the arithmetic itself are `crate::num`.

use std::rc::Rc;

use colibri_check::hir::{ArithOp, BinOp, CmpOp, Coercion, Lit, Num, Operand, TypeKey, UnOp};
use colibri_check::types::{ADD, DIV, IntTy, MUL, NEG, SUB, TypeId};
use colibri_syntax::Span;

use crate::compare::values_identical;
use crate::ctl::Eval;
use crate::engine::Engine;
use crate::num;
use crate::prelude::*;
use crate::value::{DynVal, Value};

/// A fault in integer arithmetic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    Overflow,
    DivByZero,
    ModByZero,
}

/// The operators, as methods of an engine (`self.binary(..)`).
#[doc(hidden)]
pub trait Operators: Engine {
    /// Applies a unary operator.
    fn unary(&mut self, fiber: &mut Self::Fiber, op: UnOp, v: Value, span: Span) -> Eval {
        match (op, v) {
            (UnOp::Neg(Num::Int(IntTy::I64)), Value::Int(i)) => match i.checked_neg() {
                Some(n) => Ok(Value::Int(n)),
                None => Err(self.panic(fiber, span, "integer overflow")),
            },
            (UnOp::Neg(Num::Int(ty)), v) => match num::int_of(&v).map(|a| num::int_neg(ty, a)) {
                Some(Ok(n)) => Ok(num::make_int(ty, n)),
                Some(Err(msg)) => Err(self.panic(fiber, span, msg)),
                None => Err(self.internal(fiber, span, "ill-typed negation")),
            },
            (UnOp::Neg(Num::Float), Value::Float(x)) => Ok(Value::Float(-x)),
            (UnOp::Neg(Num::F32), Value::F32(x)) => Ok(Value::F32(-x)),
            (UnOp::BitNot(ty), v) => match num::int_of(&v) {
                Some(a) => Ok(num::make_int(ty, num::int_not(ty, a))),
                None => Err(self.internal(fiber, span, "ill-typed bitwise not")),
            },
            (UnOp::Not, Value::Bool(b)) => Ok(Value::Bool(!b)),
            _ => Err(self.internal(fiber, span, "ill-typed unary operation")),
        }
    }
    /// Applies a binary operator and releases both operands.
    fn binary(
        &mut self,
        fiber: &mut Self::Fiber,
        op: BinOp,
        l: Value,
        r: Value,
        span: Span,
    ) -> Eval {
        let result = binary_of(self, fiber, op, &l, &r, span);
        self.release(l);
        self.release(r);
        result
    }

    // ---- interface values and operators reached through a bound ----

    /// A value converted to an interface value.
    fn coerce(&mut self, value: Value, how: &Coercion) -> Value {
        let impl_id = match how {
            Coercion::IntoError(conv) => return self.convert_to_error(value, *conv),
            Coercion::ResultIntoError(conv) => return self.result_into_error(value, *conv),
            Coercion::F32ToFloat => {
                return match value {
                    Value::F32(x) => Value::Float(f64::from(x)),
                    other => other,
                };
            }
            Coercion::ToDyn(id) => id.0,
            Coercion::ToDynBound(iface) => {
                if let Value::Dyn(d) = &value
                    && self
                        .core()
                        .dispatch
                        .impl_def(d.impl_id)
                        .is_some_and(|i| i.iface == *iface)
                {
                    return value;
                }
                match self.impl_for(&value, *iface) {
                    Some(i) => i,
                    None => return value,
                }
            }
        };
        Value::Dyn(Rc::new(DynVal {
            impl_id,
            val: value,
        }))
    }

    /// `a + b`, `-a`, ... where the left operand has the type of a bounded type parameter: the
    /// impl is found from the runtime types, `rhs` being the right-hand head when the bound
    /// fixes it. Numbers have no impl; they run the built-in operation, with the width of the
    /// value (`int` and `u64`: the narrow integers cannot satisfy the bound).
    fn call_operator(
        &mut self,
        fiber: &mut Self::Fiber,
        iface: TypeId,
        rhs: Option<TypeKey>,
        values: Vec<Value>,
        span: Span,
    ) -> Eval {
        if let Some(f) = self.operator_target(iface, rhs, &values) {
            Self::note_position(fiber, span);
            return self.call_fn(fiber, f, values);
        }
        self.builtin_operator(fiber, iface, values, span)
    }

    /// The operator of a number or a string reached through a bound.
    fn builtin_operator(
        &mut self,
        fiber: &mut Self::Fiber,
        iface: TypeId,
        mut values: Vec<Value>,
        span: Span,
    ) -> Eval {
        let num = match values.first() {
            Some(Value::Int(_)) => Some(Num::Int(IntTy::I64)),
            Some(Value::U64(_)) => Some(Num::Int(IntTy::U64)),
            Some(Value::Float(_)) => Some(Num::Float),
            Some(Value::F32(_)) => Some(Num::F32),
            _ => None,
        };
        if iface == NEG {
            return match (values.pop(), num) {
                (Some(v), Some(n)) => self.unary(fiber, UnOp::Neg(n), v, span),
                _ => Err(self.internal(fiber, span, "an operator without an implementation")),
            };
        }
        let arith = match iface {
            ADD => ArithOp::Add,
            SUB => ArithOp::Sub,
            MUL => ArithOp::Mul,
            DIV => ArithOp::Div,
            _ => return Err(self.internal(fiber, span, "not an operator interface")),
        };
        let op = match (&values.first(), num) {
            (Some(Value::Str(_) | Value::Bytes(_)), _) if iface == ADD => BinOp::Concat,
            (_, Some(n)) => BinOp::Arith(arith, n),
            _ => return Err(self.internal(fiber, span, "an operator without an implementation")),
        };
        let (Some(r), Some(l)) = (values.pop(), values.pop()) else {
            return Err(self.internal(fiber, span, "an operator without operands"));
        };
        self.binary(fiber, op, l, r, span)
    }
}

impl<E: Engine> Operators for E {}

#[inline(always)]
fn binary_of<E: Engine>(
    engine: &mut E,
    fiber: &mut E::Fiber,
    op: BinOp,
    l: &Value,
    r: &Value,
    span: Span,
) -> Eval {
    match (op, l, r) {
        (BinOp::Arith(op, Num::Int(IntTy::I64)), Value::Int(a), Value::Int(b)) => {
            match int_arith(op, *a, *b) {
                Ok(n) => Ok(Value::Int(n)),
                Err(Fault::Overflow) => Err(engine.panic(fiber, span, "integer overflow")),
                Err(Fault::DivByZero) => Err(engine.panic(fiber, span, "division by zero")),
                Err(Fault::ModByZero) => Err(engine.panic(fiber, span, "modulo by zero")),
            }
        }
        (BinOp::Arith(op, Num::Int(ty)), l, r) => match (num::int_of(l), num::int_of(r)) {
            (Some(a), Some(b)) => match num::int_arith(op, ty, a, b) {
                Ok(n) => Ok(num::make_int(ty, n)),
                Err(msg) => Err(engine.panic(fiber, span, msg)),
            },
            _ => Err(engine.internal(fiber, span, "ill-typed integer operation")),
        },
        (BinOp::Arith(op, Num::Float), Value::Float(a), Value::Float(b)) => {
            Ok(Value::Float(num::float_arith(op, *a, *b)))
        }
        (BinOp::Arith(op, Num::F32), Value::F32(a), Value::F32(b)) => {
            Ok(Value::F32(num::f32_arith(op, *a, *b)))
        }
        (BinOp::Bit(op, ty), l, r) => match (num::int_of(l), num::int_of(r)) {
            (Some(a), Some(b)) => match num::int_bit(op, ty, a, b) {
                Ok(n) => Ok(num::make_int(ty, n)),
                Err(msg) => Err(engine.panic(fiber, span, msg)),
            },
            _ => Err(engine.internal(fiber, span, "ill-typed bitwise operation")),
        },
        (BinOp::Concat, Value::Str(a), Value::Str(b)) => {
            engine.mem_reserve(fiber, span, a.len() + b.len())?;
            let mut s = String::with_capacity(a.len() + b.len());
            s.push_str(a);
            s.push_str(b);
            Ok(Value::Str(Rc::from(s)))
        }
        (BinOp::Concat, Value::Bytes(a), Value::Bytes(b)) => {
            engine.mem_reserve(fiber, span, a.len() + b.len())?;
            let mut bytes = Vec::with_capacity(a.len() + b.len());
            bytes.extend_from_slice(a);
            bytes.extend_from_slice(b);
            Ok(Value::Bytes(Rc::from(bytes)))
        }
        (BinOp::Is, l, r) => Ok(Value::Bool(values_identical(l, r))),
        (BinOp::Cmp(op, Operand::Structural), l, r) => {
            Ok(Value::Bool(engine.structural_cmp(fiber, op, l, r, span)?))
        }
        (BinOp::Cmp(op, operand), l, r) => match compare(op, operand, l, r) {
            Some(b) => Ok(Value::Bool(b)),
            None => Err(engine.internal(fiber, span, "ill-typed comparison")),
        },
        _ => Err(engine.internal(fiber, span, "ill-typed binary operation")),
    }
}

/// The value of a literal.
#[doc(hidden)]
#[inline]
pub fn literal(lit: &Lit) -> Value {
    match lit {
        Lit::Unit => Value::Unit,
        Lit::Bool(b) => Value::Bool(*b),
        Lit::Int(i) => Value::Int(*i),
        Lit::U64(u) => Value::U64(*u),
        Lit::Float(x) => Value::Float(*x),
        Lit::F32(x) => Value::F32(*x),
        Lit::Char(c) => Value::Char(*c),
        Lit::Str(s) => Value::Str(Rc::clone(s)),
    }
}

/// Checked 64-bit integer arithmetic: overflow and division by zero are faults, `/` truncates
/// toward zero and `%` takes the sign of the dividend.
#[inline]
pub fn int_arith(op: ArithOp, a: i64, b: i64) -> Result<i64, Fault> {
    match op {
        ArithOp::Add => a.checked_add(b).ok_or(Fault::Overflow),
        ArithOp::Sub => a.checked_sub(b).ok_or(Fault::Overflow),
        ArithOp::Mul => a.checked_mul(b).ok_or(Fault::Overflow),
        ArithOp::Div if b == 0 => Err(Fault::DivByZero),
        ArithOp::Div => a.checked_div(b).ok_or(Fault::Overflow),
        ArithOp::Rem if b == 0 => Err(Fault::ModByZero),
        ArithOp::Rem => a.checked_rem(b).ok_or(Fault::Overflow),
    }
}

/// Compares two values of the same type; `None` if they are not what the operand kind says.
#[inline]
fn compare(op: CmpOp, operand: Operand, l: &Value, r: &Value) -> Option<bool> {
    fn by_ord<T: PartialOrd + ?Sized>(op: CmpOp, a: &T, b: &T) -> bool {
        match op {
            CmpOp::Eq => a == b,
            CmpOp::Ne => a != b,
            CmpOp::Lt => a < b,
            CmpOp::Le => a <= b,
            CmpOp::Gt => a > b,
            CmpOp::Ge => a >= b,
        }
    }
    Some(match (operand, l, r) {
        (Operand::Int(_), Value::Int(a), Value::Int(b)) => by_ord(op, a, b),
        (Operand::Int(_), Value::U64(a), Value::U64(b)) => by_ord(op, a, b),
        (Operand::Float, Value::Float(a), Value::Float(b)) => by_ord(op, a, b),
        (Operand::F32, Value::F32(a), Value::F32(b)) => by_ord(op, a, b),
        (Operand::Bool, Value::Bool(a), Value::Bool(b)) => by_ord(op, a, b),
        (Operand::Char, Value::Char(a), Value::Char(b)) => by_ord(op, a, b),
        (Operand::Str, Value::Str(a), Value::Str(b)) => by_ord::<str>(op, a, b),
        (Operand::Unit, Value::Unit, Value::Unit) => by_ord(op, &(), &()),
        (Operand::Structural, ..) => return None,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integer_division_truncates_and_rem_follows_the_dividend() {
        assert_eq!(int_arith(ArithOp::Div, 7, 2).ok(), Some(3));
        assert_eq!(int_arith(ArithOp::Div, -7, 2).ok(), Some(-3));
        assert_eq!(int_arith(ArithOp::Rem, -7, 3).ok(), Some(-1));
        assert_eq!(int_arith(ArithOp::Rem, 7, -3).ok(), Some(1));
    }

    #[test]
    fn integer_faults() {
        assert!(matches!(
            int_arith(ArithOp::Add, i64::MAX, 1),
            Err(Fault::Overflow)
        ));
        assert!(matches!(
            int_arith(ArithOp::Sub, i64::MIN, 1),
            Err(Fault::Overflow)
        ));
        assert!(matches!(
            int_arith(ArithOp::Mul, i64::MAX, 2),
            Err(Fault::Overflow)
        ));
        assert!(matches!(
            int_arith(ArithOp::Div, i64::MIN, -1),
            Err(Fault::Overflow)
        ));
        assert!(matches!(
            int_arith(ArithOp::Div, 1, 0),
            Err(Fault::DivByZero)
        ));
        assert!(matches!(
            int_arith(ArithOp::Rem, 1, 0),
            Err(Fault::ModByZero)
        ));
    }

    #[test]
    fn nan_compares_like_ieee() {
        let nan = Value::Float(f64::NAN);
        assert_eq!(compare(CmpOp::Eq, Operand::Float, &nan, &nan), Some(false));
        assert_eq!(compare(CmpOp::Ne, Operand::Float, &nan, &nan), Some(true));
        assert_eq!(
            compare(CmpOp::Lt, Operand::Float, &nan, &Value::Float(1.0)),
            Some(false)
        );
    }

    #[test]
    fn nan_orders_nothing_in_either_width() {
        let ops = [
            CmpOp::Eq,
            CmpOp::Ne,
            CmpOp::Lt,
            CmpOp::Le,
            CmpOp::Gt,
            CmpOp::Ge,
        ];
        let expect = [false, true, false, false, false, false];
        let nan = Value::Float(f64::NAN);
        let nan32 = Value::F32(f32::NAN);
        for (op, want) in ops.into_iter().zip(expect) {
            assert_eq!(
                compare(op, Operand::Float, &nan, &nan),
                Some(want),
                "{op:?}"
            );
            assert_eq!(
                compare(op, Operand::Float, &nan, &Value::Float(1.0)),
                Some(want),
                "{op:?}"
            );
            assert_eq!(
                compare(op, Operand::F32, &nan32, &Value::F32(1.0)),
                Some(want),
                "{op:?}"
            );
        }
    }
}
