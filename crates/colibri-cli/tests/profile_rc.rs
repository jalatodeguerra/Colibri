//! End-to-end tests of `colibri run --profile-rc` and `colibri test --profile-rc`: the report
//! of the lines with the most reference-count traffic.

use std::path::PathBuf;
use std::process::Output;

fn colibri(args: &[&str]) -> Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_colibri"))
        .args(args)
        .output()
        .expect("the colibri binary runs")
}

fn text(out: &[u8]) -> String {
    String::from_utf8_lossy(out).into_owned()
}

/// A fresh folder for one test, with `files` in it.
fn folder(name: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("colibri-profile-rc-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for (path, body) in files {
        std::fs::write(dir.join(path), body).unwrap();
    }
    dir
}

/// One row of the report.
#[derive(Debug, PartialEq, Eq)]
struct Row {
    line: u32,
    copies: u64,
    drops: u64,
    clones: u64,
}

/// The rows of the report in `stderr`, in the order it printed them.
fn rows(stderr: &str) -> Vec<Row> {
    stderr
        .lines()
        .filter_map(|l| {
            let words: Vec<&str> = l.split_whitespace().collect();
            let [share, copies, drops, clones, place] = words[..] else {
                return None;
            };
            share.strip_suffix('%')?;
            let (_, line) = place.rsplit_once(':')?;
            Some(Row {
                line: line.parse().ok()?,
                copies: copies.parse().ok()?,
                drops: drops.parse().ok()?,
                clones: clones.parse().ok()?,
            })
        })
        .collect()
}

const STRINGS: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../benchmarks/strings.colibri"
);

#[test]
fn the_string_benchmarks_hottest_lines_are_the_concatenation_and_the_loop_over_the_parts() {
    let out = colibri(&["run", "--profile-rc", STRINGS]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), "111111\n");
    let stderr = text(&out.stderr);
    let rows = rows(&stderr);
    assert!(stderr.starts_with("rc traffic: "), "{stderr}");
    // Line 5 is `parts.push("item {i}")`: a literal loaded and dropped on each of the 200,000
    // trips. Line 10 is `for s in back`: an item taken and dropped on each trip, and the copy of
    // the list. Nothing else comes near them.
    let concat = Row {
        line: 5,
        copies: 200_000,
        drops: 200_000,
        clones: 0,
    };
    let looping = Row {
        line: 10,
        copies: 200_001,
        drops: 200_001,
        clones: 0,
    };
    let top: Vec<&Row> = rows.iter().take(2).collect();
    assert!(top.contains(&&concat), "{stderr}");
    assert!(top.contains(&&looping), "{stderr}");
    for rest in &rows[2..] {
        assert!(rest.copies + rest.drops + rest.clones < 20, "{stderr}");
    }
}

#[test]
fn the_report_lists_the_heaviest_line_first_and_says_what_it_counted() {
    let dir = folder(
        "order",
        &[(
            "p.colibri",
            "\
fn size(xs: List[int]) -> int {
    return xs.len()
}
fn main() {
    let xs = [1, 2, 3]
    let mut n = 0
    for i in 0..5 {
        n += size(xs)
    }
    let ys = xs.clone()
    print(n + ys.len())
}
",
        )],
    );
    let out = colibri(&[
        "run",
        "--profile-rc",
        dir.join("p.colibri").to_str().unwrap(),
    ]);
    assert!(out.status.success());
    assert_eq!(text(&out.stdout), "18\n");
    let stderr = text(&out.stderr);
    assert!(stderr.contains("copies"), "{stderr}");
    assert!(stderr.contains("drops"), "{stderr}");
    assert!(stderr.contains("clones"), "{stderr}");
    let rows = rows(&stderr);
    // The call in the loop: a copy of the list and a drop of it, five times.
    assert_eq!(
        rows[0],
        Row {
            line: 8,
            copies: 5,
            drops: 5,
            clones: 0
        },
        "{stderr}"
    );
    let traffic: Vec<u64> = rows.iter().map(|r| r.copies + r.drops + r.clones).collect();
    assert!(
        traffic.windows(2).all(|w| w[0] >= w[1]),
        "heaviest first: {stderr}"
    );
    assert!(
        rows.contains(&Row {
            line: 10,
            copies: 1,
            drops: 1,
            clones: 1
        }),
        "{stderr}"
    );
}

