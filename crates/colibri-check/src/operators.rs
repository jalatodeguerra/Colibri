//! Operators on user types (DESIGN §9.2): `a + b`, `a - b`, `a * b`, `a / b` and `-a` run the
//! `Add`, `Sub`, `Mul`, `Div` and `Neg` implementation of the operand, and `p op= b` is
//! `p = p op b` with the place evaluated once.
//!
//! Numbers keep the path they always had (`check_arith`, `unify_operands`): this module runs
//! only when the left operand is a user type, or when it is a number and the right operand is a
//! user type. The impl is chosen here, at check time, from the heads of the two operand types, and
//! the operator becomes a direct call of its method (`UserOp::Fn`).

use colibri_syntax::ast::{self, BinOp as AstBin};
use colibri_syntax::{Diagnostic, Span};

use crate::body::FnCtx;
use crate::checker::Dep;
use crate::deprecated::DeprKey;
use crate::expr::op_symbol;
use crate::hir::{self, ArithOp, BinOp, Expr, FnId, ImplId, TypeKey, UnOp, UserOp};
use crate::infer::VarKind;
use crate::traits::key_of;
use crate::types::{ADD, DIV, FIRST_USER_TYPE, IntTy, MUL, NEG, SUB, Ty, TypeId};

/// The operator interface of an arithmetic operator; `%` has none.
pub(crate) fn operator_iface(arith: ArithOp) -> Option<TypeId> {
    match arith {
        ArithOp::Add => Some(ADD),
        ArithOp::Sub => Some(SUB),
        ArithOp::Mul => Some(MUL),
        ArithOp::Div => Some(DIV),
        ArithOp::Rem => None,
    }
}

/// The primitive left operands an operator impl can be written for.
const SCALAR_KEYS: [TypeKey; 10] = [
    TypeKey::Int(IntTy::I64),
    TypeKey::Int(IntTy::I8),
    TypeKey::Int(IntTy::I16),
    TypeKey::Int(IntTy::I32),
    TypeKey::Int(IntTy::U8),
    TypeKey::Int(IntTy::U16),
    TypeKey::Int(IntTy::U32),
    TypeKey::Int(IntTy::U64),
    TypeKey::Float,
    TypeKey::F32,
];

/// A candidate impl of one operator for one left-hand type: the head of its right-hand type
/// (`None` for a type parameter, which stands for any) and the impl.
type Candidate = (Option<TypeKey>, ImplId);

/// How an impl was picked for a right-hand operand.
enum Pick {
    /// This impl; the flag says the right operand is an `f32` that widens to the impl's `float`.
    One(ImplId, bool),
    /// No impl fits.
    None,
    /// An untyped literal fits more than one impl.
    Ambiguous,
    /// The operand has no type yet and several impls could take it.
    Unknown,
}

/// The type an operator impl takes and gives, instantiated for one use.
struct OpInstance {
    f: FnId,
    /// The impl's `Self`, and its right-hand type for a binary operator.
    params: Vec<Ty>,
    ret: Ty,
}

/// Whether an untyped literal of `kind` can be a `ty` (as `InferCtx::unify` decides it).
fn literal_accepts(kind: VarKind, ty: &Ty) -> bool {
    match ty {
        Ty::Param(..) => true,
        Ty::Float | Ty::F32 => true,
        Ty::Int(_) => kind == VarKind::IntLit,
        _ => false,
    }
}

impl FnCtx<'_, '_> {
    /// Whether `l op r` with this left operand goes through an operator interface: a type
    /// declared by the program or a host module, a type parameter, or another built-in type that
    /// a program gave an impl of this operator. Numbers, strings and byte strings never do.
    pub(crate) fn left_uses_operator(&self, left: &Ty, iface: TypeId) -> bool {
        match left {
            Ty::Adt(id, _) => id.0 >= FIRST_USER_TYPE,
            Ty::Param(..) => true,
            Ty::Int(_) | Ty::Float | Ty::F32 | Ty::Str | Ty::Bytes => false,
            other => key_of(other).is_some_and(|k| self.ck.op_impls.contains_key(&(k, iface))),
        }
    }

