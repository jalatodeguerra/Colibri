//! Runtime values (ARCHITECTURE §3).
//!
//! Only the variants the language slice needs exist. The rest of the architecture's `Value`
//! (collections, class objects, closures, host handles) joins this enum as those features
//! land. Strings, structs, tuples and enums are reference counted, so copying a value is a
//! pointer bump; a write goes through `Rc::make_mut`, which copies only when the value is
//! shared. That is exact value semantics (DESIGN §7.1), not an accidental copy.

use std::cell::Cell;
use std::rc::Rc;

use colibri_check::types::TypeId;

/// A Colibri value.
#[derive(Clone, Debug, PartialEq)]
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
