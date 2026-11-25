//! Assets: the files of a package that a program reads at run time (`import assets`), and the
//! patterns of `[build] assets` in `colibri.toml` that say which files they are.
//!
//! A pattern is written relative to the package folder, with `/` between the pieces:
//!
//! - a piece may hold `*` (any run of characters, also none) and `?` (one character);
//! - the piece `**` stands for any number of folders, also none;
//! - a pattern selects a file when it matches the file's path or the path of a folder above it,
//!   so `data` and `data/` both select everything below `data`; a trailing `/` insists on a
//!   folder, so `data/` does not select a file called `data`.
//!
//! A path an asset is read by (`assets.read("img/logo.png")`) is the same kind of text without
//! the wildcards: relative, `/`-separated, with no `.` or `..` piece. That is checked by
//! [`path_problem`] before anything is looked up, so a path never leaves the package.

/// Why `path` cannot name an asset, or `None` when it can: a relative path of plain pieces
/// separated by `/`. Written to say the same thing on every system, so a path with `\`, a
/// drive (`C:`) or a root is refused everywhere.
pub fn path_problem(path: &str) -> Option<&'static str> {
    if path.is_empty() {
        return Some("the path is empty");
    }
    if path.starts_with('/') {
        return Some("the path is absolute: asset paths are relative to the package folder");
    }
    if path.contains('\\') {
        return Some("the path has a `\\`: write `/` between the pieces");
    }
    if path.contains('\0') {
        return Some("the path has a NUL character");
    }
    for piece in path.split('/') {
        match piece {
            "" => return Some("the path has an empty piece (`//` or a `/` at the end)"),
            "." => return Some("the path has a `.` piece"),
            ".." => return Some("the path leaves the package folder with `..`"),
            _ if piece.contains(':') => {
                return Some("a piece of the path has a `:`, which is not portable");
            }
            _ => {}
        }
    }
    None
}

/// One piece of a pattern.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Piece {
    /// `**`: any number of folders.
    Folders,
    /// A name with `*` and `?` in it.
    Name(Vec<char>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Pattern {
    pieces: Vec<Piece>,
    /// The text ended with `/`: only a folder matches.
    folder: bool,
}

/// The patterns of `[build] assets`: which files of a package are its assets.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AssetPatterns {
    patterns: Vec<Pattern>,
    texts: Vec<String>,
}

/// Why `text` is not a good pattern, or `None` when it is.
pub fn pattern_problem(text: &str) -> Option<String> {
    let body = text.strip_suffix('/').unwrap_or(text);
    if body.is_empty() {
        return Some("a pattern cannot be empty".to_string());
    }
    for piece in body.split('/') {
        if piece == "**" {
            continue;
        }
        if piece.contains("**") {
            return Some("`**` must be a whole piece of the pattern (`a/**/b`)".to_string());
        }
        // The wildcards are not part of a name, so check the rest as a path piece would be.
        let plain = piece.replace(['*', '?'], "x");
        if let Some(problem) = path_problem(&plain) {
            return Some(problem.to_string());
        }
    }
    None
}

impl AssetPatterns {
    /// The patterns, or the first that is not valid and why.
    pub fn new(texts: &[String]) -> Result<AssetPatterns, String> {
        let mut patterns = Vec::new();
        for text in texts {
            if let Some(problem) = pattern_problem(text) {
                return Err(format!("`{text}`: {problem}"));
            }
            let (body, folder) = match text.strip_suffix('/') {
                Some(body) => (body, true),
                None => (text.as_str(), false),
            };
            patterns.push(Pattern {
                pieces: body
                    .split('/')
                    .map(|p| {
                        if p == "**" {
                            Piece::Folders
                        } else {
                            Piece::Name(p.chars().collect())
                        }
                    })
                    .collect(),
                folder,
            });
        }
        Ok(AssetPatterns {
            patterns,
            texts: texts.to_vec(),
        })
    }

    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    /// The patterns as they were written.
    pub fn texts(&self) -> &[String] {
        &self.texts
    }

    /// Whether the file at `path` (valid by [`path_problem`]) is selected by any pattern.
    pub fn selects(&self, path: &str) -> bool {
        let pieces: Vec<&str> = path.split('/').collect();
        self.patterns.iter().any(|p| p.selects(&pieces))
    }

    /// The patterns that select none of `files`, for the build to name them.
    pub fn unmatched<'a>(&'a self, files: &[String]) -> Vec<&'a str> {
        let split: Vec<Vec<&str>> = files.iter().map(|f| f.split('/').collect()).collect();
        self.patterns
            .iter()
            .zip(&self.texts)
            .filter(|(p, _)| !split.iter().any(|f| p.selects(f)))
            .map(|(_, t)| t.as_str())
            .collect()
    }
}

impl Pattern {
    fn selects(&self, file: &[&str]) -> bool {
        // The pattern against the whole path, and against every folder above the file.
        (1..=file.len()).any(|n| {
            let is_file = n == file.len();
            !(self.folder && is_file) && matches_pieces(&self.pieces, &file[..n])
        })
    }
}

