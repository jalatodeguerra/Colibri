//! Patterns and `match` (vm-plan §2.7).
//!
//! Arms are tried in order. A pattern is compiled to tests and extractions only, in the order
//! the pattern is read: each test is followed by a `Jmp`
//! to where a failed test goes, and each part a sub-pattern looks at is cloned into a temporary
//! of its own. Nothing is bound while the pattern is being tried: the bindings are collected and
//! stored in their locals once the whole pattern held. So a pattern that fails half way has bound nothing, and what it cloned
//! on the way is dropped at the label its tests jump to.
//!
//! The scrutinee lives for the whole `match` (it is dropped at the end of it, with whatever its
//! evaluation left), the clones of its parts only until the pattern is done (a clone is never the
//! last reference, so dropping one can run no `Drop` hook), and the bindings until the arm ends.
//!
//! An or-pattern's alternatives bind into temporaries of their own, shared by all of them
//! (the registers are reused, and an alternative that fails after binding drops what it bound);
//! the locals are stored from those once the pattern as a whole has matched.

use colibri_check::hir::{Arm, Expr, Lit, LocalId, Pat, PatKind, Storage};
use colibri_check::types::Ty;
use colibri_core::pattern::bind_order;
use colibri_syntax::Span;

use super::expr::writes_locals;
use super::{FnCompiler, Item, Kind, Label, is_scalar, kind_of};
use crate::bytecode::{Op, Reg, Site};

/// A binding a pattern made, to be stored in its local.
struct Bound {
    local: LocalId,
    /// The register holding the value.
    src: Reg,
    /// `src` is an or-pattern's capture register, which the binding takes.
    cap: bool,
}

/// What compiling one pattern collected.
pub(super) struct PatCx {
    /// Where a failed test goes; `None` for an irrefutable pattern, which tests nothing.
    fail: Option<Label>,
    binds: Vec<Bound>,
    /// The capture register of each binding under an or-pattern.
    caps: Vec<(LocalId, Reg)>,
    in_or: bool,
    /// Whether bindings are recorded (only the first alternative of an or-pattern records).
    record: bool,
}

/// Whether a pattern can fail to match.
fn refutable(p: &Pat) -> bool {
    match &p.kind {
        PatKind::Wild => false,
        PatKind::Bind(_, inner) => inner.as_deref().is_some_and(refutable),
        PatKind::Lit(_) | PatKind::Range { .. } | PatKind::Const(_) | PatKind::Variant { .. } => {
            true
        }
        PatKind::Struct { fields: items, .. } | PatKind::Tuple(items) => {
            items.iter().any(refutable)
        }
        PatKind::Or(_) => true,
    }
}

