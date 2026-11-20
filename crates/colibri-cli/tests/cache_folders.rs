//! `colibri check <folder>` and `colibri test <folder>` read every module under the folder,
//! except under hidden folders and folders that hold a `CACHEDIR.TAG` (a cache is not source);
//! `target` and `node_modules` are ordinary folders. An `import` reaches into any of them.

use std::path::PathBuf;
use std::process::{Command, Output};

fn colibri(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_colibri"))
        .args(args)
        .env(
            "COLIBRI_HOME",
            std::env::temp_dir().join("colibri-cache-folders-home"),
        )
        .output()
        .expect("the colibri binary runs")
}

fn folder(name: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "colibri-cache-folders-{}-{name}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    for (path, text) in files {
        let full = dir.join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, text).unwrap();
    }
    dir
}

const TAG: &str = "Signature: 8a477f597d28d172789f06886806bc55\n";
const PASSING: &str = "test \"t\" {\n    assert(1 < 2)\n}\n";
const FAILING: &str = "test \"t\" {\n    assert(1 > 2)\n}\n";

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

#[test]
fn test_folder_does_not_descend_into_a_cache_but_does_into_target() {
    let dir = folder(
        "walk",
        &[
            ("colibri.toml", "[package]\nname = \"demo\"\n"),
            ("main.colibri", "fn main() {}\n"),
            ("cache/CACHEDIR.TAG", TAG),
            ("cache/bad.colibri", FAILING),
            ("target/ok.colibri", PASSING),
            ("node_modules/ok2.colibri", PASSING),
        ],
    );
    let out = colibri(&["test", dir.to_str().unwrap()]);
    let t = text(&out);
    assert_eq!(out.status.code(), Some(0), "{t}");
    assert!(t.contains("target.ok::t ... ok"), "{t}");
    assert!(t.contains("node_modules.ok2::t ... ok"), "{t}");
    assert!(!t.contains("bad"), "{t}");
}

#[test]
fn an_explicit_import_still_reaches_into_a_cache_folder() {
    let dir = folder(
        "import",
        &[
            ("colibri.toml", "[package]\nname = \"demo\"\n"),
            (
                "main.colibri",
                "import cache.c\nfn main() {\n    print(c.one())\n}\n",
            ),
            ("cache/CACHEDIR.TAG", TAG),
            ("cache/c.colibri", "pub fn one() -> int => 1\n"),
        ],
    );
    let out = colibri(&["check", dir.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    let out = colibri(&["run", dir.join("main.colibri").to_str().unwrap()]);
    assert_eq!(text(&out).trim(), "1", "{}", text(&out));
}
