//! Calls into the C test library (`colibri-ffi-testlib`) through `colibri-ffi` directly.

use std::rc::Rc;

use colibri_ffi::{CStruct, CType, Function, Libraries, Library, Val, read_memory, write_memory};

fn lib() -> Rc<Library> {
    // SAFETY: the test library's initialisers are trivial (it is built from `testlib.c`).
    unsafe { Libraries::new().get(Some(colibri_ffi_testlib::PATH)) }
        .expect("the test library loads")
}

/// Calls `f`. Safe here because every function of the test library is declared with the
/// signature `testlib.c` gives it, and the pointers the tests pass are valid.
fn invoke(f: &Function, args: &[Val]) -> Result<Val, String> {
    // SAFETY: see above.
    unsafe { f.call(args) }
}

/// A struct type with the offsets the C ABI gives it.
fn cstruct(name: &str, fields: Vec<CType>) -> CType {
    let mut offsets = Vec::new();
    let mut at = 0usize;
    let mut align = 1usize;
    for f in &fields {
        let a = f.align();
        at = at.div_ceil(a) * a;
        offsets.push(at);
        at += f.size();
        align = align.max(a);
    }
    CType::Struct(Rc::new(CStruct {
        name: name.to_string(),
        fields,
        offsets,
        size: at.div_ceil(align) * align,
        align,
    }))
}

fn func(name: &str, params: Vec<CType>, ret: CType) -> Function {
    Function::new(&lib(), name, params, ret).expect("function resolves")
}

fn point() -> CType {
    cstruct("Point", vec![CType::I32, CType::I32])
}

#[test]
fn scalars_of_every_width() {
    let add = func("ct_add_i32", vec![CType::I32; 2], CType::I32);
    assert_eq!(invoke(&add, &[Val::Int(40), Val::Int(2)]), Ok(Val::Int(42)));
    assert_eq!(
        invoke(&add, &[Val::Int(i32::MAX.into()), Val::Int(1)]),
        Ok(Val::Int(i32::MIN.into()))
    );
    let neg = func("ct_neg_i8", vec![CType::I8], CType::I8);
    assert_eq!(invoke(&neg, &[Val::Int(5)]), Ok(Val::Int(-5)));
    let inc = func("ct_inc_u8", vec![CType::U8], CType::U8);
    assert_eq!(invoke(&inc, &[Val::Int(255)]), Ok(Val::Int(0)));
    let not = func("ct_not_u16", vec![CType::U16], CType::U16);
    assert_eq!(invoke(&not, &[Val::Int(1)]), Ok(Val::Int(0xfffe)));
    let mul = func("ct_mul_u32", vec![CType::U32; 2], CType::U32);
    assert_eq!(
        invoke(&mul, &[Val::Int(65536), Val::Int(65537)]),
        Ok(Val::Int(65536))
    );
    let max = func("ct_u64_max", vec![], CType::U64);
    assert_eq!(invoke(&max, &[]), Ok(Val::UInt(u64::MAX)));
    let half = func("ct_u64_half", vec![CType::U64], CType::U64);
    assert_eq!(
        invoke(&half, &[Val::UInt(u64::MAX)]),
        Ok(Val::UInt(u64::MAX / 2))
    );
    let add64 = func("ct_add_i64", vec![CType::I64; 2], CType::I64);
    assert_eq!(
        invoke(&add64, &[Val::Int(1 << 40), Val::Int(-1)]),
        Ok(Val::Int((1 << 40) - 1))
    );
}

