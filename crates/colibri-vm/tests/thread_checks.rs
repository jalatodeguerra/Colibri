//! The words of the checker's thread-task errors that a golden cannot pin (hints and labels).

use colibri_syntax::{Diagnostic, FileId, parse_file};

fn errors(src: &str) -> Vec<Diagnostic> {
    let (ast, _) = parse_file(FileId(0), src);
    colibri_check::check(&ast)
        .diagnostics
        .into_iter()
        .filter(Diagnostic::is_error)
        .collect()
}

#[test]
fn a_script_value_read_in_a_task_says_to_copy_it_into_a_local_first() {
    let found =
        errors("let data = [1, 2]\nlet t = task.run(fn() => data.len())\nprint(t.done())\n");
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(
        found[0].message,
        "`closure -> data` reads `data`, which belongs to the main thread"
    );
    let hint = found[0].hint.as_deref().unwrap_or_default();
    assert!(
        hint.contains("copy it into a local inside a block or a function first"),
        "{hint}"
    );
    assert!(
        found[0]
            .labels
            .iter()
            .any(|(_, l)| l == "`data` is read here"),
        "{found:?}"
    );
}

#[test]
fn a_write_is_reported_as_a_write_and_told_to_return_the_result() {
    let found = errors(
        "let mut hits = 0\nfn bump() {\n    hits += 1\n}\nfn main() {\n    print(task.run(bump).done())\n}\n",
    );
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(
        found[0].message,
        "`bump -> hits` writes `hits`, which belongs to the main thread"
    );
    let hint = found[0].hint.as_deref().unwrap_or_default();
    assert!(
        hint.contains("cannot change the main thread's values")
            && hint.contains("`t.join()`")
            && !hint.contains("const")
            && !hint.contains("copy it"),
        "{hint}"
    );
    assert!(
        found[0]
            .labels
            .iter()
            .any(|(_, l)| l == "`hits` is written here"),
        "{found:?}"
    );
}

#[test]
fn a_write_in_a_script_task_gets_the_write_hint_not_the_copy_hint() {
    let found =
        errors("let mut total = 0\nlet t = task.run(fn() { total += 1 })\nprint(t.done())\n");
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(
        found[0].message,
        "`closure -> total` writes `total`, which belongs to the main thread"
    );
    let hint = found[0].hint.as_deref().unwrap_or_default();
    assert!(hint.contains("Return what the task computed"), "{hint}");
    assert!(!hint.contains("clone") && !hint.contains("len"), "{hint}");
}

#[test]
fn the_hint_for_a_wrapped_part_keeps_the_wrapper() {
    let found = errors(
        "struct R { v: List[int] }\nfn f(ch: Channel[R?], r: R) {\n    ch.send(some(r))\n}\n",
    );
    assert_eq!(found.len(), 1, "{found:?}");
    let hint = found[0].hint.as_deref().unwrap_or_default();
    assert!(hint.contains("`r.clone()` where it is read"), "{hint}");
    assert!(hint.contains("inside `some(..)`"), "{hint}");
}
