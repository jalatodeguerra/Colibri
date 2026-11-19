//! The standard library through the runtime API: installing it, capability gating, how
//! `import math` is resolved next to local files and dependencies, and a hermetic
//! [`StdConfig`]. The behaviour of each module is covered by the golden tests in
//! `tests/golden/std`.

use std::collections::BTreeMap;
use std::io::Cursor;

use colibri_runtime as colibri;
use colibri_std::{EnvSource, MODULES, ManualClock, StdConfig};

fn vm(caps: &[colibri::Cap]) -> (colibri::Vm, colibri::SharedBuffer) {
    let out = colibri::SharedBuffer::new();
    let mut builder = colibri::Vm::builder().stdout(Box::new(out.clone()));
    for cap in caps {
        builder = builder.allow(*cap);
    }
    let mut vm = builder.build();
    colibri_std::install(&mut vm).expect("installs");
    (vm, out)
}

fn run(vm: &mut colibri::Vm, source: &str) -> Result<(), colibri::Error> {
    vm.load_source("app/main.colibri", source)?;
    vm.run_main()
}

fn compile_error(vm: &mut colibri::Vm, source: &str) -> String {
    vm.load_source("app/main.colibri", source)
        .expect_err("should not compile")
        .render(false)
}

#[test]
fn every_module_installs_and_is_listed() {
    let (vm, _) = vm(&[]);
    let names = vm.module_names();
    assert_eq!(names[0], "host");
    let installed: Vec<&str> = names[1..]
        .iter()
        .map(|n| n.strip_prefix("std.").expect("a std module"))
        .collect();
    let mut wanted: Vec<&str> = MODULES.to_vec();
    let mut got = installed.clone();
    wanted.sort_unstable();
    got.sort_unstable();
    assert_eq!(got, wanted);
}

#[test]
fn installing_twice_is_an_error() {
    let (mut vm, _) = vm(&[]);
    assert!(colibri_std::install(&mut vm).is_err());
}

#[test]
fn without_install_there_is_no_standard_library() {
    let mut vm = colibri::Vm::new();
    let err = compile_error(&mut vm, "import math\nfn main() {}\n");
    assert!(err.contains("no module `math`"), "{err}");
}

// ---- capabilities ----

/// A call in each module that reaches outside the program, and the capability it needs.
const GATED: &[(&str, &str, &str, colibri::Cap)] = &[
    ("fs", "fs.read", "fs.read(\"x\")", colibri::Cap::Fs),
    ("fs", "fs.write", "fs.write(\"x\", \"y\")", colibri::Cap::Fs),
    ("fs", "fs.exists", "fs.exists(\"x\")", colibri::Cap::Fs),
    ("fs", "fs.list", "fs.list(\"x\")", colibri::Cap::Fs),
    (
        "fs",
        "fs.remove_dir",
        "fs.remove_dir(\"x\")",
        colibri::Cap::Fs,
    ),
    ("os", "os.env", "os.env(\"A\")", colibri::Cap::Os),
    ("os", "os.vars", "os.vars()", colibri::Cap::Os),
    (
        "os",
        "os.set_env",
        "os.set_env(\"A\", \"b\")",
        colibri::Cap::Os,
    ),
    ("os", "os.exit", "os.exit(0)", colibri::Cap::Os),
    ("os", "os.temp_dir", "os.temp_dir()", colibri::Cap::Os),
    ("os", "os.cwd", "os.cwd()", colibri::Cap::Os),
    ("time", "time.sleep", "time.sleep(0.0)", colibri::Cap::Os),
    ("time", "time.unix", "time.unix()", colibri::Cap::Os),
    ("io", "io.read_line", "io.read_line()", colibri::Cap::Os),
    ("io", "io.read_all", "io.read_all()", colibri::Cap::Os),
];

#[test]
fn every_gated_function_is_refused_without_its_capability() {
    for (module, shown, call, cap) in GATED {
        let (mut vm, _) = vm(&[]);
        let source = format!("import {module}\nfn main() {{\n    let _ = {call}\n}}\n");
        let err = compile_error(&mut vm, &source);
        let wanted = format!(
            "`{shown}` needs the `{}` capability, which this host did not grant",
            cap.name()
        );
        assert!(err.contains(&wanted), "{call}: {err}");
        assert!(err.contains("app/main.colibri:3:"), "{call}: {err}");
    }
}

#[test]
fn the_other_capability_does_not_help() {
    for (module, _, call, cap) in GATED {
        let other = if *cap == colibri::Cap::Fs {
            colibri::Cap::Os
        } else {
            colibri::Cap::Fs
        };
        let (mut vm, _) = vm(&[other, colibri::Cap::Net]);
        let source = format!("import {module}\nfn main() {{\n    let _ = {call}\n}}\n");
        assert!(
            vm.load_source("app/main.colibri", &source).is_err(),
            "{call}"
        );
    }
}

