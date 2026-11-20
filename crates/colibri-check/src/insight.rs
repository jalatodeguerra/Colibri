//! Position queries on a checked program, for editors (hover and go to definition).
//!
//! Checking normally records nothing of this kind. [`crate::check_modules_with_insight`] asks
//! the checker to also note, while it resolves each body, what every expression and name
//! turned out to be: the zonker (the last step of a body, which has the final types) pushes a
//! [`RawFact`] for each expression, and when the whole program is checked those become
//! [`Fact`]s with their types rendered as source text. The table is built even for a program
//! with errors, so hover keeps working while the user is in the middle of an edit.
//!
//! A fact is a span with the text to show for it and, if the thing it names is declared in
//! source, the span of the declaration. [`Insight::at`] answers "what is at this offset": the
//! smallest fact around it. A call's name has no span of its own in HIR, so facts of that
//! kind are *named*: they cover the whole call, but only answer when the word under the
//! cursor is the name.

use std::collections::HashMap;

use colibri_syntax::{FileId, Span};

use crate::checker::Checker;
use crate::hir::{
    Anchor, Callee, Expr, ExprKind, FieldIdx, FnId, GlobalId, HostFnId, Place, TypeDef,
    TypeDefKind, ValueProj,
};
use crate::host::HOST_FILE;
pub use crate::members::{Member, MemberKind};
use crate::prelude;
use crate::types::{FIRST_USER_TYPE, ParamId, Ty, TypeId};

/// What the checker knows about one place in the source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fact {
    pub span: Span,
    /// For a fact about a name that has no span of its own (a call's callee, a field of a
    /// longer expression): the name. The fact answers only when the cursor is on that word.
    pub name: Option<String>,
    /// What to show on hover: a type (`int`), a binding (`var n: int`), a signature
    /// (`fn add(a: int, b: int) -> int`).
    pub text: String,
    /// Where the thing is declared, when that is in source.
    pub def: Option<Span>,
}

/// The facts of one checked program.
#[derive(Clone, Debug, Default)]
pub struct Insight {
    facts: Vec<Fact>,
    /// The fields and methods of each distinct type that an expression of the program has.
    /// Types are told apart by identity (the type's id and arguments, a type parameter's own
    /// id), not by how they are written: two structs `P` of different modules, or the `T` of
    /// two generic declarations, have separate lists.
    members: Vec<Vec<Member>>,
    pub(crate) refs: Vec<(Span, Span)>,
    /// The type of every expression, by its span, as written, and its entry in `members`.
    types: HashMap<(FileId, u32, u32), (String, usize)>,
    /// The type of every local and global at the span of its name, likewise.
    bindings: HashMap<(FileId, u32, u32), (String, usize)>,
}

impl Insight {
    /// Every fact, in no particular order.
    pub fn facts(&self) -> &[Fact] {
        &self.facts
    }

    /// The names the checker resolved by the type of what is in front of them, as pairs of
    /// the name's span and the span of what it names: fields (`p.x`, in places too), and
    /// variants written `.Circle` or matched by a bare name. Only names that resolved are here.
    pub fn references(&self) -> &[(Span, Span)] {
        &self.refs
    }

    /// The type of the expression that spans exactly `span`, as written.
    pub fn type_at(&self, span: Span) -> Option<&str> {
        self.types
            .get(&(span.file, span.start, span.end))
            .map(|(text, _)| text.as_str())
    }

    /// The fields and methods of the value of the expression that spans exactly `span`,
    /// sorted by name. Empty where the checker saw no such expression.
    pub fn members_at(&self, span: Span) -> &[Member] {
        self.members_in(self.types.get(&(span.file, span.start, span.end)))
    }

    /// The type, as written, of the local or global whose name is at `name`.
    pub fn binding_type(&self, name: Span) -> Option<&str> {
        self.bindings
            .get(&(name.file, name.start, name.end))
            .map(|(text, _)| text.as_str())
    }

