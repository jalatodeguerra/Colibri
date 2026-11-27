//! The machine's side of the C FFI (DESIGN §15.3, ARCHITECTURE "C FFI as built"): binding the
//! functions of `extern` blocks to their libraries, converting values to and from C, and the
//! methods of `ptr[T]`.
//!
//! The conversion code is the same with and without the `ffi` feature only up to the point
//! where C is called: without it, binding an `extern` function is an error that says so, and
//! no program can obtain a pointer to dereference.
//!
//! The conversions cannot live in `colibri-ffi` (it is published, and they name the unpublished
//! core).

use std::collections::BTreeMap;
use std::rc::Rc;

use colibri_check::hir::{CLayout, ExternDef, HostFnId, Program, PtrOp};
use colibri_check::types::TypeId;
use colibri_syntax::Span;

use colibri_core::ctl::Eval;
use colibri_core::prelude::*;
use colibri_core::value::Value;

use crate::fiber::VmFiber;
use crate::machine::Machine;

/// What the machine keeps for C: the layouts of the loaded program, and the libraries.
#[derive(Default)]
pub(crate) struct CffiState {
    layouts: Rc<BTreeMap<TypeId, CLayout>>,
    #[cfg(feature = "ffi")]
    libs: colibri_ffi::Libraries,
}

impl CffiState {
    pub(crate) fn load(&mut self, program: &Program) {
        self.layouts = Rc::new(program.c_layouts.clone());
    }
}

#[cfg(feature = "ffi")]
mod enabled {
    use colibri_check::hir::CTy;
    use colibri_check::types::IntTy;
    use colibri_ffi::{CStruct, CType, Function, Val};

    use super::*;
    use crate::host::{HostFault, HostImpl};

    /// The C type of `cty`, with struct layouts from the program.
    fn ctype(
        layouts: &BTreeMap<TypeId, CLayout>,
        names: &[colibri_check::hir::TypeDef],
        cty: CTy,
    ) -> CType {
        match cty {
            CTy::Void => CType::Void,
            CTy::Bool => CType::Bool,
            CTy::Int(i) => match i {
                IntTy::I8 => CType::I8,
                IntTy::I16 => CType::I16,
                IntTy::I32 => CType::I32,
                IntTy::I64 => CType::I64,
                IntTy::U8 => CType::U8,
                IntTy::U16 => CType::U16,
                IntTy::U32 => CType::U32,
                IntTy::U64 => CType::U64,
            },
            CTy::F32 => CType::F32,
            CTy::F64 => CType::F64,
            CTy::Ptr { .. } => CType::Ptr,
            CTy::CStr { .. } => CType::CStr,
            CTy::Struct(id) => {
                let Some(layout) = layouts.get(&id) else {
                    return CType::Void;
                };
                CType::Struct(Rc::new(CStruct {
                    name: names
                        .get(id.0 as usize)
                        .map_or_else(|| "struct".to_string(), |t| t.name.clone()),
                    fields: layout
                        .fields
                        .iter()
                        .map(|f| ctype(layouts, names, *f))
                        .collect(),
                    offsets: layout.offsets.clone(),
                    size: layout.size,
                    align: layout.align,
                }))
            }
        }
    }

    impl Machine {
        pub(crate) fn c_type(&self, cty: CTy) -> CType {
            ctype(&self.cffi.layouts, &self.core.types, cty)
        }

