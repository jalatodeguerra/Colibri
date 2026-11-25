//! The pack: a program (its source files and its assets) written after the code of a
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

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};

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

    /// The asset `path` of the package at `root`: read once to measure it, and again when the
    /// pack is written.
    pub fn from_disk(root: &Path, path: &str) -> Result<PackAsset, String> {
        let mut file = crate::assets::open_below(root, path, &mut |_| {}).map_err(|e| {
            format!(
                "cannot read the asset `{path}`: {}",
                crate::assets::error_text(&e)
            )
        })?;
        let (len, sum) = measure(&mut file).map_err(|e| {
            format!(
                "cannot read the asset `{path}`: {}",
                crate::assets::io_reason(&e)
            )
        })?;
        Ok(PackAsset {
            path: path.to_string(),
            len,
            sum,
            data: AssetData::Disk {
                root: root.to_path_buf(),
            },
        })
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
fn measure(r: &mut impl Read) -> std::io::Result<(u64, u64)> {
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
    fn encode_head(&self) -> Vec<u8> {
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
    /// An asset that is on disk is read again here, and must still be what it was when it was
    /// measured.
    pub fn write_to(&self, out: &mut impl Write, runner_len: u64) -> Result<(), String> {
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
                AssetData::Disk { root } => {
                    let changed = || {
                        format!(
                            "the asset `{}` changed while it was being packed",
                            asset.path
                        )
                    };
                    let mut file = crate::assets::open_below(root, &asset.path, &mut |_| {})
                        .map_err(|e| {
                            format!(
                                "cannot read the asset `{}`: {}",
                                asset.path,
                                crate::assets::error_text(&e)
                            )
                        })?;
                    let mut check = Fnv::new();
                    let mut written = 0u64;
                    let mut buf = [0u8; 64 * 1024];
                    loop {
                        let n = file.read(&mut buf).map_err(io)?;
                        if n == 0 {
                            break;
                        }
                        written += n as u64;
                        if written > asset.len {
                            return Err(changed());
                        }
                        check.update(&buf[..n]);
                        out.write_all(&buf[..n]).map_err(io)?;
                    }
                    if written != asset.len || check.finish() != asset.sum {
                        return Err(changed());
                    }
                }
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

    /// [`Pack::write_to`] into memory: the payload and the trailer.
    pub fn append_to(&self, runner_len: u64) -> Result<Vec<u8>, String> {
        let mut out = Vec::new();
        self.write_to(&mut out, runner_len)?;
        Ok(out)
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
    fn decode_head(head: &[u8], region_len: u64) -> Result<Pack, String> {
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

/// The trailer of a file, if it has one: where the payload is and how long. `Ok(None)` is an
/// ordinary executable (the check is one seek and one read of 32 bytes).
fn trailer<R: Read + Seek>(file: &mut R) -> Result<Option<(u64, u64)>, String> {
    let io = |e: std::io::Error| format!("cannot read the program's data: {e}");
    let size = file.seek(SeekFrom::End(0)).map_err(io)?;
    if let Some(found) = trailer_ending_at(file, size)? {
        return Ok(Some(found));
    }
    // A program signed on macOS has the signature after the data (see `macho`), and up to a few
    // bytes of padding between the trailer and the signature.
    let Some(end) = crate::macho::signed_end(file, size).map_err(io)? else {
        return Ok(None);
    };
    for pad in 0..=crate::macho::SLACK {
        if let Some(at) = end.checked_sub(pad)
            && let Some(found) = trailer_ending_at(file, at)?
        {
            return Ok(Some(found));
        }
    }
    Ok(None)
}

/// The trailer of the `size` first bytes of a file.
fn trailer_ending_at<R: Read + Seek>(
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

/// Where the payload of the pack in a file starts and how long it is (the trailer follows it),
/// found as the reader finds it. For tests.
#[doc(hidden)]
pub fn payload_range<R: Read + Seek>(file: &mut R) -> Result<Option<(u64, u64)>, String> {
    trailer(file)
}

/// Whether a file already holds a pack (a runner must not).
pub fn has_pack<R: Read + Seek>(file: &mut R) -> Result<bool, String> {
    trailer(file).map(|t| t.is_some())
}

/// The pack at the end of a file, `Ok(None)` if there is none, `Err` if there is one and it is
/// damaged. Only the head is read: the assets stay where they are, `assets_at` bytes into the
/// file.
pub fn read_from<R: Read + Seek>(file: &mut R) -> Result<Option<Pack>, String> {
    let Some((offset, len)) = trailer(file)? else {
        return Ok(None);
    };
    let io = |e: std::io::Error| format!("cannot read the program's data: {e}");
    let damaged = |what: &str| format!("the program's data is damaged ({what})");
    file.seek(SeekFrom::Start(offset)).map_err(io)?;
    let mut head_len = [0u8; 8];
    if len < 16 {
        return Err(damaged("it is cut short"));
    }
    file.read_exact(&mut head_len).map_err(io)?;
    let head_len = u64::from_le_bytes(head_len);
    // `len` fits the file: the trailer was checked against its size.
    if head_len > len - 16 {
        return Err(damaged("the head is longer than the data"));
    }
    let mut head =
        vec![0u8; usize::try_from(head_len + 16).map_err(|_| "the program is too large")?];
    head[..8].copy_from_slice(&head_len.to_le_bytes());
    file.read_exact(&mut head[8..]).map_err(io)?;
    let mut pack = Pack::decode_head(&head, len - 16 - head_len)?;
    pack.assets_at += offset;
    Ok(Some(pack))
}

/// The pack at the end of the running executable, with the open file the assets are read from,
/// if it is a built program.
///
/// An executable that cannot be found or opened (a path that is not readable because the file
/// is executable-only, a system without `/proc`, a file replaced after it started) is taken
/// for the ordinary tool, and the program's arguments are then read as `colibri`'s. That is
/// the choice that cannot make the tool itself unusable: a plain `colibri` has no marker to
/// tell it from a built program that cannot read itself, and it must start everywhere.
pub fn read_own() -> Result<Option<(Pack, File)>, String> {
    let Ok(exe) = std::env::current_exe() else {
        return Ok(None);
    };
    let Ok(mut file) = File::open(exe) else {
        return Ok(None);
    };
    Ok(read_from(&mut file)?.map(|pack| (pack, file)))
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
                    name: "shown\\main.colibri".to_string(),
                    data: b"print(1)\n".to_vec(),
                },
                PackFile {
                    kind: Kind::Dependency,
                    path: "git/checkouts/x/abc/lib.colibri".to_string(),
                    name: "/home/u/.cache/colibri/git/checkouts/x/abc/lib.colibri".to_string(),
                    data: "fn f() {}\n".as_bytes().to_vec(),
                },
            ],
            assets: vec![
                PackAsset::held("data/blob.bin", vec![0, 255, 7, 0xC0]),
                PackAsset::held("empty", Vec::new()),
                PackAsset::held("data/more.txt", b"more".to_vec()),
            ],
            assets_at: 0,
        }
    }

    fn built(runner: &[u8]) -> Vec<u8> {
        let mut file = runner.to_vec();
        file.extend(sample().append_to(runner.len() as u64).unwrap());
        file
    }

    /// What reading `sample()` back gives: the same, with the assets still in the file.
    fn expected_back(runner: usize) -> Pack {
        let mut pack = sample();
        let mut offset = 0;
        for asset in &mut pack.assets {
            asset.data = AssetData::Packed { offset };
            offset += asset.len;
        }
        pack.assets_at = (built(&vec![0; runner]).len() - TRAILER_LEN) as u64 - offset;
        pack
    }

    #[test]
    fn a_pack_survives_being_written_and_read() {
        let file = built(b"runner bytes");
        let back = read_from(&mut Cursor::new(file)).unwrap().unwrap();
        assert_eq!(back, expected_back(b"runner bytes".len()));
    }

    #[test]
    fn the_assets_are_after_the_head_and_where_the_pack_says() {
        let file = built(b"runner bytes");
        let back = read_from(&mut Cursor::new(file.clone())).unwrap().unwrap();
        let region = &file[back.assets_at as usize..file.len() - TRAILER_LEN];
        assert_eq!(region, [0, 255, 7, 0xC0, b'm', b'o', b'r', b'e']);
        let AssetData::Packed { offset } = back.assets[2].data else {
            panic!("an asset that was read is packed");
        };
        assert_eq!(&region[offset as usize..][..4], b"more");
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
        // Lengths that no file can have: refused before anything is allocated or read.
        for (offset, len) in [
            (0u64, 1u64 << 62),
            (u64::MAX, 1),
            (1, u64::MAX),
            (u64::MAX, u64::MAX),
        ] {
            let mut bad = good.clone();
            bad[n - 20..n - 12].copy_from_slice(&offset.to_le_bytes());
            bad[n - 12..n - 4].copy_from_slice(&len.to_le_bytes());
            let err = read_from(&mut Cursor::new(bad)).unwrap_err();
            assert!(err.contains("does not fit"), "{offset} {len}: {err}");
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
    fn a_damaged_head_is_an_error_never_a_panic_and_a_damaged_asset_is_not_looked_at() {
        let good = built(b"runner");
        let start = b"runner".len();
        let end = good.len() - TRAILER_LEN;
        let head_end = read_from(&mut Cursor::new(good.clone()))
            .unwrap()
            .unwrap()
            .assets_at as usize;
        // Every single flipped byte of the head is found by its check sum (or by the length
        // that comes first).
        for i in start..head_end {
            let mut bad = good.clone();
            bad[i] ^= 0x55;
            assert!(read_from(&mut Cursor::new(bad)).is_err(), "byte {i}");
        }
        // A flipped byte of an asset is not the pack's business: the asset's own check sum finds
        // it when the asset is read.
        for i in head_end..end {
            let mut bad = good.clone();
            bad[i] ^= 0x55;
            assert!(read_from(&mut Cursor::new(bad)).is_ok(), "byte {i}");
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
        let seal = |body: Vec<u8>| {
            let mut out = (body.len() as u64).to_le_bytes().to_vec();
            out.extend_from_slice(&body);
            let sum = checksum(&out);
            out.extend_from_slice(&sum.to_le_bytes());
            out
        };
        // A count of four billion files in a few bytes.
        let err = Pack::decode(&seal(u32::MAX.to_le_bytes().to_vec())).unwrap_err();
        assert!(err.contains("too large"), "{err}");
        // A name that is longer than the payload.
        let mut body = 0u32.to_le_bytes().to_vec();
        body.extend_from_slice(&u32::MAX.to_le_bytes());
        let err = Pack::decode(&seal(body)).unwrap_err();
        assert!(err.contains("cut short"), "{err}");
        // A head longer than the data.
        let mut long = seal(Vec::new());
        long[..8].copy_from_slice(&u64::MAX.to_le_bytes());
        let err = Pack::decode(&long).unwrap_err();
        assert!(err.contains("longer than the data"), "{err}");
        // A file of 2^63 bytes.
        let mut pack = sample();
        pack.files.truncate(1);
        pack.assets.clear();
        let mut body = pack.encode_head();
        body.truncate(body.len() - 4);
        let at = body.len() - pack.files[0].data.len() - 8;
        body[at..at + 8].copy_from_slice(&(1u64 << 63).to_le_bytes());
        let err = Pack::decode(&seal(body)).unwrap_err();
        assert!(err.contains("damaged"), "{err}");
        // Source that is not text, and an unknown kind (1 was the kind of an asset once).
        let mut pack = sample();
        pack.files[0].data = vec![0xff, 0xfe];
        let bytes = pack.append_to(0).unwrap();
        assert!(
            Pack::decode(&bytes[..bytes.len() - TRAILER_LEN])
                .unwrap_err()
                .contains("not text")
        );
        for kind in [1u8, 9] {
            let mut pack = sample();
            pack.assets.clear();
            let mut body = pack.encode_head();
            body.truncate(body.len() - 4);
            let at = body.len()
                - pack
                    .files
                    .iter()
                    .map(|f| 1 + 4 + f.path.len() + 4 + f.name.len() + 8 + f.data.len())
                    .sum::<usize>();
            body[at] = kind;
            assert!(
                Pack::decode(&seal(body))
                    .unwrap_err()
                    .contains("unknown kind")
            );
        }
    }

    #[test]
    fn the_assets_must_fill_the_rest_of_the_payload_exactly() {
        let good = built(b"runner");
        let start = b"runner".len();
        let end = good.len() - TRAILER_LEN;
        let payload = good[start..end].to_vec();
        assert!(Pack::decode(&payload).is_ok());
        // One byte too many, and one too few, after the head.
        let mut more = payload.clone();
        more.push(0);
        assert!(Pack::decode(&more).unwrap_err().contains("do not fill"));
        assert!(
            Pack::decode(&payload[..payload.len() - 1])
                .unwrap_err()
                .contains("do not fill")
        );
        // An asset that claims more than the file has, with every check sum right.
        let mut pack = sample();
        pack.assets[0].len += 1;
        let bytes = pack.append_to(0).unwrap();
        assert!(
            Pack::decode(&bytes[..bytes.len() - TRAILER_LEN])
                .unwrap_err()
                .contains("do not fill")
        );
        let mut pack = sample();
        pack.assets[0].len = u64::MAX;
        let mut head = pack.encode_head();
        let mut framed = (head.len() as u64).to_le_bytes().to_vec();
        framed.append(&mut head);
        let sum = checksum(&framed);
        framed.extend_from_slice(&sum.to_le_bytes());
        assert!(Pack::decode(&framed).is_err());
    }

    #[test]
    fn an_asset_path_a_program_could_not_ask_for_is_refused_and_so_is_a_repeat() {
        let with = |path: &str| {
            let mut pack = sample();
            pack.assets.push(PackAsset::held(path, vec![1]));
            let bytes = pack.append_to(0).unwrap();
            Pack::decode(&bytes[..bytes.len() - TRAILER_LEN])
        };
        for bad in [
            "../x",
            "/abs",
            "",
            "a//b",
            "a/./b",
            "a\\b",
            "C:/x",
            "a/",
            "x.",
            "con",
            "a/NUL.txt",
        ] {
            let err = with(bad).unwrap_err();
            assert!(err.contains("not valid"), "{bad:?}: {err}");
        }
        assert!(with("data/other.bin").is_ok());
        // The same path as an asset of the sample.
        let err = with("data/blob.bin").unwrap_err();
        assert!(err.contains("twice"), "{err}");
        // A source file may share a path with an asset: they are different things.
        let mut pack = sample();
        pack.files[0].path = "data/blob.bin".to_string();
        let bytes = pack.append_to(0).unwrap();
        assert!(Pack::decode(&bytes[..bytes.len() - TRAILER_LEN]).is_ok());
    }

    /// A reader that counts the bytes it gives out.
    struct Counting<R> {
        inner: R,
        read: std::rc::Rc<std::cell::Cell<u64>>,
    }

    impl<R: Read> Read for Counting<R> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.inner.read(buf)?;
            self.read.set(self.read.get() + n as u64);
            Ok(n)
        }
    }

    impl<R: Seek> Seek for Counting<R> {
        fn seek(&mut self, to: SeekFrom) -> std::io::Result<u64> {
            self.inner.seek(to)
        }
    }

    #[test]
    fn reading_a_pack_does_not_read_its_assets() {
        const BIG: usize = 6 * 1024 * 1024;
        let mut pack = sample();
        pack.assets.push(PackAsset::held("big.bin", vec![7u8; BIG]));
        let mut file = b"runner".to_vec();
        file.extend(pack.append_to(6).unwrap());
        assert!(file.len() > BIG);
        let read = std::rc::Rc::new(std::cell::Cell::new(0));
        let mut counting = Counting {
            inner: Cursor::new(file),
            read: read.clone(),
        };
        let back = read_from(&mut counting).unwrap().unwrap();
        assert_eq!(back.assets.len(), 4);
        // The trailer and the head: a few hundred bytes, not the megabytes behind them.
        assert!(read.get() < 4096, "{} bytes were read", read.get());
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
            "shown\\main.colibri"
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
