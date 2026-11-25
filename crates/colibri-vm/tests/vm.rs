//! Disassembly snapshots of small functions, and behaviour of the first slice end to end.

use colibri_vm::{Options, Outcome, compile_source, run_source_with};

/// Runs with a budget, so a runaway program fails its test instead of hanging the run.
fn run_source(src: &str) -> Outcome {
    run_source_with(
        src,
        &Options {
            instruction_budget: Some(20_000_000),
            ..Options::default()
        },
    )
}

/// How many functions the prelude (written in Colibri) puts before the program's own, so that
/// a snapshot can name a function by its place among the program's: `fn#1` is the first one.
fn prelude_fns() -> u32 {
    let (ast, _) = colibri_syntax::parse_file(colibri_syntax::FileId(0), "");
    let program = colibri_check::check(&ast)
        .program
        .expect("an empty program checks");
    program.fns.len() as u32
}

/// The disassembly of `name`, with the functions numbered from 1 among the program's own (the
/// functions of the prelude have no number here).
fn dis(src: &str, name: &str) -> String {
    let all = compile_source(src).expect("the program compiles");
    let text = all
        .into_iter()
        .find(|(n, _)| n == name)
        .unwrap_or_else(|| panic!("no function `{name}`"))
        .1;
    let skip = prelude_fns() - 1;
    let mut out = String::new();
    let mut rest = text.as_str();
    while let Some(at) = rest.find("fn#") {
        let (head, tail) = rest.split_at(at + 3);
        out.push_str(head);
        let digits = tail.chars().take_while(char::is_ascii_digit).count();
        let n: u32 = tail[..digits].parse().expect("a function number");
        out.push_str(&n.saturating_sub(skip).to_string());
        rest = &tail[digits..];
    }
    out.push_str(rest);
    out
}

fn run_ok(src: &str) -> String {
    let o = run_source(src);
    assert_eq!(o.panic, None, "unexpected panic");
    o.stdout
}

fn run_panic(src: &str) -> Outcome {
    let o = run_source(src);
    assert!(o.panic.is_some(), "expected a panic, stdout: {}", o.stdout);
    o
}

// ---- disassembly snapshots ----

#[test]
fn fib_uses_fused_compare_and_a_window_that_starts_at_the_arguments() {
    let src = "fn fib(n: int) -> int {
    if n < 2 { return n }
    return fib(n - 1) + fib(n - 2)
}";
    // The argument of each call is the callee's first register (base=r3, base=r4): nothing is
    // copied. The result of the first call stays in r2 while the second runs.
    let expected = "\
0000  Step 1
0001  JmpNotLtIK r0 2 -> 0005
0002  Step 1
0003  Copy r1 r0
0004  Return r1
0005  Step 1
0006  AddIK r3 r0 -1
0007  Call r2 base=r3 fn#1
0008  AddIK r4 r0 -2
0009  Call r3 base=r4 fn#1
0010  AddI r1 r2 r3
0011  Return r1
0012  ReturnUnit
";
    assert_eq!(dis(src, "fib"), expected);
}

#[test]
fn a_counted_loop_is_one_fused_back_edge() {
    let src = "fn main() {
    let mut sum = 0
    for i in 0..5000000 {
        sum += i % 7
    }
    print(sum)
}";
    let expected = "\
0000  Step 2
0001  LoadInt r0 0
0002  LoadInt r1 0
0003  LoadInt r2 5000000
0004  JmpNotLtI r1 r2 -> 0009
0005  Step 1
0006  RemIK r3 r1 7
0007  AddI r0 r0 r3
0008  ForRangeNext r1 r2 -> 0005
0009  Step 1
0010  Print r0
0011  ReturnUnit
";
    assert_eq!(dis(src, "main"), expected);
}

#[test]
fn conditions_become_jumps_and_break_and_continue_jump_to_the_loop_labels() {
    let src = "fn f(a: int, b: int) -> int {
    let mut n = 0
    while n < a && b > 1 || n == 7 {
        n += 1
        if n == 3 { continue }
        if n > 5 { break }
    }
    return n
}";
    let expected = "\
0000  Step 2
0001  LoadInt r2 0
0002  Step 1
0003  JmpNotLtI r2 r0 -> 0005
0004  JmpNotLeIK r1 1 -> 0006
0005  JmpNotEqIK r2 7 -> 0016
0006  Step 2
0007  AddIK r2 r2 1
0008  JmpNotEqIK r2 3 -> 0011
0009  Step 1
0010  Jmp -> 0018
0011  Step 1
0012  JmpNotGtIK r2 5 -> 0015
0013  Step 1
0014  Jmp -> 0019
0015  Jmp -> 0018
0016  Step 1
0017  Jmp -> 0019
0018  Loop -> 0002
0019  Step 1
0020  Return r2
0021  ReturnUnit
";
    assert_eq!(dis(src, "f"), expected);
}