        /// A Colibri value as a C value of type `cty`.
        fn to_val(&self, cty: CTy, v: &Value) -> Result<Val, String> {
            let bad = || format!("internal error: a value does not fit the C type {cty:?}");
            Ok(match (cty, v) {
                (CTy::Void, _) => Val::Void,
                (CTy::Bool, Value::Bool(b)) => Val::Bool(*b),
                (CTy::Int(IntTy::U64), Value::U64(u)) => Val::UInt(*u),
                (CTy::Int(_), Value::Int(i)) => Val::Int(*i),
                (CTy::F32, Value::F32(f)) => Val::F32(*f),
                (CTy::F64, Value::Float(f)) => Val::F64(*f),
                (CTy::Ptr { nullable: false }, Value::Ptr(p)) => Val::Ptr(*p),
                (CTy::CStr { nullable: false }, Value::Str(s)) => Val::Str(Some(s.to_string())),
                (CTy::Ptr { nullable: true }, Value::Enum(e)) => match e.payload.first() {
                    Some(Value::Ptr(p)) => Val::Ptr(*p),
                    None => Val::Ptr(0),
                    _ => return Err(bad()),
                },
                (CTy::CStr { nullable: true }, Value::Enum(e)) => match e.payload.first() {
                    Some(Value::Str(s)) => Val::Str(Some(s.to_string())),
                    None => Val::Str(None),
                    _ => return Err(bad()),
                },
                (CTy::Struct(id), Value::Struct(s)) => {
                    let layout = self.cffi.layouts.get(&id).ok_or_else(bad)?;
                    let mut fields = Vec::with_capacity(s.fields.len());
                    for (f, v) in layout.fields.iter().zip(s.fields.iter()) {
                        fields.push(self.to_val(*f, v)?);
                    }
                    Val::Struct(fields)
                }
                _ => return Err(bad()),
            })
        }

        /// A C value of type `cty` as a Colibri value.
        fn value_from_c(&self, cty: CTy, v: Val) -> Result<Value, String> {
            let bad = || format!("internal error: a C value does not fit the type {cty:?}");
            Ok(match (cty, v) {
                (CTy::Void, _) => Value::Unit,
                (CTy::Bool, Val::Bool(b)) => Value::Bool(b),
                (CTy::Int(IntTy::U64), Val::UInt(u)) => Value::U64(u),
                (CTy::Int(_), Val::Int(i)) => Value::Int(i),
                (CTy::F32, Val::F32(f)) => Value::F32(f),
                (CTy::F64, Val::F64(f)) => Value::Float(f),
                (CTy::Ptr { nullable }, Val::Ptr(p)) => match (p, nullable) {
                    (0, false) => {
                        return Err("C produced a NULL pointer where a `ptr[T]` was declared; declare it `ptr[T]?` to accept NULL".to_string());
                    }
                    (0, true) => Value::option(None),
                    (p, false) => Value::Ptr(p),
                    (p, true) => Value::option(Some(Value::Ptr(p))),
                },
                (CTy::CStr { nullable }, Val::Str(s)) => match (s, nullable) {
                    (None, false) => {
                        return Err("C returned NULL for a `cstr`; declare the result `cstr?` to accept NULL".to_string());
                    }
                    (None, true) => Value::option(None),
                    (Some(s), false) => Value::Str(Rc::from(s)),
                    (Some(s), true) => Value::option(Some(Value::Str(Rc::from(s)))),
                },
                (CTy::Struct(id), Val::Struct(vals)) => {
                    let layout = self.cffi.layouts.get(&id).ok_or_else(bad)?;
                    let mut fields = Vec::with_capacity(vals.len());
                    for (f, v) in layout.fields.iter().zip(vals) {
                        fields.push(self.value_from_c(*f, v)?);
                    }
                    Value::strukt(id, fields)
                }
                _ => return Err(bad()),
            })
        }

