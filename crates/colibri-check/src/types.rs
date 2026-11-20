//! The type language of the checker.
//!
//! [`Ty`] is shaped for the whole language (structs, enums, classes, tuples, collections,
//! `Option`/`Result`, functions, type parameters) even though only the primitive types are
//! produced today, so later slices add checking rules instead of reshaping the type.

use std::fmt;
use std::rc::Rc;

use crate::hir::TypeDef;

/// Identifies a user-defined struct, enum, class or interface: an index into
/// [`crate::hir::Program::types`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TypeId(pub u32);

/// The prelude `Option` enum is always the first entry of [`crate::hir::Program::types`]; its
/// variants are `none` (0) and `some` (1), so `Ty::Option(T)` values and patterns use the
/// ordinary enum machinery.
pub const OPTION_TYPE: TypeId = TypeId(0);

/// The prelude `Result[T, E]` enum follows `Option` (DESIGN §10.1); it is an ordinary generic
/// enum, so `Ty::Adt(RESULT_TYPE, [T, E])` values, patterns, equality, `Show` and
/// exhaustiveness use the enum machinery. `Result[T]` is `Result[T, Error]`.
pub const RESULT_TYPE: TypeId = TypeId(1);
/// The built-in interfaces follow `Result` in the type table, in the order of the prelude
/// source (`crate::prelude`), so their ids are constants (DESIGN §9.2, §7.4).
pub const EQ: TypeId = TypeId(2);
pub const ORD: TypeId = TypeId(3);
pub const HASH: TypeId = TypeId(4);
pub const SHOW: TypeId = TypeId(5);
pub const CLONE: TypeId = TypeId(6);
pub const DROP: TypeId = TypeId(7);
pub const ITER: TypeId = TypeId(8);
pub const ITERABLE: TypeId = TypeId(9);
pub const INTO_ERROR: TypeId = TypeId(10);
/// The built-in `Error` (DESIGN §10.1): a reference type, declared in the prelude as an
/// empty `class` so the checker treats it as one (identity equality, no fields to build).
pub const ERROR_TYPE: TypeId = TypeId(11);
/// The built-in `Coroutine[T]` (DESIGN §14.2): a reference type declared in the prelude as an
/// empty generic `class`, like `Error`. `T` is the type the coroutine's function returns
/// (what `join()` gives back).
pub const COROUTINE_TYPE: TypeId = TypeId(12);
/// The built-in `CoGroup` (DESIGN §14.2): a reference type that owns coroutines and cancels
/// them when it dies.
pub const COGROUP_TYPE: TypeId = TypeId(13);
/// The built-in `Signal` (DESIGN §14.2): a reference type coroutines wait on with `wait_for`.
pub const SIGNAL_TYPE: TypeId = TypeId(14);
/// The built-in `StrBuilder` (DESIGN §12): a reference type that builds a string by appending.
pub const STR_BUILDER_TYPE: TypeId = TypeId(15);
/// The prelude classes that only built-in functions create: how to make one, if it can be made.
pub fn prelude_class_maker(id: TypeId) -> Option<&'static str> {
    match id {
        COROUTINE_TYPE => Some("`go f(..)`"),
        COGROUP_TYPE => Some("`CoGroup()`"),
        SIGNAL_TYPE => Some("`Signal()`"),
        STR_BUILDER_TYPE => Some("`StrBuilder()`"),
        _ => None,
    }
}

/// The first id a user-defined type can have.
pub const FIRST_USER_TYPE: u32 = 16;
/// The index of `ok` in the prelude `Result`.
pub const VARIANT_OK: u32 = 0;
/// The index of `err` in the prelude `Result`.
pub const VARIANT_ERR: u32 = 1;
/// The index of `none` in the prelude `Option`.
pub const VARIANT_NONE: u32 = 0;
/// The index of `some` in the prelude `Option`.
pub const VARIANT_SOME: u32 = 1;

/// Identifies a generic type parameter. Ids are unique across the whole program, so a type
/// that mentions a parameter says which declaration it belongs to and substitution is a plain
/// lookup (never a positional guess).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ParamId(pub u32);

/// An inference variable; meaningful only within the function body that created it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TyVar(pub u32);

/// Integer widths. `int` and `i64` are the same type, [`IntTy::I64`] (like `float` and `f64`);
/// integer operations carry their width so overflow traps are static (DESIGN §5.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum IntTy {
    I8,
    I16,
    I32,
    I64,
    U8,
    U16,
    U32,
    U64,
}