#[test]
fn string_temporaries_are_dropped_right_after_their_use_and_locals_at_return() {
    let src = "fn f(s: str, n: int) -> str {
    let t = s + \"x\"
    return \"v{t}{n}\"
}";
    let expected = "\
0000  Step 2
0001  LoadK r3 k0 (\"x\")
0002  Binary r2 r0 r3
0003    site Concat
0004  DropInert r3
0005  LoadK r4 k1 (\"v\")
0006  Copy r5 r2
0007  Copy r6 r1
0008  StrConcat r3 base=r4 n=3
0009  DropInert r5
0010  DropInert r4
0011  DropInert r2
0012  Return r3
0013  ReturnUnit
0014  Drop r5
0015  Drop r4
0016  Drop r2
0017  Jmp -> 0020
0018  Drop r3
0019  Jmp -> 0017
0020  Resume
unwind [0002, 0004) -> 0018
unwind [0008, 0009) -> 0014
";
    assert_eq!(dis(src, "f"), expected);
}

#[test]
fn a_shown_part_of_a_string_is_dropped_right_after_its_use() {
    let src = "fn f(s: str, x: float) -> str {
    let t = s + \"x\"
    return \"v{t}{x}\"
}";
    let expected = "\
0000  Step 2
0001  LoadK r3 k0 (\"x\")
0002  Binary r2 r0 r3
0003    site Concat
0004  DropInert r3
0005  LoadK r4 k1 (\"v\")
0006  Copy r5 r2
0007  Show r6 r1
0008  StrConcat r3 base=r4 n=3
0009  DropInert r6
0010  DropInert r5
0011  DropInert r4
0012  DropInert r2
0013  Return r3
0014  ReturnUnit
0015  Drop r6
0016  Drop r5
0017  Drop r4
0018  Drop r2
0019  Jmp -> 0022
0020  Drop r3
0021  Jmp -> 0019
0022  Resume
unwind [0002, 0004) -> 0020
unwind [0007, 0008) -> 0016
unwind [0008, 0009) -> 0015
";
    assert_eq!(dis(src, "f"), expected);
}

#[test]
fn a_var_self_call_moves_the_receiver_into_register_zero_last_and_back() {
    let src = "struct C { n: int }
impl C { fn add(mut self, k: int) {} }
fn run() {
    let mut c = C { n: 0 }
    c.add(3)
}";
    // The argument is evaluated first; the receiver is taken last (`Move r2 r0`), becomes the
    // callee's register 0 (the window starts at r2) and is moved back after the call.
    let expected = "\
0000  Step 2
0001  LoadInt r1 0
0002  NewStruct r0 struct#22 [r1]
0003  LoadInt r3 3
0004  Move r2 r0
0005  Call r1 base=r2 fn#1
0006  Move r0 r2
0007  DropCheck r0
0008  ReturnUnit
0009  Move r0 r2
0010  FoldHooks
0011  Drop r0
0012  FoldHooks
0013  Jmp -> 0016
0014  FoldHooks
0015  Jmp -> 0012
0016  Resume
unwind [0000, 0001) -> 0012
unwind [0002, 0003) -> 0014
unwind [0005, 0006) -> 0009
";
    assert_eq!(dis(src, "run"), expected);
}

#[test]
fn a_var_self_call_on_a_list_element_holds_the_place_and_writes_back_before_letting_go() {
    let src = "struct P { n: int }
impl P { fn bump(mut self, by: int) { self.n += by } }
fn elem(xs: List[P], i: int) {
    xs[i].bump(2)
}";
    // The list and the index are parameters, so they are the anchor as they are. The element is
    // copied into the callee's register 0 (`ListGetAt r3`), the callee changes it in place, and
    // the write-back (unguarded) comes before the hold is released. The landing pad of the call
    // does the same two steps in the same order.
    let expected = "\
0000  Step 1
0001  LoadInt r4 2
0002  PlaceHold r0
0003    site Elem[]
0004  ListGetAt r3 r0[r1]
0005  Call r2 base=r3 fn#1
0006  PlaceSet r0 r1 r3
0007    site Elem[] unguarded
0008  Unhold
0009  CheckHooks
0010  ReturnUnit
0011  PlaceSet r0 r1 r3
0012    site Elem[] unguarded
0013  Unhold
0014  FoldHooks
0015  FoldHooks
0016  Jmp -> 0017
0017  Resume
unwind [0000, 0001) -> 0015
unwind [0002, 0004) -> 0014
unwind [0004, 0005) -> 0013
unwind [0005, 0006) -> 0011
unwind [0006, 0008) -> 0013
unwind [0009, 0010) -> 0015
unwind [0011, 0013) -> 0013
";
    assert_eq!(dis(src, "elem"), expected);
}

#[test]
fn a_for_var_trip_writes_back_before_the_guard_and_the_source_are_let_go() {
    let src = "struct P { n: int }
fn each(ps: List[P]) {
    let mut xs = ps
    for mut p in xs {
        p.n += 1
    }
}";
    // The element is a struct of scalars, so the loop may lend the slot of the element to the
    // binding (`HoldListLend` decides at run time, from the owners of the list). The source (r3),
    // its position (r4), the copy of the element in flight (r5) and the lent flag (r6) are
    // consecutive registers. `ForListNextLend` skips the `Jmp` that ends the loop when it
    // produced an item. The write-back is the last act of a trip, after the trip's hook check
    // (in the pad: `FoldHooks` at 0022, then `ForWriteBackLend` at 0023); the pad then lets go of
    // the guard and drops the source (0025-0026).
    let expected = "\
0000  Step 2
0001  Copy r1 r0
0002  Copy r3 r1
0003  LoadInt r4 0
0004  HoldListLend r3 \"xs\"
0005  ForListNextLend r2 src=r3
0006  Jmp -> 0013
0007  Step 1
0008  LoadInt r7 1
0009  FieldAddI r2.0 r7
0010  ForWriteBackLend r2 src=r3
0011  CheckHooks
0012  Jmp -> 0005
0013  Unhold
0014  DropCheck r3
0015  DropCheck r1
0016  ReturnUnit
0017  Drop r1
0018  FoldHooks
0019  Jmp -> 0029
0020  FoldHooks
0021  FoldHooks
0022  FoldHooks
0023  ForWriteBackLend r2 src=r3
0024  FoldHooks
0025  Unhold
0026  Drop r3
0027  FoldHooks
0028  Jmp -> 0017
0029  Resume
unwind [0000, 0001) -> 0018
unwind [0004, 0005) -> 0026
unwind [0005, 0006) -> 0025
unwind [0007, 0008) -> 0021
unwind [0009, 0010) -> 0020
unwind [0011, 0012) -> 0025
unwind [0014, 0015) -> 0017
";
    assert_eq!(dis(src, "each"), expected);
}

#[test]
fn a_for_var_over_structs_that_hold_a_class_copies_the_element_in_and_out() {
    let src = "class R { k: int }
struct P { n: int, r: R }
fn each(ps: List[P]) {
    let mut xs = ps
    for mut p in xs {
        p.n += 1
    }
}";
    // The struct holds a class, so releasing an element is observable (a `Drop` hook): no slot
    // is lent. The source (r3), its position (r4) and the copy of the element in flight (r5) are
    // consecutive registers. `ForListNextVar` skips the `Jmp` that ends the loop when it
    // produced an item. The write-back is the last act of a trip, after the trip's hook check
    // (in the pad: `FoldHooks` at 0022, then `ForWriteBack` at 0023); the pad then lets go of
    // the guard and drops the source (0025-0026).
    let expected = "\
0000  Step 2
0001  Copy r1 r0
0002  Copy r3 r1
0003  LoadInt r4 0
0004  HoldListPlace r3 \"xs\"
0005  ForListNextVar r2 src=r3
0006  Jmp -> 0013
0007  Step 1
0008  LoadInt r6 1
0009  FieldAddI r2.0 r6
0010  ForWriteBack r2 src=r3
0011  CheckHooks
0012  Jmp -> 0005
0013  Unhold
0014  DropCheck r3
0015  DropCheck r1
0016  ReturnUnit
0017  Drop r1
0018  FoldHooks
0019  Jmp -> 0029
0020  FoldHooks
0021  FoldHooks
0022  FoldHooks
0023  ForWriteBack r2 src=r3
0024  FoldHooks
0025  Unhold
0026  Drop r3
0027  FoldHooks
0028  Jmp -> 0017
0029  Resume
unwind [0000, 0001) -> 0018
unwind [0004, 0005) -> 0026
unwind [0005, 0006) -> 0025
unwind [0007, 0008) -> 0021
unwind [0009, 0010) -> 0020
unwind [0011, 0012) -> 0025
unwind [0014, 0015) -> 0017
";
    assert_eq!(dis(src, "each"), expected);
}

#[test]
fn script_values_are_checked_and_initialised_through_the_global_ops() {
    let src = "let mut g = 1
let h = g + 2
fn get() -> int => g + h
print(get())
g = 5
print(get())";
    assert_eq!(
        dis(src, "get"),
        "\
0000  GetGlobalChecked r1 g0
0001  GetGlobalChecked r2 g1
0002  AddI r0 r1 r2
0003  Return r0
"
    );
    assert_eq!(
        dis(src, "<script>"),
        "\
0000  Step 5
0001  CallInit r0 base=r1 g0
0002  InitGlobal g0 r0
0003  CallInit r0 base=r1 g1
0004  InitGlobal g1 r0
0005  Call r0 base=r1 fn#1
0006  Print r0
0007  CheckGlobal g0
0008  LoadInt r0 5
0009  SetGlobal g0 r0
0010  Call r0 base=r1 fn#1
0011  Print r0
0012  ReturnUnit
"
    );
}

#[test]
fn an_argument_that_leaves_a_value_to_the_statement_end_is_copied_into_the_window() {
    // A struct argument is not inert, so it lives to the statement end; the call window is
    // then built above it with copies.
    let src = "struct P { x: int }
fn mk(a: P, n: int) -> P => a
fn two(a: P, n: int) -> int => n
fn run() -> int => two(mk(P { x: 1 }, 3), 4)";
    let d = dis(src, "run");
    // Register 1 holds mk's result and r2 the struct temp; the call to `two` gets a fresh window
    // (r4..) with copies, and the statement-end drops run newest first. The landing pads below
    // the code drop what is alive at each call, in the same order.
    let expected = "\
0000  LoadInt r3 1
0001  NewStruct r2 struct#22 [r3]
0002  LoadInt r3 3
0003  Call r1 base=r2 fn#1
0004  LoadInt r3 4
0005  Copy r4 r1
0006  Copy r5 r3
0007  Call r0 base=r4 fn#2
0008  Drop r4
0009  Drop r1
0010  DropCheck r2
0011  Return r0
0012  Drop r4
0013  Drop r1
0014  Drop r2
0015  FoldHooks
0016  Jmp -> 0017
0017  Resume
unwind [0001, 0002) -> 0015
unwind [0003, 0004) -> 0014
unwind [0007, 0008) -> 0012
";
    assert_eq!(d, expected);
}

// ---- behaviour ----

#[test]
fn arithmetic_and_comparison_follow_the_core_rules() {
    let src = "fn main() {
    print(7 / 2)
    print(-7 / 2)
    print(-7 % 3)
    print(2 * 3 + 4)
    print(1.5 + 2.25)
    print(10.0 / 4.0)
    print(3 < 4 && 4 <= 4)
    print(2.0 > 3.0 || 1 != 1)
    print(!(1 == 1))
    print(-5)
    print(-2.5)
    print(9223372036854775807)
}";
    assert_eq!(
        run_ok(src),
        "3\n-3\n-1\n10\n3.75\n2.5\ntrue\nfalse\nfalse\n-5\n-2.5\n9223372036854775807\n"
    );
}

#[test]
fn integer_faults_panic_with_the_position_and_the_stack() {
    let src = "fn div(a: int, b: int) -> int => a / b
fn outer(x: int) -> int {
    return div(x, x - 3)
}
fn main() {
    print(outer(1))
    print(outer(3))
}";
    let o = run_panic(src);
    assert_eq!(o.stdout, "0\n");
    assert_eq!(o.panic.as_deref(), Some("division by zero"));
    // Innermost first: the division (line 1), the call in outer (line 3), the call in main (line 7).
    let want: Vec<(String, u32)> = vec![("div".into(), 1), ("outer".into(), 3), ("main".into(), 7)];
    assert_eq!(o.trace, want);
}

#[test]
fn overflow_and_modulo_by_zero_are_panics() {
    let o = run_panic("fn main() { let mut x = 9223372036854775807\n x += 1\n print(x) }");
    assert_eq!(o.panic.as_deref(), Some("integer overflow"));
    let o = run_panic("fn main() { let z = 0\n print(5 % z) }");
    assert_eq!(o.panic.as_deref(), Some("modulo by zero"));
    let o = run_panic("fn main() { let m = -9223372036854775807 - 1\n print(m / -1) }");
    assert_eq!(o.panic.as_deref(), Some("integer overflow"));
    let o = run_panic("fn main() { let m = -9223372036854775807 - 1\n print(-m) }");
    assert_eq!(o.panic.as_deref(), Some("integer overflow"));
}

#[test]
fn loops_break_continue_and_labels_of_nested_loops() {
    let src = "fn main() {
    let mut total = 0
    for i in 0..5 {
        for j in 0..=3 {
            if j == 2 { continue }
            if i == 3 { break }
            total += i * 10 + j
        }
    }
    print(total)
    let mut n = 0
    while true {
        n += 1
        if n >= 4 { break }
    }
    print(n)
    let mut k = 0
    while true {
        k += 1
        if k == 2 { continue }
        if k > 3 { break }
    }
    print(k)
}";
    // i=0: j=0,1,3 -> 0+1+3; i=1: 10+11+13; i=2: 20+21+23; i=3: break at once; i=4: 40+41+43.
    assert_eq!(run_ok(src), "226\n4\n4\n");
}

