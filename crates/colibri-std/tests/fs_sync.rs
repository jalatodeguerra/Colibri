//! The whole-file `fs` functions (`read`, `write`, `append` and their `_bytes` forms) run on
//! the calling thread and return when they are done: they never give other coroutines a turn.

use colibri_runtime as colibri;

fn vm() -> (colibri::Vm, colibri::SharedBuffer) {
    let out = colibri::SharedBuffer::new();
    let mut vm = colibri::Vm::builder()
        .stdout(Box::new(out.clone()))
        .allow(colibri::Cap::Fs)
        .build();
    colibri_std::install(&mut vm).expect("installs");
    (vm, out)
}

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("colibri-fs-sync-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A host that drives coroutines with `tick` calls a script function that writes and reads a
/// file: nothing advances during the call, only on the tick.
#[test]
fn a_host_call_that_touches_a_file_does_not_advance_coroutines() {
    let dir = scratch("tick");
    let file = dir.join("save.txt").to_string_lossy().replace('\\', "/");
    let (mut vm, out) = vm();
    vm.load_source(
        "game.colibri",
        &format!(
            r#"import fs

fn mover() {{
    let mut i = 0
    while i < 5 {{
        print("step {{i}}")
        wait(0.0)
        i += 1
    }}
}}

fn start() {{
    let _ = go mover()
}}

fn save() {{
    print("save begins")
    fs.write("{file}", "state").expect("saved")
    fs.append("{file}", "!").expect("appended")
    fs.write_bytes("{file}", "bytes".to_bytes()).expect("saved bytes")
    fs.append_bytes("{file}", "?".to_bytes()).expect("appended bytes")
    print(fs.read("{file}").expect("read"))
    print(fs.read_bytes("{file}").expect("read bytes").len())
    print("save ends")
}}
"#
        ),
    )
    .unwrap();
    let start = vm.get_fn::<(), ()>("start").unwrap();
    let save = vm.get_fn::<(), ()>("save").unwrap();
    start.call(&mut vm, ()).unwrap();
    let before = out.contents();
    save.call(&mut vm, ()).unwrap();
    let during = out.contents()[before.len()..].to_string();
    assert_eq!(during, "save begins\nbytes?\n6\nsave ends\n");
    vm.tick(0.016).unwrap();
    assert!(out.contents().len() > before.len() + during.len());
    let _ = std::fs::remove_dir_all(&dir);
}

/// Two coroutines that use whole-file functions interleave exactly as the scheduler says, the
/// same way on every run.
#[test]
fn coroutines_using_whole_file_functions_finish_in_a_fixed_order() {
    let dir = scratch("order");
    let base = dir.to_string_lossy().replace('\\', "/");
    let source = format!(
        r#"import fs

fn job(name: str) {{
    let mut i = 0
    while i < 3 {{
        fs.write("{base}/{{name}}.txt", "{{i}}").expect("written")
        print("{{name}} wrote {{i}}")
        wait(0.0)
        i += 1
    }}
}}

fn main() {{
    let a = go job("a")
    let b = go job("b")
    a.join()
    b.join()
    print("{{fs.read("{base}/a.txt").expect("a")}}{{fs.read("{base}/b.txt").expect("b")}}")
}}
"#
    );
    let mut first = None;
    for _ in 0..100 {
        let (mut vm, out) = vm();
        vm.load_source("app/main.colibri", &source).unwrap();
        vm.run_main().unwrap();
        let got = out.contents();
        match &first {
            None => first = Some(got),
            Some(f) => assert_eq!(&got, f),
        }
    }
    assert_eq!(
        first.unwrap(),
        "a wrote 0\nb wrote 0\na wrote 1\nb wrote 1\na wrote 2\nb wrote 2\n22\n"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
