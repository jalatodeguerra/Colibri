//! The lines of a panic's stack trace agree between the engines.

use colibri_runtime::{EngineKind, Error, Vm};

fn trace_of(kind: EngineKind, source: &str) -> Vec<(String, u32)> {
    let mut vm = Vm::builder().engine(kind).build();
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
    let tree = trace_of(EngineKind::Tree, src);
    let vm = trace_of(EngineKind::Bytecode, src);
    // The script's own frame is at the `let`; before the fix the tree-walker said line 1.
    assert_eq!(tree.last(), Some(&("<script>".to_string(), 4)), "{tree:?}");
    assert_eq!(tree.first(), Some(&("boom".to_string(), 2)), "{tree:?}");
    assert_eq!(vm, tree);
}
