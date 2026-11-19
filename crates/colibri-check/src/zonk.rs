//! The last step of checking a body: replace inference variables by their final types, give
//! untyped literals and operators their concrete form, and report what could not be inferred.

use colibri_syntax::{Diagnostic, Span};

use crate::hir::{
    Anchor, Arg, BinOp, Block, Expr, ExprKind, Lit, LocalInfo, Num, Operand, Place, Stmt, StmtKind,
    UnOp,
};
use crate::infer::InferCtx;
use crate::types::{IntTy, Ty};

pub(crate) fn zonk_body(
    infer: &mut InferCtx,
    block: &mut Block,
    locals: &mut [LocalInfo],
    diags: &mut Vec<Diagnostic>,
) {
    let mut z = Zonker { infer, diags };
    for local in locals {
        z.ty(&mut local.ty, local.span);
    }
    z.block(block);
}

struct Zonker<'a> {
    infer: &'a mut InferCtx,
    diags: &'a mut Vec<Diagnostic>,
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
    }
}

impl Zonker<'_> {
    fn ty(&mut self, ty: &mut Ty, span: Span) {
        let (resolved, unresolved) = self.infer.finalize(ty);
        if unresolved {
            self.diags.push(
                Diagnostic::error("cannot infer the type of this expression", span)
                    .with_hint("add a type annotation"),
            );
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
        }
    }

    fn place(&mut self, p: &mut Place) {
        // A place's type is that of a resolved local or global; nothing to report here.
        p.ty = self.infer.finalize(&p.ty).0;
        match &mut p.anchor {
            Anchor::Local(_) | Anchor::Global(_) => {}
            Anchor::ClassField(e, _) => self.expr(e),
            Anchor::ListElem(a, b) | Anchor::MapEntry(a, b) => {
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
