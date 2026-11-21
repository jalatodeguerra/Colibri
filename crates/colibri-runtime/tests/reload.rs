//! Hot reload (DESIGN §16.3, ARCHITECTURE §17): `Vm::reload_changed` patches function bodies,
//! restarts on anything else, and keeps the old program when the new one does not check.

mod common;
use common::vm_pending;

use std::fs;
use std::path::{Path, PathBuf};

use colibri::{ReloadOptions, ReloadOutcome};
use colibri_runtime as colibri;

fn vm() -> (colibri::Vm, colibri::SharedBuffer) {
    let out = colibri::SharedBuffer::new();
    let vm = colibri::Vm::builder().stdout(Box::new(out.clone())).build();
    (vm, out)
}

/// A fresh temporary folder holding `files` (`(relative path, text)`).
fn tree(name: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("colibri-reload-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    for (path, text) in files {
        write(&dir, path, text);
    }
    dir
}

fn write(dir: &Path, path: &str, text: &str) {
    let full = dir.join(path);
    fs::create_dir_all(full.parent().unwrap()).unwrap();
    fs::write(full, text).unwrap();
}

fn greeting(vm: &mut colibri::Vm) -> String {
    vm.get_fn::<(), String>("greet")
        .unwrap()
        .call(vm, ())
        .unwrap()
}

#[test]
fn nothing_changed_is_unchanged() {
    let dir = tree("none", &[("a.colibri", "fn greet() -> str => \"v1\"\n")]);
    let (mut vm, _) = vm();
    vm.load_file(dir.join("a.colibri")).unwrap();
    assert!(matches!(
        vm.reload_changed().unwrap(),
        ReloadOutcome::Unchanged
    ));
    assert!(matches!(
        vm.reload_changed().unwrap(),
        ReloadOutcome::Unchanged
    ));
}

#[test]
fn a_new_body_is_used_by_the_next_call_through_the_same_handle() {
    let dir = tree(
        "patch",
        &[(
            "a.colibri",
            "fn greet() -> str => \"v1\"\nfn other() -> int => 1\n",
        )],
    );
    let (mut vm, _) = vm();
    vm.load_file(dir.join("a.colibri")).unwrap();
    let greet = vm.get_fn::<(), String>("greet").unwrap();
    assert_eq!(greet.call(&mut vm, ()).unwrap(), "v1");
    write(
        &dir,
        "a.colibri",
        "fn greet() -> str => \"version two\"\nfn other() -> int => 1\n",
    );
    match vm.reload_changed().unwrap() {
        ReloadOutcome::Patched { functions } => assert_eq!(functions, 1),
        other => panic!("{other:?}"),
    }
    // The handle from before the patch is still valid.
    assert_eq!(greet.call(&mut vm, ()).unwrap(), "version two");
    // Reading it again changes nothing.
    assert!(matches!(
        vm.reload_changed().unwrap(),
        ReloadOutcome::Unchanged
    ));
}

#[test]
fn an_edit_of_the_same_length_within_the_same_instant_is_seen() {
    let dir = tree("racy", &[("a.colibri", "fn greet() -> str => \"1\"\n")]);
    let (mut vm, _) = vm();
    vm.load_file(dir.join("a.colibri")).unwrap();
    for i in 2..8 {
        write(
            &dir,
            "a.colibri",
            &format!("fn greet() -> str => \"{i}\"\n"),
        );
        assert!(matches!(
            vm.reload_changed().unwrap(),
            ReloadOutcome::Patched { functions: 1 }
        ));
        assert_eq!(greeting(&mut vm), i.to_string());
    }
}