    /// The impls of `iface` written for the head of `left`.
    fn operator_candidates(&self, left: &Ty, iface: TypeId) -> Vec<Candidate> {
        key_of(left)
            .and_then(|k| self.ck.op_impls.get(&(k, iface)))
            .cloned()
            .unwrap_or_default()
    }

    /// The right-hand type of an impl, as written in its header.
    fn impl_rhs(&self, imp: ImplId) -> Ty {
        self.ck.impls[imp.0 as usize]
            .iface_args
            .first()
            .cloned()
            .unwrap_or(Ty::Error)
    }

    /// What a right-hand operand can be told about its type before it is checked: the right-hand
    /// type of the only impl, when it names no type parameter. With several impls the operand
    /// is checked on its own, so a hint cannot bind it to the wrong one.
    fn rhs_hint(&self, cands: &[Candidate]) -> Option<Ty> {
        match cands {
            [(_, imp)] => Some(self.impl_rhs(*imp)).filter(|t| !t.has_param() && !t.is_error()),
            _ => None,
        }
    }

    /// Picks the impl for a right-hand operand of this type (`rhs`, as its head tells).
    fn pick_by_rhs(&mut self, cands: &[Candidate], rhs: &Expr) -> Pick {
        let rt = self.infer.shallow(&rhs.ty);
        if let Ty::Infer(_) = rt {
            let kind = self.infer.unbound_kind(&rt).unwrap_or(VarKind::General);
            let fits: Vec<ImplId> = cands
                .iter()
                .filter(|(_, imp)| {
                    kind == VarKind::General || literal_accepts(kind, &self.impl_rhs(*imp))
                })
                .map(|(_, imp)| *imp)
                .collect();
            return match fits.as_slice() {
                [] => Pick::None,
                [one] => Pick::One(*one, false),
                _ if kind == VarKind::General => Pick::Unknown,
                _ => Pick::Ambiguous,
            };
        }
        let rk = key_of(&rt);
        if let Some(rk) = rk
            && let Some((_, imp)) = cands.iter().find(|(k, _)| *k == Some(rk))
        {
            return Pick::One(*imp, false);
        }
        // A right-hand type that is a type parameter of the impl takes anything.
        if let Some((_, imp)) = cands.iter().find(|(k, _)| k.is_none()) {
            return Pick::One(*imp, false);
        }
        // The one implicit conversion of numbers: an `f32` widens to `float` (DESIGN §5.1).
        if rt == Ty::F32
            && let Some((_, imp)) = cands.iter().find(|(k, _)| *k == Some(TypeKey::Float))
        {
            return Pick::One(*imp, true);
        }
        Pick::None
    }

    /// Instantiates the method of `imp`: its type parameters become fresh variables (and the
    /// bounds of the `impl` become obligations), so the types it takes and gives are the ones
    /// this use gets.
    fn instantiate_op(&mut self, imp: ImplId, span: Span) -> Option<OpInstance> {
        let f = self.ck.impls[imp.0 as usize].methods.first().copied()??;
        let info = &self.ck.fns[f.0 as usize];
        if info.is_poisoned() {
            return None;
        }
        let (name, params, ret, generics) = (
            info.name.clone(),
            info.params.clone(),
            info.ret.clone(),
            info.generics.clone(),
        );
        let outer = generics.len() - self.own_generic_count(f);
        let (_, map) = self.instantiate(&generics, outer, None, &name, span)?;
        Some(OpInstance {
            f,
            params: params.iter().map(|p| p.ty.subst(&map)).collect(),
            ret: ret.subst(&map),
        })
    }

    /// Notes that the impl's method is used, for initialisation order and deprecation.
    fn use_operator_fn(&mut self, f: FnId, span: Span) {
        self.add_dep(Dep::Fn(f), span);
        self.ck.note_deprecated_use(DeprKey::Fn(f), span);
    }

