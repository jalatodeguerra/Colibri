//! What may cross to another thread (DESIGN §14.1), decided from types.
//!
//! A thread task is an isolate with its own object graph, so everything that reaches it is copied
//! (`colibri_core::portable`, which does this at run time and refuses what it cannot copy). This
//! module is the same decision made earlier, where the type says it: [`Checker::sendable`] walks a
//! type and names the first field path that cannot be sent, with the words of the run-time
//! refusal (`` `Job.conn` is a `net.Conn`, which cannot be sent to another thread ``).
//!
//! # What is decided here
//!
//! * **Refused:** host classes (`net.Conn`), iterators (`Iter[T]` values), `Coroutine`, `CoGroup`,
//!   `Signal` and `Task`.
//! * **Crosses:** numbers, `bool`, `char`, `str`, `Bytes`, raw pointers, and every collection,
//!   tuple, struct, enum and class whose parts cross. `Shared`, `Channel` and `Mutex` cross as
//!   handles without being looked into.
//! * **Left to the run time:** a type parameter (`T` in a generic function: there is no bound to
//!   ask for, and the body is checked once for every `T`), an interface value (`Dyn`) other than
//!   `Iter`, and a function value (its captures are not part of its type). The copy at run time
//!   refuses these too, with the same wording.
//! * **Weak fields** are not followed: a weak reference is copied only when its target is part of
//!   the graph anyway, and then the target is checked where it is held.
//!
//! The paths read like the run-time ones (ARCHITECTURE §36.1), except that a position in a
//! collection is `[_]` instead of the index, and a map entry is `[entry _].key` or
//! `[entry _].value`.
//!
//! # Where it applies
//!
//! The variables a `task.run` closure captures, the task's result type, the element type of a
//! `Channel`, and the value a `Shared` holds. The types are often not known when the call is
//! checked, so each is recorded as a [`SendCheck`] and decided with the other bounds at the end of
//! the body (`FnCtx::settle`).
//!
//! `ch.send(v)` has a second rule (Q2): a value that is more than plain data must be written as a
//! visible copy, see [`FnCtx::note_channel_send`].

use colibri_syntax::{Diagnostic, Span};

use crate::body::FnCtx;
use crate::checker::Checker;
use crate::hir::{self, TypeDefKind};
use crate::types::{
    CHANNEL_TYPE, COGROUP_TYPE, COROUTINE_TYPE, ERROR_TYPE, ITER, MUTEX_TYPE, OPTION_TYPE,
    SHARED_TYPE, SIGNAL_TYPE, TASK_TYPE, Ty, TypeId,
};

/// What cannot be sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// A host class, by the name of its type.
    Host(String),
    Iterator,
    Coroutine,
    CoGroup,
    Signal,
    Task,
}

/// A type that cannot be sent, and where the offender sits in it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NotSend {
    /// As the user would write it: `Job.conn`, `Rows.rows[_].runner`. Empty when the type itself
    /// is the offender.
    pub path: String,
    pub refusal: Refusal,
}

impl NotSend {
    /// ``is a `net.Conn`, which cannot be sent to another thread`` (the words after the subject).
    fn predicate(&self) -> String {
        let what = match &self.refusal {
            Refusal::Host(name) => format!("a `{name}`"),
            Refusal::Iterator => "an iterator".to_string(),
            Refusal::Coroutine => "a `Coroutine`".to_string(),
            Refusal::CoGroup => "a `CoGroup`".to_string(),
            Refusal::Signal => "a `Signal`".to_string(),
            Refusal::Task => "a `Task`".to_string(),
        };
        format!("is {what}, which cannot be sent to another thread")
    }
}

/// One step of the way down to a part of a type.
enum Seg {
    /// A field of a struct or class, named: `.conn`.
    Field(TypeId, String),
    /// A payload field of an enum variant: the type, the variant and the field.
    Payload(TypeId, String, String),
    /// An element of a tuple.
    Item(usize),
    /// An element of a list or set.
    Index,
    /// The key (`true`) or the value of an entry of a map.
    Entry(bool),
}

/// The most steps of a path that an error shows, as at run time.
const SHOWN_STEPS: usize = 12;

