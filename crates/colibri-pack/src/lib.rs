//! The pack: a program (its source files and its assets) written after the code of a
//! `colibri` executable, so that the executable runs it when it starts (`colibri build`), or
//! on its own, which is what the web runner (`colibri-web`) loads.
//!
//! This crate is the format only (no files, no command line), so that it builds for the web too:
//! the tool's disk reading and writing, and the check of a signed Mac program, are in
//! `colibri-lang`.
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
//! The payload has a head, which is read and checked when the program starts, and then the
//! bytes of the assets, which are not: a program reads an asset from the file when it asks for
//! it (`import assets`), and a program that never does reads none.
//!
//! ```text
//! u64 head length H                 str   = u32 length, then UTF-8 bytes
//! the head, H bytes:                bytes = u64 length, then the bytes
//!   u32 number of source files
//!   str package name
//!   u8  start: 0 a script, 1 a package folder
//!   str start path
//!   str os, str arch, u8 debug, u8 no_default_features, u32 count, str feature ...   (BuildCfg)
//!   source files: u8 kind, str path, str name, bytes data
//!   u32 number of assets
//!   assets: str path, u64 length, u64 check sum of the asset's bytes
//! u64 FNV-1a check sum of the head length and the head
//! the assets' bytes, one after the other in the order of the list above
//! ```
//!
//! A source file (`kind` 0, `path` is where the loader looks for it) or a source file of a git
//! dependency (`kind` 2, `path` is below the cache folder, which is different on every machine).
//! `name` is what messages call the file. An asset is a file of the package that the program
//! reads with `import assets`: `path` is relative to the package folder, `/` between the pieces,
//! and no two assets share one.
//!
//! **Check sums.** The head has one over all of it, checked at start-up. Each asset has its own,
//! checked when the asset is read, so a flipped byte in an asset is an error of that read (the
//! program can handle it) and never a crash, and nothing reads the assets before `main`. The
//! assets must fill the rest of the payload exactly. Nothing here allocates what a length says
//! before it has been checked against the bytes that are left, so a damaged pack is an error and
//! never a crash.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use colibri_runtime::{BuildCfg, MemoryFs, asset_path_problem};

/// The last 32 bytes of a built executable start with this.
pub const MAGIC: &[u8; 12] = b"COLIBRI\0PACK";
/// The size of the trailer.
pub const TRAILER_LEN: usize = 32;
/// The version of the payload format. It is changed whenever the layout above changes.
pub const VERSION: u32 = 2;

/// Where the files of git dependencies are, in the file system of a running pack: any folder
/// does, as nothing else is in it.
const PACK_CACHE: &str = "<pack>/cache";

/// What kind of source file an entry is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A source file, text, at `path`.
    Source,
    /// A source file of a git dependency; `path` is below the cache folder.
    Dependency,
}

impl Kind {
    fn code(self) -> u8 {
        match self {
            Kind::Source => 0,
            Kind::Dependency => 2,
        }
    }

    fn from_code(code: u8) -> Option<Kind> {
        match code {
            0 => Some(Kind::Source),
            2 => Some(Kind::Dependency),
            _ => None,
        }
    }
}

/// One source file of the pack.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackFile {
    pub kind: Kind,
    /// Written with `/` between the pieces, whatever the system.
    pub path: String,
    /// How messages (diagnostics, stack traces) call the file.
    pub name: String,
    pub data: Vec<u8>,
}

/// Where the bytes of an asset are.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AssetData {
    /// In memory (a pack made by hand, in tests).
    Held(Vec<u8>),
    /// A file of the package, read when the pack is written (a build never holds the assets).
    /// Only the tool that writes packs from disk can write one (`write_with`'s callback).
    Disk { root: PathBuf },
    /// In the file the pack was read from, `offset` bytes after the start of the assets.
    Packed { offset: u64 },
}

/// One asset of the pack.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackAsset {
    /// Relative to the package folder, `/` between the pieces.
    pub path: String,
    pub len: u64,
    /// The check sum of its bytes.
    pub sum: u64,
    pub data: AssetData,
}

impl PackAsset {
    /// An asset whose bytes are in memory.
    pub fn held(path: impl Into<String>, data: Vec<u8>) -> PackAsset {
        PackAsset {
            path: path.into(),
            len: data.len() as u64,
            sum: checksum(&data),
            data: AssetData::Held(data),
        }
    }
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
    pub assets: Vec<PackAsset>,
    /// For a pack that was read: where the bytes of the assets start in the file it came from.
    pub assets_at: u64,
}

/// The 64-bit FNV-1a hash: a few lines, no dependency, the same on every system. It tells a
/// damaged pack from a good one; it is no defence against someone who means to change one.
pub fn checksum(bytes: &[u8]) -> u64 {
    let mut h = Fnv::new();
    h.update(bytes);
    h.0
}

/// [`checksum`], a piece at a time.
pub struct Fnv(u64);

impl Fnv {
    pub fn new() -> Fnv {
        Fnv(0xcbf2_9ce4_8422_2325)
    }

    pub fn update(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 ^= u64::from(b);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }

    pub fn finish(&self) -> u64 {
        self.0
    }
}

