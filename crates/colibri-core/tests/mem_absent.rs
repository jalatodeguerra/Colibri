//! Without the allocator installed (the default, and the CLI): accounts are inert, nothing is
//! counted, and a feature that needs the counts fails with the way to install it.

use colibri_core::Core;
use colibri_core::mem::{installed, require_installed};

#[test]
fn nothing_is_installed_or_counted() {
    assert!(!installed());
    let mut core = Core::new(Box::new(std::io::sink()));
    assert_eq!(core.memory_used(), None);
    assert!(!core.mem.is_live());
    let outermost = core.begin_entry();
    let _v: Vec<u8> = vec![0; 1000];
    core.end_entry(outermost, false);
    assert_eq!(core.memory_used(), None);
    core.set_memory_limit(Some(1));
    assert!(!core.memory_over());
}

#[test]
fn the_check_says_how_to_install_it() {
    let err = std::panic::catch_unwind(|| require_installed("a memory limit")).unwrap_err();
    let text = err
        .downcast_ref::<String>()
        .cloned()
        .expect("a formatted message");
    assert!(
        text.starts_with("a memory limit needs the counting allocator"),
        "{text}"
    );
    assert!(text.contains("#[global_allocator]"), "{text}");
    assert!(text.contains("colibri::CountingAlloc"), "{text}");
}
