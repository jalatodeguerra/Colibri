//! The numeric tower beyond operators (DESIGN §5.1): conversion calls (`u8(x)`, `float(x)`),
//! the constants `T.MAX` and `T.MIN`, and the methods of the number types.

use colibri_syntax::ast;
use colibri_syntax::{Diagnostic, Span};

use crate::body::FnCtx;
use crate::call::Receiver;
use crate::checker::ParamInfo;
use crate::hir::{self, Arg, Callee, Expr, Intrinsic, Lit, Num, NumMethod};
use crate::modules::BASE;
use crate::suggest;
use crate::types::{IntTy, Ty};

/// The methods of every integer type.
const INT_METHODS: &[&str] = &[
    "wrapping_add",
    "wrapping_sub",
    "wrapping_mul",
    "checked_add",
    "checked_sub",
    "checked_mul",
    "checked_div",
    "saturating_add",
    "saturating_sub",
    "saturating_mul",
    "abs",
    "min",
    "max",
    "pow",
];
/// The methods of `float` and `f32`.
const FLOAT_METHODS: &[&str] = &[
    "abs", "sqrt", "floor", "ceil", "round", "min", "max", "is_nan",
];

/// A method of a number type with its types filled in for one receiver.
struct NumSig {
    method: NumMethod,
    params: Vec<(&'static str, Ty)>,
    ret: Ty,
}

/// The method `name` of the number type `ty`, if it has one.
fn num_sig(ty: &Ty, name: &str) -> Option<NumSig> {
    use NumMethod as M;
    let same = |method: M| NumSig {
        method,
        params: vec![("other", ty.clone())],
        ret: ty.clone(),
    };
    let unary = |method: M| NumSig {
        method,
        params: Vec::new(),
        ret: ty.clone(),
    };
    match ty {
        Ty::Int(int) => Some(match name {
            "wrapping_add" => same(M::WrappingAdd),
            "wrapping_sub" => same(M::WrappingSub),
            "wrapping_mul" => same(M::WrappingMul),
            "saturating_add" => same(M::SaturatingAdd),
            "saturating_sub" => same(M::SaturatingSub),
            "saturating_mul" => same(M::SaturatingMul),
            "checked_add" | "checked_sub" | "checked_mul" | "checked_div" => {
                let method = match name {
                    "checked_add" => M::CheckedAdd,
                    "checked_sub" => M::CheckedSub,
                    "checked_mul" => M::CheckedMul,
                    _ => M::CheckedDiv,
                };
                NumSig {
                    method,
                    params: vec![("other", ty.clone())],
                    ret: Ty::Option(Box::new(ty.clone())),
                }
            }
            "abs" if int.is_signed() => unary(M::Abs),
            "min" => same(M::Min),
            "max" => same(M::Max),
            "pow" => NumSig {
                method: M::Pow,
                params: vec![("exp", Ty::INT)],
                ret: ty.clone(),
            },
            _ => return None,
        }),
        Ty::Float | Ty::F32 => Some(match name {
            "abs" => unary(M::Abs),
            "sqrt" => unary(M::Sqrt),
            "floor" => unary(M::Floor),
            "ceil" => unary(M::Ceil),
            "round" => unary(M::Round),
            "min" => same(M::Min),
            "max" => same(M::Max),
            "is_nan" => NumSig {
                method: M::IsNan,
                params: Vec::new(),
                ret: Ty::Bool,
            },
            _ => return None,
        }),
        _ => None,
    }
}

/// Whether the number type `ty` has a method called `name`.
pub(crate) fn num_method_exists(ty: &Ty, name: &str) -> bool {
    num_sig(ty, name).is_some()
}

impl FnCtx<'_, '_> {
    /// The number type that `name` stands for in an expression, unless a local, a function, a
    /// module value or a user type of that name is in scope.
    pub(crate) fn numeric_type_name(&self, name: &str) -> Option<Ty> {
        let ty = Ty::numeric_by_name(name)?;
        let shadowed = self.is_local(name) || self.ck.values.contains_key(name);
        (!shadowed).then_some(ty)
    }

    /// `u8(x)`, `int(x)`, `float(x)`, `f32(x)`: the conversion call that names its target
    /// (DESIGN §5.1). Integer to integer panics when the value does not fit, float to integer
    /// truncates toward zero and panics on `nan` or an out-of-range value, integer to float
    /// rounds to nearest.
    pub(crate) fn check_conversion(&mut self, target: Ty, args: &[ast::Arg], span: Span) -> Expr {
        let name = self.show(&target);
        let [arg] = args else {
            self.check_args_loosely(args);
            self.ck.report(
                Diagnostic::error(
                    format!(
                        "`{name}(..)` takes exactly one argument, found {}",
                        args.len()
                    ),
                    span,
                )
                .with_hint(format!("write `{name}(x)`")),
            );
            return self.error_expr(span);
        };
        if let Some(n) = &arg.name {
            self.check_expr(&arg.value, None);
            self.ck.report(Diagnostic::error(
                format!("`{name}(..)` does not take a named argument"),
                n.span,
            ));
            return self.error_expr(span);
        }
        // A bare literal takes the target type, so `u8(256)` is a compile error and
        // `f32(0.1)` rounds once, not twice.
        let hint = is_number_literal(&arg.value).then_some(&target);
        let value = self.check_expr(&arg.value, hint);
        let from_ty = self.default_ty(&value.ty);
        let (Some(from), Some(to)) = (Num::of(&from_ty), Num::of(&target)) else {
            if !from_ty.is_error() {
                // Untyped literals inside (`[1]`) read as the types they default to.
                let defaulted = self.infer.finalize(&from_ty).0;
                let shown = self.show(&defaulted);
                self.ck.report(
                    Diagnostic::error(
                        format!("cannot convert a value of type `{shown}` to `{name}`"),
                        value.span,
                    )
                    .with_hint(format!(
                        "`{name}(x)` converts between number types; this is a `{shown}`"
                    )),
                );
            }
            return self.error_expr(span);
        };
        self.mk(
            hir::ExprKind::Call(Box::new(hir::Call {
                callee: Callee::Intrinsic(Intrinsic::Convert { from, to }),
                type_args: Vec::new(),
                args: vec![Arg::Value(value)],
            })),
            target,
            span,
        )
    }

