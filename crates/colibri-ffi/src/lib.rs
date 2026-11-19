//! C FFI for Colibri: describing C types, calling C functions through libffi and loading shared
//! libraries. This crate knows nothing about Colibri values or the checker; the runtime
//! converts between them and the [`CType`]/[`Val`] pair here (ARCHITECTURE "C FFI as built").
//!
//! Structs are passed by value in both directions: their field types, offsets, size and
//! alignment come from the caller (the checker computes them once, for the platform's C ABI),
//! and [`Function::new`] checks them against the layout libffi computes for itself, so a
//! disagreement is an error at load time and never a corrupted call.

mod library;

use std::ffi::{CStr, CString, c_void};
use std::rc::Rc;

use libffi::middle::{Arg, Cif, CodePtr, Ret, Type, ffi_abi_FFI_DEFAULT_ABI};

pub use library::{Libraries, Library};

/// A type that may cross the C boundary.
#[derive(Clone, Debug, PartialEq)]
pub enum CType {
    Void,
    Bool,
    I8,
    I16,
    I32,
    I64,
    U8,
    U16,
    U32,
    U64,
    F32,
    F64,
    /// `void *`: an address.
    Ptr,
    /// `const char *`: a NUL-terminated string. Passed as a temporary copy; a returned one is
    /// copied out.
    CStr,
    /// A struct by value.
    Struct(Rc<CStruct>),
}

/// A C struct: field types with the offsets, size and alignment of the platform ABI.
#[derive(Clone, Debug, PartialEq)]
pub struct CStruct {
    pub name: String,
    pub fields: Vec<CType>,
    pub offsets: Vec<usize>,
    pub size: usize,
    pub align: usize,
}

impl CType {
    pub fn size(&self) -> usize {
        match self {
            CType::Void => 0,
            CType::Bool | CType::I8 | CType::U8 => 1,
            CType::I16 | CType::U16 => 2,
            CType::I32 | CType::U32 | CType::F32 => 4,
            CType::I64 | CType::U64 | CType::F64 => 8,
            CType::Ptr | CType::CStr => std::mem::size_of::<usize>(),
            CType::Struct(s) => s.size,
        }
    }

    pub fn align(&self) -> usize {
        match self {
            CType::Void => 1,
            CType::Struct(s) => s.align,
            other => other.size(),
        }
    }

    fn is_int(&self) -> bool {
        matches!(
            self,
            CType::I8
                | CType::I16
                | CType::I32
                | CType::I64
                | CType::U8
                | CType::U16
                | CType::U32
                | CType::U64
        )
    }

    fn ffi(&self) -> Result<Type, String> {
        Ok(match self {
            CType::Void => Type::void(),
            CType::Bool | CType::U8 => Type::u8(),
            CType::I8 => Type::i8(),
            CType::I16 => Type::i16(),
            CType::U16 => Type::u16(),
            CType::I32 => Type::i32(),
            CType::U32 => Type::u32(),
            CType::I64 => Type::i64(),
            CType::U64 => Type::u64(),
            CType::F32 => Type::f32(),
            CType::F64 => Type::f64(),
            CType::Ptr | CType::CStr => Type::pointer(),
            CType::Struct(s) => {
                if s.fields.is_empty() || s.size == 0 {
                    return Err(format!(
                        "the struct `{}` has no fields, so it cannot be passed by value",
                        s.name
                    ));
                }
                if s.fields.contains(&CType::CStr) {
                    return Err(format!(
                        "the struct `{}` has a `cstr` field; a C string inside a struct would outlive the copy made for the call, so declare the field as `ptr[u8]`",
                        s.name
                    ));
                }
                let fields = s
                    .fields
                    .iter()
                    .map(CType::ffi)
                    .collect::<Result<Vec<_>, _>>()?;
                let mut t = Type::structure(fields);
                let offsets = t
                    .struct_offsets(ffi_abi_FFI_DEFAULT_ABI)
                    .map_err(|e| format!("libffi cannot lay out `{}`: {e:?}", s.name))?;
                // SAFETY: `struct_offsets` filled in the size and alignment of this type.
                let (size, align) = unsafe {
                    let raw = &*t.as_raw_ptr();
                    (raw.size, usize::from(raw.alignment))
                };
                if offsets != s.offsets || size != s.size || align != s.align {
                    return Err(format!(
                        "the layout of `{}` computed by Colibri (size {}, align {}, offsets {:?}) differs from the platform's (size {size}, align {align}, offsets {offsets:?})",
                        s.name, s.size, s.align, s.offsets
                    ));
                }
                t
            }
        })
    }
}

