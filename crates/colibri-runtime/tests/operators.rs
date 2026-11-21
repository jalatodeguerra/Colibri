//! Operators on user types, through the embedding API: where a panic inside an operator shows
//! in the trace, and that an operator impl is patched by a hot reload like any other function.

mod common;
use common::vm_pending;

use std::fs;
use std::path::PathBuf;

use colibri::{ReloadOutcome, Vm};
use colibri_runtime as colibri;

const SCRIPT: &str = r#"
struct V { x: int }

impl Div for V {
    fn div(self, o: V) -> V => V { x: self.x / o.x }
}

fn ratio(a: V, b: V) -> V => a / b

fn boom() -> int => ratio(V { x: 1 }, V { x: 0 }).x

fn main() {}
"#;

#[test]
fn a_panic_inside_an_operator_names_the_impl_in_the_trace() {
    vm_pending(
        "a_panic_inside_an_operator_names_the_impl_in_the_trace",
        || {
            let mut vm = Vm::builder().build();
            vm.load_source("ops.colibri", SCRIPT).unwrap();
            let boom = vm.get_fn::<(), i64>("boom").unwrap();
            let err = boom.call(&mut vm, ()).unwrap_err();
            let colibri::Error::Panic { message, stack, .. } = err else {
                panic!("expected a panic");
            };
            assert_eq!(message, "division by zero");
            let names: Vec<&str> = stack.iter().map(|f| f.function.as_str()).collect();
            assert_eq!(names.len(), 3, "{names:?}");
            assert!(names[0].ends_with("div"), "{names:?}");
            assert_eq!(&names[1..], ["ratio", "boom"], "{names:?}");
        },
    )
}

/// A fresh temporary folder holding one file.
fn tree(name: &str, text: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("colibri-operators-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("a.colibri"), text).unwrap();
    dir
}

#[test]
fn an_operator_impl_is_patched_by_a_hot_reload() {
    vm_pending("an_operator_impl_is_patched_by_a_hot_reload", || {
        let v1 = "struct V { x: int }\nimpl Add for V {\n    fn add(self, o: V) -> V => V { x: self.x + o.x }\n}\nfn sum(a: int, b: int) -> int => (V { x: a } + V { x: b }).x\n";
        let dir = tree("patch", v1);
        let mut vm = Vm::builder().build();
        vm.load_file(dir.join("a.colibri")).unwrap();
        let sum = vm.get_fn::<(i64, i64), i64>("sum").unwrap();
        assert_eq!(sum.call(&mut vm, (2, 3)).unwrap(), 5);
        // Whitespace and comments are not a change.
        fs::write(dir.join("a.colibri"), format!("// same\n{v1}\n")).unwrap();
        assert!(matches!(
            vm.reload_changed().unwrap(),
            ReloadOutcome::Unchanged
        ));
        // A new body for the impl's method is used by the next operator, through the same handle.
        fs::write(
            dir.join("a.colibri"),
            v1.replace("self.x + o.x", "self.x * o.x"),
        )
        .unwrap();
        match vm.reload_changed().unwrap() {
            ReloadOutcome::Patched { functions } => assert_eq!(functions, 1),
            other => panic!("{other:?}"),
        }
        assert_eq!(sum.call(&mut vm, (2, 3)).unwrap(), 6);
    })
}