/// How big a type a walk follows into, counted in type constructors. A generic type whose
/// arguments grow at each level (`struct Nest[T] { inner: List[Nest[List[T]]] }`, or double:
/// `struct Chain[T] { next: Chain[(T, T)]? }`) never repeats a type, so only a size stops the walk.
/// What lies beyond it is left to the copy at run time, like a type parameter.
const MAX_TYPE_SIZE: usize = 256;

/// The number of constructors in `ty`, counting no further than `limit`.
fn type_size(ty: &Ty, limit: usize) -> usize {
    let mut n = 1;
    let add = |t: &Ty, n: &mut usize| {
        if *n <= limit {
            *n += type_size(t, limit - *n + 1);
        }
    };
    match ty {
        Ty::Tuple(ts) | Ty::Adt(_, ts) | Ty::Dyn(_, ts) => ts.iter().for_each(|t| add(t, &mut n)),
        Ty::List(t) | Ty::Set(t) | Ty::Option(t) | Ty::Ptr(t) | Ty::Range(t) => add(t, &mut n),
        Ty::Map(a, b) => {
            add(a, &mut n);
            add(b, &mut n);
        }
        Ty::Fn(ps, r) => {
            ps.iter().for_each(|t| add(t, &mut n));
            add(r, &mut n);
        }
        _ => {}
    }
    n
}

/// Why a type is looked at, which is where the message points and what it says.
#[derive(Clone, Debug)]
pub(crate) enum SendOrigin {
    /// A variable the closure of `task.run` captures: its name.
    Capture(String),
    /// What the task returns.
    Result,
    /// The element type of a `Channel`.
    ChannelElem,
    /// The value a `Shared` holds.
    SharedValue,
    /// A value passed to `send` whose type cannot be sent.
    SentValue,
}

/// A type that has to be sendable once the body's types are known.
#[derive(Clone, Debug)]
pub(crate) struct SendCheck {
    pub ty: Ty,
    pub origin: SendOrigin,
    pub span: Span,
}

/// A part of a sent value that is read from a variable: if it is more than plain data, the send
/// must write the copy.
#[derive(Clone, Debug)]
pub(crate) struct FreshSend {
    pub ty: Ty,
    pub span: Span,
    /// The variable the value is written as, for the hint.
    pub shown: String,
}