#[test]
fn floats_bools_and_void() {
    let hyp = func("ct_hyp", vec![CType::F64; 2], CType::F64);
    assert_eq!(
        invoke(&hyp, &[Val::F64(3.0), Val::F64(4.0)]),
        Ok(Val::F64(25.0))
    );
    let scale = func("ct_scale_f32", vec![CType::F32; 2], CType::F32);
    assert_eq!(
        invoke(&scale, &[Val::F32(1.5), Val::F32(2.0)]),
        Ok(Val::F32(3.0))
    );
    let even = func("ct_is_even", vec![CType::I32], CType::Bool);
    assert_eq!(invoke(&even, &[Val::Int(4)]), Ok(Val::Bool(true)));
    assert_eq!(invoke(&even, &[Val::Int(5)]), Ok(Val::Bool(false)));
    let and = func("ct_and", vec![CType::Bool; 2], CType::Bool);
    assert_eq!(
        invoke(&and, &[Val::Bool(true), Val::Bool(false)]),
        Ok(Val::Bool(false))
    );
    let nothing = func("ct_nothing", vec![], CType::Void);
    assert_eq!(invoke(&nothing, &[]), Ok(Val::Void));
}

#[test]
fn many_arguments_spill_to_the_stack() {
    let mut params = Vec::new();
    for i in 0..18 {
        params.push(if i % 2 == 0 { CType::I32 } else { CType::F64 });
    }
    let many = func("ct_many", params, CType::F64);
    let args: Vec<Val> = (1..=18)
        .map(|i| {
            if (i - 1) % 2 == 0 {
                Val::Int(i)
            } else {
                Val::F64(i as f64)
            }
        })
        .collect();
    assert_eq!(invoke(&many, &args), Ok(Val::F64(171.0)));
}

#[test]
fn strings_go_in_and_come_out() {
    let len = func("ct_strlen", vec![CType::CStr], CType::U64);
    assert_eq!(
        invoke(&len, &[Val::Str(Some("colibri".into()))]),
        Ok(Val::UInt(7))
    );
    assert_eq!(
        invoke(&len, &[Val::Str(Some("héllo".into()))]),
        Ok(Val::UInt(6))
    );
    let err = invoke(&len, &[Val::Str(Some("a\0b".into()))]).unwrap_err();
    assert!(err.contains("NUL"), "{err}");
    let hello = func("ct_greeting", vec![], CType::CStr);
    assert_eq!(
        invoke(&hello, &[]),
        Ok(Val::Str(Some("hello from C".into())))
    );
    let none = func("ct_null_string", vec![], CType::CStr);
    assert_eq!(invoke(&none, &[]), Ok(Val::Str(None)));
    let bad = func("ct_bad_utf8", vec![], CType::CStr);
    assert!(invoke(&bad, &[]).unwrap_err().contains("UTF-8"));
    let or = func("ct_or_default", vec![CType::CStr], CType::CStr);
    assert_eq!(
        invoke(&or, &[Val::Str(None)]),
        Ok(Val::Str(Some("(default)".into())))
    );
    let join = func("ct_join", vec![CType::CStr; 2], CType::CStr);
    assert_eq!(
        invoke(
            &join,
            &[Val::Str(Some("a".into())), Val::Str(Some("b".into()))]
        ),
        Ok(Val::Str(Some("a-b".into())))
    );
}

#[test]
fn pointers_and_memory() {
    let counter_new = func("ct_counter_new", vec![CType::I32], CType::Ptr);
    let inc = func("ct_counter_inc", vec![CType::Ptr], CType::Void);
    let get = func("ct_counter_get", vec![CType::Ptr], CType::I32);
    let free = func("ct_counter_free", vec![CType::Ptr], CType::Void);
    let Ok(Val::Ptr(c)) = invoke(&counter_new, &[Val::Int(10)]) else {
        panic!("a pointer");
    };
    assert_ne!(c, 0);
    invoke(&inc, &[Val::Ptr(c)]).unwrap();
    invoke(&inc, &[Val::Ptr(c)]).unwrap();
    assert_eq!(invoke(&get, &[Val::Ptr(c)]), Ok(Val::Int(12)));
    // The counter is one `int`: read and write it from outside.
    unsafe {
        assert_eq!(read_memory(&CType::I32, c), Ok(Val::Int(12)));
        write_memory(&CType::I32, &Val::Int(99), c).unwrap();
    }
    assert_eq!(invoke(&get, &[Val::Ptr(c)]), Ok(Val::Int(99)));
    invoke(&free, &[Val::Ptr(c)]).unwrap();

    let null = func("ct_null_ptr", vec![], CType::Ptr);
    assert_eq!(invoke(&null, &[]), Ok(Val::Ptr(0)));
    let set = func("ct_set_i32", vec![CType::Ptr, CType::I32], CType::Void);
    let mut slot = 0i32;
    invoke(
        &set,
        &[Val::Ptr(&mut slot as *mut i32 as usize), Val::Int(-7)],
    )
    .unwrap();
    assert_eq!(slot, -7);
    let divmod = func(
        "ct_divmod",
        vec![CType::I32, CType::I32, CType::Ptr, CType::Ptr],
        CType::Void,
    );
    let (mut q, mut r) = (0i32, 0i32);
    invoke(
        &divmod,
        &[
            Val::Int(17),
            Val::Int(5),
            Val::Ptr(&mut q as *mut i32 as usize),
            Val::Ptr(&mut r as *mut i32 as usize),
        ],
    )
    .unwrap();
    assert_eq!((q, r), (3, 2));
}

