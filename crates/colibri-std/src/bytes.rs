//! Bytes as scripts have them today: a `List[int]` of values 0..=255 (there is no `Bytes` type
//! yet). `base64`, `hex` and `hash` read and write them through these two functions, so a
//! later `Bytes` type changes one place.

/// The bytes of a list of ints, or the position of the first one that is not in `0..=255`.
pub(crate) fn from_ints(items: &[i64]) -> Result<Vec<u8>, usize> {
    items
        .iter()
        .enumerate()
        .map(|(i, &n)| u8::try_from(n).map_err(|_| i))
        .collect()
}

/// A list of ints for bytes.
pub(crate) fn to_ints(bytes: &[u8]) -> Vec<i64> {
    bytes.iter().map(|&b| i64::from(b)).collect()
}

/// The message of a byte list that holds a value outside `0..=255`.
pub(crate) fn out_of_range(module: &str, items: &[i64], at: usize) -> String {
    format!(
        "{module}: a byte must be between 0 and 255, but item {at} of the list is {}",
        items[at]
    )
}
