//! Moving values between Rust and Colibri (EMBEDDING §0 A, §6).
//!
//! [`IntoColibri`] builds a Colibri value from a Rust one, [`FromColibri`] reads one by
//! reference (the interpreter stays the owner of heap objects, so they die through its
//! `release`, where `Drop` hooks run). Both take a [`Cx`] because a host type needs the
//! interpreter's type table, and both describe their Colibri type to a [`TypeRegistry`], which
//! is how a function signature becomes interface text and how `get_fn` checks a Rust signature
//! against a script's.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Display;
use std::hash::Hash;

use colibri_check::types::{TypeId, VARIANT_NONE, VARIANT_OK, VARIANT_SOME, VecTy};
use colibri_interp::{Interp, Value as Inner};

use crate::ctx::Cx;
use crate::error::Error;
use crate::types::{TypeDesc, TypeRegistry};

/// A Colibri value in flight between Rust and a script. Opaque: convert with
/// [`IntoColibri`] and [`FromColibri`]; implementations for host types take it apart with
/// [`Value::field`] and [`Value::variant_index`] and build one with [`Cx::new_struct`].
#[derive(Clone, Debug)]
pub struct Value(pub(crate) Inner);

impl Value {
    /// The `i`th field of a struct value.
    pub fn field(&self, i: usize) -> Option<Value> {
        match &self.0 {
            Inner::Struct(s) => s.fields.get(i).cloned().map(Value),
            _ => None,
        }
    }

    /// The index of the variant of an enum value.
    pub fn variant_index(&self) -> Option<u32> {
        match &self.0 {
            Inner::Enum(e) => Some(e.variant),
            _ => None,
        }
    }
}

/// What a [`Value`] holds, one level deep: the shape a host function that works on any type
/// (a serializer, `json.encode`) looks at. Collections and fields come out as values again.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum ValueKind {
    Unit,
    Bool(bool),
    /// Every integer type but `u64`.
    Int(i64),
    U64(u64),
    Float(f64),
    F32(f32),
    Char(char),
    Str(String),
    /// A `struct` value: its type and its fields in declaration order.
    Struct {
        ty: TypeId,
        fields: Vec<Value>,
    },
    Tuple(Vec<Value>),
    /// An enum value (`Option` and `Result` included): its type, the index of the variant and
    /// its payload.
    Enum {
        ty: TypeId,
        variant: u32,
        payload: Vec<Value>,
    },
    List(Vec<Value>),
    /// The entries in insertion order.
    Map(Vec<(Value, Value)>),
    Set(Vec<Value>),
    /// A class object, a function, a host object or anything else that has no plain data.
    Other,
}

impl Value {
    pub fn unit() -> Value {
        Value(Inner::Unit)
    }

    pub fn bool(v: bool) -> Value {
        Value(Inner::Bool(v))
    }

    /// An `int`, or a value of any integer type other than `u64` that fits an `i64`.
    pub fn int(v: i64) -> Value {
        Value(Inner::Int(v))
    }

    pub fn u64(v: u64) -> Value {
        Value(Inner::U64(v))
    }

    pub fn float(v: f64) -> Value {
        Value(Inner::Float(v))
    }

    pub fn f32(v: f32) -> Value {
        Value(Inner::F32(v))
    }

    pub fn char(v: char) -> Value {
        Value(Inner::Char(v))
    }

    pub fn str(v: &str) -> Value {
        Value(Inner::str(v))
    }

    pub fn list(items: Vec<Value>) -> Value {
        Value(Inner::list(items.into_iter().map(|v| v.0).collect()))
    }

    /// A `Map` with these entries in order (a repeated key keeps the last value).
    pub fn map(entries: Vec<(Value, Value)>) -> Value {
        Value(colibri_interp::map_value(
            entries.into_iter().map(|(k, v)| (k.0, v.0)).collect(),
        ))
    }

    pub fn tuple(items: Vec<Value>) -> Value {
        Value(Inner::tuple(items.into_iter().map(|v| v.0).collect()))
    }

    /// `some(v)` or `none`.
    pub fn option(inner: Option<Value>) -> Value {
        Value(Inner::option(inner.map(|v| v.0)))
    }

    /// A struct of type `ty` with its fields in declaration order (see [`Ctx::type_defs`]).
    pub fn new_struct(ty: TypeId, fields: Vec<Value>) -> Value {
        Value(Inner::strukt(ty, fields.into_iter().map(|v| v.0).collect()))
    }