fn pt(x: i64, y: i64) -> Val {
    Val::Struct(vec![Val::Int(x), Val::Int(y)])
}

#[test]
fn struct_of_two_ints_in_and_out() {
    let add = func("ct_point_add", vec![point(), point()], point());
    assert_eq!(invoke(&add, &[pt(1, 2), pt(10, 20)]), Ok(pt(11, 22)));
    let dot = func("ct_point_dot", vec![point(), point()], CType::I32);
    assert_eq!(invoke(&dot, &[pt(1, 2), pt(3, 4)]), Ok(Val::Int(11)));
}

#[test]
fn structs_of_floats() {
    let v2d = cstruct("Vec2d", vec![CType::F64, CType::F64]);
    let scale = func("ct_vec2d_scale", vec![v2d.clone(), CType::F64], v2d.clone());
    assert_eq!(
        invoke(
            &scale,
            &[
                Val::Struct(vec![Val::F64(1.0), Val::F64(2.0)]),
                Val::F64(3.0)
            ]
        ),
        Ok(Val::Struct(vec![Val::F64(3.0), Val::F64(6.0)]))
    );
    let v2f = cstruct("Vec2f", vec![CType::F32, CType::F32]);
    let add = func("ct_vec2f_add", vec![v2f.clone(), v2f.clone()], v2f);
    let f2 = |a: f32, b: f32| Val::Struct(vec![Val::F32(a), Val::F32(b)]);
    assert_eq!(
        invoke(&add, &[f2(1.0, 2.0), f2(0.5, 0.25)]),
        Ok(f2(1.5, 2.25))
    );
    let v3f = cstruct("Vec3f", vec![CType::F32; 3]);
    let cross = func("ct_vec3f_cross", vec![v3f.clone(), v3f.clone()], v3f);
    let f3 = |a: f32, b: f32, c: f32| Val::Struct(vec![Val::F32(a), Val::F32(b), Val::F32(c)]);
    assert_eq!(
        invoke(&cross, &[f3(1.0, 0.0, 0.0), f3(0.0, 1.0, 0.0)]),
        Ok(f3(0.0, 0.0, 1.0))
    );
}

#[test]
fn structs_that_mix_integer_and_float_fields() {
    let mixed = cstruct("Mixed", vec![CType::I32, CType::F64]);
    let make = func("ct_mixed_make", vec![CType::I32, CType::F64], mixed.clone());
    let m = invoke(&make, &[Val::Int(7), Val::F64(0.5)]).unwrap();
    assert_eq!(m, Val::Struct(vec![Val::Int(7), Val::F64(0.5)]));
    let sum = func("ct_mixed_sum", vec![mixed], CType::F64);
    assert_eq!(invoke(&sum, &[m]), Ok(Val::F64(7.5)));

    let mixed2 = cstruct(
        "Mixed2",
        vec![CType::F64, CType::I32, CType::F32, CType::U8],
    );
    let make2 = func(
        "ct_mixed2_make",
        vec![CType::F64, CType::I32, CType::F32, CType::U8],
        mixed2.clone(),
    );
    let m2 = invoke(
        &make2,
        &[Val::F64(1.5), Val::Int(2), Val::F32(0.25), Val::Int(200)],
    )
    .unwrap();
    let sum2 = func("ct_mixed2_sum", vec![mixed2], CType::F64);
    assert_eq!(invoke(&sum2, &[m2]), Ok(Val::F64(203.75)));
}

