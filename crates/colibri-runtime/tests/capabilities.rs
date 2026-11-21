//! Capabilities (EMBEDDING §5): what scripts may reach is granted by the host.

use colibri::{Cap, CapSet};
use colibri_runtime as colibri;

fn vm_with(caps: &[Cap]) -> (colibri::Vm, colibri::SharedBuffer) {
    let out = colibri::SharedBuffer::new();
    let mut b = colibri::Vm::builder().stdout(Box::new(out.clone()));
    for cap in caps {
        b = b.allow(*cap);
    }
    let mut vm = b.build();
    vm.register_fn("read_save", |slot: i64| format!("slot {slot}"))
        .unwrap()
        .requires(Cap::Fs);
    vm.register_fn("tick", || 1i64).unwrap();
    (vm, out)
}

const SCRIPT: &str = "fn main() {\n    print(read_save(2))\n}\n";

#[test]
fn a_granted_capability_lets_the_call_run() {
    let (mut vm, out) = vm_with(&[Cap::Fs]);
    vm.load_source("a.colibri", SCRIPT).unwrap();
    vm.run_main().unwrap();
    assert_eq!(out.contents(), "slot 2\n");
}

#[test]
fn allow_all_grants_every_capability() {
    let out = colibri::SharedBuffer::new();
    let mut vm = colibri::Vm::builder()
        .stdout(Box::new(out.clone()))
        .allow_all()
        .build();
    vm.register_fn("read_save", |slot: i64| slot)
        .unwrap()
        .requires(Cap::Net);
    vm.load_source("a.colibri", "fn main() {\n    print(read_save(5))\n}\n")
        .unwrap();
    vm.run_main().unwrap();
    assert_eq!(out.contents(), "5\n");
}

#[test]
fn an_ungranted_call_is_a_checker_error_at_the_use_site() {
    let (mut vm, _) = vm_with(&[]);
    let err = vm.load_source("a.colibri", SCRIPT).unwrap_err();
    let text = err.render(false);
    assert!(
        text.contains("`read_save` needs the `fs` capability, which this host did not grant"),
        "{text}"
    );
    assert!(text.contains("a.colibri:2:"), "{text}");
}

#[test]
fn a_function_with_no_requirement_is_unaffected_by_the_missing_grant() {
    let (mut vm, out) = vm_with(&[]);
    vm.load_source("a.colibri", "fn main() {\n    print(tick())\n}\n")
        .unwrap();
    vm.run_main().unwrap();
    assert_eq!(out.contents(), "1\n");
}

#[test]
fn another_capability_does_not_stand_in() {
    let (mut vm, _) = vm_with(&[Cap::Net, Cap::Os]);
    assert!(vm.load_source("a.colibri", SCRIPT).is_err());
}

#[test]
fn extern_blocks_need_the_ffi_capability() {
    let src = "extern \"C\" {\n    fn abs(x: int) -> int\n}\nfn main() {}\n";
    let (mut vm, _) = vm_with(&[]);
    let err = vm.load_source("a.colibri", src).unwrap_err();
    assert!(
        err.render(false)
            .contains("`extern` blocks need the `ffi` capability, which this host did not grant"),
        "{}",
        err.render(false)
    );
}

#[test]
fn cap_names_and_sets() {
    assert_eq!(Cap::Fs.to_string(), "fs");
    assert_eq!(Cap::Ffi.name(), "ffi");
    let set: CapSet = [Cap::Fs, Cap::Net].into_iter().collect();
    assert!(set.contains(Cap::Fs));
    assert!(!set.contains(Cap::Os));
    assert!(!CapSet::none().contains(Cap::Fs));
    assert!(CapSet::all().contains(Cap::Ffi));
}