#[test]
fn without_the_flag_there_is_no_report_and_the_output_is_the_same() {
    let plain = colibri(&["run", STRINGS]);
    let counted = colibri(&["run", "--profile-rc", STRINGS]);
    assert_eq!(plain.status.code(), counted.status.code());
    assert_eq!(plain.stdout, counted.stdout);
    assert_eq!(
        text(&plain.stderr),
        "",
        "nothing on stderr without the flag"
    );
    assert!(text(&counted.stderr).contains("rc traffic"));
}

#[test]
fn it_works_without_profile_and_beside_it() {
    let dir = folder(
        "both",
        &[(
            "p.colibri",
            "fn main() {\n    let xs = [1]\n    print(xs.len())\n}\n",
        )],
    );
    let file = dir.join("p.colibri");
    let file = file.to_str().unwrap();
    let alone = colibri(&["run", "--profile-rc", file]);
    let stderr = text(&alone.stderr);
    assert!(stderr.contains("rc traffic"), "{stderr}");
    assert!(
        !stderr.contains("profile:"),
        "no sampling profile on its own: {stderr}"
    );
    assert!(!dir.join("profile.svg").exists());

    let svg = dir.join("both.svg");
    let both = colibri(&[
        "run",
        "--profile-rc",
        &format!("--profile={}", svg.display()),
        "--profile-weight=steps",
        file,
    ]);
    let stderr = text(&both.stderr);
    assert!(stderr.contains("rc traffic"), "{stderr}");
    assert!(stderr.contains("profile:"), "{stderr}");
    assert!(svg.exists());
}

#[test]
fn a_program_that_panics_still_reports_and_keeps_its_exit_code() {
    let dir = folder(
        "panic",
        &[(
            "p.colibri",
            "fn main() {\n    let xs = [1, 2]\n    print(xs[5])\n}\n",
        )],
    );
    let file = dir.join("p.colibri");
    let plain = colibri(&["run", file.to_str().unwrap()]);
    let counted = colibri(&["run", "--profile-rc", file.to_str().unwrap()]);
    assert_eq!(plain.status.code(), Some(101));
    assert_eq!(counted.status.code(), Some(101));
    let stderr = text(&counted.stderr);
    assert!(stderr.contains("rc traffic"), "{stderr}");
    assert!(
        stderr.contains("index"),
        "the panic is still printed: {stderr}"
    );
}

#[test]
fn it_cannot_be_combined_with_watch() {
    let dir = folder("watch", &[("p.colibri", "fn main() {}\n")]);
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_colibri"))
        .args(["run", "--watch", "--profile-rc"])
        .arg(dir.join("p.colibri"))
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("the colibri binary runs");
    // The refusal is immediate. A `--watch` that was accepted would wait for an edit forever,
    // so give it a generous limit and call that the failure.
    let limit = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break Some(status);
        }
        if std::time::Instant::now() > limit {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    assert_eq!(
        status.and_then(|s| s.code()),
        Some(2),
        "refused as a usage error"
    );
}

#[test]
fn colibri_test_makes_one_report_for_all_the_tests() {
    let dir = folder(
        "tests",
        &[(
            "t.colibri",
            "\
test \"first\" {
    let xs = [1, 2, 3]
    let ys = xs.clone()
    assert_eq(ys.len(), 3)
}
test \"second\" {
    let names = [\"a\", \"b\"]
    assert_eq(names.len(), 2)
}
",
        )],
    );
    let file = dir.join("t.colibri");
    let file = file.to_str().unwrap();
    let plain = colibri(&["test", file]);
    let counted = colibri(&["test", "--profile-rc", file]);
    assert!(counted.status.success(), "{}", text(&counted.stderr));
    assert_eq!(plain.stdout, counted.stdout, "the test report is the same");
    let stderr = text(&counted.stderr);
    assert!(stderr.contains("rc traffic"), "{stderr}");
    let rows = rows(&stderr);
    assert!(
        rows.iter().any(|r| r.line == 3 && r.clones == 1),
        "{stderr}"
    );
    assert!(
        rows.iter().any(|r| r.line == 7 && r.copies >= 2),
        "{stderr}"
    );
    assert_eq!(text(&plain.stderr), "");
}
