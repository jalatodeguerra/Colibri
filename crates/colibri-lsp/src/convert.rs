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
    PathBuf::from(uri_to_path_text(uri.as_str(), cfg!(windows)))
}

/// The `file:` URI of a path.
pub fn path_to_uri(path: &Path) -> Option<Uri> {
    Uri::from_str(&path_text_to_uri(&path.to_string_lossy(), cfg!(windows))).ok()
}

/// [`uri_to_path`] on text, for the system's kind of paths: pure, so that the Windows forms
/// are tested on every system. For Windows (`windows`):
///
/// - `file:///C:/dir/a.colibri` and what VS Code sends, `file:///c%3A/dir/a.colibri` (a
///   lower-case drive, the colon percent-encoded), are `C:/dir/a.colibri` (the drive's case is
///   kept: the URIs the server publishes must read like the client's);
/// - `file://server/share/a.colibri` is the UNC path `//server/share/a.colibri`.
///
/// A Unix path keeps its leading `/`, and a name such as `/c:/x` is an ordinary one there.
fn uri_to_path_text(uri: &str, windows: bool) -> String {
    let (authority, rest) = match uri.strip_prefix("file://") {
        Some(after_scheme) => match after_scheme.find('/') {
            Some(i) => (&after_scheme[..i], &after_scheme[i..]),
            None => (after_scheme, ""),
        },
        None => ("", uri.split_once(':').map_or(uri, |(_, rest)| rest)),
    };
    let decoded = percent_decode(rest);
    if !windows {
        return decoded;
    }
    if !authority.is_empty() && !authority.eq_ignore_ascii_case("localhost") {
        return format!("//{}{decoded}", percent_decode(authority));
    }
    // `/C:/dir/file` is a Windows path.
    let bytes = decoded.as_bytes();
    let drive = bytes.len() > 2 && bytes[0] == b'/' && bytes[1].is_ascii_alphabetic();
    if drive && bytes[2] == b':' && (bytes.len() == 3 || bytes[3] == b'/') {
        return decoded[1..].to_string();
    }
    decoded
}

/// [`path_to_uri`] on text. For Windows (`windows`), backslashes are separators, the verbatim
/// prefix that canonicalising adds (`\\?\C:\x`, `\\?\UNC\server\share`) is dropped, a drive
/// path gets its leading `/` (`file:///C%3A/x`) and a UNC path its host as the authority
/// (`file://server/share/x`).
fn path_text_to_uri(path: &str, windows: bool) -> String {
    let mut text = path.to_string();
    let mut authority = String::new();
    if windows {
        text = text.replace('\\', "/");
        if let Some(unc) = text.strip_prefix("//?/UNC/") {
            text = format!("//{unc}");
        } else if let Some(local) = text.strip_prefix("//?/") {
            text = local.to_string();
        }
        if let Some(unc) = text.strip_prefix("//") {
            let (host, rest) = unc.split_once('/').unwrap_or((unc, ""));
            authority = host.to_string();
            text = format!("/{rest}");
        }
    }
    let mut out = String::from("file://");
    percent_encode_into(&authority, &mut out, false);
    if !text.starts_with('/') {
        out.push('/');
    }
    percent_encode_into(&text, &mut out, true);
    out
}

fn percent_encode_into(text: &str, out: &mut String, slashes: bool) {
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) || (slashes && b == b'/') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
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
        let path = Path::new("/home/me/my app/é.colibri");
        let uri = path_to_uri(path).unwrap();
        assert_eq!(uri.as_str(), "file:///home/me/my%20app/%C3%A9.colibri");
        assert_eq!(uri_to_path(&uri), path);
    }

    #[test]
    fn windows_uris_become_windows_paths() {
        let to_path = |uri: &str| uri_to_path_text(uri, true);
        assert_eq!(to_path("file:///C:/dir/a.colibri"), "C:/dir/a.colibri");
        // What VS Code sends: a lower-case drive with the colon percent-encoded.
        assert_eq!(to_path("file:///c%3A/dir/a.colibri"), "c:/dir/a.colibri");
        assert_eq!(
            to_path("file:///C%3a/My%20Dir/%C3%A9.colibri"),
            "C:/My Dir/é.colibri"
        );
        assert_eq!(to_path("file://localhost/C:/dir"), "C:/dir");
        assert_eq!(to_path("file:///C:"), "C:");
        // A UNC path keeps its host.
        assert_eq!(
            to_path("file://server/share/dir/a.colibri"),
            "//server/share/dir/a.colibri"
        );
        // Not a drive: a folder called `cc:` is not one.
        assert_eq!(to_path("file:///cc:/x"), "/cc:/x");
        // On Unix the same text is an ordinary absolute path.
        assert_eq!(uri_to_path_text("file:///c%3A/dir", false), "/c:/dir");
        assert_eq!(uri_to_path_text("file:///a/b", false), "/a/b");
        // Other schemes: the part after the scheme, as a key.
        assert_eq!(uri_to_path_text("untitled:Untitled-1", true), "Untitled-1");
    }

    #[test]
    fn windows_paths_become_file_uris() {
        let to_uri = |path: &str| path_text_to_uri(path, true);
        assert_eq!(to_uri(r"C:\dir\a.colibri"), "file:///C%3A/dir/a.colibri");
        assert_eq!(
            to_uri(r"c:/dir\a b.colibri"),
            "file:///c%3A/dir/a%20b.colibri"
        );
        assert_eq!(
            to_uri(r"C:\Users\RUNNER~1\AppData\Local\Temp\x"),
            "file:///C%3A/Users/RUNNER~1/AppData/Local/Temp/x"
        );
        // What canonicalising adds.
        assert_eq!(
            to_uri(r"\\?\C:\dir\a.colibri"),
            "file:///C%3A/dir/a.colibri"
        );
        assert_eq!(
            to_uri(r"\\?\UNC\server\share\a.colibri"),
            "file://server/share/a.colibri"
        );
        assert_eq!(
            to_uri(r"\\server\share\a.colibri"),
            "file://server/share/a.colibri"
        );
        // And back, whichever way the drive was written.
        for path in [
            r"C:\dir\a b.colibri",
            r"c:\dir\é.colibri",
            r"\\server\share\x.colibri",
        ] {
            let back = uri_to_path_text(&to_uri(path), true);
            assert_eq!(back, path.replace('\\', "/"), "{path}");
        }
        // Unix paths are untouched by the Windows rules, a backslash included.
        assert_eq!(path_text_to_uri("/a/b\\c", false), "file:///a/b%5Cc");
    }
}
