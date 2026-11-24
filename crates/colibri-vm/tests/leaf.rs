//! Leaf closures run by native loops (`crate::leaf`): the same result, panic, trace and budget as
//! the prelude's code in bytecode, which `Options::no_leaf` runs instead.

use colibri_vm::{Options, Outcome, run_source_with};

fn run(src: &str, no_leaf: bool, budget: Option<u64>, depth: usize) -> Outcome {
    run_source_with(
        src,
        &Options {
            max_call_depth: depth,
            instruction_budget: budget.or(Some(50_000_000)),
            trace_drops: false,
            no_leaf,
        },
    )
}

/// The program with the native loops and without: everything the run shows must agree.
fn same(src: &str, budget: Option<u64>, depth: usize) -> (Outcome, Outcome) {
    let (fast, slow) = (
        run(src, false, budget, depth),
        run(src, true, budget, depth),
    );
    assert_eq!(slow.leaf_runs, 0, "{src}");
    assert_eq!(fast.stdout, slow.stdout, "stdout, budget {budget:?}: {src}");
    assert_eq!(fast.panic, slow.panic, "panic, budget {budget:?}: {src}");
    assert_eq!(fast.trace, slow.trace, "trace, budget {budget:?}: {src}");
    assert_eq!(fast.steps, slow.steps, "steps, budget {budget:?}: {src}");
    (fast, slow)
}

/// A program that prints the value of one expression over a few lists.
fn program(call: &str) -> String {
    format!(
        "struct P {{ x: float, n: int, on: bool, name: str }}
fn helper(x: int) -> int => x + 1
fn main() {{
    let xs = [3, 1, 4, 1, 5, 9, 2, 6]
    let fs = [1.5, -2.0, 0.5, 8.0]
    let bs = [true, false, true, true]
    let ps = [P {{ x: 1.0, n: 2, on: true, name: \"a\" }}, P {{ x: -1.0, n: 3, on: false, name: \"b\" }}]
    let k = 10
    print({call})
}}"
    )
}

/// Calls whose closure is a leaf: each runs natively once.
const LEAF: &[&str] = &[
    "xs.map(fn(x) => x * 2)",
    "xs.map(fn(x) => x)",
    "xs.map(fn(x) => x > 3)",
    "xs.map(fn(x) => -x + 1)",
    "xs.map(fn(x) => x / 2 + x % 3)",
    "xs.map(fn(x) => if x > 3 { x } else { 0 - x })",
    "xs.map(fn(x) => x > 2 && x < 6 || x == 9)",
    "xs.filter(fn(x) => x % 2 == 1)",
    "xs.filter(fn(x) => x >= 3 && x != 5)",
    "xs.any(fn(x) => x == 9)",
    "xs.any(fn(x) => x > 100)",
    "xs.all(fn(x) => x > 0)",
    "xs.all(fn(x) => x < 6)",
    "xs.find(fn(x) => x > 4)",
    "xs.find(fn(x) => x > 40)",
    "xs.find_index(fn(x) => x > 4)",
    "xs.find_index(fn(x) => x > 40)",
    "xs.fold(0, fn(a, x) => a + x)",
    "xs.fold(1, fn(a, x) => a * (x + 1))",
    "xs.fold(0, fn(a, x) => if x > a { x } else { a })",
    "fs.map(fn(x) => x * 2.0 - 1.0)",
    "fs.map(fn(x) => x / 0.0)",
    "fs.filter(fn(x) => x > 0.0)",
    "fs.fold(0.0, fn(a, x) => a + x * x)",
    "fs.any(fn(x) => x != x)",
    "bs.map(fn(b) => !b)",
    "bs.filter(fn(b) => b)",
    "bs.all(fn(b) => b)",
    "bs.find_index(fn(b) => !b)",
    "bs.fold(0, fn(n, b) => if b { n + 1 } else { n })",
    "ps.map(fn(p) => p.x * 2.0)",
    "ps.map(fn(p) => p.n + 1)",
    "ps.map(fn(p) => p.on)",
    "ps.map(fn(p) => p.name)",
    "ps.filter(fn(p) => p.on).map(fn(p) => p.name)",
    "ps.find(fn(p) => p.on)",
    "ps.map(fn(p) => p).len()",
    "ps.filter(fn(p) => p.on).len()",
    "ps.any(fn(p) => p.n > 2)",
    "ps.find_index(fn(p) => p.x < 0.0)",
    "ps.fold(0, fn(a, p) => a + p.n)",
    "xs.map(fn(x) { let y = x * x
        return y + 1 })",
    "xs.filter(fn(x) { if x > 3 {
            return true
        }
        return x == 1 })",
    "xs.fold(0, fn(a, x) { let t = a + x
        if t > 20 {
            return t - 20
        }
        return t })",
    "xs.find(fn(x) { let sq = x * x
        return sq > 30 })",
    "xs.map(fn(x) { let mut y = x
        y += 1
        return y })",
];

/// Calls whose closure is not a leaf: the prelude runs them.
const NOT_LEAF: &[&str] = &[
    "xs.map(fn(x) => x + k)",
    "xs.map(fn(x) => helper(x))",
    "xs.map(fn(x) => \"{x}\")",
    "xs.map(fn(x) => [x, x])",
    "xs.map(fn(x) => (x, x))",
    "xs.map(fn(x) => xs.filter(fn(y) => y < x).len())",
    "xs.filter(fn(x) { print(x)
        return x > 3 })",
    "xs.filter(fn(x) => x > k - 7)",
    "xs.any(fn(x) => helper(x) == 6)",
    "xs.fold(0, fn(a, x) => a + helper(x))",
    "ps.map(fn(p) => if p.on { p.name } else { \"x\" })",
    "ps.map(fn(p) => if p.on { p.n } else { p.n * 2 } + k)",
    "ps.map(fn(p) => helper(p.n))",
    "xs.map(helper)",
    "xs.find(fn(x) => (x, 1).0 > 4)",
];

