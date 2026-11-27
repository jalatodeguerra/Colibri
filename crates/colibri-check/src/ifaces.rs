//! Interfaces (DESIGN §9.2): declarations, `impl Interface for Type` blocks and their
//! validation, and the derived implementations of the built-in interfaces.
//!
//! An interface method is checked once, abstractly, against `Self` (a type parameter bounded
//! by the interface). An `impl` must provide every method that has no default, with exactly
//! the signature the interface asks for once `Self` and the interface's own type parameters
//! are replaced by the implementing type and the `impl`'s arguments.

use colibri_syntax::ast::{self, ItemKind};
use colibri_syntax::{Diagnostic, Span};

use crate::checker::{Checker, ParamInfo};
use crate::deprecated::DeprKey;
use crate::generics::SigCtx;
use crate::hir::{FnId, IfaceMethod, ImplId, TypeDefKind};
use crate::prelude;
use crate::traits::{BoundRef, GenericParamInfo, ImplInfo, key_of};
use crate::types::{
    DROP, EQ, FIRST_USER_TYPE, INTO_ERROR, IntTy, ORD, ParamId, Ty, TypeId, is_binary_operator,
    is_operator,
};

/// An interface as the checker keeps it.
pub(crate) struct IfaceInfo<'a> {
    pub name: String,
    /// The interface's own type parameters (`T` in `Iter[T]`).
    pub generics: Vec<GenericParamInfo>,
    /// `Self`: a type parameter bounded by the interface itself.
    pub self_param: GenericParamInfo,
    pub methods: Vec<IfaceMethodInfo<'a>>,
}

/// One method of an interface, as a signature over `Self` and the interface's parameters.
#[derive(Clone)]
pub(crate) struct IfaceMethodInfo<'a> {
    pub name: String,
    pub name_span: Span,
    /// `Self`, the interface's parameters and the method's own, in that order.
    pub generics: Vec<GenericParamInfo>,
    /// Including the receiver, whose type is `Self`.
    pub params: Vec<ParamInfo<'a>>,
    pub receiver: Option<bool>,
    pub ret: Ty,
    /// The function that runs when an implementation does not provide the method.
    pub default: Option<FnId>,
}

impl IfaceMethodInfo<'_> {
    /// How many type parameters the method has of its own.
    pub fn own_generics(&self, iface_params: usize) -> usize {
        self.generics.len().saturating_sub(1 + iface_params)
    }
}

/// Whether `ty` mentions the type parameter `p`.
pub(crate) fn mentions(ty: &Ty, p: ParamId) -> bool {
    match ty {
        Ty::Param(q, _) => *q == p,
        Ty::Tuple(ts) | Ty::Adt(_, ts) | Ty::Dyn(_, ts) => ts.iter().any(|t| mentions(t, p)),
        Ty::List(t) | Ty::Set(t) | Ty::Option(t) | Ty::Ptr(t) => mentions(t, p),
        Ty::Map(a, b) => mentions(a, p) || mentions(b, p),
        Ty::Fn(ps, r) => ps.iter().any(|t| mentions(t, p)) || mentions(r, p),
        _ => false,
    }
}

impl<'a> Checker<'a> {
    /// Declares the interface `id`: its `Self` parameter and an empty method list.
    pub(crate) fn declare_interface(&mut self, id: TypeId, decl: &'a ast::InterfaceDecl) {
        let generics = self.type_extra[id.0 as usize].generics.clone();
        let self_param = GenericParamInfo {
            id: self.new_param(),
            name: "Self".to_string(),
            span: decl.name.span,
            default: None,
            bounds: vec![BoundRef {
                iface: id,
                args: generics.iter().map(GenericParamInfo::ty).collect(),
                span: decl.name.span,
                implied: false,
            }],
        };
        self.param_bounds
            .insert(self_param.id, self_param.bounds.clone());
        self.ifaces.insert(
            id,
            IfaceInfo {
                name: decl.name.name.clone(),
                generics,
                self_param,
                methods: Vec::new(),
            },
        );
    }

