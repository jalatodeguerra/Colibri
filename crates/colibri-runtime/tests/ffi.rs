//! The C FFI (DESIGN §15.2, §15.3, ARCHITECTURE "C FFI as built"): calls into a C test library
//! compiled at build time (`colibri-ffi-testlib`), `@repr(C)` layouts against what the C
//! compiler says, and the capability checks.

#![cfg(feature = "ffi")]

mod common;
use common::vm_pending;

use colibri::{Cap, Vm};
use colibri_runtime as colibri;

const LIB: &str = colibri_ffi_testlib::PATH;

/// The text of a Colibri string literal's contents for `text`: a native path pasted into
/// Colibri source (`@link("...")`, `fs.write("...")`) has to be escaped, because `\` starts an
/// escape (`C:\Users` is "unknown escape `\U`") and `{` starts an interpolation. Every test
/// that puts a path into Colibri source, JSON or TOML text goes through such a helper; never `format!("\"{}\"", path.display())`.
fn string_contents(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('{', "\\{")
        .replace('}', "\\}")
}

fn vm() -> (Vm, colibri::SharedBuffer) {
    let out = colibri::SharedBuffer::new();
    let vm = Vm::builder()
        .stdout(Box::new(out.clone()))
        .allow(Cap::Ffi)
        .build();
    (vm, out)
}

/// Runs a script whose text has `LIB` replaced by the path of the test library, and returns
/// what it printed.
fn run(script: &str) -> String {
    let (mut vm, out) = vm();
    let source = script.replace("LIB", &string_contents(LIB));
    if let Err(e) = vm.load_source("t.colibri", &source) {
        panic!("{}", e.render(false));
    }
    if let Err(e) = vm.run_main() {
        panic!("{}", e.render(false));
    }
    out.contents()
}

/// The error a script gives, as text.
fn fails(script: &str) -> String {
    let (mut vm, _) = vm();
    let source = script.replace("LIB", &string_contents(LIB));
    match vm.load_source("t.colibri", &source) {
        Err(e) => e.render(false),
        Ok(()) => match vm.run_main() {
            Err(e) => e.render(false),
            Ok(()) => panic!("expected an error"),
        },
    }
}

// ---- scalars, strings and pointers ----

