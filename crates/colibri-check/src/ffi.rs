//! `@repr(C)` structs, `extern "C"` blocks, `ptr[T]` and `unsafe` (DESIGN §15.2, §15.3,
//! ARCHITECTURE "C FFI as built").
//!
//! The checker decides what may cross the C boundary and computes the C layout of every
//! `@repr(C)` struct once; the runtime turns that into libffi calls. An `extern` function is a
//! function with no body that the program, not the host, provides: it takes a `HostFnId`, so a
//! call is a `Callee::Host`, and its `HostFnDef` carries the [`ExternDef`].

use std::collections::BTreeSet;

use colibri_syntax::ast::{self, ExprKind, ItemKind, StrSeg, TypeKind};
use colibri_syntax::{Diagnostic, Span};

use crate::body::FnCtx;
use crate::call::{Bound, Receiver};
use crate::checker::Checker;
use crate::hir::{self, CLayout, CTy, Callee, ExternDef, Intrinsic, PtrOp};
use crate::types::{IntTy, Ty, TypeId};

/// The size and alignment of a type that is not a struct.
fn scalar_size_align(ty: CTy) -> Option<(usize, usize)> {
    let word = std::mem::size_of::<usize>();
    Some(match ty {
        CTy::Void => (0, 1),
        CTy::Bool => (1, 1),
        CTy::Int(i) => {
            let n = (i.bits() / 8) as usize;
            (n, n)
        }
        CTy::F32 => (4, 4),
        CTy::F64 => (8, 8),
        CTy::Ptr { .. } | CTy::CStr { .. } => (word, word),
        CTy::Vec(v) => (4 * v.n as usize, 4),
        CTy::Struct(_) => return None,
    })
}

/// What a C type may be, for messages.
const C_TYPES: &str = "`i8`..`i64`, `u8`..`u64`, `int`, `f32`, `f64`, `float`, `bool`, `vec2`..`ivec4`, `ptr[T]`, and other `@repr(C)` structs";

/// The text of a `@link("name")` or `@repr(C)` argument.
pub(crate) fn plain_string(e: &ast::Expr) -> Option<String> {
    match &e.kind {
        ExprKind::Str(segs) => {
            let mut text = String::new();
            for seg in segs {
                match seg {
                    StrSeg::Lit(s) => text.push_str(s),
                    StrSeg::Interp { .. } => return None,
                }
            }
            Some(text)
        }
        _ => None,
    }
}

impl<'a> Checker<'a> {
    /// Reads `@repr(C)` of a struct declaration (pass one, before any field is resolved, so
    /// `ptr[Handle]` can tell a `@repr(C)` struct from any other).
    pub(crate) fn note_repr_attrs(&mut self, id: TypeId, item: &'a ast::Item, is_class: bool) {
        for attr in item.attrs.iter().filter(|a| a.name.name == "repr") {
            let is_c = matches!(
                attr.args.as_slice(),
                [e] if matches!(&e.kind, ExprKind::Ident(i) if i.name == "C")
            );
            if !is_c {
                self.report(
                    Diagnostic::error(
                        "`@repr` takes `C`: the only layout is `@repr(C)`",
                        attr.span,
                    )
                    .with_hint("write `@repr(C)` before the struct"),
                );
                continue;
            }
            if is_class {
                self.report(
                    Diagnostic::error("`@repr(C)` applies to a `struct`, not a `class`", attr.span)
                        .with_hint("a class is a reference type with no C layout; use a struct"),
                );
                continue;
            }
            if !self.types[id.0 as usize].params.is_empty() {
                self.report(
                    Diagnostic::error("a `@repr(C)` struct cannot be generic", attr.span)
                        .with_hint("C has no generic types: write one struct for each type"),
                );
                continue;
            }
            self.repr_c.insert(id);
        }
    }

    /// Whether `ty` may be the pointee of a `ptr[T]`: any C type, `()` (a `void *`) or a
    /// `@repr(C)` struct (which may be empty, to name an opaque handle).
    pub(crate) fn ptr_elem_ok(&self, ty: &Ty) -> bool {
        matches!(ty, Ty::Unit | Ty::Error) || self.cty_of(ty).is_ok()
    }