fn matches_pieces(pattern: &[Piece], path: &[&str]) -> bool {
    match pattern.split_first() {
        None => path.is_empty(),
        Some((Piece::Folders, rest)) => {
            (0..=path.len()).any(|skip| matches_pieces(rest, &path[skip..]))
        }
        Some((Piece::Name(name), rest)) => path
            .split_first()
            .is_some_and(|(first, tail)| matches_name(name, first) && matches_pieces(rest, tail)),
    }
}

/// `*` and `?` against one name, by the usual two-pointer walk with one place to go back to.
fn matches_name(pattern: &[char], name: &str) -> bool {
    let name: Vec<char> = name.chars().collect();
    let (mut p, mut n) = (0, 0);
    let mut back: Option<(usize, usize)> = None;
    while n < name.len() {
        match pattern.get(p) {
            Some('*') => {
                back = Some((p, n));
                p += 1;
            }
            Some(&c) if c == '?' || c == name[n] => {
                p += 1;
                n += 1;
            }
            _ => match back {
                Some((bp, bn)) => {
                    back = Some((bp, bn + 1));
                    p = bp + 1;
                    n = bn + 1;
                }
                None => return false,
            },
        }
    }
    pattern[p..].iter().all(|&c| c == '*')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn patterns(texts: &[&str]) -> AssetPatterns {
        AssetPatterns::new(&texts.iter().map(|t| t.to_string()).collect::<Vec<_>>()).unwrap()
    }

    #[test]
    fn paths_that_leave_the_package_are_refused() {
        for bad in [
            "",
            "/etc/passwd",
            "../x",
            "a/../../x",
            "a/..",
            "./x",
            "a/./b",
            "a//b",
            "a/",
            "a\\b",
            "C:/x",
            "C:x",
            "a\0b",
        ] {
            assert!(path_problem(bad).is_some(), "{bad:?}");
        }
        for good in [
            "a",
            "a/b.txt",
            "img/logo.png",
            ".hidden",
            "a b/c",
            "..x",
            "x..",
        ] {
            assert_eq!(path_problem(good), None, "{good:?}");
        }
    }

    #[test]
    fn a_folder_pattern_selects_what_is_below_it() {
        let p = patterns(&["data/", "logo.png"]);
        assert!(p.selects("data/a.txt"));
        assert!(p.selects("data/deep/er/a.txt"));
        assert!(p.selects("logo.png"));
        assert!(!p.selects("other/logo.png"));
        assert!(!p.selects("database/a.txt"));
        assert!(!p.selects("data"));
        // Without the slash the name selects the same folder, and a file of that name.
        let p = patterns(&["data"]);
        assert!(p.selects("data/a.txt"));
        assert!(p.selects("data"));
        // With it, only a folder.
        assert!(!patterns(&["data/"]).selects("data"));
    }

    #[test]
    fn wildcards_stay_inside_a_piece_and_double_star_crosses_folders() {
        let p = patterns(&["img/*.png"]);
        assert!(p.selects("img/a.png"));
        assert!(p.selects("img/.png"));
        assert!(!p.selects("img/a.jpg"));
        assert!(!p.selects("img/sub/a.png"));
        let p = patterns(&["t/**/*.html"]);
        assert!(p.selects("t/a.html"));
        assert!(p.selects("t/x/y/a.html"));
        assert!(!p.selects("t/x/y/a.txt"));
        assert!(!p.selects("u/a.html"));
        let p = patterns(&["f?.txt"]);
        assert!(p.selects("f1.txt"));
        assert!(!p.selects("f.txt"));
        assert!(!p.selects("f12.txt"));
        assert!(patterns(&["**"]).selects("a/b/c"));
        assert!(patterns(&["a*b*c"]).selects("aXXbYYc"));
        assert!(!patterns(&["a*b*c"]).selects("aXXbYY"));
    }

    #[test]
    fn bad_patterns_are_named() {
        for bad in [
            "", "/", "/abs", "../x", "a/../b", "a\\b", "a**", "**b/c", "a//b", "./a", "C:/x",
        ] {
            assert!(pattern_problem(bad).is_some(), "{bad:?}");
            assert!(AssetPatterns::new(&[bad.to_string()]).is_err(), "{bad:?}");
        }
        for good in ["a", "a/", "a/**", "**/x", "a/*/b", "*.txt", "d?/x.*"] {
            assert_eq!(pattern_problem(good), None, "{good:?}");
        }
    }

    #[test]
    fn a_pattern_that_selects_nothing_is_found() {
        let p = patterns(&["a/", "b.txt", "c/*.png"]);
        let files = vec!["a/x".to_string(), "c/y.jpg".to_string()];
        assert_eq!(p.unmatched(&files), ["b.txt", "c/*.png"]);
    }
}
