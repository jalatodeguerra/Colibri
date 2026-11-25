//! The pack of `colibri build`: a program (its source files and its assets) written after the code
//! of a `colibri` executable, so that the executable runs it when it starts. The format is the
//! crate `colibri-pack` (re-exported here, so that it builds for the web too); this is what only
//! the tool does: reading and writing a pack with files on disk, finding the pack at the end of a
//! file that may be a signed Mac program, and the pack of the running executable.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path};

pub use colibri_pack::*;

/// Writing a pack whose assets may be files on disk (`colibri build`).
pub trait PackDisk {
    /// Writes the payload and the trailer that follows it, for a runner of `runner_len` bytes.
    /// An asset that is on disk is read again here, and must still be what it was when it was
    /// measured.
    fn write_to(&self, out: &mut impl Write, runner_len: u64) -> Result<(), String>;

    /// [`PackDisk::write_to`] into memory: the payload and the trailer.
    fn append_to(&self, runner_len: u64) -> Result<Vec<u8>, String> {
        let mut out = Vec::new();
        self.write_to(&mut out, runner_len)?;
        Ok(out)
    }
}

impl PackDisk for Pack {
    fn write_to(&self, out: &mut impl Write, runner_len: u64) -> Result<(), String> {
        self.write_with(out, runner_len, &mut write_disk_asset)
    }
}

/// The asset `path` of the package at `root`: read once to measure it, and again when the
/// pack is written.
pub fn asset_from_disk(root: &Path, path: &str) -> Result<PackAsset, String> {
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

/// The bytes of an asset that is a file of the package, written into the pack.
fn write_disk_asset(asset: &PackAsset, out: &mut dyn Write) -> Result<(), String> {
    let io = |e: std::io::Error| e.to_string();
    let AssetData::Disk { root } = &asset.data else {
        return Err(format!("the asset `{}` is not on disk", asset.path));
    };
    let changed = || {
        format!(
            "the asset `{}` changed while it was being packed",
            asset.path
        )
    };
    let mut file = crate::assets::open_below(root, &asset.path, &mut |_| {}).map_err(|e| {
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
    Ok(())
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
    use colibri_runtime::BuildCfg;
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