impl Default for Fnv {
    fn default() -> Self {
        Fnv::new()
    }
}

/// The length and the check sum of what a reader gives.
pub fn measure(r: &mut impl Read) -> std::io::Result<(u64, u64)> {
    let mut sum = Fnv::new();
    let mut len = 0u64;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = r.read(&mut buf)?;
        if n == 0 {
            return Ok((len, sum.finish()));
        }
        len += n as u64;
        sum.update(&buf[..n]);
    }
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u32).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

impl Pack {
    /// The head of the payload: everything but the assets' bytes.
    #[doc(hidden)]
    pub fn encode_head(&self) -> Vec<u8> {
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
        out.extend_from_slice(&(self.assets.len() as u32).to_le_bytes());
        for asset in &self.assets {
            put_str(&mut out, &asset.path);
            out.extend_from_slice(&asset.len.to_le_bytes());
            out.extend_from_slice(&asset.sum.to_le_bytes());
        }
        out
    }

    /// Writes the payload and the trailer that follows it, for a runner of `runner_len` bytes.
    /// The bytes of an asset that is [`AssetData::Disk`] are written by `disk` (it must write
    /// exactly the bytes the asset was measured with: the pack's length and check sum are what
    /// was measured); this crate reads no files.
    pub fn write_with(
        &self,
        out: &mut impl Write,
        runner_len: u64,
        disk: &mut dyn FnMut(&PackAsset, &mut dyn Write) -> Result<(), String>,
    ) -> Result<(), String> {
        let io = |e: std::io::Error| e.to_string();
        let head = self.encode_head();
        let head_len = (head.len() as u64).to_le_bytes();
        let mut sum = Fnv::new();
        sum.update(&head_len);
        sum.update(&head);
        let mut payload_len = 16 + head.len() as u64;
        out.write_all(&head_len).map_err(io)?;
        out.write_all(&head).map_err(io)?;
        out.write_all(&sum.finish().to_le_bytes()).map_err(io)?;
        for asset in &self.assets {
            payload_len += asset.len;
            match &asset.data {
                AssetData::Held(bytes) => out.write_all(bytes).map_err(io)?,
                AssetData::Disk { .. } => disk(asset, out)?,
                AssetData::Packed { .. } => {
                    return Err(format!(
                        "the asset `{}` was read from a built program and has no bytes to write",
                        asset.path
                    ));
                }
            }
        }
        out.write_all(MAGIC).map_err(io)?;
        out.write_all(&runner_len.to_le_bytes()).map_err(io)?;
        out.write_all(&payload_len.to_le_bytes()).map_err(io)?;
        out.write_all(&VERSION.to_le_bytes()).map_err(io)?;
        Ok(())
    }

    /// [`Pack::write_with`] for a pack whose assets are all in memory: one that has an asset on
    /// disk is an error.
    pub fn write_held(&self, out: &mut impl Write, runner_len: u64) -> Result<(), String> {
        self.write_with(out, runner_len, &mut |asset, _| {
            Err(format!(
                "the asset `{}` is on disk and this writer has no disk",
                asset.path
            ))
        })
    }

    /// [`Pack::write_held`] into memory: the payload and the trailer.
    pub fn append_held(&self, runner_len: u64) -> Result<Vec<u8>, String> {
        let mut out = Vec::new();
        self.write_held(&mut out, runner_len)?;
        Ok(out)
    }

    /// The pack in `bytes`, which end with a trailer: a pack file as `colibri build --web`
    /// writes it (the payload and the trailer, no runner in front), or a whole built program
    /// (the web runner is not one). `Err` says what is wrong with it. The assets' bytes stay in
    /// `bytes`: `assets_at` is where they start.
    pub fn from_bytes(bytes: &[u8]) -> Result<Pack, String> {
        let mut file = std::io::Cursor::new(bytes);
        let Some((offset, len)) = trailer_ending_at(&mut file, bytes.len() as u64)? else {
            return Err(
                "this is not a Colibri pack (the data does not end with its marker)".to_string(),
            );
        };
        // The trailer was checked against the size, so the payload is inside `bytes`.
        let payload = &bytes[offset as usize..(offset + len) as usize];
        let mut pack = Pack::decode(payload)?;
        pack.assets_at += offset;
        Ok(pack)
    }

    /// The pack a whole payload holds (the head and the assets' bytes), or what is wrong with
    /// it. The assets' bytes are not looked at: only that they are there.
    pub fn decode(payload: &[u8]) -> Result<Pack, String> {
        let damaged = |what: &str| format!("the program's data is damaged ({what})");
        let Some((len_bytes, rest)) = payload.split_first_chunk::<8>() else {
            return Err(damaged("it is cut short"));
        };
        let head_len = u64::from_le_bytes(*len_bytes);
        // The head, its check sum and the assets: what is left after the head is the assets.
        let Some(after) = (rest.len() as u64).checked_sub(8) else {
            return Err(damaged("it is cut short"));
        };
        if head_len > after {
            return Err(damaged("the head is longer than the data"));
        }
        let head_len = head_len as usize;
        Pack::decode_head(&payload[..head_len + 16], after - head_len as u64)
    }

