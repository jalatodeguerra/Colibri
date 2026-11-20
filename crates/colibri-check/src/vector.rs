//! The vector types (DESIGN §15.1): constructors, component fields and swizzles, arithmetic and
//! the methods. See ARCHITECTURE "Vector types as built".

use colibri_syntax::ast::{self, BinOp as AstBin};
use colibri_syntax::{Diagnostic, Span};

use crate::body::FnCtx;
use crate::call::Receiver;
use crate::checker::ParamInfo;
use crate::hir::{
    self, Arg, ArithOp, BinOp, Callee, Coercion, Expr, Intrinsic, UnOp, ValueProj, VecMethod,
    VecShape,
};
use crate::modules::BASE;
use crate::suggest;
use crate::types::{Ty, VecTy};

/// The component letters, in order.
const LETTERS: [char; 4] = ['x', 'y', 'z', 'w'];

/// The index of a component letter.
fn lane_of(c: char) -> Option<u8> {
    LETTERS.iter().position(|&l| l == c).map(|i| i as u8)
}

/// The components a vector type has, as source text (`x, y, z`).
fn components(v: VecTy) -> String {
    LETTERS[..v.n as usize]
        .iter()
        .map(char::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// The methods of a vector type, as written.
fn method_names(v: VecTy) -> Vec<&'static str> {
    let mut names = vec!["dot", "len", "len_sq", "distance", "abs", "min", "max"];
    if v.n == 3 {
        names.push("cross");
    }
    if !v.int {
        names.push("normalize");
        names.push("lerp");
    }
    names
}

/// A vector method with its types filled in for one receiver.
struct VecSig {
    method: VecMethod,
    params: Vec<(&'static str, Ty)>,
    ret: Ty,
}

/// The method `name` of the vector type `v`, if it has one.
fn vec_sig(v: VecTy, name: &str) -> Option<VecSig> {
    use VecMethod as M;
    let this = Ty::Vec(v);
    let scalar = v.scalar();
    let float = Ty::F32;
    let same = |method: M, ret: Ty| VecSig {
        method,
        params: vec![("other", this.clone())],
        ret,
    };
    let unary = |method: M, ret: Ty| VecSig {
        method,
        params: Vec::new(),
        ret,
    };
    Some(match name {
        "dot" => same(M::Dot, scalar),
        "cross" if v.n == 3 => same(M::Cross, this.clone()),
        "len" => unary(M::Len, float),
        "len_sq" => unary(M::LenSq, scalar),
        "distance" => same(M::Distance, float),
        "abs" => unary(M::Abs, this.clone()),
        "min" => same(M::Min, this.clone()),
        "max" => same(M::Max, this.clone()),
        "normalize" if !v.int => unary(M::Normalize, this.clone()),
        // `t` is a `float`, so `a.lerp(b, dt * speed)` works with `dt: float`; the engine narrows
        // it to `f32` (DESIGN §5.1 vector-by-scalar rule).
        "lerp" if !v.int => VecSig {
            method: M::Lerp,
            params: vec![("other", this.clone()), ("t", Ty::Float)],
            ret: this.clone(),
        },
        _ => return None,
    })
}

/// The methods of a vector type with their signatures, for editors.
pub(crate) fn vec_methods(v: VecTy) -> Vec<crate::members::MethodSig> {
    method_names(v)
        .into_iter()
        .filter_map(|name| {
            let sig = vec_sig(v, name)?;
            Some(crate::members::MethodSig {
                name: name.to_string(),
                params: sig
                    .params
                    .iter()
                    .map(|(n, t)| (n.to_string(), t.clone()))
                    .collect(),
                ret: sig.ret,
            })
        })
        .collect()
}

/// Whether the vector type has a method called `name`.
pub(crate) fn vec_method_exists(v: VecTy, name: &str) -> bool {
    vec_sig(v, name).is_some()
}

/// What a name after a dot selects from a vector.
enum Select {
    /// One component.
    Comp(u8),
    /// Several components, as a vector of that length.
    Swizzle([u8; 4], u8),
}

impl FnCtx<'_, '_> {
    /// A scalar operand of a vector `v`: an `f32` for `vecN`, an `i32` for `ivecN`. Untyped
    /// literals adapt; a `float` narrows to `f32` (DESIGN §5.1). `None` after reporting.
    pub(crate) fn vec_scalar(&mut self, v: VecTy, value: Expr) -> Option<Expr> {
        let target = v.scalar();
        let ty = self.infer.shallow(&value.ty);
        if ty.is_error() {
            return None;
        }
        if !v.int && ty == Ty::Float {
            let span = value.span;
            return Some(self.mk(
                hir::ExprKind::Coerce(Box::new(value), Coercion::FloatToF32),
                Ty::F32,
                span,
            ));
        }
        if self.infer.unify(&value.ty, &target).is_ok() {
            return Some(value);
        }
        let (found, wanted) = (self.show(&value.ty), self.show(&target));
        let hint = if v.int {
            format!("`ivec` components are `i32`; convert with `i32(x)`, found `{found}`")
        } else {
            format!("`vec` components are `f32`; convert with `f32(x)`, found `{found}`")
        };
        self.ck.report(
            Diagnostic::error(format!("expected `{wanted}`, found `{found}`"), value.span)
                .with_hint(hint),
        );
        None
    }

    /// `vec2(x, y)`, `ivec3(1)`, `vec2(iv)`: the constructor call that names its type.
    pub(crate) fn check_vec_ctor(&mut self, v: VecTy, args: &[ast::Arg], span: Span) -> Expr {
        let name = v.name();
        let n = v.n as usize;
        if let Some(named) = args.iter().find_map(|a| a.name.as_ref()) {
            self.check_args_loosely(args);
            self.ck.report(Diagnostic::error(
                format!("`{name}(..)` does not take named arguments"),
                named.span,
            ));
            return self.error_expr(span);
        }
        if args.len() != 1 && args.len() != n {
            self.check_args_loosely(args);
            self.ck.report(
                Diagnostic::error(
                    format!(
                        "`{name}(..)` takes {n} components or one value for all of them, found {}",
                        args.len()
                    ),
                    span,
                )
                .with_hint(format!("write `{name}({})` or `{name}(0)`", {
                    vec!["0"; n].join(", ")
                })),
            );
            return self.error_expr(span);
        }
        let scalar = v.scalar();
        let mut values = Vec::new();
        for arg in args {
            let value = self.check_expr(&arg.value, Some(&scalar));
            if args.len() == 1 {
                // A vector of the other kind and the same length converts component-wise.
                match self.infer.shallow(&value.ty) {
                    Ty::Vec(from) if from == v || from == v.other_kind() => {
                        return self.vec_new(v, vec![value], span);
                    }
                    Ty::Vec(from) => {
                        let (a, b) = (from.name(), v.name());
                        self.ck.report(
                            Diagnostic::error(
                                format!("cannot convert `{a}` to `{b}`"),
                                value.span,
                            )
                            .with_hint(format!(
                                "a vector converts to the other kind of the same length (`{}`); to change the length, build it from its components",
                                v.other_kind().name()
                            )),
                        );
                        return self.error_expr(span);
                    }
                    _ => {}
                }
            }
            match self.vec_scalar(v, value) {
                Some(value) => values.push(value),
                None => {
                    // Keep checking the rest so that every bad component is reported.
                    for rest in &args[values.len() + 1..] {
                        self.check_expr(&rest.value, Some(&scalar));
                    }
                    return self.error_expr(span);
                }
            }
        }
        self.vec_new(v, values, span)
    }

    fn vec_new(&mut self, v: VecTy, values: Vec<Expr>, span: Span) -> Expr {
        self.mk(
            hir::ExprKind::Call(Box::new(hir::Call {
                callee: Callee::Intrinsic(Intrinsic::VecNew(v)),
                type_args: Vec::new(),
                args: values.into_iter().map(Arg::Value).collect(),
            })),
            Ty::Vec(v),
            span,
        )
    }

    /// Resolves the name after a dot on a vector: a component or a swizzle.
    fn vec_select(&mut self, v: VecTy, name: &ast::Ident) -> Option<Select> {
        let text = name.name.as_str();
        let letters: Vec<char> = text.chars().collect();
        let lanes: Option<Vec<u8>> = letters.iter().map(|&c| lane_of(c)).collect();
        let shown = v.name();
        let Some(lanes) = lanes.filter(|l| (1..=4).contains(&l.len())) else {
            let swizzle_like = letters.len() > 1
                && letters.len() <= 4
                && letters.iter().any(|&c| lane_of(c).is_some());
            let hint = if letters.len() > 4 && letters.iter().all(|&c| lane_of(c).is_some()) {
                "a swizzle has at most 4 components".to_string()
            } else if swizzle_like {
                "swizzle letters are x, y, z and w".to_string()
            } else {
                suggest::closest(text, ["x", "y", "z", "w"])
                    .map(|s| format!("did you mean `{s}`?"))
                    .unwrap_or_else(|| format!("`{shown}` has the components {}", components(v)))
            };
            self.ck.report(
                Diagnostic::error(format!("`{shown}` has no field `{text}`"), name.span)
                    .with_hint(hint),
            );
            return None;
        };
        if let Some(&bad) = lanes.iter().find(|&&l| l >= v.n) {
            let letter = LETTERS[bad as usize];
            self.ck.report(
                Diagnostic::error(format!("`{shown}` has no component `{letter}`"), name.span)
                    .with_hint(format!("`{shown}` has the components {}", components(v))),
            );
            return None;
        }
        if lanes.len() == 1 {
            return Some(Select::Comp(lanes[0]));
        }
        let mut out = [0u8; 4];
        out[..lanes.len()].copy_from_slice(&lanes);
        Some(Select::Swizzle(out, lanes.len() as u8))
    }

    /// `value.x` or `value.zyx` for a vector value (`base` is its source, for messages).
    pub(crate) fn vec_field_value(
        &mut self,
        v: VecTy,
        value: Expr,
        name: &ast::Ident,
        span: Span,
    ) -> Expr {
        match self.vec_select(v, name) {
            None => self.error_expr(span),
            Some(Select::Comp(i)) => self.project_vec(value, i, v.scalar(), &name.name, span),
            Some(Select::Swizzle(lanes, len)) => {
                let ty = Ty::Vec(v.with_len(len));
                self.mk(
                    hir::ExprKind::Call(Box::new(hir::Call {
                        callee: Callee::Intrinsic(Intrinsic::VecSwizzle {
                            from: v,
                            lanes,
                            len,
                        }),
                        type_args: Vec::new(),
                        args: vec![Arg::Value(value)],
                    })),
                    ty,
                    span,
                )
            }
        }
    }

    /// The step of a place path for `base.name` on a vector, for an assignment target.
    pub(crate) fn vec_place_step(
        &mut self,
        v: VecTy,
        name: &ast::Ident,
        whole: Span,
    ) -> Option<(ValueProj, Ty)> {
        match self.vec_select(v, name)? {
            Select::Comp(i) => Some((ValueProj::Comp(i), v.scalar())),
            Select::Swizzle(..) => {
                let letters: Vec<String> = name.name.chars().map(String::from).collect();
                let example = format!(
                    "assign the components one by one: `{}.{} = ..`",
                    "v", letters[0]
                );
                self.ck.report(
                    Diagnostic::error(
                        format!("cannot assign to the swizzle `.{}`", name.name),
                        whole,
                    )
                    .with_hint(example),
                );
                None
            }
        }
    }

    /// A component of a vector value: a longer path when it is a place, a projection otherwise.
    fn project_vec(&mut self, value: Expr, i: u8, ty: Ty, name: &str, span: Span) -> Expr {
        match value.kind {
            hir::ExprKind::Read(mut place) => {
                place.path.push(ValueProj::Comp(i));
                place.ty = ty.clone();
                place.display = format!("{}.{name}", place.display);
                self.mk(hir::ExprKind::Read(place), ty, span)
            }
            kind => {
                let base = Expr {
                    kind,
                    ty: value.ty,
                    span: value.span,
                };
                self.mk(
                    hir::ExprKind::Project(Box::new(base), ValueProj::Comp(i)),
                    ty,
                    span,
                )
            }
        }
    }

    /// `-v`
    pub(crate) fn check_vec_neg(&mut self, v: VecTy, value: Expr, span: Span) -> Expr {
        self.mk(
            hir::ExprKind::Unary(UnOp::VecNeg(v), Box::new(value)),
            Ty::Vec(v),
            span,
        )
    }

    /// An arithmetic operator with a vector on at least one side (both operands are checked).
    pub(crate) fn check_vec_arith(
        &mut self,
        op: AstBin,
        arith: ArithOp,
        mut l: Expr,
        mut r: Expr,
        span: Span,
    ) -> Expr {
        let (lt, rt) = (self.infer.shallow(&l.ty), self.infer.shallow(&r.ty));
        let symbol = crate::expr::op_symbol(op);
        if arith == ArithOp::Rem {
            self.ck.report(
                Diagnostic::error("`%` cannot be applied to vectors", span).with_hint(
                    "vectors have `+ - * /`; take the remainder of each component by hand",
                ),
            );
            return self.error_expr(span);
        }
        let (v, shape) = match (&lt, &rt) {
            (Ty::Error, _) | (_, Ty::Error) => return self.error_expr(span),
            (Ty::Vec(a), Ty::Vec(b)) if a == b => (*a, VecShape::VecVec),
            (Ty::Vec(_), Ty::Vec(_)) => {
                // Reports "cannot apply `+` to `vec2` and `vec3`".
                self.unify_operands(op, &mut l, &mut r, span);
                return self.error_expr(span);
            }
            (Ty::Vec(a), _) => {
                if !matches!(arith, ArithOp::Mul | ArithOp::Div) {
                    return self.vec_scalar_error(symbol, *a, &r, span);
                }
                let Some(scalar) = self.vec_scalar(*a, r) else {
                    return self.error_expr(span);
                };
                r = scalar;
                (*a, VecShape::VecScalar)
            }
            (_, Ty::Vec(b)) => {
                if arith != ArithOp::Mul {
                    let shown = self.show(&lt);
                    self.ck.report(
                        Diagnostic::error(
                            format!("`{symbol}` cannot be applied to `{shown}` and `{}`", b.name()),
                            span,
                        )
                        .with_label(l.span, format!("this is `{shown}`"))
                        .with_label(r.span, format!("this is `{}`", b.name()))
                        .with_hint(if arith == ArithOp::Div {
                            "a number cannot be divided by a vector; divide the components by hand"
                        } else {
                            "a vector and a number combine only with `*` and `/`; add or subtract a vector (`vec2(1)` has 1 in every component)"
                        }),
                    );
                    return self.error_expr(span);
                }
                let Some(scalar) = self.vec_scalar(*b, l) else {
                    return self.error_expr(span);
                };
                l = scalar;
                (*b, VecShape::ScalarVec)
            }
            _ => return self.error_expr(span),
        };
        self.mk(
            hir::ExprKind::Binary(BinOp::Vec(arith, v, shape), Box::new(l), Box::new(r)),
            Ty::Vec(v),
            span,
        )
    }

    fn vec_scalar_error(&mut self, symbol: &str, v: VecTy, other: &Expr, span: Span) -> Expr {
        let shown = self.show(&other.ty);
        self.ck.report(
            Diagnostic::error(
                format!("`{symbol}` cannot be applied to `{}` and `{shown}`", v.name()),
                span,
            )
            .with_hint(format!(
                "a vector and a number combine only with `*` and `/`; add or subtract a vector (`{}(1)` has 1 in every component)",
                v.name()
            )),
        );
        self.error_expr(span)
    }

    /// `v += w`, `v *= 2`: the operator of a compound assignment to a vector place.
    pub(crate) fn check_vec_assign_op(
        &mut self,
        v: VecTy,
        arith: ArithOp,
        sym: &str,
        rhs: Expr,
        span: Span,
    ) -> Option<(BinOp, Expr)> {
        if arith == ArithOp::Rem {
            self.ck.report(
                Diagnostic::error("`%=` cannot be applied to vectors", span)
                    .with_hint("vectors have `+= -= *= /=`"),
            );
            return None;
        }
        match self.infer.shallow(&rhs.ty) {
            Ty::Error => None,
            Ty::Vec(w) if w == v => Some((BinOp::Vec(arith, v, VecShape::VecVec), rhs)),
            Ty::Vec(w) => {
                self.report_mismatch(rhs.span, &Ty::Vec(v), &Ty::Vec(w), None);
                None
            }
            _ => {
                if !matches!(arith, ArithOp::Mul | ArithOp::Div) {
                    let shown = self.show(&rhs.ty);
                    self.ck.report(
                        Diagnostic::error(
                            format!("`{sym}` cannot be applied to `{}` and `{shown}`", v.name()),
                            span,
                        )
                        .with_hint("a vector and a number combine only with `*=` and `/=`"),
                    );
                    return None;
                }
                let scalar = self.vec_scalar(v, rhs)?;
                Some((BinOp::Vec(arith, v, VecShape::VecScalar), scalar))
            }
        }
    }

    /// `v.dot(w)`, `v.normalize()` and the rest of the methods of a vector type.
    pub(crate) fn check_vec_method(
        &mut self,
        recv: Expr,
        v: VecTy,
        name: &ast::Ident,
        args: &[ast::Arg],
        span: Span,
    ) -> Expr {
        let Some(sig) = vec_sig(v, &name.name) else {
            self.check_args_loosely(args);
            let shown = v.name();
            let mut candidates = method_names(v);
            candidates.sort_unstable();
            let hint = match name.name.as_str() {
                "cross" => "`cross` is defined for `vec3` and `ivec3`; in 2D the cross product is `a.x * b.y - a.y * b.x`".to_string(),
                "normalize" | "lerp" if v.int => format!(
                    "`{shown}` has integer components; convert first: `{}(v).{}(..)`",
                    v.other_kind().name(),
                    name.name
                ),
                _ => suggest::closest(&name.name, candidates.iter().copied())
                    .map(|s| format!("did you mean `{s}`?"))
                    .unwrap_or_else(|| format!("its methods are: {}", candidates.join(", "))),
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
        let full = format!("{}.{}", v.name(), name.name);
        let Some(bound) = self.bind_args(&full, &params, args, span, 1) else {
            return self.error_expr_typed(span, sig.ret);
        };
        self.finish_call(
            Callee::Intrinsic(Intrinsic::VecMethod(sig.method, v)),
            Some(Receiver::Value(recv)),
            bound,
            sig.ret,
            Vec::new(),
            span,
        )
    }
}