    /// `l op rhs` where `l` is a user type: the impl is chosen by the right operand, which is
    /// checked here. `arith` has an interface (the caller sends `%` elsewhere).
    pub(crate) fn check_user_arith(
        &mut self,
        op: AstBin,
        iface: TypeId,
        mut l: Expr,
        rhs: &ast::Expr,
        span: Span,
    ) -> Expr {
        let lt = self.infer.shallow(&l.ty);
        if matches!(lt, Ty::Param(..)) {
            return self.check_bound_arith(op, iface, l, rhs, span);
        }
        let cands = self.operator_candidates(&lt, iface);
        let hint = self.rhs_hint(&cands);
        let mut r = self.check_expr(rhs, hint.as_ref());
        if lt.is_error() || self.infer.shallow(&r.ty).is_error() {
            return self.error_expr(span);
        }
        let Some((imp, widen)) = self.choose(op, iface, &cands, &mut l, &mut r, span) else {
            return self.error_expr(span);
        };
        if widen {
            r = self.widen_f32(r);
        }
        self.build_binary(imp, l, r, op, span)
    }

    /// The bounds of the type parameter `ty` that are `iface`, as (right-hand head, right-hand
    /// type, result type).
    fn operator_bounds(&self, ty: &Ty, iface: TypeId) -> Vec<(Option<TypeKey>, Ty, Ty)> {
        let Ty::Param(p, _) = ty else {
            return Vec::new();
        };
        self.ck
            .param_bounds
            .get(p)
            .into_iter()
            .flatten()
            .filter(|b| b.iface == iface)
            .map(|b| {
                let r = b.args.first().cloned().unwrap_or_else(|| ty.clone());
                let o = b.args.get(1).cloned().unwrap_or_else(|| ty.clone());
                (key_of(&r), r, o)
            })
            .collect()
    }

    /// Picks among the bounds of a type parameter by the right operand, like [`Self::pick_by_rhs`]
    /// does among impls. The flag says an `f32` operand widens to the bound's `float`.
    fn pick_bound(
        &mut self,
        bounds: &[(Option<TypeKey>, Ty, Ty)],
        rhs: &Expr,
    ) -> Result<(usize, bool), bool> {
        let rt = self.infer.shallow(&rhs.ty);
        if let Ty::Infer(_) = rt {
            let kind = self.infer.unbound_kind(&rt).unwrap_or(VarKind::General);
            let fits: Vec<usize> = bounds
                .iter()
                .enumerate()
                .filter(|(_, (_, r, _))| match kind {
                    VarKind::General => true,
                    k => !matches!(r, Ty::Param(..)) && literal_accepts(k, r),
                })
                .map(|(i, _)| i)
                .collect();
            return match fits.as_slice() {
                [one] => Ok((*one, false)),
                [] => Err(false),
                _ => Err(true),
            };
        }
        let rk = key_of(&rt);
        if let Some(i) = bounds
            .iter()
            .position(|(k, r, _)| k.is_some() && *k == rk || *r == rt)
        {
            return Ok((i, false));
        }
        if rt == Ty::F32
            && let Some(i) = bounds.iter().position(|(k, ..)| *k == Some(TypeKey::Float))
        {
            return Ok((i, true));
        }
        if let [_] = bounds {
            return Ok((0, false));
        }
        Err(false)
    }

