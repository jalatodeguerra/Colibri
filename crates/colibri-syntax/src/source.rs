//! Source files, spans and offset to line/column lookup.

/// Identifies a file registered in a [`SourceMap`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FileId(pub u32);

/// A half-open byte range `start..end` in one source file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Span {
    pub file: FileId,
    pub start: u32,
    pub end: u32,
}

impl Span {
    pub fn new(file: FileId, start: u32, end: u32) -> Self {
        debug_assert!(start <= end);
        Span { file, start, end }
    }

    /// The smallest span covering both `self` and `other` (same file assumed).
    pub fn to(self, other: Span) -> Span {
        Span::new(
            self.file,
            self.start.min(other.start),
            self.end.max(other.end),
        )
    }

    pub fn len(self) -> u32 {
        self.end - self.start
    }

    pub fn is_empty(self) -> bool {
        self.start == self.end
    }
}

#[derive(Clone, Debug)]
struct SourceFile {
    name: String,
    text: String,
    /// Byte offset of the start of each line.
    line_starts: Vec<u32>,
}

/// Owns all loaded source files.
#[derive(Clone, Debug, Default)]
pub struct SourceMap {
    files: Vec<SourceFile>,
}

impl SourceMap {
    pub fn new() -> Self {
        Self::default()
    }

    /// The id the next file registered will have.
    pub fn next_file_id(&self) -> FileId {
        FileId(self.files.len() as u32)
    }

    /// Registers a file and returns its id.
    pub fn add_file(&mut self, name: impl Into<String>, text: impl Into<String>) -> FileId {
        let text = text.into();
        let mut line_starts = vec![0];
        line_starts.extend(
            text.bytes()
                .enumerate()
                .filter(|&(_, b)| b == b'\n')
                .map(|(i, _)| i as u32 + 1),
        );
        self.files.push(SourceFile {
            name: name.into(),
            text,
            line_starts,
        });
        FileId(self.files.len() as u32 - 1)
    }

    /// Whether `file` is one of this map's files (spans of the prelude and of host interfaces
    /// point at files that are not).
    pub fn contains(&self, file: FileId) -> bool {
        (file.0 as usize) < self.files.len()
    }

    pub fn name(&self, file: FileId) -> &str {
        &self.files[file.0 as usize].name
    }

    pub fn text(&self, file: FileId) -> &str {
        &self.files[file.0 as usize].text
    }

    /// The source text covered by `span`.
    pub fn slice(&self, span: Span) -> &str {
        &self.text(span.file)[span.start as usize..span.end as usize]
    }

    /// 1-based line and column of a byte offset. The column counts characters.
    pub fn line_col(&self, file: FileId, offset: u32) -> (u32, u32) {
        let f = &self.files[file.0 as usize];
        let line = f.line_starts.partition_point(|&s| s <= offset) - 1;
        let start = f.line_starts[line] as usize;
        let end = (offset as usize).min(f.text.len());
        let col = f.text[start..end].chars().count();
        (line as u32 + 1, col as u32 + 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_col_lookup() {
        let mut map = SourceMap::new();
        let f = map.add_file("a.colibri", "ab\ncd\n\nlast");
        assert_eq!(map.line_col(f, 0), (1, 1));
        assert_eq!(map.line_col(f, 1), (1, 2));
        assert_eq!(map.line_col(f, 2), (1, 3)); // the newline itself
        assert_eq!(map.line_col(f, 3), (2, 1));
        assert_eq!(map.line_col(f, 6), (3, 1));
        assert_eq!(map.line_col(f, 7), (4, 1));
        assert_eq!(map.line_col(f, 11), (4, 5)); // end of file
    }

    #[test]
    fn columns_count_characters() {
        let mut map = SourceMap::new();
        let f = map.add_file("u.colibri", "é=1\n");
        assert_eq!(map.line_col(f, 2), (1, 2));
    }

    #[test]
    fn multiple_files_and_slices() {
        let mut map = SourceMap::new();
        let a = map.add_file("a", "one");
        let b = map.add_file("b", "two\nthree");
        assert_eq!(map.name(b), "b");
        assert_eq!(map.text(a), "one");
        assert_eq!(map.slice(Span::new(b, 4, 9)), "three");
        assert_eq!(
            Span::new(a, 1, 2).to(Span::new(a, 0, 1)),
            Span::new(a, 0, 2)
        );
    }
}
