//! The type language of the checker.
//!
//! [`Ty`] is shaped for the whole language (structs, enums, classes, tuples, collections,
//! `Option`/`Result`, functions, type parameters) even though only the primitive types are
//! produced today, so later slices add checking rules instead of reshaping the type.

use std::fmt;

use crate::hir::TypeDef;

/// Identifies a user-defined struct, enum, class or interface: an index into
/// [`crate::hir::Program::types`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TypeId(pub u32);

/// Identifies a generic type parameter within its declaration (not produced yet).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ParamId(pub u32);

/// An inference variable; meaningful only within the function body that created it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TyVar(pub u32);

/// Integer widths. `int` is [`IntTy::I64`]; the others exist so integer operations can carry
/// their width (DESIGN §5.1) and are not nameable in source yet.
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
}

/// A Colibri type.
#[derive(Clone, Debug, PartialEq)]
pub enum Ty {
    Int(IntTy),
    Float,
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
    Result(Box<Ty>, Box<Ty>),
    Fn(Vec<Ty>, Box<Ty>),
    Param(ParamId),
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
}

impl Ty {
    /// The type as written in source, with user-defined types shown by name. (`Display` has no
    /// access to the program's types and shows them as `type#N`.)
    pub fn render(&self, types: &[TypeDef]) -> String {
        Shown { ty: self, types }.to_string()
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
            Ty::Adt(id, args) => {
                match types.get(id.0 as usize) {
                    Some(def) => f.write_str(&def.name)?,
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
            Ty::Result(t, e) if **e == Ty::Error => write!(f, "Result[{}]", shown(t, types)),
            Ty::Result(t, e) => write!(f, "Result[{}, {}]", shown(t, types), shown(e, types)),
            Ty::Fn(params, ret) => {
                f.write_str("fn(")?;
                list(f, params)?;
                f.write_str(")")?;
                if **ret != Ty::Unit {
                    write!(f, " -> {}", shown(ret, types))?;
                }
                Ok(())
            }
            Ty::Param(p) => write!(f, "T{}", p.0),
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