    /// `ptr[T]`.
    pub(crate) fn resolve_ptr(&mut self, args: &[ast::TypeExpr], span: Span) -> Ty {
        let [arg] = args else {
            self.report(
                Diagnostic::error(
                    format!("`ptr` takes 1 type argument, found {}", args.len()),
                    span,
                )
                .with_hint("write it like `ptr[u8]`; `ptr[()]` is a `void *`"),
            );
            for a in args {
                self.resolve_type(a);
            }
            return Ty::Error;
        };
        let elem = self.resolve_type(arg);
        if elem.is_error() {
            return Ty::Error;
        }
        if !self.ptr_elem_ok(&elem) {
            let shown = self.render(&elem);
            self.report(
                Diagnostic::error(format!("`ptr[{shown}]` points to something C cannot see"), arg.span)
                    .with_hint(format!(
                        "a pointer's target is `()` or one of {C_TYPES}; `cstr` is a signature type, use `ptr[u8]` for bytes"
                    )),
            );
            return Ty::Error;
        }
        Ty::Ptr(Box::new(elem))
    }

    /// `cstr`: only in the signature of an `extern` function, where it stands for a `str`
    /// that C sees as a NUL-terminated `const char *`.
    pub(crate) fn resolve_cstr(&mut self, args: &[ast::TypeExpr], span: Span) -> Ty {
        for a in args {
            self.resolve_type(a);
        }
        if !self.in_extern {
            self.report(
                Diagnostic::error(
                    "`cstr` can only be used in the signature of an `extern` function",
                    span,
                )
                .with_hint("use `str` for text in your own code; `cstr` is how C sees it"),
            );
            return Ty::Error;
        }
        if !args.is_empty() {
            self.report(Diagnostic::error("`cstr` takes no type arguments", span));
            return Ty::Error;
        }
        Ty::Str
    }

    /// The C type of a Colibri type, or why it has none.
    pub(crate) fn cty_of(&self, ty: &Ty) -> Result<CTy, String> {
        Ok(match ty {
            Ty::Int(i) => CTy::Int(*i),
            Ty::Float => CTy::F64,
            Ty::F32 => CTy::F32,
            Ty::Vec(v) => CTy::Vec(*v),
            Ty::Bool => CTy::Bool,
            Ty::Ptr(_) => CTy::Ptr { nullable: false },
            Ty::Option(inner) if matches!(**inner, Ty::Ptr(_)) => CTy::Ptr { nullable: true },
            Ty::Adt(id, args) if args.is_empty() && self.repr_c.contains(id) => CTy::Struct(*id),
            Ty::Adt(id, _) => {
                let def = &self.types[id.0 as usize];
                if def.is_class() || def.is_host_class() {
                    return Err(format!("`{}` is a class, which has no C layout", def.name));
                }
                return Err(format!(
                    "`{}` is not a `@repr(C)` struct; mark it `@repr(C)` to use it with C",
                    def.name
                ));
            }
            Ty::Str => return Err("`str` is not a C type; in a signature write `cstr`".into()),
            Ty::Char => return Err("`char` is not a C type; use `u8` or `u32`".into()),
            Ty::Unit => return Err("`()` is not a value type in C".into()),
            other => {
                return Err(format!("`{}` is not a C type", other.render(&self.types)));
            }
        })
    }

    fn size_align(&self, ty: CTy) -> (usize, usize) {
        match ty {
            CTy::Struct(id) => self
                .c_layouts
                .get(&id)
                .map_or((0, 1), |l| (l.size, l.align)),
            other => scalar_size_align(other).unwrap_or((0, 1)),
        }
    }

    /// Size and alignment of a C type, for `size_of`.
    pub(crate) fn c_size_align(&self, ty: CTy) -> (usize, usize) {
        self.size_align(ty)
    }