    /// The pack that `head` (the length of the head, the head, its check sum) describes, whose
    /// assets are `region_len` bytes after it.
    pub fn decode_head(head: &[u8], region_len: u64) -> Result<Pack, String> {
        let damaged = |what: &str| format!("the program's data is damaged ({what})");
        let (covered, sum) = head.split_at(head.len() - 8);
        let want = u64::from_le_bytes(sum.try_into().expect("eight bytes"));
        if checksum(covered) != want {
            return Err(damaged("the check sum does not match"));
        }
        let body = &covered[8..];
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
            if std::str::from_utf8(data).is_err() {
                return Err(damaged("a source file is not text"));
            }
            files.push(PackFile {
                kind,
                path,
                name,
                data: data.to_vec(),
            });
        }
        let n_assets = r.u32()? as usize;
        // Every asset takes at least 20 bytes of the head.
        if n_assets > r.rest.len() / 20 {
            return Err(damaged("the number of assets is too large"));
        }
        let mut assets = Vec::with_capacity(n_assets);
        let mut seen = std::collections::HashSet::new();
        let mut offset = 0u64;
        for _ in 0..n_assets {
            let path = r.str()?;
            let len = r.u64()?;
            let sum = r.u64()?;
            // An asset is looked up by its path, so the path must be one a program can ask
            // for, and one asset must not hide another.
            if asset_path_problem(&path).is_some() {
                return Err(damaged("an asset has a path that is not valid"));
            }
            if !seen.insert(path.clone()) {
                return Err(damaged("an asset is there twice"));
            }
            assets.push(PackAsset {
                path,
                len,
                sum,
                data: AssetData::Packed { offset },
            });
            offset = offset
                .checked_add(len)
                .ok_or_else(|| damaged("an asset is too long"))?;
        }
        if !r.rest.is_empty() {
            return Err(damaged("there is more than the files"));
        }
        if offset != region_len {
            return Err(damaged("the assets do not fill the rest of the data"));
        }
        Ok(Pack {
            name,
            start,
            cfg,
            files,
            assets,
            assets_at: covered.len() as u64 + 8,
        })
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

/// The trailer of the `size` first bytes of a file, if it has one: where the payload is and how
/// long. `Ok(None)` is a file that does not end with the marker.
pub fn trailer_ending_at<R: Read + Seek>(
    file: &mut R,
    size: u64,
) -> Result<Option<(u64, u64)>, String> {
    let io = |e: std::io::Error| format!("cannot read the program's data: {e}");
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

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Pack {
        Pack {
            name: "demo".to_string(),
            start: Start::File("main.colibri".to_string()),
            cfg: BuildCfg {
                os: "web".to_string(),
                arch: "wasm32".to_string(),
                debug: true,
                features: vec!["fast".to_string()],
                no_default_features: false,
            },
            files: vec![PackFile {
                kind: Kind::Source,
                path: "main.colibri".to_string(),
                name: "main.colibri".to_string(),
                data: b"print(1)\n".to_vec(),
            }],
            assets: vec![PackAsset::held("a.txt", b"hello".to_vec())],
            assets_at: 0,
        }
    }

    #[test]
    fn a_pack_file_with_no_runner_in_front_is_read_from_its_bytes() {
        let bytes = sample().append_held(0).unwrap();
        let pack = Pack::from_bytes(&bytes).unwrap();
        assert_eq!(pack.name, "demo");
        assert_eq!(pack.files, sample().files);
        assert_eq!(pack.cfg, sample().cfg);
        // The assets stay in the bytes, where the pack says.
        let at = pack.assets_at as usize;
        assert_eq!(&bytes[at..at + 5], b"hello");
    }

    #[test]
    fn bytes_that_are_not_a_pack_or_are_cut_short_are_an_error_never_a_panic() {
        assert!(Pack::from_bytes(b"").is_err());
        assert!(Pack::from_bytes(b"not a pack at all, but long enough for a trailer....").is_err());
        let bytes = sample().append_held(0).unwrap();
        for cut in [1, 10, 31, 32, 40, bytes.len() - 1] {
            assert!(
                Pack::from_bytes(&bytes[..cut]).is_err(),
                "a pack cut to {cut} bytes"
            );
        }
        // A flipped byte of the head fails its check sum.
        let mut damaged = bytes;
        damaged[20] ^= 0xff;
        assert!(Pack::from_bytes(&damaged).is_err());
    }

    #[test]
    fn this_crate_writes_no_files_so_an_asset_on_disk_needs_its_writer() {
        let mut pack = sample();
        pack.assets = vec![PackAsset {
            path: "d.bin".to_string(),
            len: 3,
            sum: checksum(b"abc"),
            data: AssetData::Disk {
                root: PathBuf::from("."),
            },
        }];
        assert!(pack.append_held(0).is_err());
        let mut out = Vec::new();
        pack.write_with(&mut out, 0, &mut |_, w| {
            w.write_all(b"abc").map_err(|e| e.to_string())
        })
        .unwrap();
        assert_eq!(Pack::from_bytes(&out).unwrap().assets.len(), 1);
    }
}