#[test]
fn an_inclusive_range_does_not_step_past_its_end() {
    let src = "fn main() {
    let mut seen = 0
    for i in 9223372036854775805..=9223372036854775807 { seen += 1 }
    print(seen)
    for i in 5..=4 { print(\"never\") }
    for i in 3..3 { print(\"never\") }
    for i in -2..2 { print(i) }
}";
    assert_eq!(run_ok(src), "3\n-2\n-1\n0\n1\n");
}

#[test]
fn the_loop_variable_is_a_fresh_binding_even_when_the_loop_changes_it() {
    let src = "fn main() {
    for mut i in 0..3 {
        i += 10
        print(i)
    }
}";
    assert_eq!(run_ok(src), "10\n11\n12\n");
}

#[test]
fn strings_concatenate_interpolate_and_survive_early_exits() {
    // Every path out of the loops drops the string temporaries and locals, or the debug check
    // of the Unit invariant fails the test when the frame pops.
    let src = "fn pick(n: int) -> str {
    let mut acc = \"\"
    for i in 0..10 {
        let piece = \"<\" + \"{i}\" + \">\"
        if i == n { return acc + piece }
        if i % 2 == 0 { continue }
        acc = acc + piece
        if i > 7 { break }
    }
    return acc
}
fn main() {
    print(pick(3))
    print(pick(100))
    print(pick(0))
    print(\"a\" + \"b\" == \"ab\")
    print(\"{1}{2.5}{true}{'c'}\")
}";
    assert_eq!(
        run_ok(src),
        "<1><3>\n<1><3><5><7><9>\n<0>\ntrue\n12.5truec\n"
    );
}

#[test]
fn format_and_show_intrinsics() {
    let src = "fn main() {
    print(\"{255:x}|{255:08b}|{3.14159:.2}|{\"ab\":>5}|{7:<3}|\")
    let s = \"{42}\"
    print(s)
}";
    assert_eq!(run_ok(src), "ff|11111111|3.14|   ab|7  |\n42\n");
}

#[test]
fn use_of_a_script_value_before_its_line_is_rejected_before_any_code_runs() {
    // The checker catches direct and through-a-function uses, so nothing is printed; the
    // runtime guard (`GetGlobalChecked`) is only reachable through dynamic dispatch, which
    // needs struct values (golden `script_value_read_too_early_through_show`, pending).
    let o = run_panic("fn twice(x: int) -> int => x * 2\nprint(base)\nlet base = twice(2)");
    assert!(o.compile_error);
    assert_eq!(o.stdout, "");
    assert!(
        o.panic
            .as_deref()
            .unwrap_or("")
            .contains("`base` is used before it is initialised")
    );
    let o = run_panic("fn get() -> int => later\nprint(\"start\")\nprint(get())\nlet later = 5");
    assert!(o.compile_error);
    assert_eq!(o.stdout, "");
    assert!(
        o.panic
            .as_deref()
            .unwrap_or("")
            .contains("`later` is used before it is initialised (through `get()`)")
    );
}

#[test]
fn module_values_update_in_place_and_compound_assign_works_on_them() {
    let src = "let mut count = 0
let mut name = \"a\"
fn bump(by: int) {
    count += by
    name = name + \"!\"
}
bump(2)
bump(3)
print(count)
print(name)";
    assert_eq!(run_ok(src), "5\na!!\n");
}

#[test]
fn deep_recursion_hits_the_depth_limit_with_a_stack_that_names_every_frame() {
    let src = "fn down(n: int) -> int => down(n + 1) + 1
print(down(0))";
    let o = run_source_with(
        src,
        &Options {
            max_call_depth: 50,
            ..Options::default()
        },
    );
    assert_eq!(
        o.panic.as_deref(),
        Some("stack overflow: more than 50 nested calls")
    );
    // The script frame and 49 `down` frames fit; the 50th call to `down` is the refused one.
    assert_eq!(o.trace.len(), 50);
    assert!(o.trace[..49].iter().all(|(f, _)| f == "down"));
    assert_eq!(o.trace[49].0, "<script>");
}

#[test]
fn recursion_within_the_limit_runs_on_the_heap_stack() {
    // 9,000 nested calls on a test thread's small stack: the VM does not recurse natively.
    let src = "fn down(n: int) -> int {
    if n == 0 { return 0 }
    return down(n - 1) + 1
}
print(down(9000))";
    assert_eq!(run_ok(src), "9000\n");
}

#[test]
fn an_infinite_loop_is_stopped_by_the_instruction_budget() {
    let src = "fn spin() { while true { } }\nfn main() { spin() }";
    let o = run_source_with(
        src,
        &Options {
            instruction_budget: Some(10_000),
            ..Options::default()
        },
    );
    assert_eq!(
        o.panic.as_deref(),
        Some("the instruction budget is exhausted")
    );
    assert_eq!(o.trace[0].0, "spin");
    // A counted loop that is long enough runs out too.
    let o = run_source_with(
        "fn main() { let mut s = 0\n for i in 0..1000000 { s += i }\n print(s) }",
        &Options {
            instruction_budget: Some(5_000),
            ..Options::default()
        },
    );
    assert_eq!(
        o.panic.as_deref(),
        Some("the instruction budget is exhausted")
    );
    // And a program that fits is not stopped.
    let o = run_source_with(
        "fn main() { let mut s = 0\n for i in 0..100 { s += i }\n print(s) }",
        &Options {
            instruction_budget: Some(5_000),
            ..Options::default()
        },
    );
    assert_eq!(o.panic, None);
    assert_eq!(o.stdout, "4950\n");
}

#[test]
fn recursive_calls_are_charged_to_the_budget() {
    let o = run_source_with(
        "fn f(n: int) -> int => f(n + 1)\nfn main() { print(f(0)) }",
        &Options {
            instruction_budget: Some(100),
            ..Options::default()
        },
    );
    assert_eq!(
        o.panic.as_deref(),
        Some("the instruction budget is exhausted")
    );
}

#[test]
fn panic_and_assert_intrinsics() {
    let o = run_panic("fn main() { print(1)\n panic(\"boom\") }");
    assert_eq!(o.stdout, "1\n");
    assert_eq!(o.panic.as_deref(), Some("boom"));
    assert_eq!(o.trace[0], ("main".to_string(), 2));
    let o = run_panic("fn main() { assert(1 + 1 == 3, \"math\") }");
    assert_eq!(o.panic.as_deref(), Some("math"));
    let o = run_panic("fn main() { assert_eq(2 * 2, 5) }");
    assert!(
        o.panic
            .unwrap_or_default()
            .starts_with("assertion failed: `left == right`")
    );
    assert_eq!(
        run_ok("fn main() { assert(true, \"fine\")\n assert_eq(\"a\", \"a\")\n print(\"ok\") }"),
        "ok\n"
    );
}

#[test]
fn numeric_conversions_and_methods_run_through_the_intrinsic_instruction() {
    let src = "fn main() {
    let x = 200
    print(u8(x))
    print(int(2.9))
    print(f32(7) / f32(2))
    let w = u8(250)
    print(w.wrapping_add(10))
    print(9.0.sqrt())
    let big = 300
    print(u8(big))
}";
    let o = run_source(src);
    assert!(!o.compile_error, "{o:?}");
    assert_eq!(o.stdout, "200\n2\n3.5\n4\n3.0\n");
    let msg = o.panic.expect("the last conversion panics");
    assert!(msg.contains("value 300 does not fit in `u8`"), "{msg}");
    assert_eq!(o.trace[0].1, 10, "{:?}", o.trace);
}

#[test]
fn go_is_compiled_and_runs_the_coroutine_up_to_its_first_pause_before_returning() {
    // `go` was the last construct the compiler refused; nothing a checked program can say is
    // refused any more (the refusal path stays for what the checker may add).
    let o = run_source(
        "fn w() {\n    print(\"in w\")\n    wait(1.0)\n    print(\"w again\")\n}\nfn main() { print(\"before\")\n let c = go w()\n print(\"after\")\n c.join() }",
    );
    assert_eq!(o.panic, None);
    assert_eq!(o.stdout, "before\nin w\nafter\nw again\n");
}

#[test]
fn a_program_that_does_not_compile_reports_its_errors() {
    let o = run_source("fn main() { print(nope) }");
    assert!(o.compile_error);
    assert!(o.panic.unwrap_or_default().contains("nope"));
}

#[test]
fn compound_assignment_reads_the_old_value_after_the_right_hand_side() {
    // The right-hand side changes the place; the old value is read afterwards (as the
    // tree-walker does): 1 + 100 first, then + 5.
    let src = "let mut g = 1
fn bump() -> int {
    g = g + 100
    return 5
}
g += bump()
print(g)
let mut name = \"a\"
fn touch() -> str {
    name = name + \"r\"
    return \"T\"
}
name += touch()
print(name)";
    assert_eq!(run_ok(src), "106\narT\n");
}

#[test]
fn a_captured_for_binding_is_dropped_on_every_way_out_of_the_iteration() {
    // `i` is assigned and captured, so it lives in a cell made per iteration. The closure is
    // never created at run time here, which is enough: the cell must still be dropped at
    // the end of the iteration, at `continue` and at `break`.
    let src = "fn main() {
    for mut i in 0..5 {
        if i > 100 { let f = fn() => print(i) }
        i += 1
        if i == 2 { continue }
        if i == 4 { break }
        print(i)
    }
    print(9)
}";
    assert_eq!(run_ok(src), "1\n3\n9\n");
}

