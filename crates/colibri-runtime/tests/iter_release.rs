//! Built-in iterators let go of their collection (or inner iterator) when they run out of
//! items: the objects only the iterator held must be released, so their `Drop` hooks run, and
//! not dropped by Rust (a debug build stops on that).

use colibri_runtime as colibri;

const GUARD: &str = r#"
class Guard {
    name: str
}
impl Drop for Guard {
    fn drop(self) {
        print("drop {self.name}")
    }
}
"#;

fn run(body: &str) -> String {
    let out = colibri::SharedBuffer::new();
    let mut vm = colibri::Vm::builder().stdout(Box::new(out.clone())).build();
    vm.load_source("iter.colibri", &format!("{GUARD}{body}"))
        .unwrap();
    vm.run_main().unwrap();
    out.contents()
}

#[test]
fn a_for_loop_over_the_only_reference_to_a_list_releases_it_at_the_end() {
    let out = run(r#"
fn make() -> List[Guard] {
    return [Guard { name: "a" }]
}
fn main() {
    let it = make().iter()
    for g in it {
        print("got {g.name}")
    }
    print("end")
}
"#);
    assert_eq!(out, "got a\ndrop a\nend\n");
}

#[test]
fn a_map_iterator_advanced_with_next_releases_its_map_at_the_end() {
    let out = run(r#"
fn make() -> Map[str, Guard] {
    return {"k": Guard { name: "m" }}
}
fn main() {
    var it = make().iter()
    while true {
        match it.next() {
            some(p) => print("got {p.1.name}"),
            none => break,
        }
    }
    print("end")
}
"#);
    assert_eq!(out, "got m\ndrop m\nend\n");
}
