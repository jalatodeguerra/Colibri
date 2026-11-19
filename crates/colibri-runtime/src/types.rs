//! A structural description of the Colibri types the embedding API moves values through
//! (EMBEDDING §0 B), shared by `get_fn`, `register_fn` and the interface text.

use std::fmt;

use std::any::TypeId as AnyTypeId;

use colibri_check::hir::{Program, TypeDefKind};
use colibri_check::host::DEFAULT_MODULE;
use colibri_check::types::{ERROR_TYPE, FIRST_USER_TYPE, IntTy, RESULT_TYPE, Ty, VecTy};

use crate::class::{HostType, TypeBuilder};
use crate::host::RawHostFn;

/// A Colibri type, as far as the embedding API can express it.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum TypeDesc {
    Unit,
    Bool,
    /// `int`, which is also `i64`.
    Int,
    I8,
    I16,
    I32,
    U8,
    U16,
    U32,
    U64,
    /// `float`, which is also `f64`.
    Float,
    F32,
    Char,
    Str,
    /// A vector type of the `vec` module (`vec.vec3`, `vec.ivec2`); glam's `Vec3` and `IVec2`
    /// are its Rust side.
    Vec(VecTy),
    List(Box<TypeDesc>),
    Option(Box<TypeDesc>),
    /// `Result[T]`, whose error is the built-in `Error`.
    Result(Box<TypeDesc>),
    /// Two to four elements.
    Tuple(Vec<TypeDesc>),
    Map(Box<TypeDesc>, Box<TypeDesc>),
    /// A function or closure: parameters and result.
    Fn(Vec<TypeDesc>, Box<TypeDesc>),
    /// A `struct` or `enum` the host declared, by name (see [`TypeRegistry`]).
    Value(String),
    /// A class of the host: objects the host owns and scripts only hold (EMBEDDING §3).
    Host(String),
    /// A type parameter of the function (`T` in `fn decode[T](text: str) -> Result[T]`): what
    /// a [`crate::Value`] stands for in a signature.
    Param(String),
}

impl fmt::Display for TypeDesc {
    /// The type as it is written in Colibri source (and in an interface file).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let list = |items: &[TypeDesc]| {
            items
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        };
        match self {
            TypeDesc::Unit => f.write_str("()"),
            TypeDesc::Bool => f.write_str("bool"),
            TypeDesc::Int => f.write_str("int"),
            TypeDesc::I8 => f.write_str("i8"),
            TypeDesc::I16 => f.write_str("i16"),
            TypeDesc::I32 => f.write_str("i32"),
            TypeDesc::U8 => f.write_str("u8"),
            TypeDesc::U16 => f.write_str("u16"),
            TypeDesc::U32 => f.write_str("u32"),
            TypeDesc::U64 => f.write_str("u64"),
            TypeDesc::Float => f.write_str("float"),
            TypeDesc::F32 => f.write_str("f32"),
            TypeDesc::Char => f.write_str("char"),
            TypeDesc::Str => f.write_str("str"),
            TypeDesc::Vec(v) => write!(f, "vec.{}", v.name()),
            TypeDesc::List(t) => write!(f, "List[{t}]"),
            TypeDesc::Option(t) => write!(f, "Option[{t}]"),
            TypeDesc::Result(t) => write!(f, "Result[{t}]"),
            TypeDesc::Tuple(items) => write!(f, "({})", list(items)),
            TypeDesc::Map(k, v) => write!(f, "Map[{k}, {v}]"),
            TypeDesc::Fn(params, ret) => {
                write!(f, "fn({})", list(params))?;
                if **ret != TypeDesc::Unit {
                    write!(f, " -> {ret}")?;
                }
                Ok(())
            }
            TypeDesc::Value(name) | TypeDesc::Host(name) | TypeDesc::Param(name) => {
                f.write_str(name)
            }
        }
    }
}

impl TypeDesc {
    /// The names of the type parameters this type mentions, each once.
    pub(crate) fn params(&self, out: &mut Vec<String>) {
        match self {
            TypeDesc::List(t) | TypeDesc::Option(t) | TypeDesc::Result(t) => t.params(out),
            TypeDesc::Tuple(items) => items.iter().for_each(|t| t.params(out)),
            TypeDesc::Map(k, v) => {
                k.params(out);
                v.params(out);
            }
            TypeDesc::Fn(params, ret) => {
                params.iter().for_each(|t| t.params(out));
                ret.params(out);
            }
            TypeDesc::Param(name) if !out.contains(name) => out.push(name.clone()),
            _ => {}
        }
    }