#[test]
fn scalars_of_every_width_cross_the_boundary() {
    vm_pending("scalars_of_every_width_cross_the_boundary", || {
        let out = run(r#"
@link("LIB")
extern "C" {
    fn ct_add_i32(a: i32, b: i32) -> i32
    fn ct_neg_i8(a: i8) -> i8
    fn ct_inc_u8(a: u8) -> u8
    fn ct_mul_i16(a: i16, b: i16) -> i16
    fn ct_not_u16(a: u16) -> u16
    fn ct_mul_u32(a: u32, b: u32) -> u32
    fn ct_add_i64(a: int, b: int) -> int
    fn ct_u64_max() -> u64
    fn ct_u64_half(a: u64) -> u64
    fn ct_hyp(a: float, b: float) -> float
    fn ct_scale_f32(x: f32, k: f32) -> f32
    fn ct_is_even(a: i32) -> bool
    fn ct_and(a: bool, b: bool) -> bool
    fn ct_nothing()
}
fn main() {
    print(ct_add_i32(40, 2))
    print(ct_neg_i8(5))
    print(ct_inc_u8(255))
    print(ct_mul_i16(-3, 7))
    print(ct_not_u16(1))
    print(ct_mul_u32(65536, 65537))
    print(ct_add_i64(1 << 40, -1))
    print(ct_u64_max())
    print(ct_u64_half(u64.MAX))
    print(ct_hyp(3.0, 4.0))
    print(ct_scale_f32(1.5, 2.0))
    print(ct_is_even(4))
    print(ct_and(true, false))
    ct_nothing()
}
"#);
        assert_eq!(
            out,
            "42\n-5\n0\n-21\n65534\n65536\n1099511627775\n18446744073709551615\n9223372036854775807\n25.0\n3.0\ntrue\nfalse\n"
        );
    })
}

#[test]
fn many_arguments_spill_to_the_stack() {
    vm_pending("many_arguments_spill_to_the_stack", || {
        let out = run(r#"
@link("LIB")
extern "C" {
    fn ct_many(a: i32, b: float, c: i32, d: float, e: i32, f: float, g: i32, h: float,
               i: i32, j: float, k: i32, l: float, m: i32, n: float, o: i32, p: float,
               q: i32, r: float) -> float
}
fn main() {
    print(ct_many(1, 2.0, 3, 4.0, 5, 6.0, 7, 8.0, 9, 10.0, 11, 12.0, 13, 14.0, 15, 16.0, 17, 18.0))
}
"#);
        assert_eq!(out, "171.0\n");
    })
}

#[test]
fn strings_in_and_out() {
    vm_pending("strings_in_and_out", || {
        let out = run(r#"
@link("LIB")
extern "C" {
    fn ct_strlen(s: cstr) -> u64
    fn ct_greeting() -> cstr
    fn ct_null_string() -> cstr?
    fn ct_or_default(s: cstr?) -> cstr
    fn ct_join(a: cstr, b: cstr) -> cstr
}
fn main() {
    print(ct_strlen("colibrì"))
    print(ct_greeting())
    print(ct_null_string())
    print(ct_or_default(none))
    print(ct_or_default(some("given")))
    let a = "left"
    print(ct_join(a, "right + {1 + 1}"))
}
"#);
        assert_eq!(
            out,
            "8\nhello from C\nnone\n(default)\ngiven\nleft-right + 2\n"
        );
    })
}

#[test]
fn string_edge_cases_are_panics() {
    vm_pending("string_edge_cases_are_panics", || {
        let e = fails(
            r#"
@link("LIB")
extern "C" {
    fn ct_strlen(s: cstr) -> u64
}
fn main() { print(ct_strlen("a\u{0}b")) }
"#,
        );
        assert!(e.contains("NUL character"), "{e}");
        let e = fails(
            r#"
@link("LIB")
extern "C" {
    fn ct_null_string() -> cstr
}
fn main() { print(ct_null_string()) }
"#,
        );
        assert!(e.contains("C returned NULL for a `cstr`"), "{e}");
        assert!(e.contains("cstr?"), "{e}");
        let e = fails(
            r#"
@link("LIB")
extern "C" {
    fn ct_bad_utf8() -> cstr
}
fn main() { print(ct_bad_utf8()) }
"#,
        );
        assert!(e.contains("not valid UTF-8"), "{e}");
    })
}

#[test]
fn pointers_are_values_and_unsafe_reads_through_them() {
    vm_pending("pointers_are_values_and_unsafe_reads_through_them", || {
        let out = run(r#"
@link("LIB")
extern "C" {
    fn ct_counter_new(start: i32) -> ptr[()]
    fn ct_counter_inc(c: ptr[()])
    fn ct_counter_get(c: ptr[()]) -> i32
    fn ct_counter_free(c: ptr[()])
    fn ct_null_ptr() -> ptr[i32]?
    fn ct_set_i32(p: ptr[i32], v: i32)
    fn ct_sum_i32(p: ptr[i32], n: int) -> i32
    fn ct_divmod(a: i32, b: i32, q: ptr[i32], r: ptr[i32])
}
@link("c")
extern "C" {
    fn malloc(n: u64) -> ptr[u8]
    fn free(p: ptr[u8])
}
fn main() {
    let c = ct_counter_new(10)
    ct_counter_inc(c)
    ct_counter_inc(c)
    print(ct_counter_get(c))
    let same = c
    print(c == same)
    ct_counter_free(c)

    print(ct_null_ptr() == none)

    let mem = malloc(64)
    let ints = unsafe { mem.cast[i32]() }
    ct_set_i32(ints, -7)
    unsafe {
        print(ints.read())
        ints.offset(1).write(30)
        ints.offset(2).write(12)
        print(ints.offset(1).read())
        ct_set_i32(ints, 0)
    }
    print(ct_sum_i32(ints, 3))
    let q = unsafe { mem.offset(32).cast[i32]() }
    let r = unsafe { mem.offset(36).cast[i32]() }
    ct_divmod(17, 5, q, r)
    unsafe { print((q.read(), r.read())) }
    print(ints == ints)
    print(q == r)
    free(mem)
}
"#);
        assert_eq!(out, "12\ntrue\ntrue\n-7\n30\n42\n(3, 2)\ntrue\nfalse\n");
    })
}

#[test]
fn reading_a_string_through_a_pointer() {
    vm_pending("reading_a_string_through_a_pointer", || {
        let out = run(r#"
@link("LIB")
extern "C" {
    fn ct_greeting() -> ptr[u8]
}
fn main() {
    print(unsafe { ct_greeting().read_str() })
}
"#);
        assert_eq!(out, "hello from C\n");
    })
}

#[test]
fn a_null_pointer_where_none_is_not_declared_panics() {
    vm_pending("a_null_pointer_where_none_is_not_declared_panics", || {
        let e = fails(
            r#"
@link("LIB")
extern "C" {
    fn ct_null_ptr() -> ptr[i32]
}
fn main() { let p = ct_null_ptr() }
"#,
        );
        assert!(e.contains("NULL pointer"), "{e}");
        assert!(e.contains("ptr[T]?"), "{e}");
    })
}

#[test]
fn pointer_operations_need_unsafe() {
    vm_pending("pointer_operations_need_unsafe", || {
        for body in [
            "let n = p.read()",
            "p.write(1)",
            "let q = p.offset(1)",
            "let q = p.cast[u8]()",
            "let s = p.cast[u8]().addr()",
        ] {
            let source = format!(
                "@link(\"LIB\")\nextern \"C\" {{\n    fn ct_counter_new(start: i32) -> ptr[i32]\n}}\nfn main() {{\n    let p = ct_counter_new(1)\n    {body}\n}}\n"
            );
            let e = fails(&source);
            assert!(e.contains("needs an `unsafe` block"), "{body}: {e}");
        }
        // `addr` alone is safe, and a closure inside an `unsafe` block is not unsafe itself.
        let e = fails(
            "@link(\"LIB\")\nextern \"C\" {\n    fn ct_counter_new(start: i32) -> ptr[i32]\n}\nfn main() {\n    let p = ct_counter_new(1)\n    unsafe {\n        let f = fn() => p.read()\n    }\n}\n",
        );
        assert!(e.contains("needs an `unsafe` block"), "{e}");
        let out = run(
            "@link(\"LIB\")\nextern \"C\" {\n    fn ct_counter_new(start: i32) -> ptr[i32]\n}\nfn main() {\n    let p = ct_counter_new(1)\n    print(p.addr() != 0)\n}\n",
        );
        assert_eq!(out, "true\n");
    })
}

// ---- structs by value ----

const STRUCTS: &str = r#"
@link("LIB")
extern "C" {
    fn ct_point_add(a: Point, b: Point) -> Point
    fn ct_point_dot(a: Point, b: Point) -> i32
    fn ct_vec2d_scale(v: Vec2d, k: float) -> Vec2d
    fn ct_vec2d_len2(v: Vec2d) -> float
    fn ct_vec2f_add(a: Vec2f, b: Vec2f) -> Vec2f
    fn ct_vec3f_cross(a: Vec3f, b: Vec3f) -> Vec3f
    fn ct_color_invert(c: Color) -> Color
    fn ct_color_pack(c: Color) -> u32
    fn ct_mixed_make(id: i32, score: float) -> Mixed
    fn ct_mixed_sum(m: Mixed) -> float
    fn ct_mixed2_make(w: float, n: i32, f: f32, tag: u8) -> Mixed2
    fn ct_mixed2_sum(m: Mixed2) -> float
    fn ct_big_make(s: int) -> Big
    fn ct_big_sum(b: Big) -> int
    fn ct_big_add(a: Big, b: Big) -> Big
    fn ct_rect_union(a: Rect, b: Rect) -> Rect
    fn ct_rect_area(r: Rect) -> i32
    fn ct_padded_make(tag: u8, value: int, flag: bool, n: u16) -> Padded
    fn ct_padded_sum(p: Padded) -> int
    fn ct_ladder_make() -> Ladder
    fn ct_ladder_sum(l: Ladder) -> int
    fn ct_named_make(name: ptr[u8]) -> Named
    fn ct_named_len(n: Named) -> i32
    fn ct_layout(ty: cstr, field: cstr?) -> u64
}

@repr(C) struct Point { x: i32, y: i32 }
@repr(C) struct Vec2d { x: float, y: float }
@repr(C) struct Vec2f { x: f32, y: f32 }
@repr(C) struct Vec3f { x: f32, y: f32, z: f32 }
@repr(C) struct Color { r: u8, g: u8, b: u8, a: u8 }
@repr(C) struct Mixed { id: i32, score: float }
@repr(C) struct Mixed2 { w: float, n: i32, f: f32, tag: u8 }
@repr(C) struct Big { a: int, b: int, c: int, d: int }
@repr(C) struct Rect { min: Point, max: Point }
@repr(C) struct Padded { tag: u8, value: int, flag: bool, n: u16 }
@repr(C) struct Ladder { a: i8, b: i16, c: i32, d: int, e: u8 }
@repr(C) struct Named { name: ptr[u8], len: i32 }
"#;

fn run_structs(main: &str) -> String {
    run(&format!("{STRUCTS}\nfn main() {{\n{main}\n}}\n"))
}

#[test]
fn small_integer_struct_by_value_in_and_out() {
    vm_pending("small_integer_struct_by_value_in_and_out", || {
        let out = run_structs(
            "let p = ct_point_add(Point { x: 1, y: 2 }, Point { x: 10, y: 20 })
         print(p)
         print(ct_point_dot(p, Point { x: 2, y: 3 }))",
        );
        assert_eq!(out, "Point { x: 11, y: 22 }\n88\n");
    })
}

#[test]
fn float_structs_use_vector_registers() {
    vm_pending("float_structs_use_vector_registers", || {
        let out = run_structs(
            "let v = ct_vec2d_scale(Vec2d { x: 1.5, y: 2.0 }, 2.0)
         print(v)
         print(ct_vec2d_len2(v))
         print(ct_vec2f_add(Vec2f { x: 1.0, y: 2.0 }, Vec2f { x: 0.5, y: 0.25 }))
         print(ct_vec3f_cross(Vec3f { x: 1.0, y: 0.0, z: 0.0 }, Vec3f { x: 0.0, y: 1.0, z: 0.0 }))",
        );
        assert_eq!(
            out,
            "Vec2d { x: 3.0, y: 4.0 }\n25.0\nVec2f { x: 1.5, y: 2.25 }\nVec3f { x: 0.0, y: 0.0, z: 1.0 }\n"
        );
    })
}

#[test]
fn structs_that_mix_integers_and_floats() {
    vm_pending("structs_that_mix_integers_and_floats", || {
        let out = run_structs(
            "let m = ct_mixed_make(7, 0.5)
         print(m)
         print(ct_mixed_sum(m))
         let m2 = ct_mixed2_make(1.5, 2, 0.25, 200)
         print(m2)
         print(ct_mixed2_sum(m2))",
        );
        assert_eq!(
            out,
            "Mixed { id: 7, score: 0.5 }\n7.5\nMixed2 { w: 1.5, n: 2, f: 0.25, tag: 200 }\n203.75\n"
        );
    })
}

#[test]
fn byte_structs_padding_and_ladders() {
    vm_pending("byte_structs_padding_and_ladders", || {
        let out = run_structs(
            "print(ct_color_invert(Color { r: 0, g: 100, b: 255, a: 7 }))
         print(ct_color_pack(Color { r: 1, g: 2, b: 3, a: 4 }))
         let p = ct_padded_make(3, -5, true, 40000)
         print(p)
         print(ct_padded_sum(p))
         let l = ct_ladder_make()
         print(l)
         print(ct_ladder_sum(l))",
        );
        assert_eq!(
            out,
            "Color { r: 255, g: 155, b: 0, a: 7 }\n16909060\nPadded { tag: 3, value: -5, flag: true, n: 40000 }\n40998\nLadder { a: -1, b: -2, c: -3, d: -4, e: 5 }\n-5\n"
        );
    })
}

#[test]
fn large_and_nested_structs_go_through_memory() {
    vm_pending("large_and_nested_structs_go_through_memory", || {
        let out = run_structs(
            "let b = ct_big_make(10)
         print(b)
         print(ct_big_sum(ct_big_add(b, b)))
         let a = Rect { min: Point { x: 0, y: 0 }, max: Point { x: 2, y: 2 } }
         let c = Rect { min: Point { x: 1, y: -1 }, max: Point { x: 5, y: 1 } }
         let u = ct_rect_union(a, c)
         print(u)
         print(ct_rect_area(u))",
        );
        assert_eq!(
            out,
            "Big { a: 10, b: 20, c: 30, d: 40 }\n200\nRect { min: Point { x: 0, y: -1 }, max: Point { x: 5, y: 2 } }\n15\n"
        );
    })
}

#[test]
fn a_struct_with_a_pointer_field() {
    vm_pending("a_struct_with_a_pointer_field", || {
        let out = run(&format!(
            r#"{STRUCTS}
@link("c")
extern "C" {{
    fn malloc(n: u64) -> ptr[u8]
    fn free(p: ptr[u8])
}}
fn main() {{
    let text = malloc(8)
    unsafe {{
        text.write(99)
        text.offset(1).write(111)
        text.offset(2).write(108)
        text.offset(3).write(0)
    }}
    let n = ct_named_make(text)
    print(n.len)
    print(n.name == text)
    print(ct_named_len(n))
    free(text)
}}
"#
        ));
        assert_eq!(out, "3\ntrue\n3\n");
    })
}

#[test]
fn struct_through_a_pointer_read_and_write() {
    vm_pending("struct_through_a_pointer_read_and_write", || {
        let out = run(r#"
@link("LIB")
extern "C" {
    fn ct_point_scale(p: ptr[Point], k: i32)
    fn ct_point_from_ptr(p: ptr[Point]) -> Point
}
@link("c")
extern "C" {
    fn malloc(n: u64) -> ptr[Point]
    fn free(p: ptr[Point])
}
@repr(C) struct Point { x: i32, y: i32 }
fn main() {
    let p = malloc(u64(size_of[Point]()))
    unsafe {
        p.write(Point { x: 3, y: 4 })
    }
    ct_point_scale(p, 5)
    print(ct_point_from_ptr(p))
    unsafe {
        let q = p.read()
        print(q.x + q.y)
    }
    free(p)
}
"#);
        assert_eq!(out, "Point { x: 15, y: 20 }\n35\n");
    })
}

// ---- layout: Colibri's computation against the C compiler's ----

#[test]
fn repr_c_layouts_match_the_c_compiler() {
    vm_pending("repr_c_layouts_match_the_c_compiler", || {
        // size_of and align_of are computed by the checker; `ct_layout` is what C says.
        let out = run_structs(
            r##"
    let names = ["Point", "Vec2d", "Vec2f", "Vec3f", "Color", "Mixed", "Mixed2", "Big", "Rect", "Padded", "Ladder", "Named"]
    let sizes = [size_of[Point](), size_of[Vec2d](), size_of[Vec2f](), size_of[Vec3f](), size_of[Color](), size_of[Mixed](), size_of[Mixed2](), size_of[Big](), size_of[Rect](), size_of[Padded](), size_of[Ladder](), size_of[Named]()]
    let aligns = [align_of[Point](), align_of[Vec2d](), align_of[Vec2f](), align_of[Vec3f](), align_of[Color](), align_of[Mixed](), align_of[Mixed2](), align_of[Big](), align_of[Rect](), align_of[Padded](), align_of[Ladder](), align_of[Named]()]
    let mut i = 0
    while i < names.len() {
        let size = int(ct_layout(names[i], none))
        let align = int(ct_layout(names[i], some("#align")))
        if size != sizes[i] || align != aligns[i] {
            print("{names[i]}: C says {size}/{align}, Colibri says {sizes[i]}/{aligns[i]}")
        }
        i += 1
    }
    print("sizes: {sizes}")
"##,
        );
        assert_eq!(
            out,
            "sizes: [8, 16, 8, 12, 4, 16, 24, 32, 16, 24, 24, 16]\n"
        );
    })
}

#[test]
fn field_offsets_match_the_c_compiler() {
    vm_pending("field_offsets_match_the_c_compiler", || {
        // Every field is exercised through C: a wrong offset would give wrong values above. Here
        // the offsets themselves, through `offset_of`-like arithmetic on a real struct in memory.
        let out = run(&format!(
            r#"
{STRUCTS}
@link("c")
extern "C" {{
    fn malloc(n: u64) -> ptr[u8]
    fn free(p: ptr[u8])
    fn memset(p: ptr[u8], c: i32, n: u64) -> ptr[u8]
}}
fn main() {{
    let mem = malloc(64)
    memset(mem, 0, 64)
    unsafe {{
        let m = mem.cast[Mixed2]()
        m.write(Mixed2 {{ w: 1.0, n: 2, f: 3.0, tag: 4 }})
        // The bytes of the struct as C lays them out.
        print(mem.offset(8).cast[i32]().read())
        print(mem.offset(12).cast[f32]().read())
        print(mem.offset(16).read())
        let p = mem.cast[Padded]()
        p.write(Padded {{ tag: 1, value: 2, flag: true, n: 3 }})
        print(mem.read())
        print(mem.offset(8).cast[int]().read())
        print(mem.offset(16).cast[bool]().read())
        print(mem.offset(18).cast[u16]().read())
    }}
    print(ct_layout("Mixed2", some("n")))
    print(ct_layout("Mixed2", some("f")))
    print(ct_layout("Mixed2", some("tag")))
    print(ct_layout("Padded", some("value")))
    print(ct_layout("Padded", some("flag")))
    print(ct_layout("Padded", some("n")))
    free(mem)
}}
"#
        ));
        assert_eq!(out, "2\n3.0\n4\n1\n2\ntrue\n3\n8\n12\n16\n8\n16\n18\n");
    })
}

#[test]
fn size_of_and_align_of_follow_the_c_rules() {
    vm_pending("size_of_and_align_of_follow_the_c_rules", || {
        let out = run_structs(
            "print(size_of[Rect]())
         print(align_of[Rect]())
         print(size_of[ptr[u8]]())
         print(size_of[bool]())
         print(size_of[u16]())",
        );
        assert_eq!(out, "16\n4\n8\n1\n2\n");
    })
}

// ---- compile-time checks ----

#[test]
fn bad_declarations_are_clear_errors() {
    let (mut vm, _) = vm();
    let e = vm
        .load_source(
            "t.colibri",
            "@repr(C)\nstruct Bad { name: str }\n@repr(C)\nstruct Worse { xs: List[int], f: fn() }\nfn main() {}\n",
        )
        .unwrap_err()
        .render(false);
    assert!(
        e.contains("the field `name` of the `@repr(C)` struct `Bad` cannot be passed to C"),
        "{e}"
    );
    assert!(
        e.contains("`str` is not a C type; in a signature write `cstr`"),
        "{e}"
    );
    assert!(
        e.contains("the field `xs` of the `@repr(C)` struct `Worse`"),
        "{e}"
    );

    let cases: &[(&str, &str)] = &[
        (
            "extern \"C\" { fn f(s: str) }\nfn main() {}",
            "`str` is not a C type; in a signature write `cstr`",
        ),
        (
            "extern \"C\" { fn f(s: List[int]) }\nfn main() {}",
            "this type cannot cross the C boundary",
        ),
        (
            "struct Plain { x: i32 }\nextern \"C\" { fn f(p: Plain) }\nfn main() {}",
            "`Plain` is not a `@repr(C)` struct",
        ),
        (
            "extern \"C\" { fn f() -> char }\nfn main() {}",
            "`char` is not a C type",
        ),
        (
            "let s: cstr = \"a\"\nfn main() {}",
            "`cstr` can only be used in the signature of an `extern` function",
        ),
        (
            "extern \"Rust\" { fn f() }\nfn main() {}",
            "the ABI \"Rust\" is not supported",
        ),
        (
            "extern \"C\" { fn f(x: int) { } }\nfn main() {}",
            "has no body",
        ),
        (
            "@repr(C)\nstruct G[T] { x: T }\nfn main() {}",
            "a `@repr(C)` struct cannot be generic",
        ),
        (
            "@repr(C)\nclass C { x: i32 }\nfn main() {}",
            "`@repr(C)` applies to a `struct`, not a `class`",
        ),
        (
            "@repr(packed)\nstruct P { x: i32 }\nfn main() {}",
            "`@repr` takes `C`",
        ),
        (
            "fn main() { let p: ptr[str] = none }",
            "`ptr[str]` points to something C cannot see",
        ),
        (
            "@repr(C) struct E {}\nextern \"C\" { fn f(e: E) }\nfn main() {}",
            "has no fields, so it cannot be passed by value",
        ),
        (
            "@repr(C) struct S { a: [u8; 4] }\nfn main() {}",
            "fixed-size array type is not supported yet",
        ),
    ];
    for (source, want) in cases {
        let (mut vm, _) = self::vm();
        let e = vm
            .load_source("t.colibri", source)
            .unwrap_err()
            .render(false);
        assert!(e.contains(want), "for `{source}`: {e}");
    }
}

// ---- capabilities ----

#[test]
fn without_cap_ffi_a_program_with_extern_does_not_load() {
    let mut vm = Vm::builder().build();
    let e = vm
        .load_source(
            "t.colibri",
            "extern \"C\" { fn abs(x: i32) -> i32 }\nfn main() {}\n",
        )
        .unwrap_err()
        .render(false);
    assert!(
        e.contains("`extern` blocks need the `ffi` capability, which this host did not grant"),
        "{e}"
    );
    assert!(e.contains("Cap::Ffi"), "{e}");
}

fn package(files: &[(&str, &str)]) -> tempdir::Dir {
    tempdir::Dir::new(files)
}

mod tempdir {
    use std::path::{Path, PathBuf};

    pub struct Dir(pub PathBuf);

    impl Dir {
        pub fn new(files: &[(&str, &str)]) -> Dir {
            let root = std::env::temp_dir().join(format!(
                "colibri-ffi-test-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            for (path, text) in files {
                let full = root.join(path);
                std::fs::create_dir_all(full.parent().expect("parent")).expect("mkdir");
                std::fs::write(full, text).expect("write");
            }
            Dir(root)
        }

        pub fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
}

const EXTERN_MODULE: &str = "@link(\"c\")\nextern \"C\" {\n    pub fn abs(x: i32) -> i32\n}\npub fn magnitude(x: i32) -> i32 => abs(x)\n";

#[test]
fn a_package_needs_ffi_true_in_its_manifest() {
    vm_pending("a_package_needs_ffi_true_in_its_manifest", || {
        let dir = package(&[
            ("colibri.toml", "[package]\nname = \"app\"\n"),
            (
                "main.colibri",
                "fn main() { print(abs(-3)) }\n@link(\"c\")\nextern \"C\" { fn abs(x: i32) -> i32 }\n",
            ),
        ]);
        let (mut vm, _) = vm();
        let e = vm.load_package(dir.path()).unwrap_err().render(false);
        assert!(
            e.contains("the package `app` cannot contain `extern` blocks"),
            "{e}"
        );
        assert!(e.contains("ffi = true"), "{e}");

        let dir = package(&[
            ("colibri.toml", "[package]\nname = \"app\"\nffi = true\n"),
            (
                "main.colibri",
                "fn main() { print(abs(-3)) }\n@link(\"c\")\nextern \"C\" { fn abs(x: i32) -> i32 }\n",
            ),
        ]);
        let (mut vm, out) = self::vm();
        vm.load_package(dir.path()).unwrap();
        vm.run_main().unwrap();
        assert_eq!(out.contents(), "3\n");
    })
}

#[test]
fn a_dependency_cannot_link_c_without_declaring_ffi() {
    vm_pending("a_dependency_cannot_link_c_without_declaring_ffi", || {
        let main = "import shim\nfn main() { print(shim.magnitude(-9)) }\n";
        let dir = package(&[
            (
                "app/colibri.toml",
                "[package]\nname = \"app\"\nffi = true\n\n[dependencies]\nshim = { path = \"../shim\" }\n",
            ),
            ("app/main.colibri", main),
            ("shim/colibri.toml", "[package]\nname = \"shim\"\n"),
            ("shim/lib.colibri", EXTERN_MODULE),
        ]);
        let (mut vm, _) = vm();
        let e = vm
            .load_package(dir.path().join("app"))
            .unwrap_err()
            .render(false);
        assert!(
            e.contains("the package `shim` cannot contain `extern` blocks"),
            "{e}"
        );
        assert!(e.contains("ffi = true"), "{e}");

        // The program's own `ffi = true` does not extend to its dependencies.
        let dir = package(&[
            (
                "app/colibri.toml",
                "[package]\nname = \"app\"\nffi = true\n\n[dependencies]\nshim = { path = \"../shim\" }\n",
            ),
            ("app/main.colibri", main),
            (
                "shim/colibri.toml",
                "[package]\nname = \"shim\"\nffi = true\n",
            ),
            ("shim/lib.colibri", EXTERN_MODULE),
        ]);
        let (mut vm, out) = self::vm();
        vm.load_package(dir.path().join("app")).unwrap();
        vm.run_main().unwrap();
        assert_eq!(out.contents(), "9\n");
    })
}

#[test]
fn a_script_without_a_manifest_may_use_extern_when_the_host_allows() {
    vm_pending(
        "a_script_without_a_manifest_may_use_extern_when_the_host_allows",
        || {
            let dir = package(&[(
                "script.colibri",
                "@link(\"c\")\nextern \"C\" { fn abs(x: i32) -> i32 }\nprint(abs(-4))\n",
            )]);
            let (mut vm, out) = vm();
            vm.load_file(dir.path().join("script.colibri")).unwrap();
            assert_eq!(out.contents(), "4\n");
        },
    )
}

#[test]
fn manifest_ffi_must_be_a_boolean() {
    let dir = package(&[
        ("colibri.toml", "[package]\nname = \"app\"\nffi = \"yes\"\n"),
        ("main.colibri", "fn main() {}\n"),
    ]);
    let (mut vm, _) = vm();
    let e = vm.load_package(dir.path()).unwrap_err().render(false);
    assert!(e.contains("`ffi` must be `true` or `false`"), "{e}");
}

// ---- linking ----

#[test]
fn a_missing_library_or_symbol_is_a_load_error() {
    vm_pending("a_missing_library_or_symbol_is_a_load_error", || {
        let (mut vm, _) = vm();
        let e = vm
            .load_source(
                "t.colibri",
                "@link(\"definitely_not_a_library_xyz\")\nextern \"C\" { fn f() }\nfn main() {}\n",
            )
            .unwrap_err();
        assert!(matches!(e, colibri::Error::Ffi(_)), "{e}");
        assert!(
            e.to_string()
                .contains("cannot load the library `definitely_not_a_library_xyz`"),
            "{e}"
        );
        let (mut vm, _) = self::vm();
        let e = vm
            .load_source(
                "t.colibri",
                "@link(\"c\")\nextern \"C\" { fn colibri_no_such_function() }\nfn main() {}\n",
            )
            .unwrap_err()
            .to_string();
        assert!(e.contains("no symbol `colibri_no_such_function`"), "{e}");
        // The Vm is still usable.
        let (mut vm, out) = self::vm();
        vm.load_source("t.colibri", "fn main() { print(1) }\n")
            .unwrap();
        vm.run_main().unwrap();
        assert_eq!(out.contents(), "1\n");
    })
}

#[test]
fn symbols_of_the_process_need_no_link_attribute() {
    vm_pending("symbols_of_the_process_need_no_link_attribute", || {
        let out = run(
            "extern \"C\" { fn strlen(s: cstr) -> u64 }\nfn main() { print(strlen(\"four\")) }\n",
        );
        assert_eq!(out, "4\n");
    })
}

#[test]
fn a_layout_error_in_a_struct_is_not_reported_twice_and_other_code_still_checks() {
    let (mut vm, _) = vm();
    let e = vm
        .load_source(
            "t.colibri",
            "@repr(C)\nstruct Bad { s: str }\nextern \"C\" { fn f(b: Bad) }\nfn main() { let x: int = \"no\" }\n",
        )
        .unwrap_err()
        .render(false);
    assert!(e.contains("the field `s`"), "{e}");
    assert!(e.contains("expected"), "{e}");
}