#[test]
fn an_exhausted_budget_at_a_back_edge_names_the_loop_not_its_first_statement() {
    // The edge that runs out is the loop's (line 3); the statements it jumps to are on 4 and 5.
    let counted = "fn main() {
    let mut s = 0
    for i in 0..1000000 {
        s += i
        s += 1
    }
}";
    let unbounded = "fn main() {
    let mut n = 0
    while true {
        n += 1
        n += 2
    }
}";
    for src in [counted, unbounded] {
        let o = run_source_with(
            src,
            &Options {
                instruction_budget: Some(5_000),
                ..Options::default()
            },
        );
        assert_eq!(
            o.panic.as_deref(),
            Some("the instruction budget is exhausted")
        );
        assert_eq!(o.trace[0], ("main".to_string(), 3), "{:?}", o.trace);
    }
}

#[test]
fn the_hot_built_ins_are_instructions_and_the_others_go_through_core() {
    let src = "fn f(xs: List[int], s: str) -> int {
    xs.push(1)
    let n = xs.len() + s.len()
    let top = xs.pop()
    return n
}";
    let text = dis(src, "f");
    for want in ["ListPush ", "ListLen ", "StrLen ", "Builtin "] {
        assert!(text.contains(want), "{want} is missing:\n{text}");
    }
    // `pop` is the only one that is not an instruction of its own.
    assert_eq!(text.matches("Builtin ").count(), 1, "{text}");
    assert!(text.contains("Builtin(ListPop)/1"), "{text}");
}