impl FnCompiler<'_> {
    // ---- the tests ----

    /// Compiles the tests and extractions of `pat` against the value in `v`. The registers it
    /// clones parts into stay on the cleanup stack above where it began.
    pub(super) fn pat_tests(&mut self, pat: &Pat, v: Reg, fail: Option<Label>) -> PatCx {
        let mut cx = PatCx {
            fail,
            binds: Vec::new(),
            caps: Vec::new(),
            in_or: false,
            record: true,
        };
        self.pat(pat, v, &mut cx);
        cx
    }

    /// The tests of a pattern in a place that needs it to match (`let`, `for`). The checker proved
    /// it does, but an or-pattern or a variant is only exhaustive together with its siblings, so
    /// what is refutable is tested all the same, and a failure is an internal error. The clones
    /// are off the cleanup stack when this returns; they are the second result.
    pub(super) fn pat_tests_must_match(
        &mut self,
        pat: &Pat,
        v: Reg,
        span: Span,
    ) -> (PatCx, Vec<Item>) {
        let base = self.cleanup_depth();
        let fail = refutable(pat).then(|| self.new_label());
        let cx = self.pat_tests(pat, v, fail);
        let held: Vec<Item> = self.stack.items[base..].to_vec();
        self.truncate_stack(base);
        if let Some(fail) = fail {
            let ok = self.new_label();
            self.jmp(ok, span);
            self.bind(fail);
            self.emit_drop_items(&held, span);
            let site = self.add_site(Site::Unsupported(
                "an irrefutable pattern failed".to_string(),
            ));
            self.emit(Op::Unsupported { site }, span);
            self.bind(ok);
        }
        (cx, held)
    }

    fn test(&mut self, op: Op, span: Span, cx: &PatCx) {
        if let Some(fail) = cx.fail {
            self.emit(op, span);
            self.emit_to(Op::Jmp { off: 0 }, span, fail);
        }
    }

    fn pat(&mut self, p: &Pat, v: Reg, cx: &mut PatCx) {
        let span = p.span;
        match &p.kind {
            PatKind::Wild => {}
            PatKind::Bind(local, inner) => {
                if let Some(inner) = inner {
                    self.pat(inner, v, cx);
                }
                self.pat_bind(*local, v, cx, span);
            }
            PatKind::Lit(Lit::Int(k)) if i32::try_from(*k).is_ok() => {
                self.test(Op::TestInt { r: v, k: *k as i32 }, span, cx);
            }
            PatKind::Lit(lit) => {
                if cx.fail.is_some() {
                    let site = self.add_site(Site::Lit(lit.clone()));
                    self.test(Op::TestLit { r: v, site }, span, cx);
                }
            }
            PatKind::Range { lo, hi, inclusive } => {
                if cx.fail.is_some() {
                    let site = self.add_site(Site::Range {
                        lo: lo.clone(),
                        hi: hi.clone(),
                        inclusive: *inclusive,
                    });
                    self.test(Op::TestRange { r: v, site }, span, cx);
                }
            }
            PatKind::Const(g) => self.test(Op::TestConst { r: v, g: g.0 }, span, cx),
            PatKind::Variant { variant, args, .. } => {
                self.test(Op::TestVariant { r: v, v: *variant }, span, cx);
                self.pat_parts(args, &[], v, cx);
            }
            PatKind::Struct {
                fields: parts,
                weak,
                ..
            } => self.pat_parts(parts, weak, v, cx),
            PatKind::Tuple(parts) => self.pat_parts(parts, &[], v, cx),
            PatKind::Or(alts) => self.pat_or(alts, v, cx),
        }
    }

    /// The sub-patterns of a variant, struct or tuple: each part is cloned into a temporary.
    /// The parts at the indices in `weak` are weak references that the pattern sees as an
    /// `Option`: once cloned out, they are upgraded before the part is matched.
    fn pat_parts(&mut self, parts: &[Pat], weak: &[usize], v: Reg, cx: &mut PatCx) {
        for (i, part) in parts.iter().enumerate() {
            if matches!(part.kind, PatKind::Wild) {
                continue;
            }
            let Ok(i) = u16::try_from(i) else {
                self.fail("a pattern with too many parts");
                return;
            };
            let t = self.alloc_for(&part.ty);
            if weak.contains(&usize::from(i)) {
                // The weak reference goes into its own temporary, and the upgraded `Option` into
                // `t`: a register may not be overwritten by an instruction that would lose a
                // weak value, and the weak one has to be dropped by the cleanup stack.
                let w = self.alloc_for(&part.ty);
                self.emit(Op::Unpack { d: w, s: v, i }, part.span);
                self.own(w);
                self.emit(Op::FromWeak { d: t, s: w }, part.span);
            } else {
                self.emit(Op::Unpack { d: t, s: v, i }, part.span);
            }
            self.own(t);
            self.pat(part, t, cx);
        }
    }

    fn pat_bind(&mut self, local: LocalId, src: Reg, cx: &mut PatCx, span: Span) {
        if !cx.in_or {
            cx.binds.push(Bound {
                local,
                src,
                cap: false,
            });
            return;
        }
        let Some(&(_, cap)) = cx.caps.iter().find(|(l, _)| *l == local) else {
            self.fail("an or-pattern binds a name that not all its alternatives bind");
            return;
        };
        self.emit(Op::Copy { d: cap, s: src }, span);
        if cx.record {
            cx.binds.push(Bound {
                local,
                src: cap,
                cap: true,
            });
        }
    }

    /// `a | b | c`: each alternative is tried in turn; one that fails drops what it cloned and
    /// what it captured, then the next one starts.
    fn pat_or(&mut self, alts: &[Pat], v: Reg, cx: &mut PatCx) {
        let Some(outer_fail) = cx.fail else {
            // Nothing is tested, so the first alternative is the one that matches.
            if let Some(first) = alts.first() {
                self.pat(first, v, cx);
            }
            return;
        };
        let was_in_or = cx.in_or;
        let was_recording = cx.record;
        if !was_in_or {
            let mut locals = Vec::new();
            if let Some(first) = alts.first() {
                bind_order(first, &mut locals);
            }
            for l in locals {
                let t = self.alloc_for(self.local_ty(l));
                self.own(t);
                cx.caps.push((l, t));
            }
        }
        cx.in_or = true;
        let join = self.new_label();
        for (k, alt) in alts.iter().enumerate() {
            let last = k + 1 == alts.len();
            cx.record = was_recording && k == 0;
            let alt_fail = self.new_label();
            let (tmark, smark) = (self.temps.len(), self.cleanup_depth());
            cx.fail = Some(alt_fail);
            self.pat(alt, v, cx);
            cx.fail = Some(outer_fail);
            // What this alternative may be holding when it fails: its clones and its captures.
            let mut held: Vec<Item> = self.stack.items[smark..].to_vec();
            let mut bound = Vec::new();
            bind_order(alt, &mut bound);
            for l in bound {
                if let Some(&(_, cap)) = cx.caps.iter().find(|(c, _)| *c == l) {
                    match kind_of(self.local_ty(l)) {
                        Kind::Scalar => {}
                        Kind::Inert => held.push(Item::Plain(cap)),
                        Kind::Owned => held.push(Item::Reg(cap)),
                    }
                }
            }
            // It matched: its clones go, its captures stay for the bindings.
            let clones: Vec<Item> = self.stack.items[smark..].to_vec();
            self.emit_drop_items(&clones, alt.span);
            self.truncate_stack(smark);
            self.temps.truncate(tmark);
            self.jmp(join, alt.span);
            self.bind(alt_fail);
            self.emit_drop_items(&held, alt.span);
            if last {
                self.jmp(outer_fail, alt.span);
            }
        }
        self.bind(join);
        cx.in_or = was_in_or;
        cx.record = was_recording;
    }

    /// Drops the registers of `items` newest first, without touching the cleanup stack: for the
    /// code of a path that has already left the scope the items belong to.
    pub(super) fn emit_drop_items(&mut self, items: &[Item], span: Span) {
        let mut i = items.len();
        while i > 0 {
            i -= 1;
            match items[i] {
                Item::Plain(r) => {
                    self.emit(Op::DropInert { r }, span);
                }
                Item::Reg(r) => {
                    let mut lo = r;
                    while i > 0
                        && let Item::Reg(q) = items[i - 1]
                        && q + 1 == lo
                    {
                        lo = q;
                        i -= 1;
                    }
                    if lo == r {
                        self.emit(Op::Drop { r }, span);
                    } else {
                        self.emit(
                            Op::DropN {
                                r: lo,
                                n: r - lo + 1,
                            },
                            span,
                        );
                    }
                }
                _ => {}
            }
        }
    }

    // ---- the bindings ----

    /// Stores the bindings of a pattern that matched the value in `scrut` in their locals, and
    /// says what each of them needs dropped at the end of its scope (the items) and which of the
    /// pattern's registers were taken by a binding (the moved).
    pub(super) fn emit_binds(
        &mut self,
        cx: &PatCx,
        scrut: Reg,
        span: Span,
    ) -> (Vec<Item>, Vec<Reg>) {
        let mut items = Vec::new();
        let mut moved = Vec::new();
        for (i, b) in cx.binds.iter().enumerate() {
            let l = self.local(b.local);
            let ty = self.local_ty(b.local);
            let last_use = !cx.binds[i + 1..].iter().any(|o| o.src == b.src);
            let take = b.cap || (last_use && b.src != scrut && usize::from(b.src) >= self.nlocals);
            if self.storage(b.local) == Storage::Cell {
                if take {
                    self.emit(Op::NewCell { l, s: b.src }, span);
                    moved.push(b.src);
                } else {
                    let t = self.alloc_for(ty);
                    self.emit(Op::Copy { d: t, s: b.src }, span);
                    self.emit(Op::NewCell { l, s: t }, span);
                    self.free_moved(t);
                }
                // A cell holds a value whatever the type of the binding is.
                items.push(Item::Reg(l));
                continue;
            }
            if take {
                self.emit(Op::Move { d: l, s: b.src }, span);
                moved.push(b.src);
            } else {
                self.emit(Op::Copy { d: l, s: b.src }, span);
            }
            match kind_of(ty) {
                Kind::Scalar => {}
                Kind::Inert => items.push(Item::Plain(l)),
                Kind::Owned => items.push(Item::Reg(l)),
            }
        }
        (items, moved)
    }

    /// Drops the clones a pattern made that no binding took.
    pub(super) fn drop_unmoved(&mut self, held: &[Item], moved: &[Reg], span: Span) {
        let rest: Vec<Item> = held
            .iter()
            .copied()
            .filter(|i| !matches!(i, Item::Reg(r) | Item::Plain(r) if moved.contains(r)))
            .collect();
        self.emit_drop_items(&rest, span);
    }

    // ---- `match` ----

    /// `match`: the scrutinee is evaluated once and kept for the whole construct; the first arm
    /// whose pattern matches and whose guard holds runs. The value goes to `dst`.
    pub(super) fn match_expr(
        &mut self,
        scrutinee: &Expr,
        arms: &[Arm],
        dst: Option<Reg>,
        span: Span,
    ) {
        // Whatever the scrutinee leaves alive dies when the construct ends, not at the end of
        // the statement.
        let m = self.mark();
        // A guard may assign the local the scrutinee reads in place.
        let later = arms
            .iter()
            .any(|a| a.guard.as_ref().is_some_and(writes_locals));
        let v = self.operand(scrutinee, later);
        let end = self.new_label();
        let mut falls_through = true;
        for arm in arms {
            let next = self.new_label();
            falls_through = self.arm(arm, v, dst, next, end, span);
            self.bind(next);
        }
        if falls_through {
            // The checker proved the arms exhaustive.
            let site = self.add_site(Site::Unsupported("no `match` arm matched".to_string()));
            self.emit(Op::Unsupported { site }, span);
        }
        self.bind(end);
        self.emit_exit(m.cleanups, None, span);
        self.abandon(m);
    }

    /// One arm: its tests, its bindings, its guard and its body. A failed test or guard goes to
    /// `next`; the body ends at `end`. Says whether control can reach `next`.
    fn arm(
        &mut self,
        arm: &Arm,
        v: Reg,
        dst: Option<Reg>,
        next: Label,
        end: Label,
        span: Span,
    ) -> bool {
        let base = self.cleanup_depth();
        let tmark = self.temps.len();
        let fail = self.new_label();
        let cx = self.pat_tests(&arm.pat, v, Some(fail));
        // What a failed test leaves to drop: the clones the pattern made.
        let held: Vec<Item> = self.stack.items[base..].to_vec();
        self.truncate_stack(base);
        // The arm's scope: its bindings die when it ends, and hook panics fold there.
        self.push_fold();
        let (items, moved) = self.emit_binds(&cx, v, arm.pat.span);
        for item in items {
            self.push_item(item);
        }
        self.drop_unmoved(&held, &moved, arm.pat.span);
        self.temps.truncate(tmark);
        if let Some(guard) = &arm.guard {
            let body = self.new_label();
            self.jump_if(guard, true, body);
            self.emit_exit(base, None, guard.span);
            self.jmp(next, guard.span);
            self.bind(body);
        }
        self.block(&arm.body, dst);
        if arm.body.ty == Ty::Never {
            self.truncate_stack(base);
        } else {
            let keep = dst.filter(|_| !is_scalar(&arm.body.ty));
            self.end_scope(base, keep, arm.body.span);
            self.jmp(end, span);
        }
        // The tests failed: what they cloned is dropped, then the next arm is tried.
        self.bind(fail);
        self.emit_drop_items(&held, arm.pat.span);
        arm.guard.is_some() || refutable(&arm.pat)
    }
}
