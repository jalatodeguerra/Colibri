//! The last step of checking a body: replace inference variables by their final types, give
//! untyped literals and operators their concrete form, and report what could not be inferred.

use colibri_syntax::{Diagnostic, Span};

use crate::hir::{
    Anchor, Arg, BinOp, Block, Builtin, Callee, CaptureMode, Expr, ExprKind, ForBinding, ForSource,
    Intrinsic, Lit, LocalInfo, Num, Operand, Pat, PatKind, Place, Stmt, StmtKind, Storage, UnOp,
};
use crate::infer::InferCtx;
use crate::types::{IntTy, Ty};

pub(crate) fn zonk_body(
    infer: &mut InferCtx,
    block: &mut Block,
    locals: &mut [LocalInfo],
    diags: &mut Vec<Diagnostic>,
    had_errors: bool,
) {
    let mut z = Zonker {
        infer,
        diags,
        reported_unresolved: false,
        had_errors,
        storage: Vec::new(),
    };
    for local in locals.iter_mut() {
        z.ty(&mut local.ty, local.span);
    }
    z.storage = locals.iter().map(|l| l.storage).collect();
    z.block(block);
}

struct Zonker<'a> {
    infer: &'a mut InferCtx,
    diags: &'a mut Vec<Diagnostic>,
    /// Whether an unresolved type was already reported in this body: the others are the same
    /// mistake seen again (the binding, the literal, each use).
    reported_unresolved: bool,
    /// The body already has an error, which probably explains any open type or bad key.
    had_errors: bool,
    /// How each local of the body is stored, to give captures of cell variables their mode.
    storage: Vec<Storage>,
}

/// The arithmetic kind of a resolved operand type.
fn num_of(ty: &Ty) -> Num {
    match ty {
        Ty::Float => Num::Float,
        Ty::Int(i) => Num::Int(*i),
        _ => Num::Int(IntTy::I64),
    }
}

/// The comparison kind of a resolved operand type.
fn operand_of(ty: &Ty) -> Operand {
    match ty {
        Ty::Float => Operand::Float,
        Ty::Int(i) => Operand::Int(*i),
        Ty::Bool => Operand::Bool,
        Ty::Char => Operand::Char,
        Ty::Str => Operand::Str,
        Ty::Unit => Operand::Unit,
        // A type parameter stands for any type, so it compares by what the values are.
        Ty::Tuple(_)
        | Ty::Adt(..)
        | Ty::List(_)
        | Ty::Map(..)
        | Ty::Set(_)
        | Ty::Option(_)
        | Ty::Range(_)
        | Ty::Param(..) => Operand::Structural,
        _ => Operand::Int(IntTy::I64),
    }
}

/// An operator re-derived from the resolved type of its operands, which the checker could
/// not know while a literal was still untyped.
fn resolve_binop(op: BinOp, operand: &Ty) -> BinOp {
    match op {
        BinOp::Arith(..) | BinOp::Concat if *operand == Ty::Str => BinOp::Concat,
        BinOp::Arith(a, _) => BinOp::Arith(a, num_of(operand)),
        BinOp::Concat => BinOp::Concat,
        BinOp::Cmp(c, _) => BinOp::Cmp(c, operand_of(operand)),
        BinOp::Is => BinOp::Is,
    }
}