#[test]
fn a_built_in_that_pauses_is_an_instruction_of_its_own_not_a_core_call() {
    let text = dis(
        "fn w() -> int => 1\nfn f() -> int {\n    let c = go w()\n    return c.join()\n}",
        "f",
    );
    for want in ["Go ", "Join "] {
        assert!(text.contains(want), "{want} is missing:\n{text}");
    }
    assert!(!text.contains("Builtin "), "{text}");
}

#[test]
fn built_ins_run_and_give_core_answers() {
    let out = run_ok(
        "fn main() {
    let xs = [3, 1, 2]
    xs.push(5)
    print(xs.len())
    print(xs.pop())
    print(\"héllo\".len())
    print(compare(1, 2))
    print(xs.contains(2))
}",
    );
    assert_eq!(out, "4\nsome(5)\n6\n-1\ntrue\n");
}

#[test]
fn the_closure_built_ins_are_calls_of_functions_written_in_colibri() {
    let src = "fn f(xs: List[int], o: int?) -> int {
    let a = xs.map(fn(x) => x + 1)
    let b = o.map(fn(x) => x + 1)
    let mut c = xs.clone()
    c.sort_by(fn(x, y) => x - y)
    return a.len()
}";
    let text = dis(src, "f");
    // A call of a function, not an instruction of core's built-ins. Three of them: `map`,
    // `Option.map` and `sort_by`; the others (`clone`, `len`) are not closure built-ins.
    assert!(!text.contains("Builtin(ListMap)"), "{text}");
    assert!(!text.contains("Builtin(OptionMap)"), "{text}");
    assert!(!text.contains("Builtin(ListSortBy)"), "{text}");
    assert!(text.contains("Builtin(Clone)"), "{text}");
    assert_eq!(text.matches("CallHidden r").count(), 3, "{text}");
    // They run as bytecode, so the closure is called by `CallValue` from a function of the prelude.
    let (_, text) = compile_source(src)
        .expect("compiles")
        .into_iter()
        .find(|(n, _)| n == "__list_map")
        .expect("the prelude function is compiled with the program");
    assert!(text.contains("CallValue"), "{text}");
    assert!(text.contains("ListPush"), "{text}");
}