#[test]
fn comments_and_moved_code_do_not_count() {
    let dir = tree(
        "moved",
        &[(
            "a.colibri",
            "fn greet() -> str => \"v1\"\nfn other() -> int {\n    return 1 + 1\n}\n",
        )],
    );
    let (mut vm, _) = vm();
    vm.load_file(dir.join("a.colibri")).unwrap();
    write(
        &dir,
        "a.colibri",
        "// a new header\n\n\nfn greet() -> str => \"v1\"\n\nfn other() -> int {\n    // why\n    return 1 + 1\n}\n",
    );
    assert!(matches!(
        vm.reload_changed().unwrap(),
        ReloadOutcome::Unchanged
    ));
    // The bodies now point into the new text, and a later edit still patches.
    write(
        &dir,
        "a.colibri",
        "// a new header\n\n\nfn greet() -> str => \"v2\"\n\nfn other() -> int {\n    // why\n    return 1 + 1\n}\n",
    );
    assert!(matches!(
        vm.reload_changed().unwrap(),
        ReloadOutcome::Patched { functions: 1 }
    ));
    assert_eq!(greeting(&mut vm), "v2");
}

#[test]
fn a_patch_does_not_rerun_initialisers_and_keeps_module_state() {
    let src = |msg: &str| {
        format!(
            "var count = start()\nfn start() -> int {{\n    print(\"init\")\n    return 10\n}}\nfn bump() -> int {{\n    count += 1\n    return count\n}}\nfn greet() -> str => \"{msg}\"\nprint(\"top level\")\n"
        )
    };
    let dir = tree("state", &[("a.colibri", &src("a"))]);
    let (mut vm, out) = vm();
    vm.load_file(dir.join("a.colibri")).unwrap();
    let bump = vm.get_fn::<(), i64>("bump").unwrap();
    assert_eq!(bump.call(&mut vm, ()).unwrap(), 11);
    write(&dir, "a.colibri", &src("b"));
    assert!(matches!(
        vm.reload_changed().unwrap(),
        ReloadOutcome::Patched { functions: 1 }
    ));
    assert_eq!(bump.call(&mut vm, ()).unwrap(), 12);
    assert_eq!(greeting(&mut vm), "b");
    assert_eq!(out.contents(), "init\ntop level\n");
}

#[test]
fn a_coroutine_in_an_old_body_finishes_on_it_and_new_calls_use_the_new_one() {
    vm_pending(
        "a_coroutine_in_an_old_body_finishes_on_it_and_new_calls_use_the_new_one",
        || {
            let src = |tag: &str| {
                format!(
                    "fn worker() {{\n    wait(1.0)\n    print(\"worker {tag}\")\n}}\nfn main() {{\n    go worker()\n}}\nfn spawn() {{\n    go worker()\n}}\n"
                )
            };
            let dir = tree("coro", &[("a.colibri", &src("v1"))]);
            let (mut vm, out) = vm();
            vm.load_file(dir.join("a.colibri")).unwrap();
            vm.run_main().unwrap();
            assert_eq!(vm.coroutine_count(), 1);
            write(&dir, "a.colibri", &src("v2"));
            assert!(matches!(
                vm.reload_changed().unwrap(),
                ReloadOutcome::Patched { functions: 1 }
            ));
            vm.get_fn::<(), ()>("spawn")
                .unwrap()
                .call(&mut vm, ())
                .unwrap();
            assert_eq!(vm.coroutine_count(), 2);
            vm.tick(1.5).unwrap();
            assert_eq!(out.contents(), "worker v1\nworker v2\n");
        },
    )
}