    /// A value of the enum `ty`: its `variant`th variant with a payload.
    pub fn new_enum(ty: TypeId, variant: u32, payload: Vec<Value>) -> Value {
        Value(Inner::variant(
            ty,
            variant,
            payload.into_iter().map(|v| v.0).collect(),
        ))
    }

    /// `ok(v)` of `Result[T]`.
    pub fn ok(v: Value) -> Value {
        Value(Interp::ok_variant(v.0))
    }

    /// `err(e)` of `Result[T]`, with an `Error` made from the message.
    pub fn err(message: &str) -> Value {
        Value(Interp::err_variant(Interp::error_value(message)))
    }

    /// What the value holds. Collections are copied out (the elements are shared).
    pub fn kind(&self) -> ValueKind {
        let wrap = |items: Vec<Inner>| items.into_iter().map(Value).collect::<Vec<_>>();
        match &self.0 {
            Inner::Unit => ValueKind::Unit,
            Inner::Bool(v) => ValueKind::Bool(*v),
            Inner::Int(v) => ValueKind::Int(*v),
            Inner::U64(v) => ValueKind::U64(*v),
            Inner::Float(v) => ValueKind::Float(*v),
            Inner::F32(v) => ValueKind::F32(*v),
            Inner::Char(v) => ValueKind::Char(*v),
            Inner::Str(s) => ValueKind::Str(s.to_string()),
            Inner::Struct(s) => ValueKind::Struct {
                ty: s.ty,
                fields: wrap(s.fields.to_vec()),
            },
            Inner::Tuple(items) => ValueKind::Tuple(wrap(items.to_vec())),
            Inner::Enum(e) => ValueKind::Enum {
                ty: e.ty,
                variant: e.variant,
                payload: wrap(e.payload.to_vec()),
            },
            Inner::List(list) => ValueKind::List(wrap(list.snapshot())),
            Inner::Map(map) => ValueKind::Map(
                map.snapshot()
                    .into_iter()
                    .map(|(k, v)| (Value(k), Value(v)))
                    .collect(),
            ),
            Inner::Set(set) => ValueKind::Set(wrap(set.snapshot())),
            _ => ValueKind::Other,
        }
    }
}

/// Any value, for a function that works whatever the type is: its signature has a type
/// parameter `T` (`fn encode[T](a: T) -> Result[str]`), which scripts fill in. Use it with
/// [`Ctx::type_args`] to read the type arguments of the call.
impl IntoColibri for Value {
    fn ty(_: &mut TypeRegistry) -> TypeDesc {
        TypeDesc::Param("T".to_string())
    }
    fn into_colibri(self, _: &mut Cx<'_>) -> Value {
        self
    }
}

impl FromColibri for Value {
    fn ty(_: &mut TypeRegistry) -> TypeDesc {
        TypeDesc::Param("T".to_string())
    }
    fn from_colibri(value: &Value, _: &mut Cx<'_>) -> Result<Self, Error> {
        Ok(value.clone())
    }
}