/// A value on its way into or out of C.
#[derive(Clone, Debug, PartialEq)]
pub enum Val {
    Void,
    Bool(bool),
    /// Any integer type except `u64`.
    Int(i64),
    UInt(u64),
    F32(f32),
    F64(f64),
    /// An address (`ptr[T]`).
    Ptr(usize),
    /// A C string: `None` is NULL.
    Str(Option<String>),
    /// A struct by value: one value per field.
    Struct(Vec<Val>),
}

/// Zeroed, 16-byte aligned storage: every C type is at most that aligned here.
struct Buf {
    words: Vec<u128>,
    len: usize,
}

impl Buf {
    fn new(len: usize) -> Buf {
        Buf {
            words: vec![0; len.div_ceil(16).max(1)],
            len,
        }
    }

    fn bytes(&self) -> &[u8] {
        // SAFETY: `words` owns at least `len` bytes.
        unsafe { std::slice::from_raw_parts(self.words.as_ptr().cast::<u8>(), self.len) }
    }

    fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: as in `bytes`, and `&mut self` is unique.
        unsafe { std::slice::from_raw_parts_mut(self.words.as_mut_ptr().cast::<u8>(), self.len) }
    }
}

fn put(out: &mut [u8], bytes: &[u8]) {
    out[..bytes.len()].copy_from_slice(bytes);
}

fn mismatch(ty: &CType, v: &Val) -> String {
    format!("internal error: the value {v:?} does not fit the C type {ty:?}")
}

fn as_i64(v: &Val) -> Option<i64> {
    match v {
        Val::Int(i) => Some(*i),
        Val::UInt(u) => Some(*u as i64),
        _ => None,
    }
}

/// Writes `v` as a `ty` into `out` (which is `ty.size()` bytes). A string's copy is kept alive
/// in `keep` until the call returns.
fn encode(ty: &CType, v: &Val, out: &mut [u8], keep: &mut Vec<CString>) -> Result<(), String> {
    match (ty, v) {
        (CType::Bool, Val::Bool(b)) => out[0] = u8::from(*b),
        (CType::F32, Val::F32(f)) => put(out, &f.to_ne_bytes()),
        (CType::F64, Val::F64(f)) => put(out, &f.to_ne_bytes()),
        (CType::Ptr, Val::Ptr(p)) => put(out, &p.to_ne_bytes()),
        (CType::CStr, Val::Str(None)) => put(out, &0usize.to_ne_bytes()),
        (CType::CStr, Val::Str(Some(s))) => {
            let c = CString::new(s.as_str()).map_err(|_| {
                "a string with a NUL character cannot be passed as a `cstr`".to_string()
            })?;
            put(out, &(c.as_ptr() as usize).to_ne_bytes());
            keep.push(c);
        }
        (t, v) if t.is_int() => {
            let Some(i) = as_i64(v) else {
                return Err(mismatch(ty, v));
            };
            match t {
                CType::I8 | CType::U8 => put(out, &(i as u8).to_ne_bytes()),
                CType::I16 | CType::U16 => put(out, &(i as u16).to_ne_bytes()),
                CType::I32 | CType::U32 => put(out, &(i as u32).to_ne_bytes()),
                _ => put(out, &(i as u64).to_ne_bytes()),
            }
        }
        (CType::Struct(s), Val::Struct(vals)) if vals.len() == s.fields.len() => {
            for ((fty, off), fv) in s.fields.iter().zip(&s.offsets).zip(vals) {
                encode(fty, fv, &mut out[*off..*off + fty.size()], keep)?;
            }
        }
        _ => return Err(mismatch(ty, v)),
    }
    Ok(())
}

