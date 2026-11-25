//! The lines of a panic's stack trace are right.

use colibri_runtime::{Error, Location, StackFrame, Vm};

fn trace_of(source: &str) -> Vec<(String, u32)> {
    let mut vm = Vm::builder().build();
    let result = vm
        .load_source("a.colibri", source)
        .and_then(|()| vm.run_main().map(|_| ()));
    match result {
        Err(Error::Panic { stack, .. }) => stack
            .into_iter()
            .map(|f| (f.function, f.location.line))
            .collect(),
        other => panic!("expected a panic, got {other:?}"),
    }
}

#[test]
fn a_panic_in_a_script_initialiser_names_the_line_of_its_let() {
    let src = "let a = 1
fn boom(n: int) -> int => 10 / n

let b = boom(a - 1)
";
    let vm = trace_of(src);
    // The script's own frame is at the `let`.
    let frame = |name: &str, line: u32| (name.to_string(), line);
    assert_eq!(
        vm,
        [frame("boom", 2), frame("<init b>", 4), frame("<script>", 4)]
    );
}

/// The whole panic of a run: message, position and every frame of the stack, with columns.
fn panic_of(source: &str) -> (String, Option<Location>, Vec<StackFrame>) {
    let mut vm = Vm::builder().build();
    let result = vm
        .load_source("a.colibri", source)
        .and_then(|()| vm.run_main().map(|_| ()));
    match result {
        Err(Error::Panic {
            message,
            location,
            stack,
            ..
        }) => (message, location, stack),
        other => panic!("expected a panic, got {other:?}"),
    }
}

fn loc(line: u32, column: u32) -> Location {
    Location {
        file: "a.colibri".to_string(),
        line,
        column,
    }
}

fn frame(function: &str, line: u32, column: u32) -> StackFrame {
    StackFrame {
        function: function.to_string(),
        location: loc(line, column),
    }
}

#[test]
fn a_panic_inside_a_map_closure_has_the_right_stack() {
    let panic = panic_of(
        "fn check(x: int) -> int => 10 / (x - 3)
fn main() {
    let xs = [1, 2, 3]
    let ys = xs.map(fn(x) => check(x))
    print(ys)
}",
    );
    // The closure, `check`, and `main` at the call of `map`; nothing of `map` itself.
    assert_eq!(
        panic,
        (
            "division by zero".to_string(),
            Some(loc(1, 28)),
            vec![
                frame("check", 1, 28),
                frame("<closure in main>", 4, 30),
                frame("main", 4, 14),
            ]
        )
    );
}

#[test]
fn a_panic_inside_every_kind_of_closure_built_in_has_the_right_stack() {
    // Each call, and the column of the division inside its closure.
    for (call, column) in [
        ("xs.filter(fn(x) => 1 / (x - 2) > 0)", 30),
        ("xs.retain(fn(x) => 1 / (x - 2) > 0)", 30),
        ("xs.any(fn(x) => 1 / (x - 2) > 5)", 27),
        ("xs.all(fn(x) => 1 / (x - 2) > -5)", 27),
        ("xs.find(fn(x) => 1 / (x - 2) > 5)", 28),
        ("xs.find_index(fn(x) => 1 / (x - 2) > 5)", 34),
        ("xs.fold(0, fn(a, x) => a + 1 / (x - 2))", 38),
        ("xs.for_each(fn(x) => print(1 / (x - 2)))", 38),
        ("xs.flat_map(fn(x) => [1 / (x - 2)])", 33),
        ("xs.sort_by(fn(a, b) => a - 1 / (b - 2))", 38),
        ("xs.sorted_by(fn(a, b) => a - 1 / (b - 2))", 40),
        ("xs.sort_by_key(fn(x) => 1 / (x - 2))", 35),
        ("some(2).map(fn(x) => 1 / (x - 2))", 32),
        ("some(2).and_then(fn(x) => some(1 / (x - 2)))", 42),
    ] {
        let panic = panic_of(&format!(
            "fn main() {{\n    let mut xs = [3, 1, 2]\n    print({call})\n}}"
        ));
        assert_eq!(
            panic,
            (
                "division by zero".to_string(),
                Some(loc(3, column)),
                vec![frame("<closure in main>", 3, column), frame("main", 3, 11)]
            ),
            "{call}"
        );
    }
}

#[test]
fn a_panic_of_the_built_in_itself_is_reported_at_its_call() {
    // The guard of the list is checked by code that the VM writes in Colibri; the panic names
    // the line of the call, with no frame of that code.
    let panic = panic_of(
        "fn main() {
    let mut xs = [2, 1]
    for x in xs {
        xs.sort_by_key(fn(y) => y)
    }
}",
    );
    assert_eq!(
        panic,
        (
            "list modified during iteration".to_string(),
            Some(loc(4, 9)),
            vec![frame("main", 4, 9)]
        )
    );
}