/// A Rust type that can be passed to Colibri.
pub trait IntoColibri {
    /// The Colibri type this becomes. A struct or enum of the host declares itself to `reg`.
    fn ty(reg: &mut TypeRegistry) -> TypeDesc;
    fn into_colibri(self, cx: &mut Cx<'_>) -> Value;
    /// Whether the value still stands for something: a `Handle` to a destroyed host object
    /// does not, and `Option<Handle<T>>` turns it into `none` (EMBEDDING §3).
    #[doc(hidden)]
    fn is_live(&self, _cx: &Cx<'_>) -> bool {
        true
    }
}

/// A Rust type that a Colibri value can be turned into.
pub trait FromColibri: Sized {
    /// The Colibri type this is read from.
    fn ty(reg: &mut TypeRegistry) -> TypeDesc;
    /// Reads the value. It is borrowed: the interpreter keeps its reference.
    fn from_colibri(value: &Value, cx: &mut Cx<'_>) -> Result<Self, Error>;
}

/// A tuple of Rust values passed as the arguments of a call.
pub trait Args {
    /// The Colibri types of the arguments, in order.
    fn tys(reg: &mut TypeRegistry) -> Vec<TypeDesc>;
    fn into_values(self, cx: &mut Cx<'_>) -> Vec<Value>;
}

pub(crate) fn wrong(expected: &TypeDesc, got: &Inner) -> Error {
    Error::Convert(format!("expected a `{expected}` value, got {got:?}"))
}

macro_rules! scalar {
    ($rust:ty, $ty:expr, $variant:ident) => {
        impl IntoColibri for $rust {
            fn ty(_: &mut TypeRegistry) -> TypeDesc {
                $ty
            }
            fn into_colibri(self, _: &mut Cx<'_>) -> Value {
                Value(Inner::$variant(self))
            }
        }
        impl FromColibri for $rust {
            fn ty(_: &mut TypeRegistry) -> TypeDesc {
                $ty
            }
            fn from_colibri(value: &Value, _: &mut Cx<'_>) -> Result<Self, Error> {
                match &value.0 {
                    Inner::$variant(v) => Ok(*v),
                    other => Err(wrong(&$ty, other)),
                }
            }
        }
    };
}

scalar!(i64, TypeDesc::Int, Int);
scalar!(u64, TypeDesc::U64, U64);
scalar!(f64, TypeDesc::Float, Float);
scalar!(f32, TypeDesc::F32, F32);
scalar!(bool, TypeDesc::Bool, Bool);
scalar!(char, TypeDesc::Char, Char);

/// The vector types, as glam's (`Vec2` is `vec.vec2`, `IVec3` is `vec.ivec3`).
macro_rules! vector {
    ($($rust:ty => $ty:expr, $variant:ident, $strukt:ident, $n:expr, $scalar:ty;)*) => {$(
        impl IntoColibri for $rust {
            fn ty(_: &mut TypeRegistry) -> TypeDesc {
                TypeDesc::Vec($ty)
            }
            fn into_colibri(self, _: &mut Cx<'_>) -> Value {
                let mut c = [<$scalar>::default(); 4];
                c[..$n].copy_from_slice(&self.to_array());
                Value(Inner::$variant(colibri_interp::$strukt { n: $n, c }))
            }
        }
        impl FromColibri for $rust {
            fn ty(_: &mut TypeRegistry) -> TypeDesc {
                TypeDesc::Vec($ty)
            }
            fn from_colibri(value: &Value, _: &mut Cx<'_>) -> Result<Self, Error> {
                match &value.0 {
                    Inner::$variant(v) if v.n == $n => {
                        let mut a = [<$scalar>::default(); $n];
                        a.copy_from_slice(&v.c[..$n]);
                        Ok(<$rust>::from_array(a))
                    }
                    other => Err(wrong(&TypeDesc::Vec($ty), other)),
                }
            }
        }
    )*};
}

vector! {
    glam::Vec2 => VecTy { int: false, n: 2 }, Vec, FVec, 2, f32;
    glam::Vec3 => VecTy { int: false, n: 3 }, Vec, FVec, 3, f32;
    glam::Vec4 => VecTy { int: false, n: 4 }, Vec, FVec, 4, f32;
    glam::IVec2 => VecTy { int: true, n: 2 }, IVec, IVec, 2, i32;
    glam::IVec3 => VecTy { int: true, n: 3 }, IVec, IVec, 3, i32;
    glam::IVec4 => VecTy { int: true, n: 4 }, IVec, IVec, 4, i32;
}

/// The integer types stored as `Int(i64)`: every value of the Colibri type fits, so reading
/// one back narrows without loss (a value out of range is an interpreter bug, reported as
/// an error instead of truncated).
macro_rules! narrow_int {
    ($($rust:ty => $ty:expr),*) => {$(
        impl IntoColibri for $rust {
            fn ty(_: &mut TypeRegistry) -> TypeDesc {
                $ty
            }
            fn into_colibri(self, _: &mut Cx<'_>) -> Value {
                Value(Inner::Int(i64::from(self)))
            }
        }
        impl FromColibri for $rust {
            fn ty(_: &mut TypeRegistry) -> TypeDesc {
                $ty
            }
            fn from_colibri(value: &Value, _: &mut Cx<'_>) -> Result<Self, Error> {
                match &value.0 {
                    Inner::Int(v) => <$rust>::try_from(*v).map_err(|_| wrong(&$ty, &Inner::Int(*v))),
                    other => Err(wrong(&$ty, other)),
                }
            }
        }
    )*};
}

narrow_int!(i8 => TypeDesc::I8, i16 => TypeDesc::I16, i32 => TypeDesc::I32, u8 => TypeDesc::U8, u16 => TypeDesc::U16, u32 => TypeDesc::U32);

/// `usize` and `isize` are Colibri's `int` with a range check on the way.
impl IntoColibri for usize {
    fn ty(_: &mut TypeRegistry) -> TypeDesc {
        TypeDesc::Int
    }
    /// # Panics
    /// If the value does not fit an `int` (more than `i64::MAX`).
    fn into_colibri(self, _: &mut Cx<'_>) -> Value {
        let v =
            i64::try_from(self).expect("a `usize` above `i64::MAX` does not fit a Colibri `int`");
        Value(Inner::Int(v))
    }
}

impl FromColibri for usize {
    fn ty(_: &mut TypeRegistry) -> TypeDesc {
        TypeDesc::Int
    }
    fn from_colibri(value: &Value, _: &mut Cx<'_>) -> Result<Self, Error> {
        match &value.0 {
            Inner::Int(v) => usize::try_from(*v)
                .map_err(|_| Error::Convert(format!("the value {v} does not fit a `usize`"))),
            other => Err(wrong(&TypeDesc::Int, other)),
        }
    }
}

impl IntoColibri for isize {
    fn ty(_: &mut TypeRegistry) -> TypeDesc {
        TypeDesc::Int
    }
    fn into_colibri(self, _: &mut Cx<'_>) -> Value {
        Value(Inner::Int(self as i64))
    }
}

impl FromColibri for isize {
    fn ty(_: &mut TypeRegistry) -> TypeDesc {
        TypeDesc::Int
    }
    fn from_colibri(value: &Value, _: &mut Cx<'_>) -> Result<Self, Error> {
        match &value.0 {
            Inner::Int(v) => isize::try_from(*v)
                .map_err(|_| Error::Convert(format!("the value {v} does not fit an `isize`"))),
            other => Err(wrong(&TypeDesc::Int, other)),
        }
    }
}

impl IntoColibri for () {
    fn ty(_: &mut TypeRegistry) -> TypeDesc {
        TypeDesc::Unit
    }
    fn into_colibri(self, _: &mut Cx<'_>) -> Value {
        Value(Inner::Unit)
    }
}

impl FromColibri for () {
    fn ty(_: &mut TypeRegistry) -> TypeDesc {
        TypeDesc::Unit
    }
    fn from_colibri(value: &Value, _: &mut Cx<'_>) -> Result<Self, Error> {
        match &value.0 {
            Inner::Unit => Ok(()),
            other => Err(wrong(&TypeDesc::Unit, other)),
        }
    }
}

impl IntoColibri for &str {
    fn ty(_: &mut TypeRegistry) -> TypeDesc {
        TypeDesc::Str
    }
    fn into_colibri(self, _: &mut Cx<'_>) -> Value {
        Value(Inner::str(self))
    }
}

impl IntoColibri for String {
    fn ty(_: &mut TypeRegistry) -> TypeDesc {
        TypeDesc::Str
    }
    fn into_colibri(self, _: &mut Cx<'_>) -> Value {
        Value(Inner::str(&self))
    }
}

impl FromColibri for String {
    fn ty(_: &mut TypeRegistry) -> TypeDesc {
        TypeDesc::Str
    }
    fn from_colibri(value: &Value, _: &mut Cx<'_>) -> Result<Self, Error> {
        match &value.0 {
            Inner::Str(s) => Ok(s.to_string()),
            other => Err(wrong(&TypeDesc::Str, other)),
        }
    }
}

// ---- collections ----

impl<T: IntoColibri> IntoColibri for Vec<T> {
    fn ty(reg: &mut TypeRegistry) -> TypeDesc {
        TypeDesc::List(Box::new(T::ty(reg)))
    }
    fn into_colibri(self, cx: &mut Cx<'_>) -> Value {
        Value(Inner::list(
            self.into_iter().map(|t| t.into_colibri(cx).0).collect(),
        ))
    }
}

/// A slice is copied into a new list.
impl<T: IntoColibri + Clone> IntoColibri for &[T] {
    fn ty(reg: &mut TypeRegistry) -> TypeDesc {
        TypeDesc::List(Box::new(T::ty(reg)))
    }
    fn into_colibri(self, cx: &mut Cx<'_>) -> Value {
        Value(Inner::list(
            self.iter().map(|t| t.clone().into_colibri(cx).0).collect(),
        ))
    }
}

impl<T: FromColibri> FromColibri for Vec<T> {
    fn ty(reg: &mut TypeRegistry) -> TypeDesc {
        TypeDesc::List(Box::new(T::ty(reg)))
    }
    fn from_colibri(value: &Value, cx: &mut Cx<'_>) -> Result<Self, Error> {
        match &value.0 {
            Inner::List(list) => list
                .snapshot()
                .into_iter()
                .map(|item| T::from_colibri(&Value(item), cx))
                .collect(),
            other => Err(wrong(&Self::ty(&mut TypeRegistry::new()), other)),
        }
    }
}

impl<T: IntoColibri> IntoColibri for Option<T> {
    fn ty(reg: &mut TypeRegistry) -> TypeDesc {
        TypeDesc::Option(Box::new(T::ty(reg)))
    }
    fn into_colibri(self, cx: &mut Cx<'_>) -> Value {
        let live = self.filter(|t| t.is_live(cx));
        Value(Inner::option(live.map(|t| t.into_colibri(cx).0)))
    }
}

impl<T: FromColibri> FromColibri for Option<T> {
    fn ty(reg: &mut TypeRegistry) -> TypeDesc {
        TypeDesc::Option(Box::new(T::ty(reg)))
    }
    fn from_colibri(value: &Value, cx: &mut Cx<'_>) -> Result<Self, Error> {
        match &value.0 {
            Inner::Enum(e) if e.variant == VARIANT_NONE => Ok(None),
            Inner::Enum(e) if e.variant == VARIANT_SOME => match e.payload.first() {
                Some(inner) => Ok(Some(T::from_colibri(&Value(inner.clone()), cx)?)),
                None => Err(wrong(&Self::ty(&mut TypeRegistry::new()), &value.0)),
            },
            other => Err(wrong(&Self::ty(&mut TypeRegistry::new()), other)),
        }
    }
}

/// A Rust `Err` becomes a Colibri `err` whose `Error` has the message of `E` (EMBEDDING §1);
/// it is not a panic.
impl<T: IntoColibri, E: Display> IntoColibri for Result<T, E> {
    fn ty(reg: &mut TypeRegistry) -> TypeDesc {
        TypeDesc::Result(Box::new(T::ty(reg)))
    }
    fn into_colibri(self, cx: &mut Cx<'_>) -> Value {
        Value(match self {
            Ok(v) => Interp::ok_variant(v.into_colibri(cx).0),
            Err(e) => Interp::err_variant(Interp::error_value(&e.to_string())),
        })
    }
}

/// A Colibri `Result[T]` read as a Rust `Result`: the error is its message (the whole chain).
impl<T: FromColibri> FromColibri for Result<T, String> {
    fn ty(reg: &mut TypeRegistry) -> TypeDesc {
        TypeDesc::Result(Box::new(T::ty(reg)))
    }
    fn from_colibri(value: &Value, cx: &mut Cx<'_>) -> Result<Self, Error> {
        match &value.0 {
            Inner::Enum(e) if e.variant == VARIANT_OK => match e.payload.first() {
                Some(inner) => Ok(Ok(T::from_colibri(&Value(inner.clone()), cx)?)),
                None => Err(wrong(&Self::ty(&mut TypeRegistry::new()), &value.0)),
            },
            Inner::Enum(e) => match e.payload.first() {
                Some(error) => Ok(Err(cx.interp.error_report(cx.fiber, error))),
                None => Err(wrong(&Self::ty(&mut TypeRegistry::new()), &value.0)),
            },
            other => Err(wrong(&Self::ty(&mut TypeRegistry::new()), other)),
        }
    }
}

macro_rules! tuples {
    ($(($($name:ident $idx:tt),+))*) => {$(
        impl<$($name: IntoColibri),+> IntoColibri for ($($name,)+) {
            fn ty(reg: &mut TypeRegistry) -> TypeDesc {
                TypeDesc::Tuple(vec![$($name::ty(reg)),+])
            }
            fn into_colibri(self, cx: &mut Cx<'_>) -> Value {
                Value(Inner::tuple(vec![$(self.$idx.into_colibri(cx).0),+]))
            }
        }
        impl<$($name: FromColibri),+> FromColibri for ($($name,)+) {
            fn ty(reg: &mut TypeRegistry) -> TypeDesc {
                TypeDesc::Tuple(vec![$($name::ty(reg)),+])
            }
            fn from_colibri(value: &Value, cx: &mut Cx<'_>) -> Result<Self, Error> {
                match &value.0 {
                    Inner::Tuple(items) => Ok(($(
                        $name::from_colibri(
                            &Value(items.get($idx).cloned().ok_or_else(|| wrong(&Self::ty(&mut TypeRegistry::new()), &value.0))?),
                            cx,
                        )?,
                    )+)),
                    other => Err(wrong(&Self::ty(&mut TypeRegistry::new()), other)),
                }
            }
        }
    )*};
}

tuples! {
    (A 0, B 1)
    (A 0, B 1, C 2)
    (A 0, B 1, C 2, D 3)
}

/// A `Map` keeps insertion order; a `BTreeMap` comes out in key order.
impl<K: IntoColibri, V: IntoColibri> IntoColibri for BTreeMap<K, V> {
    fn ty(reg: &mut TypeRegistry) -> TypeDesc {
        TypeDesc::Map(Box::new(K::ty(reg)), Box::new(V::ty(reg)))
    }
    fn into_colibri(self, cx: &mut Cx<'_>) -> Value {
        Value(colibri_interp::map_value(
            self.into_iter()
                .map(|(k, v)| (k.into_colibri(cx).0, v.into_colibri(cx).0))
                .collect(),
        ))
    }
}

/// A `HashMap` comes out sorted by key, so a script iterating it sees the same order on every
/// run (there is no `HashMap` going into Colibri, for the same reason).
impl<K: IntoColibri + Ord + Hash, V: IntoColibri> IntoColibri for HashMap<K, V> {
    fn ty(reg: &mut TypeRegistry) -> TypeDesc {
        TypeDesc::Map(Box::new(K::ty(reg)), Box::new(V::ty(reg)))
    }
    fn into_colibri(self, cx: &mut Cx<'_>) -> Value {
        let sorted: BTreeMap<K, V> = self.into_iter().collect();
        sorted.into_colibri(cx)
    }
}

impl<K: FromColibri + Ord, V: FromColibri> FromColibri for BTreeMap<K, V> {
    fn ty(reg: &mut TypeRegistry) -> TypeDesc {
        TypeDesc::Map(Box::new(K::ty(reg)), Box::new(V::ty(reg)))
    }
    fn from_colibri(value: &Value, cx: &mut Cx<'_>) -> Result<Self, Error> {
        match &value.0 {
            Inner::Map(map) => map
                .snapshot()
                .into_iter()
                .map(|(k, v)| {
                    Ok((
                        K::from_colibri(&Value(k), cx)?,
                        V::from_colibri(&Value(v), cx)?,
                    ))
                })
                .collect(),
            other => Err(wrong(&Self::ty(&mut TypeRegistry::new()), other)),
        }
    }
}

macro_rules! args {
    ($($name:ident),*) => {
        impl<$($name: IntoColibri),*> Args for ($($name,)*) {
            fn tys(reg: &mut TypeRegistry) -> Vec<TypeDesc> {
                let _ = &reg;
                vec![$($name::ty(reg)),*]
            }
            #[allow(non_snake_case, unused_variables)]
            fn into_values(self, cx: &mut Cx<'_>) -> Vec<Value> {
                let ($($name,)*) = self;
                vec![$($name.into_colibri(cx)),*]
            }
        }
    };
}

args!();
args!(A);
args!(A, B);
args!(A, B, C);
args!(A, B, C, D);
args!(A, B, C, D, E);
args!(A, B, C, D, E, F);
args!(A, B, C, D, E, F, G);
args!(A, B, C, D, E, F, G, H);

#[cfg(test)]
mod tests {
    use colibri_interp::{Fiber, Interp};

