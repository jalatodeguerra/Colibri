//! Without the counting allocator (the CLI, and any host that did not install it) a `Vm` reports
//! no memory figure and entries cost nothing extra (phase 3 L13).

use colibri_runtime as colibri;

#[test]
fn memory_used_is_none_without_the_allocator() {
    let mut vm = colibri::Vm::builder().build();
    vm.load_source("m.colibri", "fn f() -> int => 1").unwrap();
    let f = vm.get_fn::<(), i64>("f").unwrap();
    assert_eq!(f.call(&mut vm, ()).unwrap(), 1);
    assert_eq!(vm.memory_used(), None);
}