    /// The fields and methods of the type of the local or global whose name is at `name`.
    pub fn binding_members(&self, name: Span) -> &[Member] {
        self.members_in(self.bindings.get(&(name.file, name.start, name.end)))
    }

    fn members_in(&self, entry: Option<&(String, usize)>) -> &[Member] {
        entry.map_or(&[], |(_, i)| self.members[*i].as_slice())
    }

    /// The fact for the byte `offset` of `file` (a span covers `start..end`), given the identifier (possibly empty) the
    /// cursor is on: the smallest span around the offset, a named fact before an unnamed one of
    /// the same size. Named facts whose name is not `word` do not apply.
    pub fn at(&self, file: FileId, offset: u32, word: &str) -> Option<&Fact> {
        // The fact recorded last wins a tie: a name's own fact follows the one for its type.
        self.facts
            .iter()
            .rev()
            .filter(|f| {
                f.span.file == file
                    && f.span.start <= offset
                    && offset < f.span.end
                    && f.name.as_deref().is_none_or(|n| n == word)
            })
            .min_by_key(|f| (f.span.len(), f.name.is_none()))
    }
}

/// What kind of thing a prelude name is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreludeKind {
    Function,
    Type,
    Interface,
}

/// The names every module can use without declaring or importing them, for editors: the
/// built-in functions, the built-in types and the prelude's interfaces. (Functions the host
/// provides without a module are in [`crate::host::DEFAULT_MODULE`].)
pub fn prelude_names() -> Vec<(String, PreludeKind)> {
    use crate::checker::BUILTIN_FNS;
    use colibri_syntax::ast::{ItemKind, TopLevel};
    let mut out: Vec<(String, PreludeKind)> = BUILTIN_FNS
        .iter()
        .map(|n| (n.to_string(), PreludeKind::Function))
        .collect();
    let primitives = [
        "int", "i8", "i16", "i32", "i64", "u8", "u16", "u32", "u64", "float", "f32", "f64", "bool",
        "char", "str", "List", "Map", "Set", "Option", "Result", "ptr",
    ];
    out.extend(
        primitives
            .iter()
            .map(|n| (n.to_string(), PreludeKind::Type)),
    );
    for top in &prelude::prelude().items {
        let TopLevel::Item(item) = top else { continue };
        match &item.kind {
            ItemKind::Interface(i) => out.push((i.name.name.clone(), PreludeKind::Interface)),
            ItemKind::Struct(s) => out.push((s.name.name.clone(), PreludeKind::Type)),
            _ => {}
        }
    }
    out
}

/// What a recorded span stands for; turned into a [`Fact`] once every type is known.
#[derive(Debug)]
pub(crate) enum Target {
    Expr,
    /// A local binding (at its declaration or a read of it).
    Local {
        name: String,
        mutable: bool,
        def: Span,
    },
    /// A read of a module-level binding; `prefix` records only the name at the start of a
    /// longer span (`CONFIG.size`).
    Global {
        id: GlobalId,
        prefix: bool,
    },
    Call(Callee),
    FnRef(FnId),
    /// A struct, class or variant written as a literal or constructor call.
    TypeLit(TypeId),
    VariantLit(TypeId, u32),
    /// The last field of a path of projections starting at a value of type `base`.
    Field {
        base: FieldBase,
        path: Vec<ValueProj>,
    },
}

#[derive(Debug)]
pub(crate) enum FieldBase {
    Ty(Ty),
    Global(GlobalId),
}

#[derive(Debug)]
pub(crate) struct RawFact {
    pub span: Span,
    pub ty: Ty,
    pub target: Target,
}