    /// Resolves `l op r` where `l` has the type parameter `lt`: through its bound. Returns the
    /// operator, the type it gives, and whether the right operand must widen from `f32`.
    fn resolve_bound_op(
        &mut self,
        op: AstBin,
        iface: TypeId,
        lt: &Ty,
        l_span: Span,
        r: &Expr,
        span: Span,
    ) -> Option<(UserOp, Ty, bool)> {
        let bounds = self.operator_bounds(lt, iface);
        let shown = self.show(lt);
        let iface_name = self.ck.types[iface.0 as usize].name.clone();
        if bounds.is_empty() {
            self.ck.report(
                Diagnostic::error(
                    format!("`{}` cannot be applied to `{shown}`", op_symbol(op)),
                    span,
                )
                .with_label(l_span, format!("this is `{shown}`"))
                .with_hint(format!("add the bound `{shown}: {iface_name}`")),
            );
            return None;
        }
        let (i, widen) = match self.pick_bound(&bounds, r) {
            Ok(p) => p,
            Err(ambiguous) => {
                let rt = self.show(&r.ty);
                let message = if ambiguous {
                    format!(
                        "cannot tell which `{}` this means: `{shown}` has several `{iface_name}` bounds",
                        op_symbol(op)
                    )
                } else {
                    format!("cannot apply `{}` to `{shown}` and `{rt}`", op_symbol(op))
                };
                self.ck.report(
                    Diagnostic::error(message, span)
                        .with_label(l_span, format!("this is `{shown}`"))
                        .with_label(r.span, format!("this is `{rt}`")),
                );
                return None;
            }
        };
        let (rk, rhs_ty, out) = bounds[i].clone();
        let right = if widen { Ty::Float } else { r.ty.clone() };
        if self.infer.unify(&right, &rhs_ty).is_err() {
            let rt = self.show(&r.ty);
            let want = self.show(&rhs_ty);
            self.ck.report(
                Diagnostic::error(
                    format!("cannot apply `{}` to `{shown}` and `{rt}`", op_symbol(op)),
                    span,
                )
                .with_label(l_span, format!("this is `{shown}`"))
                .with_label(r.span, format!("this is `{rt}`"))
                .with_hint(format!(
                    "the bound `{shown}: {iface_name}` takes `{want}` on the right"
                )),
            );
            return None;
        }
        Some((UserOp::Bound { iface, rhs: rk }, out, widen))
    }

    /// `l op rhs` where `l` is a type parameter.
    fn check_bound_arith(
        &mut self,
        op: AstBin,
        iface: TypeId,
        l: Expr,
        rhs: &ast::Expr,
        span: Span,
    ) -> Expr {
        let lt = self.infer.shallow(&l.ty);
        let bounds = self.operator_bounds(&lt, iface);
        let hint = match bounds.as_slice() {
            [(_, r, _)] => Some(r.clone()),
            _ => None,
        };
        let mut r = self.check_expr(rhs, hint.as_ref());
        if self.infer.shallow(&r.ty).is_error() {
            return self.error_expr(span);
        }
        let Some((user, out, widen)) = self.resolve_bound_op(op, iface, &lt, l.span, &r, span)
        else {
            return self.error_expr(span);
        };
        if widen {
            r = self.widen_f32(r);
        }
        self.mk(
            hir::ExprKind::Binary(BinOp::User(user), Box::new(l), Box::new(r)),
            out,
            span,
        )
    }

    /// `l op r` where `l` is a number (or an untyped literal) and `r` a user type, if some impl
    /// is written for the number: `2 * v`, `dt * v`. `None` when none is, and then nothing was
    /// changed and the ordinary rules decide (and report).
    pub(crate) fn check_scalar_left(
        &mut self,
        op: AstBin,
        iface: TypeId,
        l: &mut Expr,
        r: &mut Expr,
        span: Span,
    ) -> Option<Expr> {
        let rt = self.infer.shallow(&r.ty);
        let rk = match &rt {
            Ty::Adt(id, _) if id.0 >= FIRST_USER_TYPE => key_of(&rt)?,
            _ => return None,
        };
        let lt = self.infer.shallow(&l.ty);
        // The left operands that have an impl taking this right operand.
        let mut lefts: Vec<(Ty, ImplId, bool)> = Vec::new();
        let kind = self.infer.unbound_kind(&lt);
        let concrete = if kind.is_none() { key_of(&lt) } else { None };
        for key in SCALAR_KEYS {
            let Some(list) = self.ck.op_impls.get(&(key, iface)) else {
                continue;
            };
            let Some(&(_, imp)) = list.iter().find(|(k, _)| *k == Some(rk) || k.is_none()) else {
                continue;
            };
            let scalar = scalar_ty(key);
            let widen = match (&concrete, key) {
                (Some(c), k) if *c == k => false,
                (Some(TypeKey::F32), TypeKey::Float) => true,
                (Some(_), _) => continue,
                (None, _) => match kind {
                    Some(k) if literal_accepts(k, &scalar) => false,
                    _ => continue,
                },
            };
            lefts.push((scalar, imp, widen));
        }
        // A concrete `f32` prefers its own impl over the widened one.
        if lefts.len() > 1 && concrete == Some(TypeKey::F32) {
            lefts.retain(|(_, _, widen)| !widen);
        }
        match lefts.as_slice() {
            [] => None,
            [(scalar, imp, widen)] => {
                let (scalar, imp, widen) = (scalar.clone(), *imp, *widen);
                let mut left = std::mem::replace(l, self.error_expr(span));
                let right = std::mem::replace(r, self.error_expr(span));
                if widen {
                    left = self.widen_f32(left);
                } else if self.infer.unify(&left.ty, &scalar).is_err() {
                    return Some(self.error_expr(span));
                }
                Some(self.build_binary(imp, left, right, op, span))
            }
            many => {
                let sym = op_symbol(op);
                let shown = self.show(&r.ty);
                let names: Vec<String> = many
                    .iter()
                    .map(|(t, _, _)| format!("`{}`", self.show(t)))
                    .collect();
                self.ck.report(
                    Diagnostic::error(
                        format!(
                            "cannot tell which `{sym}` this means: `{shown}` implements it with {} on the left",
                            names.join(" and ")
                        ),
                        span,
                    )
                    .with_label(l.span, "this literal fits more than one of them")
                    .with_hint("give the literal a type: write `2.0` for a `float`, or `int(2)` for an `int`"),
                );
                Some(self.error_expr(span))
            }
        }
    }