impl Checker<'_> {
    /// Whether a value of type `ty` can be sent to another thread; if not, why and where.
    ///
    /// `root` names the variable the value is held in (`closure.job` for a capture), which then
    /// starts the path; without it the path starts with the name of the first type that has a
    /// field on it.
    pub(crate) fn sendable(&self, ty: &Ty, root: Option<&str>) -> Result<(), NotSend> {
        let mut path = Vec::new();
        let mut done = Vec::new();
        self.send_walk(ty, root, &mut path, &mut done)
    }

    fn send_walk(
        &self,
        ty: &Ty,
        root: Option<&str>,
        path: &mut Vec<Seg>,
        done: &mut Vec<Ty>,
    ) -> Result<(), NotSend> {
        let refuse = |this: &Self, path: &[Seg], refusal| {
            Err(NotSend {
                path: this.render_send_path(root, path),
                refusal,
            })
        };
        match ty {
            Ty::Tuple(items) => {
                for (i, t) in items.iter().enumerate() {
                    path.push(Seg::Item(i));
                    self.send_walk(t, root, path, done)?;
                    path.pop();
                }
                Ok(())
            }
            // The `some` of an optional is not a step the program writes.
            Ty::Option(t) => self.send_walk(t, root, path, done),
            Ty::List(t) | Ty::Set(t) => {
                path.push(Seg::Index);
                let r = self.send_walk(t, root, path, done);
                path.pop();
                r
            }
            Ty::Map(k, v) => {
                path.push(Seg::Entry(true));
                let r = self.send_walk(k, root, path, done);
                path.pop();
                r?;
                path.push(Seg::Entry(false));
                let r = self.send_walk(v, root, path, done);
                path.pop();
                r
            }
            Ty::Dyn(id, _) if *id == ITER => refuse(self, path, Refusal::Iterator),
            Ty::Adt(id, args) => {
                match *id {
                    COROUTINE_TYPE => return refuse(self, path, Refusal::Coroutine),
                    COGROUP_TYPE => return refuse(self, path, Refusal::CoGroup),
                    SIGNAL_TYPE => return refuse(self, path, Refusal::Signal),
                    TASK_TYPE => return refuse(self, path, Refusal::Task),
                    // Handles that cross as they are.
                    CHANNEL_TYPE | SHARED_TYPE | MUTEX_TYPE | ERROR_TYPE => return Ok(()),
                    _ => {}
                }
                let Some(def) = self.types.get(id.0 as usize) else {
                    return Ok(());
                };
                if matches!(def.kind, TypeDefKind::HostClass) {
                    return refuse(self, path, Refusal::Host(def.display_name().into_owned()));
                }
                // A type that holds itself (a list of its own kind) is looked at once, and one
                // that keeps growing is looked at up to a size.
                if done.contains(ty) || type_size(ty, MAX_TYPE_SIZE) > MAX_TYPE_SIZE {
                    return Ok(());
                }
                done.push(ty.clone());
                let map: Vec<_> = def
                    .params
                    .iter()
                    .copied()
                    .zip(args.iter().cloned())
                    .collect();
                match &def.kind {
                    TypeDefKind::Struct { fields } | TypeDefKind::Class { fields } => {
                        for f in fields.iter().filter(|f| !f.weak) {
                            path.push(Seg::Field(*id, f.name.clone()));
                            self.send_walk(&f.ty.subst(&map), root, path, done)?;
                            path.pop();
                        }
                    }
                    TypeDefKind::Enum { variants } => {
                        for v in variants {
                            for f in v.fields.iter().filter(|f| !f.weak) {
                                if *id == OPTION_TYPE {
                                    self.send_walk(&f.ty.subst(&map), root, path, done)?;
                                    continue;
                                }
                                path.push(Seg::Payload(*id, v.name.clone(), f.name.clone()));
                                self.send_walk(&f.ty.subst(&map), root, path, done)?;
                                path.pop();
                            }
                        }
                    }
                    TypeDefKind::HostClass | TypeDefKind::Interface { .. } => {}
                }
                Ok(())
            }
            // Numbers, text, pointers and unit; a function value, an interface value other
            // than `Iter`, a type parameter and what is not known yet are the run time's.
            _ => Ok(()),
        }
    }

    /// The path as the run-time refusal writes it (`portable::render`), with `[_]` for a
    /// position. The type's name comes with the first step that names one.
    fn render_send_path(&self, root: Option<&str>, path: &[Seg]) -> String {
        let mut out = root.unwrap_or_default().to_string();
        let elided = path.len().saturating_sub(SHOWN_STEPS);
        let name = |id: &TypeId| {
            self.types
                .get(id.0 as usize)
                .map(|d| d.display_name().into_owned())
                .unwrap_or_default()
        };
        for (n, seg) in path.iter().enumerate() {
            if elided > 0 && n >= SHOWN_STEPS / 2 && n < SHOWN_STEPS / 2 + elided {
                if n == SHOWN_STEPS / 2 {
                    out.push_str(&format!("...({elided} levels)..."));
                }
                continue;
            }
            match seg {
                Seg::Field(id, field) => {
                    if out.is_empty() {
                        out.push_str(&name(id));
                    }
                    out.push('.');
                    out.push_str(field);
                }
                Seg::Payload(id, variant, field) => {
                    if out.is_empty() {
                        out.push_str(&name(id));
                    }
                    out.push('.');
                    out.push_str(variant);
                    out.push('.');
                    out.push_str(field);
                }
                Seg::Item(i) => out.push_str(&format!(".{i}")),
                Seg::Index => out.push_str("[_]"),
                Seg::Entry(key) => {
                    out.push_str(&format!("[entry _].{}", if *key { "key" } else { "value" }));
                }
            }
        }
        out
    }

    /// Whether a value of this type is more than plain data: something that can be changed
    /// through a second name (a list, a map, a set, an object, a closure, an interface value), or
    /// that holds one. Handles and plain values are not.
    pub(crate) fn holds_reference(&self, ty: &Ty) -> bool {
        self.holds_ref_walk(ty, &mut Vec::new())
    }

    fn holds_ref_walk(&self, ty: &Ty, done: &mut Vec<Ty>) -> bool {
        match ty {
            Ty::List(_) | Ty::Map(..) | Ty::Set(_) | Ty::Fn(..) | Ty::Dyn(..) => true,
            Ty::Tuple(items) => items.iter().any(|t| self.holds_ref_walk(t, done)),
            Ty::Option(t) | Ty::Range(t) => self.holds_ref_walk(t, done),
            Ty::Adt(id, args) => {
                if matches!(*id, CHANNEL_TYPE | SHARED_TYPE | MUTEX_TYPE | ERROR_TYPE) {
                    return false;
                }
                let Some(def) = self.types.get(id.0 as usize) else {
                    return false;
                };
                if done.contains(ty) || type_size(ty, MAX_TYPE_SIZE) > MAX_TYPE_SIZE {
                    return false;
                }
                done.push(ty.clone());
                let map: Vec<_> = def
                    .params
                    .iter()
                    .copied()
                    .zip(args.iter().cloned())
                    .collect();
                match &def.kind {
                    TypeDefKind::Class { .. } | TypeDefKind::HostClass => true,
                    TypeDefKind::Struct { fields } => fields
                        .iter()
                        .any(|f| self.holds_ref_walk(&f.ty.subst(&map), done)),
                    TypeDefKind::Enum { variants } => variants.iter().any(|v| {
                        v.fields
                            .iter()
                            .any(|f| self.holds_ref_walk(&f.ty.subst(&map), done))
                    }),
                    TypeDefKind::Interface { .. } => true,
                }
            }
            _ => false,
        }
    }
}