    /// Resolves the signatures of an interface's methods, and creates the functions of its
    /// default methods.
    pub(crate) fn define_interface(&mut self, id: TypeId, decl: &'a ast::InterfaceDecl) {
        let Some(info) = self.ifaces.get(&id) else {
            return;
        };
        let (generics, self_param) = (info.generics.clone(), info.self_param.clone());
        let mut outer = vec![self_param.clone()];
        outer.extend(generics);
        let mark = self.enter_generics(&outer);
        let ctx = SigCtx {
            self_ty: Some(self_param.ty()),
            outer,
            owner: Some(decl.name.name.clone()),
            class: false,
        };
        let mut methods: Vec<IfaceMethodInfo<'a>> = Vec::new();
        for member in &decl.items {
            let ItemKind::Fn(f) = &member.kind else {
                self.report(Diagnostic::error(
                    "an interface can only declare methods",
                    member.span,
                ));
                continue;
            };
            if let Some(prev) = methods.iter().find(|m| m.name == f.name.name) {
                let prev_span = prev.name_span;
                self.report(
                    Diagnostic::error(
                        format!("the method `{}` is declared more than once", f.name.name),
                        f.name.span,
                    )
                    .with_label(prev_span, "first declared here"),
                );
                continue;
            }
            if f.receiver.is_none() {
                self.report(
                    Diagnostic::error(
                        format!("the interface method `{}` needs a `self` receiver", f.name.name),
                        f.name.span,
                    )
                    .with_hint("an interface describes what a value can do, so every method acts on `self`; static functions are not supported in interfaces yet"),
                );
                continue;
            }
            let index = methods.len() as u32;
            let method = if f.body.is_some() {
                let fid = self.add_fn(member, f, &ctx);
                // A call through an interface value or a bound finds the method by its place
                // in the interface, so the deprecation is kept under that key too.
                if let Some(dep) = self.deprecated.get(&DeprKey::Fn(fid)).cloned() {
                    self.deprecated.insert(DeprKey::IfaceMethod(id, index), dep);
                }
                let info = &self.fns[fid.0 as usize];
                IfaceMethodInfo {
                    name: f.name.name.clone(),
                    name_span: f.name.span,
                    generics: info.generics.clone(),
                    params: info.params.clone(),
                    receiver: info.receiver,
                    ret: info.ret.clone(),
                    default: Some(fid),
                }
            } else {
                let key = DeprKey::IfaceMethod(id, index);
                self.check_member_attrs(member);
                self.declare_deprecation(
                    key,
                    &format!("{}.{}", decl.name.name, f.name.name),
                    &member.attrs,
                );
                let saved = std::mem::replace(&mut self.self_ty, ctx.self_ty.clone());
                let sig = self.with_depr_ctx(&[key], |ck| ck.resolve_sig(f, &ctx));
                self.self_ty = saved;
                IfaceMethodInfo {
                    name: f.name.name.clone(),
                    name_span: f.name.span,
                    generics: sig.generics,
                    params: sig.params,
                    receiver: sig.receiver,
                    ret: sig.ret,
                    default: None,
                }
            };
            methods.push(method);
        }
        self.leave_generics(mark);
        let hir_methods: Vec<IfaceMethod> = methods
            .iter()
            .map(|m| IfaceMethod {
                name: m.name.clone(),
                span: m.name_span,
                receiver: m.receiver,
                default: m.default,
            })
            .collect();
        self.types[id.0 as usize].kind = TypeDefKind::Interface {
            methods: hir_methods,
        };
        if let Some(info) = self.ifaces.get_mut(&id) {
            info.methods = methods;
        }
    }

    /// Why a method cannot be called through an interface value (an object-safety problem,
    /// DESIGN §9.2), or `None` when it can.
    pub(crate) fn object_unsafety(&self, iface: TypeId, method: usize) -> Option<String> {
        let info = self.ifaces.get(&iface)?;
        let m = info.methods.get(method)?;
        let own = m.own_generics(info.generics.len());
        if own > 0 {
            return Some(format!(
                "`{}` has type parameters of its own, and an interface value does not know which types to run it with (native builds generate one copy of the method per type)",
                m.name
            ));
        }
        let self_id = info.self_param.id;
        let uses_self =
            m.params.iter().skip(1).any(|p| mentions(&p.ty, self_id)) || mentions(&m.ret, self_id);
        uses_self.then(|| {
            format!(
                "`{}` mentions `Self` outside its receiver, and the concrete type behind an interface value is not known statically",
                m.name
            )
        })
    }

