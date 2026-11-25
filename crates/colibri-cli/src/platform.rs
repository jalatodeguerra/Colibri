//! The platforms `colibri build` can write for (`--target`), and the checks on a runner that is
//! not the running `colibri` (`--runner`).
//!
//! A runner is looked at without running it, since it may be for another system:
//!
//! - **Format.** The first bytes tell ELF (Linux), PE (Windows) or Mach-O (macOS), and the
//!   machine field tells x86_64 or aarch64. A fat Mach-O is accepted if one of its slices is the
//!   target's architecture (it cannot be signed after the build, see [`crate::macho`]).
//! - **Version.** Every `colibri` holds [`MARKER`], a fixed byte string with its version, in its
//!   data (the build reads it back, so the linker keeps it). [`runner_version`] looks for it in
//!   the file, 1 MiB at a time. A file without it is not a `colibri` of this kind at all.
//!   Two builds of one version number are the same release: a mismatch is judged on the
//!   number alone.

use std::io::{Read, Seek, SeekFrom};

/// The operating systems of the targets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Os {
    Linux,
    Macos,
    Windows,
}

/// The processors of the targets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arch {
    X86_64,
    Aarch64,
}

/// A platform a program can be built for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Target {
    pub os: Os,
    pub arch: Arch,
}

/// Every target, by name.
pub const TARGETS: &[(&str, Target)] = &[
    ("linux-x86_64", Target::new(Os::Linux, Arch::X86_64)),
    ("linux-aarch64", Target::new(Os::Linux, Arch::Aarch64)),
    ("macos-x86_64", Target::new(Os::Macos, Arch::X86_64)),
    ("macos-aarch64", Target::new(Os::Macos, Arch::Aarch64)),
    ("windows-x86_64", Target::new(Os::Windows, Arch::X86_64)),
    ("windows-aarch64", Target::new(Os::Windows, Arch::Aarch64)),
];

impl Target {
    pub const fn new(os: Os, arch: Arch) -> Target {
        Target { os, arch }
    }

    /// The platform this `colibri` runs on, if it is one of the targets.
    pub fn host() -> Option<Target> {
        TARGETS.iter().map(|(_, t)| *t).find(|t| {
            t.os_name() == std::env::consts::OS && t.arch_name() == std::env::consts::ARCH
        })
    }

    /// What `@when(os == "..")` compares with.
    pub fn os_name(self) -> &'static str {
        match self.os {
            Os::Linux => "linux",
            Os::Macos => "macos",
            Os::Windows => "windows",
        }
    }

    /// What `@when(arch == "..")` compares with.
    pub fn arch_name(self) -> &'static str {
        match self.arch {
            Arch::X86_64 => "x86_64",
            Arch::Aarch64 => "aarch64",
        }
    }

    /// `linux-x86_64`.
    pub fn name(self) -> String {
        format!("{}-{}", self.os_name(), self.arch_name())
    }

    /// `.exe` for Windows, nothing elsewhere.
    pub fn exe_suffix(self) -> &'static str {
        if self.os == Os::Windows { ".exe" } else { "" }
    }
}

impl std::str::FromStr for Target {
    type Err = String;

    fn from_str(name: &str) -> Result<Target, String> {
        TARGETS
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, t)| *t)
            .ok_or_else(|| {
                let all: Vec<&str> = TARGETS.iter().map(|(n, _)| *n).collect();
                format!("unknown target `{name}` (one of: {})", all.join(", "))
            })
    }
}

fn describe(os: &str, arch: &str) -> String {
    format!("{os} {arch}")
}