#[test]
fn small_byte_structs_padding_and_ladders() {
    let color = cstruct("Color", vec![CType::U8; 4]);
    let invert = func("ct_color_invert", vec![color.clone()], color.clone());
    let c = |r, g, b, a| Val::Struct([r, g, b, a].map(Val::Int).to_vec());
    assert_eq!(invoke(&invert, &[c(0, 100, 255, 7)]), Ok(c(255, 155, 0, 7)));
    let pack = func("ct_color_pack", vec![color], CType::U32);
    assert_eq!(invoke(&pack, &[c(1, 2, 3, 4)]), Ok(Val::Int(0x0102_0304)));

    let padded = cstruct(
        "Padded",
        vec![CType::U8, CType::I64, CType::Bool, CType::U16],
    );
    let make = func(
        "ct_padded_make",
        vec![CType::U8, CType::I64, CType::Bool, CType::U16],
        padded.clone(),
    );
    let p = invoke(
        &make,
        &[Val::Int(3), Val::Int(-5), Val::Bool(true), Val::Int(40000)],
    )
    .unwrap();
    assert_eq!(
        p,
        Val::Struct(vec![
            Val::Int(3),
            Val::Int(-5),
            Val::Bool(true),
            Val::Int(40000)
        ])
    );
    let sum = func("ct_padded_sum", vec![padded], CType::I64);
    assert_eq!(invoke(&sum, &[p]), Ok(Val::Int(3 - 5 + 1000 + 40000)));

    let ladder = cstruct(
        "Ladder",
        vec![CType::I8, CType::I16, CType::I32, CType::I64, CType::U8],
    );
    let make = func("ct_ladder_make", vec![], ladder.clone());
    let l = invoke(&make, &[]).unwrap();
    assert_eq!(
        l,
        Val::Struct(vec![
            Val::Int(-1),
            Val::Int(-2),
            Val::Int(-3),
            Val::Int(-4),
            Val::Int(5)
        ])
    );
    let sum = func("ct_ladder_sum", vec![ladder], CType::I64);
    assert_eq!(invoke(&sum, &[l]), Ok(Val::Int(-5)));
}

#[test]
fn large_and_nested_structs_go_through_memory() {
    let big = cstruct("Big", vec![CType::I64; 4]);
    let make = func("ct_big_make", vec![CType::I64], big.clone());
    let b = invoke(&make, &[Val::Int(10)]).unwrap();
    assert_eq!(
        b,
        Val::Struct(vec![Val::Int(10), Val::Int(20), Val::Int(30), Val::Int(40)])
    );
    let add = func("ct_big_add", vec![big.clone(), big.clone()], big.clone());
    let sum = func("ct_big_sum", vec![big.clone()], CType::I64);
    let twice = invoke(&add, &[b.clone(), b]).unwrap();
    assert_eq!(invoke(&sum, &[twice]), Ok(Val::Int(200)));

    let rect = cstruct("Rect", vec![point(), point()]);
    let union = func(
        "ct_rect_union",
        vec![rect.clone(), rect.clone()],
        rect.clone(),
    );
    let r = |a, b, c, d| Val::Struct(vec![pt(a, b), pt(c, d)]);
    assert_eq!(
        invoke(&union, &[r(0, 0, 2, 2), r(1, -1, 5, 1)]),
        Ok(r(0, -1, 5, 2))
    );
    let area = func("ct_rect_area", vec![rect], CType::I32);
    assert_eq!(invoke(&area, &[r(1, 1, 4, 3)]), Ok(Val::Int(6)));
}