fn read_word(bytes: &[u8]) -> u64 {
    let mut word = [0u8; 8];
    word[..bytes.len()].copy_from_slice(bytes);
    u64::from_ne_bytes(word)
}

/// Reads a `ty` from `bytes`, which holds it in memory layout (a struct field, or a value
/// read through a pointer). Strings are copied out.
fn decode(ty: &CType, bytes: &[u8]) -> Result<Val, String> {
    Ok(match ty {
        CType::Void => Val::Void,
        CType::Bool => Val::Bool(bytes[0] != 0),
        CType::I8 => Val::Int(i64::from(bytes[0] as i8)),
        CType::U8 => Val::Int(i64::from(bytes[0])),
        CType::I16 => Val::Int(i64::from(read_word(&bytes[..2]) as u16 as i16)),
        CType::U16 => Val::Int(i64::from(read_word(&bytes[..2]) as u16)),
        CType::I32 => Val::Int(i64::from(read_word(&bytes[..4]) as u32 as i32)),
        CType::U32 => Val::Int(i64::from(read_word(&bytes[..4]) as u32)),
        CType::I64 => Val::Int(read_word(&bytes[..8]) as i64),
        CType::U64 => Val::UInt(read_word(&bytes[..8])),
        CType::F32 => Val::F32(f32::from_bits(read_word(&bytes[..4]) as u32)),
        CType::F64 => Val::F64(f64::from_bits(read_word(&bytes[..8]))),
        CType::Ptr => Val::Ptr(read_word(&bytes[..8]) as usize),
        CType::CStr => {
            let addr = read_word(&bytes[..8]) as usize;
            if addr == 0 {
                Val::Str(None)
            } else {
                // SAFETY: the declaration promised a NUL-terminated string. A wrong promise
                // is the declaration's fault, as with any C binding.
                let c = unsafe { CStr::from_ptr(addr as *const std::ffi::c_char) };
                match c.to_str() {
                    Ok(s) => Val::Str(Some(s.to_string())),
                    Err(_) => return Err("the C string is not valid UTF-8".to_string()),
                }
            }
        }
        CType::Struct(s) => {
            let mut vals = Vec::with_capacity(s.fields.len());
            for (fty, off) in s.fields.iter().zip(&s.offsets) {
                vals.push(decode(fty, &bytes[*off..*off + fty.size()])?);
            }
            Val::Struct(vals)
        }
    })
}

/// The result of a call: libffi widens small integers to a full machine word.
fn decode_return(ty: &CType, buf: &Buf) -> Result<Val, String> {
    let bytes = buf.bytes();
    Ok(match ty {
        CType::Bool => Val::Bool(read_word(&bytes[..8]) as u8 != 0),
        t if t.is_int() => {
            let w = read_word(&bytes[..8]);
            match t {
                CType::I8 => Val::Int(i64::from(w as u8 as i8)),
                CType::U8 => Val::Int(i64::from(w as u8)),
                CType::I16 => Val::Int(i64::from(w as u16 as i16)),
                CType::U16 => Val::Int(i64::from(w as u16)),
                CType::I32 => Val::Int(i64::from(w as u32 as i32)),
                CType::U32 => Val::Int(i64::from(w as u32)),
                CType::I64 => Val::Int(w as i64),
                _ => Val::UInt(w),
            }
        }
        other => decode(other, bytes)?,
    })
}

/// A resolved C function with its call interface prepared.
pub struct Function {
    name: String,
    address: *const c_void,
    cif: Cif,
    params: Vec<CType>,
    ret: CType,
    // Keeps the library mapped for as long as `address` may be called.
    _library: Rc<Library>,
}

