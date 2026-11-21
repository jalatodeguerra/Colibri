//! A Rust type registered by several modules is one Colibri type whose owner and interface
//! text do not depend on the order the registrations were made in (EMBEDDING §1): every
//! order, interleaved ones included, gives each module the same text and hash, and scripts
//! run the same.

mod common;
use common::vm_pending;

use colibri::{CheckConfig, Interface};
use colibri_runtime as colibri;

#[colibri::export(value)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pos {
    pub x: i64,
}

#[colibri::export(value)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Seg {
    pub a: Pos,
    pub b: Pos,
}

#[colibri::export(value)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Vel {
    pub v: i64,
}

#[colibri::export(value)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Dir {
    Up,
    Down,
}

/// The registrations of each module, in two steps each (`step` 0 or 1). `Pos` is registered by
/// every module, `Vel` by `a`, `c` and the default module, `Seg` (which holds `Pos`) by `b`
/// and `c`, and the enum `Dir` by `a` and `c`.
fn step(vm: &mut colibri::Vm, module: &str, step: usize) {
    match (module, step) {
        ("default", 0) => {
            vm.register_fn("dflt", |p: Pos| p.x + 100).unwrap();
        }
        ("default", _) => {
            vm.register_type::<Pos>().unwrap();
            vm.register_fn("dvel", |v: Vel| v.v + 1000).unwrap();
        }
        ("a", 0) => {
            vm.module("a")
                .register_fn("make", |x: i64| Pos { x })
                .unwrap();
            vm.module("a")
                .register_fn("dir", |d: Dir| matches!(d, Dir::Up) as i64)
                .unwrap();
        }
        ("a", _) => {
            vm.module("a").register_fn("av", |v: Vel| v.v).unwrap();
        }
        ("b", 0) => {
            vm.module("b")
                .register_fn("seg", |p: Pos| Seg { a: p, b: p })
                .unwrap();
        }
        ("b", _) => {
            // Registered by itself, after (or before) the others described it in a signature.
            vm.module("b").register_type::<Pos>().unwrap();
            vm.module("b")
                .register_fn("opt", |x: i64| if x > 0 { Some(Pos { x }) } else { None })
                .unwrap();
        }
        ("c", 0) => {
            vm.module("c")
                .register_fn("len", |s: Seg| s.b.x - s.a.x)
                .unwrap();
        }
        ("c", _) => {
            vm.module("c").register_type::<Vel>().unwrap();
            vm.module("c").register_type::<Dir>().unwrap();
            vm.module("c").register_fn("up", || Dir::Up).unwrap();
            vm.module("c").register_fn("cv", |v: Vel| v.v + 1).unwrap();
        }
        _ => unreachable!(),
    }
}

const MODULES: [&str; 4] = ["default", "a", "b", "c"];

const SCRIPT: &str = "import a\nimport b\nimport c\nfn main() {\n    let p = a.make(3)\n    let s = b.seg(p)\n    print(c.len(s))\n    print(dflt(p))\n    print(a.dir(c.up()))\n    let q: b.Pos = p\n    let r: Pos = q\n    print(b.opt(2) != none)\n    print(r.x)\n    print(a.av(Vel { v: 5 }) + c.cv(Vel { v: 1 }) + dvel(Vel { v: 2 }))\n}\n";

/// What one registration order gives: the interface text and hash of each module, and what
/// the script prints.
fn outcome(order: &[(&str, usize)]) -> (Vec<(String, String)>, String) {
    let out = colibri::SharedBuffer::new();
    let mut vm = colibri::Vm::builder().stdout(Box::new(out.clone())).build();
    for (module, n) in order {
        step(&mut vm, module, *n);
    }
    let mut texts = vec![(vm.interface_text(), vm.interface_hash())];
    for m in &MODULES[1..] {
        let module = vm.module(m);
        texts.push((module.interface_text(), module.interface_hash()));
    }
    vm.load_source("m.colibri", SCRIPT)
        .unwrap_or_else(|e| panic!("{order:?}: {}", e.render(false)));
    vm.run_main()
        .unwrap_or_else(|e| panic!("{order:?}: {}", e.render(false)));
    (texts, out.contents())
}

