//! Moving values between Rust and Colibri.

use std::fmt;

use colibri_check::types::Ty;
use colibri_interp::Value as Inner;

use crate::error::Error;

/// A Colibri value in flight between Rust and a script. Opaque: convert with
/// [`IntoColibri`] and [`FromColibri`].
#[derive(Clone, Debug)]
pub struct Value(pub(crate) Inner);

/// The Colibri types a Rust value can stand for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Type {
    Unit,
    Bool,
    Int,
    Float,
    Char,
    Str,
}

impl Type {
    /// The type of a checked Colibri signature entry, if the embedding API supports it.
    pub(crate) fn of(ty: &Ty) -> Option<Type> {
        Some(match ty {
            Ty::Unit => Type::Unit,
            Ty::Bool => Type::Bool,
            Ty::Int(colibri_check::types::IntTy::I64) => Type::Int,
            Ty::Float => Type::Float,
            Ty::Char => Type::Char,
            Ty::Str => Type::Str,
            _ => return None,
        })
    }
}

impl fmt::Display for Type {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Type::Unit => "()",
            Type::Bool => "bool",
            Type::Int => "int",
            Type::Float => "float",
            Type::Char => "char",
            Type::Str => "str",
        })
    }
}

/// A Rust type that can be passed to Colibri.
pub trait IntoColibri {
    /// The Colibri type this becomes.
    fn colibri_type() -> Type;
    fn into_colibri(self) -> Value;
}

/// A Rust type that a Colibri value can be turned into.
pub trait FromColibri: Sized {
    /// The Colibri type this is read from.
    fn colibri_type() -> Type;
    fn from_colibri(value: Value) -> Result<Self, Error>;
}

/// A tuple of Rust values passed as the arguments of a call.
pub trait Args {
    /// The Colibri types of the arguments, in order.
    fn colibri_types() -> Vec<Type>;
    fn into_values(self) -> Vec<Value>;
}

fn wrong(expected: Type, got: &Inner) -> Error {
    Error::Internal(format!("expected a `{expected}` value, got {got:?}"))
}

macro_rules! scalar {
    ($rust:ty, $ty:expr, $variant:ident) => {
        impl IntoColibri for $rust {
            fn colibri_type() -> Type {
                $ty
            }
            fn into_colibri(self) -> Value {
                Value(Inner::$variant(self))
            }
        }
        impl FromColibri for $rust {
            fn colibri_type() -> Type {
                $ty
            }
            fn from_colibri(value: Value) -> Result<Self, Error> {
                match value.0 {
                    Inner::$variant(v) => Ok(v),
                    other => Err(wrong($ty, &other)),
                }
            }
        }
    };
}

scalar!(i64, Type::Int, Int);
scalar!(f64, Type::Float, Float);
scalar!(bool, Type::Bool, Bool);
scalar!(char, Type::Char, Char);

impl IntoColibri for () {
    fn colibri_type() -> Type {
        Type::Unit
    }
    fn into_colibri(self) -> Value {
        Value(Inner::Unit)
    }
}

impl FromColibri for () {
    fn colibri_type() -> Type {
        Type::Unit
    }
    fn from_colibri(value: Value) -> Result<Self, Error> {
        match value.0 {
            Inner::Unit => Ok(()),
            other => Err(wrong(Type::Unit, &other)),
        }
    }
}

impl IntoColibri for &str {
    fn colibri_type() -> Type {
        Type::Str
    }
    fn into_colibri(self) -> Value {
        Value(Inner::str(self))
    }
}

impl IntoColibri for String {
    fn colibri_type() -> Type {
        Type::Str
    }
    fn into_colibri(self) -> Value {
        Value(Inner::str(&self))
    }
}

impl FromColibri for String {
    fn colibri_type() -> Type {
        Type::Str
    }
    fn from_colibri(value: Value) -> Result<Self, Error> {
        match value.0 {
            Inner::Str(s) => Ok(s.to_string()),
            other => Err(wrong(Type::Str, &other)),
        }
    }
}

macro_rules! args {
    ($($name:ident),*) => {
        impl<$($name: IntoColibri),*> Args for ($($name,)*) {
            fn colibri_types() -> Vec<Type> {
                vec![$($name::colibri_type()),*]
            }
            #[allow(non_snake_case)]
            fn into_values(self) -> Vec<Value> {
                let ($($name,)*) = self;
                vec![$($name.into_colibri()),*]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalars_round_trip() {
        assert_eq!(i64::from_colibri(5i64.into_colibri()).ok(), Some(5));
        assert_eq!(f64::from_colibri(1.5.into_colibri()).ok(), Some(1.5));
        assert_eq!(bool::from_colibri(true.into_colibri()).ok(), Some(true));
        assert_eq!(char::from_colibri('x'.into_colibri()).ok(), Some('x'));
        assert_eq!(
            String::from_colibri("hi".into_colibri()).ok().as_deref(),
            Some("hi")
        );
        assert!(<()>::from_colibri(().into_colibri()).is_ok());
    }

    #[test]
    fn wrong_variants_are_errors_not_panics() {
        assert!(i64::from_colibri("x".into_colibri()).is_err());
        assert!(String::from_colibri(1i64.into_colibri()).is_err());
    }

    #[test]
    fn tuples_are_argument_lists() {
        assert_eq!(<(i64, &str)>::colibri_types(), [Type::Int, Type::Str]);
        assert_eq!(<()>::colibri_types(), []);
        assert_eq!((1i64, 2.5f64, "x").into_values().len(), 3);
    }
}
