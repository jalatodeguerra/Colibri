//! Type parameters in scope, their bounds, and function signatures (DESIGN §9.1).
//!
//! Every declared type parameter gets a program-wide [`ParamId`]. While a declaration's
//! signature or body is checked, its parameters are in `Checker::type_scope`, so a type
//! written in source resolves `T` to `Ty::Param`. A generic body is checked once, with its
//! parameters opaque and their bounds as the only thing known about them (like Rust and
//! Swift, unlike C++ templates).

use colibri_syntax::ast;
use colibri_syntax::{Diagnostic, Span};

use crate::checker::{Checker, ParamInfo};
use crate::traits::{BoundRef, GenericParamInfo};
use crate::types::{ParamId, Ty};

/// What a signature is declared inside of.
#[derive(Clone, Default)]
pub(crate) struct SigCtx {
    /// What `Self` means, and the type of a `self` receiver.
    pub self_ty: Option<Ty>,
    /// The type parameters of the enclosing `impl` or interface; they are already in scope.
    pub outer: Vec<GenericParamInfo>,
    /// The enclosing type's name, for `Type.method` names.
    pub owner: Option<String>,
    /// The method belongs to a class (`mut self` is an error there).
    pub class: bool,
}

/// A resolved function signature.
pub(crate) struct Sig<'a> {
    /// Every type parameter in scope: the enclosing ones first.
    pub generics: Vec<GenericParamInfo>,
    /// All parameters; a method's first one is `self`.
    pub params: Vec<ParamInfo<'a>>,
    pub receiver: Option<bool>,
    pub ret: Ty,
    pub ret_span: Option<Span>,
    /// The declaration cannot be checked (it has no body, or something in it is unsupported).
    pub poisoned: bool,
}