#[test]
fn a_granted_capability_makes_each_function_compile() {
    for (module, _, call, cap) in GATED {
        let (mut vm, _) = vm(&[*cap]);
        let source = format!("import {module}\nfn main() {{\n    let _ = {call}\n}}\n");
        // Compile only: running some of them would touch the machine.
        vm.load_source("app/main.colibri", &source)
            .unwrap_or_else(|e| panic!("{call}: {}", e.render(false)));
    }
}

#[test]
fn the_ungated_modules_need_no_capability() {
    let (mut vm, out) = vm(&[]);
    run(
        &mut vm,
        "import math\nimport path\nimport json\nimport str\nimport rand\nimport time\nimport io\nimport os\nfn main() {\n    print(math.floor(1.5))\n    print(path.join(\"a\", \"b\"))\n    print(json.encode(1))\n    print(str.repeat(\"x\", 2))\n    rand.seed(1)\n    print(rand.int(0, 1))\n    print(time.monotonic() >= 0)\n    io.write(\"w\\n\")\n    print(os.args())\n}\n",
    )
    .unwrap();
    assert_eq!(out.contents(), "1.0\na/b\nok(\"1\")\nxx\n0\ntrue\nw\n[]\n");
}

#[test]
fn an_ungranted_function_is_not_installed_at_all() {
    // The checker refuses every call, so the slot is never reached; a program that does not
    // call it loads fine.
    let (mut vm, out) = vm(&[]);
    run(
        &mut vm,
        "import fs\nfn main() {\n    if false {\n        print(fs.exists(\"x\"))\n    }\n}\n",
    )
    .expect_err("the call is still an error even when it cannot run");
    assert_eq!(out.contents(), "");
}

// ---- resolution ----

#[test]
fn a_local_file_with_the_name_of_a_std_module_makes_the_import_ambiguous() {
    let (mut vm, _) = vm(&[]);
    let err = vm
        .load_sources(&[
            ("app/main.colibri", "import math\nfn main() {}\n"),
            (
                "app/math.colibri",
                "pub fn double(x: int) -> int => x * 2\n",
            ),
        ])
        .unwrap_err()
        .render(false);
    assert!(err.contains("`import math` is ambiguous"), "{err}");
    assert!(err.contains("the file `app/math.colibri`"), "{err}");
    assert!(
        err.contains("the standard library module `std.math`"),
        "{err}"
    );
    assert!(err.contains("import std.math"), "{err}");
}

#[test]
fn the_std_prefix_picks_the_standard_library_whatever_files_exist() {
    let (mut vm, out) = vm(&[]);
    vm.load_sources(&[
        (
            "app/main.colibri",
            "import std.math\nfn main() {\n    print(math.floor(2.5))\n}\n",
        ),
        (
            "app/math.colibri",
            "pub fn floor(x: float) -> float => -1.0\n",
        ),
        (
            "app/std/math.colibri",
            "pub fn floor(x: float) -> float => -2.0\n",
        ),
    ])
    .unwrap();
    vm.run_main().unwrap();
    assert_eq!(out.contents(), "2.0\n");
}

#[test]
fn a_local_file_with_another_name_is_unaffected() {
    let (mut vm, out) = vm(&[]);
    vm.load_sources(&[
        (
            "app/main.colibri",
            "import math\nimport mymath\nfn main() {\n    print(math.floor(2.5))\n    print(mymath.double(4))\n}\n",
        ),
        ("app/mymath.colibri", "pub fn double(x: int) -> int => x * 2\n"),
    ])
    .unwrap();
    vm.run_main().unwrap();
    assert_eq!(out.contents(), "2.0\n8\n");
}

#[test]
fn a_dependency_named_like_a_std_module_is_ambiguous_until_the_prefix_is_used() {
    let files = |main: &'static str| {
        vec![
            ("app/main.colibri", main),
            (
                "app/colibri.toml",
                "[package]\nname = \"app\"\n\n[dependencies]\nmath = { path = \"deps/math\" }\n",
            ),
            ("app/deps/math/colibri.toml", "[package]\nname = \"math\"\n"),
            (
                "app/deps/math/lib.colibri",
                "pub fn floor(x: float) -> float => -1.0\n",
            ),
        ]
    };
    let (mut vm1, _) = vm(&[]);
    let err = vm1
        .load_sources(&files("import math\nfn main() {}\n"))
        .unwrap_err()
        .render(false);
    assert!(err.contains("`import math` is ambiguous"), "{err}");
    assert!(err.contains("the dependency `math`"), "{err}");
    let (mut vm2, out) = vm(&[]);
    vm2.load_sources(&files(
        "import std.math\nfn main() {\n    print(math.floor(2.5))\n}\n",
    ))
    .unwrap();
    vm2.run_main().unwrap();
    assert_eq!(out.contents(), "2.0\n");
}

