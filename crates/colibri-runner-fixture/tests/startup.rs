//! Start-up costs of delegation, measured: what handing over adds, and that a package with only plain
//! dependencies starts as it always did. They are timings, so they run one at a time in a target of
//! their own (the tests of `delegate.rs` run beside each other and would disturb them).

mod common;

use std::fs;
use std::process::Stdio;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use common::*;

/// One timing at a time, whatever the harness does.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

/// What handing over costs: the same run, in the runner directly and through `colibri`, the best
/// of ten each (a run that is already cached, as every run but the first is).
#[test]
fn handing_over_costs_little() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let w = World::new("overhead");
    let bound = w.package("bound", BOUND);
    let warm = w.colibri(&bound, &["run", "main.colibri"], "");
    assert_eq!(warm.code, 0, "{}", warm.stderr);
    let once = |exe: &str| {
        let start = Instant::now();
        let out = w.run_with(exe, &bound, &["run", "main.colibri"], "");
        assert_eq!(out.code, 0, "{}", out.stderr);
        start.elapsed()
    };
    let (mut direct, mut through) = (Duration::MAX, Duration::MAX);
    for _ in 0..10 {
        direct = direct.min(once(RUNNER));
        through = through.min(once(UNDER_TEST));
    }
    let extra = through.saturating_sub(direct);
    eprintln!(
        "delegation overhead: {:.1} ms (runner alone {:.1} ms, through colibri {:.1} ms, best of 10)",
        extra.as_secs_f64() * 1000.0,
        direct.as_secs_f64() * 1000.0,
        through.as_secs_f64() * 1000.0
    );
    // 10 ms in a release build on Unix, where `colibri` becomes the runner. A debug build is several
    // times slower at everything, and on Windows the runner is a child process (about 20 ms more).
    let limit = match (cfg!(debug_assertions), cfg!(windows)) {
        (false, false) => 10,
        (true, false) => 60,
        (_, true) => 250,
    };
    assert!(extra < Duration::from_millis(limit), "{extra:?}");
}

/// A package with a path dependency that has no bindings, and many modules: `colibri run` reads the
/// manifests to learn that there is nothing to hand over, and starts as fast as with the guard.
/// Many modules, so that loading them (what the old check did) is a large part of a start-up
/// and far above the noise of a busy machine.
const MODULES: u32 = 1000;

#[test]
fn a_package_with_only_plain_dependencies_starts_as_fast_as_ever() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let w = World::new("plaindeps");
    let dir = w.root.join("big");
    fs::create_dir_all(dir.join("dep")).unwrap();
    fs::write(
        dir.join("colibri.toml"),
        "[package]\nname = \"app\"\n\n[dependencies]\ndep = { path = \"dep\" }\n",
    )
    .unwrap();
    fs::write(dir.join("dep/colibri.toml"), "[package]\nname = \"dep\"\n").unwrap();
    fs::write(
        dir.join("dep/lib.colibri"),
        "pub fn one() -> int {\n    return 1\n}\n",
    )
    .unwrap();
    let mut main = String::from("import dep\n");
    let mut body = String::from("    let mut total = dep.one()\n");
    for i in 0..MODULES {
        fs::write(
            dir.join(format!("m{i}.colibri")),
            format!("pub fn f() -> int {{\n    let a = {i}\n    let b = a * 2 + 1\n    return b - a\n}}\n"),
        )
        .unwrap();
        main.push_str(&format!("import m{i}\n"));
        body.push_str(&format!("    total = total + m{i}.f()\n"));
    }
    main.push_str(&format!("\nfn main() {{\n{body}    print(total)\n}}\n"));
    fs::write(dir.join("main.colibri"), main).unwrap();

    let expected = (MODULES * (MODULES + 1) / 2 + 1).to_string();
    let once = |guard: bool| {
        let mut c = w.command(UNDER_TEST, &dir, &["run", "main.colibri"]);
        // No cargo to be found: a hand-over would fail.
        c.env("CARGO", "colibri-no-such-cargo").env("PATH", "");
        if guard {
            c.env("COLIBRI_PROJECT_RUNNER", "1");
        }
        let start = Instant::now();
        let out = c.stdin(Stdio::null()).output().unwrap();
        let took = start.elapsed();
        assert_eq!(
            out.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(String::from_utf8_lossy(&out.stdout).starts_with(&expected));
        took
    };
    // The two sides take turns, so a slow moment of the machine hits both.
    let (mut plain, mut guarded) = (Duration::MAX, Duration::MAX);
    for _ in 0..10 {
        plain = plain.min(once(false));
        guarded = guarded.min(once(true));
    }
    let extra = plain.saturating_sub(guarded);
    eprintln!(
        "start-up of a package with a plain dependency: {:.1} ms, with the guard {:.1} ms (extra {:.1} ms, best of 10)",
        plain.as_secs_f64() * 1000.0,
        guarded.as_secs_f64() * 1000.0,
        extra.as_secs_f64() * 1000.0
    );
    // Within noise: a package load (what the check used to cost) is a good part of the time.
    assert!(
        extra < guarded / 8 + Duration::from_millis(5),
        "extra {extra:?} on {guarded:?}"
    );
    assert!(w.cargo_log().is_empty());
}