    /// The names of the structs, enums and host classes this type mentions.
    pub(crate) fn mentions<'a>(&'a self, out: &mut Vec<&'a str>) {
        match self {
            TypeDesc::List(t) | TypeDesc::Option(t) | TypeDesc::Result(t) => t.mentions(out),
            TypeDesc::Tuple(items) => items.iter().for_each(|t| t.mentions(out)),
            TypeDesc::Map(k, v) => {
                k.mentions(out);
                v.mentions(out);
            }
            TypeDesc::Fn(params, ret) => {
                params.iter().for_each(|t| t.mentions(out));
                ret.mentions(out);
            }
            TypeDesc::Value(name) | TypeDesc::Host(name) => out.push(name),
            _ => {}
        }
    }

    /// The description of a checked Colibri type, if the embedding API can express it. `program`
    /// says which structs and enums are the host's own.
    pub(crate) fn of(ty: &Ty, program: &Program) -> Option<TypeDesc> {
        let of = |t: &Ty| TypeDesc::of(t, program);
        Some(match ty {
            Ty::Unit => TypeDesc::Unit,
            Ty::Bool => TypeDesc::Bool,
            Ty::Int(IntTy::I64) => TypeDesc::Int,
            Ty::Int(IntTy::I8) => TypeDesc::I8,
            Ty::Int(IntTy::I16) => TypeDesc::I16,
            Ty::Int(IntTy::I32) => TypeDesc::I32,
            Ty::Int(IntTy::U8) => TypeDesc::U8,
            Ty::Int(IntTy::U16) => TypeDesc::U16,
            Ty::Int(IntTy::U32) => TypeDesc::U32,
            Ty::Int(IntTy::U64) => TypeDesc::U64,
            Ty::Float => TypeDesc::Float,
            Ty::F32 => TypeDesc::F32,
            Ty::Char => TypeDesc::Char,
            Ty::Str => TypeDesc::Str,
            Ty::Vec(v) => TypeDesc::Vec(*v),
            Ty::List(t) => TypeDesc::List(Box::new(of(t)?)),
            Ty::Option(t) => TypeDesc::Option(Box::new(of(t)?)),
            Ty::Tuple(items) => TypeDesc::Tuple(items.iter().map(of).collect::<Option<Vec<_>>>()?),
            Ty::Map(k, v) => TypeDesc::Map(Box::new(of(k)?), Box::new(of(v)?)),
            Ty::Fn(params, ret) => TypeDesc::Fn(
                params.iter().map(of).collect::<Option<Vec<_>>>()?,
                Box::new(of(ret)?),
            ),
            Ty::Adt(RESULT_TYPE, args) => match args.as_slice() {
                [t, Ty::Adt(ERROR_TYPE, _)] => TypeDesc::Result(Box::new(of(t)?)),
                _ => return None,
            },
            Ty::Adt(id, args) if args.is_empty() => {
                let index = id.0.checked_sub(FIRST_USER_TYPE)?;
                if index >= program.host_types {
                    return None;
                }
                let def = &program.types[id.0 as usize];
                match def.kind {
                    TypeDefKind::Struct { .. } | TypeDefKind::Enum { .. } => {
                        TypeDesc::Value(def.name.clone())
                    }
                    TypeDefKind::HostClass => TypeDesc::Host(def.name.clone()),
                    TypeDefKind::Class { .. } | TypeDefKind::Interface { .. } => return None,
                }
            }
            _ => return None,
        })
    }
}

/// The signature of a host function.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SigDesc {
    pub params: Vec<TypeDesc>,
    pub ret: TypeDesc,
}

/// A struct, enum or class the host declares.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TypeDecl {
    Struct {
        name: String,
        fields: Vec<(String, TypeDesc)>,
    },
    /// A `enum` whose variants carry no payload.
    Enum { name: String, variants: Vec<String> },
    /// A class of host objects. Its methods are in [`TypeRegistry::classes`].
    Class { name: String },
}

impl TypeDecl {
    pub(crate) fn name(&self) -> &str {
        match self {
            TypeDecl::Struct { name, .. }
            | TypeDecl::Enum { name, .. }
            | TypeDecl::Class { name } => name,
        }
    }
}

/// The types a `Vm`'s host functions mention, in the order they were first described. That
/// order is the order of the type declarations in the interface file, and so their type ids.
#[derive(Clone, Debug, Default)]
pub struct TypeRegistry {
    pub(crate) decls: Vec<TypeDecl>,
    /// The host module each declaration belongs to, parallel to `decls`: the one that was
    /// being registered into when the type was first described (`None` is the default).
    pub(crate) decl_modules: Vec<Option<String>>,
    /// The module registrations go into now; `None` is the default `host` module.
    pub(crate) current_module: Option<String>,
    /// The host classes described through [`HostType`], with their methods.
    pub(crate) classes: Vec<ClassEntry>,
    /// Types described twice in different ways.
    pub(crate) conflicts: Vec<String>,
}

