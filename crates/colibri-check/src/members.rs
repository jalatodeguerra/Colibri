//! What can follow a dot on a value of a type: its fields and methods, for editors
//! (completion). [`Checker::members_of`] collects them from the checker's tables; the
//! built-in types' methods come from the same signature tables the call checker uses, so the
//! two cannot disagree.

use crate::checker::Checker;
use crate::hir::{FnId, TypeDefKind};
use crate::traits::{key_of, match_ty};
use crate::types::{EQ, FIRST_USER_TYPE, HASH, ORD, ParamId, Ty, TypeId};

/// A method of a built-in type with its types filled in for one receiver.
pub(crate) struct MethodSig {
    pub name: String,
    pub params: Vec<(String, Ty)>,
    pub ret: Ty,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemberKind {
    Field,
    Method,
}

/// A field or method that a value of some type has.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    pub name: String,
    pub kind: MemberKind,
    /// `x: float` for a field, `fn scaled(k: float) -> Point` for a method (without the
    /// receiver).
    pub detail: String,
    /// Marked `pub`. Always true for the built-in types.
    pub public: bool,
    /// The package (an index into the program's packages) that declares it, when it is one of
    /// the program's source modules. Something not `pub` is visible only inside that package.
    pub package: Option<usize>,
}

impl Checker<'_> {
    fn sig_detail(&self, name: &str, params: &[(String, Ty)], ret: &Ty) -> String {
        let params: Vec<String> = params
            .iter()
            .map(|(n, t)| format!("{n}: {}", self.show_ty(t)))
            .collect();
        let mut out = format!("fn {name}({})", params.join(", "));
        if *ret != Ty::Unit {
            out.push_str(&format!(" -> {}", self.show_ty(ret)));
        }
        out
    }

    /// The method `f` as a member of a value of type `ty`: the type parameters of its impl
    /// are replaced by what `ty` has for them (`Box[Sq]`'s `get` is `fn get() -> Sq`).
    fn method_member(&self, f: FnId, ty: &Ty) -> Member {
        let info = &self.fns[f.0 as usize];
        let name = info.name.rsplit('.').next().unwrap_or(&info.name);
        let mut bound: Vec<(ParamId, Ty)> = Vec::new();
        if info.receiver.is_some()
            && let Some(receiver) = info.params.first()
        {
            let ids: Vec<ParamId> = info.generics.iter().map(|g| g.id).collect();
            if !match_ty(&receiver.ty, ty, &ids, &mut bound) {
                bound.clear();
            }
        }
        Member {
            name: name.to_string(),
            kind: MemberKind::Method,
            detail: self.signature_of(f, name, true, &bound),
            public: info.public,
            package: self.mods[info.module.0 as usize].package,
        }
    }

    /// The fields and methods of `ty`, sorted by name, each once (an inherent method hides an
    /// interface method of the same name, as in a call).
    pub(crate) fn members_of(&self, ty: &Ty) -> Vec<Member> {
        let mut out: Vec<Member> = Vec::new();
        let builtin = |m: crate::members::MethodSig| Member {
            detail: self.sig_detail(&m.name, &m.params, &m.ret),
            name: m.name,
            kind: MemberKind::Method,
            public: true,
            package: None,
        };
        match ty {
            Ty::Adt(id, args) if id.0 >= FIRST_USER_TYPE => {
                self.adt_members(*id, args, ty, &mut out)
            }
            Ty::Dyn(iface, _) => self.iface_members(*iface, &mut out),
            Ty::Param(p, _) => {
                let bounds = self.param_bounds.get(p).cloned().unwrap_or_default();
                for b in bounds {
                    self.iface_members(b.iface, &mut out);
                }
            }
            Ty::Int(_) | Ty::Float | Ty::F32 => {
                out.extend(crate::numeric::num_methods(ty).into_iter().map(builtin));
            }
            Ty::Vec(v) => out.extend(crate::vector::vec_methods(*v).into_iter().map(builtin)),
            Ty::Error | Ty::Infer(_) | Ty::Never | Ty::Unit | Ty::Fn(..) => return Vec::new(),
            _ => out.extend(
                crate::builtins::builtin_methods(ty)
                    .into_iter()
                    .map(builtin),
            ),
        }
        if !out.iter().any(|m| m.name == "clone") && !matches!(ty, Ty::Dyn(..)) {
            out.push(Member {
                name: "clone".into(),
                kind: MemberKind::Method,
                detail: format!("fn clone() -> {}", self.show_ty(ty)),
                public: true,
                package: None,
            });
        }
        out.sort_by(|a, b| {
            a.name
                .cmp(&b.name)
                .then((a.kind as u8).cmp(&(b.kind as u8)))
        });
        out.dedup_by(|b, a| a.name == b.name);
        out
    }

    fn iface_members(&self, iface: TypeId, out: &mut Vec<Member>) {
        let Some(info) = self.ifaces.get(&iface) else {
            return;
        };
        for m in &info.methods {
            if m.receiver.is_none() {
                continue;
            }
            let params: Vec<(String, Ty)> = m
                .params
                .iter()
                .skip(1)
                .map(|p| (p.name.clone(), p.ty.clone()))
                .collect();
            out.push(Member {
                name: m.name.clone(),
                kind: MemberKind::Method,
                detail: self.sig_detail(&m.name, &params, &m.ret),
                public: true,
                package: None,
            });
        }
    }

    fn adt_members(&self, id: TypeId, args: &[Ty], ty: &Ty, out: &mut Vec<Member>) {
        let def = &self.types[id.0 as usize];
        let extra = &self.type_extra[id.0 as usize];
        let package = extra.module.and_then(|m| self.mods[m.0 as usize].package);
        if matches!(
            def.kind,
            TypeDefKind::Struct { .. } | TypeDefKind::Class { .. }
        ) {
            let map = Checker::param_map_of(def, args);
            for (i, f) in def.fields().iter().enumerate() {
                out.push(Member {
                    name: f.name.clone(),
                    kind: MemberKind::Field,
                    detail: format!("{}: {}", f.name, self.show_ty(&f.ty.subst(&map))),
                    public: extra.field_public.get(i).copied().unwrap_or(true),
                    package,
                });
            }
        }
        // Inherent methods first: they hide interface methods of the same name.
        let mut inherent: Vec<FnId> = self
            .methods
            .iter()
            .filter(|((t, _), f)| *t == id && self.fns[f.0 as usize].receiver.is_some())
            .map(|(_, f)| *f)
            .collect();
        inherent.sort();
        out.extend(inherent.into_iter().map(|f| self.method_member(f, ty)));
        // Methods of the interfaces the type implements.
        let Some(key) = key_of(ty) else { return };
        for imp in self.impls.iter().filter(|i| i.key == key) {
            let params: Vec<ParamId> = imp.generics.iter().map(|g| g.id).collect();
            if !match_ty(&imp.target, ty, &params, &mut Vec::new()) {
                continue;
            }
            let Some(iface) = self.ifaces.get(&imp.iface) else {
                continue;
            };
            for (i, m) in iface.methods.iter().enumerate() {
                if m.receiver.is_none() {
                    continue;
                }
                match imp.methods.get(i).copied().flatten().or(m.default) {
                    Some(f) => {
                        let mut member = self.method_member(f, ty);
                        member.name = m.name.clone();
                        out.push(member);
                    }
                    None => self.iface_members(imp.iface, out),
                }
            }
        }
        // `eq`, `cmp` and `hash` derived from the structure.
        for (iface, name) in [(EQ, "eq"), (ORD, "cmp"), (HASH, "hash")] {
            let derived = matches!(
                self.env().implements(ty, iface, &[]),
                Ok(crate::traits::Source::Derived)
            );
            if derived && let Some(m) = self.ifaces.get(&iface).and_then(|i| i.methods.first()) {
                let params: Vec<(String, Ty)> = m
                    .params
                    .iter()
                    .skip(1)
                    .map(|p| (p.name.clone(), ty.clone()))
                    .collect();
                out.push(Member {
                    name: name.to_string(),
                    kind: MemberKind::Method,
                    detail: self.sig_detail(name, &params, &m.ret),
                    public: true,
                    package: None,
                });
            }
        }
    }
}
