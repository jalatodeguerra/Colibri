//! A gzip reader (RFC 1952 around RFC 1951 "deflate"), for the one place the tool needs it: the
//! web runner is embedded in `colibri` compressed, because the raw 3.6 MB would carry the tool
//! over its 15 MB size gate, and `colibri build --web` writes it out again as `colibri.wasm`.
//! The release pipeline compresses it with `gzip -9n`.
//!
//! It is the plain bit-at-a-time Huffman decoder (the shape of zlib's `puff.c`): about a hundred
//! lines, no table to build, and fast enough for a few megabytes once per web build. Every length
//! and distance is checked, and the CRC-32 and the size in the trailer are compared, so damaged or
//! cut-off data is an error and never a wrong file.

/// The longest output a gzip file may claim (the runner is a few megabytes).
const MAX_OUTPUT: u64 = 256 * 1024 * 1024;

/// Whether `bytes` start like a gzip file.
pub fn is_gzip(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0x1f, 0x8b])
}

/// The bytes a gzip file holds, or what is wrong with it. Only one member is read; anything after
/// it is an error (a file made with `gzip` has exactly one).
pub fn gunzip(data: &[u8]) -> Result<Vec<u8>, String> {
    let bad = |what: &str| format!("not valid gzip data: {what}");
    if data.len() < 18 || !is_gzip(data) {
        return Err(bad("the header is missing"));
    }
    if data[2] != 8 {
        return Err(bad("the method is not deflate"));
    }
    let flags = data[3];
    if flags & 0xe0 != 0 {
        return Err(bad("reserved flags are set"));
    }
    let mut at = 10;
    if flags & 4 != 0 {
        let len = data
            .get(at..at + 2)
            .map(|b| usize::from(u16::from_le_bytes([b[0], b[1]])))
            .ok_or_else(|| bad("cut short in the header"))?;
        at += 2 + len;
    }
    for flag in [8, 16] {
        if flags & flag != 0 {
            let end = data
                .get(at..)
                .and_then(|rest| rest.iter().position(|&b| b == 0))
                .ok_or_else(|| bad("cut short in the header"))?;
            at += end + 1;
        }
    }
    if flags & 2 != 0 {
        at += 2;
    }
    if data.len() < at + 8 {
        return Err(bad("cut short"));
    }
    let trailer = &data[data.len() - 8..];
    let want_crc = u32::from_le_bytes([trailer[0], trailer[1], trailer[2], trailer[3]]);
    let want_len = u32::from_le_bytes([trailer[4], trailer[5], trailer[6], trailer[7]]);
    if u64::from(want_len) > MAX_OUTPUT {
        return Err(bad("it claims to hold more than 256 MiB"));
    }
    let mut bits = Bits {
        data: &data[at..data.len() - 8],
        pos: 0,
        buf: 0,
        count: 0,
    };
    let mut out = Vec::with_capacity(want_len as usize);
    inflate(&mut bits, &mut out, u64::from(want_len)).map_err(|e| bad(&e))?;
    if bits.unread_bytes() != 0 {
        return Err(bad("there is data after the compressed stream"));
    }
    if out.len() as u64 != u64::from(want_len) {
        return Err(bad("its size does not match the size in the trailer"));
    }
    if crc32(&out) != want_crc {
        return Err(bad("its check sum does not match"));
    }
    Ok(out)
}

/// The CRC-32 of the gzip trailer (the reflected polynomial `0xedb88320`).
fn crc32(bytes: &[u8]) -> u32 {
    let mut table = [0u32; 256];
    for (n, slot) in table.iter_mut().enumerate() {
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
    !bytes.iter().fold(!0u32, |crc, &b| {
        table[((crc ^ u32::from(b)) & 0xff) as usize] ^ (crc >> 8)
    })
}

/// The input, a bit at a time, least significant bit first.
struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
    buf: u32,
    count: u32,
}

impl Bits<'_> {
    fn bits(&mut self, need: u32) -> Result<u32, String> {
        while self.count < need {
            let byte = *self.data.get(self.pos).ok_or("the data is cut short")?;
            self.pos += 1;
            self.buf |= u32::from(byte) << self.count;
            self.count += 8;
        }
        let value = self.buf & ((1u32 << need) - 1);
        self.buf = self.buf.checked_shr(need).unwrap_or(0);
        self.count -= need;
        Ok(value)
    }

    /// Bytes not yet taken, once the bits left in the last byte are dropped.
    fn unread_bytes(&self) -> usize {
        self.data.len() - self.pos + (self.count / 8) as usize
    }

    /// Drops the bits left in the byte being read (a stored block starts on a byte).
    fn align(&mut self) {
        let drop = self.count % 8;
        self.buf >>= drop;
        self.count -= drop;
    }
}

