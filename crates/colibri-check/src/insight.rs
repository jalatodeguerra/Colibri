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

use colibri_syntax::{FileId, Span};

use crate::checker::Checker;
use crate::hir::{
    Anchor, Callee, Expr, ExprKind, FieldIdx, FnId, GlobalId, HostFnId, Place, TypeDef,
    TypeDefKind, ValueProj,
};
use crate::host::HOST_FILE;
use crate::prelude;
use crate::types::{FIRST_USER_TYPE, Ty, TypeId};

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
}

impl Insight {
    /// Every fact, in no particular order.
    pub fn facts(&self) -> &[Fact] {
        &self.facts
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
    fn show_ty(&self, ty: &Ty) -> String {
        ty.render(&self.types)
    }

    /// `fn name[T](a: int, var self) -> int` for a function of the program.
    fn signature(&self, f: FnId) -> String {
        let info = &self.fns[f.0 as usize];
        let mut out = format!("fn {}", info.name);
        if !info.generics.is_empty() {
            let names: Vec<&str> = info.generics.iter().map(|g| g.name.as_str()).collect();
            out.push_str(&format!("[{}]", names.join(", ")));
        }
        let params: Vec<String> = info
            .params
            .iter()
            .enumerate()
            .map(|(i, p)| match (i, info.receiver) {
                (0, Some(true)) => "var self".to_string(),
                (0, Some(false)) => "self".to_string(),
                _ => format!("{}: {}", p.name, self.show_ty(&p.ty)),
            })
            .collect();
        out.push_str(&format!("({})", params.join(", ")));
        if info.ret != Ty::Unit {
            out.push_str(&format!(" -> {}", self.show_ty(&info.ret)));
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
        format!("{kw} {}", def.name)
    }

    /// Turns what the zonker recorded into facts, and adds the declarations of items.
    pub(crate) fn build_insight(&self, raw: Vec<RawFact>) -> Insight {
        let mut facts = Vec::new();
        for r in raw {
            let ty = self.show_ty(&r.ty);
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
                        text: format!("{}.{}: {ty}", def.name, variant.name),
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
        Insight { facts }
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
                    text: format!("{}.{}", def.name, v.name),
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
            dependency: false,
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
}