    use super::*;
    use crate::ctx::Ctx;

    /// Runs `f` with a context over a bare interpreter (no program is loaded).
    fn with_cx<T>(f: impl FnOnce(&mut Cx<'_>) -> T) -> T {
        let mut interp = Interp::new(Box::new(std::io::sink()), 100);
        let mut fiber = Fiber::new();
        f(&mut Ctx::new(&mut interp, &mut fiber))
    }

    fn round<T: IntoColibri + FromColibri>(v: T) -> Result<T, Error> {
        with_cx(|cx| {
            let value = v.into_colibri(cx);
            T::from_colibri(&value, cx)
        })
    }

    #[test]
    fn scalars_round_trip() {
        assert_eq!(round(5i64).ok(), Some(5));
        assert_eq!(round(1.5f64).ok(), Some(1.5));
        assert_eq!(round(true).ok(), Some(true));
        assert_eq!(round('x').ok(), Some('x'));
        assert_eq!(round("hi".to_string()).ok().as_deref(), Some("hi"));
        assert!(round(()).is_ok());
    }

    #[test]
    fn sized_numbers_round_trip() {
        assert_eq!(round(255u8).ok(), Some(255));
        assert_eq!(round(-128i8).ok(), Some(-128));
        assert_eq!(round(u32::MAX).ok(), Some(u32::MAX));
        assert_eq!(round(u64::MAX).ok(), Some(u64::MAX));
        assert_eq!(round(0.1f32).ok(), Some(0.1f32));
        assert_eq!(round(7usize).ok(), Some(7));
        assert_eq!(round(-7isize).ok(), Some(-7));
        let mut reg = TypeRegistry::new();
        assert_eq!(<u16 as IntoColibri>::ty(&mut reg).to_string(), "u16");
        with_cx(|cx| {
            assert!(u8::from_colibri(&300i64.into_colibri(cx), cx).is_err());
            assert!(usize::from_colibri(&(-1i64).into_colibri(cx), cx).is_err());
            assert!(i64::from_colibri(&1u8.into_colibri(cx), cx).is_ok());
            assert!(f64::from_colibri(&1f32.into_colibri(cx), cx).is_err());
        });
    }

    #[test]
    fn wrong_variants_are_errors_not_panics() {
        with_cx(|cx| {
            assert!(i64::from_colibri(&"x".into_colibri(cx), cx).is_err());
            assert!(String::from_colibri(&1i64.into_colibri(cx), cx).is_err());
            assert!(Vec::<i64>::from_colibri(&1i64.into_colibri(cx), cx).is_err());
        });
    }

    #[test]
    fn collections_options_and_tuples_round_trip() {
        assert_eq!(round(vec![1i64, 2, 3]).ok(), Some(vec![1, 2, 3]));
        assert_eq!(round(Some(3u8)).ok(), Some(Some(3)));
        assert_eq!(round(None::<i64>).ok(), Some(None));
        assert_eq!(
            round((1i64, "a".to_string())).ok(),
            Some((1, "a".to_string()))
        );
        let map: BTreeMap<String, i64> = [("a".to_string(), 1), ("b".to_string(), 2)].into();
        assert_eq!(round(map.clone()).ok(), Some(map));
        let nested = vec![Some(vec![1.5f64]), None];
        assert_eq!(round(nested.clone()).ok(), Some(nested));
    }

    #[test]
    fn a_hash_map_goes_out_in_key_order() {
        let map: HashMap<i64, i64> = (0..20).map(|i| (19 - i, i)).collect();
        with_cx(|cx| {
            let value = map.into_colibri(cx);
            let keys: Vec<i64> = BTreeMap::<i64, i64>::from_colibri(&value, cx)
                .unwrap()
                .into_keys()
                .collect();
            assert_eq!(keys, (0..20).collect::<Vec<_>>());
            let Inner::Map(m) = &value.0 else { panic!() };
            let order: Vec<Inner> = m.keys();
            assert_eq!(order[0], Inner::Int(0));
            assert_eq!(order[19], Inner::Int(19));
        });
    }

    #[test]
    fn descriptions_nest() {
        let mut reg = TypeRegistry::new();
        assert_eq!(
            <Vec<Option<(i64, String)>> as IntoColibri>::ty(&mut reg).to_string(),
            "List[Option[(int, str)]]"
        );
        assert_eq!(
            <Result<BTreeMap<String, f32>, String> as IntoColibri>::ty(&mut reg).to_string(),
            "Result[Map[str, f32]]"
        );
        assert_eq!(<(i64, &str) as Args>::tys(&mut reg).len(), 2);
        assert!(<() as Args>::tys(&mut reg).is_empty());
    }

    #[test]
    fn a_rust_err_becomes_a_colibri_err() {
        with_cx(|cx| {
            let ok = Ok::<i64, String>(1).into_colibri(cx);
            assert_eq!(Result::<i64, String>::from_colibri(&ok, cx).unwrap(), Ok(1));
            let err = Err::<i64, String>("bad".into()).into_colibri(cx);
            assert_eq!(
                Result::<i64, String>::from_colibri(&err, cx).unwrap(),
                Err("bad".to_string())
            );
            cx.release(err);
        });
    }
}
