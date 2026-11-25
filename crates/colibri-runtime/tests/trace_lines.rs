//! The lines of a panic's stack trace are right.

use colibri_runtime::{Error, Vm};

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
    assert_eq!(vm.last(), Some(&("<script>".to_string(), 4)), "{vm:?}");
    assert_eq!(vm.first(), Some(&("boom".to_string(), 2)), "{vm:?}");
}

/// The whole panic of a run: message, position and every frame of the stack, with columns.
fn panic_of(
    source: &str,
) -> (
    String,
    Option<colibri_runtime::Location>,
    Vec<colibri_runtime::StackFrame>,
) {
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

fn stack_of_panic(source: &str) -> Vec<colibri_runtime::StackFrame> {
    panic_of(source).2
}

#[test]
fn a_panic_inside_a_map_closure_has_the_right_stack() {
    let stack = stack_of_panic(
        "fn check(x: int) -> int => 10 / (x - 3)
fn main() {
    let xs = [1, 2, 3]
    let ys = xs.map(fn(x) => check(x))
    print(ys)
}",
    );
    // The closure, `check`, and `main` at the call of `map`; nothing of `map` itself.
    let names: Vec<&str> = stack.iter().map(|f| f.function.as_str()).collect();
    assert_eq!(names.len(), 3, "{stack:?}");
    assert_eq!(names[0], "check");
    assert_eq!(names[2], "main");
    assert_eq!(stack[2].location.line, 4);
}

#[test]
fn a_panic_inside_every_kind_of_closure_built_in_has_the_right_stack() {
    for call in [
        "xs.filter(fn(x) => 1 / (x - 2) > 0)",
        "xs.retain(fn(x) => 1 / (x - 2) > 0)",
        "xs.any(fn(x) => 1 / (x - 2) > 5)",
        "xs.all(fn(x) => 1 / (x - 2) > -5)",
        "xs.find(fn(x) => 1 / (x - 2) > 5)",
        "xs.find_index(fn(x) => 1 / (x - 2) > 5)",
        "xs.fold(0, fn(a, x) => a + 1 / (x - 2))",
        "xs.for_each(fn(x) => print(1 / (x - 2)))",
        "xs.flat_map(fn(x) => [1 / (x - 2)])",
        "xs.sort_by(fn(a, b) => a - 1 / (b - 2))",
        "xs.sorted_by(fn(a, b) => a - 1 / (b - 2))",
        "xs.sort_by_key(fn(x) => 1 / (x - 2))",
        "some(2).map(fn(x) => 1 / (x - 2))",
        "some(2).and_then(fn(x) => some(1 / (x - 2)))",
    ] {
        let stack = stack_of_panic(&format!(
            "fn main() {{\n    let mut xs = [3, 1, 2]\n    print({call})\n}}"
        ));
        let names: Vec<&str> = stack.iter().map(|f| f.function.as_str()).collect();
        assert_eq!(names.len(), 2, "{call}: {stack:?}");
        assert_eq!(names[1], "main", "{call}");
        assert_eq!(stack[1].location.line, 3, "{call}");
    }
}

#[test]
fn a_panic_of_the_built_in_itself_is_reported_at_its_call() {
    // The guard of the list is checked by code that the VM writes in Colibri; the panic names
    // the line of the call, with no frame of that code.
    let stack = stack_of_panic(
        "fn main() {
    let mut xs = [2, 1]
    for x in xs {
        xs.sort_by_key(fn(y) => y)
    }
}",
    );
    assert_eq!(stack.len(), 1, "{stack:?}");
    assert_eq!(stack[0].function, "main");
    assert_eq!(stack[0].location.line, 4);
}