/// Checks that the file is an executable of `target`'s format and processor.
pub fn check_format<R: Read + Seek>(file: &mut R, target: Target) -> Result<(), String> {
    let io = |e: std::io::Error| format!("cannot read the runner: {e}");
    let mut start = [0u8; 64];
    file.seek(SeekFrom::Start(0)).map_err(io)?;
    let n = read_up_to(file, &mut start).map_err(io)?;
    let start = &start[..n];
    let wrong = |found: String| {
        format!(
            "the runner is {found}, not an executable for {}",
            target.name()
        )
    };
    let want = |ok: bool, os: &str, arch: &str| {
        if ok {
            Ok(())
        } else {
            Err(wrong(describe(os, arch)))
        }
    };
    if start.starts_with(b"\x7fELF") && n >= 20 {
        let machine = u16::from_le_bytes([start[18], start[19]]);
        let arch = match machine {
            0x3e => "x86_64",
            0xb7 => "aarch64",
            _ => "another processor",
        };
        return want(
            target.os == Os::Linux && arch == target.arch_name(),
            "a Linux (ELF) program for",
            arch,
        );
    }
    if start.starts_with(b"MZ") && n >= 64 {
        let at = u32::from_le_bytes([start[60], start[61], start[62], start[63]]);
        let mut pe = [0u8; 6];
        file.seek(SeekFrom::Start(u64::from(at))).map_err(io)?;
        let got = read_up_to(file, &mut pe).map_err(io)?;
        if got == 6 && &pe[..4] == b"PE\0\0" {
            let arch = match u16::from_le_bytes([pe[4], pe[5]]) {
                0x8664 => "x86_64",
                0xaa64 => "aarch64",
                _ => "another processor",
            };
            return want(
                target.os == Os::Windows && arch == target.arch_name(),
                "a Windows (PE) program for",
                arch,
            );
        }
    }
    let cpu = |t: u32| match t {
        0x0100_0007 => "x86_64",
        0x0100_000c => "aarch64",
        _ => "another processor",
    };
    if start.starts_with(&[0xcf, 0xfa, 0xed, 0xfe]) && n >= 8 {
        let arch = cpu(u32::from_le_bytes([start[4], start[5], start[6], start[7]]));
        return want(
            target.os == Os::Macos && arch == target.arch_name(),
            "a macOS (Mach-O) program for",
            arch,
        );
    }
    if start.starts_with(&[0xca, 0xfe, 0xba, 0xbe]) && n >= 8 {
        // A fat file: a list of slices. (Java class files start the same way, with a small
        // number that is not a count of slices.)
        let count = u32::from_be_bytes([start[4], start[5], start[6], start[7]]) as usize;
        if (1..=16).contains(&count) {
            let mut table = vec![0u8; count * 20];
            file.seek(SeekFrom::Start(8)).map_err(io)?;
            let got = read_up_to(file, &mut table).map_err(io)?;
            let archs: Vec<&str> = table[..got - got % 20]
                .chunks(20)
                .map(|e| cpu(u32::from_be_bytes([e[0], e[1], e[2], e[3]])))
                .collect();
            return want(
                target.os == Os::Macos && archs.contains(&target.arch_name()),
                "a macOS (Mach-O) program for",
                &archs.join("+"),
            );
        }
    }
    Err(wrong(
        "not an executable of a known kind (ELF, PE or Mach-O)".to_string(),
    ))
}

fn read_up_to<R: Read>(file: &mut R, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut got = 0;
    while got < buf.len() {
        match file.read(&mut buf[got..])? {
            0 => break,
            n => got += n,
        }
    }
    Ok(got)
}

const PREFIX: &str = "\u{1}colibri-runner-version=";
const TEXT: &str = concat!(
    "\u{1}colibri-runner-version=",
    env!("CARGO_PKG_VERSION"),
    "\u{2}"
);

const fn bytes_of<const N: usize>(s: &str) -> [u8; N] {
    let b = s.as_bytes();
    let mut out = [0u8; N];
    let mut i = 0;
    while i < N {
        out[i] = b[i];
        i += 1;
    }
    out
}

/// The bytes every `colibri` carries: the prefix, its version, and a 2 byte. [`build`
/// ](crate::buildcmd) reads them (through [`own_version`]), which keeps them in the file.
#[used]
pub static MARKER: [u8; TEXT.len()] = bytes_of(TEXT);

/// The version of this `colibri`, as the marker in its own data says.
pub fn own_version() -> &'static str {
    let bytes: &'static [u8; TEXT.len()] = std::hint::black_box(&MARKER);
    std::str::from_utf8(&bytes[PREFIX.len()..bytes.len() - 1]).expect("the version is text")
}