/// The parts of a sent value that are not visibly fresh: a variable, field or element read. A
/// call, a closure, a literal and the like make a new value; a variant, struct, tuple or
/// collection literal is only as fresh as its parts, since wrapping `report` in `some(..)` copies
/// it as invisibly as sending `report` does.
fn unfresh_parts(e: &hir::Expr, out: &mut Vec<(Ty, Span, String)>) {
    use hir::ExprKind as K;
    match &e.kind {
        K::Read(place) => out.push((e.ty.clone(), e.span, place.display.clone())),
        K::Coerce(inner, _) => unfresh_parts(inner, out),
        K::Struct { fields: parts, .. }
        | K::Object { fields: parts, .. }
        | K::Variant { args: parts, .. }
        | K::Tuple(parts)
        | K::ListLit(parts) => parts.iter().for_each(|p| unfresh_parts(p, out)),
        K::MapLit(entries) => entries.iter().for_each(|(k, v)| {
            unfresh_parts(k, out);
            unfresh_parts(v, out);
        }),
        _ => {}
    }
}

impl FnCtx<'_, '_> {
    /// Records that `ty` must be sendable, decided at the end of the body.
    pub(crate) fn require_sendable(&mut self, ty: &Ty, origin: SendOrigin, span: Span) {
        self.send_checks.push(SendCheck {
            ty: ty.clone(),
            origin,
            span,
        });
    }

    /// `ch.send(value)` (Q2): a part of the value that is more than plain data must be written as
    /// a visible copy. The types may not be known yet, so the rule is decided at the end of the
    /// body.
    pub(crate) fn note_channel_send(&mut self, value: &hir::Expr) {
        let mut parts = Vec::new();
        unfresh_parts(value, &mut parts);
        for (ty, span, shown) in parts {
            self.fresh_sends.push(FreshSend { ty, span, shown });
        }
    }

