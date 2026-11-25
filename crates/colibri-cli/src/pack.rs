//! The pack: a program (its source files and, later, its assets) written after the code of a
//! `colibri` executable, so that the executable runs it when it starts (`colibri build`).
//!
//! A built file is the runner (a copy of `colibri`), then the payload, then a 32-byte trailer:
//!
//! ```text
//! offset  size  what
//!      0    12  the magic `COLIBRI\0PACK`
//!     12     8  where the payload starts in the file (u64, little endian)
//!     20     8  the length of the payload (u64)
//!     28     4  the format version (u32)
//! ```
//!
//! The payload is a small archive, everything little endian, with a check sum at the end:
//!
//! ```text
//! u32 number of entries            str = u32 length, then UTF-8 bytes
//! str package name                 bytes = u64 length, then the bytes
//! u8  start: 0 a script, 1 a package folder
//! str start path
//! str os, str arch, u8 debug, u8 no_default_features, u32 count, str feature ...   (BuildCfg)
//! entries: u8 kind, str path, str name, bytes data
//! u64 FNV-1a check sum of everything before it
//! ```
//!
//! An entry is a source file (`kind` 0, `path` is where the loader looks for it), a source file of a
//! git dependency (`kind` 2, `path` is below the cache folder, which is different on every machine),
//! or an asset (`kind` 1, not made yet). `name` is what messages call the file. Nothing here
//! allocates what a length says before it has been checked against the bytes that are left, so a
//! damaged pack is an error and never a crash.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path};

use colibri_runtime::{BuildCfg, MemoryFs};

/// The last 32 bytes of a built executable start with this.
pub const MAGIC: &[u8; 12] = b"COLIBRI\0PACK";
/// The size of the trailer.
pub const TRAILER_LEN: usize = 32;
/// The version of the payload format. It is changed whenever the layout above changes.
pub const VERSION: u32 = 1;

/// Where the files of git dependencies are, in the file system of a running pack: any folder
/// does, as nothing else is in it.
const PACK_CACHE: &str = "<pack>/cache";

/// What kind of file an entry is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A source file, text, at `path`.
    Source,
    /// A file the program reads at run time (not made yet).
    Asset,
    /// A source file of a git dependency; `path` is below the cache folder.
    Dependency,
}

impl Kind {
    fn code(self) -> u8 {
        match self {
            Kind::Source => 0,
            Kind::Asset => 1,
            Kind::Dependency => 2,
        }
    }

    fn from_code(code: u8) -> Option<Kind> {
        match code {
            0 => Some(Kind::Source),
            1 => Some(Kind::Asset),
            2 => Some(Kind::Dependency),
            _ => None,
        }
    }
}

/// One file of the pack.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackFile {
    pub kind: Kind,
    /// Written with `/` between the pieces, whatever the system.
    pub path: String,
    /// How messages (diagnostics, stack traces) call the file.
    pub name: String,
    pub data: Vec<u8>,
}

/// Where the program starts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Start {
    /// `colibri run script.colibri`.
    File(String),
    /// `colibri run folder`.
    Package(String),
}

/// A program, ready to be written after a runner or just read from one.
#[derive(Clone, Debug, PartialEq)]
pub struct Pack {
    /// The name of the package (or of the script), for messages.
    pub name: String,
    pub start: Start,
    /// What `@when` was decided with at build time.
    pub cfg: BuildCfg,
    pub files: Vec<PackFile>,
}

/// The 64-bit FNV-1a hash: a few lines, no dependency, the same on every system. It tells a
/// damaged pack from a good one; it is no defence against someone who means to change one.
fn checksum(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u32).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