const MAX_BITS: usize = 15;

/// A canonical Huffman code: how many codes there are of each length, and the symbols in the
/// order of their codes.
struct Huffman {
    count: [u16; MAX_BITS + 1],
    symbol: Vec<u16>,
}

impl Huffman {
    /// The code for symbols whose lengths are `lengths` (0 for a symbol not used). A code that is
    /// over-subscribed is an error; an incomplete one is allowed (a block may use a single code).
    fn new(lengths: &[u8]) -> Result<Huffman, String> {
        let mut count = [0u16; MAX_BITS + 1];
        for &l in lengths {
            count[usize::from(l)] += 1;
        }
        let mut left: i32 = 1;
        for &n in &count[1..] {
            left = (left << 1) - i32::from(n);
            if left < 0 {
                return Err("a Huffman code is over-subscribed".to_string());
            }
        }
        let mut offsets = [0u16; MAX_BITS + 2];
        for len in 1..=MAX_BITS {
            offsets[len + 1] = offsets[len] + count[len];
        }
        let mut symbol = vec![0u16; lengths.len()];
        for (sym, &l) in lengths.iter().enumerate() {
            if l != 0 {
                symbol[usize::from(offsets[usize::from(l)])] = sym as u16;
                offsets[usize::from(l)] += 1;
            }
        }
        Ok(Huffman { count, symbol })
    }

