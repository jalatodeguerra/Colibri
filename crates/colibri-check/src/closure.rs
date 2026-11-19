//! Closures (DESIGN §6.2): checking `fn(x) => e` and `fn() { .. }` expressions against the
//! expected type, capturing variables of the enclosing frames, and deciding how each captured
//! variable is stored (ARCHITECTURE §1).
//!
//! A closure body is checked in a frame of its own inside the same [`FnCtx`], so it shares the
//! inference table with the enclosing function (the expected type flows into its parameters and
//! its result flows back out). Names the body uses that belong to an enclosing frame become
//! *captures*: the closure gets a local of its own for each, filled in when the closure is
//! created. Whether a captured variable needs a shared cell is only known once the enclosing
//! body is complete (a later assignment counts), so storage is settled in [`FnCtx::finish`].

use colibri_syntax::ast::{self, FnBody};
use colibri_syntax::{Diagnostic, Span};

use crate::body::{BlockMode, CaptureRec, DoneClosure, FnCtx, Frame, LocalFlags, LocalKind};
use crate::checker::{FnInfo, ParamInfo};
use crate::hir::{
    self, Block, Body, Capture, CaptureMode, ClosureExpr, Expr, FnId, LocalId, LocalInfo, Storage,
};
use crate::infer::VarKind;
use crate::modules::BASE;
use crate::types::Ty;
use crate::zonk;

/// The parameter and result types an expected function type gives a closure.
struct Expectation {
    params: Vec<Ty>,
    ret: Ty,
}

impl FnCtx<'_, '_> {
    /// A closure expression. `expected` is the type the context wants: when it is a function
    /// type, its parameter and result types fill in whatever the closure leaves unwritten.
    #[inline(never)]
    pub fn check_closure(&mut self, c: &ast::Closure, expected: Option<&Ty>, span: Span) -> Expr {
        let arity_mismatch = matches!(
            expected.map(|t| self.infer.shallow(t)),
            Some(Ty::Fn(p, _)) if p.len() != c.params.len()
        );
        let expectation = self.closure_expectation(c, expected, span);
        let param_tys = self.closure_param_types(c, expectation.as_ref(), arity_mismatch);
        let ret = self.closure_ret_type(c, expectation.as_ref());

        let f = self.new_closure_fn(span);
        self.enter_closure_frame(ret.clone());
        self.push_scope();
        for (p, ty) in c.params.iter().zip(&param_tys) {
            let kind = if p.mutable {
                LocalKind::Var
            } else {
                LocalKind::Param
            };
            if p.default.is_some() {
                self.ck
                    .unsupported("a default value for a closure parameter", p.span);
            }
            self.declare(&p.name.name, p.name.span, ty.clone(), kind);
        }
        self.declare_explicit_captures(c);
        let outer_unsafe = std::mem::take(&mut self.unsafe_depth);
        let block = self.closure_body(&c.body, &ret);
        self.unsafe_depth = outer_unsafe;
        let mut done = self.leave_closure_frame(f, block, ret.clone(), span);
        done.param_count = c.params.len() as u32;
        let captures: Vec<Capture> = done
            .frame
            .captures
            .iter()
            .map(|r| Capture {
                outer: r.outer,
                inner: r.inner,
                mode: r.mode,
            })
            .collect();
        self.done.push(done);
        if arity_mismatch {
            // Already reported; no second error about the closure's type.
            return self.error_expr(span);
        }
        let ty = Ty::Fn(param_tys, Box::new(ret));
        self.mk(
            hir::ExprKind::Closure(Box::new(ClosureExpr { f, captures })),
            ty,
            span,
        )
    }

    /// The function type the context expects, if it is one with the closure's arity.
    fn closure_expectation(
        &mut self,
        c: &ast::Closure,
        expected: Option<&Ty>,
        span: Span,
    ) -> Option<Expectation> {
        let Ty::Fn(params, ret) = self.infer.shallow(expected?) else {
            return None;
        };
        if params.len() != c.params.len() {
            let plural = |n: usize| if n == 1 { "parameter" } else { "parameters" };
            self.ck.report(
                Diagnostic::error(
                    format!(
                        "this closure takes {} {}, but {} {} expected here",
                        c.params.len(),
                        plural(c.params.len()),
                        params.len(),
                        if params.len() == 1 { "is" } else { "are" },
                    ),
                    span,
                )
                .with_hint(format!(
                    "the expected type is `{}`",
                    self.show(&Ty::Fn(params, ret))
                )),
            );
            return None;
        }
        Some(Expectation { params, ret: *ret })
    }