    /// `u8.MAX`, `int.MIN`, `float.EPSILON`: constants of a number type.
    pub(crate) fn check_num_const(&mut self, ty: Ty, name: &ast::Ident, span: Span) -> Expr {
        let lit = match (&ty, name.name.as_str()) {
            (Ty::Int(t), "MAX") => int_lit(*t, t.max()),
            (Ty::Int(t), "MIN") => int_lit(*t, t.min()),
            (Ty::Float, "MAX") => Lit::Float(f64::MAX),
            (Ty::Float, "MIN") => Lit::Float(f64::MIN),
            (Ty::Float, "EPSILON") => Lit::Float(f64::EPSILON),
            (Ty::Float, "INFINITY") => Lit::Float(f64::INFINITY),
            (Ty::Float, "NAN") => Lit::Float(f64::NAN),
            (Ty::F32, "MAX") => Lit::F32(f32::MAX),
            (Ty::F32, "MIN") => Lit::F32(f32::MIN),
            (Ty::F32, "EPSILON") => Lit::F32(f32::EPSILON),
            (Ty::F32, "INFINITY") => Lit::F32(f32::INFINITY),
            (Ty::F32, "NAN") => Lit::F32(f32::NAN),
            _ => {
                let shown = self.show(&ty);
                let names: &[&str] = if matches!(ty, Ty::Int(_)) {
                    &["MAX", "MIN"]
                } else {
                    &["MAX", "MIN", "EPSILON", "INFINITY", "NAN"]
                };
                let hint = suggest::closest(&name.name, names.iter().copied())
                    .map(|s| format!("did you mean `{s}`?"))
                    .unwrap_or_else(|| format!("its constants are: {}", names.join(", ")));
                self.ck.report(
                    Diagnostic::error(
                        format!("`{shown}` has no constant `{}`", name.name),
                        name.span,
                    )
                    .with_hint(hint),
                );
                return self.error_expr(span);
            }
        };
        self.mk(hir::ExprKind::Lit(lit), ty, span)
    }

    /// `x.wrapping_add(y)`, `x.sqrt()` and the rest of the methods of a number type.
    pub(crate) fn check_num_method(
        &mut self,
        recv: Expr,
        recv_ty: &Ty,
        name: &ast::Ident,
        args: &[ast::Arg],
        span: Span,
    ) -> Expr {
        let (Some(sig), Some(num)) = (num_sig(recv_ty, &name.name), Num::of(recv_ty)) else {
            self.check_args_loosely(args);
            let shown = self.show(recv_ty);
            let candidates = if matches!(recv_ty, Ty::Int(_)) {
                INT_METHODS
            } else {
                FLOAT_METHODS
            };
            // Conversions are calls named after the target type, not methods.
            let conversion = matches!(
                name.name.as_str(),
                "to_float" | "to_f64" | "as_float" | "as_f64" | "to_int" | "to_i64" | "as_int"
            );
            let hint = if conversion {
                "numbers convert with a call named after the target type: `float(x)`, `int(x)`"
                    .to_string()
            } else {
                suggest::closest(&name.name, candidates.iter().copied())
                    .map(|s| format!("did you mean `{s}`?"))
                    .unwrap_or_else(|| format!("its methods are: {}", candidates.join(", ")))
            };
            self.ck.report(
                Diagnostic::error(
                    format!("`{shown}` has no method `{}`", name.name),
                    name.span,
                )
                .with_hint(hint),
            );
            return self.error_expr(span);
        };
        let params: Vec<ParamInfo<'static>> = sig
            .params
            .iter()
            .map(|(pname, ty)| ParamInfo {
                name: (*pname).to_string(),
                ty: ty.clone(),
                span: name.span,
                default: None,
                mutable: false,
                module: BASE,
            })
            .collect();
        let full = format!("{}.{}", self.show(recv_ty), name.name);
        let Some(bound) = self.bind_args(&full, &params, args, span, 1) else {
            return self.error_expr_typed(span, sig.ret);
        };
        self.finish_call(
            Callee::Intrinsic(Intrinsic::NumMethod(sig.method, num)),
            Some(Receiver::Value(recv)),
            bound,
            sig.ret,
            Vec::new(),
            span,
        )
    }
}

/// Whether `e` is a plain number literal, possibly negated or parenthesised.
fn is_number_literal(e: &ast::Expr) -> bool {
    match &e.kind {
        ast::ExprKind::Int(_) | ast::ExprKind::Float(_) => true,
        ast::ExprKind::Paren(inner) => is_number_literal(inner),
        ast::ExprKind::Unary {
            op: ast::UnOp::Neg,
            expr,
        } => is_number_literal(expr),
        _ => false,
    }
}

/// The literal for an integer constant of type `ty`.
fn int_lit(ty: IntTy, v: i128) -> Lit {
    if ty == IntTy::U64 {
        Lit::U64(v as u64)
    } else {
        Lit::Int(v as i64)
    }
}
