//! `Bytes` in the embedding API: a host function takes and returns a byte string without a
//! `List[u8]` in between (EMBEDDING §1).

use colibri_runtime as colibri;

fn vm() -> (colibri::Vm, colibri::SharedBuffer) {
    let out = colibri::SharedBuffer::new();
    let vm = colibri::Vm::builder().stdout(Box::new(out.clone())).build();
    (vm, out)
}

fn run(vm: &mut colibri::Vm, src: &str) -> Result<(), colibri::Error> {
    vm.load_source("game.colibri", src)?;
    vm.run_main()
}

#[test]
fn a_host_function_takes_and_returns_bytes() {
    let (mut vm, out) = vm();
    vm.register_fn("reverse", |b: colibri::Bytes| {
        let mut v = b.0;
        v.reverse();
        colibri::Bytes(v)
    })
    .unwrap();
    vm.register_fn("total", |b: colibri::Bytes| {
        b.iter().map(|&x| i64::from(x)).sum::<i64>()
    })
    .unwrap();
    // A plain `Vec<u8>` is still a list.
    vm.register_fn("as_list", |v: Vec<u8>| v.len() as i64)
        .unwrap();
    run(
        &mut vm,
        r#"
fn main() {
    print(reverse(Bytes([1, 2, 255])))
    print(total(Bytes([1, 2, 255])))
    print(as_list([1, 2, 3]))
    print(reverse(Bytes()).is_empty())
}
"#,
    )
    .unwrap();
    assert_eq!(out.contents(), "b\"\\xff\\x02\\x01\"\n258\n3\ntrue\n");
}

#[test]
fn the_interface_text_names_bytes() {
    let (mut vm, _) = vm();
    vm.register_fn("digest", |b: colibri::Bytes| b).unwrap();
    let text = vm.interface_text();
    assert!(text.contains("fn digest(a: Bytes) -> Bytes"), "{text}");
    let parsed = colibri::Interface::from_text("host.cbi", &text).unwrap();
    assert_eq!(parsed.canonical_text(), text);
}

#[test]
fn get_fn_checks_and_moves_bytes() {
    let (mut vm, _) = vm();
    vm.load_source(
        "a.colibri",
        "fn shout(b: Bytes) -> Bytes => b + \"!\".to_bytes()\nfn count(b: Bytes) -> int => b.len()\n",
    )
    .unwrap();
    let shout = vm
        .get_fn::<(colibri::Bytes,), colibri::Bytes>("shout")
        .unwrap();
    let got = shout
        .call(&mut vm, (colibri::Bytes(b"hi".to_vec()),))
        .unwrap();
    assert_eq!(got.0, b"hi!");
    // A list is not a `Bytes`.
    assert!(vm.get_fn::<(Vec<u8>,), i64>("count").is_err());
    let count = vm.get_fn::<(colibri::Bytes,), i64>("count").unwrap();
    assert_eq!(
        count
            .call(&mut vm, (colibri::Bytes(vec![0; 100_000]),))
            .unwrap(),
        100_000
    );
}

#[test]
fn a_value_of_bytes_has_its_own_kind() {
    assert!(matches!(
        colibri::Value::bytes(&[1, 2]).kind(),
        colibri::ValueKind::Bytes(b) if b == [1, 2]
    ));
}

#[colibri::export]
fn xor_with(data: colibri::Bytes, key: u8) -> colibri::Bytes {
    colibri::Bytes(data.iter().map(|b| b ^ key).collect())
}

#[test]
fn an_exported_function_names_bytes_in_its_signature() {
    let (mut vm, out) = vm();
    xor_with::register(&mut vm).unwrap();
    assert!(
        vm.interface_text()
            .contains("fn xor_with(data: Bytes, key: u8) -> Bytes\n"),
        "{}",
        vm.interface_text()
    );
    run(
        &mut vm,
        "fn main() {\n    print(xor_with(Bytes([0, 255, 1]), 255))\n}\n",
    )
    .unwrap();
    assert_eq!(out.contents(), "b\"\\xff\\x00\\xfe\"\n");
}