    /// Chooses the impl for `l op r` among `cands` (the impls of the left type), reporting
    /// when there is none or when an untyped literal fits several.
    fn choose(
        &mut self,
        op: AstBin,
        iface: TypeId,
        cands: &[Candidate],
        l: &mut Expr,
        r: &mut Expr,
        span: Span,
    ) -> Option<(ImplId, bool)> {
        match self.pick_by_rhs(cands, r) {
            Pick::One(imp, widen) => Some((imp, widen)),
            Pick::Ambiguous => {
                let sym = op_symbol(op);
                let lt = self.show(&l.ty);
                let rights = self.rhs_names(cands);
                self.ck.report(
                    Diagnostic::error(
                        format!(
                            "cannot tell which `{}` this means: `{lt}` implements it with {rights} on the right",
                            sym
                        ),
                        span,
                    )
                    .with_label(r.span, "this literal fits more than one of them")
                    .with_hint("give the literal a type: write `2.0` for a `float`, or `int(2)` for an `int`"),
                );
                None
            }
            Pick::Unknown => {
                let sym = op_symbol(op);
                let lt = self.show(&l.ty);
                let rights = self.rhs_names(cands);
                self.ck.report(
                    Diagnostic::error(
                        format!(
                            "cannot tell which `{sym}` this means: `{lt}` implements it with {rights} on the right, and the type of the right operand is not known yet"
                        ),
                        span,
                    )
                    .with_label(r.span, "its type is decided later in the function")
                    .with_hint("give it a type where it is declared (for example `var xs: List[float] = []`) or convert it, so that the impl can be chosen here"),
                );
                None
            }
            Pick::None => {
                self.report_no_operator(op, iface, cands, l, r, span);
                None
            }
        }
    }

    /// `` `a`, `b` and `c` `` for the right-hand types of some impls.
    fn rhs_names(&self, cands: &[Candidate]) -> String {
        let names: Vec<String> = cands
            .iter()
            .map(|(_, imp)| format!("`{}`", self.show(&self.impl_rhs(*imp))))
            .collect();
        match names.as_slice() {
            [] => String::new(),
            [one] => one.clone(),
            [init @ .., last] => format!("{} and {last}", init.join(", ")),
        }
    }

