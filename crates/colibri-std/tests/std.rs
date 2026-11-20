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

/// A child that waits and then creates `marker`, as the program and the Colibri list literal of
/// its arguments: `sh -c "sleep S; touch marker"` on Unix; on Windows `cmd` with `ping -n N
/// 127.0.0.1` as the sleep (about N-1 seconds; `timeout` fails without a console), each word
/// its own argument so that nothing depends on how `cmd` reads quotes.
fn sleep_then_touch(unix_seconds: &str, pings: u32, marker: &std::path::Path) -> (String, String) {
    let marker = colibri_text(&marker.to_string_lossy());
    if cfg!(windows) {
        (
            "cmd".to_string(),
            format!(
                "[\"/C\", \"ping\", \"-n\", \"{pings}\", \"127.0.0.1\", \">nul\", \"&\", \"echo\", \"x\", \">\", \"{marker}\"]"
            ),
        )
    } else {
        (
            "sh".to_string(),
            format!("[\"-c\", \"sleep {unix_seconds}; touch {marker}\"]"),
        )
    }
}

/// Text for the inside of a Colibri string literal: `\` and `{` are special there.
fn colibri_text(text: &str) -> String {
    text.replace('\\', "\\\\").replace('{', "\\{")
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
    (
        "fs",
        "fs.remove_tree",
        "fs.remove_tree(\"x\")",
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
    (
        "http",
        "http.get",
        "http.get(\"http://x\")",
        colibri::Cap::Net,
    ),
    (
        "http",
        "http.post",
        "http.post(\"http://x\", \"b\")",
        colibri::Cap::Net,
    ),
    (
        "http",
        "http.request",
        "http.request(\"GET\", \"http://x\", {:}, \"\")",
        colibri::Cap::Net,
    ),
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
        // Everything but the one needed.
        let others: Vec<colibri::Cap> = [colibri::Cap::Fs, colibri::Cap::Os, colibri::Cap::Net]
            .into_iter()
            .filter(|c| c != cap)
            .collect();
        let (mut vm, _) = vm(&others);
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
        ("task", "does not exist yet"),
        ("mem", "does not exist yet"),
        ("reflect", "does not exist yet"),
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
            local_offset: None,
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
            "import fs\nimport path\nfn main() -> Result[()] {{\n    let file = path.join(\"{dir}\", \"a.txt\")\n    fs.write(file, \"héllo\")?\n    print(fs.read(file)?)\n    print(fs.stat(file)?.size)\n    print(fs.read(path.join(\"{dir}\", \"nope\")).is_err())\n    return ok(())\n}}\n"
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

// ---- args, process and datetime under other configurations ----

fn vm_with(config: StdConfig, caps: &[colibri::Cap]) -> (colibri::Vm, colibri::SharedBuffer) {
    let out = colibri::SharedBuffer::new();
    let mut builder = colibri::Vm::builder().stdout(Box::new(out.clone()));
    for cap in caps {
        builder = builder.allow(*cap);
    }
    let mut vm = builder.build();
    colibri_std::install_with(&mut vm, config).expect("installs");
    (vm, out)
}

fn with_args(args: &[&str]) -> StdConfig {
    StdConfig {
        args: args.iter().map(|a| a.to_string()).collect(),
        ..StdConfig::default()
    }
}

#[test]
fn args_reads_flags_values_positionals_and_the_terminator() {
    let (mut vm, out) = vm_with(
        with_args(&[
            "-vq",
            "--out",
            "result.txt",
            "--level=3",
            "input.txt",
            "-5",
            "--",
            "--not-a-flag",
            "-v",
        ]),
        &[],
    );
    run(
        &mut vm,
        "import args\nfn main() {\n    print(args.flag(\"v\"))\n    print(args.flag(\"q\"))\n    print(args.flag(\"out\"))  // present, whether or not it takes a value\n    print(args.flag(\"not-a-flag\"))\n    print(args.value(\"out\"))\n    print(args.value(\"level\"))\n    print(args.value(\"missing\"))\n    print(args.positional())\n    print(args.positional_with([\"out\"]))\n    print(args.list().len())\n}\n",
    )
    .unwrap();
    assert_eq!(
        out.contents(),
        "true\ntrue\ntrue\nfalse\nsome(\"result.txt\")\nsome(\"3\")\nnone\n\
         [\"result.txt\", \"input.txt\", \"-5\", \"--not-a-flag\", \"-v\"]\n\
         [\"input.txt\", \"-5\", \"--not-a-flag\", \"-v\"]\n9\n"
    );
}

#[test]
fn the_local_time_zone_comes_from_the_config() {
    let config = StdConfig {
        clock: Box::new(ManualClock::new(0.0)),
        local_offset: Some(-3600),
        ..StdConfig::default()
    };
    let (mut vm, out) = vm_with(config, &[colibri::Cap::Os]);
    run(
        &mut vm,
        "import datetime\nfn main() {\n    print(datetime.to_iso(datetime.now_local()))\n    print(datetime.to_iso(datetime.to_local(datetime.now())))\n}\n",
    )
    .unwrap();
    assert_eq!(
        out.contents(),
        "1969-12-31T23:00:00-01:00\n1969-12-31T23:00:00-01:00\n"
    );
}

// Unix only: `sh`, `$VAR` and `pwd -P` (the folder with symlinks resolved); the same options are checked on every system by crates/colibri-cli/tests/process_options.rs and tests/golden/std/process_windows.colibri.
#[cfg(unix)]
#[test]
fn a_child_gets_the_variables_the_script_set_and_its_own_folder() {
    let scratch = Scratch::new("child");
    let dir = scratch.0.to_string_lossy().replace('\\', "/");
    let (mut vm, out) = vm(&[colibri::Cap::Os]);
    run(
        &mut vm,
        &format!(
            "import process\nimport os\nfn main() {{\n    os.set_env(\"A\", \"from-set-env\")\n    var o = process.options()\n    o.cwd = some(\"{dir}\")\n    o.env = {{ \"B\": \"from-options\" }}\n    let r = process.run_with(\"sh\", [\"-c\", \"echo $A $B; pwd -P\"], o) or |e| {{ panic(e.message()) }}\n    print(r.stdout)\n}}\n"
        ),
    )
    .unwrap();
    let real = std::fs::canonicalize(&scratch.0).unwrap();
    assert_eq!(
        out.contents(),
        format!("from-set-env from-options\n{}\n\n", real.display())
    );
}

// Unix only: a signal that ends a program exists only on Unix (Windows has exit codes only).
#[cfg(unix)]
#[test]
fn a_child_killed_by_a_signal_has_a_negative_status() {
    let (mut vm, out) = vm(&[colibri::Cap::Os]);
    run(
        &mut vm,
        "import process\nfn main() {\n    let r = process.shell(\"kill -9 $$\") or |e| { panic(e.message()) }\n    print(r.status)\n}\n",
    )
    .unwrap();
    assert_eq!(out.contents(), "-9\n");
}

#[test]
fn an_unfinished_child_is_killed_when_it_is_dropped() {
    let scratch = Scratch::new("dropped");
    let marker = scratch.0.join("marker");
    // The child waits about a second and then creates the marker, unless it was killed.
    let (program, args) = sleep_then_touch("1", 2, &marker);
    let (mut vm, _) = vm(&[colibri::Cap::Os]);
    run(
        &mut vm,
        &format!(
            "import process\nfn start() {{\n    let c = process.spawn(\"{program}\", {args}) or |e| {{ panic(e.message()) }}\n    print(c.id() > 0)\n}}\nfn main() {{\n    start()\n}}\n"
        ),
    )
    .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1500));
    assert!(!marker.exists(), "the child outlived its handle");
}

