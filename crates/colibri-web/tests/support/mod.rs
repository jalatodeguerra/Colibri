//! Shared by the native tests and by `examples/make_packs.rs`: a golden test as a pack and the
//! output it must print.

use std::path::{Path, PathBuf};

use colibri_pack::{Kind, Pack, PackFile, Start};
use colibri_runtime::BuildCfg;

/// The folder of the golden tests of the whole project.
pub fn golden_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/golden")
}

/// The folder of the programs that only the web runner has (timers, ordering of waits).
pub fn web_tests_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("web-tests")
}

/// What `// expect:` lines of a program make: its whole standard output.
pub fn expected_output(source: &str) -> String {
    let directives = colibri_lang::golden::parse_directives(source);
    directives.expect.iter().map(|line| format!("{line}\n")).collect()
}

/// A program (one file) as a pack, what `colibri build --web` writes for it. The configuration
/// is the one of a golden test (a debug build), on the web.
pub fn pack_of(name: &str, source: &str) -> Vec<u8> {
    let pack = Pack {
        name: name.to_string(),
        start: Start::File("main.colibri".to_string()),
        cfg: BuildCfg {
            os: "web".to_string(),
            arch: "wasm32".to_string(),
            debug: true,
            features: Vec::new(),
            no_default_features: false,
        },
        files: vec![PackFile {
            kind: Kind::Source,
            path: "main.colibri".to_string(),
            name: format!("{name}.colibri"),
            data: source.as_bytes().to_vec(),
        }],
        assets: Vec::new(),
        assets_at: 0,
    };
    pack.append_held(0).expect("a pack with no files on disk writes")
}

/// The golden programs the web runner runs, from `web-goldens.txt` (paths below `tests/golden`),
/// and the programs of `web-tests`: their names, sources and pack bytes. A name has no `/`.
pub fn cases() -> Vec<(String, String)> {
    let list = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("web-goldens.txt"))
        .expect("web-goldens.txt");
    let mut found = Vec::new();
    for line in list.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        let path = golden_dir().join(line);
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("web-goldens.txt names {line}: {e}"));
        let name = line.trim_end_matches(".colibri").replace('/', "__");
        found.push((name, source));
    }
    let mut own: Vec<PathBuf> = std::fs::read_dir(web_tests_dir())
        .expect("web-tests")
        .map(|e| e.expect("entry").path())
        .filter(|p| p.extension().is_some_and(|e| e == "colibri"))
        .collect();
    own.sort();
    for path in own {
        let name = format!("web__{}", path.file_stem().expect("stem").to_string_lossy());
        found.push((name, std::fs::read_to_string(&path).expect("read")));
    }
    found
}

/// `// virtual-ms: N` in a program of `web-tests`: how many milliseconds of the page's clock the
/// run takes at least (the harness's clock is virtual, so it is exact): a program that waits
/// for a second must not end before a second has passed, nor much after.
pub fn virtual_ms(source: &str) -> Option<u64> {
    source
        .lines()
        .find_map(|l| l.trim().strip_prefix("// virtual-ms:"))
        .and_then(|n| n.trim().parse().ok())
}

/// `// web-exit: N` in a program of `web-tests`: it must fail, with this exit code (`101` for a
/// panic, `1` for any other failure). Without it the program must end normally.
pub fn web_exit(source: &str) -> Option<i32> {
    directive(source, "web-exit").and_then(|n| n.parse().ok())
}

/// `// web-stderr: text` in a program of `web-tests`: what the error stream must contain.
pub fn web_stderr(source: &str) -> Option<String> {
    directive(source, "web-stderr")
}

fn directive(source: &str, name: &str) -> Option<String> {
    let key = format!("// {name}:");
    source
        .lines()
        .find_map(|l| l.trim().strip_prefix(key.as_str()))
        .map(|rest| rest.trim().to_string())
}