    /// Computes the layout of every `@repr(C)` struct, reporting fields that are not C types.
    /// Runs once every type's fields are resolved.
    pub(crate) fn layout_repr_c(&mut self) {
        let ids: Vec<TypeId> = self
            .repr_c
            .iter()
            .copied()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        for id in ids {
            self.layout_one(id);
        }
    }

    /// Fields come before the struct that holds them, so a nested struct has its layout when
    /// the outer one is computed; a struct that contains itself was already refused as
    /// infinite.
    fn layout_one(&mut self, id: TypeId) -> bool {
        if self.c_layouts.contains_key(&id) {
            return true;
        }
        if !self.c_layout_visiting.insert(id) {
            return false;
        }
        let def = self.types[id.0 as usize].clone();
        let mut ok = true;
        let mut fields = Vec::new();
        for f in def.fields() {
            match self.cty_of(&f.ty) {
                Ok(cty) => {
                    if let CTy::Struct(inner) = cty
                        && !self.layout_one(inner)
                    {
                        ok = false;
                    }
                    fields.push(cty);
                }
                Err(why) => {
                    if !f.ty.is_error() {
                        self.report(
                            Diagnostic::error(
                                format!(
                                    "the field `{}` of the `@repr(C)` struct `{}` cannot be passed to C: {why}",
                                    f.name, def.name
                                ),
                                f.span,
                            )
                            .with_hint(format!("the fields of a `@repr(C)` struct are {C_TYPES}")),
                        );
                    }
                    ok = false;
                }
            }
        }
        self.c_layout_visiting.remove(&id);
        if !ok {
            return false;
        }
        let mut offsets = Vec::new();
        let mut at = 0usize;
        let mut align = 1usize;
        for f in &fields {
            let (size, a) = self.size_align(*f);
            at = at.div_ceil(a) * a;
            offsets.push(at);
            at += size;
            align = align.max(a);
        }
        let size = at.div_ceil(align) * align;
        self.c_layouts.insert(
            id,
            CLayout {
                fields,
                offsets,
                size,
                align,
            },
        );
        true
    }

    // ---- extern blocks ----

    /// An `extern "C" { .. }` block: each function becomes a host-style function the runtime
    /// binds to the C library.
    pub(crate) fn collect_extern(&mut self, item: &'a ast::Item, block: &'a ast::ExternBlock) {
        // A refused block still declares its functions (poisoned: calls are not checked), so a
        // refusal is one error and not one more for every use.
        let mut denied = false;
        if !self.host.is_granted("ffi") {
            let diag = Diagnostic::error(
                "`extern` blocks need the `ffi` capability, which this host did not grant",
                item.span,
            )
            .with_hint(
                "the host grants it with `VmBuilder::allow(Cap::Ffi)`; `colibri run` grants it",
            );
            self.report(diag);
            denied = true;
        } else if !self.mods[self.cur_mod.0 as usize].ffi {
            let module = &self.mods[self.cur_mod.0 as usize];
            let package = module
                .package
                .and_then(|p| self.package_names.get(p).cloned())
                .unwrap_or_default();
            let diag = Diagnostic::error(
                format!(
                    "the package `{package}` cannot contain `extern` blocks: it has not declared the `ffi` capability"
                ),
                item.span,
            )
            .with_hint("add `ffi = true` to the `[package]` table of its `colibri.toml`");
            self.report(diag);
            denied = true;
        }
        self.extern_denied = denied;
        if block.abi != "C" {
            self.report(
                Diagnostic::error(
                    format!("the ABI \"{}\" is not supported", block.abi),
                    item.span,
                )
                .with_hint("the only ABI is \"C\": `extern \"C\" { .. }`"),
            );
            return;
        }
        let mut lib: Option<String> = None;
        for attr in &item.attrs {
            if attr.name.name != "link" {
                self.unsupported(&format!("the attribute `@{}`", attr.name.name), attr.span);
                continue;
            }
            match attr.args.as_slice() {
                [e] if plain_string(e).is_some() => {
                    if lib.is_some() {
                        self.report(Diagnostic::error(
                            "an `extern` block has one `@link`",
                            attr.span,
                        ));
                    }
                    lib = plain_string(e);
                }
                _ => self.report(
                    Diagnostic::error("`@link` takes the library's name as a string", attr.span)
                        .with_hint("write `@link(\"m\")`: the name as for `-lm`, or a path"),
                ),
            }
        }
        for member in &block.items {
            let ItemKind::Fn(decl) = &member.kind else {
                continue;
            };
            self.collect_extern_fn(member, decl, lib.clone());
        }
        self.extern_denied = false;
    }

