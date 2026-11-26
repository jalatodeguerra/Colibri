//! The standard library's built-ins that allocate in proportion to an argument ask the memory
//! limit first (phase 3 L14): `str.repeat`, `fs.read`, `json.decode` and `json.parse` refuse
//! before they allocate. This binary installs the counting allocator.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use colibri_runtime as colibri;

/// The system allocator that remembers the largest request this thread made.
struct Biggest;

thread_local! {
    static BIGGEST: Cell<usize> = const { Cell::new(0) };
}

fn note(size: usize) {
    let _ = BIGGEST.try_with(|b| b.set(b.get().max(size)));
}

// SAFETY: every method forwards to `System` unchanged.
unsafe impl GlobalAlloc for Biggest {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note(layout.size());
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        note(layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note(new_size);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOC: colibri::CountingAlloc<Biggest> = colibri::CountingAlloc(Biggest);

const MIB: u64 = 1 << 20;

fn vm() -> colibri::Vm {
    let mut vm = colibri::Vm::builder()
        .allow(colibri::Cap::Fs)
        .memory_limit(8 * MIB)
        .build();
    colibri_std::install(&mut vm).expect("installs");
    vm.load_source(
        "std_limit.colibri",
        r#"import str
import fs
import json

fn repeat(n: int) -> int {
    let s = str.repeat("x", n)
    return s.len()
}

fn read(path: str) -> int {
    match fs.read(path) {
        ok(text) => return text.len()
        err(_) => return -1
    }
}

fn lines(text: str) -> int {
    return str.lines(text).len()
}

fn read_part(path: str, n: int) -> int {
    match fs.open(path) {
        ok(f) => {
            match f.read(n) {
                ok(b) => return b.len()
                err(_) => return -1
            }
        }
        err(_) => return -1
    }
}

fn read_all(path: str) -> int {
    match fs.open(path) {
        ok(f) => {
            match f.read_all() {
                ok(b) => return b.len()
                err(_) => return -1
            }
        }
        err(_) => return -1
    }
}

fn decode(text: str) -> int {
    match json.decode[List[int]](text) {
        ok(xs) => return xs.len()
        err(_) => return -1
    }
}
"#,
    )
    .unwrap();
    vm
}

fn refused<R: std::fmt::Debug>(res: Result<R, colibri::Error>, biggest: usize) {
    assert!(
        matches!(res, Err(colibri::Error::MemoryLimit { limit, .. }) if limit == 8 * MIB),
        "{res:?}"
    );
    assert!(
        biggest < MIB as usize,
        "it allocated {biggest} bytes before failing"
    );
}

fn call_path_n(
    vm: &mut colibri::Vm,
    name: &str,
    path: String,
    n: i64,
) -> Result<i64, colibri::Error> {
    vm.get_fn::<(String, i64), i64>(name)
        .unwrap()
        .call(vm, (path, n))
}

fn call_int(vm: &mut colibri::Vm, name: &str, n: i64) -> Result<i64, colibri::Error> {
    vm.get_fn::<(i64,), i64>(name).unwrap().call(vm, (n,))
}

fn call_text(vm: &mut colibri::Vm, name: &str, text: String) -> Result<i64, colibri::Error> {
    vm.get_fn::<(String,), i64>(name).unwrap().call(vm, (text,))
}

#[test]
fn str_repeat_fails_before_it_allocates() {
    let mut vm = vm();
    BIGGEST.with(|b| b.set(0));
    let res = call_int(&mut vm, "repeat", 100_000_000);
    refused(res, BIGGEST.with(Cell::get));
    assert_eq!(call_int(&mut vm, "repeat", 1000).unwrap(), 1000);
}

#[test]
fn fs_read_fails_before_it_reads() {
    let dir = std::env::temp_dir().join(format!("colibri-memlimit-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let big = dir.join("big.txt");
    std::fs::write(&big, vec![b'a'; 6 << 20]).unwrap();
    let small = dir.join("small.txt");
    std::fs::write(&small, "hello").unwrap();
    let mut vm = vm();
    BIGGEST.with(|b| b.set(0));
    let res = call_text(&mut vm, "read", big.to_string_lossy().into_owned());
    refused(res, BIGGEST.with(Cell::get));
    assert_eq!(
        call_text(&mut vm, "read", small.to_string_lossy().into_owned()).unwrap(),
        5
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn json_decode_fails_before_it_parses() {
    let mut vm = vm();
    // About 3 MiB of text: the parsed list would take several times that.
    let text = format!("[{}0]", "1,".repeat(1_500_000));
    BIGGEST.with(|b| b.set(0));
    let res = call_text(&mut vm, "decode", text.clone());
    // The argument itself is made before the entry; what must not happen is the parse.
    assert!(
        matches!(res, Err(colibri::Error::MemoryLimit { .. })),
        "{res:?}"
    );
    assert_eq!(
        call_text(&mut vm, "decode", "[1,2,3]".to_string()).unwrap(),
        3
    );
    drop(text);
}

#[test]
fn lines_refuses_before_it_splits() {
    let mut vm = vm();
    // A million empty lines are 56 bytes each as a list, and a megabyte as a text.
    let text = "\n".repeat(1_000_000);
    BIGGEST.with(|b| b.set(0));
    let res = call_text(&mut vm, "lines", text);
    // The argument is made before the entry; what must not happen is a list of lines.
    assert!(
        matches!(res, Err(colibri::Error::MemoryLimit { .. })),
        "{res:?}"
    );
    assert_eq!(
        call_text(&mut vm, "lines", "a\nb\nc".to_string()).unwrap(),
        3
    );
}

#[test]
fn file_reads_ask_for_the_file_not_the_count() {
    let dir = std::env::temp_dir().join(format!("colibri-memlimit-file-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let big = dir.join("big.bin");
    std::fs::write(&big, vec![b'a'; 6 << 20]).unwrap();
    let path = big.to_string_lossy().into_owned();
    let mut vm = vm();
    BIGGEST.with(|b| b.set(0));
    let res = call_path_n(&mut vm, "read_part", path.clone(), 1 << 40);
    refused(res, BIGGEST.with(Cell::get));
    BIGGEST.with(|b| b.set(0));
    let res = call_text(&mut vm, "read_all", path.clone());
    refused(res, BIGGEST.with(Cell::get));
    // A small count of a big file is fine.
    assert_eq!(call_path_n(&mut vm, "read_part", path, 1000).unwrap(), 1000);
    std::fs::remove_dir_all(&dir).unwrap();
}