impl IntTy {
    /// The type's name as written in source (`int` for `I64`).
    pub fn name(self) -> &'static str {
        match self {
            IntTy::I8 => "i8",
            IntTy::I16 => "i16",
            IntTy::I32 => "i32",
            IntTy::I64 => "int",
            IntTy::U8 => "u8",
            IntTy::U16 => "u16",
            IntTy::U32 => "u32",
            IntTy::U64 => "u64",
        }
    }

    /// Every integer type, narrowest signed first.
    pub const ALL: [IntTy; 8] = [
        IntTy::I8,
        IntTy::I16,
        IntTy::I32,
        IntTy::I64,
        IntTy::U8,
        IntTy::U16,
        IntTy::U32,
        IntTy::U64,
    ];

    /// The integer type a name written in source stands for (`int` and `i64` are one type).
    pub fn from_name(name: &str) -> Option<IntTy> {
        Some(match name {
            "i8" => IntTy::I8,
            "i16" => IntTy::I16,
            "i32" => IntTy::I32,
            "int" | "i64" => IntTy::I64,
            "u8" => IntTy::U8,
            "u16" => IntTy::U16,
            "u32" => IntTy::U32,
            "u64" => IntTy::U64,
            _ => return None,
        })
    }

    /// The width in bits.
    pub fn bits(self) -> u32 {
        match self {
            IntTy::I8 | IntTy::U8 => 8,
            IntTy::I16 | IntTy::U16 => 16,
            IntTy::I32 | IntTy::U32 => 32,
            IntTy::I64 | IntTy::U64 => 64,
        }
    }

    pub fn is_signed(self) -> bool {
        matches!(self, IntTy::I8 | IntTy::I16 | IntTy::I32 | IntTy::I64)
    }

    /// The smallest value of the type.
    pub fn min(self) -> i128 {
        if self.is_signed() {
            -(1i128 << (self.bits() - 1))
        } else {
            0
        }
    }

    /// The largest value of the type.
    pub fn max(self) -> i128 {
        if self.is_signed() {
            (1i128 << (self.bits() - 1)) - 1
        } else {
            (1i128 << self.bits()) - 1
        }
    }

    /// Whether `v` is a value of the type.
    pub fn fits(self, v: i128) -> bool {
        (self.min()..=self.max()).contains(&v)
    }
}

/// A built-in vector type: `vec2`..`vec4` (`f32` components) or `ivec2`..`ivec4` (`i32`
/// components), DESIGN §15.1. A plain value, copied by value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct VecTy {
    /// Whether the components are `i32` (`ivecN`) rather than `f32` (`vecN`).
    pub int: bool,
    /// The number of components, 2 to 4.
    pub n: u8,
}

impl VecTy {
    /// Every vector type.
    pub const ALL: [VecTy; 6] = [
        VecTy { int: false, n: 2 },
        VecTy { int: false, n: 3 },
        VecTy { int: false, n: 4 },
        VecTy { int: true, n: 2 },
        VecTy { int: true, n: 3 },
        VecTy { int: true, n: 4 },
    ];

    /// The type a name written in source stands for.
    pub fn from_name(name: &str) -> Option<VecTy> {
        VecTy::ALL.into_iter().find(|v| v.name() == name)
    }

    /// The type's name as written in source.
    pub fn name(self) -> &'static str {
        match (self.int, self.n) {
            (false, 2) => "vec2",
            (false, 3) => "vec3",
            (false, _) => "vec4",
            (true, 2) => "ivec2",
            (true, 3) => "ivec3",
            (true, _) => "ivec4",
        }
    }

    /// The type of one component.
    pub fn scalar(self) -> Ty {
        if self.int {
            Ty::Int(IntTy::I32)
        } else {
            Ty::F32
        }
    }

    /// The vector type of the same kind with `n` components.
    pub fn with_len(self, n: u8) -> VecTy {
        VecTy { n, ..self }
    }

    /// The same number of components of the other kind (`vec3` and `ivec3`).
    pub fn other_kind(self) -> VecTy {
        VecTy {
            int: !self.int,
            n: self.n,
        }
    }
}