    /// The type of each parameter: as written, else from the expected function type, else an
    /// error asking for an annotation.
    fn closure_param_types(
        &mut self,
        c: &ast::Closure,
        expect: Option<&Expectation>,
        already_reported: bool,
    ) -> Vec<Ty> {
        let mut out = Vec::new();
        let mut reported = already_reported;
        for (i, p) in c.params.iter().enumerate() {
            let wanted = expect.map(|e| e.params[i].clone());
            let ty = match (&p.ty, wanted) {
                (Some(t), wanted) => {
                    let ty = self.ck.resolve_type(t);
                    if let Some(w) = wanted {
                        // A mismatch is reported where the closure is used.
                        let _ = self.infer.unify(&ty, &w);
                    }
                    ty
                }
                (None, Some(w)) => w,
                (None, None) => {
                    if !reported {
                        reported = true;
                        self.ck.report(
                            Diagnostic::error(
                                format!("cannot infer the type of the parameter `{}`", p.name.name),
                                p.name.span,
                            )
                            .with_hint(format!(
                                "write the type: `fn({}: int) => ...`, or use the closure where a function type is expected",
                                p.name.name
                            )),
                        );
                    }
                    Ty::Error
                }
            };
            out.push(ty);
        }
        out
    }

    /// The result type: as written, else the expected one, else a fresh variable.
    fn closure_ret_type(&mut self, c: &ast::Closure, expect: Option<&Expectation>) -> Ty {
        match (&c.ret, expect) {
            (Some(t), expect) => {
                let ty = self.ck.resolve_ret_type(t);
                if let Some(e) = expect {
                    let _ = self.infer.unify(&ty, &e.ret);
                }
                ty
            }
            (None, Some(e)) => e.ret.clone(),
            (None, None) => self.fresh_var(VarKind::General),
        }
    }

    /// Reserves the function id of a closure's lifted body.
    fn new_closure_fn(&mut self, span: Span) -> FnId {
        let id = FnId(self.ck.fns.len() as u32);
        self.ck.fns.push(FnInfo {
            name: format!("<closure in {}>", self.name),
            name_span: span,
            span,
            params: Vec::new(),
            receiver: None,
            ret: Ty::Unit,
            ret_span: None,
            decl: None,
            body: None,
            deps: Vec::new(),
            generics: Vec::new(),
            self_ty: None,
            host: None,
            module: self.ck.cur_mod,
            public: false,
        });
        id
    }

    /// Makes a fresh frame the live one; the frame it replaces becomes the closure's parent.
    fn enter_closure_frame(&mut self, ret: Ty) {
        let id = self.next_frame_id;
        self.next_frame_id += 1;
        let mut frame = Frame::new(id, Some((ret, None)));
        self.swap_frame(&mut frame);
        self.parents.push(frame);
    }

    /// Restores the enclosing frame and returns the closure's finished one.
    fn leave_closure_frame(&mut self, f: FnId, block: Block, ret: Ty, span: Span) -> DoneClosure {
        let mut frame = self.parents.pop().unwrap_or_else(|| Frame::new(0, None));
        self.swap_frame(&mut frame);
        DoneClosure {
            f,
            parent: self.frame_id,
            frame,
            param_count: 0,
            block,
            ret,
            span,
        }
    }

    /// The body of the closure, in the scope that holds its parameters. A `{ }` body follows
    /// the rule of function bodies (DESIGN §6.1): it must `return` a value if it has one.
    fn closure_body(&mut self, body: &FnBody, ret: &Ty) -> Block {
        match body {
            FnBody::Expr(e) => {
                let value = self.check_expr(e, Some(ret));
                self.expect(&value, ret, None);
                let locals = self.pop_scope();
                Block {
                    locals,
                    stmts: Vec::new(),
                    ty: value.ty.clone(),
                    span: e.span,
                    tail: Some(Box::new(value)),
                }
            }
            FnBody::Block(b) => {
                let (stmts, tail, ty) = self.block_in_scope(b, BlockMode::Stmts);
                let locals = self.pop_scope();
                if self.infer.shallow(&ty) != Ty::Never {
                    self.close_falling_body(ret, b.span);
                }
                Block {
                    locals,
                    stmts,
                    tail,
                    ty,
                    span: b.span,
                }
            }
        }
    }