    /// When a number is on the left of a type of the program that implements the operator only
    /// with the number on the right, or for another number type on the left: what to write.
    pub(crate) fn hint_number_on_the_left(&self, op: AstBin, l: &Ty, r: &Ty) -> Option<String> {
        let iface = match op {
            AstBin::Add => ADD,
            AstBin::Sub => SUB,
            AstBin::Mul => MUL,
            AstBin::Div => DIV,
            _ => return None,
        };
        let (l, r) = (self.infer.shallow(l), self.infer.shallow(r));
        if !matches!(l, Ty::Int(_) | Ty::Float | Ty::F32 | Ty::Infer(_)) {
            return None;
        }
        if !matches!(&r, Ty::Adt(id, _) if id.0 >= FIRST_USER_TYPE) {
            return None;
        }
        let rt = self.show(&r);
        let rk = key_of(&r)?;
        // The number types that have an impl taking `r` on the right.
        let lefts: Vec<String> = SCALAR_KEYS
            .iter()
            .filter(|k| {
                self.ck
                    .op_impls
                    .get(&(**k, iface))
                    .is_some_and(|list| list.iter().any(|(rhs, _)| *rhs == Some(rk)))
            })
            .map(|k| self.show(&scalar_ty(*k)))
            .collect();
        let sym = op_symbol(op);
        let own: Vec<(Option<TypeKey>, ImplId)> = self
            .operator_candidates(&r, iface)
            .into_iter()
            .filter(|(k, _)| k.is_some_and(|k| SCALAR_KEYS.contains(&k)))
            .collect();
        if let [first, ..] = lefts.as_slice() {
            Some(format!(
                "a number on the left of `{sym}` must be of type `{first}` here: convert it, for example `{first}(n) {sym} v`, or write the `{rt}` first"
            ))
        } else if !own.is_empty() {
            Some(format!(
                "`{rt}` takes a number on the right of `{sym}` but nothing takes a number on the left: write the `{rt}` first (`v {sym} n`), or implement `impl {} [{rt}, {rt}] for <number>`",
                self.ck.types[iface.0 as usize].name
            ))
        } else {
            None
        }
    }

    /// "cannot apply `*` to `A` and `B`", with what the left type does implement.
    fn report_no_operator(
        &mut self,
        op: AstBin,
        iface: TypeId,
        cands: &[Candidate],
        l: &Expr,
        r: &Expr,
        span: Span,
    ) {
        let (lt, rt) = (self.show(&l.ty), self.show(&r.ty));
        let iface_name = self.ck.types[iface.0 as usize].name.clone();
        let method = iface_name.to_lowercase();
        let left_ours =
            matches!(self.infer.shallow(&l.ty), Ty::Adt(id, _) if id.0 >= FIRST_USER_TYPE);
        let hint = if cands.is_empty() && !left_ours {
            format!(
                "there is no `{iface_name}` for `{lt}` and `{rt}`: a built-in operator cannot be given to built-in types, wrap one in a type of your own"
            )
        } else if cands.is_empty() {
            let header = if lt == rt {
                format!("impl {iface_name} for {lt}")
            } else {
                format!("impl {iface_name}[{rt}] for {lt}")
            };
            format!(
                "`{lt}` has no `{iface_name}` implementation; write `{header} {{ fn {method}(self, other: {rt}) -> {lt} => .. }}`"
            )
        } else {
            let mut hint = format!(
                "`{lt}` implements `{iface_name}` for {}; the right side must be one of them",
                self.rhs_names(cands)
            );
            if matches!(self.infer.shallow(&r.ty), Ty::Int(_)) {
                hint.push_str(", convert it, for example `float(n)`");
            }
            hint
        };
        self.ck.report(
            Diagnostic::error(
                format!("cannot apply `{}` to `{lt}` and `{rt}`", op_symbol(op)),
                span,
            )
            .with_label(l.span, format!("this is `{lt}`"))
            .with_label(r.span, format!("this is `{rt}`"))
            .with_hint(hint),
        );
    }

    /// The call of the chosen impl's method on both operands.
    fn build_binary(&mut self, imp: ImplId, l: Expr, r: Expr, op: AstBin, span: Span) -> Expr {
        let Some(inst) = self.instantiate_op(imp, span) else {
            return self.error_expr(span);
        };
        self.use_operator_fn(inst.f, span);
        let (p0, p1) = (inst.params.first(), inst.params.get(1));
        let fits = p0.is_none_or(|p| self.infer.unify(p, &l.ty).is_ok())
            && p1.is_none_or(|p| self.infer.unify(p, &r.ty).is_ok());
        if !fits {
            let (lt, rt) = (self.show(&l.ty), self.show(&r.ty));
            self.ck.report(
                Diagnostic::error(
                    format!("cannot apply `{}` to `{lt}` and `{rt}`", op_symbol(op)),
                    span,
                )
                .with_label(l.span, format!("this is `{lt}`"))
                .with_label(r.span, format!("this is `{rt}`")),
            );
            return self.error_expr(span);
        }
        self.mk(
            hir::ExprKind::Binary(BinOp::User(UserOp::Fn(inst.f)), Box::new(l), Box::new(r)),
            inst.ret,
            span,
        )
    }