#[test]
fn a_dependency_named_std_cannot_shadow_the_standard_library() {
    let (mut vm, out) = vm(&[]);
    vm.load_sources(&[
        (
            "app/main.colibri",
            "import std.math\nfn main() {\n    print(math.floor(2.5))\n}\n",
        ),
        (
            "app/colibri.toml",
            "[package]\nname = \"app\"\n\n[dependencies]\nstd = { path = \"deps/std\" }\n",
        ),
        ("app/deps/std/colibri.toml", "[package]\nname = \"std\"\n"),
        (
            "app/deps/std/math.colibri",
            "pub fn floor(x: float) -> float => -1.0\n",
        ),
    ])
    .unwrap();
    vm.run_main().unwrap();
    assert_eq!(out.contents(), "2.0\n");
}

#[test]
fn modules_that_are_not_built_say_when_they_come() {
    let (mut vm, _) = vm(&[]);
    for (module, wanted) in [
        ("task", "is not available until phase 2"),
        ("mem", "is not available until phase 3"),
        ("reflect", "is not available until phase 2"),
        ("game", "is not available until phase 2"),
        ("collections", "`List`, `Map` and `Set` are built in"),
    ] {
        for path in [module.to_string(), format!("std.{module}")] {
            let err = compile_error(&mut vm, &format!("import {path}\nfn main() {{}}\n"));
            assert!(
                err.contains(&format!("the standard library module `{module}`"))
                    && err.contains(wanted),
                "{path}: {err}"
            );
        }
    }
    let err = compile_error(&mut vm, "import std.nope\nfn main() {}\n");
    assert!(
        err.contains("the standard library has no module `nope`"),
        "{err}"
    );
    assert!(
        err.contains("the standard library modules are io, fs"),
        "{err}"
    );
}

// ---- the interface text ----

#[test]
fn the_interfaces_are_the_text_of_the_installed_modules() {
    let (mut vm, _) = vm(&[]);
    let interfaces = colibri_std::interfaces();
    assert_eq!(interfaces.len(), MODULES.len());
    for (name, interface) in MODULES.iter().zip(&interfaces) {
        let live = vm.std_module(name).interface_text();
        assert_eq!(interface.canonical_text(), live, "{name}");
        assert!(live.contains(&format!("module = \"std.{name}\"")), "{name}");
    }
    let fs = vm.std_module("fs").interface_text();
    assert!(fs.contains("caps = [\"fs\"]"), "{fs}");
    assert!(
        fs.contains("@requires(fs) fn read(path: str) -> Result[str]"),
        "{fs}"
    );
    let math = vm.std_module("math").interface_text();
    assert!(
        math.contains("const PI: float = 3.141592653589793"),
        "{math}"
    );
    let json = vm.std_module("json").interface_text();
    assert!(
        json.contains("@typed fn decode[T](text: str) -> Result[T]"),
        "{json}"
    );
    assert!(
        json.contains("fn encode[T](value: T) -> Result[str]"),
        "{json}"
    );
}

#[test]
fn a_program_checks_against_the_interfaces_without_a_vm() {
    let config = colibri::CheckConfig {
        interfaces: colibri_std::interfaces(),
        caps: colibri::CapSet::all(),
        ..colibri::CheckConfig::default()
    };
    let source = "import math\nimport fs\nimport json\nstruct P { x: int }\nfn main() {\n    print(math.sqrt(4.0))\n    print(fs.exists(\"a\"))\n    let p = json.decode[P](\"[]\")\n    print(math.nothing(1))\n}\n";
    let report = colibri::check_source_with("a.colibri", source, &config);
    let text = report.render(false);
    assert!(
        text.contains("`math` has no `nothing`") || text.contains("nothing"),
        "{text}"
    );
    assert_eq!(
        report.diagnostics.iter().filter(|d| d.is_error()).count(),
        1,
        "{text}"
    );
    // The grants are the config's: without `fs` the same source has a second error.
    let config = colibri::CheckConfig {
        caps: colibri::CapSet::none(),
        ..config
    };
    let report = colibri::check_source_with("a.colibri", source, &config);
    assert!(
        report
            .render(false)
            .contains("`fs.exists` needs the `fs` capability"),
        "{}",
        report.render(false)
    );
}

// ---- a hermetic world ----

