//! Runtime values (ARCHITECTURE §3).
//!
//! Closures and host handles are variants of this enum, like the scalars. Strings, structs, tuples and enums
//! are reference counted, so copying a value is a pointer bump; a write goes through `Rc::make_mut`, which copies only
//! when the value is shared. That is exact value semantics (DESIGN §7.1), not an accidental
//! copy. Lists, maps, sets and class objects are reference types (DESIGN §5.3, §7.2): copying
//! the value shares the object, and equality of the `Rc` is identity.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

use colibri_check::hir::FnId;
use colibri_check::types::TypeId;

pub use crate::closure::{CellObj, ClosureObj};
pub use crate::heap::{ClassObj, ListObj, MapObj, SetObj};
pub use crate::hostheap::HostRef;
pub use crate::iter::IterObj;

/// A Colibri value.
#[derive(Clone, Debug)]
pub enum Value {
    Unit,
    Bool(bool),
    /// Every integer type but `u64` (`int` is 64-bit; narrower types are range-checked on
    /// operations by the width the operator carries).
    Int(i64),
    /// A `u64` (every value of that type, so `Show`, comparison and hashing need no static
    /// type; the other unsigned widths fit `Int`). ARCHITECTURE §6.
    U64(u64),
    /// A raw C pointer (`ptr[T]`, DESIGN §15.3): an address. A `ptr[T]` is never NULL; a
    /// nullable pointer is `ptr[T]?` and NULL is its `none`.
    Ptr(usize),
    Float(f64),
    /// An `f32`, rounded to single precision after every operation.
    F32(f32),
    Char(char),
    Str(Rc<str>),
    /// A `Bytes`: immutable, one byte per byte.
    Bytes(Rc<[u8]>),
    /// The buffer of a `BytesBuilder`: the field of its object, never visible to Colibri code.
    ByteBuf(Rc<RefCell<Vec<u8>>>),
    Struct(Rc<StructVal>),
    Tuple(Rc<[Value]>),
    /// Enums are immutable (a payload is not a place), so sharing is unobservable.
    Enum(Rc<EnumVal>),
    List(Rc<ListObj>),
    Map(Rc<MapObj>),
    Set(Rc<SetObj>),
    /// A class object (reference type).
    Obj(Rc<ClassObj>),
    /// An object of the embedding program, held through the host heap (EMBEDDING §3). Equal
    /// when it names the same slot and generation.
    Host(Rc<HostRef>),
    /// The content of a `weak` class field; an empty `Weak` stands for a cleared reference.
    Weak(Weak<ClassObj>),
    /// An interface value: a value boxed with the implementation that dispatches its methods
    /// (DESIGN §9.2). A struct inside is copied lazily like any struct; a class is shared.
    Dyn(Rc<DynVal>),
    /// A function used as a value; the name is for `Show` (`<fn add>`).
    Fn(FnId, Rc<str>),
    /// A closure (DESIGN §6.2).
    Closure(Rc<ClosureObj>),
    /// The storage of a captured variable (never visible to Colibri code): a local whose
    /// storage is `Cell` holds one in its frame slot.
    Cell(Rc<CellObj>),
    /// `a..b` or `a..=b`.
    Range(Rc<RangeVal>),
    /// An iterator object of a built-in collection, string or range; it implements `Iter[T]`
    /// for the program (reference semantics).
    Iter(Rc<IterObj>),
}

// A value is a tag and at most 16 bytes of payload (`Str(Rc<str>)` is a fat pointer, `Fn(FnId, Rc<str>)`
// a name and an id): 24 bytes. Nothing is stored inline beyond a scalar, so a struct such as a
// vector is a reference to its fields like any other.
const _: () = assert!(std::mem::size_of::<Value>() == 24);

/// The ends of a range value (`Range[int]`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RangeVal {
    pub start: i64,
    pub end: i64,
    pub inclusive: bool,
}