impl Pack {
    /// The payload.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&(self.files.len() as u32).to_le_bytes());
        put_str(&mut out, &self.name);
        match &self.start {
            Start::File(p) => {
                out.push(0);
                put_str(&mut out, p);
            }
            Start::Package(p) => {
                out.push(1);
                put_str(&mut out, p);
            }
        }
        put_str(&mut out, &self.cfg.os);
        put_str(&mut out, &self.cfg.arch);
        out.push(u8::from(self.cfg.debug));
        out.push(u8::from(self.cfg.no_default_features));
        out.extend_from_slice(&(self.cfg.features.len() as u32).to_le_bytes());
        for f in &self.cfg.features {
            put_str(&mut out, f);
        }
        for file in &self.files {
            out.push(file.kind.code());
            put_str(&mut out, &file.path);
            put_str(&mut out, &file.name);
            out.extend_from_slice(&(file.data.len() as u64).to_le_bytes());
            out.extend_from_slice(&file.data);
        }
        let sum = checksum(&out);
        out.extend_from_slice(&sum.to_le_bytes());
        out
    }

    /// The pack a payload holds, or what is wrong with it.
    pub fn decode(payload: &[u8]) -> Result<Pack, String> {
        let damaged = |what: &str| format!("the program's data is damaged ({what})");
        let Some(split) = payload.len().checked_sub(8) else {
            return Err(damaged("it is cut short"));
        };
        let (body, sum) = payload.split_at(split);
        let want = u64::from_le_bytes(sum.try_into().expect("eight bytes"));
        if checksum(body) != want {
            return Err(damaged("the check sum does not match"));
        }
        let mut r = Reader { rest: body };
        let count = r.u32()? as usize;
        // Every entry takes at least 17 bytes: a count that cannot fit is a lie.
        if count > body.len() / 17 {
            return Err(damaged("the number of files is too large"));
        }
        let name = r.str()?;
        let start = match r.u8()? {
            0 => Start::File(r.str()?),
            1 => Start::Package(r.str()?),
            _ => return Err(damaged("an unknown way to start")),
        };
        let mut cfg = BuildCfg {
            os: r.str()?,
            arch: r.str()?,
            debug: r.flag()?,
            no_default_features: r.flag()?,
            features: Vec::new(),
        };
        let features = r.u32()? as usize;
        if features > r.rest.len() / 4 {
            return Err(damaged("the number of features is too large"));
        }
        for _ in 0..features {
            cfg.features.push(r.str()?);
        }
        let mut files = Vec::with_capacity(count);
        for _ in 0..count {
            let kind =
                Kind::from_code(r.u8()?).ok_or_else(|| damaged("an unknown kind of file"))?;
            let path = r.str()?;
            let name = r.str()?;
            let len = r.u64()?;
            let data = r.take(usize::try_from(len).map_err(|_| damaged("a file is too long"))?)?;
            if kind != Kind::Asset && std::str::from_utf8(data).is_err() {
                return Err(damaged("a source file is not text"));
            }
            files.push(PackFile {
                kind,
                path,
                name,
                data: data.to_vec(),
            });
        }
        if !r.rest.is_empty() {
            return Err(damaged("there is more than the files"));
        }
        Ok(Pack {
            name,
            start,
            cfg,
            files,
        })
    }

    /// The payload and the trailer that follows it, for a runner of `runner_len` bytes.
    pub fn append_to(&self, runner_len: u64) -> Vec<u8> {
        let mut out = self.encode();
        let len = out.len() as u64;
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&runner_len.to_le_bytes());
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&VERSION.to_le_bytes());
        out
    }

    /// The files as the loader reads them: the program in memory, its dependencies in a cache
    /// folder of its own, and the entry to start from.
    pub fn file_system(&self) -> (MemoryFs, Start) {
        let cache = Path::new(PACK_CACHE);
        let mut fs = MemoryFs::new().with_cache_root(cache);
        for file in &self.files {
            // The text was checked to be UTF-8 when the pack was read.
            let text = String::from_utf8_lossy(&file.data).into_owned();
            match file.kind {
                Kind::Source => fs.insert_named(Path::new(&file.path), &file.name, text),
                Kind::Dependency => {
                    fs.insert_named(cache.join(&file.path), &file.name, text);
                }
                Kind::Asset => {}
            }
        }
        (fs, self.start.clone())
    }
}

