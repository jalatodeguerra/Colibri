//! Conversions between the compiler's view of a file (byte offsets, file names) and the
//! protocol's (UTF-16 positions, URIs).

use std::path::{Path, PathBuf};
use std::str::FromStr;

use lsp_types::{Position, Range, Uri};

/// Line starts of a text, to convert between byte offsets and LSP positions. A position's
/// character is a count of UTF-16 code units, whatever the file's encoding.
pub struct LineIndex<'a> {
    text: &'a str,
    starts: Vec<usize>,
}

impl<'a> LineIndex<'a> {
    pub fn new(text: &'a str) -> Self {
        let mut starts = vec![0];
        starts.extend(text.match_indices('\n').map(|(i, _)| i + 1));
        LineIndex { text, starts }
    }

    /// The position of a byte offset (clamped to the text, and to a character boundary).
    pub fn position(&self, offset: usize) -> Position {
        let mut offset = offset.min(self.text.len());
        while !self.text.is_char_boundary(offset) {
            offset -= 1;
        }
        let line = self.starts.partition_point(|&s| s <= offset) - 1;
        let units: usize = self.text[self.starts[line]..offset]
            .chars()
            .map(char::len_utf16)
            .sum();
        Position::new(line as u32, units as u32)
    }

    pub fn range(&self, start: usize, end: usize) -> Range {
        Range::new(self.position(start), self.position(end))
    }

    /// The range of the whole text.
    pub fn whole(&self) -> Range {
        self.range(0, self.text.len())
    }

    /// The byte offset of a position. A line past the end is the end of the text; a character
    /// past the end of its line is the end of the line, and one inside a surrogate pair is the end
    /// of that character.
    pub fn offset(&self, pos: Position) -> usize {
        let Some(&start) = self.starts.get(pos.line as usize) else {
            return self.text.len();
        };
        let line_end = self
            .starts
            .get(pos.line as usize + 1)
            .map_or(self.text.len(), |&e| e - 1);
        let line = &self.text[start..line_end.max(start)];
        let mut units = 0u32;
        for (i, c) in line.char_indices() {
            if units >= pos.character {
                return start + i;
            }
            units += c.len_utf16() as u32;
        }
        start + line.len()
    }
}

/// The path a `file:` URI names (any other scheme: the part after it, so an unsaved buffer
/// still has a key).
pub fn uri_to_path(uri: &Uri) -> PathBuf {
    let s = uri.as_str();
    let rest = match s.strip_prefix("file://") {
        Some(after_scheme) => after_scheme.find('/').map_or("", |i| &after_scheme[i..]),
        None => s.split_once(':').map_or(s, |(_, rest)| rest),
    };
    let decoded = percent_decode(rest);
    // `/C:/dir/file` is a Windows path.
    let bytes = decoded.as_bytes();
    if bytes.len() > 2 && bytes[0] == b'/' && bytes[1].is_ascii_alphabetic() && bytes[2] == b':' {
        return PathBuf::from(&decoded[1..]);
    }
    PathBuf::from(decoded)
}

/// The `file:` URI of a path.
pub fn path_to_uri(path: &Path) -> Option<Uri> {
    let text = path.to_string_lossy().replace('\\', "/");
    let mut out = String::from("file://");
    if !text.starts_with('/') {
        out.push('/');
    }
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    Uri::from_str(&out).ok()
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(hex) = s.get(i + 1..i + 3)
            && let Ok(b) = u8::from_str_radix(hex, 16)
        {
            out.push(b);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positions_count_utf16_units() {
        // `é` is 2 bytes and 1 unit; the emoji is 4 bytes and 2 units.
        let text = "ab\nlet é = \"😀\" + x\n";
        let index = LineIndex::new(text);
        let x = text.find("x\n").unwrap();
        assert_eq!(index.position(x), Position::new(1, 15));
        assert_eq!(index.offset(Position::new(1, 15)), x);
        assert_eq!(index.position(0), Position::new(0, 0));
        assert_eq!(index.position(text.len()), Position::new(2, 0));
        // Inside a surrogate pair: the end of the character.
        let emoji = text.find('😀').unwrap();
        assert_eq!(index.offset(Position::new(1, 10)), emoji + 4);
        // Past the end of a line and of the text.
        assert_eq!(index.offset(Position::new(0, 99)), 2);
        assert_eq!(index.offset(Position::new(9, 0)), text.len());
    }

    #[test]
    fn crlf_lines_do_not_count_the_carriage_return_as_text_end() {
        let index = LineIndex::new("a\r\nb");
        assert_eq!(index.position(3), Position::new(1, 0));
        assert_eq!(index.offset(Position::new(1, 1)), 4);
    }

    #[test]
    fn uris_and_paths_round_trip() {
        let path = Path::new("/home/me/my game/é.colibri");
        let uri = path_to_uri(path).unwrap();
        assert_eq!(uri.as_str(), "file:///home/me/my%20game/%C3%A9.colibri");
        assert_eq!(uri_to_path(&uri), path);
        let win = Uri::from_str("file:///C%3A/dir/a.colibri").unwrap();
        assert_eq!(uri_to_path(&win), Path::new("C:/dir/a.colibri"));
    }
}