    /// A closure written where it crosses to another thread (`task.run(..)`, `ch.send(..)`,
    /// `Shared(..)`): what it captures is copied, and what it reaches runs on the worker. `mark`
    /// and `ob_mark` are where the argument's uses and bounds started. A function name counts for
    /// the module-state rule too. Returns whether `arg` was one of the two.
    pub(crate) fn note_thread_boundary(&mut self, arg: &hir::Expr, mark: usize, ob_mark: usize) {
        let closure = match &arg.kind {
            hir::ExprKind::Closure(c) => Some(c.f),
            hir::ExprKind::FnRef { .. } => None,
            _ => return,
        };
        if let Some(f) = closure
            && let Some(done) = self.done.iter().find(|d| d.f == f)
        {
            let captured: Vec<(String, Ty)> = done
                .frame
                .captures
                .iter()
                .filter_map(|c| done.frame.locals.get(c.inner.0 as usize))
                .map(|l| (l.name.clone(), l.ty.clone()))
                .collect();
            for (name, ty) in captured {
                self.require_sendable(&ty, SendOrigin::Capture(name), arg.span);
            }
        }
        self.note_task_site(closure.is_some(), mark, ob_mark, arg.span);
    }

    /// Keeps what the argument of a thread boundary refers to for the module-state rule.
    pub(crate) fn note_task_site(
        &mut self,
        closure: bool,
        mark: usize,
        ob_mark: usize,
        span: Span,
    ) {
        let deps = self.deps_since(mark);
        self.pending_sites.push((
            crate::checker::TaskSite {
                deps,
                closure,
                span,
            },
            ob_mark..self.obligations.len(),
        ));
    }

    /// Decides the sendability checks and the visible-copy rule recorded in this body.
    pub(crate) fn settle_sends(&mut self) {
        let checks = std::mem::take(&mut self.send_checks);
        for c in checks {
            let ty = self.infer.finalize(&c.ty).0;
            let root = match &c.origin {
                SendOrigin::Capture(name) => Some(format!("closure.{name}")),
                _ => None,
            };
            if let Err(bad) = self.ck.sendable(&ty, root.as_deref()) {
                self.report_not_sendable(&bad, &c.origin, c.span);
            }
        }
        let sends = std::mem::take(&mut self.fresh_sends);
        for s in sends {
            let ty = self.infer.finalize(&s.ty).0;
            if !self.ck.holds_reference(&ty) {
                continue;
            }
            // What cannot be sent at all gets that error: a copy would not help.
            if let Err(bad) = self.ck.sendable(&ty, None) {
                self.report_not_sendable(&bad, &SendOrigin::SentValue, s.span);
                continue;
            }
            let shown_ty = self.show(&ty);
            self.ck.report(
                Diagnostic::error(
                    format!("sending a `{shown_ty}` copies it, so the copy must be visible"),
                    s.span,
                )
                .with_hint(format!(
                    "write `ch.send({}.clone())`, or build the value in the call",
                    s.shown
                )),
            );
        }
    }

    fn report_not_sendable(&mut self, bad: &NotSend, origin: &SendOrigin, span: Span) {
        let (subject, why) = match origin {
            SendOrigin::Capture(_) => (
                format!("`{}`", bad.path),
                "a closure that crosses to another thread copies what it captures",
            ),
            SendOrigin::Result => (
                subject_of(bad, "the result of the task"),
                "`join` copies the task's result back to the thread that joins it",
            ),
            SendOrigin::ChannelElem => (
                subject_of(bad, "the element type of the channel"),
                "`send` copies each value to the thread that receives it",
            ),
            SendOrigin::SharedValue => (
                subject_of(bad, "the value of a `Shared`"),
                "`Shared` keeps a frozen copy that every thread can read",
            ),
            SendOrigin::SentValue => (
                subject_of(bad, "the value sent"),
                "`send` copies each value to the thread that receives it",
            ),
        };
        self.ck.report(
            Diagnostic::error(format!("{subject} {}", bad.predicate()), span).with_hint(format!(
                "{why}, and this part cannot be copied: pass plain data (numbers, strings, lists and structs of them), a `Shared`, or a `Channel`, and open handles inside the task"
            )),
        );
    }
}

/// The subject of a message when the offender is a part (`` `Job.conn` ``), or the whole thing.
fn subject_of(bad: &NotSend, whole: &str) -> String {
    if bad.path.is_empty() {
        whole.to_string()
    } else if bad.path.starts_with(['.', '[']) {
        // A path that starts with a step has no type name to say what it is a part of.
        format!("`{}` of {whole}", bad.path)
    } else {
        format!("`{}`", bad.path)
    }
}