impl RangeVal {
    /// How many integers the range holds, saturating at `u64::MAX` (the full
    /// inclusive `i64` range holds 2^64).
    pub fn len(&self) -> u64 {
        let span = i128::from(self.end) - i128::from(self.start) + i128::from(self.inclusive);
        u64::try_from(span.max(0)).unwrap_or(u64::MAX)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn contains(&self, x: i64) -> bool {
        x >= self.start && (x < self.end || (self.inclusive && x == self.end))
    }
}

/// The contents of an interface value.
#[derive(Clone, Debug, PartialEq)]
pub struct DynVal {
    /// The implementation (an index into the program's `impls`).
    pub impl_id: u32,
    pub val: Value,
}

impl PartialEq for Value {
    /// Scalars, strings, structs, tuples and enums by content; reference types and weak
    /// references by identity. (Colibri's own `==` is [`crate::compare::values_equal`].)
    fn eq(&self, other: &Value) -> bool {
        match (self, other) {
            (Value::Unit, Value::Unit) => true,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Int(a), Value::Int(b)) => a == b,
            (Value::U64(a), Value::U64(b)) => a == b,
            (Value::Ptr(a), Value::Ptr(b)) => a == b,
            (Value::Float(a), Value::Float(b)) => a == b,
            (Value::F32(a), Value::F32(b)) => a == b,
            (Value::Char(a), Value::Char(b)) => a == b,
            (Value::Str(a), Value::Str(b)) => a == b,
            (Value::Bytes(a), Value::Bytes(b)) => a == b,
            (Value::ByteBuf(a), Value::ByteBuf(b)) => Rc::ptr_eq(a, b),
            (Value::Struct(a), Value::Struct(b)) => a == b,
            (Value::Tuple(a), Value::Tuple(b)) => a == b,
            (Value::Enum(a), Value::Enum(b)) => a == b,
            (Value::List(a), Value::List(b)) => Rc::ptr_eq(a, b),
            (Value::Map(a), Value::Map(b)) => Rc::ptr_eq(a, b),
            (Value::Set(a), Value::Set(b)) => Rc::ptr_eq(a, b),
            (Value::Obj(a), Value::Obj(b)) => Rc::ptr_eq(a, b),
            (Value::Host(a), Value::Host(b)) => a.index == b.index && a.generation == b.generation,
            (Value::Weak(a), Value::Weak(b)) => Weak::ptr_eq(a, b),
            (Value::Dyn(a), Value::Dyn(b)) => a == b,
            (Value::Fn(a, _), Value::Fn(b, _)) => a == b,
            (Value::Closure(a), Value::Closure(b)) => Rc::ptr_eq(a, b),
            (Value::Cell(a), Value::Cell(b)) => Rc::ptr_eq(a, b),
            (Value::Range(a), Value::Range(b)) => a == b,
            (Value::Iter(a), Value::Iter(b)) => Rc::ptr_eq(a, b),
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

    pub fn bytes(b: &[u8]) -> Value {
        Value::Bytes(Rc::from(b))
    }

    pub fn range(start: i64, end: i64, inclusive: bool) -> Value {
        Value::Range(Rc::new(RangeVal {
            start,
            end,
            inclusive,
        }))
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
        matches!(
            self,
            Value::Struct(_) | Value::Tuple(_) | Value::Enum(_) | Value::Dyn(_)
        )
    }
}

/// The value of a variable slot, looking through a cell.
pub fn unwrap_cell_ref(v: &Value) -> Value {
    match v {
        Value::Cell(c) => c.get(),
        other => other.clone(),
    }
}

/// The value a cell holds, for a variable that is handed back to its owner.
pub fn unwrap_cell(v: Value) -> Value {
    match v {
        Value::Cell(c) if Rc::strong_count(&c) == 1 => c.take(),
        Value::Cell(c) => c.get(),
        other => other,
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
        Value::Dyn(rc) => {
            if let Some(d) = Rc::get_mut(rc)
                && d.val.is_composite()
            {
                work.push(std::mem::replace(&mut d.val, Value::Unit));
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
