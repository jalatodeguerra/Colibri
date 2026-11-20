//! Two different Rust types with one Colibri name are an error, in whatever module and order
//! they are registered: the same shape does not make them one type, only the same Rust type
//! does. A host class is the type of the module that described it first, and is refused when
//! another module (or the default module) registers it, in every order.

use colibri_runtime as colibri;
use colibri_runtime::{HostType, TypeBuilder};

mod p1 {
    use colibri_runtime as colibri;
    #[colibri::export(value)]
    #[derive(Clone, Copy, Debug, PartialEq)]
    pub struct Pos {
        pub x: i64,
    }
}

mod p2 {
    use colibri_runtime as colibri;
    #[colibri::export(value)]
    #[derive(Clone, Copy, Debug, PartialEq)]
    pub struct Pos {
        pub x: i64,
    }
}

pub struct Holder;

impl HostType for Holder {
    const NAME: &'static str = "Holder";
    fn describe(d: &mut TypeBuilder<'_, Self>) {
        d.method("n", [], |_: &Holder| 1i64);
    }
}

fn new_vm() -> colibri::Vm {
    colibri::Vm::builder().build()
}

/// The error of a registration that must fail, as text.
fn refused<T>(r: Result<T, colibri::Error>, what: &str) -> String {
    match r {
        Ok(_) => panic!("{what}: accepted"),
        Err(e) => e.to_string(),
    }
}

/// The message names both Rust types (`...p1::Pos` and `...p2::Pos`).
fn names_both(message: &str, what: &str) {
    assert!(
        message.contains("p1::Pos") && message.contains("p2::Pos"),
        "{what}: {message}"
    );
}

#[test]
fn two_rust_types_of_one_name_in_one_module_are_refused_in_every_order() {
    // The same module, one after the other.
    let mut vm = new_vm();
    vm.module("a").register_type::<p1::Pos>().unwrap();
    let m = refused(vm.module("a").register_type::<p2::Pos>(), "a then a");
    names_both(&m, "same module");

    // The module shares the first (one owner) and registers the other.
    let mut vm = new_vm();
    vm.module("a").register_type::<p1::Pos>().unwrap();
    vm.module("b").register_type::<p1::Pos>().unwrap();
    let m = refused(
        vm.module("b").register_type::<p2::Pos>(),
        "shared then other",
    );
    names_both(&m, "shared then other");

    // `b` owns the other type and then registers the shared one.
    let mut vm = new_vm();
    vm.module("b").register_type::<p2::Pos>().unwrap();
    vm.module("a").register_type::<p1::Pos>().unwrap();
    let m = refused(vm.module("b").register_type::<p1::Pos>(), "other first");
    names_both(&m, "other first");

    // The same, with the owner of the shared type sorting after `b`.
    let mut vm = new_vm();
    vm.module("b").register_type::<p2::Pos>().unwrap();
    vm.module("c").register_type::<p1::Pos>().unwrap();
    let m = refused(vm.module("b").register_type::<p1::Pos>(), "owner moves");
    names_both(&m, "owner moves");

    // The default module and a module.
    let mut vm = new_vm();
    vm.register_type::<p1::Pos>().unwrap();
    vm.module("b").register_type::<p1::Pos>().unwrap();
    let m = refused(vm.module("b").register_type::<p2::Pos>(), "default");
    names_both(&m, "default");
    let mut vm = new_vm();
    vm.register_type::<p1::Pos>().unwrap();
    let m = refused(vm.register_type::<p2::Pos>(), "default twice");
    names_both(&m, "default twice");
}

#[test]
fn one_rust_type_in_a_module_twice_and_in_several_modules_is_fine() {
    let mut vm = new_vm();
    vm.module("a").register_type::<p1::Pos>().unwrap();
    vm.module("a").register_type::<p1::Pos>().unwrap();
    vm.module("b").register_type::<p1::Pos>().unwrap();
    vm.register_type::<p1::Pos>().unwrap();
    // Another type of the same name in a module that has neither is its own type.
    vm.module("z").register_type::<p2::Pos>().unwrap();
}

#[test]
fn a_host_class_is_refused_by_any_other_module_in_every_order() {
    type Order = fn(&mut colibri::Vm) -> Result<(), colibri::Error>;
    let orders: [(&str, Order); 4] = [
        ("a then b", |vm| {
            vm.module("a").register_type::<Holder>()?;
            vm.module("b").register_type::<Holder>()
        }),
        ("b then a", |vm| {
            vm.module("b").register_type::<Holder>()?;
            vm.module("a").register_type::<Holder>()
        }),
        ("default then a", |vm| {
            vm.register_type::<Holder>()?;
            vm.module("a").register_type::<Holder>()
        }),
        ("a then default", |vm| {
            vm.module("a").register_type::<Holder>()?;
            vm.register_type::<Holder>()
        }),
    ];
    for (what, run) in orders {
        let mut vm = new_vm();
        let m = refused(run(&mut vm), what);
        assert!(
            m.contains("Holder") && m.contains("described it first"),
            "{what}: {m}"
        );
    }
    // Registered twice by its own module is nothing new.
    let mut vm = new_vm();
    vm.register_type::<Holder>().unwrap();
    vm.register_type::<Holder>().unwrap();
    vm.module("a")
        .register_type::<Holder>()
        .expect_err("another module");
}