    // ---- impl Interface for Type ----

    pub(crate) fn collect_iface_impl(
        &mut self,
        decl: &'a ast::ImplDecl,
        item: &'a ast::Item,
        iface_expr: &ast::TypeExpr,
        target: Ty,
        generics: &[GenericParamInfo],
    ) {
        // A type argument left out of the header takes the interface's default, with `Self`
        // standing for the implementing type.
        self.default_self = Some(target.clone());
        let resolved = self.resolve_type(iface_expr);
        self.default_self = None;
        let (iface, iface_args) = match resolved {
            Ty::Dyn(id, args) => (id, args),
            Ty::Error => return,
            other => {
                let shown = self.render(&other);
                self.report(
                    Diagnostic::error(format!("`{shown}` is not an interface"), iface_expr.span)
                        .with_hint("`impl Interface for Type` needs an interface before `for`"),
                );
                return;
            }
        };
        if target.is_error() {
            return;
        }
        let iface_name = self.types[iface.0 as usize].name.clone();
        let target_shown = self.render(&target);
        // Dispatch on a generic receiver reads the type from the value, and a `Value::Int` of
        // the narrow widths carries none (ARCHITECTURE §6).
        // An operator impl is reached from the checker's own choice of impl, never by the
        // runtime type, so it may be written for the narrow widths too.
        if let Ty::Int(t) = &target
            && !matches!(t, IntTy::I64 | IntTy::U64)
            && !is_operator(iface)
        {
            self.unsupported(&format!("`impl` for `{target_shown}`"), decl.target.span);
            return;
        }
        let Some(key) = key_of(&target) else {
            self.report(
                Diagnostic::error(
                    format!("`{iface_name}` cannot be implemented for `{target_shown}`"),
                    decl.target.span,
                )
                .with_hint("only concrete types can implement an interface: structs, enums, classes, tuples, collections and the primitive types"),
            );
            return;
        };
        // The orphan rule (DESIGN §9.2): the interface or the type must be ours. Everything
        // user-written is one package for now, so only built-in against built-in is refused.
        let target_builtin = !matches!(&target, Ty::Adt(id, _) if id.0 >= FIRST_USER_TYPE);
        // An operator interface may also be implemented for a number type when the right-hand
        // type `R` (its first argument) is a type of the program: `impl Mul[Vec3, Vec3] for
        // float` is the `float` side of `2.0 * v`, and cannot change what `2.0 * 3.0` means. The
        // output type does not count, and neither does a type parameter `R`: either could be a
        // built-in type, and a built-in operator must never be replaceable. Only numbers can be
        // the left operand of a user type, so no other built-in type qualifies.
        // A type of a standard library module, written in Colibri (`vec.vec2`) or by the host
        // (`datetime.DateTime`), is the library's: the library may add impls for it later, so a program
        // cannot write one of the built-in interfaces for it, and a type of the library is not
        // "a type of the program" for the rule above. (The library's own sources are exempt.)
        let in_std = self.std_files.contains(&iface_expr.span.file);
        let of_std = |ck: &Self, t: &Ty| {
            matches!(t, Ty::Adt(id, _) if id.0 >= FIRST_USER_TYPE && {
                let def = &ck.types[id.0 as usize];
                ck.std_files.contains(&def.span.file)
                    || def.module.as_deref().is_some_and(|m| m.starts_with("std."))
            })
        };
        let program_ty = |ck: &Self, t: &Ty| {
            matches!(t, Ty::Adt(id, _) if id.0 >= FIRST_USER_TYPE) && (in_std || !of_std(ck, t))
        };
        let std_target = !in_std && of_std(self, &target);
        let theirs = is_binary_operator(iface)
            && (std_target || matches!(&target, Ty::Int(_) | Ty::Float | Ty::F32))
            && iface_args.first().is_some_and(|a| program_ty(self, a));
        if iface.0 < FIRST_USER_TYPE && std_target && !theirs {
            self.report(
                Diagnostic::error(
                    format!("cannot implement the built-in `{iface_name}` for `{target_shown}`, which belongs to the standard library"),
                    iface_expr.span,
                )
                .with_hint(if is_binary_operator(iface) {
                    format!("the orphan rule: the standard library may add impls for its own types, so `{target_shown}` gets an operator impl only when its right-hand type is yours (`impl {iface_name}[YourType] for {target_shown}`); wrap it in your own struct, or declare your own interface")
                } else {
                    format!("the orphan rule: `impl I for T` must be written in the package that defines `I` or `T`, and `{target_shown}` belongs to the standard library; wrap it in your own struct, or declare your own interface")
                }),
            );
            return;
        }
        if iface.0 < FIRST_USER_TYPE && target_builtin && !theirs {
            let std_rhs = iface_args.first().is_some_and(|a| of_std(self, a)) && !in_std;
            self.report(
                Diagnostic::error(
                    format!("cannot implement the built-in `{iface_name}` for the built-in type `{target_shown}`"),
                    iface_expr.span,
                )
                .with_hint(if std_rhs && is_binary_operator(iface) {
                    format!("the orphan rule: the right-hand type of `impl {iface_name}[..] for {target_shown}` must be a type of your own; a type of the standard library does not count, because the library may add that impl later")
                } else if is_binary_operator(iface) {
                    format!("the orphan rule: `impl I for T` must be written in the package that defines `I` or `T`; an operator interface may also be implemented for a number type when its right-hand type is yours (`impl {iface_name}[YourType, YourType] for float`); the output type and a type parameter do not count")
                } else {
                    "the orphan rule: `impl I for T` must be written in the package that defines `I` or `T`; wrap the type in your own struct, or declare your own interface".to_string()
                }),
            );
            return;
        }
        // The enums of a host module are the module's: the standard library may add variants and
        // implementations to them later, so a built-in interface cannot be implemented for one
        // here. (`IntoError` already works for them: `err(fs.Error.NotFound)` and `?`.)
        if iface.0 < FIRST_USER_TYPE
            && matches!(&target, Ty::Adt(id, _) if self.types[id.0 as usize].is_open_enum())
        {
            self.report(
                Diagnostic::error(
                    format!("cannot implement the built-in `{iface_name}` for `{target_shown}`"),
                    iface_expr.span,
                )
                .with_hint(if iface == INTO_ERROR {
                    format!("the orphan rule: `{target_shown}` belongs to a host module; it already converts to `Error` (the message is the variant's name), so `err({target_shown}.Variant)` and `?` work as they are")
                } else {
                    format!("the orphan rule: `impl I for T` must be written in the package that defines `I` or `T`, and `{target_shown}` belongs to a host module; wrap it in your own struct, or declare your own interface")
                }),
            );
            return;
        }
        if iface == DROP
            && matches!(&target, Ty::Adt(id, _) if self.types[id.0 as usize].is_host_class())
        {
            self.report(
                Diagnostic::error(
                    format!("`Drop` cannot be implemented for the host class `{target_shown}`"),
                    iface_expr.span,
                )
                .with_hint("the host owns the object and its Rust `Drop` runs when it is freed; hold the object in a class of your own to run script code at that moment"),
            );
            return;
        }
        if iface == DROP
            && !matches!(&target, Ty::Adt(id, _) if self.types[id.0 as usize].is_class())
        {
            self.report(
                Diagnostic::error(
                    format!("`Drop` can only be implemented for a `class`, not for `{target_shown}`"),
                    iface_expr.span,
                )
                .with_hint("a struct or enum is copied freely, so a destructor would run once per copy; hold the resource in a class field instead"),
            );
            return;
        }
        // An operator interface is implemented once for each right-hand type; the others once.
        let rhs = is_binary_operator(iface)
            .then(|| iface_args.first().and_then(key_of))
            .flatten();
        let taken = if is_binary_operator(iface) {
            self.op_impls.get(&(key, iface)).and_then(|list| {
                list.iter()
                    .find(|(r, _)| r.is_none() || rhs.is_none() || *r == rhs)
                    .map(|(_, id)| *id)
            })
        } else {
            self.impl_by_key.get(&(key, iface)).copied()
        };
        if let Some(prev) = taken {
            let prev_span = self.impls[prev.0 as usize].span;
            self.report(
                Diagnostic::error(
                    format!("`{iface_name}` is implemented for `{target_shown}` more than once"),
                    iface_expr.span,
                )
                .with_label(prev_span, "the first implementation")
                .with_hint(if is_binary_operator(iface) {
                    "a type has at most one implementation of an operator for each right-hand type, and right-hand types are told apart by their head only: `Add[Box[int]]` and `Add[Box[str]]` count as the same one (a right-hand type that is a type parameter is the only one)"
                } else {
                    "one type has at most one implementation of an interface, whatever its type arguments"
                }),
            );
            return;
        }
        let method_infos: Vec<IfaceMethodInfo<'a>> = self
            .ifaces
            .get(&iface)
            .map(|i| i.methods.clone())
            .unwrap_or_default();
        let id = ImplId(self.impls.len() as u32);
        self.impls.push(ImplInfo {
            iface,
            iface_args: iface_args.clone(),
            target: target.clone(),
            key,
            generics: generics.to_vec(),
            methods: vec![None; method_infos.len()],
            span: item.span,
            derived: false,
        });
        self.impl_by_key.entry((key, iface)).or_insert(id);
        if is_binary_operator(iface) {
            self.op_impls
                .entry((key, iface))
                .or_default()
                .push((rhs, id));
        }

        let owner = match &target {
            Ty::Adt(t, _) => self.types[t.0 as usize].name.clone(),
            other => self.render(other),
        };
        let class = matches!(&target, Ty::Adt(t, _) if self.types[t.0 as usize].is_class());
        let ctx = SigCtx {
            self_ty: Some(target.clone()),
            outer: generics.to_vec(),
            owner: Some(owner),
            class,
        };
        let mut provided: Vec<Option<FnId>> = vec![None; method_infos.len()];
        for member in &decl.items {
            let ItemKind::Fn(f) = &member.kind else {
                continue;
            };
            let Some(index) = method_infos.iter().position(|m| m.name == f.name.name) else {
                self.report_extra_method(f, &iface_name, &method_infos, &target_shown);
                // Still resolve it, so mistakes inside it are reported once.
                self.add_fn(member, f, &ctx);
                continue;
            };
            let fid = self.add_fn(member, f, &ctx);
            if provided[index].is_some() {
                self.report(Diagnostic::error(
                    format!(
                        "the method `{}` is defined more than once in this impl",
                        f.name.name
                    ),
                    f.name.span,
                ));
                continue;
            }
            self.check_method_signature(
                fid,
                &method_infos[index],
                &iface_name,
                &iface_args,
                &target,
                generics,
            );
            provided[index] = Some(fid);
        }
        let missing: Vec<&IfaceMethodInfo<'a>> = method_infos
            .iter()
            .zip(&provided)
            .filter(|(m, p)| m.default.is_none() && p.is_none())
            .map(|(m, _)| m)
            .collect();
        if !missing.is_empty() {
            let names: Vec<String> = missing.iter().map(|m| format!("`{}`", m.name)).collect();
            let plural = if missing.len() == 1 { "" } else { "s" };
            let mut diag = Diagnostic::error(
                format!(
                    "`impl {iface_name} for {target_shown}` is missing the method{plural} {}",
                    names.join(", ")
                ),
                iface_expr.span,
            );
            for m in &missing {
                diag = diag.with_label(
                    m.name_span,
                    format!("`{}` is declared here without a default", m.name),
                );
            }
            self.report(diag.with_hint(format!(
                "add {} to the impl, or give the interface a default body",
                if missing.len() == 1 { "it" } else { "them" }
            )));
        }
        self.impls[id.0 as usize].methods = provided;
    }