    /// `a.add(b)`, `a.mul(b)`, ... on a type with several impls of the operator: the same
    /// choice `a * b` makes, so a call and an operator never disagree.
    pub(crate) fn call_operator_method(
        &mut self,
        iface: TypeId,
        recv: Expr,
        name: &ast::Ident,
        args: &[ast::Arg],
        span: Span,
    ) -> Expr {
        if iface == NEG {
            if !args.is_empty() {
                self.check_args_loosely(args);
                self.ck.report(Diagnostic::error(
                    format!("`{}` takes no arguments, found {}", name.name, args.len()),
                    name.span,
                ));
                return self.error_expr(span);
            }
            return self.check_user_neg(recv, span);
        }
        let ast_op = match iface {
            ADD => AstBin::Add,
            SUB => AstBin::Sub,
            MUL => AstBin::Mul,
            _ => AstBin::Div,
        };
        let [arg] = args else {
            self.check_args_loosely(args);
            self.ck.report(
                Diagnostic::error(
                    format!("`{}` takes 1 argument, found {}", name.name, args.len()),
                    name.span,
                )
                .with_hint("the argument is the right-hand side of the operator"),
            );
            return self.error_expr(span);
        };
        if let Some(named) = &arg.name {
            self.check_args_loosely(args);
            self.ck.report(Diagnostic::error(
                format!("`{}` takes its argument by position", name.name),
                named.span,
            ));
            return self.error_expr(span);
        }
        self.check_user_arith(ast_op, iface, recv, &arg.value, span)
    }

    /// `-x` where `x` is a user type: its `Neg` impl.
    pub(crate) fn check_user_neg(&mut self, value: Expr, span: Span) -> Expr {
        let ty = self.infer.shallow(&value.ty);
        if let Ty::Param(p, _) = &ty {
            let bounded = self
                .ck
                .param_bounds
                .get(p)
                .is_some_and(|bs| bs.iter().any(|b| b.iface == NEG));
            if !bounded {
                let shown = self.show(&ty);
                self.ck.report(
                    Diagnostic::error(format!("cannot negate a value of type `{shown}`"), span)
                        .with_label(value.span, format!("this is `{shown}`"))
                        .with_hint(format!("add the bound `{shown}: Neg`")),
                );
                return self.error_expr(span);
            }
            let result = ty.clone();
            return self.mk(
                hir::ExprKind::Unary(
                    UnOp::User(UserOp::Bound {
                        iface: NEG,
                        rhs: None,
                    }),
                    Box::new(value),
                ),
                result,
                span,
            );
        }
        let imp = key_of(&ty).and_then(|k| self.ck.impl_by_key.get(&(k, NEG)).copied());
        let Some(imp) = imp else {
            let shown = self.show(&ty);
            self.ck.report(
                Diagnostic::error(format!("cannot negate a value of type `{shown}`"), span)
                    .with_label(value.span, format!("this is `{shown}`"))
                    .with_hint(format!(
                        "`{shown}` has no `Neg` implementation; write `impl Neg for {shown} {{ fn neg(self) -> {shown} => .. }}`"
                    )),
            );
            return self.error_expr(span);
        };
        let Some(inst) = self.instantiate_op(imp, span) else {
            return self.error_expr(span);
        };
        self.use_operator_fn(inst.f, span);
        if let Some(p) = inst.params.first()
            && self.infer.unify(p, &value.ty).is_err()
        {
            return self.error_expr(span);
        }
        self.mk(
            hir::ExprKind::Unary(UnOp::User(UserOp::Fn(inst.f)), Box::new(value)),
            inst.ret,
            span,
        )
    }

