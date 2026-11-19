//! Runtime values (ARCHITECTURE §3).
//!
//! The variants the language needs so far exist; closures and host handles join this enum
//! as those features land. Strings, structs, tuples and enums are reference counted, so
//! copying a value is a pointer bump; a write goes through `Rc::make_mut`, which copies only
//! when the value is shared. That is exact value semantics (DESIGN §7.1), not an accidental
//! copy. Lists, maps, sets and class objects are reference types (DESIGN §5.3, §7.2): copying
//! the value shares the object, and equality of the `Rc` is identity.

use std::cell::Cell;
use std::rc::{Rc, Weak};

use colibri_check::types::TypeId;

pub use crate::heap::{ClassObj, ListObj, MapObj, SetObj};

/// A Colibri value.
#[derive(Clone, Debug)]
pub enum Value {
    Unit,
    Bool(bool),
    /// Every integer width (`int` is 64-bit; narrower types are range-checked on operations).
    Int(i64),
    Float(f64),
    Char(char),
    Str(Rc<str>),
    Struct(Rc<StructVal>),
    Tuple(Rc<[Value]>),
    /// Enums are immutable (a payload is not a place), so sharing is unobservable.
    Enum(Rc<EnumVal>),
    List(Rc<ListObj>),
    Map(Rc<MapObj>),
    Set(Rc<SetObj>),
    /// A class object (reference type).
    Obj(Rc<ClassObj>),
    /// The content of a `weak` class field; an empty `Weak` stands for a cleared reference.
    Weak(Weak<ClassObj>),
}

impl PartialEq for Value {
    /// Scalars, strings, structs, tuples and enums by content; reference types and weak
    /// references by identity. (Colibri's own `==` is [`crate::values_equal`].)
    fn eq(&self, other: &Value) -> bool {
        match (self, other) {
            (Value::Unit, Value::Unit) => true,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Int(a), Value::Int(b)) => a == b,
            (Value::Float(a), Value::Float(b)) => a == b,
            (Value::Char(a), Value::Char(b)) => a == b,
            (Value::Str(a), Value::Str(b)) => a == b,
            (Value::Struct(a), Value::Struct(b)) => a == b,
            (Value::Tuple(a), Value::Tuple(b)) => a == b,
            (Value::Enum(a), Value::Enum(b)) => a == b,
            (Value::List(a), Value::List(b)) => Rc::ptr_eq(a, b),
            (Value::Map(a), Value::Map(b)) => Rc::ptr_eq(a, b),
            (Value::Set(a), Value::Set(b)) => Rc::ptr_eq(a, b),
            (Value::Obj(a), Value::Obj(b)) => Rc::ptr_eq(a, b),
            (Value::Weak(a), Value::Weak(b)) => Weak::ptr_eq(a, b),
            _ => false,
        }
    }
}

/// A struct value: its type and its fields in declaration order.
#[derive(Clone, Debug, PartialEq)]
pub struct StructVal {
    pub ty: TypeId,
    pub fields: Box<[Value]>,
}

/// An enum value: its type, which variant, and the variant's payload.
#[derive(Clone, Debug, PartialEq)]
pub struct EnumVal {
    pub ty: TypeId,
    pub variant: u32,
    pub payload: Box<[Value]>,
}

impl Value {
    pub fn str(s: &str) -> Value {
        Value::Str(Rc::from(s))
    }

    pub fn tuple(items: Vec<Value>) -> Value {
        Value::Tuple(Rc::from(items))
    }

    pub fn strukt(ty: TypeId, fields: Vec<Value>) -> Value {
        Value::Struct(Rc::new(StructVal {
            ty,
            fields: fields.into_boxed_slice(),
        }))
    }

    pub fn variant(ty: TypeId, variant: u32, payload: Vec<Value>) -> Value {
        Value::Enum(Rc::new(EnumVal {
            ty,
            variant,
            payload: payload.into_boxed_slice(),
        }))
    }

