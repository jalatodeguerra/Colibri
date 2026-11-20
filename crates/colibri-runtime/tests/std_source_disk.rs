//! An import inside a standard module written in Colibri names the standard library only: the
//! virtual tree `<std>` is never looked up on disk. This file holds one test because it changes
//! the working directory of the process.

/// `<` and `>` cannot be in a file name on Windows, so there the folder below cannot exist and
/// nothing could be mistaken for the virtual tree; the test then has nothing to show.
#[cfg(unix)]
#[test]
fn a_folder_named_like_the_virtual_tree_is_not_the_library() {
    use colibri_runtime as colibri;

    let dir = std::env::temp_dir().join(format!("colibri-std-disk-{}", std::process::id()));
    let decoy = dir.join("<std>");
    std::fs::create_dir_all(&decoy).unwrap();
    // If this file were read, `inner.one()` would be 666 or the import would be ambiguous.
    std::fs::write(decoy.join("inner.colibri"), "pub fn one() -> int => 666\n").unwrap();
    let main = dir.join("main.colibri");
    std::fs::write(&main, "import outer\nprint(outer.two())\n").unwrap();
    std::env::set_current_dir(&dir).unwrap();

    let out = colibri::SharedBuffer::new();
    let mut vm = colibri::Vm::builder().stdout(Box::new(out.clone())).build();
    vm.std_source_module("inner", "pub fn one() -> int => 1\n");
    vm.std_source_module(
        "outer",
        "import inner\npub fn two() -> int => inner.one() + 1\n",
    );
    vm.load_file(&main).unwrap();
    assert_eq!(out.contents(), "2\n");
    std::fs::remove_dir_all(&dir).unwrap();
}
