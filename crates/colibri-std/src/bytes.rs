//! Helpers on bytes that more than one module needs. `Bytes` itself is a built-in type of the
//! language; a host function takes and returns it as [`colibri_runtime::Bytes`].

/// Lowercase hex digits for `bytes` (`hex.encode`, `hash.sha256_hex`).
pub(crate) fn hex_lower(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(DIGITS[usize::from(b >> 4)] as char);
        out.push(DIGITS[usize::from(b & 15)] as char);
    }
    out
}

/// Why `bytes` are not UTF-8 text, naming the byte where they stop being valid.
#[cfg_attr(target_family = "wasm", allow(dead_code))]
pub(crate) fn utf8_problem(e: &std::str::Utf8Error) -> String {
    let at = e.valid_up_to();
    match e.error_len() {
        Some(_) => format!("byte {at} is not part of a valid UTF-8 character"),
        None => {
            format!("the data ends in the middle of a UTF-8 character that starts at byte {at}")
        }
    }
}
