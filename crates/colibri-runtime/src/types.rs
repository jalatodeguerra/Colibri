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
    /// `!`, the result of a function that never returns (a return type only): see [`crate::Never`].
    Never,
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
    /// A `struct` or `enum` the host declared, by name (see [`TypeRegistry`]): the bare name
    /// for a type of the module being described or of the default `host` module, and
    /// `module.Name` for a type of another module (`json.Value`), as the interface writes it.
    Value(String),
    /// A class of the host: objects the host owns and scripts only hold (EMBEDDING §3). Named
    /// as [`TypeDesc::Value`] is.
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
            TypeDesc::Never => f.write_str("!"),
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

    /// This type with the name of every struct, enum and class in it replaced by what `f`
    /// says (it is given the name and whether it is a host class).
    pub(crate) fn map_names(&self, f: &dyn Fn(&str, bool) -> String) -> TypeDesc {
        let m = |t: &TypeDesc| t.map_names(f);
        match self {
            TypeDesc::List(t) => TypeDesc::List(Box::new(m(t))),
            TypeDesc::Option(t) => TypeDesc::Option(Box::new(m(t))),
            TypeDesc::Result(t) => TypeDesc::Result(Box::new(m(t))),
            TypeDesc::Tuple(items) => TypeDesc::Tuple(items.iter().map(m).collect()),
            TypeDesc::Map(k, v) => TypeDesc::Map(Box::new(m(k)), Box::new(m(v))),
            TypeDesc::Fn(params, ret) => {
                TypeDesc::Fn(params.iter().map(m).collect(), Box::new(m(ret)))
            }
            TypeDesc::Value(n) => TypeDesc::Value(f(n, false)),
            TypeDesc::Host(n) => TypeDesc::Host(f(n, true)),
            other => other.clone(),
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
            Ty::Never => TypeDesc::Never,
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
                // Seen from the default module: a type of a named module is `module.Name`.
                let shown = def.display_name().into_owned();
                match def.kind {
                    TypeDefKind::Struct { .. } | TypeDefKind::Enum { .. } => TypeDesc::Value(shown),
                    TypeDefKind::HostClass => TypeDesc::Host(shown),
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
    Enum {
        name: String,
        variants: Vec<String>,
        /// Belongs to the module that declares it, even when another module declares an enum
        /// of the same name.
        local: bool,
    },
    /// A class of host objects. Its methods are in [`TypeRegistry::classes`].
    Class { name: String },
}

impl TypeDecl {
    /// Whether this is an enum made with [`TypeRegistry::define_module_enum`].
    pub(crate) fn is_module_local(&self) -> bool {
        matches!(self, TypeDecl::Enum { local: true, .. })
    }

    pub(crate) fn name(&self) -> &str {
        match self {
            TypeDecl::Struct { name, .. }
            | TypeDecl::Enum { name, .. }
            | TypeDecl::Class { name } => name,
        }
    }
}

/// The last segment of a module's name: what a script writes for it (`json` for `std.json`).
pub(crate) fn short_module_name(module: &str) -> &str {
    module.rsplit('.').next().unwrap_or(module)
}

/// The types a `Vm`'s host functions mention, in the order they were first described. That
/// order is the order of the type declarations in the interface file, and so their type ids.
///
/// A type belongs to the module that described it and is known by that module and its name:
/// the `Child` of `std.process` and the `Child` of the default `host` module, or of a module
/// `game`, are different types.
#[derive(Clone, Debug, Default)]
pub struct TypeRegistry {
    pub(crate) decls: Vec<TypeDecl>,
    /// The host module each declaration belongs to, parallel to `decls`: the one that was
    /// being registered into when the type was first described (`None` is the default).
    pub(crate) decl_modules: Vec<Option<String>>,
    /// The module whose point of view the names inside each declaration (field types) were
    /// written from, parallel to `decls`: the module that described it first. The owner can
    /// change (see [`TypeRegistry::share`]); the names are resolved from here when the text
    /// is printed, never copied as written.
    pub(crate) decl_scope: Vec<Option<String>>,
    /// Every time a module described a type for the first time, `(module, declaration)`, in
    /// the order the registrations were made. A module's own calls come in the same order
    /// whatever the other modules did, so the position of its events is what orders its
    /// interface text.
    pub(crate) events: Vec<(String, usize)>,
    /// A type changed owner since the flag was last cleared: the interface text of the other
    /// modules changed too.
    pub(crate) moved: std::cell::Cell<bool>,
    /// The Rust type each declaration was described from, parallel to `decls`: two
    /// registrations are one Colibri type only when this is the same `TypeId`. `None` for a
    /// declaration made from interface text (`define_struct`), which is never shared.
    pub(crate) decl_rust: Vec<Option<AnyTypeId>>,
    /// The names of the Rust types in `decl_rust` that were told (`std::any::type_name`), for
    /// the messages that say which two types collided.
    pub(crate) rust_names: std::collections::HashMap<AnyTypeId, String>,
    /// The class that the last [`TypeRegistry::define_host_type`] described or found.
    pub(crate) last_class: Option<usize>,
    /// Names a module has for a type another module owns: `(module, name, declaration, via)`.
    /// The module registered the same Rust type, so `module.Name` is that type too; `via` is
    /// the module that owned it then (its signatures may have written `via.Name`), and the
    /// interface uses it.
    pub(crate) aliases: Vec<(String, String, usize, String)>,
    /// The module registrations go into now; `None` is the default `host` module.
    pub(crate) current_module: Option<String>,
    /// The host classes described through [`HostType`], with their methods.
    pub(crate) classes: Vec<ClassEntry>,
    /// The modules whose types each module's signatures may name (`HostModule::uses`), by
    /// module name.
    pub(crate) uses: Vec<(String, Vec<String>)>,
    /// Types described twice in different ways.
    pub(crate) conflicts: Vec<String>,
    /// The registry describes the host's side of a call into a loaded program (`get_fn`): a
    /// bare type name that the default module does not declare means the one named module
    /// that does (see [`TypeRegistry::define_as`]).
    pub(crate) host_side: bool,
}

impl TypeRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Tells the name of the Rust type `id` (`std::any::type_name`), so that a message about
    /// two different Rust types with one Colibri name can name both. What
    /// `#[colibri::export(value)]` does before it describes the type.
    pub fn name_rust_type(&mut self, id: AnyTypeId, name: &str) {
        self.rust_names
            .entry(id)
            .or_insert_with(|| name.to_string());
    }

    /// How a message names the Rust type `id`.
    fn rust_type(&self, id: Option<AnyTypeId>) -> String {
        id.and_then(|id| self.rust_names.get(&id))
            .map_or_else(|| "another Rust type".to_string(), |n| format!("`{n}`"))
    }

    /// The name of the module being described into (`host` is the default).
    pub(crate) fn current(&self) -> &str {
        self.current_module.as_deref().unwrap_or(DEFAULT_MODULE)
    }

    fn module_key(m: &Option<String>) -> &str {
        m.as_deref().unwrap_or(DEFAULT_MODULE)
    }

    /// The index of the declaration of `name` that module `module` made.
    pub(crate) fn find_decl(&self, module: &str, name: &str) -> Option<usize> {
        self.decls
            .iter()
            .zip(&self.decl_modules)
            .position(|(d, m)| d.name() == name && Self::module_key(m) == module)
    }

    /// The index of the declaration `module` can name `name` by: its own, or one it shares
    /// with the module that owns it.
    pub(crate) fn find_visible(&self, module: &str, name: &str) -> Option<usize> {
        self.find_decl(module, name).or_else(|| {
            self.aliases
                .iter()
                .find(|(m, n, _, _)| m == module && n == name)
                .map(|(_, _, i, _)| *i)
        })
    }

    /// The module that owns declaration `i`.
    pub(crate) fn owner_of(&self, i: usize) -> &str {
        Self::module_key(&self.decl_modules[i])
    }

    /// The modules `module` uses: the ones it declared (`HostModule::uses`), then the owners
    /// of the types it shares with them, by name (not by what was registered when).
    pub(crate) fn uses_of(&self, module: &str) -> Vec<String> {
        let mut out: Vec<String> = self
            .uses
            .iter()
            .find(|(m, _)| m == module)
            .map(|(_, u)| u.clone())
            .unwrap_or_default();
        let mut shared: Vec<String> = Vec::new();
        for (m, _, j, _) in &self.aliases {
            let owner = self.owner_of(*j);
            if m == module
                && owner != DEFAULT_MODULE
                && owner != module
                && !out.iter().any(|u| u == owner)
                && !shared.iter().any(|u| u == owner)
            {
                shared.push(owner.to_string());
            }
        }
        shared.sort();
        out.extend(shared);
        out
    }

    /// How `home`'s type `name` is written in a signature of the module being described: the
    /// bare name in its own module and for a type of the default module (in scope everywhere),
    /// `module.Name` for a type of another named module.
    pub(crate) fn written(&self, home: &str, name: &str) -> String {
        if home == self.current() || home == DEFAULT_MODULE {
            name.to_string()
        } else {
            format!("{}.{name}", short_module_name(home))
        }
    }

    /// The declaration a name in a signature of `module` stands for: `Name` is the module's
    /// own type or the default module's, `json.Name` a type of a module it uses. `Err` says
    /// why there is none.
    pub(crate) fn resolve(&self, module: &str, written: &str) -> Result<usize, String> {
        if let Some((alias, name)) = written.split_once('.') {
            let used: Vec<String> = self
                .uses_of(module)
                .into_iter()
                .filter(|u| short_module_name(u) == alias)
                .collect();
            let Some(home) = used.first() else {
                // A type this module registered too (the same Rust type), named through any
                // module that registered it.
                if let Some(j) = self.find_visible(module, name)
                    && self
                        .registrants(j)
                        .iter()
                        .any(|m| short_module_name(m) == alias)
                    && self.aliases.iter().any(|a| a.2 == j)
                {
                    return Ok(j);
                }
                let home = self
                    .decls
                    .iter()
                    .zip(&self.decl_modules)
                    .find(|(d, m)| {
                        d.name() == name
                            && Self::module_key(m) != module
                            && short_module_name(Self::module_key(m)) == alias
                    })
                    .map(|(_, m)| Self::module_key(m).to_string())
                    .or_else(|| {
                        self.aliases
                            .iter()
                            .find(|(m, n, _, _)| n == name && short_module_name(m) == alias)
                            .map(|(m, _, _, _)| m.clone())
                    });
                return Err(match home {
                    Some(home) if module == DEFAULT_MODULE => format!(
                        "the type `{written}` is a type of the module `{home}`, and the default module cannot name another module's types (its interface is in the scope of every module); put the function in a named module that uses `{home}`"
                    ),
                    Some(home) => format!(
                        "the type `{written}` is a type of the module `{home}`, which the module `{module}` does not declare it uses (call `.uses(\"{home}\")` on the module before registering what mentions it)"
                    ),
                    None => format!("the type `{written}` is not a type of a module in use"),
                });
            };
            return self
                .find_visible(home, name)
                .ok_or_else(|| format!("the module `{home}` declares no type `{name}`"));
        }
        self.find_visible(module, written)
            .or_else(|| self.find_decl(DEFAULT_MODULE, written))
            .ok_or_else(|| format!("the type `{written}` is not declared"))
    }

    /// Whether `name` is the name of a module enum (of any module).
    pub(crate) fn is_module_enum_name(&self, name: &str) -> bool {
        let name = name.rsplit('.').next().unwrap_or(name);
        self.decls
            .iter()
            .any(|d| d.is_module_local() && d.name() == name)
    }

    /// Records a declaration for the module being described and says which module it ended up
    /// in. `rust` is the Rust type it was described from, if any: the same Rust type
    /// described by two modules is one type (see [`TypeRegistry::share`]).
    fn define(&mut self, decl: TypeDecl, rust: Option<AnyTypeId>) -> String {
        self.define_as(decl, true, rust)
    }

    /// [`TypeRegistry::define`]; a class is never shared by its description (`share` false):
    /// two classes are the same when their Rust type is.
    fn define_as(&mut self, decl: TypeDecl, share: bool, rust: Option<AnyTypeId>) -> String {
        let local = decl.is_module_local();
        let current = self.current().to_string();
        // A module enum's bare name is its own: it cannot also be the name of another host
        // type of any module, whichever is registered first.
        if self
            .decls
            .iter()
            .any(|d| d.name() == decl.name() && d.is_module_local() != local)
        {
            self.conflicts.push(format!(
                "the name `{}` is used both by a module enum (`register_module_enum`) and by another host type; a module enum needs a name of its own",
                decl.name()
            ));
            return current;
        }
        if let Some(i) = self.find_decl(&current, decl.name()) {
            // The same shape is not the same type: two Rust types of one name in one module
            // are two types for one name, whichever came first and whoever owns the name.
            if let (Some(had), Some(now)) = (self.decl_rust[i], rust)
                && had != now
            {
                self.conflicts.push(format!(
                    "the module `{current}` names two different Rust types `{}`: {} and {}",
                    decl.name(),
                    self.rust_type(Some(had)),
                    self.rust_type(Some(now))
                ));
                return current;
            }
            if !self.same_declaration(i, &decl, &current) {
                self.conflicts.push(format!(
                    "the type `{}` is described in two different ways",
                    decl.name()
                ));
            }
            return current;
        }
        if share
            && !local
            && !colibri_check::host::is_std_module_name(&current)
            && let Some(r) = rust
        {
            if let Some(&(_, _, j, _)) = self
                .aliases
                .iter()
                .find(|(m, n, _, _)| *m == current && n == decl.name())
            {
                if self.decl_rust[j] != Some(r) {
                    self.conflicts.push(format!(
                            "the module `{current}` names two different types `{}`: the one of module `{}` ({}) and {}",
                            decl.name(),
                            self.owner_of(j),
                            self.rust_type(self.decl_rust[j]),
                            self.rust_type(Some(r))
                        ));
                }
                return self.owner_of(j).to_string();
            }
            if let Some(j) = (0..self.decls.len()).find(|&j| {
                self.decl_rust[j] == Some(r)
                    && !colibri_check::host::is_std_module_name(self.owner_of(j))
            }) {
                return self.share(j, &current, decl);
            }
        }
        // The host calling into the program has no module: a bare name that no default type
        // has is the type of the one named module that declares it.
        if self.host_side
            && share
            && !local
            && current == DEFAULT_MODULE
            && self.find_decl(DEFAULT_MODULE, decl.name()).is_none()
        {
            let owners: Vec<String> = self
                .decls
                .iter()
                .zip(&self.decl_modules)
                .filter(|(d, m)| {
                    d.name() == decl.name()
                        && !d.is_module_local()
                        && m.as_deref()
                            .is_some_and(|m| !colibri_check::host::is_std_module_name(m))
                })
                .filter_map(|(_, m)| m.clone())
                .collect();
            match owners.as_slice() {
                [] => {}
                [home]
                    if self.find_decl(home, decl.name()).is_some_and(|i| {
                        self.same_decl(&self.decls[i], home, &decl, DEFAULT_MODULE)
                    }) =>
                {
                    return home.clone();
                }
                [_] => {}
                _ => {
                    self.conflicts.push(format!(
                        "the type `{}` is declared by several modules ({}): the host cannot tell which one it means; name the module's function or callback signature through that module's registration instead",
                        decl.name(),
                        owners.join(", ")
                    ));
                    return current;
                }
            }
        }
        self.decls.push(decl);
        self.decl_modules.push(self.current_module.clone());
        self.decl_scope.push(self.current_module.clone());
        self.decl_rust.push(rust);
        self.events.push((current.clone(), self.decls.len() - 1));
        current
    }

    /// The Rust type behind declaration `j` is described again, by module `current`: one
    /// Colibri type. Its owner, whatever the order of the registrations, is the default
    /// module if it registered the type, else the module among the registrants whose name
    /// sorts first; every other registrant names it by an alias and says so in its interface
    /// (`type Pos = physics.Pos`). When a registration changes the owner, the type moves and
    /// every alias follows it. Returns the owner.
    fn share(&mut self, j: usize, current: &str, _decl: TypeDecl) -> String {
        let name = self.decls[j].name().to_string();
        let owner = self.owner_of(j).to_string();
        // The host asking about a type of the program changes nothing: it only needs the name.
        if owner == current || self.host_side {
            return owner;
        }
        if current == DEFAULT_MODULE && self.find_decl(DEFAULT_MODULE, &name).is_some() {
            self.conflicts.push(format!(
                "the type `{name}` of the default module is another Rust type than the one the module `{owner}` registered under the same name"
            ));
            return current.to_string();
        }
        self.events.push((current.to_string(), j));
        let new_owner =
            if owner == DEFAULT_MODULE || (current != DEFAULT_MODULE && owner.as_str() < current) {
                owner.clone()
            } else {
                current.to_string()
            };
        if new_owner == owner {
            self.aliases
                .push((current.to_string(), name.clone(), j, owner.clone()));
        } else {
            // The type moves to `current`; the module that had it names it by an alias like
            // any other, and so does every alias that pointed at the old owner.
            self.decl_modules[j] = (new_owner != DEFAULT_MODULE).then(|| new_owner.clone());
            for a in self.aliases.iter_mut().filter(|a| a.2 == j) {
                a.3 = new_owner.clone();
            }
            self.aliases.retain(|a| !(a.2 == j && a.0 == new_owner));
            self.aliases
                .push((owner.clone(), name.clone(), j, new_owner.clone()));
            self.moved.set(true);
        }
        if new_owner != DEFAULT_MODULE && owner != DEFAULT_MODULE && self.uses_cycle(&new_owner) {
            self.conflicts.push(format!(
                "the type `{name}` is shared by the modules `{owner}` and `{current}`, but one already uses the other: modules cannot name each other's types in both directions"
            ));
        }
        new_owner
    }

    /// Whether the modules `module` uses (directly or through others) include one that uses
    /// `module` back.
    fn uses_cycle(&self, module: &str) -> bool {
        self.uses_of(module)
            .iter()
            .any(|u| u != DEFAULT_MODULE && self.uses_reach(u, module))
    }

    /// Whether the declaration `i` and `decl`, described by module `scope`, are the same:
    /// field types naming the same types, however each module wrote them.
    fn same_declaration(&self, i: usize, decl: &TypeDecl, scope: &str) -> bool {
        let home = self.decl_scope[i].as_deref().unwrap_or(DEFAULT_MODULE);
        self.canonical(&self.decls[i], home) == self.canonical(decl, scope)
    }

    /// `decl` with the type names in its fields replaced by the index of the declaration
    /// each stands for, seen from `scope`.
    fn canonical(&self, decl: &TypeDecl, scope: &str) -> TypeDecl {
        match decl {
            TypeDecl::Struct { name, fields } => TypeDecl::Struct {
                name: name.clone(),
                fields: fields
                    .iter()
                    .map(|(n, t)| {
                        let t = t.map_names(&|written, _| match self.resolve(scope, written) {
                            Ok(i) => format!("#{i}"),
                            Err(_) => written.to_string(),
                        });
                        (n.clone(), t)
                    })
                    .collect(),
            },
            other => other.clone(),
        }
    }

    /// `t`, written in a signature of module `scope`, as module `shown_in` prints it: each
    /// struct, enum or class named by the module that owns it now (the bare name for the
    /// module's own types and the default module's, `module.Name` for another module's),
    /// whatever the registration order wrote at the time.
    pub(crate) fn spelled(&self, scope: &str, shown_in: &str, t: &TypeDesc) -> TypeDesc {
        t.map_names(&|written, _| match self.resolve(scope, written) {
            Ok(i) => {
                let owner = self.owner_of(i);
                let name = self.decls[i].name();
                if owner == shown_in || owner == DEFAULT_MODULE {
                    name.to_string()
                } else {
                    format!("{}.{name}", short_module_name(owner))
                }
            }
            Err(_) => written.to_string(),
        })
    }

    /// The modules that registered declaration `j`: its owner, then the ones that name it by
    /// an alias.
    fn registrants(&self, j: usize) -> Vec<&str> {
        let mut out = vec![self.owner_of(j)];
        out.extend(
            self.aliases
                .iter()
                .filter(|a| a.2 == j)
                .map(|a| a.0.as_str()),
        );
        out
    }

    /// For each declaration `module` described, the position of the first time it did: what
    /// orders its own types in its interface text.
    pub(crate) fn local_orders(&self, module: &str) -> std::collections::HashMap<usize, usize> {
        let mut out = std::collections::HashMap::new();
        for (_, j) in self.events.iter().filter(|(m, _)| m == module) {
            let next = out.len();
            out.entry(*j).or_insert(next);
        }
        out
    }

    /// Whether two declarations, made in modules `a_module` and `b_module`, are the same type:
    /// the same name and shape, field types naming the same types (a field written `Inner` in
    /// one module and `physics.Inner` in the other can be one).
    fn same_decl(&self, a: &TypeDecl, a_module: &str, b: &TypeDecl, b_module: &str) -> bool {
        match (a, b) {
            (
                TypeDecl::Struct {
                    name: an,
                    fields: af,
                },
                TypeDecl::Struct {
                    name: bn,
                    fields: bf,
                },
            ) => {
                an == bn
                    && af.len() == bf.len()
                    && af.iter().zip(bf).all(|((n1, t1), (n2, t2))| {
                        n1 == n2 && self.qualify(t1, a_module) == self.qualify(t2, b_module)
                    })
            }
            (TypeDecl::Enum { .. }, TypeDecl::Enum { .. }) => a == b,
            _ => false,
        }
    }

    /// `t` with every type name it mentions spelled `module|Name` with the module that
    /// declares it, as seen from `module`.
    fn qualify(&self, t: &TypeDesc, module: &str) -> TypeDesc {
        let q = |t: &TypeDesc| self.qualify(t, module);
        let name = |written: &str| -> String {
            if let Some((alias, name)) = written.split_once('.') {
                match self
                    .uses_of(module)
                    .iter()
                    .find(|u| short_module_name(u) == alias)
                {
                    Some(home) => format!("{home}|{name}"),
                    None => written.to_string(),
                }
            } else if self.find_decl(module, written).is_some() {
                format!("{module}|{written}")
            } else {
                format!("{DEFAULT_MODULE}|{written}")
            }
        };
        match t {
            TypeDesc::List(i) => TypeDesc::List(Box::new(q(i))),
            TypeDesc::Option(i) => TypeDesc::Option(Box::new(q(i))),
            TypeDesc::Result(i) => TypeDesc::Result(Box::new(q(i))),
            TypeDesc::Tuple(items) => TypeDesc::Tuple(items.iter().map(q).collect()),
            TypeDesc::Map(k, v) => TypeDesc::Map(Box::new(q(k)), Box::new(q(v))),
            TypeDesc::Fn(params, ret) => {
                TypeDesc::Fn(params.iter().map(q).collect(), Box::new(q(ret)))
            }
            TypeDesc::Value(n) => TypeDesc::Value(name(n)),
            TypeDesc::Host(n) => TypeDesc::Host(name(n)),
            other => other.clone(),
        }
    }

    /// Whether `from` uses `target`, directly or through the modules it uses.
    fn uses_reach(&self, from: &str, target: &str) -> bool {
        let mut seen = vec![from.to_string()];
        let mut todo = vec![from.to_string()];
        while let Some(m) = todo.pop() {
            for u in self.uses_of(&m) {
                if u == target {
                    return true;
                }
                if !seen.contains(&u) {
                    seen.push(u.clone());
                    todo.push(u);
                }
            }
        }
        false
    }

    /// Declares a Colibri `struct` with these fields and returns the type to use in
    /// signatures. Describing it again identically is fine; differently is an error reported
    /// when the function is registered. The struct belongs to the module being registered
    /// into: `fs.Stat` and a `Stat` of the embedder are two types. A struct declared here has
    /// no Rust type, so it is never the same type as one another module declares; use
    /// [`TypeRegistry::define_struct_of`] (what `#[colibri::export(value)]` does) to share.
    pub fn define_struct(&mut self, name: &str, fields: Vec<(&str, TypeDesc)>) -> TypeDesc {
        self.define_struct_for(None, name, fields)
    }

    /// [`TypeRegistry::define_struct`] for the struct that the Rust type `rust` converts to.
    /// Two modules that describe the same Rust type share one Colibri type: the default
    /// module's name for it if the default module registered it (in any order), else the name
    /// in the module that registered it first; each module that registered it can name it
    /// (`physics.Pos` and `render.Pos`).
    pub fn define_struct_of(
        &mut self,
        rust: AnyTypeId,
        name: &str,
        fields: Vec<(&str, TypeDesc)>,
    ) -> TypeDesc {
        self.define_struct_for(Some(rust), name, fields)
    }

    fn define_struct_for(
        &mut self,
        rust: Option<AnyTypeId>,
        name: &str,
        fields: Vec<(&str, TypeDesc)>,
    ) -> TypeDesc {
        let home = self.define(
            TypeDecl::Struct {
                name: name.to_string(),
                fields: fields
                    .into_iter()
                    .map(|(n, t)| (n.to_string(), t))
                    .collect(),
            },
            rust,
        );
        TypeDesc::Value(self.written(&home, name))
    }

    /// Declares a Colibri `enum` whose variants carry no payload (the current module's, as
    /// [`TypeRegistry::define_struct`]).
    pub fn define_enum(&mut self, name: &str, variants: &[&str]) -> TypeDesc {
        self.define_enum_for(None, name, variants)
    }

    /// [`TypeRegistry::define_enum`] for the enum that the Rust type `rust` converts to; shared
    /// between modules as [`TypeRegistry::define_struct_of`] says.
    pub fn define_enum_of(&mut self, rust: AnyTypeId, name: &str, variants: &[&str]) -> TypeDesc {
        self.define_enum_for(Some(rust), name, variants)
    }

    fn define_enum_for(
        &mut self,
        rust: Option<AnyTypeId>,
        name: &str,
        variants: &[&str],
    ) -> TypeDesc {
        let home = self.define(
            TypeDecl::Enum {
                name: name.to_string(),
                variants: variants.iter().map(ToString::to_string).collect(),
                local: false,
            },
            rust,
        );
        TypeDesc::Value(self.written(&home, name))
    }

    /// Like [`TypeRegistry::define_enum`], for an enum that belongs to the module being
    /// registered (`fs.Error`) and is open: another module may declare an enum of the same
    /// name, and signatures cannot mention it (a module's functions return the erased `Error`,
    /// whose `is`/`as` recover the enum).
    pub fn define_module_enum(&mut self, name: &str, variants: &[&str]) -> TypeDesc {
        let home = self.define(
            TypeDecl::Enum {
                name: name.to_string(),
                variants: variants.iter().map(ToString::to_string).collect(),
                local: true,
            },
            None,
        );
        TypeDesc::Value(self.written(&home, name))
    }

    /// The type `name` that the module `module` declared (`std.json`'s `Match`, say), for a
    /// signature of the module being registered, which must declare it uses `module`
    /// ([`crate::HostModule::uses`]). A struct or enum of another module cannot be described
    /// again from here (it would be this module's own copy), so it is asked for by name; a
    /// class is found by its Rust type through [`TypeRegistry::define_host_type`] instead.
    /// A module that declares no such type is a conflict, reported when the function is
    /// registered.
    pub fn foreign_type(&mut self, module: &str, name: &str) -> TypeDesc {
        let written = self.written(module, name);
        match self.find_visible(module, name).map(|i| &self.decls[i]) {
            Some(TypeDecl::Class { .. }) => TypeDesc::Host(written),
            Some(_) => TypeDesc::Value(written),
            None => {
                self.conflicts.push(format!(
                    "the module `{module}` declares no type `{name}`: register it before the module that mentions it"
                ));
                TypeDesc::Value(written)
            }
        }
    }

    /// Declares the host class `T` and its methods (what [`HostType::describe`] says) and
    /// returns the type to use in signatures. Describing it again is a no-op, so a class that
    /// several signatures mention (or that mentions itself) is declared once, in the module
    /// that first described it; another module's signatures name it `module.Name` when they
    /// declare they use that module. A different Rust type with the same name in the same
    /// module is a conflict, reported when the function is registered; in another module it is
    /// another class.
    pub fn define_host_type<T: HostType>(&mut self) -> TypeDesc {
        let name = T::NAME;
        let any = AnyTypeId::of::<T>();
        if let Some(at) = self.classes.iter().position(|c| c.any == any) {
            self.last_class = Some(at);
            let existing = &self.classes[at];
            let home = Self::module_key(&existing.module).to_string();
            return TypeDesc::Host(self.written(&home, name));
        }
        let current = self.current().to_string();
        if let Some(i) = self.find_decl(&current, name) {
            // Another Rust type, or a struct or enum, already has the name in this module.
            let what = match &self.decls[i] {
                TypeDecl::Class { .. } => {
                    "the host class `{name}` is described by two different Rust types"
                }
                _ => "the name `{name}` is already used by a struct or enum of the module",
            };
            self.conflicts.push(what.replace("{name}", name));
            return TypeDesc::Host(name.to_string());
        }
        self.define_as(
            TypeDecl::Class {
                name: name.to_string(),
            },
            false,
            None,
        );
        // Entered before its methods are described, so a method that mentions the class itself
        // (or a class that mentions this one) finds it already there.
        let index = self.classes.len();
        self.classes.push(ClassEntry {
            name: name.to_string(),
            any,
            module: self.current_module.clone(),
            methods: Vec::new(),
            hooks: crate::class::ClassHooks::default(),
        });
        let mut builder = TypeBuilder::<T>::new(self);
        T::describe(&mut builder);
        let (methods, hooks) = builder.finish();
        self.classes[index].methods = methods;
        self.classes[index].hooks = hooks;
        // Set last: the classes its methods mention were described (and noted) on the way.
        self.last_class = Some(index);
        TypeDesc::Host(name.to_string())
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
    /// The `Show` text and `==` of its objects, when the class customises them.
    pub hooks: crate::class::ClassHooks,
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
