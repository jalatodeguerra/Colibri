//! Just enough of the Mach-O format for `colibri build` on macOS.
//!
//! A program is the runner with the pack appended (see [`crate::pack`]). On macOS that has two
//! problems. `codesign` refuses a file whose `__LINKEDIT` segment does not reach its end, and
//! it puts the signature at the end of that segment, so the pack must lie inside `__LINKEDIT`
//! and the signature must come after it.
//!
//! [`prepare`] therefore works on the runner's load commands (a thin 64-bit file, the only kind
//! `colibri` is released as):
//!
//! 1. the runner's own `LC_CODE_SIGNATURE` goes: its blob is cut off the end of the file, its
//!    load command is removed (the later commands move up, `ncmds` and `sizeofcmds` shrink);
//! 2. the pack is appended;
//! 3. `__LINKEDIT` is made to end where the file ends ([`Prepared::head_with`]).
//!
//! The result is what `codesign -s -` accepts, on a Mac, now or later (a cross build leaves it
//! unsigned and says so). The file is unsigned until then.
//!
//! After signing the pack is no longer at the end of the file, the signature is. So the reader
//! ([`signed_end`], used by `pack::trailer`) takes a thin Mach-O with an `LC_CODE_SIGNATURE`
//! whose blob reaches the end of the file, and looks for the trailer just before the blob
//! (`codesign` starts the blob at a multiple of 16, so up to [`SLACK`] zero bytes lie between).
//!
//! The segment's `vmsize` is rounded up to 16 KiB, the page size of arm64 (also a multiple of
//! the 4 KiB of x86_64).

use std::io::{Read, Seek, SeekFrom};

const MAGIC_64: [u8; 4] = [0xcf, 0xfa, 0xed, 0xfe];
const HEADER: usize = 32;
const LC_SEGMENT_64: u32 = 0x19;
const LC_CODE_SIGNATURE: u32 = 0x1d;
const PAGE: u64 = 0x4000;
/// More than any real set of load commands (a few KiB).
const MAX_COMMANDS: usize = 1 << 20;
/// How far before the signature the trailer may start (the signature's alignment is 16).
pub const SLACK: u64 = 32;

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().expect("four bytes"))
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().expect("eight bytes"))
}

/// Whether the file starts like a thin 64-bit Mach-O.
pub fn is_thin(start: &[u8]) -> bool {
    start.starts_with(&MAGIC_64)
}

/// How many bytes the header and the load commands take, from the first 32 bytes of a file.
pub fn head_len(start: &[u8]) -> Option<usize> {
    if start.len() < HEADER || !is_thin(start) {
        return None;
    }
    let size = u32_at(start, 20) as usize;
    (size <= MAX_COMMANDS).then_some(HEADER + size)
}

/// Where the commands are, found by one walk over them.
struct Walk {
    /// `(offset, size)` of the signature command, if any.
    sig: Option<(usize, usize)>,
    /// Offset of the `__LINKEDIT` segment command.
    linkedit: Option<usize>,
}

fn walk(head: &[u8]) -> Result<Walk, String> {
    let bad = |what: &str| format!("the runner is not a Mach-O file that can be signed ({what})");
    if head.len() < HEADER || !is_thin(head) {
        return Err(bad("not a thin 64-bit file"));
    }
    let count = u32_at(head, 16);
    let size = u32_at(head, 20) as usize;
    if head.len() < HEADER + size {
        return Err(bad("the load commands are cut short"));
    }
    let mut at = HEADER;
    let mut found = Walk {
        sig: None,
        linkedit: None,
    };
    for _ in 0..count {
        if at + 8 > HEADER + size {
            return Err(bad("a load command is cut short"));
        }
        let (cmd, cmdsize) = (u32_at(head, at), u32_at(head, at + 4) as usize);
        if cmdsize < 8 || at + cmdsize > HEADER + size {
            return Err(bad("a load command has a wrong size"));
        }
        if cmd == LC_SEGMENT_64 && cmdsize >= 72 && head[at + 8..].starts_with(b"__LINKEDIT\0") {
            found.linkedit = Some(at);
        } else if cmd == LC_CODE_SIGNATURE && cmdsize >= 16 {
            found.sig = Some((at, cmdsize));
        }
        at += cmdsize;
    }
    Ok(found)
}

/// A runner made ready to carry a pack: write `keep` bytes of it, then the pack, then the head
/// from [`Prepared::head_with`] over the first bytes of the file.
pub struct Prepared {
    /// How much of the runner stays: all of it, or all but its signature.
    pub keep: u64,
    head: Vec<u8>,
    linkedit: usize,
}

