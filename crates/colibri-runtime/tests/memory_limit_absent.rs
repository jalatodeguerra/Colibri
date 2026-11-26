//! A memory limit on a program that did not install the counting allocator fails at once, with
//! the way to install it (phase 3 L14). This binary never installs it, so it is the one that can
//! show that.

use colibri_runtime as colibri;

fn message_of(f: impl FnOnce() + std::panic::UnwindSafe) -> String {
    let err = std::panic::catch_unwind(f).expect_err("the allocator is not installed");
    err.downcast_ref::<String>()
        .cloned()
        .expect("a formatted message")
}

#[test]
fn the_builder_panics_with_the_way_to_install_the_allocator() {
    let text = message_of(|| {
        let _ = colibri::Vm::builder().memory_limit(64 << 20).build();
    });
    assert!(
        text.starts_with("a memory limit needs the counting allocator"),
        "{text}"
    );
    assert!(text.contains("#[global_allocator]"), "{text}");
    assert!(
        text.contains("colibri::CountingAlloc<std::alloc::System>"),
        "{text}"
    );
}

#[test]
fn the_setter_panics_too_and_lifting_a_limit_needs_nothing() {
    let mut vm = colibri::Vm::builder().build();
    let text = message_of(std::panic::AssertUnwindSafe(|| {
        vm.set_memory_limit(Some(1 << 20))
    }));
    assert!(text.contains("colibri::CountingAlloc"), "{text}");
    vm.set_memory_limit(None);
    vm.load_source("f.colibri", "fn f() -> int => 1").unwrap();
    assert_eq!(
        vm.get_fn::<(), i64>("f")
            .unwrap()
            .call(&mut vm, ())
            .unwrap(),
        1
    );
}