impl TypeRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// The module a type belongs to; the default module for one that is not known.
    pub(crate) fn module_of(&self, type_name: &str) -> &str {
        self.decls
            .iter()
            .position(|d| d.name() == type_name)
            .and_then(|i| self.decl_modules[i].as_deref())
            .unwrap_or(DEFAULT_MODULE)
    }

    fn define(&mut self, decl: TypeDecl) {
        match self.decls.iter().find(|d| d.name() == decl.name()) {
            None => {
                self.decls.push(decl);
                self.decl_modules.push(self.current_module.clone());
            }
            Some(existing) if *existing == decl => {}
            Some(_) => self.conflicts.push(format!(
                "the type `{}` is described in two different ways",
                decl.name()
            )),
        }
    }

    /// Declares a Colibri `struct` with these fields and returns the type to use in
    /// signatures. Describing it again identically is fine; differently is an error reported
    /// when the function is registered.
    pub fn define_struct(&mut self, name: &str, fields: Vec<(&str, TypeDesc)>) -> TypeDesc {
        self.define(TypeDecl::Struct {
            name: name.to_string(),
            fields: fields
                .into_iter()
                .map(|(n, t)| (n.to_string(), t))
                .collect(),
        });
        TypeDesc::Value(name.to_string())
    }

    /// Declares a Colibri `enum` whose variants carry no payload.
    pub fn define_enum(&mut self, name: &str, variants: &[&str]) -> TypeDesc {
        self.define(TypeDecl::Enum {
            name: name.to_string(),
            variants: variants.iter().map(ToString::to_string).collect(),
        });
        TypeDesc::Value(name.to_string())
    }

    /// Declares the host class `T` and its methods (what [`HostType::describe`] says) and
    /// returns the type to use in signatures. Describing it again is a no-op, so a class that
    /// several signatures mention (or that mentions itself) is declared once; a different Rust
    /// type with the same name is a conflict, reported when the function is registered.
    pub fn define_host_type<T: HostType>(&mut self) -> TypeDesc {
        let name = T::NAME;
        let any = AnyTypeId::of::<T>();
        let ty = TypeDesc::Host(name.to_string());
        if let Some(existing) = self.classes.iter().find(|c| c.name == name) {
            if existing.any != any {
                self.conflicts.push(format!(
                    "the host class `{name}` is described by two different Rust types"
                ));
            }
            return ty;
        }
        self.define(TypeDecl::Class {
            name: name.to_string(),
        });
        // Entered before its methods are described, so a method that mentions the class itself
        // (or a class that mentions this one) finds it already there.
        let index = self.classes.len();
        self.classes.push(ClassEntry {
            name: name.to_string(),
            any,
            module: self.current_module.clone(),
            methods: Vec::new(),
        });
        let mut builder = TypeBuilder::<T>::new(self);
        T::describe(&mut builder);
        let methods = builder.finish();
        self.classes[index].methods = methods;
        ty
    }
}

/// A host class: its Rust type and its methods, in the order they were described.
#[derive(Clone, Debug)]
pub(crate) struct ClassEntry {
    pub name: String,
    pub any: AnyTypeId,
    /// `None` is the default module.
    pub module: Option<String>,
    pub methods: Vec<MethodEntry>,
}

/// A method or static function of a host class.
#[derive(Clone)]
pub(crate) struct MethodEntry {
    pub name: String,
    /// The names of the parameters, not counting the receiver.
    pub params: Vec<String>,
    pub sig: SigDesc,
    /// Whether the first argument is the object (`self`).
    pub has_self: bool,
    pub raw: RawHostFn,
}

impl std::fmt::Debug for MethodEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MethodEntry")
            .field("name", &self.name)
            .field("params", &self.params)
            .field("sig", &self.sig)
            .field("has_self", &self.has_self)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn types_print_as_colibri_source() {
        let t = TypeDesc::Fn(
            vec![TypeDesc::Int, TypeDesc::List(Box::new(TypeDesc::Str))],
            Box::new(TypeDesc::Result(Box::new(TypeDesc::Tuple(vec![
                TypeDesc::Bool,
                TypeDesc::Option(Box::new(TypeDesc::F32)),
            ])))),
        );
        assert_eq!(
            t.to_string(),
            "fn(int, List[str]) -> Result[(bool, Option[f32])]"
        );
        assert_eq!(
            TypeDesc::Fn(Vec::new(), Box::new(TypeDesc::Unit)).to_string(),
            "fn()"
        );
    }

    #[test]
    fn redefining_a_type_differently_is_a_conflict() {
        let mut reg = TypeRegistry::new();
        reg.define_struct("V", vec![("x", TypeDesc::F32)]);
        reg.define_struct("V", vec![("x", TypeDesc::F32)]);
        assert!(reg.conflicts.is_empty());
        reg.define_struct("V", vec![("y", TypeDesc::F32)]);
        assert_eq!(reg.conflicts.len(), 1);
        assert_eq!(reg.decls.len(), 1);
    }
}
