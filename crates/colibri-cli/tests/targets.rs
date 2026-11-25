//! `colibri build --target T --runner PATH`: the checks on a runner, the effect of the target on
//! what is packed, and (on a Mac) a program that `codesign` accepts.
//!
//! No other platform's `colibri` is at hand, so a runner for another platform is a small file
//! that looks like one to the checks: the header of its format, and the version marker.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

struct Out {
    stdout: String,
    stderr: String,
    code: i32,
}

/// A fresh folder, removed at the end.
struct Dir(PathBuf);

impl Dir {
    fn new(name: &str) -> Dir {
        let dir =
            std::env::temp_dir().join(format!("colibri-target-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("home")).unwrap();
        Dir(dir)
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.0.join(rel)
    }

    fn write(&self, rel: &str, text: &str) {
        let full = self.0.join(rel);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, text).unwrap();
    }

    fn run(&self, mut cmd: Command) -> Out {
        let out = cmd
            .current_dir(&self.0)
            .env("COLIBRI_HOME", self.path("home"))
            .output()
            .expect("the program runs");
        Out {
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            code: out.status.code().unwrap_or(-1),
        }
    }

    fn colibri(&self, args: &[&str]) -> Out {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_colibri"));
        cmd.args(args);
        self.run(cmd)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn marker(version: &str) -> Vec<u8> {
    format!("\u{1}colibri-runner-version={version}\u{2}").into_bytes()
}

fn elf(machine: u16) -> Vec<u8> {
    let mut b = vec![0u8; 64];
    b[..4].copy_from_slice(b"\x7fELF");
    b[18..20].copy_from_slice(&machine.to_le_bytes());
    b
}

fn pe(machine: u16) -> Vec<u8> {
    let mut b = vec![0u8; 0x100];
    b[..2].copy_from_slice(b"MZ");
    b[60..64].copy_from_slice(&0x80u32.to_le_bytes());
    b[0x80..0x84].copy_from_slice(b"PE\0\0");
    b[0x84..0x86].copy_from_slice(&machine.to_le_bytes());
    b
}

/// A runner that looks like `colibri` of `version`, after `header`.
fn fake(header: Vec<u8>, version: Option<&str>) -> Vec<u8> {
    let mut b = header;
    b.extend(vec![0x90; 3000]);
    if let Some(v) = version {
        b.extend(marker(v));
    }
    b.extend(vec![0x90; 3000]);
    b
}

/// The name of this platform as `--target` says it.
fn host() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

/// A target that is not this platform.
fn elsewhere() -> &'static str {
    if std::env::consts::OS == "windows" {
        "linux-x86_64"
    } else {
        "windows-x86_64"
    }
}

const PLATFORM_PROGRAM: &str = "@when(os == \"windows\")\nimport winmod as plat\n@when(os != \"windows\")\nimport othermod as plat\n\nfn main() {\n    print(plat.name())\n}\n";

fn platform_program(dir: &Dir) {
    dir.write("prog.colibri", PLATFORM_PROGRAM);
    dir.write("winmod.colibri", "pub fn name() -> str => \"win\"\n");
    dir.write("othermod.colibri", "pub fn name() -> str => \"other\"\n");
}

fn packed_paths(file: PathBuf) -> (Vec<String>, colibri_lang::pack::Pack) {
    let mut f = fs::File::open(file).unwrap();
    let pack = colibri_lang::pack::read_from(&mut f)
        .unwrap()
        .expect("a built program");
    (pack.files.iter().map(|f| f.path.clone()).collect(), pack)
}

#[test]
fn another_platform_needs_a_runner_and_a_runner_needs_a_platform() {
    let dir = Dir::new("flags");
    dir.write("prog.colibri", "fn main() {\n    print(1)\n}\n");
    let out = dir.colibri(&["build", "prog.colibri", "-o", "x", "--target", elsewhere()]);
    assert_eq!(out.code, 1, "{}", out.stderr);
    assert!(out.stderr.contains("--runner"), "{}", out.stderr);
    assert!(out.stderr.contains(elsewhere()), "{}", out.stderr);
    assert!(!dir.path("x").exists());
    let runner = dir.path("r");
    fs::write(&runner, fake(elf(0x3e), Some(VERSION))).unwrap();
    let out = dir.colibri(&["build", "prog.colibri", "-o", "x", "--runner", "r"]);
    assert_eq!(out.code, 1, "{}", out.stderr);
    assert!(out.stderr.contains("needs `--target`"), "{}", out.stderr);
    assert!(out.stderr.contains("linux-aarch64"), "{}", out.stderr);
    assert!(!dir.path("x").exists());
    // An unknown name lists the known ones.
    let out = dir.colibri(&["build", "prog.colibri", "--target", "plan9-mips"]);
    assert_ne!(out.code, 0);
    assert!(out.stderr.contains("plan9-mips"), "{}", out.stderr);
    assert!(out.stderr.contains("macos-aarch64"), "{}", out.stderr);
}

#[test]
fn this_platform_is_a_target_that_needs_no_runner_and_this_colibri_is_a_runner() {
    let dir = Dir::new("host");
    dir.write("prog.colibri", "fn main() {\n    print(\"hi\")\n}\n");
    let suffix = std::env::consts::EXE_SUFFIX;
    let own = format!("own{suffix}");
    let out = dir.colibri(&["build", "prog.colibri", "-o", &own, "--target", &host()]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    let by_hand = format!("by_hand{suffix}");
    let me = env!("CARGO_BIN_EXE_colibri");
    let out = dir.colibri(&[
        "build",
        "prog.colibri",
        "-o",
        &by_hand,
        "--target",
        &host(),
        "--runner",
        me,
    ]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    for name in [&own, &by_hand] {
        let ran = dir.run(Command::new(dir.path(name)));
        assert_eq!(
            (ran.code, ran.stdout.as_str()),
            (0, "hi\n"),
            "{}",
            ran.stderr
        );
    }
    // A built program is not a runner.
    let out = dir.colibri(&[
        "build",
        "prog.colibri",
        "-o",
        "again",
        "--target",
        &host(),
        "--runner",
        &own,
    ]);
    assert_eq!(out.code, 1, "{}", out.stderr);
    assert!(
        out.stderr.contains("built program already"),
        "{}",
        out.stderr
    );
    assert!(!dir.path("again").exists());
}

#[test]
fn a_runner_of_the_wrong_kind_or_version_is_refused_and_nothing_is_written() {
    let dir = Dir::new("refuse");
    dir.write("prog.colibri", "fn main() {\n    print(1)\n}\n");
    let cases: Vec<(&str, &str, Vec<u8>, &str)> = vec![
        (
            "linux-x86_64",
            "wrong processor",
            fake(elf(0xb7), Some(VERSION)),
            "not an executable for linux-x86_64",
        ),
        (
            "linux-x86_64",
            "windows file",
            fake(pe(0x8664), Some(VERSION)),
            "Windows",
        ),
        (
            "windows-x86_64",
            "linux file",
            fake(elf(0x3e), Some(VERSION)),
            "Linux",
        ),
        (
            "windows-aarch64",
            "wrong pe processor",
            fake(pe(0x8664), Some(VERSION)),
            "not an executable for windows-aarch64",
        ),
        (
            "linux-x86_64",
            "a script",
            b"#!/bin/sh\necho hi\n".to_vec(),
            "not an executable",
        ),
        (
            "linux-x86_64",
            "other version",
            fake(elf(0x3e), Some("99.0.1")),
            "99.0.1",
        ),
        (
            "linux-x86_64",
            "no marker",
            fake(elf(0x3e), None),
            "does not say",
        ),
    ];
    for (target, what, bytes, message) in cases {
        fs::write(dir.path("runner"), bytes).unwrap();
        let out = dir.colibri(&[
            "build",
            "prog.colibri",
            "-o",
            "x",
            "--target",
            target,
            "--runner",
            "runner",
        ]);
        assert_eq!(out.code, 1, "{what}: {}", out.stderr);
        assert!(out.stderr.contains(message), "{what}: {}", out.stderr);
        assert!(!dir.path("x").exists(), "{what}");
        let left: Vec<String> = fs::read_dir(&dir.0)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            left.iter().all(|n| !n.ends_with(".colibri-build")),
            "{what}: {left:?}"
        );
    }
    // Missing, and a folder.
    let out = dir.colibri(&[
        "build",
        "prog.colibri",
        "-o",
        "x",
        "--target",
        "linux-x86_64",
        "--runner",
        "nothing",
    ]);
    assert_eq!(out.code, 1, "{}", out.stderr);
    fs::create_dir_all(dir.path("folder")).unwrap();
    let out = dir.colibri(&[
        "build",
        "prog.colibri",
        "-o",
        "x",
        "--target",
        "linux-x86_64",
        "--runner",
        "folder",
    ]);
    assert_eq!(out.code, 1, "{}", out.stderr);
    // The right kind and version is taken.
    fs::write(dir.path("runner"), fake(elf(0x3e), Some(VERSION))).unwrap();
    let out = dir.colibri(&[
        "build",
        "prog.colibri",
        "-o",
        "x",
        "--target",
        "linux-x86_64",
        "--runner",
        "runner",
    ]);
    assert_eq!(out.code, 0, "{}", out.stderr);
}

#[test]
fn the_target_decides_which_files_the_program_reads_and_the_name_of_the_output() {
    let dir = Dir::new("when");
    platform_program(&dir);
    fs::write(dir.path("win"), fake(pe(0x8664), Some(VERSION))).unwrap();
    fs::write(dir.path("lin"), fake(elf(0xb7), Some(VERSION))).unwrap();
    let out = dir.colibri(&[
        "build",
        "prog.colibri",
        "--target",
        "windows-x86_64",
        "--runner",
        "win",
    ]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    // No `-o`: the default name has the suffix of the target, not of this system.
    assert!(dir.path("prog.exe").is_file(), "{}", out.stderr);
    let (paths, pack) = packed_paths(dir.path("prog.exe"));
    assert!(
        paths.iter().any(|p| p.ends_with("winmod.colibri")),
        "{paths:?}"
    );
    assert!(
        !paths.iter().any(|p| p.ends_with("othermod.colibri")),
        "{paths:?}"
    );
    assert_eq!(
        (pack.cfg.os.as_str(), pack.cfg.arch.as_str()),
        ("windows", "x86_64")
    );
    // Another system, the other file, and no suffix.
    let out = dir.colibri(&[
        "build",
        "prog.colibri",
        "--target",
        "linux-aarch64",
        "--runner",
        "lin",
    ]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    let (paths, pack) = packed_paths(dir.path("prog"));
    assert!(
        paths.iter().any(|p| p.ends_with("othermod.colibri")),
        "{paths:?}"
    );
    assert!(
        !paths.iter().any(|p| p.ends_with("winmod.colibri")),
        "{paths:?}"
    );
    assert_eq!(
        (pack.cfg.os.as_str(), pack.cfg.arch.as_str()),
        ("linux", "aarch64")
    );
}

#[test]
fn a_universal_macos_runner_is_taken_with_a_warning_that_it_stays_unsigned() {
    let dir = Dir::new("fat");
    dir.write("prog.colibri", "fn main() {\n    print(1)\n}\n");
    let mut fat = vec![0xca, 0xfe, 0xba, 0xbe, 0, 0, 0, 2];
    for cpu in [0x0100_0007u32, 0x0100_000c] {
        fat.extend(cpu.to_be_bytes());
        fat.extend([0u8; 16]);
    }
    fs::write(dir.path("fat"), fake(fat, Some(VERSION))).unwrap();
    let out = dir.colibri(&[
        "build",
        "prog.colibri",
        "-o",
        "p",
        "--target",
        "macos-aarch64",
        "--runner",
        "fat",
    ]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(out.stderr.contains("lipo -thin"), "{}", out.stderr);
    assert!(dir.path("p").is_file());
}

/// A thin 64-bit Mach-O of `len` bytes that looks like a linker's output, with a signature.
#[cfg(not(target_os = "macos"))]
fn mach_o(len: usize, version: &str) -> Vec<u8> {
    fn segment(name: &str, fileoff: u64, filesize: u64) -> Vec<u8> {
        let mut c = Vec::new();
        c.extend(0x19u32.to_le_bytes());
        c.extend(72u32.to_le_bytes());
        let mut n = name.as_bytes().to_vec();
        n.resize(16, 0);
        c.extend(n);
        c.extend(0u64.to_le_bytes());
        c.extend(0u64.to_le_bytes());
        c.extend(fileoff.to_le_bytes());
        c.extend(filesize.to_le_bytes());
        c.extend([0u8; 16]);
        c
    }
    let sig = 64usize;
    let mut cmds = segment("__TEXT", 0, 0x4000);
    cmds.extend(segment("__LINKEDIT", 0x4000, (len - 0x4000) as u64));
    cmds.extend(0x1du32.to_le_bytes());
    cmds.extend(16u32.to_le_bytes());
    cmds.extend(((len - sig) as u32).to_le_bytes());
    cmds.extend((sig as u32).to_le_bytes());
    let mut file = vec![0xcf, 0xfa, 0xed, 0xfe];
    file.extend(0x0100_000cu32.to_le_bytes());
    file.extend([0u8; 4]);
    file.extend(2u32.to_le_bytes());
    file.extend(3u32.to_le_bytes());
    file.extend((cmds.len() as u32).to_le_bytes());
    file.extend([0u8; 8]);
    file.extend(cmds);
    file.resize(len - sig, 0x90);
    let at = 0x4100;
    file[at..at + marker(version).len()].copy_from_slice(&marker(version));
    file.resize(len, 0xEE);
    file
}

#[cfg(not(target_os = "macos"))]
#[test]
fn a_program_for_macos_built_elsewhere_can_be_signed_there_and_says_so() {
    let dir = Dir::new("mac");
    dir.write("prog.colibri", "fn main() {\n    print(1)\n}\n");
    fs::write(dir.path("mac"), mach_o(0x6000, VERSION)).unwrap();
    let out = dir.colibri(&[
        "build",
        "prog.colibri",
        "-o",
        "prog",
        "--target",
        "macos-aarch64",
        "--runner",
        "mac",
    ]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(out.stderr.contains("codesign -s -"), "{}", out.stderr);
    let built = fs::read(dir.path("prog")).unwrap();
    // The signature of the runner is gone (its command too), and __LINKEDIT reaches the end.
    assert_eq!(u32::from_le_bytes(built[16..20].try_into().unwrap()), 2);
    let linkedit = 32 + 72;
    let off = u64::from_le_bytes(built[linkedit + 40..linkedit + 48].try_into().unwrap());
    let size = u64::from_le_bytes(built[linkedit + 48..linkedit + 56].try_into().unwrap());
    assert_eq!(off + size, built.len() as u64);
    assert!(built.len() > 0x6000 - 64);
    // And the program is still found at the end of the file.
    let (paths, _) = packed_paths(dir.path("prog"));
    assert_eq!(paths.len(), 1, "{paths:?}");
    // For Windows or Linux there is no such warning.
    fs::write(dir.path("lin"), fake(elf(0x3e), Some(VERSION))).unwrap();
    let out = dir.colibri(&[
        "build",
        "prog.colibri",
        "-o",
        "p2",
        "--target",
        "linux-x86_64",
        "--runner",
        "lin",
    ]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(!out.stderr.contains("codesign"), "{}", out.stderr);
}

/// On a Mac, the build signs the program itself: `codesign --verify` accepts it, it runs (arm64
/// refuses unsigned code), and a signature of the user's own is possible afterwards.
#[cfg(target_os = "macos")]
#[test]
fn a_program_built_on_a_mac_is_signed_verifies_and_runs() {
    let dir = Dir::new("signed");
    dir.write("prog.colibri", "fn main() {\n    print(\"signed\")\n}\n");
    let out = dir.colibri(&["build", "prog.colibri", "-o", "prog"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(!out.stderr.contains("warning"), "{}", out.stderr);
    let verify = |name: &str| {
        let out = Command::new("codesign")
            .args(["--verify", "--strict", "--verbose=2"])
            .arg(dir.path(name))
            .output()
            .expect("codesign is on every Mac");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    verify("prog");
    let ran = dir.run(Command::new(dir.path("prog")));
    assert_eq!(
        (ran.code, ran.stdout.as_str()),
        (0, "signed\n"),
        "{}",
        ran.stderr
    );
    // Signing again, as a user with an identity would, keeps the program working.
    let again = Command::new("codesign")
        .args(["--force", "-s", "-"])
        .arg(dir.path("prog"))
        .output()
        .unwrap();
    assert!(
        again.status.success(),
        "{}",
        String::from_utf8_lossy(&again.stderr)
    );
    verify("prog");
    let ran = dir.run(Command::new(dir.path("prog")));
    assert_eq!(
        (ran.code, ran.stdout.as_str()),
        (0, "signed\n"),
        "{}",
        ran.stderr
    );
    // The same through `--target` and `--runner`.
    let me = env!("CARGO_BIN_EXE_colibri");
    let out = dir.colibri(&[
        "build",
        "prog.colibri",
        "-o",
        "p2",
        "--target",
        &host(),
        "--runner",
        me,
    ]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    verify("p2");
}