impl Function {
    /// Looks `symbol` up in `library` and prepares calls with this signature.
    pub fn new(
        library: &Rc<Library>,
        symbol: &str,
        params: Vec<CType>,
        ret: CType,
    ) -> Result<Function, String> {
        let address = library.symbol(symbol)?;
        let args = params
            .iter()
            .map(CType::ffi)
            .collect::<Result<Vec<_>, _>>()?;
        let cif = Cif::try_new(args, ret.ffi()?)
            .map_err(|e| format!("cannot prepare a call of `{symbol}`: {e:?}"))?;
        Ok(Function {
            name: symbol.to_string(),
            address,
            cif,
            params,
            ret,
            _library: Rc::clone(library),
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Calls the function. Strings in `args` are copied into temporary C strings for the
    /// duration of the call.
    pub fn call(&self, args: &[Val]) -> Result<Val, String> {
        if args.len() != self.params.len() {
            return Err(format!(
                "internal error: `{}` takes {} arguments, got {}",
                self.name,
                self.params.len(),
                args.len()
            ));
        }
        let mut keep: Vec<CString> = Vec::new();
        let mut bufs: Vec<Buf> = Vec::with_capacity(args.len());
        for (ty, v) in self.params.iter().zip(args) {
            let mut buf = Buf::new(ty.size());
            encode(ty, v, buf.bytes_mut(), &mut keep)?;
            bufs.push(buf);
        }
        let ffi_args: Vec<Arg> = bufs.iter_mut().map(|b| Arg::new(&*b.words)).collect();
        // Room for a machine word at least, and for a whole struct.
        let mut out = Buf::new(self.ret.size().max(16) + 16);
        // SAFETY: the cif was built from `params` and `ret`, the argument buffers hold values
        // of exactly those types, and the result buffer is large enough for the return type.
        // That the declared signature is the function's real one is the declaration's promise.
        unsafe {
            if self.ret == CType::Void {
                self.cif
                    .call_return_into(CodePtr(self.address.cast_mut()), &ffi_args, Ret::void());
            } else {
                let ret = Ret::new(&mut *out.words);
                self.cif
                    .call_return_into(CodePtr(self.address.cast_mut()), &ffi_args, ret);
            }
        }
        // A returned string may point into an argument's temporary copy (a function that
        // hands its argument back), so it is copied out before the copies are freed.
        let result = decode_return(&self.ret, &out);
        drop(ffi_args);
        drop(keep);
        result
    }
}

/// Reads a value of type `ty` from memory.
///
/// # Safety
/// `addr` must point to `ty.size()` readable bytes.
pub unsafe fn read_memory(ty: &CType, addr: usize) -> Result<Val, String> {
    // SAFETY: the caller promises the bytes are readable.
    let bytes = unsafe { std::slice::from_raw_parts(addr as *const u8, ty.size()) };
    decode(ty, bytes)
}

/// Writes a value of type `ty` to memory. A `cstr` value cannot be stored: it would point at a
/// temporary copy.
///
/// # Safety
/// `addr` must point to `ty.size()` writable bytes.
pub unsafe fn write_memory(ty: &CType, v: &Val, addr: usize) -> Result<(), String> {
    if contains_cstr(ty) {
        return Err("a `cstr` cannot be written through a pointer".to_string());
    }
    let mut buf = Buf::new(ty.size());
    let mut keep = Vec::new();
    encode(ty, v, buf.bytes_mut(), &mut keep)?;
    // SAFETY: the caller promises the bytes are writable.
    unsafe {
        std::ptr::copy_nonoverlapping(buf.bytes().as_ptr(), addr as *mut u8, ty.size());
    }
    Ok(())
}

fn contains_cstr(ty: &CType) -> bool {
    match ty {
        CType::CStr => true,
        CType::Struct(s) => s.fields.iter().any(contains_cstr),
        _ => false,
    }
}

/// Reads the NUL-terminated string at `addr`.
///
/// # Safety
/// `addr` must point to a NUL-terminated string.
pub unsafe fn read_cstring(addr: usize) -> Result<String, String> {
    // SAFETY: the caller promises a terminated string.
    let c = unsafe { CStr::from_ptr(addr as *const std::ffi::c_char) };
    c.to_str()
        .map(str::to_string)
        .map_err(|_| "the C string is not valid UTF-8".to_string())
}