/// The version of the `colibri` in a file, if the marker is there: the text between a prefix and
/// a 2 byte, made of the characters a version has. A prefix followed by anything else (the
/// code that looks for it holds the prefix too) is skipped.
pub fn runner_version<R: Read>(file: &mut R) -> std::io::Result<Option<String>> {
    const CHUNK: usize = 1 << 20;
    const KEEP: usize = 96;
    let prefix = PREFIX.as_bytes();
    let mut buf: Vec<u8> = Vec::with_capacity(CHUNK + KEEP);
    loop {
        let old = buf.len();
        buf.resize(old + CHUNK, 0);
        let got = read_up_to(file, &mut buf[old..])?;
        buf.truncate(old + got);
        let end = buf.len();
        let mut at = 0;
        // A prefix is only judged when its whole version is in the buffer, or the file ended.
        while let Some(found) = buf[at..].windows(prefix.len()).position(|w| w == prefix) {
            let start = at + found + prefix.len();
            let room = &buf[start..end.min(start + 64)];
            match room.iter().position(|b| *b == 2) {
                Some(len)
                    if len > 0
                        && room[..len]
                            .iter()
                            .all(|b| b.is_ascii_alphanumeric() || b"._-+".contains(b)) =>
                {
                    return Ok(Some(String::from_utf8_lossy(&room[..len]).into_owned()));
                }
                None if got == CHUNK && end - start < 64 => {
                    // The version may go on in the next chunk: keep from here.
                    at += found;
                    break;
                }
                _ => at = start,
            }
        }
        if got < CHUNK {
            return Ok(None);
        }
        let keep_from = at.max(end.saturating_sub(KEEP));
        buf.drain(..keep_from);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

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

    fn macho(cpu: u32) -> Vec<u8> {
        let mut b = vec![0xcf, 0xfa, 0xed, 0xfe];
        b.extend(cpu.to_le_bytes());
        b.resize(64, 0);
        b
    }

    fn fat(cpus: &[u32]) -> Vec<u8> {
        let mut b = vec![0xca, 0xfe, 0xba, 0xbe];
        b.extend((cpus.len() as u32).to_be_bytes());
        for c in cpus {
            b.extend(c.to_be_bytes());
            b.extend([0u8; 16]);
        }
        b
    }

    fn ok(bytes: &[u8], target: &str) -> Result<(), String> {
        check_format(&mut Cursor::new(bytes), target.parse().unwrap())
    }

    #[test]
    fn a_runner_must_match_the_format_and_the_processor() {
        assert!(ok(&elf(0x3e), "linux-x86_64").is_ok());
        assert!(ok(&elf(0xb7), "linux-aarch64").is_ok());
        assert!(ok(&pe(0x8664), "windows-x86_64").is_ok());
        assert!(ok(&pe(0xaa64), "windows-aarch64").is_ok());
        assert!(ok(&macho(0x0100_0007), "macos-x86_64").is_ok());
        assert!(ok(&macho(0x0100_000c), "macos-aarch64").is_ok());
        assert!(ok(&fat(&[0x0100_0007, 0x0100_000c]), "macos-aarch64").is_ok());
        assert!(ok(&fat(&[0x0100_0007, 0x0100_000c]), "macos-x86_64").is_ok());
        // Wrong processor, wrong system, not an executable.
        assert!(ok(&elf(0x3e), "linux-aarch64").is_err());
        assert!(ok(&pe(0xaa64), "windows-x86_64").is_err());
        assert!(ok(&macho(0x0100_000c), "macos-x86_64").is_err());
        assert!(ok(&fat(&[0x0100_0007]), "macos-aarch64").is_err());
        assert!(ok(&elf(0x3e), "macos-x86_64").is_err());
        assert!(ok(&elf(0x3e), "windows-x86_64").is_err());
        assert!(ok(&pe(0x8664), "linux-x86_64").is_err());
        assert!(ok(&macho(0x0100_0007), "linux-x86_64").is_err());
        assert!(ok(b"#!/bin/sh\necho hi\n", "linux-x86_64").is_err());
        assert!(ok(b"", "linux-x86_64").is_err());
        // A Java class file looks like a fat file with 52 slices.
        assert!(ok(&[0xca, 0xfe, 0xba, 0xbe, 0, 0, 0, 52], "macos-aarch64").is_err());
        // The message names both sides.
        let e = ok(&elf(0x3e), "windows-aarch64").unwrap_err();
        assert!(e.contains("Linux") && e.contains("windows-aarch64"), "{e}");
    }

    #[test]
    fn the_version_is_found_wherever_the_marker_is() {
        let marker = |v: &str| format!("{PREFIX}{v}\u{2}").into_bytes();
        for pad in [0usize, 5, 1 << 20, (1 << 20) - 3, (2 << 20) + 17] {
            let mut file = vec![7u8; pad];
            file.extend(marker("1.2.3-rc.1+x"));
            file.extend(vec![9u8; 100]);
            let got = runner_version(&mut Cursor::new(&file)).unwrap();
            assert_eq!(got.as_deref(), Some("1.2.3-rc.1+x"), "pad {pad}");
        }
        // Each position around the edge of a chunk.
        for pad in (1 << 20) - 40..(1 << 20) + 2 {
            let mut file = vec![7u8; pad];
            file.extend(marker("0.9.9"));
            let got = runner_version(&mut Cursor::new(&file)).unwrap();
            assert_eq!(got.as_deref(), Some("0.9.9"), "pad {pad}");
        }
    }

    #[test]
    fn a_file_without_a_real_marker_has_no_version() {
        assert_eq!(runner_version(&mut Cursor::new(b"nothing")).unwrap(), None);
        // The prefix alone, or followed by what is not a version, is not a marker.
        let mut file = PREFIX.as_bytes().to_vec();
        file.extend(b"not a version\x02 and more");
        file.extend(PREFIX.as_bytes());
        assert_eq!(runner_version(&mut Cursor::new(&file)).unwrap(), None);
        // A real one after a false one is found.
        file.extend(b"4.5.6\x02");
        assert_eq!(
            runner_version(&mut Cursor::new(&file)).unwrap().as_deref(),
            Some("4.5.6")
        );
    }

    #[test]
    fn this_program_carries_its_own_marker() {
        let exe = std::env::current_exe().unwrap();
        let mut file = std::fs::File::open(exe).unwrap();
        assert_eq!(
            runner_version(&mut file).unwrap().as_deref(),
            Some(env!("CARGO_PKG_VERSION"))
        );
        assert_eq!(own_version(), env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn targets_have_names_suffixes_and_a_helpful_error() {
        for (name, t) in TARGETS {
            assert_eq!(&t.name(), name);
            assert_eq!(name.parse::<Target>().unwrap(), *t);
        }
        assert_eq!(
            "windows-x86_64".parse::<Target>().unwrap().exe_suffix(),
            ".exe"
        );
        assert_eq!("linux-x86_64".parse::<Target>().unwrap().exe_suffix(), "");
        let e = "plan9-mips".parse::<Target>().unwrap_err();
        assert!(
            e.contains("plan9-mips") && e.contains("macos-aarch64"),
            "{e}"
        );
    }
}