/// Every way of interleaving the two steps of each module, each module's own steps staying in
/// order.
fn interleavings() -> Vec<Vec<(&'static str, usize)>> {
    fn go(
        next: &mut [usize; 4],
        so_far: &mut Vec<(&'static str, usize)>,
        out: &mut Vec<Vec<(&'static str, usize)>>,
    ) {
        if so_far.len() == 8 {
            out.push(so_far.clone());
            return;
        }
        for (i, m) in MODULES.iter().enumerate() {
            if next[i] < 2 {
                so_far.push((m, next[i]));
                next[i] += 1;
                go(next, so_far, out);
                next[i] -= 1;
                so_far.pop();
            }
        }
    }
    let mut out = Vec::new();
    go(&mut [0; 4], &mut Vec::new(), &mut out);
    out
}

#[test]
fn every_registration_order_gives_every_module_the_same_interface_and_the_same_run() {
    vm_pending(
        "every_registration_order_gives_every_module_the_same_interface_and_the_same_run",
        || {
            let all = interleavings();
            assert_eq!(all.len(), 2520);
            let (first_texts, first_out) = outcome(&all[0]);
            // The rule: the default module owns what it registered, else the module that sorts first.
            let [default, a, b, c] = &first_texts[..] else {
                unreachable!()
            };
            assert!(default.0.contains("struct Pos { x: int }"), "{}", default.0);
            assert!(default.0.contains("struct Vel { v: int }"), "{}", default.0);
            assert!(a.0.contains("enum Dir { Up, Down }"), "{}", a.0);
            assert!(a.0.contains("type Pos = Pos"), "{}", a.0);
            assert!(!a.0.contains("uses"), "{}", a.0);
            assert!(b.0.contains("struct Seg { a: Pos, b: Pos }"), "{}", b.0);
            assert!(c.0.contains("type Seg = b.Seg"), "{}", c.0);
            assert!(c.0.contains("type Dir = a.Dir"), "{}", c.0);
            assert!(c.0.contains("uses = [\"a\", \"b\"]"), "{}", c.0);
            assert_eq!(first_out, "0\n103\n1\ntrue\n3\n1009\n");
            for order in &all {
                let (texts, out) = outcome(order);
                assert_eq!(texts, first_texts, "{order:?}");
                assert_eq!(out, first_out, "{order:?}");
            }
        },
    )
}

#[test]
fn the_interface_files_of_one_order_verify_in_another() {
    let dir = std::env::temp_dir().join(format!("colibri-sharing-order-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let build = |order: &[&str]| {
        let mut vm = colibri::Vm::builder().build();
        for m in order {
            step(&mut vm, m, 0);
            step(&mut vm, m, 1);
        }
        vm
    };
    let mut written = build(&["default", "a", "b", "c"]);
    let mut elsewhere = build(&["c", "b", "a", "default"]);
    for m in &MODULES[1..] {
        let file = dir.join(format!("{m}.cbi"));
        written.module(m).write_interface(&file).unwrap();
        elsewhere
            .module(m)
            .verify_interface(&file)
            .unwrap_or_else(|e| panic!("{m}: {e}"));
    }
    let file = dir.join("default.cbi");
    written.write_interface(&file).unwrap();
    elsewhere.verify_interface(&file).unwrap();
    // The files written by one build check the script for the other.
    let interfaces: Vec<Interface> = MODULES[1..]
        .iter()
        .map(|m| Interface::read(dir.join(format!("{m}.cbi"))).unwrap())
        .chain([Interface::read(&file).unwrap()])
        .collect();
    let config = CheckConfig {
        interfaces,
        ..CheckConfig::default()
    };
    let report = colibri::check_source_with("main.colibri", SCRIPT, &config);
    assert!(!report.has_errors(), "{}", report.render(false));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_type_that_moves_to_the_default_module_leaves_no_stale_names_behind() {
    // `c` registers `Pos`, `a` registers `Pos` and `Vel`, then the default module registers
    // `Pos`, then `c` registers `Vel`: `Pos` moved to the default module, so `a` does not use
    // `c` any more, and `Vel` can be shared by `c` too.
    let mut vm = colibri::Vm::builder().build();
    vm.module("c").register_fn("cp", |p: Pos| p.x).unwrap();
    vm.module("a").register_fn("ap", |p: Pos| p.x).unwrap();
    vm.module("a").register_fn("av", |v: Vel| v.v).unwrap();
    vm.register_fn("dflt", |p: Pos| p.x).unwrap();
    vm.module("c").register_fn("cv", |v: Vel| v.v).unwrap();
    vm.load_source("m.colibri", "fn main() { }\n").unwrap();
    let a = vm.module("a").interface_text();
    assert!(!a.contains("uses"), "{a}");
    assert!(
        a.contains("fn ap(a: Pos)") || a.contains("ap(a: Pos)"),
        "{a}"
    );
    assert!(a.contains("type Pos = Pos"), "{a}");
    let c = vm.module("c").interface_text();
    assert!(c.contains("type Vel = a.Vel"), "{c}");
    assert!(c.contains("uses = [\"a\"]"), "{c}");
}