    fn report_extra_method(
        &mut self,
        f: &ast::FnDecl,
        iface: &str,
        methods: &[IfaceMethodInfo<'a>],
        target: &str,
    ) {
        let names: Vec<&str> = methods.iter().map(|m| m.name.as_str()).collect();
        let mut diag = Diagnostic::error(
            format!("`{}` is not a method of `{iface}`", f.name.name),
            f.name.span,
        );
        let hint = match crate::suggest::closest(&f.name.name, names.iter().copied()) {
            Some(s) => format!("did you mean `{s}`?"),
            None => format!(
                "an impl of `{iface}` defines only its methods ({}); put other methods in `impl {target} {{ ... }}`",
                names.join(", ")
            ),
        };
        diag = diag.with_hint(hint);
        self.report(diag);
    }

    /// Compares the signature of an impl's method with what the interface asks for.
    fn check_method_signature(
        &mut self,
        fid: FnId,
        wanted: &IfaceMethodInfo<'a>,
        iface_name: &str,
        iface_args: &[Ty],
        target: &Ty,
        impl_generics: &[GenericParamInfo],
    ) {
        let info = &self.fns[fid.0 as usize];
        let (got_params, got_ret, got_recv, name_span) = (
            info.params.clone(),
            info.ret.clone(),
            info.receiver,
            info.name_span,
        );
        let own_got: Vec<GenericParamInfo> =
            info.generics[impl_generics.len().min(info.generics.len())..].to_vec();
        let iface_param_count = iface_args.len();
        // Self, the interface's parameters and the method's own parameters, in the impl's terms.
        let self_id = wanted.generics.first().map(|g| g.id);
        let mut map: Vec<(ParamId, Ty)> = Vec::new();
        if let Some(id) = self_id {
            map.push((id, target.clone()));
        }
        for (g, a) in wanted.generics.iter().skip(1).zip(iface_args) {
            map.push((g.id, a.clone()));
        }
        let own_wanted = &wanted.generics[(1 + iface_param_count).min(wanted.generics.len())..];
        for (w, g) in own_wanted.iter().zip(&own_got) {
            map.push((w.id, g.ty()));
        }
        let exp_params: Vec<Ty> = wanted
            .params
            .iter()
            .skip(1)
            .map(|p| p.ty.subst(&map))
            .collect();
        let exp_ret = wanted.ret.subst(&map);
        let got_tys: Vec<Ty> = got_params.iter().skip(1).map(|p| p.ty.clone()).collect();
        // A class's `self` can always change its object, so it may implement a `mut self` method.
        let class = matches!(target, Ty::Adt(id, _) if self.types[id.0 as usize].is_class());
        let receiver_ok = wanted.receiver == got_recv
            || (class && wanted.receiver == Some(true) && got_recv == Some(false));
        // A parameter whose type is already an error was reported where it was written; a second
        // complaint that the signature differs would only repeat it.
        if got_tys.iter().any(Ty::is_error) {
            return;
        }
        let same = own_wanted.len() == own_got.len()
            && receiver_ok
            && exp_params == got_tys
            && (exp_ret == got_ret || exp_ret.is_error() || got_ret.is_error());
        if same {
            self.check_method_bounds(
                own_wanted,
                &own_got,
                &map,
                name_span,
                iface_name,
                &wanted.name,
            );
            return;
        }
        let expected = self.render_signature(
            &wanted.name,
            wanted.receiver,
            own_wanted.iter().map(|g| g.name.as_str()).collect(),
            &wanted
                .params
                .iter()
                .skip(1)
                .zip(&exp_params)
                .map(|(p, t)| (p.name.clone(), t.clone()))
                .collect::<Vec<_>>(),
            &exp_ret,
        );
        let found = self.render_signature(
            &wanted.name,
            got_recv,
            own_got.iter().map(|g| g.name.as_str()).collect(),
            &got_params
                .iter()
                .skip(1)
                .map(|p| (p.name.clone(), p.ty.clone()))
                .collect::<Vec<_>>(),
            &got_ret,
        );
        let diag = Diagnostic::error(
            format!(
                "the method `{}` does not match the signature in `{iface_name}`: expected `{expected}`, found `{found}`",
                wanted.name
            ),
            name_span,
        )
        .with_label(wanted.name_span, format!("`{iface_name}` declares it here"));
        self.report(diag);
    }

    /// A method of an impl may ask less of its own type parameters than the interface does, but
    /// not more: a caller that only knows the interface (`m.conv(x)` with `M: Mk`) has checked
    /// `x` against the interface's bounds, so a stricter bound in the impl would be assumed to
    /// hold for a type nothing checked (a closure where the impl wants `Ord`).
    ///
    /// Each bound the impl writes on one of the method's own parameters must be one the
    /// interface writes on the matching parameter (`Ord` also gives `Eq`). `Show` and `Clone` are no
    /// exception: a host object cannot be cloned, and a user's `impl[T: Ord] Show for W[T]` holds
    /// only for some `W[T]`. `map` takes the interface's parameters to the impl's terms.
    fn check_method_bounds(
        &mut self,
        own_wanted: &[GenericParamInfo],
        own_got: &[GenericParamInfo],
        map: &[(ParamId, Ty)],
        name_span: Span,
        iface_name: &str,
        method: &str,
    ) {
        for (w, g) in own_wanted.iter().zip(own_got) {
            for b in &g.bounds {
                if b.implied {
                    continue;
                }
                let granted = w.bounds.iter().any(|wb| {
                    let args: Vec<Ty> = wb.args.iter().map(|a| a.subst(map)).collect();
                    (wb.iface == b.iface && args == b.args)
                        || (b.iface == EQ && wb.iface == ORD && args.is_empty())
                });
                if granted {
                    continue;
                }
                let bound = self.render(&Ty::Dyn(b.iface, b.args.clone()));
                self.report(
                    Diagnostic::error(
                        format!(
                            "the method `{method}` asks more of `{}` than `{iface_name}` does: it needs `{}: {bound}`",
                            g.name, g.name
                        ),
                        b.span,
                    )
                    .with_label(name_span, format!("implements `{iface_name}.{method}`"))
                    .with_hint(format!(
                        "a caller that only knows `{iface_name}` cannot promise it; add `{bound}` to the bounds in the interface, or remove it here"
                    )),
                );
            }
        }
    }

    /// `fn area[T](self, scale: float) -> float`, for messages.
    pub(crate) fn render_signature(
        &self,
        name: &str,
        receiver: Option<bool>,
        generics: Vec<&str>,
        params: &[(String, Ty)],
        ret: &Ty,
    ) -> String {
        let mut parts: Vec<String> = Vec::new();
        match receiver {
            Some(true) => parts.push("mut self".to_string()),
            Some(false) => parts.push("self".to_string()),
            None => {}
        }
        parts.extend(
            params
                .iter()
                .map(|(n, t)| format!("{n}: {}", self.render(t))),
        );
        let gens = if generics.is_empty() {
            String::new()
        } else {
            format!("[{}]", generics.join(", "))
        };
        let ret = if *ret == Ty::Unit {
            String::new()
        } else {
            format!(" -> {}", self.render(ret))
        };
        format!("fn {name}{gens}({}){ret}", parts.join(", "))
    }

    // ---- derived implementations ----

    /// The implementation of the built-in interface `iface` that `ty` has, as an id the engine
    /// can dispatch through: its explicit `impl`, or a derived one made on first use.
    pub(crate) fn impl_of(&mut self, ty: &Ty, iface: TypeId) -> Option<ImplId> {
        let key = key_of(ty)?;
        if let Some(&id) = self.impl_by_key.get(&(key, iface)) {
            return Some(id);
        }
        if let Some(&id) = self.derived_impls.get(&(key, iface)) {
            return Some(id);
        }
        let methods = self.types[iface.0 as usize].iface_methods().len();
        let id = ImplId(self.impls.len() as u32);
        self.impls.push(ImplInfo {
            iface,
            iface_args: Vec::new(),
            target: ty.clone(),
            key,
            generics: Vec::new(),
            methods: vec![None; methods],
            span: Span::new(prelude::PRELUDE_FILE, 0, 0),
            derived: true,
        });
        self.derived_impls.insert((key, iface), id);
        Some(id)
    }
}