/// Takes the header and load commands of a runner (`head`, [`head_len`] bytes) of `file_len`
/// bytes, and removes its signature.
pub fn prepare(head: &[u8], file_len: u64) -> Result<Prepared, String> {
    let bad = |what: &str| format!("the runner is not a Mach-O file that can be signed ({what})");
    let found = walk(head)?;
    let mut linkedit = found.linkedit.ok_or_else(|| bad("it has no __LINKEDIT"))?;
    let fileoff = u64_at(head, linkedit + 40);
    let end = fileoff.saturating_add(u64_at(head, linkedit + 48));
    if end != file_len {
        return Err(bad("__LINKEDIT does not end the file"));
    }
    let mut head = head.to_vec();
    let mut keep = file_len;
    if let Some((at, size)) = found.sig {
        let (off, len) = (
            u64::from(u32_at(&head, at + 8)),
            u64::from(u32_at(&head, at + 12)),
        );
        if off < fileoff || off.saturating_add(len) != file_len {
            return Err(bad("the signature is not at the end of the file"));
        }
        keep = off;
        let used = head.len();
        head.copy_within(at + size..used, at);
        head[used - size..].fill(0);
        let ncmds = u32_at(&head, 16) - 1;
        let sizeofcmds = u32_at(&head, 20) - size as u32;
        head[16..20].copy_from_slice(&ncmds.to_le_bytes());
        head[20..24].copy_from_slice(&sizeofcmds.to_le_bytes());
        if linkedit > at {
            linkedit -= size;
        }
    }
    Ok(Prepared {
        keep,
        head,
        linkedit,
    })
}

impl Prepared {
    /// The head to write at the start of the file, for a file of `total` bytes: `__LINKEDIT`
    /// reaches its end.
    pub fn head_with(&self, total: u64) -> Vec<u8> {
        let mut head = self.head.clone();
        let at = self.linkedit;
        let size = total.saturating_sub(u64_at(&head, at + 40));
        head[at + 48..at + 56].copy_from_slice(&size.to_le_bytes());
        let vm = size.div_ceil(PAGE) * PAGE;
        head[at + 32..at + 40].copy_from_slice(&vm.to_le_bytes());
        head
    }
}

/// Where the data of a signed program ends, when the file is a thin Mach-O with a signature
/// that reaches the end of the file: the start of the signature. `None` for any other file.
pub fn signed_end<R: Read + Seek>(file: &mut R, size: u64) -> std::io::Result<Option<u64>> {
    let mut start = [0u8; HEADER];
    if size < HEADER as u64 {
        return Ok(None);
    }
    file.seek(SeekFrom::Start(0))?;
    file.read_exact(&mut start)?;
    let Some(len) = head_len(&start) else {
        return Ok(None);
    };
    if len as u64 > size {
        return Ok(None);
    }
    let mut head = start.to_vec();
    head.resize(len, 0);
    file.read_exact(&mut head[HEADER..])?;
    let Ok(found) = walk(&head) else {
        return Ok(None);
    };
    let Some((at, _)) = found.sig else {
        return Ok(None);
    };
    let (off, len) = (
        u64::from(u32_at(&head, at + 8)),
        u64::from(u32_at(&head, at + 12)),
    );
    Ok((off.saturating_add(len) == size && off > 0).then_some(off))
}

#[cfg(test)]
pub mod tests {
    use super::*;

    fn segment(name: &str, fileoff: u64, filesize: u64) -> Vec<u8> {
        let mut c = Vec::new();
        c.extend(LC_SEGMENT_64.to_le_bytes());
        c.extend(72u32.to_le_bytes());
        let mut n = name.as_bytes().to_vec();
        n.resize(16, 0);
        c.extend(n);
        c.extend((0x1_0000_0000u64 + fileoff).to_le_bytes());
        c.extend(0u64.to_le_bytes());
        c.extend(fileoff.to_le_bytes());
        c.extend(filesize.to_le_bytes());
        c.extend([0x11u8; 16]);
        c
    }

    fn signature(off: u32, len: u32) -> Vec<u8> {
        let mut c = Vec::new();
        c.extend(LC_CODE_SIGNATURE.to_le_bytes());
        c.extend(16u32.to_le_bytes());
        c.extend(off.to_le_bytes());
        c.extend(len.to_le_bytes());
        c
    }