/// Calls that fail inside the closure at some element: the prelude reports it.
const FAILING: &[&str] = &[
    "xs.map(fn(x) => x * 4611686018427387904)",
    "xs.map(fn(x) => 100 / (x - 4))",
    "xs.filter(fn(x) => 7 % (x - 5) > 0)",
    "xs.any(fn(x) => 10 / (x - 9) > 100)",
    "xs.all(fn(x) => 10 / (x - 9) > -100)",
    "xs.find(fn(x) => 10 / (x - 9) > 100)",
    "xs.find_index(fn(x) => 10 / (x - 9) > 100)",
    "xs.fold(0, fn(a, x) => a + 10 % (x - 9))",
    "xs.fold(1, fn(a, x) => a * x * 1000000000)",
    "xs.map(fn(x) => -x - 9223372036854775807)",
];

#[test]
fn leaf_closures_run_natively_and_say_what_the_prelude_says() {
    for call in LEAF {
        let src = program(call);
        let (fast, _) = same(&src, None, 10_000);
        assert!(fast.panic.is_none(), "{call}: {:?}", fast.panic);
        let calls = [
            "map(fn",
            "filter(fn",
            "any(fn",
            "all(fn",
            "find(fn",
            "find_index(fn",
            "fold(",
        ]
        .iter()
        .map(|m| call.matches(m).count() as u64)
        .sum::<u64>();
        assert_eq!(fast.leaf_runs, calls, "{call} should run in native loops");
    }
}

#[test]
fn other_closures_run_in_the_prelude_as_before() {
    for call in NOT_LEAF {
        let src = program(call);
        let (fast, _) = same(&src, None, 10_000);
        assert!(fast.panic.is_none(), "{call}: {:?}", fast.panic);
        assert_eq!(fast.leaf_runs, 0, "{call}");
    }
}

#[test]
fn a_failure_inside_a_leaf_closure_is_the_prelude_s_with_its_message_and_trace() {
    for call in FAILING {
        let src = program(call);
        let (fast, _) = same(&src, None, 10_000);
        let message = fast.panic.as_deref().expect("it panics");
        assert!(
            message.contains("division by zero")
                || message.contains("modulo by zero")
                || message.contains("integer overflow"),
            "{call}: {message}"
        );
        // The closure's frame, and `main` at the call of the built-in.
        let names: Vec<&str> = fast.trace.iter().map(|(f, _)| f.as_str()).collect();
        assert_eq!(names, ["<closure in main>", "main"], "{call}");
        assert_eq!(fast.stdout, "", "{call}");
        assert_eq!(fast.leaf_runs, 0, "{call}");
    }
}

/// Every budget from nothing to a little over what the run needs: the same outcome, the same step
/// count, the same place of the failure, with the native loops and without.
#[test]
fn a_budget_that_runs_out_anywhere_stops_where_the_prelude_stops() {
    for call in LEAF
        .iter()
        .chain(FAILING)
        .chain(["xs.map(fn(x) => x + k)"].iter())
    {
        let src = program(call);
        let full = run(&src, true, None, 10_000);
        for budget in 0..=full.steps + 2 {
            same(&src, Some(budget), 10_000);
        }
    }
}

/// With exactly the budget the run needs the native loop is used; one step less and it declines,
/// and the prelude fails at the place it always failed.
#[test]
fn the_native_loop_needs_the_whole_budget_up_front_and_charges_exactly_what_the_calls_did() {
    for call in [
        "xs.map(fn(x) => x * 2)",
        "xs.filter(fn(x) => x % 2 == 1)",
        "xs.fold(0, fn(a, x) => a + x)",
        "fs.map(fn(x) => x * 2.0)",
        "ps.map(fn(p) => p.x * 2.0)",
        "xs.map(fn(x) { let y = x * x
            return y + 1 })",
    ] {
        let src = program(call);
        let full = run(&src, true, None, 10_000);
        let exact = run(&src, false, Some(full.steps), 10_000);
        assert!(exact.panic.is_none(), "{call}: {:?}", exact.panic);
        assert_eq!(exact.leaf_runs, 1, "{call}");
        assert_eq!(exact.steps, full.steps, "{call}");
        let short = run(&src, false, Some(full.steps - 1), 10_000);
        assert!(short.panic.is_some(), "{call}");
        assert_eq!(short.leaf_runs, 0, "{call}");
    }
}

/// The closure's frame would be one past the call limit: the prelude refuses it, at the call.
#[test]
fn the_call_limit_is_met_at_the_closure_call_as_before() {
    let src = |down: usize| {
        format!(
            "fn dive(n: int) -> int {{
    if n == 0 {{
        let xs = [1, 2, 3]
        return xs.map(fn(x) => x + 1).len()
    }}
    return dive(n - 1)
}}
fn main() {{
    print(dive({down}))
}}"
        )
    };
    let (mut over, mut fine) = (0, 0);
    for depth in 2..=8 {
        for down in 0..=8 {
            let (fast, _) = same(&src(down), None, depth);
            if fast.panic.is_some() {
                over += 1;
                assert!(
                    fast.panic.as_deref().unwrap().contains("stack overflow"),
                    "{:?}",
                    fast.panic
                );
            } else {
                fine += 1;
                assert_eq!(fast.leaf_runs, 1, "depth {depth}, {down} calls down");
            }
        }
    }
    assert!(over > 0 && fine > 0, "{over} {fine}");
}