/// The fact for the type of an expression, recorded before those of its parts: of two facts
/// with the same span (a read and the `Show` call around it in an interpolation), the later one wins
/// ([`Insight::at`]), so the inner expression and then what it names come after.
pub(crate) fn type_fact(e: &Expr, out: &mut Vec<RawFact>) {
    if !e.ty.is_error() {
        out.push(RawFact {
            span: e.span,
            ty: e.ty.clone(),
            target: Target::Expr,
        });
    }
}

/// The facts of an expression about what it names, once its parts are final.
pub(crate) fn expr_facts(
    e: &Expr,
    local: &dyn Fn(crate::hir::LocalId) -> Option<(String, bool, Span, Ty)>,
    out: &mut Vec<RawFact>,
) {
    let push = |out: &mut Vec<RawFact>, span, ty: &Ty, target| {
        out.push(RawFact {
            span,
            ty: ty.clone(),
            target,
        });
    };
    match &e.kind {
        ExprKind::Read(p) => place_facts(e, p, local, out),
        ExprKind::Call(c) => match c.callee {
            Callee::Fn(_) | Callee::Host(_) | Callee::Bound { .. } | Callee::Dyn { .. } => {
                push(out, e.span, &e.ty, Target::Call(c.callee.clone()));
            }
            Callee::Intrinsic(_) | Callee::Value => {}
        },
        ExprKind::FnRef { f, .. } => push(out, e.span, &e.ty, Target::FnRef(*f)),
        ExprKind::Struct { ty, .. } | ExprKind::Object { ty, .. } => {
            push(out, e.span, &e.ty, Target::TypeLit(*ty));
        }
        ExprKind::Variant { ty, variant, .. } => {
            push(out, e.span, &e.ty, Target::VariantLit(*ty, *variant));
        }
        ExprKind::Project(base, p @ ValueProj::Field(_)) => push(
            out,
            e.span,
            &e.ty,
            Target::Field {
                base: FieldBase::Ty(base.ty.clone()),
                path: vec![*p],
            },
        ),
        _ => {}
    }
}

fn place_facts(
    e: &Expr,
    p: &Place,
    local: &dyn Fn(crate::hir::LocalId) -> Option<(String, bool, Span, Ty)>,
    out: &mut Vec<RawFact>,
) {
    let (base, path): (Option<FieldBase>, Vec<ValueProj>) = match &p.anchor {
        Anchor::Local(id) => {
            let Some((name, mutable, def, ty)) = local(*id) else {
                return;
            };
            let span = if p.path.is_empty() {
                e.span
            } else {
                Span::new(e.span.file, e.span.start, e.span.start + name.len() as u32)
            };
            out.push(RawFact {
                span,
                ty: ty.clone(),
                target: Target::Local { name, mutable, def },
            });
            (Some(FieldBase::Ty(ty)), p.path.clone())
        }
        Anchor::Global(id) => {
            out.push(RawFact {
                span: e.span,
                ty: e.ty.clone(),
                target: Target::Global {
                    id: *id,
                    prefix: !p.path.is_empty(),
                },
            });
            (Some(FieldBase::Global(*id)), p.path.clone())
        }
        Anchor::ClassField(obj, idx) => {
            let mut path = vec![ValueProj::Field(*idx)];
            path.extend(p.path.iter().copied());
            (Some(FieldBase::Ty(obj.ty.clone())), path)
        }
        Anchor::ListElem(..) | Anchor::MapEntry(..) => (None, Vec::new()),
    };
    if let Some(base) = base
        && matches!(path.last(), Some(ValueProj::Field(_)))
    {
        out.push(RawFact {
            span: e.span,
            ty: e.ty.clone(),
            target: Target::Field { base, path },
        });
    }
}

fn last_segment(name: &str) -> &str {
    name.rsplit('.').next().unwrap_or(name)
}

fn is_source(span: Span) -> bool {
    span.file != HOST_FILE && !prelude::in_prelude(span)
}