/// A Colibri type.
#[derive(Clone, Debug, PartialEq)]
pub enum Ty {
    /// `vec2`..`vec4`, `ivec2`..`ivec4`.
    Vec(VecTy),
    Int(IntTy),
    /// `float` (`f64`).
    Float,
    /// `f32`; widens to `float` implicitly (DESIGN §5.1).
    F32,
    Bool,
    Char,
    Str,
    /// `()`
    Unit,
    /// The type of `return`, `break` and `continue`; converts to every type.
    Never,
    Tuple(Vec<Ty>),
    /// A struct, enum or class with its type arguments.
    Adt(TypeId, Vec<Ty>),
    List(Box<Ty>),
    Map(Box<Ty>, Box<Ty>),
    Set(Box<Ty>),
    Option(Box<Ty>),
    Fn(Vec<Ty>, Box<Ty>),
    /// `ptr[T]`: a raw C pointer to a `T` (DESIGN §15.3). Safe code can pass, store and compare
    /// it; reading, writing and arithmetic need `unsafe`.
    Ptr(Box<Ty>),
    /// `Range[T]`: a closed or half-open range of integers, `a..b` or `a..=b` (DESIGN §5.2).
    Range(Box<Ty>),
    /// An interface used as a type: a dynamically dispatched value (DESIGN §9.2), with the
    /// interface's type arguments.
    Dyn(TypeId, Vec<Ty>),
    /// A type parameter, opaque inside the generic declaration that owns it; the name is for
    /// messages.
    Param(ParamId, Rc<str>),
    /// An inference variable, resolved through the function's [`crate::infer::InferCtx`].
    Infer(TyVar),
    /// A type that already caused a diagnostic; it unifies with everything so one mistake
    /// reports once.
    Error,
}

impl Ty {
    /// `int`, the default integer type.
    pub const INT: Ty = Ty::Int(IntTy::I64);

    pub fn is_error(&self) -> bool {
        matches!(self, Ty::Error)
    }

    /// The numeric type a primitive type name stands for: `int`, `i8`..`u64`, `float`, `f32`.
    pub fn numeric_by_name(name: &str) -> Option<Ty> {
        match name {
            "float" | "f64" => Some(Ty::Float),
            "f32" => Some(Ty::F32),
            _ => IntTy::from_name(name).map(Ty::Int),
        }
    }

    /// Whether the type is an integer or a float type.
    pub fn is_number(&self) -> bool {
        matches!(self, Ty::Int(_) | Ty::Float | Ty::F32)
    }
}

impl Ty {
    /// Replaces type parameters by the types `map` gives them.
    pub fn subst(&self, map: &[(ParamId, Ty)]) -> Ty {
        if map.is_empty() {
            return self.clone();
        }
        let go = |t: &Ty| t.subst(map);
        match self {
            Ty::Param(p, _) => map
                .iter()
                .find(|(q, _)| q == p)
                .map_or_else(|| self.clone(), |(_, t)| t.clone()),
            Ty::Tuple(ts) => Ty::Tuple(ts.iter().map(go).collect()),
            Ty::Adt(id, ts) => Ty::Adt(*id, ts.iter().map(go).collect()),
            Ty::Dyn(id, ts) => Ty::Dyn(*id, ts.iter().map(go).collect()),
            Ty::List(t) => Ty::List(Box::new(go(t))),
            Ty::Set(t) => Ty::Set(Box::new(go(t))),
            Ty::Option(t) => Ty::Option(Box::new(go(t))),
            Ty::Range(t) => Ty::Range(Box::new(go(t))),
            Ty::Ptr(t) => Ty::Ptr(Box::new(go(t))),
            Ty::Map(k, v) => Ty::Map(Box::new(go(k)), Box::new(go(v))),
            Ty::Fn(ps, r) => Ty::Fn(ps.iter().map(go).collect(), Box::new(go(r))),
            _ => self.clone(),
        }
    }

    /// Whether the type mentions a type parameter.
    pub fn has_param(&self) -> bool {
        match self {
            Ty::Param(..) => true,
            Ty::Tuple(ts) | Ty::Adt(_, ts) | Ty::Dyn(_, ts) => ts.iter().any(Ty::has_param),
            Ty::List(t) | Ty::Set(t) | Ty::Option(t) | Ty::Range(t) | Ty::Ptr(t) => t.has_param(),
            Ty::Map(a, b) => a.has_param() || b.has_param(),
            Ty::Fn(ps, r) => ps.iter().any(Ty::has_param) || r.has_param(),
            _ => false,
        }
    }

    /// The type as written in source, with user-defined types shown by name. (`Display` has no
    /// access to the program's types and shows them as `type#N`.)
    pub fn render(&self, types: &[TypeDef]) -> String {
        Shown { ty: self, types }.to_string()
    }
}

/// The `T` of `Result[T, Error]`'s arguments, when the error type is the built-in `Error`.
fn default_error(args: &[Ty]) -> Option<&Ty> {
    match args {
        [ok, Ty::Adt(e, a)] if *e == ERROR_TYPE && a.is_empty() => Some(ok),
        _ => None,
    }
}

/// A type together with the names of the user-defined types it may mention.
struct Shown<'a> {
    ty: &'a Ty,
    types: &'a [TypeDef],
}