#[test]
fn a_signature_change_restarts_and_carries_state_through_the_hooks() {
    let v1 = "var score = 7\nfn save_state() -> str => \"score={score}\"\nfn add(a: int) -> int => a + 1\nfn main() { print(\"main v1\") }\n";
    let v2 = "var score = 0\nfn restore_state(s: str) {\n    print(\"restoring {s}\")\n}\nfn add(a: int, b: int) -> int => a + b\nprint(\"init v2\")\nfn main() { print(\"main v2\") }\n";
    let dir = tree("restart", &[("a.colibri", v1)]);
    let (mut vm, out) = vm();
    vm.load_file(dir.join("a.colibri")).unwrap();
    let old_add = vm.get_fn::<(i64,), i64>("add").unwrap();
    write(&dir, "a.colibri", v2);
    match vm.reload_changed().unwrap() {
        ReloadOutcome::Restarted {
            reason,
            state_carried,
        } => {
            assert!(reason.contains("functions"), "{reason}");
            assert!(state_carried);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(out.contents(), "init v2\nrestoring score=7\n");
    // `main` is the host's to call.
    vm.run_main().unwrap();
    assert!(out.contents().ends_with("main v2\n"));
    // Handles of the old program are stale; new ones work.
    assert!(matches!(
        old_add.call(&mut vm, (1,)),
        Err(colibri::Error::StaleHandle)
    ));
    let add = vm.get_fn::<(i64, i64), i64>("add").unwrap();
    assert_eq!(add.call(&mut vm, (2, 3)).unwrap(), 5);
}

#[test]
fn restart_without_hooks_carries_nothing() {
    let dir = tree(
        "nohooks",
        &[(
            "a.colibri",
            "fn restore_state(s: str) { print(s) }\nfn f() -> int => 1\n",
        )],
    );
    let (mut vm, out) = vm();
    vm.load_file(dir.join("a.colibri")).unwrap();
    write(
        &dir,
        "a.colibri",
        "fn restore_state(s: str) { print(s) }\nfn f(x: int) -> int => x\n",
    );
    match vm.reload_changed().unwrap() {
        ReloadOutcome::Restarted { state_carried, .. } => assert!(!state_carried),
        other => panic!("{other:?}"),
    }
    assert_eq!(out.contents(), "");
}

#[test]
fn what_forces_a_restart() {
    let base = "struct P {\n    x: int\n}\nenum Shape {\n    Dot\n    Line\n}\nvar n = 1\nfn f() -> int => 1\n";
    let cases: &[(&str, String, &str)] = &[
        (
            "field",
            base.replace("x: int", "x: int\n    y: int"),
            "struct",
        ),
        ("variant", base.replace("Line", "Line\n    Arc"), "enum"),
        (
            "initialiser",
            base.replace("var n = 1", "var n = 2"),
            "initialiser",
        ),
        (
            "global type",
            base.replace("var n = 1", "var n = 1.5"),
            "declarations",
        ),
        (
            "new function",
            format!("{base}fn g() -> int => 2\n"),
            "functions",
        ),
        (
            "renamed function",
            base.replace("fn f()", "fn h()"),
            "functions",
        ),
        (
            "return type",
            base.replace("fn f() -> int => 1", "fn f() -> float => 1.0"),
            "functions",
        ),
    ];
    for (name, new, expect) in cases {
        let dir = tree(
            &format!("force-{}", name.replace(' ', "-")),
            &[("a.colibri", base)],
        );
        let (mut vm, _) = vm();
        vm.load_file(dir.join("a.colibri")).unwrap();
        write(&dir, "a.colibri", new);
        match vm.reload_changed().unwrap() {
            ReloadOutcome::Restarted { reason, .. } => {
                assert!(reason.contains(expect), "{name}: {reason}")
            }
            other => panic!("{name}: {other:?}"),
        }
    }
}

/// A field default is inlined at the struct literals that omit it and called by
/// `json.decode`; patching one function body would leave the two disagreeing, so a changed
/// default restarts.
#[test]
fn a_changed_field_default_restarts_instead_of_patching() {
    vm_pending(
        "a_changed_field_default_restarts_instead_of_patching",
        || {
            let v1 = "struct P {\n    x: int = 1\n}\nfn literal() -> int => P {}.x\n";
            let dir = tree("default-change", &[("a.colibri", v1)]);
            let (mut vm, _) = vm();
            vm.load_file(dir.join("a.colibri")).unwrap();
            write(&dir, "a.colibri", &v1.replace("x: int = 1", "x: int = 2"));
            match vm.reload_changed().unwrap() {
                ReloadOutcome::Restarted { reason, .. } => {
                    assert!(reason.contains("struct"), "{reason}")
                }
                other => panic!("{other:?}"),
            }
            let now: i64 = vm
                .get_fn::<(), i64>("literal")
                .unwrap()
                .call(&mut vm, ())
                .unwrap();
            assert_eq!(now, 2);
            // A body that is not a default still patches.
            write(
                &dir,
                "a.colibri",
                &v1.replace("x: int = 1", "x: int = 2")
                    .replace("P {}.x", "P {}.x + 0"),
            );
            assert!(matches!(
                vm.reload_changed().unwrap(),
                ReloadOutcome::Patched { .. }
            ));
        },
    )
}

#[test]
fn code_that_does_not_check_leaves_the_old_program_running() {
    let dir = tree(
        "broken",
        &[("a.colibri", "var n = 1\nfn greet() -> str => \"v1\"\n")],
    );
    let (mut vm, _) = vm();
    vm.load_file(dir.join("a.colibri")).unwrap();
    write(&dir, "a.colibri", "var n = 1\nfn greet() -> str => 5\n");
    match vm.reload_changed().unwrap() {
        ReloadOutcome::CheckFailed(report) => {
            assert!(report.has_errors());
            assert!(report.render(false).contains("a.colibri"));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(greeting(&mut vm), "v1");
    // Reported once.
    assert!(matches!(
        vm.reload_changed().unwrap(),
        ReloadOutcome::Unchanged
    ));
    write(
        &dir,
        "a.colibri",
        "var n = 1\nfn greet() -> str => \"v3\"\n",
    );
    assert!(matches!(
        vm.reload_changed().unwrap(),
        ReloadOutcome::Patched { functions: 1 }
    ));
    assert_eq!(greeting(&mut vm), "v3");
}

#[test]
fn a_syntax_error_is_a_check_failure_too() {
    let dir = tree("syntax", &[("a.colibri", "fn greet() -> str => \"v1\"\n")]);
    let (mut vm, _) = vm();
    vm.load_file(dir.join("a.colibri")).unwrap();
    write(&dir, "a.colibri", "fn greet( -> str => \"v1\"\n");
    assert!(matches!(
        vm.reload_changed().unwrap(),
        ReloadOutcome::CheckFailed(_)
    ));
    assert_eq!(greeting(&mut vm), "v1");
}

#[test]
fn a_package_notices_a_change_in_a_module_and_in_a_dependency() {
    let main =
        "import util\nimport physics\nfn greet() -> str => \"{util.name()} {physics.g()}\"\n";
    let dir = tree(
        "pkg",
        &[
            (
                "app/colibri.toml",
                "[package]\nname = \"app\"\n\n[dependencies]\nphysics = { path = \"../physics\" }\n",
            ),
            ("app/main.colibri", main),
            ("app/util.colibri", "pub fn name() -> str => \"u1\"\n"),
            ("physics/colibri.toml", "[package]\nname = \"physics\"\n"),
            ("physics/lib.colibri", "pub fn g() -> int => 10\n"),
        ],
    );
    let (mut vm, _) = vm();
    vm.load_package(dir.join("app")).unwrap();
    assert_eq!(greeting(&mut vm), "u1 10");
    write(&dir, "app/util.colibri", "pub fn name() -> str => \"u2\"\n");
    assert!(matches!(
        vm.reload_changed().unwrap(),
        ReloadOutcome::Patched { functions: 1 }
    ));
    assert_eq!(greeting(&mut vm), "u2 10");
    write(&dir, "physics/lib.colibri", "pub fn g() -> int => 99\n");
    assert!(matches!(
        vm.reload_changed().unwrap(),
        ReloadOutcome::Patched { functions: 1 }
    ));
    assert_eq!(greeting(&mut vm), "u2 99");
    // A dependency's public signature changes: the whole program restarts.
    write(&dir, "physics/lib.colibri", "pub fn g() -> str => \"x\"\n");
    assert!(matches!(
        vm.reload_changed().unwrap(),
        ReloadOutcome::Restarted { .. }
    ));
    assert_eq!(greeting(&mut vm), "u2 x");
}

#[test]
fn a_panic_in_the_new_initialisers_leaves_no_program_until_the_next_edit() {
    let dir = tree(
        "initpanic",
        &[("a.colibri", "var n = 1\nfn greet() -> str => \"v1\"\n")],
    );
    let (mut vm, _) = vm();
    vm.load_file(dir.join("a.colibri")).unwrap();
    write(
        &dir,
        "a.colibri",
        "var n = 1 / (1 - 1)\nfn greet() -> str => \"v1\"\n",
    );
    let err = vm.reload_changed().expect_err("the initialiser panics");
    assert!(matches!(err, colibri::Error::Panic { .. }), "{err:?}");
    assert!(vm.get_fn::<(), String>("greet").is_err());
    write(
        &dir,
        "a.colibri",
        "var n = 2\nfn greet() -> str => \"v4\"\n",
    );
    assert!(matches!(
        vm.reload_changed().unwrap(),
        ReloadOutcome::Restarted { .. }
    ));
    assert_eq!(greeting(&mut vm), "v4");
}

#[test]
fn forcing_a_restart_reinitialises_module_state() {
    let dir = tree(
        "forced",
        &[(
            "a.colibri",
            "var n = 0\nfn bump() -> int {\n    n += 1\n    return n\n}\n",
        )],
    );
    let (mut vm, _) = vm();
    vm.load_file(dir.join("a.colibri")).unwrap();
    let bump = vm.get_fn::<(), i64>("bump").unwrap();
    bump.call(&mut vm, ()).unwrap();
    write(
        &dir,
        "a.colibri",
        "var n = 0\nfn bump() -> int {\n    n += 10\n    return n\n}\n",
    );
    let out = vm
        .reload_changed_with(ReloadOptions {
            force_restart: true,
        })
        .unwrap();
    assert!(matches!(out, ReloadOutcome::Restarted { .. }));
    let bump = vm.get_fn::<(), i64>("bump").unwrap();
    assert_eq!(bump.call(&mut vm, ()).unwrap(), 10);
}

#[test]
fn host_functions_budget_and_capabilities_survive_a_restart() {
    vm_pending(
        "host_functions_budget_and_capabilities_survive_a_restart",
        || {
            let src = |extra: &str| {
                format!(
                    "fn twice(n: int) -> int => double(n)\nfn spin() {{\n    while true {{\n    }}\n}}\n{extra}"
                )
            };
            let dir = tree("host", &[("a.colibri", &src(""))]);
            let mut vm = colibri::Vm::builder().instruction_budget(10_000).build();
            vm.register_fn("double", |n: i64| n * 2).unwrap();
            vm.load_file(dir.join("a.colibri")).unwrap();
            write(&dir, "a.colibri", &src("fn added() -> int => 1\n"));
            assert!(matches!(
                vm.reload_changed().unwrap(),
                ReloadOutcome::Restarted { .. }
            ));
            let twice = vm.get_fn::<(i64,), i64>("twice").unwrap();
            assert_eq!(twice.call(&mut vm, (4,)).unwrap(), 8);
            let spin = vm.get_fn::<(), ()>("spin").unwrap();
            assert!(matches!(
                spin.call(&mut vm, ()),
                Err(colibri::Error::BudgetExhausted { .. })
            ));
        },
    )
}

#[test]
fn programs_from_memory_have_nothing_to_reload() {
    let (mut vm, _) = vm();
    vm.load_source("a.colibri", "fn f() -> int => 1\n").unwrap();
    assert!(matches!(
        vm.reload_changed().unwrap(),
        ReloadOutcome::Unchanged
    ));
}

#[test]
fn the_watching_event_loop_calls_the_hook_between_ticks() {
    vm_pending(
        "the_watching_event_loop_calls_the_hook_between_ticks",
        || {
            let dir = tree(
                "loop",
                &[(
                    "a.colibri",
                    "fn msg() -> str => \"one\"\nfn ticker() {\n    var i = 0\n    while i < 6 {\n        print(msg())\n        i += 1\n        wait_real(0.02)\n    }\n}\nfn main() {\n    go ticker()\n}\n",
                )],
            );
            let (mut vm, out) = vm();
            vm.load_file(dir.join("a.colibri")).unwrap();
            vm.run_main().unwrap();
            let mut calls = 0;
            let mut patched = 0;
            let path = dir.clone();
            vm.run_event_loop_watching(0.001, 0.01, |vm| {
        calls += 1;
        if calls == 3 {
            write(
                &path,
                "a.colibri",
                "fn msg() -> str => \"two\"\nfn ticker() {\n    var i = 0\n    while i < 6 {\n        print(msg())\n        i += 1\n        wait_real(0.02)\n    }\n}\nfn main() {\n    go ticker()\n}\n",
            );
        }
        if let ReloadOutcome::Patched { .. } = vm.reload_changed()? {
            patched += 1;
        }
        Ok(())
    })
    .unwrap();
            assert_eq!(patched, 1);
            let text = out.contents();
            assert!(text.starts_with("one\n"), "{text}");
            assert!(text.ends_with("two\n"), "{text}");
            assert_eq!(text.lines().count(), 6);
        },
    )
}

/// A generated program of about 10 000 lines, patched once. Prints the time; the bound is
/// loose because test builds are unoptimised (DESIGN §17.4 asks for 200 ms in a release build).
#[test]
fn patching_a_ten_thousand_line_program_is_quick() {
    fn program(tag: &str) -> String {
        let mut s = String::new();
        s.push_str("struct Item {\n    id: int\n    name: str\n}\n");
        for i in 0..1000 {
            s.push_str(&format!(
                "fn f{i}(x: int) -> int {{\n    var total = x\n    for k in 0..3 {{\n        if k % 2 == 0 {{\n            total += k * {i}\n        }} else {{\n            total -= 1\n        }}\n    }}\n    let it = Item {{ id: total, name: \"n{i}\" }}\n    return it.id + {}\n}}\n",
                if i == 400 { tag } else { "0" }
            ));
        }
        s.push_str("fn main() {\n    print(f400(1))\n}\n");
        s
    }
    let dir = tree("perf", &[("a.colibri", &program("0"))]);
    assert!(program("0").lines().count() > 10_000);
    let (mut vm, _) = vm();
    vm.load_file(dir.join("a.colibri")).unwrap();
    // The first reload also fingerprints the running program; measure the second.
    write(&dir, "a.colibri", &program("1"));
    assert!(matches!(
        vm.reload_changed().unwrap(),
        ReloadOutcome::Patched { functions: 1 }
    ));
    write(&dir, "a.colibri", &program("2"));
    let start = std::time::Instant::now();
    let outcome = vm.reload_changed().unwrap();
    let took = start.elapsed();
    assert!(matches!(outcome, ReloadOutcome::Patched { functions: 1 }));
    eprintln!("patch of a 10k-line program: {took:?}");
    assert!(took.as_millis() < 3_000, "{took:?}");
}

/// Removes a folder when dropped, so a failing test leaves nothing behind.
struct RemoveOnDrop(PathBuf);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// `to` written relative to `from` (both absolute): `../../tmp/x`.
fn relative_to(from: &std::path::Path, to: &std::path::Path) -> PathBuf {
    let from: Vec<_> = from.components().collect();
    let to: Vec<_> = to.components().collect();
    let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    let mut out = PathBuf::new();
    for _ in common..from.len() {
        out.push("..");
    }
    for c in &to[common..] {
        out.push(c);
    }
    out
}

#[test]
fn a_relative_entry_path_does_not_look_changed_right_after_the_load() {
    // The entry is given relative to the working directory (as `colibri run --watch a.colibri`
    // does); the files the load read are recorded absolute, and the two must be one file. The
    // folder is in the target folder, reached by a relative path that goes up and down again.
    // A relative path cannot lead to another drive (Windows), so the folder is kept on the
    // working folder's own drive.
    let cwd = std::env::current_dir().unwrap();
    let mut base = std::path::absolute(env!("CARGO_TARGET_TMPDIR")).unwrap();
    if base.components().next() != cwd.components().next() {
        base = cwd.clone();
    }
    let dir = base.join(format!("colibri-reload-rel-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let _cleanup = RemoveOnDrop(dir.clone());
    write(&dir, "a.colibri", "fn greet() -> str => \"v1\"\n");
    let relative = relative_to(&cwd, &dir).join("a.colibri");
    assert!(relative.is_relative(), "{relative:?}");
    let (mut vm, _) = vm();
    vm.load_file(&relative).unwrap();
    // Just written: inside the window in which a file's time is not trusted, a file whose
    // content is not known would be read again and look different every time.
    for _ in 0..3 {
        let outcome = vm
            .reload_changed_with(ReloadOptions {
                force_restart: true,
            })
            .unwrap();
        assert!(matches!(outcome, ReloadOutcome::Unchanged), "{outcome:?}");
    }
}