#[test]
fn a_struct_with_a_pointer_field_carries_the_address() {
    let named = cstruct("Named", vec![CType::Ptr, CType::I32]);
    let make = func("ct_named_make", vec![CType::Ptr], named.clone());
    let text = b"colibri\0";
    let n = invoke(&make, &[Val::Ptr(text.as_ptr() as usize)]).unwrap();
    assert_eq!(
        n,
        Val::Struct(vec![Val::Ptr(text.as_ptr() as usize), Val::Int(7)])
    );
    let len = func("ct_named_len", vec![named], CType::I32);
    assert_eq!(invoke(&len, &[n]), Ok(Val::Int(7)));
}

#[test]
fn a_cstr_field_is_refused() {
    let named = cstruct("Named", vec![CType::CStr, CType::I32]);
    let err = Function::new(&lib(), "ct_named_len", vec![named], CType::I32)
        .err()
        .unwrap();
    assert!(err.contains("`cstr` field"), "{err}");
}

#[test]
fn struct_through_a_pointer() {
    let scale = func("ct_point_scale", vec![CType::Ptr, CType::I32], CType::Void);
    let from = func("ct_point_from_ptr", vec![CType::Ptr], point());
    let mut p = [3i32, 4];
    let addr = p.as_mut_ptr() as usize;
    invoke(&scale, &[Val::Ptr(addr), Val::Int(5)]).unwrap();
    assert_eq!(p, [15, 20]);
    assert_eq!(invoke(&from, &[Val::Ptr(addr)]), Ok(pt(15, 20)));
    unsafe {
        assert_eq!(read_memory(&point(), addr), Ok(pt(15, 20)));
        write_memory(&point(), &pt(1, 2), addr).unwrap();
    }
    assert_eq!(p, [1, 2]);
}

#[test]
fn a_layout_that_disagrees_with_the_platform_is_refused() {
    let wrong = CType::Struct(Rc::new(CStruct {
        name: "Wrong".to_string(),
        fields: vec![CType::I32, CType::I32],
        offsets: vec![0, 8],
        size: 12,
        align: 4,
    }));
    let err = Function::new(
        &lib(),
        "ct_point_add",
        vec![wrong.clone(), wrong.clone()],
        wrong,
    )
    .err()
    .unwrap();
    assert!(err.contains("differs from the platform's"), "{err}");
}

#[test]
fn an_unknown_symbol_and_an_empty_struct_are_errors() {
    let err = Function::new(&lib(), "ct_no_such_function", vec![], CType::Void)
        .err()
        .unwrap();
    assert!(err.contains("no symbol `ct_no_such_function`"), "{err}");
    let empty = cstruct("Empty", vec![]);
    let err = Function::new(&lib(), "ct_nothing", vec![empty], CType::Void)
        .err()
        .unwrap();
    assert!(err.contains("cannot be passed by value"), "{err}");
}

#[test]
fn the_c_library_is_found_by_name() {
    let mut libs = Libraries::new();
    let libc = unsafe { libs.get(Some("c")) }.expect("libc");
    let strlen = Function::new(&libc, "strlen", vec![CType::CStr], CType::U64).unwrap();
    assert_eq!(
        invoke(&strlen, &[Val::Str(Some("abcd".into()))]),
        Ok(Val::UInt(4))
    );
    let libm = unsafe { libs.get(Some("m")) }.expect("libm");
    let cos = Function::new(&libm, "cos", vec![CType::F64], CType::F64).unwrap();
    assert_eq!(invoke(&cos, &[Val::F64(0.0)]), Ok(Val::F64(1.0)));
    let process = unsafe { libs.get(None) }.expect("the process");
    assert!(Function::new(&process, "strlen", vec![CType::CStr], CType::U64).is_ok());
    assert!(unsafe { libs.get(Some("definitely_not_a_library_xyz")) }.is_err());
}