    fn collect_extern_fn(
        &mut self,
        item: &'a ast::Item,
        decl: &'a ast::FnDecl,
        lib: Option<String>,
    ) {
        for attr in &item.attrs {
            self.unsupported(&format!("the attribute `@{}`", attr.name.name), attr.span);
        }
        if decl.body.is_some() {
            self.report(
                Diagnostic::error(
                    format!(
                        "`{}` is declared in an `extern` block, so it has no body",
                        decl.name.name
                    ),
                    decl.name.span,
                )
                .with_hint("C provides the body; remove the `{ .. }`"),
            );
            return;
        }
        if !decl.generics.is_empty() {
            self.report(Diagnostic::error(
                "an `extern` function cannot be generic",
                decl.name.span,
            ));
            return;
        }
        if let Some(p) = decl.params.iter().find(|p| p.default.is_some()) {
            self.report(Diagnostic::error(
                "an `extern` function takes no default values",
                p.span,
            ));
            return;
        }
        self.extern_lib = Some(lib.clone());
        self.in_extern = true;
        let id = self.add_fn(item, decl, &crate::generics::SigCtx::default());
        self.in_extern = false;
        self.extern_lib = None;
        let mut params = Vec::new();
        let mut ok = true;
        let infos = self.fns[id.0 as usize].params.clone();
        for (info, written) in infos.iter().zip(&decl.params) {
            let Some(texpr) = &written.ty else {
                ok = false;
                continue;
            };
            match self.extern_cty(texpr, &info.ty, false) {
                Some(c) => params.push(c),
                None => ok = false,
            }
        }
        let ret_ty = self.fns[id.0 as usize].ret.clone();
        let ret = match &decl.ret {
            Some(texpr) => self.extern_cty(texpr, &ret_ty, true),
            None => Some(CTy::Void),
        };
        let (Some(ret), true) = (ret, ok) else {
            return;
        };
        let symbol = decl.name.name.clone();
        let ext = ExternDef {
            lib,
            symbol,
            params,
            ret,
        };
        if let Some(h) = &self.fns[id.0 as usize].host {
            self.host_ext.insert(h.id.0, ext);
        }
        self.declare_value(
            &decl.name.name,
            decl.name.span,
            crate::checker::ValueRef::Fn(id),
            item.public,
        );
    }

    /// The C type of one parameter or the return type of an `extern` function, from what was
    /// written (`cstr` is a `str` in Colibri) and what it resolved to.
    fn extern_cty(&mut self, texpr: &ast::TypeExpr, ty: &Ty, is_ret: bool) -> Option<CTy> {
        if ty.is_error() {
            return None;
        }
        let is_cstr = |t: &ast::TypeExpr| {
            matches!(&t.kind, TypeKind::Named { path, args }
                if args.is_empty() && path.len() == 1 && path[0].name == "cstr")
        };
        match &texpr.kind {
            _ if is_cstr(texpr) => return Some(CTy::CStr { nullable: false }),
            TypeKind::Optional(inner) if is_cstr(inner) => {
                return Some(CTy::CStr { nullable: true });
            }
            _ => {}
        }
        if *ty == Ty::Unit && is_ret {
            return Some(CTy::Void);
        }
        match self.cty_of(ty) {
            Ok(CTy::Struct(id)) => {
                if self.c_layouts.get(&id).is_some_and(|l| l.size == 0) {
                    let name = self.types[id.0 as usize].name.clone();
                    self.report(
                        Diagnostic::error(
                            format!("`{name}` has no fields, so it cannot be passed by value"),
                            texpr.span,
                        )
                        .with_hint(format!("pass a `ptr[{name}]` instead")),
                    );
                    return None;
                }
                Some(CTy::Struct(id))
            }
            Ok(c) => Some(c),
            Err(why) => {
                self.report(
                    Diagnostic::error(
                        format!("this type cannot cross the C boundary: {why}"),
                        texpr.span,
                    )
                    .with_hint(format!(
                        "an `extern` signature uses {C_TYPES}, `cstr` and `cstr?`"
                    )),
                );
                None
            }
        }
    }
}