#[test]
fn a_config_replaces_the_world() {
    let out = colibri::SharedBuffer::new();
    let mut vm = colibri::Vm::builder()
        .stdout(Box::new(out.clone()))
        .allow(colibri::Cap::Os)
        .build();
    colibri_std::install_with(
        &mut vm,
        StdConfig {
            clock: Box::new(ManualClock::new(100.0)),
            args: vec!["one".to_string()],
            env: EnvSource::Fixed(BTreeMap::from([("K".to_string(), "v".to_string())])),
            temp_dir: Some("/somewhere".into()),
            seed: Some(5),
            stdin: Some(Box::new(Cursor::new("a\nb"))),
        },
    )
    .unwrap();
    run(
        &mut vm,
        "import os\nimport time\nimport rand\nimport io\nfn main() {\n    print(os.args())\n    print(os.env(\"K\"))\n    print(os.env(\"PATH\"))\n    print(os.temp_dir())\n    time.sleep(2.5)\n    print(time.now())\n    print(time.unix())\n    print(rand.int(0, 1000))\n    print(io.read_line())\n    print(io.read_line())\n    print(io.read_line())\n}\n",
    )
    .unwrap();
    let first_draw = colibri_std::Rng::from_seed(5).range(0, 1000).unwrap();
    assert_eq!(
        out.contents(),
        format!(
            "[\"one\"]\nsome(\"v\")\nnone\n/somewhere\n2.5\n102.5\n{first_draw}\nok(some(\"a\"))\nok(some(\"b\"))\nok(none)\n"
        )
    );
}

#[test]
fn the_same_seed_gives_the_same_numbers_in_every_vm() {
    let draw = || {
        let (mut vm, out) = vm(&[]);
        run(
            &mut vm,
            "import rand\nfn main() {\n    rand.seed(2024)\n    print([rand.int(0, 1000), rand.int(0, 1000), rand.int(0, 1000)])\n    print(rand.float())\n}\n",
        )
        .unwrap();
        out.contents()
    };
    assert_eq!(draw(), draw());
}

#[test]
fn exit_code_is_what_the_script_asked_for() {
    let (mut vm, out) = vm(&[colibri::Cap::Os]);
    assert_eq!(colibri_std::exit_code(&mut vm), None);
    let err = run(
        &mut vm,
        "import os\nfn main() {\n    defer print(\"bye\")\n    os.exit(7)\n}\n",
    )
    .unwrap_err();
    assert!(
        matches!(&err, colibri::Error::Panic { message, .. } if message == "the program exited with code 7"),
        "{err:?}"
    );
    assert_eq!(colibri_std::exit_code(&mut vm), Some(7));
    assert_eq!(out.contents(), "bye\n");
}

#[test]
fn a_manual_clock_only_moves_when_told() {
    let mut clock = ManualClock::new(10.0);
    use colibri_std::Clock;
    assert_eq!(clock.monotonic_nanos(), 0);
    clock.sleep(0.5);
    assert_eq!(clock.monotonic_nanos(), 500_000_000);
    assert_eq!(clock.unix_seconds(), 10.5);
    clock.sleep(f64::NAN);
    clock.sleep(-1.0);
    assert_eq!(clock.monotonic_nanos(), 500_000_000);
}

#[test]
fn the_system_clock_never_goes_back_and_sleep_waits() {
    use colibri_std::{Clock, SystemClock};
    let mut clock = SystemClock::new();
    let a = clock.monotonic_nanos();
    clock.sleep(0.01);
    let b = clock.monotonic_nanos();
    assert!(b >= a + 10_000_000, "{a} {b}");
}

// ---- files, only in a folder of the test's own ----

struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!("colibri-std-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn files_are_written_and_read_in_a_scratch_folder() {
    let scratch = Scratch::new("files");
    let (mut vm, out) = vm(&[colibri::Cap::Fs]);
    let dir = scratch.0.to_string_lossy().replace('\\', "/");
    run(
        &mut vm,
        &format!(
            "import fs\nimport path\nfn main() -> Result[()] {{\n    let file = path.join(\"{dir}\", \"a.txt\")\n    fs.write(file, \"héllo\")?\n    print(fs.read(file)?)\n    print(fs.size(file)?)\n    print(fs.read(path.join(\"{dir}\", \"nope\")).is_err())\n    return ok(())\n}}\n"
        ),
    )
    .unwrap();
    assert_eq!(out.contents(), "héllo\n6\ntrue\n");
    assert_eq!(
        std::fs::read_to_string(scratch.0.join("a.txt")).unwrap(),
        "héllo"
    );
}

#[test]
fn a_missing_file_is_an_err_with_the_path_and_the_reason() {
    let scratch = Scratch::new("missing");
    let (mut vm, out) = vm(&[colibri::Cap::Fs]);
    let dir = scratch.0.to_string_lossy().replace('\\', "/");
    run(
        &mut vm,
        &format!(
            "import fs\nfn main() {{\n    match fs.read(\"{dir}/gone.txt\") {{\n        ok(t) => print(t)\n        err(e) => print(e.message())\n    }}\n}}\n"
        ),
    )
    .unwrap();
    assert_eq!(
        out.contents(),
        format!("cannot read `{dir}/gone.txt`: no such file or directory\n")
    );
}
