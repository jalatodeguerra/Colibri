//! Checking one body (a function, an initialiser or the script): scopes, blocks and
//! statements. Expressions are in [`crate::expr`], calls in [`crate::call`].

use std::collections::HashMap;

use colibri_syntax::ast::{self, AssignOp, BindKind, ExprKind, PatKind, StmtKind};
use colibri_syntax::{Diagnostic, Span};

use crate::checker::{Checker, Dep, ParamInfo, ScriptStep, ValueRef};
use crate::exhaust::{self, ArmInfo};
use crate::hir::{
    self, ArithOp, BinOp, BitOp, Block, Body, CaptureMode, Expr, FnId, LocalId, LocalInfo, LoopId,
    Num, Stmt, Storage,
};
use crate::infer::{InferCtx, VarKind};
use crate::modules::ModId;
use crate::obligations::Obligation;
use crate::pat::PatCx;
use crate::suggest;
use crate::types::{IntTy, Ty};
use crate::zonk;

/// How the value of a block is used.
#[derive(Clone, Copy)]
pub(crate) enum BlockMode<'t> {
    /// The block is an expression: its trailing expression is its value.
    Value(Option<&'t Ty>),
    /// The block is a statement list: a trailing expression is evaluated and dropped
    /// (DESIGN §6.1: `{ }` bodies use `return`).
    Stmts,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum LocalKind {
    Param,
    Let,
    Var,
    Const,
    Temp,
}

/// What the checker learned about a local, to decide how it is stored (ARCHITECTURE §1).
#[derive(Clone, Copy, Default)]
pub(crate) struct LocalFlags {
    /// A closure captures it by reference to the variable.
    pub captured: bool,
    /// It is assigned (or changed by a `var self` call) somewhere after its initialisation,
    /// in this body or in a closure that captures it.
    pub assigned: bool,
    /// Its value is read somewhere (or a closure captures it, or a place inside it is used).
    pub read: bool,
    /// An unused-variable warning applies to it if nothing reads it: a plain `let` or `var`.
    pub lint_unused: bool,
}

/// One variable a closure captures, as recorded while its body is checked.
#[derive(Clone, Copy)]
pub(crate) struct CaptureRec {
    pub outer: LocalId,
    pub inner: LocalId,
    pub mode: CaptureMode,
}

/// The state of one frame: a function body or a closure body being checked. The live frame's
/// fields are stored directly in [`FnCtx`]; the enclosing frames of a closure wait in
/// `FnCtx::parents` and are swapped in while a captured name is looked up.
pub(crate) struct Frame {
    pub locals: Vec<LocalInfo>,
    pub kinds: Vec<LocalKind>,
    pub flags: Vec<LocalFlags>,
    scopes: Vec<Scope>,
    scope_floor: usize,
    pub loops: Vec<LoopFrame>,
    pub next_loop: u32,
    pub ret: Option<(Ty, Option<Span>)>,
    defer_depth: u32,
    pub captures: Vec<CaptureRec>,
    pub id: usize,
}

impl Frame {
    pub(crate) fn new(id: usize, ret: Option<(Ty, Option<Span>)>) -> Frame {
        Frame {
            locals: Vec::new(),
            kinds: Vec::new(),
            flags: Vec::new(),
            scopes: Vec::new(),
            scope_floor: 0,
            loops: Vec::new(),
            next_loop: 0,
            ret,
            defer_depth: 0,
            captures: Vec::new(),
            id,
        }
    }
}

/// A closure whose body is checked; its storage and types are settled when the outermost body
/// finishes (see [`FnCtx::finish`]).
pub(crate) struct DoneClosure {
    pub f: FnId,
    pub frame: Frame,
    /// The frame it was created in.
    pub parent: usize,
    pub param_count: u32,
    pub block: Block,
    pub ret: Ty,
    pub span: Span,
}

#[derive(Default)]
struct Scope {
    names: HashMap<String, LocalId>,
    /// Every local declared directly in the scope, in declaration order.
    locals: Vec<LocalId>,
}

pub(crate) struct LoopFrame {
    pub id: LoopId,
    pub label: Option<String>,
}

/// Where a function body comes from: a `fn`'s block or `=>` expression, or a test's block.
enum BodySrc<'x> {
    Block(&'x ast::Block),
    Expr(&'x ast::Expr),
}

/// The state of checking one body.
pub(crate) struct FnCtx<'c, 'a> {
    pub ck: &'c mut Checker<'a>,
    pub infer: InferCtx,
    pub(crate) name: String,
    pub locals: Vec<LocalInfo>,
    pub kinds: Vec<LocalKind>,
    pub flags: Vec<LocalFlags>,
    scopes: Vec<Scope>,
    /// Scopes below this index are invisible (used for default arguments, which cannot see
    /// the caller's locals).
    scope_floor: usize,
    pub loops: Vec<LoopFrame>,
    pub next_loop: u32,
    /// Ids of loops some `break` leaves (a `while true` without one never ends). Ids are per
    /// frame, so a closure's loop can mark an outer id: that only makes the answer cautious.
    broken_loops: Vec<u32>,
    /// The declared return type and its span; `None` where `return` is not allowed.
    pub(crate) ret: Option<(Ty, Option<Span>)>,
    /// Module-level items referred to, each with the span of a use.
    deps: Vec<(Dep, Span)>,
    depth: u32,
    /// The body is a `var self` method.
    self_mut: bool,
    /// The body is a `test` block, where a bare `return` means `return ok(())`.
    test_body: bool,
    /// How many `defer` statements are being checked (`return` is not allowed in them).
    pub(crate) defer_depth: u32,
    /// The variables the live closure captures (empty in a function body).
    pub captures: Vec<CaptureRec>,
    /// The id of the live frame: 0 for the function body, one per closure.
    pub frame_id: usize,
    pub(crate) next_frame_id: usize,
    /// The frames enclosing the live closure, outermost first.
    pub parents: Vec<Frame>,
    /// The inner frames swapped out while an enclosing frame is live (see `enter_parent`).
    children: Vec<Frame>,
    /// Closures whose bodies are checked, innermost first.
    pub done: Vec<DoneClosure>,
    /// How many errors the checker had when this body started.
    errors_at_start: usize,
    /// Bounds to decide once the body's types are known.
    pub obligations: Vec<Obligation>,
    /// Calls of generic functions, whose type arguments must all be known by the end.
    pub generic_calls: Vec<GenericCall>,
    /// How many `unsafe` blocks enclose the code being checked (a closure starts again at 0).
    pub(crate) unsafe_depth: u32,
    /// The target of a plain `x = value` is being resolved: naming `x` there is not a read.
    pub(crate) whole_assign_target: bool,
}

/// A call with type arguments that are inferred, kept so one that is never fixed is reported.
pub(crate) struct GenericCall {
    pub args: Vec<Ty>,
    pub names: Vec<String>,
    pub func: String,
    pub span: Span,
}

/// Whether `ty` is `Result[(), E]`, the type of a procedure that can fail (DESIGN §6.1).
/// Whether a shown type is a number type (or an untyped numeric literal).
fn numeric_name(t: &str) -> bool {
    matches!(t, "float" | "f32" | "{integer}" | "{float}") || IntTy::from_name(t).is_some()
}

/// The hint for a mismatch between two numeric types (DESIGN §5.1): only `f32` widens to
/// `float` implicitly, everything else converts by a call that names the target.
pub(crate) fn numeric_hint(expected: &str, found: &str) -> Option<String> {
    if !(numeric_name(expected) && numeric_name(found)) {
        return None;
    }
    Some(match (expected, found) {
        ("f32", "float") => {
            "`float` never narrows to `f32` implicitly; convert it with `f32(x)`".to_string()
        }
        (_, "{float}") => {
            "a float literal never becomes an integer; write an integer literal, or convert a float value with `int(x)`".to_string()
        }
        _ => format!(
            "numbers never convert implicitly (only `f32` widens to `float`); convert with `{expected}(x)`"
        ),
    })
}

fn is_unit_result(ty: &Ty) -> bool {
    matches!(ty, Ty::Adt(crate::types::RESULT_TYPE, args) if args.first() == Some(&Ty::Unit))
}

/// How deeply expressions may nest before the checker gives up, whatever the stack budget.
const MAX_DEPTH: u32 = 2_000;

impl<'c, 'a> FnCtx<'c, 'a> {
    pub fn new(ck: &'c mut Checker<'a>, name: String, ret: Option<(Ty, Option<Span>)>) -> Self {
        FnCtx {
            errors_at_start: ck.error_count(),
            ck,
            infer: InferCtx::new(),
            name,
            locals: Vec::new(),
            kinds: Vec::new(),
            flags: Vec::new(),
            scopes: Vec::new(),
            scope_floor: 0,
            loops: Vec::new(),
            next_loop: 0,
            broken_loops: Vec::new(),
            ret,
            deps: Vec::new(),
            depth: 0,
            self_mut: false,
            test_body: false,
            defer_depth: 0,
            captures: Vec::new(),
            frame_id: 0,
            next_frame_id: 1,
            parents: Vec::new(),
            children: Vec::new(),
            done: Vec::new(),
            obligations: Vec::new(),
            generic_calls: Vec::new(),
            unsafe_depth: 0,
            whole_assign_target: false,
        }
    }

    /// Marks the body as a `var self` method.
    pub fn mutating_self(mut self, yes: bool) -> Self {
        self.self_mut = yes;
        self
    }

    /// The module-level items this body refers to, each once with the span of its first use.
    pub fn take_deps(&mut self) -> Vec<(Dep, Span)> {
        let deps = self.deps_since(0);
        self.deps.clear();
        deps
    }

    /// Records a use of a module-level item at `span`.
    pub fn add_dep(&mut self, dep: Dep, span: Span) {
        self.deps.push((dep, span));
    }

    /// The items referred to since `mark` (a length of the list), each once with its first use.
    fn deps_since(&self, mark: usize) -> Vec<(Dep, Span)> {
        let mut seen = std::collections::HashSet::new();
        self.deps[mark..]
            .iter()
            .filter(|(d, _)| seen.insert(*d))
            .copied()
            .collect()
    }

    // ---- scopes and locals ----

    pub fn push_scope(&mut self) {
        self.scopes.push(Scope::default());
    }

    /// Closes the innermost scope and returns its locals (the block's drop list).
    pub fn pop_scope(&mut self) -> Vec<LocalId> {
        let locals = self.scopes.pop().map(|s| s.locals).unwrap_or_default();
        // Only the program's own packages are linted, not a dependency's code.
        let own = self.ck.mods[self.ck.cur_mod.0 as usize].root;
        for l in &locals {
            let f = self.flags[l.0 as usize];
            if own && f.lint_unused && !f.read {
                let info = &self.locals[l.0 as usize];
                let span = info.span;
                let name = info.name.clone();
                let diag = Diagnostic::warning(format!("unused variable `{name}`"), span)
                    .with_hint(format!(
                        "use it, or rename it `_{name}` to say it is unused on purpose"
                    ));
                self.ck.report_lint(diag);
            }
        }
        locals
    }

    /// Declares a binding in the innermost scope and returns its slot.
    pub fn declare(&mut self, name: &str, span: Span, ty: Ty, kind: LocalKind) -> LocalId {
        let id = LocalId(self.locals.len() as u32);
        self.locals.push(LocalInfo {
            name: name.to_string(),
            ty,
            mutable: kind == LocalKind::Var,
            storage: Storage::Slot,
            span,
        });
        self.kinds.push(kind);
        self.flags.push(LocalFlags::default());
        if let Some(scope) = self.scopes.last_mut() {
            let previous = if kind == LocalKind::Temp {
                None
            } else {
                scope.names.insert(name.to_string(), id)
            };
            scope.locals.push(id);
            if let Some(prev) = previous {
                let prev_info = &self.locals[prev.0 as usize];
                let prev_span = prev_info.span;
                let is_param = self.kinds[prev.0 as usize] == LocalKind::Param;
                if !is_param {
                    self.ck.report(
                        Diagnostic::warning(
                            format!("`{name}` shadows an earlier binding in the same block"),
                            span,
                        )
                        .with_label(prev_span, "the earlier binding")
                        .with_hint("use a different name, or assign to the `var` instead"),
                    );
                }
            }
        }
        id
    }

    /// A hidden local (used to fix evaluation order).
    pub fn declare_temp(&mut self, ty: Ty, span: Span) -> LocalId {
        self.declare("<temp>", span, ty, LocalKind::Temp)
    }

    /// The local `name` of the live frame, if it has one (no capture).
    fn lookup_own(&self, name: &str) -> Option<LocalId> {
        self.scopes[self.scope_floor.min(self.scopes.len())..]
            .iter()
            .rev()
            .find_map(|s| s.names.get(name).copied())
    }

    /// The local a name refers to. In a closure, a name found only in an enclosing frame is
    /// captured: the closure gets a local of its own for it (ARCHITECTURE §1), and so does every
    /// closure in between.
    pub fn lookup_local(&mut self, name: &str) -> Option<LocalId> {
        if let Some(id) = self.lookup_own(name) {
            return Some(id);
        }
        if self.parents.is_empty() {
            return None;
        }
        self.enter_parent();
        let outer = self.lookup_local(name);
        self.leave_parent();
        outer.map(|o| self.capture_local(o))
    }

    /// Swaps the live frame with the innermost enclosing one, so the enclosing frame can be
    /// searched or changed. [`FnCtx::leave_parent`] swaps back.
    pub(crate) fn enter_parent(&mut self) {
        if let Some(mut parent) = self.parents.pop() {
            self.swap_frame(&mut parent);
            self.children.push(parent);
        }
    }

    pub(crate) fn leave_parent(&mut self) {
        if let Some(mut child) = self.children.pop() {
            self.swap_frame(&mut child);
            self.parents.push(child);
        }
    }

    /// Exchanges the live frame with `other`.
    pub(crate) fn swap_frame(&mut self, other: &mut Frame) {
        std::mem::swap(&mut self.locals, &mut other.locals);
        std::mem::swap(&mut self.kinds, &mut other.kinds);
        std::mem::swap(&mut self.flags, &mut other.flags);
        std::mem::swap(&mut self.scopes, &mut other.scopes);
        std::mem::swap(&mut self.scope_floor, &mut other.scope_floor);
        std::mem::swap(&mut self.loops, &mut other.loops);
        std::mem::swap(&mut self.next_loop, &mut other.next_loop);
        std::mem::swap(&mut self.ret, &mut other.ret);
        std::mem::swap(&mut self.defer_depth, &mut other.defer_depth);
        std::mem::swap(&mut self.captures, &mut other.captures);
        std::mem::swap(&mut self.frame_id, &mut other.id);
    }

    /// Makes `name` resolve to `id` in the live frame's outermost scope.
    pub(crate) fn insert_name(&mut self, name: &str, id: LocalId) {
        if let Some(scope) = self.scopes.first_mut() {
            scope.names.insert(name.to_string(), id);
        }
    }

    /// Whether the name resolves to a local in this frame or an enclosing one, without
    /// capturing it.
    pub fn is_local(&self, name: &str) -> bool {
        self.lookup_own(name).is_some()
            || self.parents.iter().rev().any(|p| {
                p.scopes[p.scope_floor.min(p.scopes.len())..]
                    .iter()
                    .any(|s| s.names.contains_key(name))
            })
    }

    pub fn local_ty(&self, id: LocalId) -> Ty {
        self.locals[id.0 as usize].ty.clone()
    }

    /// Candidate names for "did you mean": visible locals and module-level names.
    pub fn suggestion(&self, name: &str) -> Option<String> {
        let locals = self.scopes[self.scope_floor.min(self.scopes.len())..]
            .iter()
            .flat_map(|s| s.names.keys().map(String::as_str));
        suggest::closest(name, locals.chain(self.ck.module_names())).map(str::to_string)
    }

    // ---- building and reporting ----

    pub fn mk(&self, kind: hir::ExprKind, ty: Ty, span: Span) -> Expr {
        Expr { kind, ty, span }
    }

    /// The placeholder left after an error; its type is [`Ty::Error`], so nothing cascades.
    pub fn error_expr(&self, span: Span) -> Expr {
        self.mk(hir::ExprKind::Error, Ty::Error, span)
    }

    /// Reports a construct the checker does not support and returns a placeholder.
    pub fn unsupported(&mut self, what: &str, span: Span) -> Expr {
        self.ck.unsupported(what, span);
        self.error_expr(span)
    }

    /// A type as shown in a message: unresolved literals read `{integer}` and `{float}`, also
    /// inside tuples.
    pub fn show(&self, ty: &Ty) -> String {
        self.infer.display_ty(ty).render(&self.ck.types)
    }

    pub fn report_mismatch(
        &mut self,
        span: Span,
        expected: &Ty,
        found: &Ty,
        reason: Option<(Span, String)>,
    ) {
        let (exp, got) = (self.show(expected), self.show(found));
        let mut diag = Diagnostic::error(format!("expected `{exp}`, found `{got}`"), span);
        if let Some((s, text)) = reason {
            diag = diag.with_label(s, text);
        }
        if let Some(hint) = numeric_hint(&exp, &got) {
            diag = diag.with_hint(hint);
        } else if exp == "Bytes" && got == "str" {
            diag = diag.with_hint("use `s.to_bytes()` to get the bytes of a string");
        } else if exp == "str" && got == "Bytes" {
            diag = diag.with_hint(
                "use `b.to_str()` (an error when the bytes are not UTF-8) or `b.to_str_lossy()`",
            );
        } else if exp == "()" {
            diag = diag.with_hint("this position has no value; if a value is wanted, declare a return type with `-> T`");
        } else if got == "()" {
            diag = diag.with_hint("`()` means no value: the expression ends in a statement, or a call to a function without a return type");
        }
        self.ck.report(diag);
    }

    /// Checks that `e` can be used as `expected`, reporting a mismatch otherwise.
    pub fn expect(&mut self, e: &Expr, expected: &Ty, reason: Option<(Span, String)>) -> bool {
        if self.infer.coerce(&e.ty, expected).is_ok() {
            return true;
        }
        self.report_mismatch(e.span, expected, &e.ty, reason);
        false
    }

    /// Ends literal polymorphism at a binding (DESIGN §3.4): an untyped literal type becomes
    /// `int` or `float`.
    pub fn default_ty(&mut self, ty: &Ty) -> Ty {
        if let Ty::Tuple(items) = self.infer.shallow(ty) {
            for item in &items {
                self.default_ty(item);
            }
        }
        match self.infer.unbound_kind(ty) {
            Some(VarKind::IntLit) => {
                let _ = self.infer.unify(ty, &Ty::INT);
            }
            Some(VarKind::FloatLit) => {
                let _ = self.infer.unify(ty, &Ty::Float);
            }
            _ => {}
        }
        self.infer.resolve(ty)
    }

    // ---- entry points ----

    /// Finishes a body: resolves every type and fixes literals and operators accordingly. The
    /// closures created in it are finished with it: they share its inference table, and
    /// whether a variable needs a cell depends on every closure that captures it.
    fn finish(mut self, block: Block, ret: Ty, span: Span, param_count: u32) -> Body {
        let mut block = block;
        let mut diags = Vec::new();
        self.settle();
        self.settle_storage();
        // Types left open by an earlier mistake in this body are not worth a second error.
        let had_errors = self.ck.error_count() > self.errors_at_start;
        zonk::zonk_body(
            &mut self.infer,
            &mut block,
            &mut self.locals,
            &mut diags,
            had_errors,
            &self.ck.host_typed,
            self.ck.facts.as_mut(),
        );
        let ret = self.infer.finalize(&ret).0;
        self.finish_closures(&mut diags, had_errors);
        for d in diags {
            self.ck.report(d);
        }
        Body {
            name: self.name,
            span,
            param_count,
            locals: self.locals,
            block,
            ret,
            mutates_self: self.self_mut,
        }
    }

    /// Decides what had to wait for the body's types: type arguments that were never fixed,
    /// then the bounds the body relies on.
    fn settle(&mut self) {
        let calls = std::mem::take(&mut self.generic_calls);
        for call in calls {
            for (arg, name) in call.args.iter().zip(&call.names) {
                let (_, unresolved) = self.infer.finalize(arg);
                if unresolved && self.ck.error_count() == self.errors_at_start {
                    let others: Vec<&str> = call.names.iter().map(String::as_str).collect();
                    let example = vec!["int"; others.len()].join(", ");
                    self.ck.report(
                        Diagnostic::error(
                            format!("cannot infer `{name}` in the call to `{}`", call.func),
                            call.span,
                        )
                        .with_hint(format!(
                            "write the type explicitly, like `{}[{example}](...)`, or annotate the result",
                            call.func
                        )),
                    );
                    break;
                }
            }
        }
        let obligations = std::mem::take(&mut self.obligations);
        let mut seen: Vec<(Ty, crate::types::TypeId, Span)> = Vec::new();
        for ob in obligations {
            let ty = self.infer.finalize(&ob.ty).0;
            let args: Vec<Ty> = ob.args.iter().map(|a| self.infer.finalize(a).0).collect();
            let key = (ty.clone(), ob.iface, ob.span);
            if seen.contains(&key) {
                continue;
            }
            seen.push(key);
            self.ck.check_obligation(&Obligation { ty, args, ..ob });
        }
    }

    pub fn check_fn_body(
        self,
        decl: &ast::FnDecl,
        params: &[ParamInfo<'a>],
        ret: &Ty,
        ret_span: Option<Span>,
        span: Span,
    ) -> (Body, Vec<(Dep, Span)>) {
        let src = match &decl.body {
            Some(ast::FnBody::Block(b)) => Some(BodySrc::Block(b)),
            Some(ast::FnBody::Expr(e)) => Some(BodySrc::Expr(e)),
            None => None,
        };
        self.check_body_src(src, params, ret, ret_span, span)
    }

    /// The body of a `test "name" { .. }` block: a function without parameters that returns
    /// `Result[()]` (ARCHITECTURE §15).
    pub fn check_test_body(
        mut self,
        block: &ast::Block,
        ret: &Ty,
        span: Span,
    ) -> (Body, Vec<(Dep, Span)>) {
        self.test_body = true;
        self.check_body_src(Some(BodySrc::Block(block)), &[], ret, None, span)
    }

    fn check_body_src(
        mut self,
        src: Option<BodySrc<'_>>,
        params: &[ParamInfo<'a>],
        ret: &Ty,
        ret_span: Option<Span>,
        span: Span,
    ) -> (Body, Vec<(Dep, Span)>) {
        self.push_scope();
        for p in params {
            let kind = if p.mutable {
                LocalKind::Var
            } else {
                LocalKind::Param
            };
            self.declare(&p.name, p.span, p.ty.clone(), kind);
        }
        let block = match src {
            Some(BodySrc::Block(b)) => {
                let (stmts, mut tail, mut ty) = self.block_in_scope(b, BlockMode::Stmts);
                let locals = self.pop_scope();
                let returns_value = *ret != Ty::Unit && !ret.is_error();
                let falls_off = returns_value && self.infer.shallow(&ty) != Ty::Never;
                if falls_off && is_unit_result(ret) {
                    // DESIGN §6.1: a `Result[()]` body that reaches its end returns `ok(())`.
                    let close = Span::new(b.span.file, b.span.end.saturating_sub(1), b.span.end);
                    tail = Some(Box::new(self.ok_unit(ret, close)));
                    ty = ret.clone();
                } else if falls_off {
                    let close = Span::new(b.span.file, b.span.end.saturating_sub(1), b.span.end);
                    let never = *ret == Ty::Never;
                    let mut diag = Diagnostic::error(
                        if never {
                            format!("`{}` is declared `-> !`, but its body can end", self.name)
                        } else {
                            format!(
                                "`{}` must return `{}`, but its body can end without returning",
                                self.name,
                                self.show(ret)
                            )
                        },
                        close,
                    )
                    .with_hint(if never {
                        "end every path with `panic(..)`, `os.exit(..)`, another `-> !` call or an endless loop"
                    } else {
                        "end every path with `return value`"
                    });
                    if let Some(rs) = ret_span {
                        diag = diag.with_label(rs, "the return type is declared here");
                    }
                    self.ck.report(diag);
                }
                Block {
                    locals,
                    stmts,
                    tail,
                    ty,
                    span: b.span,
                }
            }
            Some(BodySrc::Expr(e)) => {
                let value = self.check_expr(e, Some(ret));
                let reason = ret_span.map(|s| (s, "the return type is declared here".to_string()));
                self.expect(&value, ret, reason);
                let locals = self.pop_scope();
                Block {
                    locals,
                    stmts: Vec::new(),
                    ty: value.ty.clone(),
                    span: e.span,
                    tail: Some(Box::new(value)),
                }
            }
            None => {
                let locals = self.pop_scope();
                Block {
                    locals,
                    stmts: Vec::new(),
                    tail: None,
                    ty: Ty::Unit,
                    span,
                }
            }
        };
        let deps = self.take_deps();
        let body = self.finish(block, ret.clone(), span, params.len() as u32);
        (body, deps)
    }

    /// The top-level statements of a script, run in order.
    /// The top-level code of the entry file, in source order. A [`ScriptStep::Global`] (a
    /// script `let`/`var`, whose initialiser was checked already) becomes an
    /// [`hir::StmtKind::InitGlobal`] at its place. Also returns, for each step, the module-level
    /// items it refers to with their first use (an initialiser's are its own).
    pub fn check_script(
        mut self,
        steps: &[ScriptStep<'_>],
        span: Span,
    ) -> (Body, Vec<Vec<(Dep, Span)>>) {
        self.push_scope();
        let mut out = Vec::new();
        let mut step_deps = Vec::with_capacity(steps.len());
        for step in steps {
            match *step {
                ScriptStep::Stmt(s) => {
                    let mark = self.deps.len();
                    self.check_stmt(s, &mut out);
                    step_deps.push(self.deps_since(mark));
                }
                ScriptStep::Global(g) => {
                    let info = &self.ck.globals[g.0 as usize];
                    let init = info
                        .body
                        .clone()
                        .expect("a script value's initialiser is checked before the script");
                    step_deps.push(info.deps.clone());
                    out.push(hir::Stmt {
                        span: info.span,
                        kind: hir::StmtKind::InitGlobal { global: g, init },
                    });
                }
            }
        }
        let locals = self.pop_scope();
        let block = Block {
            locals,
            stmts: out,
            tail: None,
            ty: Ty::Unit,
            span,
        };
        (self.finish(block, Ty::Unit, span, 0), step_deps)
    }

    /// A module-level initialiser; returns the body and the value's type.
    pub fn check_initialiser(
        mut self,
        init: &ast::Expr,
        declared: Option<&Ty>,
        annotation: Option<Span>,
        span: Span,
    ) -> (Body, Ty, Vec<(Dep, Span)>) {
        self.push_scope();
        let value = self.check_expr(init, declared);
        let ty = match declared {
            Some(t) => {
                let reason =
                    annotation.map(|s| (s, "expected because of this type annotation".to_string()));
                self.expect(&value, t, reason);
                t.clone()
            }
            None => self.default_ty(&value.ty),
        };
        let ty = self.infer.finalize(&ty).0;
        let locals = self.pop_scope();
        let block = Block {
            locals,
            stmts: Vec::new(),
            ty: ty.clone(),
            span: init.span,
            tail: Some(Box::new(value)),
        };
        let deps = self.take_deps();
        let body = self.finish(block, ty.clone(), span, 0);
        (body, ty, deps)
    }

    /// Checks a default argument against its parameter type (once, to report its errors).
    pub fn check_default(mut self, default: &ast::Expr, ty: &Ty) -> Body {
        self.push_scope();
        let value = self.check_expr(default, Some(ty));
        self.expect(
            &value,
            ty,
            Some((default.span, "the default value".to_string())),
        );
        let locals = self.pop_scope();
        let span = default.span;
        let block = Block {
            locals,
            stmts: Vec::new(),
            ty: ty.clone(),
            span,
            tail: Some(Box::new(value)),
        };
        self.finish(block, ty.clone(), span, 0)
    }

    /// Re-checks a default argument at a call site, in the callee's module-level scope, with
    /// its diagnostics already reported once.
    pub fn check_default_at_call(
        &mut self,
        default: &ast::Expr,
        ty: &Ty,
        call: Span,
        module: ModId,
    ) -> Expr {
        let floor = std::mem::replace(&mut self.scope_floor, self.scopes.len());
        self.ck.mute();
        let back = self.ck.enter_module(module);
        let mark = self.deps.len();
        let mut value = self.check_expr(default, Some(ty));
        // What the default uses is used by this call: errors point at the call.
        for dep in &mut self.deps[mark..] {
            dep.1 = call;
        }
        let ok = self.infer.coerce(&value.ty, ty).is_ok();
        self.ck.enter_module(back);
        self.ck.unmute();
        self.scope_floor = floor;
        value.span = call;
        if ok { value } else { self.error_expr(call) }
    }

    // ---- blocks ----

    /// A block with its own scope.
    pub fn check_block(&mut self, b: &ast::Block, mode: BlockMode<'_>) -> Block {
        self.push_scope();
        let (stmts, tail, ty) = self.block_in_scope(b, mode);
        let locals = self.pop_scope();
        Block {
            locals,
            stmts,
            tail,
            ty,
            span: b.span,
        }
    }

    /// The statements and value of a block, in the already-open innermost scope.
    pub(crate) fn block_in_scope(
        &mut self,
        b: &ast::Block,
        mode: BlockMode<'_>,
    ) -> (Vec<Stmt>, Option<Box<Expr>>, Ty) {
        let mut stmts = Vec::new();
        let mut diverges = false;
        let mut warned = false;
        for s in &b.stmts {
            if diverges && !warned {
                self.warn_unreachable(s.span);
                warned = true;
            }
            diverges |= self.check_stmt(s, &mut stmts);
        }
        if diverges
            && !warned
            && let Some(tail) = &b.tail
        {
            self.warn_unreachable(tail.span);
        }
        match (&b.tail, mode) {
            (Some(tail), BlockMode::Value(expected)) => {
                let value = self.check_expr(tail, expected);
                let ty = value.ty.clone();
                (stmts, Some(Box::new(value)), ty)
            }
            (Some(tail), BlockMode::Stmts) => {
                let (stmt, never) = self.expr_stmt(tail);
                stmts.push(stmt);
                let ty = if diverges || never {
                    Ty::Never
                } else {
                    Ty::Unit
                };
                (stmts, None, ty)
            }
            (None, _) => {
                let ty = if diverges { Ty::Never } else { Ty::Unit };
                (stmts, None, ty)
            }
        }
    }

    /// The lint for code after `return`, `break`, `continue` or a call that never returns: one
    /// warning per block, at the first statement that cannot run. Dependencies are not linted.
    fn warn_unreachable(&mut self, span: Span) {
        if !self.ck.mods[self.ck.cur_mod.0 as usize].root {
            return;
        }
        let diag = Diagnostic::warning("unreachable code", span).with_hint(
            "nothing before this point lets the block continue; remove the code or the early exit",
        );
        self.ck.report_lint(diag);
    }

    // ---- statements ----

    /// Checks a statement into `out`; returns whether control cannot continue past it.
    pub(crate) fn check_stmt(&mut self, s: &ast::Stmt, out: &mut Vec<Stmt>) -> bool {
        match &s.kind {
            StmtKind::Let(l) => self.check_let(s, l, out),
            StmtKind::Assign { op, target, value } => {
                if let Some(stmt) = self.check_assign(s.span, *op, target, value) {
                    out.push(stmt);
                }
                false
            }
            StmtKind::Expr(e) => {
                let (stmt, never) = self.expr_stmt(e);
                out.push(stmt);
                never
            }
            StmtKind::Return(value) => {
                self.check_return(s.span, value.as_ref(), out);
                true
            }
            StmtKind::Break { label, value } => {
                if let Some(v) = value {
                    self.ck.report(
                        Diagnostic::error(
                            "`break` cannot carry a value out of a `while` loop",
                            v.span,
                        )
                        .with_hint("`while` loops do not produce a value"),
                    );
                }
                if let Some(id) = self.find_loop(label.as_ref(), "break", s.span) {
                    self.broken_loops.push(id.0);
                    out.push(Stmt {
                        span: s.span,
                        kind: hir::StmtKind::Break(id),
                    });
                }
                true
            }
            StmtKind::Continue { label } => {
                if let Some(id) = self.find_loop(label.as_ref(), "continue", s.span) {
                    out.push(Stmt {
                        span: s.span,
                        kind: hir::StmtKind::Continue(id),
                    });
                }
                true
            }
            StmtKind::While { label, cond, body } => {
                let id = self.next_loop;
                let value = self.check_while(s.span, label.as_ref(), cond, body);
                out.push(Stmt {
                    span: s.span,
                    kind: hir::StmtKind::Expr(value),
                });
                // `while true` that nothing `break`s out of never ends.
                matches!(cond.kind, ast::ExprKind::Bool(true)) && !self.broken_loops.contains(&id)
            }
            StmtKind::WhileLet {
                label,
                pat,
                scrutinee,
                body,
            } => {
                let value = self.check_while_let(s.span, label.as_ref(), pat, scrutinee, body);
                out.push(Stmt {
                    span: s.span,
                    kind: hir::StmtKind::Expr(value),
                });
                false
            }
            StmtKind::For {
                label,
                mutable,
                pat,
                iter,
                body,
            } => {
                let value = self.check_for(s.span, label.as_ref(), *mutable, pat, iter, body);
                out.push(Stmt {
                    span: s.span,
                    kind: hir::StmtKind::Expr(value),
                });
                false
            }
            StmtKind::Defer(inner) => {
                self.check_defer(s.span, inner, out);
                false
            }
            StmtKind::Error => false,
        }
    }

    /// `defer stmt`: the statement runs when the enclosing block exits (DESIGN §6.1). It is
    /// checked in place, so it sees the bindings declared before it.
    fn check_defer(&mut self, span: Span, inner: &ast::Stmt, out: &mut Vec<Stmt>) {
        if !matches!(inner.kind, StmtKind::Expr(_) | StmtKind::Assign { .. }) {
            self.ck.report(
                Diagnostic::error("`defer` needs a call or an assignment", inner.span)
                    .with_hint("write `defer cleanup()`; to run several statements, put them in a function or a block"),
            );
            return;
        }
        // A `break` or `continue` inside would jump out of a block that is already exiting.
        let loops = std::mem::take(&mut self.loops);
        self.defer_depth += 1;
        let mut checked = Vec::new();
        self.check_stmt(inner, &mut checked);
        self.defer_depth -= 1;
        self.loops = loops;
        for stmt in checked {
            out.push(Stmt {
                span,
                kind: hir::StmtKind::Defer(std::rc::Rc::new(stmt)),
            });
        }
    }

    /// An expression in statement position: its value is dropped, so an `if` there need not
    /// have matching branch types.
    pub(crate) fn expr_stmt(&mut self, e: &ast::Expr) -> (Stmt, bool) {
        let value = match &e.kind {
            ExprKind::If { cond, then, else_ } => {
                self.check_if(e.span, cond, then, else_.as_deref(), BlockMode::Stmts)
            }
            ExprKind::IfLet {
                pat,
                scrutinee,
                then,
                else_,
            } => self.check_if_let(
                e.span,
                pat,
                scrutinee,
                then,
                else_.as_deref(),
                BlockMode::Stmts,
            ),
            ExprKind::Match { scrutinee, arms } => {
                self.check_match(e.span, scrutinee, arms, BlockMode::Stmts)
            }
            ExprKind::Block(b) => {
                let block = self.check_block(b, BlockMode::Stmts);
                let ty = block.ty.clone();
                self.mk(hir::ExprKind::Block(Box::new(block)), ty, e.span)
            }
            _ => self.check_expr(e, None),
        };
        let never = self.infer.shallow(&value.ty) == Ty::Never;
        // A deferred call (`defer file.close()`) has nowhere to propagate a failure to, so it
        // is allowed to ignore the `Result`.
        if self.defer_depth == 0
            && matches!(
                self.infer.shallow(&value.ty),
                Ty::Adt(crate::types::RESULT_TYPE, _)
            )
        {
            self.ck.report(
                Diagnostic::warning("unused `Result`; handle it with `?`, `or`, `match`, or `let _ = ...`", e.span)
                    .with_hint("an ignored `Result` hides a failure: propagate it with `?`, give a fallback with `or`, or write `let _ = ...` to ignore it on purpose"),
            );
        }
        (
            Stmt {
                span: e.span,
                kind: hir::StmtKind::Expr(value),
            },
            never,
        )
    }

    /// Checks a `let`; whether its initialiser never finishes (`let x: int = die()`), so that
    /// control cannot continue past the statement.
    fn check_let(&mut self, s: &ast::Stmt, l: &ast::LetStmt, out: &mut Vec<Stmt>) -> bool {
        let annotated = l.ty.as_ref().map(|t| (self.ck.resolve_type(t), t.span));
        let value = self.check_expr(&l.init, annotated.as_ref().map(|(t, _)| t));
        let never = self.infer.shallow(&value.ty) == Ty::Never;
        let ty = match &annotated {
            Some((t, span)) => {
                let reason = Some((
                    *span,
                    "expected because of this type annotation".to_string(),
                ));
                self.expect(&value, t, reason);
                t.clone()
            }
            None => self.default_ty(&value.ty),
        };
        let kind = match l.kind {
            BindKind::Let => LocalKind::Let,
            BindKind::Var => LocalKind::Var,
            BindKind::Const => LocalKind::Const,
        };
        match &l.pat.kind {
            PatKind::Ident(name) => {
                let local = self.declare(&name.name, name.span, ty, kind);
                if kind != LocalKind::Const && !name.name.starts_with('_') {
                    self.flags[local.0 as usize].lint_unused = true;
                }
                out.push(Stmt {
                    span: s.span,
                    kind: hir::StmtKind::Let { local, init: value },
                });
            }
            PatKind::Wild => out.push(Stmt {
                span: s.span,
                kind: hir::StmtKind::Expr(value),
            }),
            PatKind::Error => {}
            _ => {
                let mut cx = PatCx::new(kind);
                let pat = self.check_pat(&l.pat, &ty, &mut cx);
                self.require_irrefutable(&pat, &ty, l.pat.span);
                out.push(Stmt {
                    span: s.span,
                    kind: hir::StmtKind::LetPat { pat, init: value },
                });
            }
        }
        never
    }

    /// A `let` pattern must match every value of its type.
    pub(crate) fn require_irrefutable(&mut self, pat: &hir::Pat, ty: &Ty, span: Span) {
        if ty.is_error() {
            return;
        }
        let ty = self.infer.resolve(ty);
        let arms = [ArmInfo {
            pat,
            guarded: false,
        }];
        let analysis = exhaust::analyze(&self.ck.types, &ty, &arms);
        if analysis.gave_up {
            self.report_gave_up(span, "`let` pattern");
        } else if !analysis.missing.is_empty() {
            let shown = analysis.missing.join(", ");
            self.ck.report(
                Diagnostic::error(
                    format!("this `let` pattern does not match every value; missing case: {shown}"),
                    span,
                )
                .with_hint("use `if let` or `match` to handle the other cases"),
            );
        }
    }

    fn check_return(&mut self, span: Span, value: Option<&ast::Expr>, out: &mut Vec<Stmt>) {
        if self.defer_depth > 0 {
            self.ck.report(
                Diagnostic::error("`return` cannot be used inside `defer`", span)
                    .with_hint("a deferred statement runs while the block is already exiting"),
            );
            return;
        }
        let Some((ret, ret_span)) = self.ret.clone() else {
            self.ck.report(
                Diagnostic::error("`return` outside of a function", span)
                    .with_hint("module-level code and initialisers cannot return"),
            );
            return;
        };
        let reason = ret_span.map(|s| (s, "the return type is declared here".to_string()));
        if self.infer.shallow(&ret) == Ty::Never {
            if let Some(v) = value {
                self.check_expr(v, None);
            }
            let mut diag = Diagnostic::error("`return` in a function that never returns", span)
                .with_hint("a `-> !` function ends in `panic`, `os.exit`, another `-> !` call or an endless loop");
            if let Some((s, text)) = reason {
                diag = diag.with_label(s, text);
            }
            self.ck.report(diag);
            return;
        }
        let value = match value {
            Some(v) => {
                let e = self.check_expr(v, Some(&ret));
                self.expect(&e, &ret, reason);
                Some(e)
            }
            // In a test, `return` alone ends it successfully: `return ok(())`.
            None if self.test_body && self.parents.is_empty() => Some(self.ok_unit(&ret, span)),
            None => {
                // A closure whose result is not known yet returns `()` here.
                if matches!(self.infer.shallow(&ret), Ty::Infer(_)) {
                    let _ = self.infer.unify(&ret, &Ty::Unit);
                }
                let ret = self.infer.shallow(&ret);
                if ret != Ty::Unit && !ret.is_error() {
                    let shown = self.show(&ret);
                    let mut diag = Diagnostic::error(
                        format!("`return` needs a value of type `{shown}`"),
                        span,
                    );
                    if let Some((s, text)) = reason {
                        diag = diag.with_label(s, text);
                    }
                    self.ck.report(diag);
                }
                None
            }
        };
        out.push(Stmt {
            span,
            kind: hir::StmtKind::Return(value),
        });
    }

    fn find_loop(&mut self, label: Option<&ast::Label>, what: &str, span: Span) -> Option<LoopId> {
        match label {
            None => match self.loops.last() {
                Some(l) => Some(l.id),
                None => {
                    self.ck.report(
                        Diagnostic::error(format!("`{what}` outside of a loop"), span)
                            .with_hint(format!("`{what}` only works inside `while`")),
                    );
                    None
                }
            },
            Some(label) => {
                let found = self
                    .loops
                    .iter()
                    .rev()
                    .find(|l| l.label.as_deref() == Some(label.name.as_str()))
                    .map(|l| l.id);
                if found.is_none() {
                    self.ck.report(Diagnostic::error(
                        format!("there is no enclosing loop labelled `'{}`", label.name),
                        label.span,
                    ));
                }
                found
            }
        }
    }

    fn check_while(
        &mut self,
        span: Span,
        label: Option<&ast::Label>,
        cond: &ast::Expr,
        body: &ast::Block,
    ) -> Expr {
        let id = LoopId(self.next_loop);
        self.next_loop += 1;
        let cond = self.check_cond(cond);
        self.loops.push(LoopFrame {
            id,
            label: label.map(|l| l.name.clone()),
        });
        let body = self.check_block(body, BlockMode::Stmts);
        self.loops.pop();
        // `while c { body }` is `loop { if c { body } else { break } }`.
        let exit = Block {
            locals: Vec::new(),
            stmts: vec![Stmt {
                span,
                kind: hir::StmtKind::Break(id),
            }],
            tail: None,
            ty: Ty::Never,
            span,
        };
        let test = self.mk(
            hir::ExprKind::If {
                cond: Box::new(cond),
                then: Box::new(body),
                else_: Some(Box::new(exit)),
            },
            Ty::Unit,
            span,
        );
        let wrapper = Block {
            locals: Vec::new(),
            stmts: vec![Stmt {
                span,
                kind: hir::StmtKind::Expr(test),
            }],
            tail: None,
            ty: Ty::Unit,
            span,
        };
        self.mk(
            hir::ExprKind::Loop {
                id,
                body: Box::new(wrapper),
            },
            Ty::Unit,
            span,
        )
    }

    /// An `if` or `while` condition: it must be a `bool`.
    pub fn check_cond(&mut self, cond: &ast::Expr) -> Expr {
        let e = self.check_expr(cond, Some(&Ty::Bool));
        if self.infer.coerce(&e.ty, &Ty::Bool).is_err() {
            let found = self.show(&e.ty);
            self.ck.report(
                Diagnostic::error(format!("expected `bool`, found `{found}`"), e.span).with_hint(
                    "a condition must be a `bool`; compare explicitly, for example `x != 0`",
                ),
            );
        }
        e
    }

    // ---- assignment ----

    fn check_assign(
        &mut self,
        span: Span,
        op: AssignOp,
        target: &ast::Expr,
        value: &ast::Expr,
    ) -> Option<Stmt> {
        self.whole_assign_target =
            op == AssignOp::Assign && matches!(target.kind, ExprKind::Ident(_));
        let place = self.check_place(target);
        self.whole_assign_target = false;
        let place = place?;
        let weak = self.is_weak_place(&place);
        let place_ty = place.ty.clone();
        // `p += v` on a user type is `p = p + v` through its operator impl.
        let user_iface = match op {
            AssignOp::Add => Some(crate::types::ADD),
            AssignOp::Sub => Some(crate::types::SUB),
            AssignOp::Mul => Some(crate::types::MUL),
            AssignOp::Div => Some(crate::types::DIV),
            _ => None,
        };
        if let Some(iface) = user_iface {
            let left = self.infer.shallow(&place_ty);
            if self.left_uses_operator(&left, iface) {
                return self.check_user_assign(span, op, place, target.span, iface, value);
            }
        }
        // A shift count may have any integer type; every other right-hand side has the type
        // of the place.
        let shift = matches!(op, AssignOp::Shl | AssignOp::Shr);
        let mut rhs = self.check_expr(value, if shift { None } else { Some(&place_ty) });
        if shift {
            self.default_ty(&rhs.ty);
        }
        let arith = match op {
            AssignOp::Add => Some(ArithOp::Add),
            AssignOp::Sub => Some(ArithOp::Sub),
            AssignOp::Mul => Some(ArithOp::Mul),
            AssignOp::Div => Some(ArithOp::Div),
            AssignOp::Rem => Some(ArithOp::Rem),
            _ => None,
        };
        let bit = match op {
            AssignOp::BitAnd => Some((BitOp::And, "&=")),
            AssignOp::BitOr => Some((BitOp::Or, "|=")),
            AssignOp::BitXor => Some((BitOp::Xor, "^=")),
            AssignOp::Shl => Some((BitOp::Shl, "<<=")),
            AssignOp::Shr => Some((BitOp::Shr, ">>=")),
            _ => None,
        };
        let reason = Some((target.span, format!("this is `{}`", self.show(&place_ty))));
        let left_unknown = matches!(self.infer.shallow(&place_ty), Ty::Infer(_));
        let stored = if weak && op == AssignOp::Assign {
            rhs = self.coerce_to_weak(rhs, &place_ty, reason.clone())?;
            true
        } else if shift {
            true
        } else {
            self.expect(&rhs, &place_ty, reason)
        };
        if stored && let Some(arith) = arith {
            let ok = match self.infer.shallow(&place_ty) {
                Ty::Error => true,
                Ty::Str | Ty::Bytes => arith == ArithOp::Add,
                t => self.is_numeric(&t),
            };
            if !ok && left_unknown {
                let sym = match op {
                    AssignOp::Add => "+=",
                    AssignOp::Sub => "-=",
                    AssignOp::Mul => "*=",
                    AssignOp::Div => "/=",
                    _ => "%=",
                };
                self.report_unknown_operand(sym, target.span, span);
            } else if !ok {
                let sym = match op {
                    AssignOp::Add => "+=",
                    AssignOp::Sub => "-=",
                    AssignOp::Mul => "*=",
                    AssignOp::Div => "/=",
                    _ => "%=",
                };
                let shown = self.show(&place_ty);
                self.ck.report(Diagnostic::error(
                    format!("`{sym}` cannot be applied to `{shown}`"),
                    span,
                ));
            }
        }
        if stored && let Some((_, sym)) = bit {
            let symbol = sym.trim_end_matches('=');
            self.require_integer_ty(symbol, &place_ty, target.span, span);
            if shift {
                self.require_integer_ty(symbol, &rhs.ty, rhs.span, span);
            }
        }
        let op = match (arith, bit) {
            (Some(_), _) if matches!(self.infer.shallow(&place_ty), Ty::Str | Ty::Bytes) => {
                Some(BinOp::Concat)
            }
            (Some(a), _) => Some(BinOp::Arith(a, Num::Int(IntTy::I64))),
            (None, Some((b, _))) => Some(BinOp::Bit(b, IntTy::I64)),
            (None, None) => None,
        };
        Some(Stmt {
            span,
            kind: hir::StmtKind::Assign { place, op, rhs },
        })
    }

    pub fn report_unknown_name(&mut self, name: &str, span: Span) {
        if let Some((alias, member)) = self.ck.split_qualified(name)
            && let Some(diag) = self.ck.unknown_member(alias, member, span)
        {
            self.ck.report(diag);
            return;
        }
        if self.ck.scoped_param(name).is_some() {
            self.ck.report(
                Diagnostic::error(format!("`{name}` is a type parameter, not a value"), span)
                    .with_hint("a type parameter has no static functions yet; pass a value of the type instead"),
            );
            return;
        }
        let mut diag = Diagnostic::error(format!("unknown name `{name}`"), span);
        if let Some(hint) = self.ck.excluded_hint(self.ck.cur_mod, name) {
            self.ck.report(diag.with_hint(hint));
            return;
        }
        if crate::checker::is_vec_name(name) {
            diag = diag.with_hint(format!(
                "the vector types are in the `vec` module: write `import vec` and then `vec.{name}(..)`, or `import vec.{{{name}}}`"
            ));
            self.ck.report(diag);
            return;
        }
        if name == "wait_frames" {
            diag = diag.with_hint(
                "there is no `wait_frames`; use `wait(seconds)`, or `wait_until(cond)` to wait for a condition",
            );
            self.ck.report(diag);
            return;
        }
        let owners: Vec<&str> = self
            .ck
            .types
            .iter()
            .filter(|t| t.variants().iter().any(|v| v.name == name))
            .map(|t| t.name.as_str())
            .collect();
        if let [owner] = owners.as_slice() {
            diag = diag.with_hint(format!(
                "`{name}` is a variant of `{owner}`; write `{owner}.{name}`, or `.{name}` where the type is known"
            ));
        } else if let Some(s) = self.suggestion(name) {
            diag = diag.with_hint(format!("did you mean `{s}`?"));
        }
        self.ck.report(diag);
    }

    // ---- shared helpers for calls ----

    /// The id of the function a name refers to, if any.
    pub fn lookup_fn(&self, name: &str) -> Option<FnId> {
        match self.ck.values.get(name) {
            Some(ValueRef::Fn(f)) => Some(*f),
            _ => None,
        }
    }

    pub fn enter(&mut self) -> bool {
        if self.depth >= MAX_DEPTH || self.ck.out_of_stack() {
            return false;
        }
        self.depth += 1;
        true
    }

    pub fn leave(&mut self) {
        self.depth -= 1;
    }

    pub fn fresh_var(&mut self, kind: VarKind) -> Ty {
        self.infer.fresh(kind)
    }

    /// Whether `ty` is (or may still become) an integer or float.
    pub fn is_numeric(&self, ty: &Ty) -> bool {
        match self.infer.shallow(ty) {
            Ty::Int(_) | Ty::Float | Ty::F32 => true,
            t => matches!(
                self.infer.unbound_kind(&t),
                Some(VarKind::IntLit | VarKind::FloatLit)
            ),
        }
    }
}