impl Checker<'_> {
    pub(crate) fn show_ty(&self, ty: &Ty) -> String {
        ty.render(&self.types)
    }

    /// `fn name[T](a: int, var self) -> int` for a function of the program.
    fn signature(&self, f: FnId) -> String {
        self.signature_of(f, &self.fns[f.0 as usize].name, false, &[])
    }

    /// Like [`Checker::signature`] under another `name`, and without the receiver when
    /// `skip_receiver` is set (a method written after a value: `v.name(..)`). The type
    /// parameters in `bound` are replaced by their types and left out of the list.
    pub(crate) fn signature_of(
        &self,
        f: FnId,
        name: &str,
        skip_receiver: bool,
        bound: &[(ParamId, Ty)],
    ) -> String {
        let info = &self.fns[f.0 as usize];
        let mut out = format!("fn {name}");
        let open: Vec<&str> = info
            .generics
            .iter()
            .filter(|g| !bound.iter().any(|(p, _)| *p == g.id))
            .map(|g| g.name.as_str())
            .collect();
        if !open.is_empty() {
            out.push_str(&format!("[{}]", open.join(", ")));
        }
        let params: Vec<String> = info
            .params
            .iter()
            .enumerate()
            .filter(|(i, _)| !(skip_receiver && *i == 0 && info.receiver.is_some()))
            .map(|(i, p)| match (i, info.receiver) {
                (0, Some(true)) => "var self".to_string(),
                (0, Some(false)) => "self".to_string(),
                _ => format!("{}: {}", p.name, self.show_ty(&p.ty.subst(bound))),
            })
            .collect();
        out.push_str(&format!("({})", params.join(", ")));
        if info.ret != Ty::Unit {
            out.push_str(&format!(" -> {}", self.show_ty(&info.ret.subst(bound))));
        }
        out
    }

    fn host_fn(&self, id: HostFnId) -> Option<FnId> {
        self.fns
            .iter()
            .position(|f| f.host.as_ref().is_some_and(|h| h.id == id))
            .map(|i| FnId(i as u32))
    }

    /// The field a path of projections ends in: its definition, and the type definition it
    /// belongs to.
    fn field_of(&self, base: &FieldBase, path: &[ValueProj]) -> Option<(String, Span)> {
        let mut ty = match base {
            FieldBase::Ty(t) => t.clone(),
            FieldBase::Global(g) => self.globals[g.0 as usize].ty.clone()?,
        };
        let (last, init) = path.split_last()?;
        for step in init {
            ty = self.step(&ty, *step)?.ty;
        }
        let ValueProj::Field(FieldIdx(i)) = last else {
            return None;
        };
        let Ty::Adt(id, _) = ty else { return None };
        let field = self.types.get(id.0 as usize)?.fields().get(*i as usize)?;
        Some((field.name.clone(), field.span))
    }

    fn step(&self, ty: &Ty, proj: ValueProj) -> Option<crate::hir::FieldDef> {
        match (ty, proj) {
            (Ty::Adt(id, args), ValueProj::Field(FieldIdx(i))) => {
                let def = self.types.get(id.0 as usize)?;
                let mut field = def.fields().get(i as usize)?.clone();
                let map: Vec<_> = def
                    .params
                    .iter()
                    .copied()
                    .zip(args.iter().cloned())
                    .collect();
                field.ty = field.ty.subst(&map);
                Some(field)
            }
            (Ty::Tuple(items), ValueProj::TupleIdx(i)) => Some(crate::hir::FieldDef {
                name: i.to_string(),
                ty: items.get(i as usize)?.clone(),
                span: Span::new(FileId(0), 0, 0),
                weak: false,
                default: None,
                rename: None,
                skip: false,
            }),
            _ => None,
        }
    }

    fn type_text(&self, id: TypeId) -> String {
        let def: &TypeDef = &self.types[id.0 as usize];
        let kw = match def.kind {
            TypeDefKind::Struct { .. } => "struct",
            TypeDefKind::Class { .. } | TypeDefKind::HostClass => "class",
            TypeDefKind::Enum { .. } => "enum",
            TypeDefKind::Interface { .. } => "interface",
        };
        format!("{kw} {}", def.display_name())
    }

    /// Turns what the zonker recorded into facts, and adds the declarations of items.
    pub(crate) fn build_insight(&self, raw: Vec<RawFact>) -> Insight {
        let mut facts = Vec::new();
        let mut members: Vec<Vec<Member>> = Vec::new();
        // The list of each type by identity: `Debug` of a `Ty` names type ids, arguments and
        // the ids of type parameters, where the written text names none of them.
        let mut by_identity: HashMap<String, usize> = HashMap::new();
        let mut types: HashMap<(FileId, u32, u32), (String, usize)> = HashMap::new();
        let mut bindings: HashMap<(FileId, u32, u32), (String, usize)> = HashMap::new();
        for r in raw {
            let ty = self.show_ty(&r.ty);
            let member_list = matches!(
                r.target,
                Target::Expr | Target::Local { .. } | Target::Global { .. }
            )
            .then(|| {
                *by_identity.entry(format!("{:?}", r.ty)).or_insert_with(|| {
                    members.push(self.members_of(&r.ty));
                    members.len() - 1
                })
            });
            if let Some(list) = member_list {
                let key = |s: Span| (s.file, s.start, s.end);
                match &r.target {
                    Target::Expr if is_source(r.span) => {
                        types.insert(key(r.span), (ty.clone(), list));
                    }
                    Target::Local { def, .. } if is_source(*def) => {
                        bindings.insert(key(*def), (ty.clone(), list));
                    }
                    Target::Global { id, .. } => {
                        let name_span = self.globals[id.0 as usize].name_span;
                        if is_source(name_span) {
                            bindings.insert(key(name_span), (ty.clone(), list));
                        }
                    }
                    _ => {}
                }
            }
            let fact = match r.target {
                Target::Expr => Some(Fact {
                    span: r.span,
                    name: None,
                    text: ty,
                    def: None,
                }),
                Target::Local { name, mutable, def } => Some(Fact {
                    span: r.span,
                    text: format!("{}{name}: {ty}", if mutable { "var " } else { "" }),
                    name: None,
                    def: Some(def),
                }),
                Target::Global { id, prefix } => {
                    let g = &self.globals[id.0 as usize];
                    let span = if prefix {
                        Span::new(
                            r.span.file,
                            r.span.start,
                            r.span.start + g.name.len() as u32,
                        )
                    } else {
                        r.span
                    };
                    Some(Fact {
                        span,
                        text: format!("{} {}: {ty}", if g.mutable { "var" } else { "let" }, g.name),
                        name: None,
                        def: Some(g.name_span),
                    })
                }
                Target::Call(callee) => self.call_fact(r.span, &callee),
                Target::FnRef(f) => {
                    let info = &self.fns[f.0 as usize];
                    Some(Fact {
                        span: r.span,
                        name: Some(last_segment(&info.name).to_string()),
                        text: self.signature(f),
                        def: Some(info.name_span),
                    })
                }
                Target::TypeLit(id) => {
                    let def = &self.types[id.0 as usize];
                    Some(Fact {
                        span: r.span,
                        name: Some(def.name.clone()),
                        text: self.type_text(id),
                        def: Some(def.span),
                    })
                }
                Target::VariantLit(id, v) => {
                    let def = &self.types[id.0 as usize];
                    def.variants().get(v as usize).map(|variant| Fact {
                        span: r.span,
                        name: Some(variant.name.clone()),
                        text: format!("{}.{}: {ty}", def.display_name(), variant.name),
                        def: Some(variant.span),
                    })
                }
                Target::Field { base, path } => {
                    self.field_of(&base, &path).map(|(name, def)| Fact {
                        span: r.span,
                        text: format!("{name}: {ty}"),
                        name: Some(name),
                        def: Some(def),
                    })
                }
            };
            facts.extend(fact);
        }
        self.declaration_facts(&mut facts);
        facts.retain(|f| is_source(f.span));
        for f in &mut facts {
            f.def = f.def.filter(|d| is_source(*d));
        }
        Insight {
            facts,
            members,
            types,
            bindings,
            refs: Vec::new(),
        }
    }

    fn call_fact(&self, span: Span, callee: &Callee) -> Option<Fact> {
        let f = match callee {
            Callee::Fn(f) => *f,
            Callee::Host(h) => self.host_fn(*h)?,
            Callee::Bound { iface, method } | Callee::Dyn { iface, method } => {
                let m = self
                    .types
                    .get(iface.0 as usize)?
                    .iface_methods()
                    .get(*method as usize)?;
                return Some(Fact {
                    span,
                    name: Some(m.name.clone()),
                    text: format!("fn {}.{}", self.types[iface.0 as usize].name, m.name),
                    def: Some(m.span),
                });
            }
            Callee::Intrinsic(_) | Callee::Value => return None,
        };
        let info = &self.fns[f.0 as usize];
        Some(Fact {
            span,
            name: Some(last_segment(&info.name).to_string()),
            text: self.signature(f),
            def: Some(info.name_span),
        })
    }

    /// Facts for the names at declarations: functions, types and their members, globals.
    fn declaration_facts(&self, facts: &mut Vec<Fact>) {
        for (i, info) in self.fns.iter().enumerate() {
            if info.name.starts_with('<') || info.host.is_some() {
                continue;
            }
            facts.push(Fact {
                span: info.name_span,
                name: None,
                text: self.signature(FnId(i as u32)),
                def: Some(info.name_span),
            });
        }
        for g in &self.globals {
            let ty = g.ty.as_ref().map_or("_".to_string(), |t| self.show_ty(t));
            facts.push(Fact {
                span: g.name_span,
                name: None,
                text: format!("{} {}: {ty}", if g.mutable { "var" } else { "let" }, g.name),
                def: Some(g.name_span),
            });
        }
        for (i, def) in self.types.iter().enumerate().skip(FIRST_USER_TYPE as usize) {
            facts.push(Fact {
                span: def.span,
                name: None,
                text: self.type_text(TypeId(i as u32)),
                def: Some(def.span),
            });
            for f in def.fields() {
                facts.push(Fact {
                    span: f.span,
                    name: None,
                    text: format!("{}: {}", f.name, self.show_ty(&f.ty)),
                    def: Some(f.span),
                });
            }
            for v in def.variants() {
                facts.push(Fact {
                    span: v.span,
                    name: None,
                    text: format!("{}.{}", def.display_name(), v.name),
                    def: Some(v.span),
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::package::{ModuleMeta, PackageNode};
    use crate::{HostEnv, ModuleInput, check_modules_with_insight};
    use colibri_syntax::parse_file;
    use std::collections::HashMap;

    fn insight_of(src: &str) -> Insight {
        let (file, _) = parse_file(FileId(0), src);
        let meta = ModuleMeta {
            name: String::new(),
            package: 0,
            entry: true,
            script: false,
            dependency: false,
            ffi: true,
            imports: HashMap::new(),
        };
        let packages = [PackageNode {
            name: String::new(),
        }];
        let inputs = [ModuleInput {
            file: &file,
            meta: &meta,
        }];
        check_modules_with_insight(&inputs, &packages, &HostEnv::new(), 1 << 20).1
    }

    fn text_at(insight: &Insight, src: &str, needle: &str, word: &str) -> Option<String> {
        let at = src.find(needle)? as u32;
        insight.at(FileId(0), at, word).map(|f| f.text.clone())
    }

    #[test]
    fn facts_name_types_bindings_and_signatures() {
        let src = "fn add(a: int, b: int) -> int => a + b\nfn main() {\n    let n = add(1, 2)\n    print(n)\n}\n";
        let insight = insight_of(src);
        assert_eq!(
            text_at(&insight, src, "add(1", "add").unwrap(),
            "fn add(a: int, b: int) -> int"
        );
        assert_eq!(text_at(&insight, src, "add(1", "").unwrap(), "int");
        assert_eq!(text_at(&insight, src, "n = ", "n").unwrap(), "n: int");
        assert_eq!(text_at(&insight, src, "1, 2", "").unwrap(), "int");
        // The definition of the call is the function's name.
        let at = src.find("add(1").unwrap() as u32;
        let def = insight.at(FileId(0), at, "add").unwrap().def.unwrap();
        assert_eq!((def.start, def.end), (3, 6));
    }

    #[test]
    fn facts_exist_for_a_program_with_errors() {
        let src = "fn main() {\n    let a = 1\n    let b: str = a\n}\n";
        let insight = insight_of(src);
        assert_eq!(text_at(&insight, src, "a\n}", "a").unwrap(), "a: int");
    }

    /// The members of the first expression whose type is written `ty`.
    fn members_written<'a>(insight: &'a Insight, ty: &str) -> &'a [Member] {
        let at = insight
            .types
            .iter()
            .filter(|(_, (text, _))| text == ty)
            .map(|(k, _)| *k)
            .min();
        at.map_or(&[], |(file, start, end)| {
            insight.members_at(Span::new(file, start, end))
        })
    }

    fn member_details(insight: &Insight, ty: &str) -> Vec<String> {
        members_written(insight, ty)
            .iter()
            .map(|m| m.detail.clone())
            .collect()
    }

    #[test]
    fn members_of_types_the_program_uses() {
        let src = "struct P { x: float, pub y: int }\n\
            impl P {\n    fn scaled(self, k: float) -> P => P { x: self.x * k, y: self.y }\n    fn origin() -> P => P { x: 0.0, y: 0 }\n}\n\
            interface Area { fn area(self) -> float }\n\
            impl Area for P { fn area(self) -> float => self.x }\n\
            fn main() {\n    let p = P { x: 1.0, y: 2 }\n    let xs = [1, 2]\n    let s = \"a\"\n    let o = some(1)\n    print(\"{p.x} {xs.len()} {s.len()} {o.is_some()}\")\n}\n";
        let insight = insight_of(src);
        let p = member_details(&insight, "P");
        assert_eq!(
            p,
            [
                "fn area() -> float",
                "fn clone() -> P",
                "fn eq(other: P) -> bool",
                "fn scaled(k: float) -> P",
                "x: float",
                "y: int"
            ],
            "{p:?}"
        );
        let names: Vec<&str> = members_written(&insight, "P")
            .iter()
            .map(|m| m.name.as_str())
            .collect();
        assert_eq!(names, ["area", "clone", "eq", "scaled", "x", "y"]);
        let xs = member_details(&insight, "List[int]");
        assert!(
            xs.contains(&"fn push(item: int)".to_string())
                || xs.iter().any(|d| d.starts_with("fn push(")),
            "{xs:?}"
        );
        assert!(
            xs.iter().any(|d| d.starts_with("fn map(f: fn(int) -> U)")),
            "{xs:?}"
        );
        let s = member_details(&insight, "str");
        assert!(s.contains(&"fn len() -> int".to_string()), "{s:?}");
        assert!(
            s.contains(&"fn contains(sub: str) -> bool".to_string()),
            "{s:?}"
        );
        let o = member_details(&insight, "int?");
        assert!(o.contains(&"fn is_some() -> bool".to_string()), "{o:?}");
        // A type no expression has is unknown.
        assert!(members_written(&insight, "Q").is_empty());
    }

    #[test]
    fn members_are_told_apart_by_type_identity_not_by_text() {
        // The `T` of the impl has no bounds; the `T` of `one` is a `Shape`. Both print as `T`.
        let src = "interface Shape { fn area(self) -> float }\n\
            struct Box[T] { v: T }\n\
            impl[T] Box[T] {\n    fn get(self) -> T => self.v\n}\n\
            fn one[T: Shape](t: T) -> float {\n    return t.area()\n}\n";
        let insight = insight_of(src);
        let use_of_t = src.find("t.area").unwrap() as u32;
        let span = Span::new(FileId(0), use_of_t, use_of_t + 1);
        assert_eq!(insight.type_at(span), Some("T"));
        let names: Vec<&str> = insight
            .members_at(span)
            .iter()
            .map(|m| m.name.as_str())
            .collect();
        assert_eq!(names, ["area", "clone"]);
        // The unbounded `T` of the impl (`self.v`) keeps its own list.
        let v = src.find("self.v").unwrap() as u32;
        let span = Span::new(FileId(0), v, v + 6);
        assert_eq!(insight.type_at(span), Some("T"));
        let names: Vec<&str> = insight
            .members_at(span)
            .iter()
            .map(|m| m.name.as_str())
            .collect();
        assert_eq!(names, ["clone"]);
    }

    #[test]
    fn generic_methods_show_the_types_of_the_receiver() {
        let src = "struct Sq { s: int }\n\
            struct Box[T] { v: T }\n\
            impl[T] Box[T] {\n    fn get(self) -> T => self.v\n    fn pair[U](self, u: U) -> (T, U) => (self.v, u)\n}\n\
            fn main() {\n    let b = Box { v: Sq { s: 1 } }\n    print(\"{b.get().s}\")\n}\n";
        let insight = insight_of(src);
        let details = member_details(&insight, "Box[Sq]");
        assert!(
            details.contains(&"fn get() -> Sq".to_string()),
            "{details:?}"
        );
        assert!(
            details.contains(&"fn pair[U](u: U) -> (Sq, U)".to_string()),
            "{details:?}"
        );
    }

    #[test]
    fn references_name_fields_variants_and_expression_types() {
        let src = "struct P { x: int, y: int }\n\
            enum E { A(int), B }\n\
            fn f(e: E) -> int {\n    match e {\n        .A(n) => return n\n        B => return 0\n    }\n}\n\
            fn main() {\n    var p = P { x: 1, y: 2 }\n    p.x = p.y\n    let e: E = .A(p.x)\n    print(\"{f(e)}\")\n}\n";
        let insight = insight_of(src);
        let at = |needle: &str, add: usize| (src.find(needle).unwrap() + add) as u32;
        let target = |use_at: u32| {
            insight
                .references()
                .iter()
                .find(|(u, _)| u.start == use_at)
                .map(|(_, d)| (d.start, d.end))
        };
        let in_decl = |d: (u32, u32), name_at: u32| d.0 <= name_at && name_at < d.1;
        // The write `p.x = ..`, the read `p.y`, and the read in `.A(p.x)`.
        let field_x = at("x: int", 0);
        assert!(in_decl(target(at("x = p.y", 0)).unwrap(), field_x));
        assert!(in_decl(
            target(at("y\n    let e", 0)).unwrap(),
            at("y: int", 0)
        ));
        assert!(in_decl(target(at("x)", 0)).unwrap(), field_x));
        // A `.A(n)` pattern, a bare `B` pattern and a `.A(..)` expression.
        let a = at("A(int)", 0);
        assert!(in_decl(target(at("A(n)", 0)).unwrap(), a));
        assert!(in_decl(target(at("B =>", 0)).unwrap(), at("B }", 0)));
        assert!(in_decl(target(at("A(p.x)", 0)).unwrap(), a));
        // Types by span: the whole `p.y`.
        let span = colibri_syntax::Span::new(FileId(0), at("p.y\n", 0), at("p.y\n", 3));
        assert_eq!(insight.type_at(span), Some("int"));
    }
}
