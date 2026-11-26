//! `colibri build --web` and `colibri check --web`: the folder that is written, the runner it
//! embeds, and what a page cannot have (compile errors for `fs` and the others, `task.run`,
//! `task.tick` from `main`).
//!
//! No wasm is built here: the runner is a stand-in file that is a WebAssembly module in the one
//! way the build looks at (its first four bytes). The page itself, with the real runner, is run in
//! node by CI (`examples/web/smoke.mjs`) and by `crates/colibri-web`'s harness.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

struct Out {
    stderr: String,
    code: i32,
}

/// A fresh folder, removed at the end.
struct Dir(PathBuf);

impl Dir {
    fn new(name: &str) -> Dir {
        let dir = std::env::temp_dir().join(format!("colibri-web-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("home")).unwrap();
        Dir(dir)
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.0.join(rel)
    }

    fn write(&self, rel: &str, text: &str) {
        self.write_bytes(rel, text.as_bytes());
    }

    fn write_bytes(&self, rel: &str, bytes: &[u8]) {
        let full = self.0.join(rel);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, bytes).unwrap();
    }

    fn colibri(&self, args: &[&str]) -> Out {
        let out = Command::new(env!("CARGO_BIN_EXE_colibri"))
            .args(args)
            .current_dir(&self.0)
            .env("COLIBRI_HOME", self.path("home"))
            .output()
            .expect("colibri runs");
        Out {
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            code: out.status.code().unwrap_or(-1),
        }
    }

    /// `colibri build --web ARGS` with the stand-in runner.
    fn build_web(&self, args: &[&str]) -> Out {
        let mut all = vec!["build", "--web", "--web-runner", "runner.wasm"];
        all.extend_from_slice(args);
        self.colibri(&all)
    }

    /// `colibri check --web FILE`.
    fn check_web(&self, file: &str) -> Out {
        self.colibri(&["check", "--web", file])
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A module that is a module only by its first four bytes.
const RUNNER: &[u8] = b"\0asm\x01\0\0\0stand-in for colibri.wasm";

const HELLO: &str = "fn main() {\n    print(\"hello page\")\n}\n";

/// A gzip file of `data` made of one stored block (what a level-0 `gzip` writes).
fn gzip_stored(data: &[u8]) -> Vec<u8> {
    let mut crc_table = [0u32; 256];
    for (n, slot) in crc_table.iter_mut().enumerate() {
        let mut c = n as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 {
                0xedb8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
        *slot = c;
    }
    let crc = !data.iter().fold(!0u32, |crc, &b| {
        crc_table[((crc ^ u32::from(b)) & 0xff) as usize] ^ (crc >> 8)
    });
    let mut out = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 3, 1];
    out.extend_from_slice(&(data.len() as u16).to_le_bytes());
    out.extend_from_slice(&(!(data.len() as u16)).to_le_bytes());
    out.extend_from_slice(data);
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out
}

fn pack_of(dir: &Dir, rel: &str) -> colibri_lang::pack::Pack {
    let bytes = fs::read(dir.path(rel)).unwrap();
    colibri_lang::pack::Pack::from_bytes(&bytes).expect("app.pack is a pack")
}

#[test]
fn a_web_build_writes_the_page_the_script_the_runner_and_the_pack() {
    let dir = Dir::new("layout");
    dir.write("hello.colibri", HELLO);
    dir.write_bytes("runner.wasm", RUNNER);
    let out = dir.build_web(&["hello.colibri", "-o", "site"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    let mut names: Vec<String> = fs::read_dir(dir.path("site"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(
        names,
        ["app.pack", "colibri.js", "colibri.wasm", "index.html"]
    );
    assert_eq!(fs::read(dir.path("site/colibri.wasm")).unwrap(), RUNNER);
    // colibri.js is the one `crates/colibri-web` tests.
    let script = fs::read_to_string(dir.path("site/colibri.js")).unwrap();
    assert_eq!(
        script,
        include_str!("../../colibri-web/js/colibri.js"),
        "the page loads the script the harness tests"
    );
    let page = fs::read_to_string(dir.path("site/index.html")).unwrap();
    for needed in [
        "./colibri.js",
        "colibri.wasm",
        "app.pack",
        "id=\"colibri-output\"",
        "<title>hello</title>",
    ] {
        assert!(page.contains(needed), "index.html lacks {needed}:\n{page}");
    }
    assert!(out.stderr.contains("1 source file"), "{}", out.stderr);
    // The pack holds the program, for the web: no runner in front of it.
    let pack = pack_of(&dir, "site/app.pack");
    assert_eq!(pack.cfg.os, "web");
    assert_eq!(pack.cfg.arch, "wasm32");
    assert_eq!(pack.files.len(), 1);
    assert_eq!(pack.files[0].data, HELLO.as_bytes());
}

#[test]
fn the_default_folder_is_named_after_the_program() {
    let dir = Dir::new("default");
    dir.write("clock.colibri", HELLO);
    dir.write_bytes("runner.wasm", RUNNER);
    let out = dir.build_web(&["clock.colibri"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(dir.path("clock-web/index.html").is_file());
}

#[test]
fn a_gzipped_runner_is_written_as_the_module_it_holds() {
    let dir = Dir::new("gzip");
    dir.write("hello.colibri", HELLO);
    dir.write_bytes("runner.wasm", &gzip_stored(RUNNER));
    let out = dir.build_web(&["hello.colibri", "-o", "site"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(fs::read(dir.path("site/colibri.wasm")).unwrap(), RUNNER);
}

#[test]
fn a_runner_that_is_not_a_module_or_is_damaged_builds_nothing() {
    let dir = Dir::new("badrunner");
    dir.write("hello.colibri", HELLO);
    dir.write_bytes("runner.wasm", b"MZ this is not a module");
    let out = dir.build_web(&["hello.colibri", "-o", "site"]);
    assert_eq!(out.code, 1);
    assert!(
        out.stderr.contains("not a WebAssembly module"),
        "{}",
        out.stderr
    );
    let mut damaged = gzip_stored(RUNNER);
    let n = damaged.len();
    damaged[n - 8] ^= 0xff;
    dir.write_bytes("runner.wasm", &damaged);
    let out = dir.build_web(&["hello.colibri", "-o", "site"]);
    assert_eq!(out.code, 1);
    assert!(out.stderr.contains("not valid gzip data"), "{}", out.stderr);
    assert!(!dir.path("site").exists(), "nothing is written");
}

/// Without `COLIBRI_WEB_RUNNER` at build time (what `cargo test` is), the tool says what to do.
/// With it (a release pipeline running the tests), the runner inside is a real module.
#[test]
fn the_runner_inside_the_tool_is_there_or_the_tool_says_how_to_get_it() {
    let dir = Dir::new("embedded");
    dir.write("hello.colibri", HELLO);
    let out = dir.colibri(&["build", "--web", "hello.colibri", "-o", "site"]);
    if option_env!("COLIBRI_WEB_RUNNER").is_some_and(|p| !p.is_empty()) {
        assert_eq!(out.code, 0, "{}", out.stderr);
        let wasm = fs::read(dir.path("site/colibri.wasm")).unwrap();
        assert!(
            wasm.starts_with(b"\0asm"),
            "the embedded runner is a module"
        );
    } else {
        assert_eq!(out.code, 1);
        assert!(
            out.stderr
                .contains("built without the web runner; build `crates/colibri-web` first"),
            "{}",
            out.stderr
        );
        assert!(!dir.path("site").exists(), "nothing is written");
    }
}

#[test]
fn a_web_build_decides_when_os_conditions_for_the_web() {
    let dir = Dir::new("when");
    dir.write(
        "prog.colibri",
        "@when(os == \"web\")\nimport webmod as plat\n@when(os != \"web\")\nimport othermod as plat\n\nfn main() {\n    print(plat.name())\n}\n",
    );
    dir.write("webmod.colibri", "pub fn name() -> str => \"web\"\n");
    dir.write("othermod.colibri", "pub fn name() -> str => \"other\"\n");
    dir.write_bytes("runner.wasm", RUNNER);
    let out = dir.build_web(&["prog.colibri", "-o", "site"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    let pack = pack_of(&dir, "site/app.pack");
    let mut paths: Vec<&str> = pack.files.iter().map(|f| f.path.as_str()).collect();
    paths.sort();
    assert_eq!(paths.len(), 2, "{paths:?}");
    assert!(paths.iter().any(|p| p.ends_with("webmod.colibri")));
    assert!(!paths.iter().any(|p| p.ends_with("othermod.colibri")));
}

#[test]
fn the_assets_of_a_package_are_in_the_pack() {
    let dir = Dir::new("assets");
    dir.write(
        "app/colibri.toml",
        "[package]\nname = \"app\"\n\n[build]\nassets = [\"data/\"]\n",
    );
    dir.write(
        "app/main.colibri",
        "import assets\nfn main() {\n    print(assets.read(\"data/a.txt\").expect(\"a\"))\n}\n",
    );
    dir.write("app/data/a.txt", "alpha");
    dir.write("app/data/b/c.txt", "gamma");
    dir.write_bytes("runner.wasm", RUNNER);
    let out = dir.build_web(&["app", "-o", "site"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(out.stderr.contains("2 assets"), "{}", out.stderr);
    let pack = pack_of(&dir, "site/app.pack");
    let paths: Vec<&str> = pack.assets.iter().map(|a| a.path.as_str()).collect();
    assert_eq!(paths, ["data/a.txt", "data/b/c.txt"]);
    // The title is the folder's name, which `index.html` escapes.
    let page = fs::read_to_string(dir.path("site/index.html")).unwrap();
    assert!(page.contains("<title>app</title>"), "{page}");
}

#[test]
fn the_output_may_not_be_a_file_or_an_asset_and_other_files_in_the_folder_stay() {
    let dir = Dir::new("clobber");
    dir.write("hello.colibri", HELLO);
    dir.write_bytes("runner.wasm", RUNNER);
    // A file where the folder should be.
    dir.write("site", "a file");
    let out = dir.build_web(&["hello.colibri", "-o", "site"]);
    assert_eq!(out.code, 1);
    assert!(out.stderr.contains("is not a folder"), "{}", out.stderr);
    assert_eq!(fs::read_to_string(dir.path("site")).unwrap(), "a file");
    // An asset of the program in the place of one of the four files.
    dir.write(
        "pkg3/colibri.toml",
        "[package]\nname = \"pkg3\"\n\n[build]\nassets = [\"app.pack\"]\n",
    );
    dir.write("pkg3/main.colibri", "fn main() {}\n");
    dir.write("pkg3/app.pack", "an asset");
    let out = dir.build_web(&["pkg3", "-o", "pkg3"]);
    assert_eq!(out.code, 1);
    assert!(out.stderr.contains("app.pack"), "{}", out.stderr);
    assert_eq!(
        fs::read_to_string(dir.path("pkg3/app.pack")).unwrap(),
        "an asset"
    );
    // ... and so is an asset in the place of the script or the page.
    dir.write(
        "pkg4/colibri.toml",
        "[package]\nname = \"pkg4\"\n\n[build]\nassets = [\"colibri.js\"]\n",
    );
    dir.write("pkg4/main.colibri", "fn main() {}\n");
    dir.write("pkg4/colibri.js", "an asset");
    let out = dir.build_web(&["pkg4", "-o", "pkg4"]);
    assert_eq!(out.code, 1);
    assert!(
        out.stderr.contains("is the asset `colibri.js`"),
        "{}",
        out.stderr
    );
    assert_eq!(
        fs::read_to_string(dir.path("pkg4/colibri.js")).unwrap(),
        "an asset"
    );
    // A file that cannot be replaced fails the build and leaves no temporary file behind.
    fs::create_dir_all(dir.path("site3/index.html")).unwrap();
    let out = dir.build_web(&["hello.colibri", "-o", "site3"]);
    assert_eq!(out.code, 1, "{}", out.stderr);
    assert!(out.stderr.contains("cannot write"), "{}", out.stderr);
    let leftovers: Vec<String> = fs::read_dir(dir.path("site3"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".colibri-build"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "temporary files are gone: {leftovers:?}"
    );
    // A page of somebody's own is not replaced; one of an earlier build is.
    dir.write("site4/index.html", "<p>mine</p>");
    let out = dir.build_web(&["hello.colibri", "-o", "site4"]);
    assert_eq!(out.code, 1);
    assert!(
        out.stderr.contains("not a page written by"),
        "{}",
        out.stderr
    );
    assert_eq!(
        fs::read_to_string(dir.path("site4/index.html")).unwrap(),
        "<p>mine</p>"
    );
    assert!(
        !dir.path("site4/app.pack").exists(),
        "nothing else was written"
    );
    // A folder that holds files already: the page's files are replaced, others stay.
    dir.write("site2/notes.txt", "mine");
    dir.write(
        "site2/index.html",
        "<!-- written by colibri build --web -->an older page",
    );
    let out = dir.build_web(&["hello.colibri", "-o", "site2"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(
        fs::read_to_string(dir.path("site2/notes.txt")).unwrap(),
        "mine"
    );
    let page = fs::read_to_string(dir.path("site2/index.html")).unwrap();
    assert!(page.contains("colibri.js"), "{page}");
    let leftovers: Vec<String> = fs::read_dir(dir.path("site2"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".colibri-build"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "temporary files are gone: {leftovers:?}"
    );
}

#[test]
fn a_source_that_is_one_of_the_four_files_is_refused() {
    let dir = Dir::new("source-name");
    dir.write_bytes("runner.wasm", RUNNER);
    // The entry file itself is named like a file of the page.
    dir.write("index.html", "fn main() {}\n");
    let out = dir.build_web(&["index.html", "-o", "."]);
    assert_eq!(out.code, 1);
    assert!(out.stderr.contains("is the source file"), "{}", out.stderr);
    assert_eq!(
        fs::read_to_string(dir.path("index.html")).unwrap(),
        "fn main() {}\n"
    );
    assert!(!dir.path("colibri.js").exists(), "nothing else was written");
}

/// The modules a page does not have, and the one message for them.
#[test]
fn the_modules_that_reach_the_machine_are_compile_errors_on_the_web() {
    let dir = Dir::new("modules");
    dir.write_bytes("runner.wasm", RUNNER);
    for module in ["fs", "os", "io", "process", "net", "http"] {
        dir.write(
            "prog.colibri",
            &format!("import {module}\n\nfn main() {{\n    print(\"x\")\n}}\n"),
        );
        let out = dir.check_web("prog.colibri");
        assert_eq!(out.code, 1, "{module}: {}", out.stderr);
        assert!(
            out.stderr
                .contains(&format!("`{module}` is not available in a web build")),
            "{module}: {}",
            out.stderr
        );
        // The build says it too, and writes nothing.
        let out = dir.build_web(&["prog.colibri", "-o", "site"]);
        assert_eq!(out.code, 1, "{module}: {}", out.stderr);
        assert!(
            out.stderr
                .contains(&format!("`{module}` is not available in a web build")),
            "{module}: {}",
            out.stderr
        );
        assert!(!dir.path("site").exists());
    }
    // Written with the prefix, and the web server written in Colibri that rests on `net`.
    for text in ["import std.fs\n", "import httpd\n"] {
        dir.write("prog.colibri", &format!("{text}\nfn main() {{}}\n"));
        let out = dir.check_web("prog.colibri");
        assert_eq!(out.code, 1, "{text}: {}", out.stderr);
        assert!(
            out.stderr.contains("is not available in a web build"),
            "{}",
            out.stderr
        );
    }
    // The same program is fine natively, and so is a program for the web without them.
    dir.write(
        "prog.colibri",
        "import fs\n\nfn main() {\n    print(fs.exists(\".\"))\n}\n",
    );
    assert_eq!(dir.colibri(&["check", "prog.colibri"]).code, 0);
}

#[test]
fn the_modules_a_page_has_are_there() {
    let dir = Dir::new("subset");
    let imports = [
        "math", "json", "str", "regex", "rand", "time", "datetime", "vec", "base64", "hex", "hash",
        "toml", "csv", "assets", "path", "args",
    ];
    let text: String = imports.iter().map(|m| format!("import {m}\n")).collect();
    dir.write("prog.colibri", &format!("{text}\nfn main() {{}}\n"));
    let out = dir.check_web("prog.colibri");
    assert_eq!(out.code, 0, "{}", out.stderr);
}

#[test]
fn a_file_of_the_program_named_like_a_missing_module_is_still_a_module() {
    let dir = Dir::new("shadow");
    dir.write("fs.colibri", "pub fn exists() -> bool => true\n");
    dir.write(
        "prog.colibri",
        "import fs\n\nfn main() {\n    print(fs.exists())\n}\n",
    );
    let out = dir.check_web("prog.colibri");
    assert_eq!(out.code, 0, "{}", out.stderr);
}

#[test]
fn the_clocks_are_there_and_sleeping_is_not() {
    let dir = Dir::new("clocks");
    dir.write(
        "prog.colibri",
        "import datetime\nimport time\n\nfn main() {\n    print(time.unix())\n    print(datetime.to_iso(datetime.now_local()))\n}\n",
    );
    assert_eq!(dir.check_web("prog.colibri").code, 0);
    dir.write(
        "prog.colibri",
        "import time\n\nfn main() {\n    time.sleep(1.0)\n}\n",
    );
    let out = dir.check_web("prog.colibri");
    assert_eq!(out.code, 1);
    assert!(out.stderr.contains("sleep"), "{}", out.stderr);
    assert!(
        out.stderr.contains("use") || out.stderr.contains("`wait(seconds)`"),
        "{}",
        out.stderr
    );
    assert_eq!(
        dir.colibri(&["check", "prog.colibri"]).code,
        0,
        "natively it is fine"
    );
}

#[test]
fn foreign_functions_are_refused_on_the_web() {
    let dir = Dir::new("ffi");
    dir.write(
        "prog.colibri",
        "extern \"c\" {\n    fn abs(x: i32) -> i32\n}\n\nfn main() {\n    print(unsafe { abs(-3) })\n}\n",
    );
    let out = dir.check_web("prog.colibri");
    assert_eq!(out.code, 1, "{}", out.stderr);
    assert!(out.stderr.contains("ffi"), "{}", out.stderr);
}

#[test]
fn thread_tasks_are_a_compile_error_on_the_web() {
    let dir = Dir::new("task-run");
    dir.write(
        "prog.colibri",
        "fn work() -> int => 1\n\nfn main() {\n    let t = task.run(work)\n}\n",
    );
    let out = dir.check_web("prog.colibri");
    assert_eq!(out.code, 1, "{}", out.stderr);
    assert!(
        out.stderr
            .contains("thread tasks are not available in a web build"),
        "{}",
        out.stderr
    );
    // Natively the checker accepts it (the unused `t` is a warning).
    let out = dir.colibri(&["check", "prog.colibri"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(!out.stderr.contains("web build"), "{}", out.stderr);
    // Wherever it is written, and whatever it is given, the web refuses it, and the channel and
    // `Shared` that go with it are checked as usual.
    let programs = [
        "fn main() {\n    let t = task.run(fn() => 1)\n    print(t.done())\n}\n",
        "let t = task.run(fn() => 1)\n",
        "fn go_on() {\n    let step = fn() {\n        let t = task.run(fn() { print(1) })\n        print(t.done())\n    }\n    step()\n}\n\nfn main() {\n    go_on()\n}\n",
    ];
    for (n, text) in programs.iter().enumerate() {
        let file = format!("p{n}.colibri");
        dir.write(&file, text);
        let out = dir.check_web(&file);
        assert_eq!(out.code, 1, "{file}: {}", out.stderr);
        assert!(
            out.stderr
                .contains("thread tasks are not available in a web build"),
            "{file}: {}",
            out.stderr
        );
        assert_eq!(
            dir.colibri(&["check", &file]).code,
            0,
            "{file} is fine natively"
        );
    }
}

#[test]
fn ticking_is_a_compile_error_everywhere_on_the_web_and_fine_natively() {
    let dir = Dir::new("tick");
    // In `main`, in the top level of a script, in a helper, in a closure and in a method.
    let programs = [
        "fn main() {\n    task.tick(0.1)\n}\n",
        "task.tick(0.1)\n",
        "fn pump() {\n    task.tick(0.1)\n}\n\nfn main() {\n    pump()\n}\n",
        "fn main() {\n    let step = fn() {\n        task.tick(0.1)\n    }\n    step()\n}\n",
        "struct S {}\n\nimpl S {\n    fn step(self) {\n        task.tick(0.1)\n    }\n}\n\nfn main() {\n    print(1)\n}\n",
    ];
    for (n, text) in programs.iter().enumerate() {
        let file = format!("p{n}.colibri");
        dir.write(&file, text);
        let out = dir.check_web(&file);
        assert_eq!(out.code, 1, "{file}: {}", out.stderr);
        assert!(
            out.stderr
                .contains("`task.tick` is not available in a web build"),
            "{file}: {}",
            out.stderr
        );
        assert_eq!(
            dir.colibri(&["check", &file]).code,
            0,
            "{file} is fine natively"
        );
    }
}

#[test]
fn web_flags_do_not_mix_with_the_targets_of_executables() {
    let dir = Dir::new("flags");
    dir.write("hello.colibri", HELLO);
    dir.write_bytes("runner.wasm", RUNNER);
    for args in [
        vec![
            "build",
            "--web",
            "--target",
            "linux-x86_64",
            "hello.colibri",
        ],
        vec!["build", "--web", "--runner", "runner.wasm", "hello.colibri"],
        vec!["build", "--web-runner", "runner.wasm", "hello.colibri"],
    ] {
        let out = dir.colibri(&args);
        assert_eq!(out.code, 2, "{args:?}: {}", out.stderr);
    }
}

/// The demo page (`examples/web`, built and run under node by CI) is a program that checks and
/// builds for the web, and prints natively what its page shows, except for the one line that says
/// where it runs.
#[test]
fn the_demo_page_checks_builds_and_prints_what_the_smoke_test_expects() {
    let demo = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/web");
    let demo = demo.to_str().unwrap();
    let dir = Dir::new("demo");
    dir.write_bytes("runner.wasm", RUNNER);
    assert_eq!(dir.check_web(demo).code, 0);
    let out = dir.build_web(&[demo, "-o", "site"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    let pack = pack_of(&dir, "site/app.pack");
    assert_eq!(pack.assets.len(), 1);
    let run = Command::new(env!("CARGO_BIN_EXE_colibri"))
        .args(["run", demo])
        .output()
        .expect("colibri runs");
    let printed = String::from_utf8_lossy(&run.stdout).replace("os: not the web", "os: web");
    let expected = fs::read_to_string(std::path::Path::new(demo).join("expected.txt")).unwrap();
    assert_eq!(printed, expected);
}

/// With `assets = ["**"]` in a package, the folder of the first build is inside the package, and
/// the second build must not take what the first wrote for assets.
#[test]
fn building_twice_into_the_default_folder_with_a_broad_asset_pattern_works() {
    let dir = Dir::new("twice");
    dir.write(
        "app/colibri.toml",
        "[package]\nname = \"app\"\n\n[build]\nassets = [\"**\"]\n",
    );
    dir.write("app/main.colibri", "fn main() {}\n");
    dir.write("app/data.txt", "one");
    dir.write_bytes("runner.wasm", RUNNER);
    for round in 0..2 {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_colibri"));
        cmd.args(["build", "--web", "--web-runner", "../runner.wasm"])
            .current_dir(dir.path("app"))
            .env("COLIBRI_HOME", dir.path("home"));
        let out = cmd.output().unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "round {round}: {err}");
    }
    let pack = pack_of(&dir, "app/app-web/app.pack");
    let mut paths: Vec<&str> = pack.assets.iter().map(|a| a.path.as_str()).collect();
    paths.sort();
    assert_eq!(paths, ["colibri.toml", "data.txt", "main.colibri"]);
}