impl Checker<'_> {
    /// A fresh program-wide type parameter id.
    pub fn new_param(&mut self) -> ParamId {
        let id = ParamId(self.next_param);
        self.next_param += 1;
        id
    }

    /// Puts `generics` in scope until [`Checker::leave_generics`] is given the returned mark.
    pub fn enter_generics(&mut self, generics: &[GenericParamInfo]) -> usize {
        let mark = self.type_scope.len();
        for g in generics {
            self.type_scope.push((g.name.clone(), g.ty()));
        }
        mark
    }

    pub fn leave_generics(&mut self, mark: usize) {
        self.type_scope.truncate(mark);
    }

    /// The type parameter called `name` that is in scope.
    pub fn scoped_param(&self, name: &str) -> Option<Ty> {
        self.type_scope
            .iter()
            .rev()
            .find(|(n, _)| n == name)
            .map(|(_, t)| t.clone())
    }

    /// Declares the parameters of a declaration: ids first, then (with all of them in scope,
    /// so bounds may mention each other) their bounds. The parameters stay in scope; the caller
    /// leaves them with the mark returned alongside.
    pub fn declare_generics(
        &mut self,
        decls: &[ast::GenericParam],
    ) -> (Vec<GenericParamInfo>, usize) {
        let mut infos: Vec<GenericParamInfo> = Vec::new();
        let mark = self.type_scope.len();
        for d in decls {
            self.refuse_default(d);
            if infos.iter().any(|g| g.name == d.name.name) {
                self.report(Diagnostic::error(
                    format!(
                        "the type parameter `{}` is declared more than once",
                        d.name.name
                    ),
                    d.name.span,
                ));
                continue;
            }
            let info = GenericParamInfo {
                id: self.new_param(),
                name: d.name.name.clone(),
                span: d.name.span,
                bounds: Vec::new(),
                default: None,
            };
            self.type_scope.push((info.name.clone(), info.ty()));
            infos.push(info);
        }
        for d in decls {
            let bounded = infos
                .iter()
                .find(|g| g.name == d.name.name && g.span == d.name.span)
                .map(GenericParamInfo::ty);
            let bounds: Vec<BoundRef> = match bounded {
                Some(bounded) => d
                    .bounds
                    .iter()
                    .filter_map(|b| self.resolve_bound(b, bounded.clone()))
                    .collect(),
                None => Vec::new(),
            };
            if let Some(info) = infos
                .iter_mut()
                .find(|g| g.name == d.name.name && g.span == d.name.span)
            {
                self.param_bounds.insert(info.id, bounds.clone());
                info.bounds = bounds;
            }
        }
        (infos, mark)
    }

    /// One bound of the type parameter `bounded`: an interface with its type arguments. A
    /// type argument left out takes the interface's default, with `Self` standing for `bounded`.
    pub fn resolve_bound(&mut self, t: &ast::TypeExpr, bounded: Ty) -> Option<BoundRef> {
        self.default_self = Some(bounded);
        let resolved = self.resolve_type(t);
        self.default_self = None;
        match resolved {
            Ty::Dyn(iface, args) => Some(BoundRef {
                iface,
                args,
                span: t.span,
                implied: false,
            }),
            Ty::Error => None,
            other => {
                let shown = self.render(&other);
                self.report(
                    Diagnostic::error(format!("`{shown}` is not an interface"), t.span)
                        .with_hint("a bound names an interface: `T: Ord`, `T: Hash + Eq`"),
                );
                None
            }
        }
    }

    /// Resolves the signature of a function or method. The enclosing declaration's type
    /// parameters (`ctx.outer`) must be in scope already; the function's own are added here.
    pub fn resolve_sig<'a>(&mut self, decl: &'a ast::FnDecl, ctx: &SigCtx) -> Sig<'a> {
        let mut poisoned = false;
        let (own, mark) = self.declare_generics(&decl.generics);
        let mut params: Vec<ParamInfo<'a>> = Vec::new();
        let mut receiver = None;
        if let Some(r) = &decl.receiver {
            match &ctx.self_ty {
                Some(self_ty) => {
                    let mut mutable = r.mutable;
                    if mutable && ctx.class {
                        self.report(
                            Diagnostic::error("a class method cannot take `mut self`", r.span)
                                .with_hint("`self` can always change the fields of a class, because the object is shared; write `self`"),
                        );
                        mutable = false;
                    }
                    receiver = Some(mutable);
                    params.push(ParamInfo {
                        name: "self".to_string(),
                        ty: self_ty.clone(),
                        span: r.span,
                        default: None,
                        mutable,
                        module: self.cur_mod,
                    });
                }
                None => {
                    self.report(
                        Diagnostic::error("only a method can take `self`", r.span)
                            .with_hint("declare the function inside an `impl` block"),
                    );
                    poisoned = true;
                }
            }
        }
        for p in &decl.params {
            if params.iter().any(|q| q.name == p.name.name) {
                self.report(Diagnostic::error(
                    format!("the parameter `{}` is declared more than once", p.name.name),
                    p.name.span,
                ));
            }
            let ty = match &p.ty {
                Some(t) => self.resolve_type(t),
                None => {
                    self.report(
                        Diagnostic::error(
                            format!("the parameter `{}` needs a type", p.name.name),
                            p.name.span,
                        )
                        .with_hint(format!("write `{}: int`, for example", p.name.name)),
                    );
                    Ty::Error
                }
            };
            if p.mutable {
                self.unsupported("`mut` parameter", p.span);
            }
            params.push(ParamInfo {
                name: p.name.name.clone(),
                ty,
                span: p.name.span,
                default: p.default.as_ref(),
                mutable: false,
                module: self.cur_mod,
            });
        }
        let ret = match &decl.ret {
            Some(t) => self.resolve_ret_type(t),
            None => Ty::Unit,
        };
        let ret_span = decl.ret.as_ref().map(|t| t.span);
        self.leave_generics(mark);
        let mut generics = ctx.outer.clone();
        generics.extend(own);
        Sig {
            generics,
            params,
            receiver,
            ret,
            ret_span,
            poisoned,
        }
    }

    /// The instantiation of the type parameters of `def` with `args`.
    pub fn param_map_of(def: &crate::hir::TypeDef, args: &[Ty]) -> Vec<(ParamId, Ty)> {
        def.params
            .iter()
            .copied()
            .zip(args.iter().cloned())
            .collect()
    }

    /// The instantiation of `generics` with the types in `args`, for [`Ty::subst`].
    pub fn param_map(generics: &[GenericParamInfo], args: &[Ty]) -> Vec<(ParamId, Ty)> {
        generics
            .iter()
            .map(|g| g.id)
            .zip(args.iter().cloned())
            .collect()
    }
}