// ---- process in coroutines ----

/// `process.run` waits on a background thread: a coroutine that is not waiting for the
/// program keeps running meanwhile (real clock; `sh` is on every platform the tests run on).
// Unix only: the child is an `sh` loop that polls for a marker file (a `cmd` loop needs `goto`/`timeout`, and `timeout` fails without a console); the Windows side of running a child beside coroutines is tests/golden/std/process_polling_windows.colibri.
#[cfg(unix)]
#[test]
fn coroutines_make_progress_while_a_child_process_runs() {
    // The child ends when the ticker has written a marker after its tenth tick, so the test
    // waits for a condition and not for a duration (timers on a loaded macOS runner sleep far
    // longer than asked). A blocked `Vm` never writes the marker: the child gives up after
    // twenty seconds and the count stays at one.
    let scratch = Scratch::new("progress");
    let marker = scratch
        .0
        .join("marker")
        .to_string_lossy()
        .replace('\\', "/");
    let (mut vm, out) = vm(&[colibri::Cap::Os, colibri::Cap::Fs]);
    run(
        &mut vm,
        &format!(
            r#"import process
import fs

var ticks = 0
var finished_at = 0

fn ticker() {{
    while true {{
        ticks += 1
        if ticks == 10 {{
            let _ = fs.write("{marker}", "x")
        }}
        wait(0.001)
    }}
}}

fn runner() -> int {{
    let script = "i=0; while [ ! -f \"$1\" ] && [ $i -lt 1000 ]; do sleep 0.02; i=$((i+1)); done; echo done"
    let r = process.run("sh", ["-c", script, "sh", "{marker}"]) or |e| {{ panic(e.message()) }}
    finished_at = ticks
    return r.status
}}

fn main() {{
    let t = go ticker()
    let r = go runner()
    print(r.join())
    t.cancel()
    print(finished_at >= 10)
}}
"#
        ),
    )
    .unwrap();
    assert_eq!(out.contents(), "0\ntrue\n");
}