    /// `place op= value` where the place is a user type: `place = place op value`, and the
    /// operator must give the type of the place back.
    pub(crate) fn check_user_assign(
        &mut self,
        span: Span,
        op: ast::AssignOp,
        place: hir::Place,
        target_span: Span,
        iface: TypeId,
        value: &ast::Expr,
    ) -> Option<hir::Stmt> {
        let place_ty = place.ty.clone();
        let ast_op = match iface {
            ADD => AstBin::Add,
            SUB => AstBin::Sub,
            MUL => AstBin::Mul,
            _ => AstBin::Div,
        };
        let sym = match op {
            ast::AssignOp::Add => "+=",
            ast::AssignOp::Sub => "-=",
            ast::AssignOp::Mul => "*=",
            _ => "/=",
        };
        let lt = self.infer.shallow(&place_ty);
        if matches!(lt, Ty::Param(..)) {
            let bounds = self.operator_bounds(&lt, iface);
            let hint = match bounds.as_slice() {
                [(_, r, _)] => Some(r.clone()),
                _ => None,
            };
            let mut rhs = self.check_expr(value, hint.as_ref());
            if self.infer.shallow(&rhs.ty).is_error() {
                return None;
            }
            let (user, out, widen) =
                self.resolve_bound_op(ast_op, iface, &lt, target_span, &rhs, span)?;
            if widen {
                rhs = self.widen_f32(rhs);
            }
            if self.infer.unify(&out, &place_ty).is_err() {
                let (target, got) = (self.show(&place_ty), self.show(&out));
                self.ck.report(Diagnostic::error(
                    format!("`{sym}` needs the operator to give a `{target}`; it gives `{got}`"),
                    span,
                ));
                return None;
            }
            return Some(hir::Stmt {
                span,
                kind: hir::StmtKind::Assign {
                    place,
                    op: Some(BinOp::User(user)),
                    rhs,
                },
            });
        }
        let cands = self.operator_candidates(&lt, iface);
        let hint = self.rhs_hint(&cands);
        let mut rhs = self.check_expr(value, hint.as_ref());
        if self.infer.shallow(&rhs.ty).is_error() {
            return None;
        }
        // The place stands for its own value here: its span is the left operand's.
        let mut left = self.mk(hir::ExprKind::Error, place_ty.clone(), target_span);
        let (imp, widen) = self.choose(ast_op, iface, &cands, &mut left, &mut rhs, span)?;
        if widen {
            rhs = self.widen_f32(rhs);
        }
        let inst = self.instantiate_op(imp, span)?;
        self.use_operator_fn(inst.f, span);
        let fits = inst
            .params
            .first()
            .is_none_or(|p| self.infer.unify(p, &place_ty).is_ok())
            && inst
                .params
                .get(1)
                .is_none_or(|p| self.infer.unify(p, &rhs.ty).is_ok());
        if !fits {
            let (lt, rt) = (self.show(&place_ty), self.show(&rhs.ty));
            self.ck.report(
                Diagnostic::error(format!("cannot apply `{sym}` to `{lt}` and `{rt}`"), span)
                    .with_label(rhs.span, format!("this is `{rt}`")),
            );
            return None;
        }
        if self.infer.unify(&inst.ret, &place_ty).is_err() {
            let (target, got) = (self.show(&place_ty), self.show(&inst.ret));
            let right = self.show(&rhs.ty);
            self.ck.report(
                Diagnostic::error(
                    format!("`{sym}` needs `{target} {} {right}` to give a `{target}`; it gives `{got}`", &sym[..1]),
                    span,
                )
                .with_label(rhs.span, format!("this is `{right}`"))
                .with_hint(format!(
                    "write `x = x {} y` to store a `{got}` in a variable that holds one",
                    &sym[..1]
                )),
            );
            return None;
        }
        Some(hir::Stmt {
            span,
            kind: hir::StmtKind::Assign {
                place,
                op: Some(BinOp::User(UserOp::Fn(inst.f))),
                rhs,
            },
        })
    }
}

/// The type a primitive impl head stands for.
fn scalar_ty(key: TypeKey) -> Ty {
    match key {
        TypeKey::Int(t) => Ty::Int(t),
        TypeKey::F32 => Ty::F32,
        _ => Ty::Float,
    }
}