// ---- unsafe and pointers ----

impl FnCtx<'_, '_> {
    /// `unsafe { .. }` is an ordinary block that allows the raw pointer operations.
    pub(crate) fn check_unsafe_block(
        &mut self,
        b: &ast::Block,
        expected: Option<&Ty>,
        span: Span,
    ) -> hir::Expr {
        self.unsafe_depth += 1;
        let block = self.check_block(b, crate::body::BlockMode::Value(expected));
        self.unsafe_depth -= 1;
        let ty = block.ty.clone();
        self.mk(hir::ExprKind::Block(Box::new(block)), ty, span)
    }

    fn require_unsafe(&mut self, what: &str, span: Span) -> bool {
        if self.unsafe_depth > 0 {
            return true;
        }
        self.ck.report(
            Diagnostic::error(format!("`{what}` needs an `unsafe` block"), span).with_hint(
                "a raw pointer can point anywhere; wrap the operation in `unsafe { .. }` once you have checked it is valid",
            ),
        );
        false
    }

    /// `p.read()`, `p.write(v)`, `p.offset(n)`, `p.cast[U]()`, `p.addr()` and `p.read_str()`
    /// on a `ptr[T]`.
    pub(crate) fn check_ptr_method(
        &mut self,
        recv: hir::Expr,
        elem: &Ty,
        name: &ast::Ident,
        explicit: Option<Vec<Ty>>,
        args: &[ast::Arg],
        span: Span,
    ) -> hir::Expr {
        let method = name.name.as_str();
        let elem = self.infer.resolve(elem);
        let shown_ptr = format!("ptr[{}]", self.show(&elem));
        let wants_args = usize::from(method == "write" || method == "offset");
        if !matches!(
            method,
            "read" | "write" | "offset" | "cast" | "addr" | "read_str"
        ) {
            self.check_args_loosely(args);
            self.ck.report(
                Diagnostic::error(format!("`{shown_ptr}` has no method `{method}`"), name.span)
                    .with_hint("its methods are: read, write, offset, cast, addr, read_str"),
            );
            return self.error_expr(span);
        }
        if args.len() != wants_args || args.iter().any(|a| a.name.is_some()) {
            self.check_args_loosely(args);
            self.ck.report(Diagnostic::error(
                format!(
                    "`{method}` takes {wants_args} argument(s), found {}",
                    args.len()
                ),
                span,
            ));
            return self.error_expr(span);
        }
        if method != "cast" && explicit.is_some() {
            self.check_args_loosely(args);
            self.ck.report(Diagnostic::error(
                format!("`{method}` takes no type arguments"),
                name.span,
            ));
            return self.error_expr(span);
        }
        if method != "addr" && !self.require_unsafe(&format!("{shown_ptr}.{method}"), name.span) {
            self.check_args_loosely(args);
            return self.error_expr(span);
        }
        let cty = self.ck.cty_of(&elem);
        let (op, ret, bound): (PtrOp, Ty, Vec<Bound>) = match method {
            "addr" => (PtrOp::Addr, Ty::Int(IntTy::U64), Vec::new()),
            "read_str" => {
                if !matches!(elem, Ty::Int(IntTy::U8 | IntTy::I8)) {
                    self.ck.report(
                        Diagnostic::error(
                            format!("`read_str` reads a C string, so it needs a `ptr[u8]`, not `{shown_ptr}`"),
                            name.span,
                        )
                        .with_hint("convert first: `p.cast[u8]().read_str()`"),
                    );
                    return self.error_expr(span);
                }
                (PtrOp::ReadStr, Ty::Str, Vec::new())
            }
            "cast" => {
                let [target] = explicit.as_deref().unwrap_or(&[]) else {
                    self.ck.report(
                        Diagnostic::error("`cast` takes the new pointee type", name.span)
                            .with_hint("write `p.cast[u8]()`"),
                    );
                    return self.error_expr(span);
                };
                let target = self.infer.resolve(target);
                if !self.ck.ptr_elem_ok(&target) {
                    let shown = self.show(&target);
                    self.ck.report(Diagnostic::error(
                        format!("`ptr[{shown}]` points to something C cannot see"),
                        name.span,
                    ));
                    return self.error_expr(span);
                }
                (PtrOp::Cast, Ty::Ptr(Box::new(target)), Vec::new())
            }
            _ => {
                // `read`, `write` and `offset` need to know how big the pointee is.
                let Ok(c) = cty else {
                    let reason = if elem == Ty::Unit {
                        "it points to `()`: cast it to the type you mean first, `p.cast[u8]()`"
                            .to_string()
                    } else {
                        cty.err().unwrap_or_default()
                    };
                    self.ck.report(Diagnostic::error(
                        format!("`{shown_ptr}.{method}` is not possible: {reason}"),
                        name.span,
                    ));
                    self.check_args_loosely(args);
                    return self.error_expr(span);
                };
                match method {
                    "read" => (PtrOp::Read(c), elem.clone(), Vec::new()),
                    "write" => {
                        let value = self.check_expr(&args[0].value, Some(&elem));
                        if !self.expect(&value, &elem, None) {
                            return self.error_expr_typed(span, Ty::Unit);
                        }
                        (
                            PtrOp::Write(c),
                            Ty::Unit,
                            vec![Bound {
                                expr: value,
                                source: Some(1),
                            }],
                        )
                    }
                    _ => {
                        let n = self.check_expr(&args[0].value, Some(&Ty::INT));
                        if !self.expect(&n, &Ty::INT, None) {
                            return self.error_expr_typed(span, Ty::Ptr(Box::new(elem)));
                        }
                        let (size, _) = self.ck.c_size_align(c);
                        (
                            PtrOp::Offset { size },
                            Ty::Ptr(Box::new(elem.clone())),
                            vec![Bound {
                                expr: n,
                                source: Some(1),
                            }],
                        )
                    }
                }
            }
        };
        self.finish_call(
            Callee::Intrinsic(Intrinsic::Ptr(op)),
            Some(Receiver::Value(recv)),
            bound,
            ret,
            Vec::new(),
            span,
        )
    }