/// Two children started by two coroutines run at the same time.
#[test]
fn child_processes_of_different_coroutines_overlap() {
    // Each child sleeps `nap` seconds (`ping -n 3` is the two seconds a Windows console-less
    // `cmd` can sleep for); run one after the other they would take twice that.
    let (line, nap) = if cfg!(windows) {
        ("ping -n 3 127.0.0.1 >nul & echo {n}", 2.0)
    } else {
        ("sleep 0.4; echo {n}", 0.4)
    };
    let (mut vm, out) = vm(&[colibri::Cap::Os]);
    let start = std::time::Instant::now();
    run(
        &mut vm,
        &format!(
            r#"import process

fn one(n: int) -> str {{
    let r = process.shell("{line}") or |e| {{ panic(e.message()) }}
    return r.stdout.trim()
}}

fn main() {{
    let a = go one(1)
    let b = go one(2)
    print(a.join() + b.join())
}}
"#
        ),
    )
    .unwrap();
    assert_eq!(out.contents(), "12\n");
    let took = start.elapsed();
    let limit = std::time::Duration::from_secs_f64(nap * 1.875);
    assert!(
        took < limit,
        "two {nap} s programs should overlap, took {took:?}"
    );
}

/// Cancelling the coroutine that waits for a program stops the program.
#[test]
fn cancelling_a_coroutine_stops_its_child_process() {
    let scratch = Scratch::new("cancel-child");
    let marker = scratch.0.join("marker");
    // The child waits and then creates the marker, unless it was stopped; the script looks
    // once the child would have been done.
    let (program, args) = sleep_then_touch("0.5", 2, &marker);
    let settle = if cfg!(windows) { 1.5 } else { 0.8 };
    let marker = colibri_text(&marker.to_string_lossy());
    let (mut vm, out) = vm(&[colibri::Cap::Os, colibri::Cap::Fs]);
    run(
        &mut vm,
        &format!(
            r#"import process
import fs

fn job() {{
    let _ = process.run("{program}", {args})
    print("never")
}}

fn main() {{
    let c = go job()
    wait(0.1)
    c.cancel()
    wait({settle})
    print(fs.exists("{marker}"))
}}
"#
        ),
    )
    .unwrap();
    assert_eq!(out.contents(), "false\n");
}

// ---- removed functions say what replaced them ----

#[test]
fn a_removed_function_names_its_replacement() {
    let cases = [
        ("fs", "size", "fs.stat(path)?.size"),
        ("fs", "modified", "fs.stat(path)?.modified"),
        ("fs", "remove_empty_dir", "fs.remove_dir"),
        ("str", "pad_left", "s.pad_left(width)"),
        ("str", "pad_right", "s.pad_right(width)"),
    ];
    for (module, item, hint) in cases {
        let (mut first, _) = vm(&[colibri::Cap::Fs]);
        // Qualified use, and a named import.
        let qualified = compile_error(
            &mut first,
            &format!("import {module}\nfn main() {{\n    let _ = {module}.{item}(\"x\")\n}}\n"),
        );
        assert!(
            qualified.contains(&format!("has no `{item}`")) && qualified.contains(hint),
            "{module}.{item}: {qualified}"
        );
        let (mut second, _) = vm(&[colibri::Cap::Fs]);
        let named = compile_error(
            &mut second,
            &format!("import {module}.{{{item}}}\nfn main() {{}}\n"),
        );
        assert!(named.contains(hint), "{module}.{{{item}}}: {named}");
    }
}

// Unix only: creating a symlink needs a privilege on Windows that a test cannot count on.
#[cfg(unix)]
#[test]
fn stat_follow_looks_through_a_symlink_and_stat_does_not() {
    let scratch = Scratch::new("stat-follow");
    let dir = scratch.0.to_string_lossy().replace('\\', "/");
    std::fs::write(scratch.0.join("target.txt"), "twelve bytes").unwrap();
    std::os::unix::fs::symlink(scratch.0.join("target.txt"), scratch.0.join("link")).unwrap();
    std::os::unix::fs::symlink(scratch.0.join("missing"), scratch.0.join("dangling")).unwrap();
    let (mut vm, out) = vm(&[colibri::Cap::Fs]);
    run(
        &mut vm,
        &format!(
            "import fs\nfn main() -> Result[()] {{\n    let direct = fs.stat(\"{dir}/link\")?\n    let through = fs.stat_follow(\"{dir}/link\")?\n    print(\"{{direct.kind}} {{through.kind}} {{through.size}}\")\n    print(fs.stat_follow(\"{dir}/dangling\").is_err())\n    print(fs.stat(\"{dir}/dangling\")?.kind)\n    return ok(())\n}}\n"
        ),
    )
    .unwrap();
    assert_eq!(out.contents(), "symlink file 12\ntrue\nsymlink\n");
}