        /// Binds `id` to the C function `def` describes: opens the library, finds the symbol
        /// and prepares the call. Errors are the message a load failure shows.
        pub fn bind_extern(
            &mut self,
            id: HostFnId,
            name: &str,
            def: &ExternDef,
        ) -> Result<(), String> {
            // SAFETY: opening a library runs its initialisers. A program can only declare an
            // `@link` library when the `ffi` capability is allowed, which is the embedder's
            // decision to trust the native code the program names.
            let lib = unsafe { self.cffi.libs.get(def.lib.as_deref()) }
                .map_err(|e| format!("`{name}`: {e}"))?;
            let params: Vec<CType> = def.params.iter().map(|p| self.c_type(*p)).collect();
            let ret = self.c_type(def.ret);
            let func = Function::new(&lib, &def.symbol, params, ret)
                .map_err(|e| format!("`{name}`: {e}"))?;
            let (ptys, rty) = (def.params.clone(), def.ret);
            let name = name.to_string();
            let imp: HostImpl = Rc::new(
                move |machine: &mut Machine, _fiber: &mut VmFiber, args: Vec<Value>| {
                    let fault = |e: String| {
                        HostFault::Panic(format!("calling the C function `{name}`: {e}"))
                    };
                    let mut vals = Vec::with_capacity(args.len());
                    for (ty, v) in ptys.iter().zip(&args) {
                        vals.push(machine.to_val(*ty, v).map_err(fault)?);
                    }
                    machine.release_all(args);
                    // What the program printed so far comes before anything C writes.
                    machine.core.flush();
                    // SAFETY: `func` was made from the signature the program declared, and
                    // `vals` were converted to exactly those types just above. That the
                    // declaration is the C function's real signature is the program's promise
                    // (as with any C binding), which the `ffi` capability stands behind; the
                    // pointers it passes are the program's responsibility in the same way.
                    let out = unsafe { func.call(&vals) }.map_err(fault)?;
                    machine.value_from_c(rty, out).map_err(fault)
                },
            );
            self.set_host_fn(id, imp);
            // A C function runs on the caller's stack: it never waits, and C code may need more
            // stack than a host function's small one.
            self.set_host_no_await(id);
            Ok(())
        }

        pub(crate) fn ptr_intrinsic(
            &mut self,
            fiber: &mut VmFiber,
            op: &PtrOp,
            args: Vec<Value>,
            span: Span,
        ) -> Eval {
            let Some(Value::Ptr(addr)) = args.first() else {
                return Err(self.internal(fiber, span, "a pointer method without a pointer"));
            };
            let addr = *addr;
            let result = match op {
                PtrOp::Addr => Ok(Value::U64(addr as u64)),
                PtrOp::Cast => Ok(Value::Ptr(addr)),
                PtrOp::Offset { size } => {
                    let Some(Value::Int(n)) = args.get(1) else {
                        return Err(self.internal(fiber, span, "`offset` without a count"));
                    };
                    let moved =
                        i128::try_from(addr).unwrap_or(i128::MAX) + i128::from(*n) * *size as i128;
                    match usize::try_from(moved) {
                        Ok(a) if a != 0 => Ok(Value::Ptr(a)),
                        _ => Err("the pointer arithmetic left the address space".to_string()),
                    }
                }
                PtrOp::ReadStr => {
                    // SAFETY: inside an `unsafe` block the program promises a C string here.
                    unsafe { colibri_ffi::read_cstring(addr) }.map(|s| Value::Str(Rc::from(s)))
                }
                PtrOp::Read(cty) => {
                    let ty = self.c_type(*cty);
                    // SAFETY: inside an `unsafe` block the program promises the address is
                    // valid for a value of this type.
                    unsafe { colibri_ffi::read_memory(&ty, addr) }
                        .and_then(|v| self.value_from_c(*cty, v))
                }
                PtrOp::Write(cty) => {
                    let Some(value) = args.get(1) else {
                        return Err(self.internal(fiber, span, "`write` without a value"));
                    };
                    let ty = self.c_type(*cty);
                    self.to_val(*cty, value).and_then(|v| {
                        // SAFETY: as for `Read`.
                        unsafe { colibri_ffi::write_memory(&ty, &v, addr) }.map(|()| Value::Unit)
                    })
                }
            };
            self.release_all(args);
            result.map_err(|message| self.panic(fiber, span, message))
        }
    }
}

#[cfg(not(feature = "ffi"))]
impl Machine {
    /// Binds `id` to a C function: not possible in a build without the `ffi` feature.
    pub fn bind_extern(
        &mut self,
        _id: HostFnId,
        name: &str,
        _def: &ExternDef,
    ) -> Result<(), String> {
        Err(format!(
            "`{name}` is declared in an `extern` block, but this build of Colibri has no C FFI (the `ffi` feature of `colibri-runtime` is off)"
        ))
    }

    pub(crate) fn ptr_intrinsic(
        &mut self,
        fiber: &mut VmFiber,
        _op: &PtrOp,
        args: Vec<Value>,
        span: Span,
    ) -> Eval {
        self.release_all(args);
        Err(self.internal(
            fiber,
            span,
            "a pointer in a build without the `ffi` feature",
        ))
    }
}