impl fmt::Display for Ty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Shown {
            ty: self,
            types: &[],
        }
        .fmt(f)
    }
}

impl fmt::Display for Shown<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let types = self.types;
        fn shown<'a>(ty: &'a Ty, types: &'a [TypeDef]) -> Shown<'a> {
            Shown { ty, types }
        }
        let list = |f: &mut fmt::Formatter<'_>, tys: &[Ty]| -> fmt::Result {
            for (i, ty) in tys.iter().enumerate() {
                if i > 0 {
                    f.write_str(", ")?;
                }
                write!(f, "{}", shown(ty, types))?;
            }
            Ok(())
        };
        match self.ty {
            Ty::Int(i) => f.write_str(i.name()),
            Ty::Float => f.write_str("float"),
            Ty::F32 => f.write_str("f32"),
            Ty::Vec(v) => f.write_str(v.name()),
            Ty::Bool => f.write_str("bool"),
            Ty::Char => f.write_str("char"),
            Ty::Str => f.write_str("str"),
            Ty::Unit => f.write_str("()"),
            Ty::Never => f.write_str("!"),
            Ty::Tuple(tys) => {
                f.write_str("(")?;
                list(f, tys)?;
                if tys.len() == 1 {
                    f.write_str(",")?;
                }
                f.write_str(")")
            }
            // `Result[T, Error]` is written `Result[T]`.
            Ty::Adt(id, args) if *id == RESULT_TYPE && default_error(args).is_some() => {
                let ok = default_error(args).unwrap_or(&Ty::Error);
                write!(f, "Result[{}]", shown(ok, types))
            }
            Ty::Adt(id, args) | Ty::Dyn(id, args) => {
                match types.get(id.0 as usize) {
                    Some(def) => f.write_str(&def.display_name())?,
                    None => write!(f, "type#{}", id.0)?,
                }
                if !args.is_empty() {
                    f.write_str("[")?;
                    list(f, args)?;
                    f.write_str("]")?;
                }
                Ok(())
            }
            Ty::List(t) => write!(f, "List[{}]", shown(t, types)),
            Ty::Map(k, v) => write!(f, "Map[{}, {}]", shown(k, types), shown(v, types)),
            Ty::Set(t) => write!(f, "Set[{}]", shown(t, types)),
            Ty::Option(t) => write!(f, "{}?", shown(t, types)),
            Ty::Range(t) => write!(f, "Range[{}]", shown(t, types)),
            Ty::Ptr(t) => write!(f, "ptr[{}]", shown(t, types)),
            Ty::Fn(params, ret) => {
                f.write_str("fn(")?;
                list(f, params)?;
                f.write_str(")")?;
                if **ret != Ty::Unit {
                    write!(f, " -> {}", shown(ret, types))?;
                }
                Ok(())
            }
            Ty::Param(_, name) => f.write_str(name),
            Ty::Infer(_) => f.write_str("_"),
            Ty::Error => f.write_str("{error}"),
        }
    }
}

/// A function's signature as the host sees it.
#[derive(Clone, Debug, PartialEq)]
pub struct FnSig {
    pub params: Vec<Ty>,
    pub ret: Ty,
}

impl FnSig {
    /// The signature as written in source, with user-defined types shown by name.
    pub fn render(&self, types: &[TypeDef]) -> String {
        Ty::Fn(self.params.clone(), Box::new(self.ret.clone())).render(types)
    }
}

impl fmt::Display for FnSig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}",
            Ty::Fn(self.params.clone(), Box::new(self.ret.clone()))
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_follows_source_syntax() {
        assert_eq!(Ty::INT.to_string(), "int");
        assert_eq!(Ty::Unit.to_string(), "()");
        assert_eq!(Ty::Tuple(vec![Ty::Int(IntTy::I64)]).to_string(), "(int,)");
        assert_eq!(
            Ty::Tuple(vec![Ty::Float, Ty::Str]).to_string(),
            "(float, str)"
        );
        assert_eq!(Ty::Option(Box::new(Ty::Bool)).to_string(), "bool?");
        assert_eq!(
            Ty::Map(Box::new(Ty::Str), Box::new(Ty::List(Box::new(Ty::INT)))).to_string(),
            "Map[str, List[int]]"
        );
        let sig = FnSig {
            params: vec![Ty::INT, Ty::INT],
            ret: Ty::INT,
        };
        assert_eq!(sig.to_string(), "fn(int, int) -> int");
        let unit = FnSig {
            params: vec![],
            ret: Ty::Unit,
        };
        assert_eq!(unit.to_string(), "fn()");
    }
}