    pub fn list(items: Vec<Value>) -> Value {
        Value::List(ListObj::new(items))
    }

    /// The prelude `Option`: `none`, or `some(v)`.
    pub fn option(inner: Option<Value>) -> Value {
        use colibri_check::types::{OPTION_TYPE, VARIANT_NONE, VARIANT_SOME};
        match inner {
            Some(v) => Value::variant(OPTION_TYPE, VARIANT_SOME, vec![v]),
            None => Value::variant(OPTION_TYPE, VARIANT_NONE, Vec::new()),
        }
    }

    /// Whether the value owns other values.
    fn is_composite(&self) -> bool {
        matches!(self, Value::Struct(_) | Value::Tuple(_) | Value::Enum(_))
    }
}

// ---- dropping deeply nested values ----
//
// A list built as `Cons(1, Cons(2, ...))` a million deep would overflow the Rust stack if its
// destructor recursed once per node. Past a modest depth, destruction switches to an explicit
// worklist.

/// How many nested destructors may run recursively before switching to the worklist.
const MAX_DROP_DEPTH: u32 = 256;

thread_local! {
    static DROP_DEPTH: Cell<u32> = const { Cell::new(0) };
}

/// Destroys the children of a struct or enum that is being dropped.
fn drop_children(children: Box<[Value]>) {
    if !children.iter().any(Value::is_composite) {
        return;
    }
    let depth = DROP_DEPTH.with(Cell::get);
    if depth < MAX_DROP_DEPTH {
        DROP_DEPTH.with(|d| d.set(depth + 1));
        drop(children);
        DROP_DEPTH.with(|d| d.set(depth));
        return;
    }
    let mut work: Vec<Value> = children.into_vec();
    while let Some(value) = work.pop() {
        dismantle(value, &mut work);
    }
}

/// Moves the children of `value` onto `work` when it is the last owner, then drops the (now
/// shallow) value.
fn dismantle(mut value: Value, work: &mut Vec<Value>) {
    let mut take = |items: &mut [Value]| {
        work.extend(
            items
                .iter_mut()
                .filter(|v| v.is_composite())
                .map(|v| std::mem::replace(v, Value::Unit)),
        );
    };
    match &mut value {
        Value::Struct(rc) => {
            if let Some(s) = Rc::get_mut(rc) {
                take(&mut s.fields);
            }
        }
        Value::Enum(rc) => {
            if let Some(e) = Rc::get_mut(rc) {
                take(&mut e.payload);
            }
        }
        Value::Tuple(rc) => {
            if let Some(items) = Rc::get_mut(rc) {
                take(items);
            }
        }
        _ => {}
    }
}

impl Drop for StructVal {
    fn drop(&mut self) {
        drop_children(std::mem::take(&mut self.fields));
    }
}

impl Drop for EnumVal {
    fn drop(&mut self) {
        drop_children(std::mem::take(&mut self.payload));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A chain a million values deep, built and dropped without recursing a million times.
    #[test]
    fn dropping_a_very_deep_value_does_not_overflow_the_stack() {
        let mut list = Value::variant(TypeId(0), 0, Vec::new());
        for i in 0..1_000_000 {
            list = Value::variant(TypeId(0), 1, vec![Value::Int(i), list]);
        }
        drop(list);
    }

    #[test]
    fn nested_tuples_and_structs_drop_too() {
        let mut v = Value::Unit;
        for _ in 0..200_000 {
            v = Value::tuple(vec![Value::strukt(TypeId(0), vec![v])]);
        }
        drop(v);
    }

    #[test]
    fn a_shared_value_survives_the_drop_of_one_owner() {
        let inner = Value::strukt(TypeId(0), vec![Value::Int(1)]);
        let outer = Value::tuple(vec![inner.clone(), inner.clone()]);
        drop(outer);
        assert!(matches!(inner, Value::Struct(_)));
    }
}