    /// A Mach-O image of `len` bytes: `__TEXT` (0..0x4000), `__LINKEDIT` (0x4000..), and a
    /// signature in the last `sig` bytes, with its command before (`sig_first`) or after
    /// `__LINKEDIT`.
    pub fn image(len: u64, sig: u32, sig_first: bool) -> Vec<u8> {
        let mut cmds = Vec::<u8>::new();
        let mut count = 1u32;
        let text = segment("__TEXT", 0, 0x4000);
        let link = segment("__LINKEDIT", 0x4000, len - 0x4000);
        let sig_cmd = signature((len - u64::from(sig)) as u32, sig);
        cmds.extend(&text);
        if sig > 0 && sig_first {
            cmds.extend(&sig_cmd);
            count += 1;
        }
        cmds.extend(&link);
        count += 1;
        if sig > 0 && !sig_first {
            cmds.extend(&sig_cmd);
            count += 1;
        }
        let mut file = Vec::new();
        file.extend(MAGIC_64);
        file.extend(0x0100_000cu32.to_le_bytes());
        file.extend([0u8; 4]);
        file.extend(2u32.to_le_bytes());
        file.extend(count.to_le_bytes());
        file.extend((cmds.len() as u32).to_le_bytes());
        file.extend([0u8; 8]);
        file.extend(cmds);
        file.resize(len as usize, 0xAB);
        file
    }

    fn head_of(file: &[u8]) -> Vec<u8> {
        file[..head_len(file).unwrap()].to_vec()
    }

    pub fn linkedit_of(head: &[u8]) -> (u64, u64, u64) {
        let at = walk(head).unwrap().linkedit.unwrap();
        (
            u64_at(head, at + 40),
            u64_at(head, at + 48),
            u64_at(head, at + 32),
        )
    }

    #[test]
    fn a_signature_before_linkedit_is_removed_and_the_later_commands_move_up() {
        let file = image(0x8000, 0x100, true);
        let p = prepare(&head_of(&file), file.len() as u64).unwrap();
        assert_eq!(p.keep, 0x8000 - 0x100);
        let head = p.head_with(0x9_0123);
        let found = walk(&head).unwrap();
        assert!(found.sig.is_none());
        assert_eq!(u32_at(&head, 16), 2);
        assert_eq!(u32_at(&head, 20), 144);
        // The freed bytes at the end of the commands are zero, and the head keeps its size.
        assert_eq!(head.len(), HEADER + 144 + 16);
        assert!(head[HEADER + 144..].iter().all(|b| *b == 0));
        // __LINKEDIT now reaches the new end, and its vmsize is a whole number of pages.
        assert_eq!(linkedit_of(&head), (0x4000, 0x9_0123 - 0x4000, 0x9_0000));
    }

    #[test]
    fn a_signature_after_linkedit_is_removed() {
        let file = image(0x8000, 0x100, false);
        let p = prepare(&head_of(&file), file.len() as u64).unwrap();
        assert_eq!(p.keep, 0x7f00);
        let head = p.head_with(0x7f00 + 1000);
        assert!(walk(&head).unwrap().sig.is_none());
        assert_eq!(linkedit_of(&head), (0x4000, 0x3f00 + 1000, 0x8000));
    }

    #[test]
    fn an_unsigned_runner_is_kept_whole() {
        let file = image(0x6000, 0, false);
        let p = prepare(&head_of(&file), file.len() as u64).unwrap();
        assert_eq!(p.keep, 0x6000);
        assert_eq!(u32_at(&p.head_with(0x6000 + 7), 16), 2);
    }

    #[test]
    fn what_cannot_be_signed_afterwards_is_refused() {
        let file = image(0x8000, 0x100, false);
        let head = head_of(&file);
        // __LINKEDIT does not end the file.
        assert!(prepare(&head, 0x8001).is_err());
        // The signature does not end the file.
        let mut moved = head.clone();
        let at = walk(&moved).unwrap().sig.unwrap().0;
        moved[at + 8..at + 12].copy_from_slice(&0x7000u32.to_le_bytes());
        assert!(prepare(&moved, 0x8000).is_err());
        // An unsigned runner whose __LINKEDIT does not end the file.
        let unsigned = image(0x6000, 0, false);
        assert!(prepare(&head_of(&unsigned), 0x6001).is_err());
        // Not a Mach-O.
        assert!(prepare(&[0u8; 64], 64).is_err());
        // A command that is too big.
        let mut big = head.clone();
        big[HEADER + 4..HEADER + 8].copy_from_slice(&0x10_0000u32.to_le_bytes());
        assert!(prepare(&big, 0x8000).is_err());
    }

    #[test]
    fn the_end_of_the_data_is_where_the_signature_starts() {
        let file = image(0x8000, 0x100, false);
        let end = signed_end(&mut std::io::Cursor::new(&file), file.len() as u64).unwrap();
        assert_eq!(end, Some(0x7f00));
        // A signature that does not reach the end of the file is not one this reader trusts.
        let end = signed_end(&mut std::io::Cursor::new(&file), 0x8001).unwrap();
        assert_eq!(end, None);
        let plain = vec![7u8; 100];
        assert_eq!(
            signed_end(&mut std::io::Cursor::new(&plain), 100).unwrap(),
            None
        );
        let unsigned = image(0x6000, 0, false);
        assert_eq!(
            signed_end(&mut std::io::Cursor::new(&unsigned), 0x6000).unwrap(),
            None
        );
    }
}