    /// `size_of[T]()` and `align_of[T]()`: the C size and alignment of a type, as an `int`.
    pub(crate) fn check_size_of(
        &mut self,
        id: &ast::Ident,
        type_args: &[ast::IndexArg],
        args: &[ast::Arg],
        span: Span,
    ) -> hir::Expr {
        self.check_args_loosely(args);
        let which = id.name.as_str();
        let Some(explicit) = self.explicit_type_args(type_args) else {
            return self.error_expr(span);
        };
        let ([ty], true) = (explicit.as_slice(), args.is_empty()) else {
            self.ck.report(
                Diagnostic::error(format!("`{which}` takes one type and no arguments"), span)
                    .with_hint(format!("write `{which}[Point]()`")),
            );
            return self.error_expr(span);
        };
        let ty = self.infer.resolve(ty);
        let cty = match self.ck.cty_of(&ty) {
            Ok(c) => c,
            Err(why) => {
                self.ck.report(
                    Diagnostic::error(format!("`{which}` needs a C type: {why}"), id.span)
                        .with_hint(format!("it works on {C_TYPES}")),
                );
                return self.error_expr(span);
            }
        };
        let (size, align) = self.ck.c_size_align(cty);
        let n = if which == "size_of" { size } else { align };
        self.mk(hir::ExprKind::Lit(hir::Lit::Int(n as i64)), Ty::INT, span)
    }
}