#[test]
fn closure_built_ins_give_the_answers_of_the_native_ones() {
    let out = run_ok(
        "fn main() {
    let xs = [5, 2, 8, 1]
    print(xs.map(fn(x) => x * 10))
    print(xs.filter(fn(x) => x > 1))
    print(xs.any(fn(x) => x == 8))
    print(xs.all(fn(x) => x < 8))
    print(xs.find(fn(x) => x < 3))
    print(xs.find_index(fn(x) => x < 3))
    print(xs.fold(100, fn(a, x) => a - x))
    print(xs.flat_map(fn(x) => [x, x]))
    print(xs.sorted_by(fn(a, b) => b - a))
    let mut ys = xs.clone()
    ys.sort_by_key(fn(x) => 0 - x)
    print(ys)
    ys.retain(fn(x) => x % 2 == 0)
    print(ys)
    xs.for_each(fn(x) => print(x))
    let none_int: int? = none
    print(none_int.map(fn(x) => x + 1))
    print(some(1).and_then(fn(x) => some(x + 1)))
}",
    );
    assert_eq!(
        out,
        "[50, 20, 80, 10]\n[5, 2, 8]\ntrue\nfalse\nsome(2)\nsome(1)\n84\n[5, 5, 2, 2, 8, 8, 1, 1]\n[8, 5, 2, 1]\n[8, 5, 2, 1]\n[8, 2]\n5\n2\n8\n1\nnone\nsome(2)\n"
    );
}

#[test]
fn wait_until_is_a_bytecode_loop_with_a_check_before_it() {
    let text = dis("fn f(done: fn() -> bool) {\n    wait_until(done)\n}", "f");
    // The check that may refuse a pause, then: run the condition, leave when it holds, pause,
    // go round again.
    let at = |what: &str| {
        text.find(what)
            .unwrap_or_else(|| panic!("{what} is missing:\n{text}"))
    };
    assert!(at("Intrinsic") < at("CallValue"), "{text}");
    assert!(at("CallValue") < at("JmpIf"), "{text}");
    assert!(at("JmpIf") < at("SuspendReady"), "{text}");
    assert!(at("SuspendReady") < at("Loop"), "{text}");
}

#[test]
fn wait_until_outside_a_coroutine_panics_before_it_runs_the_condition() {
    let o = run_panic(
        "fn main() {\n    wait_until(fn() {\n        print(\"asked\")\n        return true\n    })\n}",
    );
    assert_eq!(o.stdout, "");
    assert_eq!(
        o.panic.as_deref(),
        Some(
            "`wait_until` can only pause a coroutine, but this code is not running in one (start one with `go`)"
        )
    );
}