    /// A `{ }` closure body that can end without `return`: fine for `()`, an error otherwise.
    fn close_falling_body(&mut self, ret: &Ty, body: Span) {
        match self.infer.shallow(ret) {
            Ty::Unit | Ty::Error => {}
            t @ Ty::Infer(_) => {
                if self.infer.unify(&t, &Ty::Unit).is_err() {
                    self.report_missing_return(ret, body);
                }
            }
            _ => self.report_missing_return(ret, body),
        }
    }

    fn report_missing_return(&mut self, ret: &Ty, body: Span) {
        let close = Span::new(body.file, body.end.saturating_sub(1), body.end);
        let shown = self.show(ret);
        self.ck.report(
            Diagnostic::error(
                format!("this closure must return `{shown}`, but its body can end without returning"),
                close,
            )
            .with_hint("a `{ }` body needs `return value`; for a single expression write `fn(..) => value`"),
        );
    }

    // ---- captures ----

    /// Declares the variables of a `use(..)` list: `weak` ones as optional locals, plain ones
    /// as ordinary captures.
    fn declare_explicit_captures(&mut self, c: &ast::Closure) {
        for cap in &c.captures {
            let name = cap.name.name.as_str();
            self.enter_parent();
            let found = self.lookup_local(name).map(|o| {
                self.flags[o.0 as usize].read = true;
                if !cap.weak {
                    self.flags[o.0 as usize].captured = true;
                }
                (
                    o,
                    self.locals[o.0 as usize].clone(),
                    self.kinds[o.0 as usize],
                )
            });
            self.leave_parent();
            let Some((outer, info, kind)) = found else {
                self.report_unknown_name(name, cap.name.span);
                continue;
            };
            if !cap.weak {
                let inner = self.add_capture(outer, &info, kind, CaptureMode::ByValue);
                self.name_capture(name, inner);
                continue;
            }
            if !self.is_class_ty(&info.ty) || self.is_host_class_ty(&info.ty) {
                let shown = self.show(&info.ty);
                self.ck.report(
                    Diagnostic::error(
                        format!("`{name}` is a `{shown}`, and only a class can be captured weakly"),
                        cap.name.span,
                    )
                    .with_hint("a weak capture does not keep an object alive, so it needs a class instance; other values are captured by the closure itself"),
                );
                continue;
            }
            let ty = Ty::Option(Box::new(info.ty.clone()));
            let inner = self.declare(name, cap.name.span, ty, LocalKind::Let);
            self.captures.push(CaptureRec {
                outer,
                inner,
                mode: CaptureMode::Weak,
            });
        }
    }

    /// Makes `name` find the capture `inner` in the closure's outermost scope.
    fn name_capture(&mut self, name: &str, inner: LocalId) {
        self.insert_name(name, inner);
    }

    /// The closure-local standing for `outer`, after `lookup_local` found it in an enclosing
    /// frame (which has already been told it is captured).
    pub(crate) fn capture_local(&mut self, outer: LocalId) -> LocalId {
        let (info, kind) = self.captured_info(outer);
        self.add_capture(outer, &info, kind, CaptureMode::ByValue)
    }

    /// What the live frame's parent says about a local it is about to lend.
    fn captured_info(&mut self, outer: LocalId) -> (LocalInfo, LocalKind) {
        self.enter_parent();
        let i = outer.0 as usize;
        if let Some(flags) = self.flags.get_mut(i) {
            flags.captured = true;
            flags.read = true;
        }
        let out = (self.locals[i].clone(), self.kinds[i]);
        self.leave_parent();
        out
    }

    /// Adds (once per variable) the closure-local that holds the captured `outer`.
    fn add_capture(
        &mut self,
        outer: LocalId,
        info: &LocalInfo,
        kind: LocalKind,
        mode: CaptureMode,
    ) -> LocalId {
        if let Some(rec) = self
            .captures
            .iter()
            .find(|r| r.outer == outer && r.mode != CaptureMode::Weak)
        {
            return rec.inner;
        }
        let inner = LocalId(self.locals.len() as u32);
        self.locals.push(LocalInfo {
            name: info.name.clone(),
            ty: info.ty.clone(),
            mutable: info.mutable,
            storage: Storage::Slot,
            span: info.span,
        });
        self.kinds.push(kind);
        self.flags.push(LocalFlags::default());
        self.captures.push(CaptureRec { outer, inner, mode });
        inner
    }