struct Reader<'a> {
    rest: &'a [u8],
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        if n > self.rest.len() {
            return Err("the program's data is damaged (it is cut short)".to_string());
        }
        let (head, tail) = self.rest.split_at(n);
        self.rest = tail;
        Ok(head)
    }

    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }

    fn flag(&mut self) -> Result<bool, String> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err("the program's data is damaged (a flag is neither 0 nor 1)".to_string()),
        }
    }

    fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_le_bytes(
            self.take(4)?.try_into().expect("four bytes"),
        ))
    }

    fn u64(&mut self) -> Result<u64, String> {
        Ok(u64::from_le_bytes(
            self.take(8)?.try_into().expect("eight bytes"),
        ))
    }

    fn str(&mut self) -> Result<String, String> {
        let len = self.u32()? as usize;
        let bytes = self.take(len)?;
        String::from_utf8(bytes.to_vec())
            .map_err(|_| "the program's data is damaged (a name is not text)".to_string())
    }
}

/// The trailer of a file, if it has one: where the payload is and how long. `Ok(None)` is an
/// ordinary executable (the check is one seek and one read of 32 bytes).
fn trailer<R: Read + Seek>(file: &mut R) -> Result<Option<(u64, u64)>, String> {
    let io = |e: std::io::Error| format!("cannot read the program's data: {e}");
    let size = file.seek(SeekFrom::End(0)).map_err(io)?;
    if size < TRAILER_LEN as u64 {
        return Ok(None);
    }
    file.seek(SeekFrom::Start(size - TRAILER_LEN as u64))
        .map_err(io)?;
    let mut t = [0u8; TRAILER_LEN];
    file.read_exact(&mut t).map_err(io)?;
    if &t[..12] != MAGIC {
        return Ok(None);
    }
    let offset = u64::from_le_bytes(t[12..20].try_into().expect("eight bytes"));
    let len = u64::from_le_bytes(t[20..28].try_into().expect("eight bytes"));
    let version = u32::from_le_bytes(t[28..32].try_into().expect("four bytes"));
    if version != VERSION {
        return Err(format!(
            "the program's data has format version {version}, and this runner reads version {VERSION}: build the program again"
        ));
    }
    // The payload ends where the trailer starts.
    if offset.checked_add(len) != Some(size - TRAILER_LEN as u64) {
        return Err(
            "the program's data is damaged (the trailer does not fit the file)".to_string(),
        );
    }
    Ok(Some((offset, len)))
}

/// Whether a file already holds a pack (a runner must not).
pub fn has_pack<R: Read + Seek>(file: &mut R) -> Result<bool, String> {
    trailer(file).map(|t| t.is_some())
}

/// The pack at the end of a file, `Ok(None)` if there is none, `Err` if there is one and it is
/// damaged.
pub fn read_from<R: Read + Seek>(file: &mut R) -> Result<Option<Pack>, String> {
    let Some((offset, len)) = trailer(file)? else {
        return Ok(None);
    };
    let io = |e: std::io::Error| format!("cannot read the program's data: {e}");
    file.seek(SeekFrom::Start(offset)).map_err(io)?;
    // `len` fits the file: the trailer was checked against its size.
    let mut payload = vec![0u8; usize::try_from(len).map_err(|_| "the program is too large")?];
    file.read_exact(&mut payload).map_err(io)?;
    Pack::decode(&payload).map(Some)
}

/// The pack at the end of the running executable, if it is a built program.
pub fn read_own() -> Result<Option<Pack>, String> {
    let Ok(exe) = std::env::current_exe() else {
        return Ok(None);
    };
    let Ok(mut file) = File::open(exe) else {
        return Ok(None);
    };
    read_from(&mut file)
}