impl Zonker<'_> {
    fn ty(&mut self, ty: &mut Ty, span: Span) {
        let (resolved, unresolved) = self.infer.finalize(ty);
        if unresolved && !self.reported_unresolved && !self.had_errors {
            self.reported_unresolved = true;
            let (message, hint) = match &resolved {
                Ty::List(_) => (
                    "cannot infer the element type of this list",
                    "add a type annotation, for example `let xs: List[int] = []`",
                ),
                Ty::Map(..) => (
                    "cannot infer the key and value types of this map",
                    "add a type annotation, for example `let m: Map[str, int] = {:}`",
                ),
                Ty::Set(_) => (
                    "cannot infer the element type of this set",
                    "add a type annotation, for example `let s: Set[int] = Set([])`",
                ),
                Ty::Option(_) => (
                    "cannot infer the type of this optional value",
                    "add a type annotation, for example `let x: int? = none`",
                ),
                _ => (
                    "cannot infer the type of this expression",
                    "add a type annotation",
                ),
            };
            self.diags
                .push(Diagnostic::error(message, span).with_hint(hint));
        }
        *ty = resolved;
    }

    fn block(&mut self, b: &mut Block) {
        self.ty(&mut b.ty, b.span);
        for s in &mut b.stmts {
            self.stmt(s);
        }
        if let Some(t) = &mut b.tail {
            self.expr(t);
        }
    }

    fn stmt(&mut self, s: &mut Stmt) {
        match &mut s.kind {
            StmtKind::Let { init, .. } => self.expr(init),
            StmtKind::Expr(e) => self.expr(e),
            StmtKind::LetPat { pat, init } => {
                self.pat(pat);
                self.expr(init);
            }
            StmtKind::Assign { place, op, rhs } => {
                self.place(place);
                self.expr(rhs);
                if let Some(op) = op {
                    *op = resolve_binop(*op, &place.ty);
                }
            }
            StmtKind::Return(v) => {
                if let Some(v) = v {
                    self.expr(v);
                }
            }
            StmtKind::Break(_) | StmtKind::Continue(_) => {}
            StmtKind::Defer(inner) => {
                // The deferred statement is shared by the cleanup list at runtime, but is
                // still uniquely owned here.
                if let Some(inner) = std::rc::Rc::get_mut(inner) {
                    self.stmt(inner);
                }
            }
        }
    }

    fn pat(&mut self, p: &mut Pat) {
        self.ty(&mut p.ty, p.span);
        match &mut p.kind {
            PatKind::Bind(_, Some(inner)) => self.pat(inner),
            PatKind::Variant { args: items, .. }
            | PatKind::Struct { fields: items, .. }
            | PatKind::Tuple(items)
            | PatKind::Or(items) => items.iter_mut().for_each(|item| self.pat(item)),
            PatKind::Bind(_, None)
            | PatKind::Wild
            | PatKind::Lit(_)
            | PatKind::Range { .. }
            | PatKind::Const(_) => {}
        }
    }

    fn place(&mut self, p: &mut Place) {
        // A place's type is that of a resolved local or global; nothing to report here.
        p.ty = self.infer.finalize(&p.ty).0;
        match &mut p.anchor {
            Anchor::Local(_) | Anchor::Global(_) => {}
            Anchor::ClassField(e, _) => self.expr(e),
            Anchor::ListElem(a, b, _) | Anchor::MapEntry(a, b, _) => {
                self.expr(a);
                self.expr(b);
            }
        }
    }

    fn expr(&mut self, e: &mut Expr) {
        self.ty(&mut e.ty, e.span);
        match &mut e.kind {
            ExprKind::Lit(lit) => {
                // An untyped integer literal used as a float.
                if let Lit::Int(i) = lit
                    && e.ty == Ty::Float
                {
                    *lit = Lit::Float(*i as f64);
                }
            }
            ExprKind::Read(p) => self.place(p),
            ExprKind::Unary(op, x) => {
                self.expr(x);
                if let UnOp::Neg(_) = op {
                    *op = UnOp::Neg(num_of(&x.ty));
                }
            }
            ExprKind::Binary(op, l, r) => {
                self.expr(l);
                self.expr(r);
                *op = resolve_binop(*op, &l.ty);
            }
            ExprKind::And(l, r) | ExprKind::Or(l, r) => {
                self.expr(l);
                self.expr(r);
            }
            ExprKind::Call(call) => {
                for t in &mut call.type_args {
                    self.ty(t, e.span);
                }
                for a in &mut call.args {
                    match a {
                        Arg::Value(v) => self.expr(v),
                        Arg::Place(p) => self.place(p),
                    }
                }
                // The sum of an empty list of floats is `0.0`: the engine cannot tell from
                // the (empty) list, so the checker hands it the zero.
                if e.ty == Ty::Float
                    && call.callee == Callee::Intrinsic(Intrinsic::Builtin(Builtin::ListSum))
                {
                    let zero = Expr {
                        kind: ExprKind::Lit(Lit::Float(0.0)),
                        ty: Ty::Float,
                        span: e.span,
                    };
                    call.args.push(Arg::Value(zero));
                }
            }
            ExprKind::StrConcat(parts) => parts.iter_mut().for_each(|p| self.expr(p)),
            ExprKind::If { cond, then, else_ } => {
                self.expr(cond);
                self.block(then);
                if let Some(b) = else_ {
                    self.block(b);
                }
            }
            ExprKind::Block(b) => self.block(b),
            ExprKind::Loop { body, .. } => self.block(body),
            ExprKind::For(f) => {
                match &mut f.source {
                    ForSource::Range { start, end, .. } => {
                        self.expr(start);
                        self.expr(end);
                    }
                    ForSource::List { list: e, .. }
                    | ForSource::Map(e)
                    | ForSource::Set(e)
                    | ForSource::Iter(e) => self.expr(e),
                }
                if let ForBinding::Pat(p) = &mut f.binding {
                    self.pat(p);
                }
                self.block(&mut f.body);
            }
            ExprKind::Struct { fields: items, .. }
            | ExprKind::Object { fields: items, .. }
            | ExprKind::Variant { args: items, .. }
            | ExprKind::ListLit(items)
            | ExprKind::Tuple(items) => items.iter_mut().for_each(|item| self.expr(item)),
            ExprKind::MapLit(entries) => {
                for (k, v) in entries {
                    self.expr(k);
                    self.expr(v);
                }
            }
            ExprKind::ToWeak(inner) | ExprKind::FromWeak(inner) | ExprKind::Coerce(inner, _) => {
                self.expr(inner)
            }
            ExprKind::Project(base, _) => self.expr(base),
            ExprKind::Match { scrutinee, arms } => {
                self.expr(scrutinee);
                for arm in arms {
                    self.pat(&mut arm.pat);
                    if let Some(g) = &mut arm.guard {
                        self.expr(g);
                    }
                    self.block(&mut arm.body);
                }
            }
            ExprKind::FnRef { type_args, .. } => {
                for t in type_args {
                    self.ty(t, e.span);
                }
            }
            ExprKind::Range { start, end, .. } => {
                self.expr(start);
                self.expr(end);
            }
            ExprKind::Closure(c) => {
                // A captured variable that lives in a cell is shared, not copied.
                for cap in &mut c.captures {
                    let cell = self.storage.get(cap.outer.0 as usize) == Some(&Storage::Cell);
                    if cap.mode == CaptureMode::ByValue && cell {
                        cap.mode = CaptureMode::Cell;
                    }
                }
            }
            ExprKind::Error => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hir::{ArithOp, CmpOp};

    #[test]
    fn operators_follow_operand_types() {
        let add = BinOp::Arith(ArithOp::Add, Num::Int(IntTy::I64));
        assert_eq!(
            resolve_binop(add, &Ty::Float),
            BinOp::Arith(ArithOp::Add, Num::Float)
        );
        assert_eq!(resolve_binop(add, &Ty::Str), BinOp::Concat);
        let lt = BinOp::Cmp(CmpOp::Lt, Operand::Int(IntTy::I64));
        assert_eq!(
            resolve_binop(lt, &Ty::Str),
            BinOp::Cmp(CmpOp::Lt, Operand::Str)
        );
    }
}