    // ---- finishing ----

    /// Turns every checked closure into a function: resolves its types (the enclosing body
    /// has settled the shared inference table) and stores its body under its id.
    pub(crate) fn finish_closures(&mut self, diags: &mut Vec<Diagnostic>, had_errors: bool) {
        for mut d in std::mem::take(&mut self.done) {
            zonk::zonk_body(
                &mut self.infer,
                &mut d.block,
                &mut d.frame.locals,
                diags,
                had_errors,
                &self.ck.host_typed,
                self.ck.facts.as_mut(),
            );
            let ret = self.infer.finalize(&d.ret).0;
            let params: Vec<ParamInfo<'_>> = d
                .frame
                .locals
                .iter()
                .take(d.param_count as usize)
                .map(|l| ParamInfo {
                    name: l.name.clone(),
                    ty: l.ty.clone(),
                    span: l.span,
                    default: None,
                    mutable: l.mutable,
                    module: BASE,
                })
                .collect();
            let info = &mut self.ck.fns[d.f.0 as usize];
            let body = Body {
                name: info.name.clone(),
                span: d.span,
                param_count: d.param_count,
                locals: d.frame.locals,
                block: d.block,
                ret: ret.clone(),
                mutates_self: false,
            };
            info.params = params;
            info.ret = ret;
            info.body = Some(std::rc::Rc::new(body));
        }
    }

    // ---- storage ----

    /// Decides, for the outermost body and every closure inside it, which locals live in
    /// cells (ARCHITECTURE §1): a local is a `Cell` iff it is captured by reference, is `var`
    /// and is assigned after its initialisation, in its own body or in any closure.
    pub(crate) fn settle_storage(&mut self) {
        // Assignments inside a closure count for the variable it captures, innermost first.
        for i in 0..self.done.len() {
            let c = &self.done[i];
            let (parent, assigned): (usize, Vec<LocalId>) = (
                c.parent,
                c.frame
                    .captures
                    .iter()
                    .filter(|r| {
                        r.mode != CaptureMode::Weak
                            && c.frame
                                .flags
                                .get(r.inner.0 as usize)
                                .is_some_and(|f| f.assigned)
                    })
                    .map(|r| r.outer)
                    .collect(),
            );
            for outer in assigned {
                self.mark_assigned_in(parent, outer);
            }
        }
        // Storage follows from the flags; closures take it from the variable they capture, so
        // enclosing frames are decided first.
        let decide = |locals: &mut [LocalInfo], flags: &[LocalFlags], skip: &[LocalId]| {
            for (i, info) in locals.iter_mut().enumerate() {
                if skip.contains(&LocalId(i as u32)) {
                    continue;
                }
                let f = flags.get(i).copied().unwrap_or_default();
                info.storage = if f.captured && info.mutable && f.assigned {
                    Storage::Cell
                } else {
                    Storage::Slot
                };
            }
        };
        decide(&mut self.locals, &self.flags, &[]);
        for i in (0..self.done.len()).rev() {
            let inners: Vec<LocalId> = self.done[i]
                .frame
                .captures
                .iter()
                .map(|r| r.inner)
                .collect();
            let c = &mut self.done[i];
            decide(&mut c.frame.locals, &c.frame.flags, &inners);
            let (parent, recs): (usize, Vec<CaptureRec>) = (c.parent, c.frame.captures.clone());
            for r in recs {
                let storage = if r.mode == CaptureMode::Weak {
                    Storage::Slot
                } else {
                    self.storage_in(parent, r.outer)
                };
                self.done[i].frame.locals[r.inner.0 as usize].storage = storage;
            }
        }
    }

    fn mark_assigned_in(&mut self, frame: usize, local: LocalId) {
        let flags = if frame == self.frame_id {
            &mut self.flags
        } else if let Some(d) = self.done.iter_mut().find(|d| d.frame.id == frame) {
            &mut d.frame.flags
        } else {
            return;
        };
        if let Some(f) = flags.get_mut(local.0 as usize) {
            f.assigned = true;
        }
    }

    fn storage_in(&self, frame: usize, local: LocalId) -> Storage {
        let locals = if frame == self.frame_id {
            &self.locals
        } else if let Some(d) = self.done.iter().find(|d| d.frame.id == frame) {
            &d.frame.locals
        } else {
            return Storage::Slot;
        };
        locals
            .get(local.0 as usize)
            .map_or(Storage::Slot, |l| l.storage)
    }
}