/// `path` written with `/` between the pieces, whatever the system, for a pack: `None` if a
/// piece is not text.
pub fn portable(path: &Path) -> Option<String> {
    let mut out = String::new();
    let sep = |out: &mut String| {
        if !out.is_empty() && !out.ends_with('/') {
            out.push('/');
        }
    };
    for c in path.components() {
        match c {
            Component::Prefix(p) => out.push_str(p.as_os_str().to_str()?),
            Component::RootDir => out.push('/'),
            Component::CurDir => {}
            Component::ParentDir => {
                sep(&mut out);
                out.push_str("..");
            }
            Component::Normal(n) => {
                sep(&mut out);
                out.push_str(n.to_str()?);
            }
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn sample() -> Pack {
        Pack {
            name: "demo".to_string(),
            start: Start::Package("app".to_string()),
            cfg: BuildCfg {
                os: "linux".to_string(),
                arch: "x86_64".to_string(),
                debug: false,
                features: vec!["fast".to_string(), "x".to_string()],
                no_default_features: true,
            },
            files: vec![
                PackFile {
                    kind: Kind::Source,
                    path: "app/main.colibri".to_string(),
                    name: "app/main.colibri".to_string(),
                    data: b"print(1)\n".to_vec(),
                },
                PackFile {
                    kind: Kind::Dependency,
                    path: "git/checkouts/x/abc/lib.colibri".to_string(),
                    name: "/home/u/.cache/colibri/git/checkouts/x/abc/lib.colibri".to_string(),
                    data: "fn f() {}\n".as_bytes().to_vec(),
                },
                PackFile {
                    kind: Kind::Asset,
                    path: "data/blob.bin".to_string(),
                    name: "data/blob.bin".to_string(),
                    data: vec![0, 255, 7, 0xC0],
                },
            ],
        }
    }

    fn built(runner: &[u8]) -> Vec<u8> {
        let mut file = runner.to_vec();
        file.extend(sample().append_to(runner.len() as u64));
        file
    }

    #[test]
    fn a_pack_survives_being_written_and_read() {
        let file = built(b"runner bytes");
        let back = read_from(&mut Cursor::new(file)).unwrap().unwrap();
        assert_eq!(back, sample());
    }

    #[test]
    fn a_file_without_a_trailer_is_an_ordinary_executable() {
        assert_eq!(read_from(&mut Cursor::new(b"just code".to_vec())), Ok(None));
        assert_eq!(read_from(&mut Cursor::new(Vec::new())), Ok(None));
        // The magic anywhere but the end is not a trailer.
        let mut early = MAGIC.to_vec();
        early.extend_from_slice(&[0u8; 64]);
        assert_eq!(read_from(&mut Cursor::new(early)), Ok(None));
    }

    #[test]
    fn the_trailer_must_point_at_the_payload() {
        let good = built(b"runner");
        let n = good.len();
        // Each of offset, length and a version that is not ours, written into the trailer.
        for (at, what) in [(n - 20, "offset"), (n - 12, "length"), (n - 4, "version")] {
            for delta in [1u8, 0x80] {
                let mut bad = good.clone();
                bad[at] = bad[at].wrapping_add(delta);
                let err = read_from(&mut Cursor::new(bad)).unwrap_err();
                assert!(err.contains("program's data"), "{what}: {err}");
            }
        }
        let mut bad = good.clone();
        bad[n - 4..].copy_from_slice(&(VERSION + 1).to_le_bytes());
        assert!(
            read_from(&mut Cursor::new(bad))
                .unwrap_err()
                .contains("format version")
        );
    }

    #[test]
    fn a_damaged_payload_is_an_error_never_a_panic() {
        let good = built(b"runner");
        let start = b"runner".len();
        let end = good.len() - TRAILER_LEN;
        // Every single flipped byte of the payload is found by the check sum.
        for i in start..end {
            let mut bad = good.clone();
            bad[i] ^= 0x55;
            assert!(read_from(&mut Cursor::new(bad)).is_err(), "byte {i}");
        }
        // Every cut of the payload, with the trailer kept consistent, is an error too.
        let payload = &good[start..end];
        for cut in 0..payload.len() {
            assert!(Pack::decode(&payload[..cut]).is_err(), "cut {cut}");
        }
    }

    #[test]
    fn a_payload_whose_lengths_lie_is_refused_before_anything_is_allocated() {
        // A well-formed check sum around nonsense: the lengths are what must be refused.
        let seal = |mut body: Vec<u8>| {
            let sum = checksum(&body);
            body.extend_from_slice(&sum.to_le_bytes());
            body
        };
        // A count of four billion files in a few bytes.
        let err = Pack::decode(&seal(u32::MAX.to_le_bytes().to_vec())).unwrap_err();
        assert!(err.contains("too large"), "{err}");
        // A name that is longer than the payload.
        let mut body = 0u32.to_le_bytes().to_vec();
        body.extend_from_slice(&u32::MAX.to_le_bytes());
        let err = Pack::decode(&seal(body)).unwrap_err();
        assert!(err.contains("cut short"), "{err}");
        // A file of 2^63 bytes.
        let mut pack = sample();
        pack.files.truncate(1);
        let mut body = pack.encode();
        body.truncate(body.len() - 8);
        let at = body.len() - pack.files[0].data.len() - 8;
        body[at..at + 8].copy_from_slice(&(1u64 << 63).to_le_bytes());
        let err = Pack::decode(&seal(body)).unwrap_err();
        assert!(err.contains("damaged"), "{err}");
        // Source that is not text, and an unknown kind.
        let mut pack = sample();
        pack.files[0].data = vec![0xff, 0xfe];
        assert!(
            Pack::decode(&pack.encode())
                .unwrap_err()
                .contains("not text")
        );
        let mut body = sample().encode();
        body.truncate(body.len() - 8);
        let at = body.len()
            - sample()
                .files
                .iter()
                .map(|f| 1 + 4 + f.path.len() + 4 + f.name.len() + 8 + f.data.len())
                .sum::<usize>();
        body[at] = 9;
        assert!(
            Pack::decode(&seal(body))
                .unwrap_err()
                .contains("unknown kind")
        );
    }

    #[test]
    fn the_file_system_of_a_pack_keeps_names_and_puts_dependencies_in_its_cache() {
        use colibri_runtime::SourceFs;
        let (fs, start) = sample().file_system();
        assert_eq!(start, Start::Package("app".to_string()));
        assert_eq!(
            fs.read(Path::new("app/main.colibri")).unwrap(),
            "print(1)\n"
        );
        assert_eq!(
            fs.display_name(Path::new("./app/main.colibri")),
            "app/main.colibri"
        );
        let cache = fs.cache_root().unwrap();
        let dep = cache.join("git/checkouts/x/abc/lib.colibri");
        assert_eq!(fs.read(&dep).unwrap(), "fn f() {}\n");
        assert_eq!(
            fs.display_name(&dep),
            "/home/u/.cache/colibri/git/checkouts/x/abc/lib.colibri"
        );
        // An asset is not a source file.
        assert!(!fs.is_file(Path::new("data/blob.bin")));
    }

    #[test]
    fn paths_are_written_with_slashes() {
        assert_eq!(
            portable(Path::new("a/b/c.colibri")).unwrap(),
            "a/b/c.colibri"
        );
        assert_eq!(portable(Path::new("../x/./y")).unwrap(), "../x/y");
        assert_eq!(portable(Path::new("/r/s")).unwrap(), "/r/s");
        assert_eq!(portable(Path::new("")).unwrap(), "");
        let joined: std::path::PathBuf = ["a", "b"].iter().collect();
        assert_eq!(portable(&joined).unwrap(), "a/b");
    }
}