    fn decode(&self, bits: &mut Bits<'_>) -> Result<u16, String> {
        let (mut code, mut first, mut index) = (0i32, 0i32, 0i32);
        for len in 1..=MAX_BITS {
            code |= bits.bits(1)? as i32;
            let count = i32::from(self.count[len]);
            if code - count < first {
                return Ok(self.symbol[(index + (code - first)) as usize]);
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
        }
        Err("a Huffman code is not in the table".to_string())
    }
}

const LENGTH_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LENGTH_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];
/// The order in which the lengths of the code-length code are stored.
const CLEN_ORDER: [usize; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

/// Inflates blocks until the last one, appending to `out` (at most `limit` bytes).
fn inflate(bits: &mut Bits<'_>, out: &mut Vec<u8>, limit: u64) -> Result<(), String> {
    loop {
        let last = bits.bits(1)? == 1;
        match bits.bits(2)? {
            0 => stored(bits, out, limit)?,
            1 => {
                let mut lengths = [8u8; 288];
                lengths[144..256].fill(9);
                lengths[256..280].fill(7);
                let lit = Huffman::new(&lengths)?;
                let dist = Huffman::new(&[5u8; 30])?;
                codes(bits, out, limit, &lit, &dist)?;
            }
            2 => {
                let (lit, dist) = dynamic_tables(bits)?;
                codes(bits, out, limit, &lit, &dist)?;
            }
            _ => return Err("a block has a type that does not exist".to_string()),
        }
        if last {
            return Ok(());
        }
    }
}

fn stored(bits: &mut Bits<'_>, out: &mut Vec<u8>, limit: u64) -> Result<(), String> {
    bits.align();
    let len = bits.bits(16)? as usize;
    let not_len = bits.bits(16)? as usize;
    if len != !not_len & 0xffff {
        return Err("a stored block's length does not match its complement".to_string());
    }
    if out.len() as u64 + len as u64 > limit {
        return Err("it holds more than its trailer says".to_string());
    }
    for _ in 0..len {
        out.push(bits.bits(8)? as u8);
    }
    Ok(())
}

fn dynamic_tables(bits: &mut Bits<'_>) -> Result<(Huffman, Huffman), String> {
    let nlen = bits.bits(5)? as usize + 257;
    let ndist = bits.bits(5)? as usize + 1;
    let ncode = bits.bits(4)? as usize + 4;
    if nlen > 286 || ndist > 30 {
        return Err("a block has too many codes".to_string());
    }
    let mut lengths = [0u8; 320];
    for &slot in &CLEN_ORDER[..ncode] {
        lengths[slot] = bits.bits(3)? as u8;
    }
    let clen = Huffman::new(&lengths[..19])?;
    lengths = [0u8; 320];
    let mut index = 0;
    while index < nlen + ndist {
        let symbol = clen.decode(bits)?;
        if symbol < 16 {
            lengths[index] = symbol as u8;
            index += 1;
            continue;
        }
        let (value, repeat) = match symbol {
            16 => {
                if index == 0 {
                    return Err("a code length repeats nothing".to_string());
                }
                (lengths[index - 1], 3 + bits.bits(2)? as usize)
            }
            17 => (0, 3 + bits.bits(3)? as usize),
            _ => (0, 11 + bits.bits(7)? as usize),
        };
        if index + repeat > nlen + ndist {
            return Err("a code length repeats past the end of the table".to_string());
        }
        lengths[index..index + repeat].fill(value);
        index += repeat;
    }
    if lengths[256] == 0 {
        return Err("a block has no end-of-block code".to_string());
    }
    Ok((
        Huffman::new(&lengths[..nlen])?,
        Huffman::new(&lengths[nlen..nlen + ndist])?,
    ))
}

fn codes(
    bits: &mut Bits<'_>,
    out: &mut Vec<u8>,
    limit: u64,
    lit: &Huffman,
    dist: &Huffman,
) -> Result<(), String> {
    loop {
        let symbol = usize::from(lit.decode(bits)?);
        if symbol < 256 {
            if out.len() as u64 >= limit {
                return Err("it holds more than its trailer says".to_string());
            }
            out.push(symbol as u8);
            continue;
        }
        if symbol == 256 {
            return Ok(());
        }
        let symbol = symbol - 257;
        if symbol >= 29 {
            return Err("a length code does not exist".to_string());
        }
        let len =
            usize::from(LENGTH_BASE[symbol]) + bits.bits(u32::from(LENGTH_EXTRA[symbol]))? as usize;
        let d = usize::from(dist.decode(bits)?);
        if d >= 30 {
            return Err("a distance code does not exist".to_string());
        }
        let back = usize::from(DIST_BASE[d]) + bits.bits(u32::from(DIST_EXTRA[d]))? as usize;
        if back > out.len() {
            return Err("a match reaches back before the start of the data".to_string());
        }
        if out.len() as u64 + len as u64 > limit {
            return Err("it holds more than its trailer says".to_string());
        }
        // The copy may overlap what it writes (a distance shorter than the length repeats).
        for _ in 0..len {
            out.push(out[out.len() - back]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXED_TEXT: &str = "hello hello hello hello\n";

    include!("inflate_vectors.rs");

    #[test]
    fn a_stored_block_is_copied() {
        assert_eq!(gunzip(STORED).unwrap(), b"abcde");
    }

    #[test]
    fn a_fixed_huffman_block_with_a_match_is_decoded() {
        assert_eq!(gunzip(FIXED).unwrap(), FIXED_TEXT.as_bytes());
    }

    #[test]
    fn dynamic_huffman_blocks_are_decoded() {
        assert_eq!(gunzip(DYNAMIC).unwrap(), dynamic_text());
    }

    #[test]
    fn the_crc_32_is_the_standard_one() {
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn data_that_is_cut_short_or_changed_is_an_error_and_never_a_panic() {
        for whole in [FIXED, STORED, DYNAMIC] {
            for cut in 0..whole.len() {
                assert!(gunzip(&whole[..cut]).is_err(), "cut at {cut}");
            }
            // Any one byte changed after the header is caught by the codes or the check sum.
            for at in 10..whole.len() {
                let mut changed = whole.to_vec();
                changed[at] ^= 0x55;
                assert!(gunzip(&changed).is_err(), "byte {at} changed");
            }
        }
    }

    #[test]
    fn a_wrong_check_sum_or_size_or_trailing_data_is_refused() {
        let mut bad = FIXED.to_vec();
        let n = bad.len();
        bad[n - 8] ^= 1;
        assert!(gunzip(&bad).unwrap_err().contains("check sum"));
        let mut bad = FIXED.to_vec();
        bad[n - 4] ^= 1;
        assert!(gunzip(&bad).is_err());
        let mut more = FIXED[..n - 8].to_vec();
        more.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0]);
        // (A trailer that does not belong to the data is a size or check sum error as well.)
        assert!(gunzip(&more).is_err());
        assert!(!is_gzip(b"\0asm"));
        assert!(gunzip(b"\0asm\x01\0\0\0").is_err());
    }

    #[test]
    fn a_header_with_a_name_and_extra_fields_is_skipped() {
        // FNAME and FEXTRA set; the deflate data and trailer are those of `STORED`.
        let mut named = vec![0x1f, 0x8b, 0x08, 0x0c, 0, 0, 0, 0, 0, 3, 2, 0, 9, 9];
        named.extend_from_slice(b"name.txt\0");
        named.extend_from_slice(&STORED[10..]);
        assert_eq!(gunzip(&named).unwrap(), b"abcde");
    }
}
