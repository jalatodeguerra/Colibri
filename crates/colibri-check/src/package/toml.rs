//! A small TOML subset, enough for `colibri.toml` (ARCHITECTURE §11.6).
//!
//! Understood: `#` comments, `[table]` and `[a.b]` headers, `key = "string"` (basic strings with
//! the usual escapes, and `'literal'` strings), `key = { inline = "tables" }` on one line, and
//! bare or quoted keys, the booleans `true` and `false`, and one-line arrays of strings
//! (`["a", "b"]`). Everything else (numbers, other arrays, dotted keys, multi-line
//! strings) is an error that says what was found, with its line and column. Duplicate keys and
//! tables are errors.

use std::collections::{HashMap, HashSet};

use colibri_syntax::{Diagnostic, FileId, Span};

/// A value, with the span of its text.
#[derive(Clone, Debug, PartialEq)]
pub struct Value {
    pub span: Span,
    pub kind: ValueKind,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ValueKind {
    Str(String),
    Bool(bool),
    /// `["a", "b"]`: strings only.
    Array(Vec<Value>),
    Table(Vec<Entry>),
}

/// `key = value`.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub key: String,
    pub key_span: Span,
    pub value: Value,
}

/// A `[header]` and the entries under it. The entries before the first header belong to the
/// table with an empty path.
#[derive(Clone, Debug, PartialEq)]
pub struct Table {
    pub path: Vec<String>,
    pub span: Span,
    pub entries: Vec<Entry>,
}

/// What reading a `colibri.toml` or a `colibri.lock` gives: the text to show, and what it means.
///
/// This is the one way into those files (the parser below is private): every reader goes
/// through [`read_config`], so that the two decisions about credentials are made in one place
/// and never from each other.
pub struct Config {
    /// The text for the source map, every snippet and every printed line: the file with the
    /// same length in UTF-8 and in UTF-16 (see [`read_config`] for what is hidden in it).
    pub display: String,
    /// The tables, or the errors that stop the file being used.
    pub tables: Result<Vec<Table>, Vec<Diagnostic>>,
    /// Where the file has a credential written in it, when it parses: it is refused as a whole
    /// (`tables` is then the error at each of these places), and nothing is fetched or locked
    /// from it.
    pub credentials: Vec<(usize, usize)>,
}

/// Reads `text`, the contents of file `file`, which is called `name` in messages.
///
/// Refusal is decided from the parsed values alone. If the text parses, every string, key and
/// table name with a credential in it (`git::credential_like`) makes the file refused; with
/// none, the values are used as written. A comment never refuses a file, and a file that does
/// not parse is not refused for a credential: it has its parse error.
///
/// What is shown is decided apart from that. If the text parses, the display is the text with
/// each credential and each comment that has an `@` in it hidden whole (the comment found from
/// the parser's own spans, not by looking at quotes). If it does not, every line with an `@`
/// in it (or an escape for one) is hidden whole, and an error on such a line says only that the
/// line cannot be shown.
pub fn read_config(file: FileId, text: &str, name: &str) -> Config {
    match parse_named(file, text, name) {
        Ok(tables) => {
            let mut credentials = Vec::new();
            super::git::credential_spans(&tables, &mut credentials);
            credentials.sort_unstable();
            let mut hidden = credentials.clone();
            comments_with_at(text, &tables, &mut hidden);
            let display = mask_spans(text, hidden);
            let tables = if credentials.is_empty() {
                Ok(tables)
            } else {
                Err(super::git::credential_diagnostics(file, name, &credentials))
            };
            Config {
                display,
                tables,
                credentials,
            }
        }
        Err(mut error) => {
            let has_at = |at: usize| {
                let at = at.min(text.len());
                let start = text[..at].rfind('\n').map_or(0, |i| i + 1);
                let end = text[at..].find('\n').map_or(text.len(), |i| at + i);
                super::git::last_at_like(&text[start..end]).is_some()
            };
            if has_at(error.span.start as usize)
                || error.labels.iter().any(|(s, _)| has_at(s.start as usize))
            {
                error.message = format!(
                    "this line of `{name}` cannot be read (it is not shown: the file does not parse and the line has an `@` in it, so it may hold a credential)"
                );
                error.hint = None;
                error.labels.clear();
            }
            Config {
                display: mask_lines_with_at(text),
                tables: Err(vec![error]),
                credentials: Vec::new(),
            }
        }
    }
}

/// `text` with each of `spans` (byte ranges) replaced by a mask of the same length.
fn mask_spans(text: &str, mut spans: Vec<(usize, usize)>) -> String {
    if spans.is_empty() {
        return text.to_string();
    }
    spans.sort_unstable();
    let mut out = String::with_capacity(text.len());
    let mut done = 0;
    for (s, e) in spans {
        let e = e.min(text.len());
        if s < done || !text.is_char_boundary(s) || !text.is_char_boundary(e) {
            continue;
        }
        out.push_str(&text[done..s]);
        out.push_str(&super::git::mask_str(&text[s..e]));
        done = e;
    }
    out.push_str(&text[done..]);
    out
}

/// The comments of a text that parsed, as byte ranges (from the character after `#` to the end
/// of the line), for the ones that have an `@` in them. A comment starts at the first `#` of a
/// line that is outside the table headers, keys and values the parser found.
fn comments_with_at(text: &str, tables: &[Table], out: &mut Vec<(usize, usize)>) {
    if !text.contains('@') || !text.contains('#') {
        return;
    }
    let mut regions: Vec<(usize, usize)> = Vec::new();
    for t in tables {
        regions.push((t.span.start as usize, t.span.end as usize));
        for e in &t.entries {
            regions.push((e.key_span.start as usize, e.key_span.end as usize));
            regions.push((e.value.span.start as usize, e.value.span.end as usize));
        }
    }
    regions.sort_unstable();
    let inside = |p: usize| {
        let i = regions.partition_point(|r| r.0 <= p);
        i > 0 && p < regions[i - 1].1
    };
    let mut resume = 0;
    for (p, _) in text.match_indices('#') {
        if p < resume || inside(p) {
            continue;
        }
        let line_end = text[p..].find('\n').map_or(text.len(), |i| p + i);
        let end = if text[..line_end].ends_with('\r') {
            line_end - 1
        } else {
            line_end
        };
        resume = line_end;
        if text[p + 1..end].contains('@') {
            out.push((p + 1, end));
        }
    }
}

/// `text` with every line that has an `@` in it (or an escape for one) hidden whole, from its
/// first character that is not blank to the end of the line. No shape of url is looked for: a
/// file that does not parse is not understood, and a line that may hold a credential is not
/// shown.
fn mask_lines_with_at(text: &str) -> String {
    let mut spans = Vec::new();
    let mut at = 0;
    for line in text.split_inclusive('\n') {
        let content = line.strip_suffix('\n').unwrap_or(line);
        let content = content.strip_suffix('\r').unwrap_or(content);
        if super::git::last_at_like(content).is_some() {
            let blank = content.len() - content.trim_start().len();
            spans.push((at + blank, at + content.len()));
        }
        at += line.len();
    }
    mask_spans(text, spans)
}

/// The text parsed, for the file called `name` (`colibri.lock` is read with the same parser),
/// which the messages that name the file use.
fn parse_named(file: FileId, text: &str, name: &str) -> Result<Vec<Table>, Diagnostic> {
    let mut p = Parser {
        file,
        name,
        text,
        pos: 0,
    };
    p.document()
}

#[cfg(test)]
fn parse(file: FileId, text: &str) -> Result<Vec<Table>, Diagnostic> {
    parse_named(file, text, "colibri.toml")
}

struct Parser<'a> {
    file: FileId,
    /// What the file is called in a message.
    name: &'a str,
    text: &'a str,
    pos: usize,
}

impl Parser<'_> {
    fn span(&self, start: usize, end: usize) -> Span {
        Span::new(self.file, start as u32, end as u32)
    }

    fn here(&self) -> Span {
        let end = self
            .rest()
            .chars()
            .next()
            .map_or(self.pos, |c| self.pos + c.len_utf8());
        self.span(self.pos, end)
    }

    fn rest(&self) -> &str {
        &self.text[self.pos..]
    }

    fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += c.len_utf8();
        Some(c)
    }

    fn error(&self, message: impl Into<String>) -> Diagnostic {
        Diagnostic::error(message, self.here())
    }

    /// Skips spaces and tabs.
    fn blanks(&mut self) {
        while matches!(self.peek(), Some(' ' | '\t')) {
            self.pos += 1;
        }
    }

    /// Skips blanks and a trailing comment; the line must then end.
    fn end_of_line(&mut self) -> Result<(), Diagnostic> {
        self.blanks();
        if self.peek() == Some('#') {
            while !matches!(self.peek(), None | Some('\n')) {
                self.bump();
            }
        }
        match self.peek() {
            None => Ok(()),
            Some('\n') => {
                self.pos += 1;
                Ok(())
            }
            Some('\r') if self.rest().starts_with("\r\n") => {
                self.pos += 2;
                Ok(())
            }
            Some(c) => Err(self.error(format!("expected the end of the line, found `{c}`"))),
        }
    }

    fn document(&mut self) -> Result<Vec<Table>, Diagnostic> {
        let mut tables = vec![Table {
            path: Vec::new(),
            span: self.span(0, 0),
            entries: Vec::new(),
        }];
        // What was seen, by hash: a file with many tables or many keys is read in time
        // proportional to its size.
        let mut paths: HashSet<Vec<String>> = HashSet::from([Vec::new()]);
        let mut keys: HashMap<String, Span> = HashMap::new();
        loop {
            self.blanks();
            match self.peek() {
                None => break,
                Some('\n') => self.pos += 1,
                Some('\r') if self.rest().starts_with("\r\n") => self.pos += 2,
                Some('#') => self.end_of_line()?,
                Some('[') => {
                    let table = self.header()?;
                    if !paths.insert(table.path.clone()) {
                        return Err(Diagnostic::error(
                            format!(
                                "the table `[{}]` is defined more than once",
                                table.path.join(".")
                            ),
                            table.span,
                        ));
                    }
                    tables.push(table);
                    keys.clear();
                    self.end_of_line()?;
                }
                Some(_) => {
                    let entry = self.entry()?;
                    let current = tables.last_mut().expect("the root table exists");
                    if let Some(&first) = keys.get(&entry.key) {
                        return Err(Diagnostic::error(
                            format!("the key `{}` is defined more than once", entry.key),
                            entry.key_span,
                        )
                        .with_label(first, "first defined here"));
                    }
                    keys.insert(entry.key.clone(), entry.key_span);
                    current.entries.push(entry);
                    self.end_of_line()?;
                }
            }
        }
        Ok(tables)
    }

    fn header(&mut self) -> Result<Table, Diagnostic> {
        let start = self.pos;
        self.bump(); // `[`
        if self.peek() == Some('[') {
            return Err(self.error("arrays of tables (`[[..]]`) are not supported"));
        }
        let mut path = Vec::new();
        loop {
            self.blanks();
            path.push(self.key()?.0);
            self.blanks();
            match self.peek() {
                Some('.') => {
                    self.pos += 1;
                }
                Some(']') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err(self.error("expected `.` or `]` in a table header")),
            }
        }
        Ok(Table {
            path,
            span: self.span(start, self.pos),
            entries: Vec::new(),
        })
    }

    /// A bare key (`name`, `my-dep`, `a_b`) or a quoted one.
    fn key(&mut self) -> Result<(String, Span), Diagnostic> {
        let start = self.pos;
        match self.peek() {
            Some('"' | '\'') => {
                let s = self.string()?;
                Ok((s, self.span(start, self.pos)))
            }
            Some(c) if c.is_ascii_alphanumeric() || c == '_' || c == '-' => {
                while self
                    .peek()
                    .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
                {
                    self.pos += 1;
                }
                Ok((
                    self.text[start..self.pos].to_string(),
                    self.span(start, self.pos),
                ))
            }
            Some(c) => Err(self.error(format!("expected a key, found `{c}`"))),
            None => Err(self.error("expected a key, found the end of the file")),
        }
    }

    fn entry(&mut self) -> Result<Entry, Diagnostic> {
        let (key, key_span) = self.key()?;
        self.blanks();
        if self.peek() == Some('.') {
            return Err(self.error(format!(
                "dotted keys are not supported: write a `[{key}]` table or an inline table `{key} = {{ .. }}`"
            )));
        }
        if self.peek() != Some('=') {
            return Err(self.error(format!("expected `=` after the key `{key}`")));
        }
        self.pos += 1;
        self.blanks();
        let value = self.value()?;
        Ok(Entry {
            key,
            key_span,
            value,
        })
    }

    fn value(&mut self) -> Result<Value, Diagnostic> {
        let start = self.pos;
        match self.peek() {
            Some('"' | '\'') => {
                let s = self.string()?;
                Ok(Value {
                    span: self.span(start, self.pos),
                    kind: ValueKind::Str(s),
                })
            }
            Some('{') => self.inline_table(),
            Some('[') => self.array(),
            Some(c) if c.is_ascii_digit() || c == '+' || c == '-' => Err(self.error(format!(
                "numbers are not supported in `{}`: use a string",
                self.name
            ))),
            Some('t' | 'f') => {
                for (word, value) in [("true", true), ("false", false)] {
                    if self.rest().starts_with(word) {
                        for _ in 0..word.len() {
                            self.bump();
                        }
                        return Ok(Value {
                            span: self.span(start, self.pos),
                            kind: ValueKind::Bool(value),
                        });
                    }
                }
                Err(self.error("expected a string, `true`, `false` or `{`"))
            }
            Some(c) => Err(self.error(format!("expected a string or `{{`, found `{c}`"))),
            None => Err(self.error("expected a value, found the end of the file")),
        }
    }

    /// `["a", "b"]` on one line; the elements are strings.
    fn array(&mut self) -> Result<Value, Diagnostic> {
        let start = self.pos;
        self.bump(); // `[`
        let mut items = Vec::new();
        loop {
            self.blanks();
            match self.peek() {
                Some(']') => {
                    self.pos += 1;
                    break;
                }
                Some('"' | '\'') => {
                    let item_start = self.pos;
                    let s = self.string()?;
                    items.push(Value {
                        span: self.span(item_start, self.pos),
                        kind: ValueKind::Str(s),
                    });
                    self.blanks();
                    match self.peek() {
                        Some(',') => self.pos += 1,
                        Some(']') => {}
                        _ => {
                            return Err(self.error(
                                "expected `,` or `]` in an array (it must be on one line)",
                            ));
                        }
                    }
                }
                _ => {
                    return Err(self.error(format!(
                        "an array in `{}` holds strings, on one line: `[\"a\", \"b\"]`",
                        self.name
                    )));
                }
            }
        }
        Ok(Value {
            span: self.span(start, self.pos),
            kind: ValueKind::Array(items),
        })
    }

    fn inline_table(&mut self) -> Result<Value, Diagnostic> {
        let start = self.pos;
        self.bump(); // `{`
        let mut entries: Vec<Entry> = Vec::new();
        let mut keys: HashMap<String, Span> = HashMap::new();
        self.blanks();
        if self.peek() == Some('}') {
            self.pos += 1;
        } else {
            loop {
                self.blanks();
                let entry = self.entry()?;
                if let Some(&first) = keys.get(&entry.key) {
                    return Err(Diagnostic::error(
                        format!("the key `{}` is defined more than once", entry.key),
                        entry.key_span,
                    )
                    .with_label(first, "first defined here"));
                }
                keys.insert(entry.key.clone(), entry.key_span);
                entries.push(entry);
                self.blanks();
                match self.peek() {
                    Some(',') => self.pos += 1,
                    Some('}') => {
                        self.pos += 1;
                        break;
                    }
                    _ => {
                        return Err(self.error(
                            "expected `,` or `}` in an inline table (it must be on one line)",
                        ));
                    }
                }
            }
        }
        Ok(Value {
            span: self.span(start, self.pos),
            kind: ValueKind::Table(entries),
        })
    }

    fn string(&mut self) -> Result<String, Diagnostic> {
        let quote = self.bump().expect("the caller saw a quote");
        if self.rest().starts_with(quote) && self.rest()[1..].starts_with(quote) {
            return Err(self.error("multi-line strings are not supported"));
        }
        let mut out = String::new();
        loop {
            let at = self.here();
            match self.bump() {
                None | Some('\n') => {
                    return Err(Diagnostic::error("this string is not closed", at));
                }
                Some(c) if c == quote => return Ok(out),
                Some('\\') if quote == '"' => match self.bump() {
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    Some('r') => out.push('\r'),
                    Some('"') => out.push('"'),
                    Some('\\') => out.push('\\'),
                    _ => {
                        return Err(Diagnostic::error(
                            "an unknown escape (use `\\n`, `\\t`, `\\r`, `\\\"` or `\\\\`)",
                            self.span(self.pos - 1, self.pos),
                        ));
                    }
                },
                Some(c) => out.push(c),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(text: &str) -> Vec<Table> {
        parse(FileId(0), text).expect("parses")
    }

    fn err(text: &str) -> String {
        parse(FileId(0), text).expect_err("fails").message
    }

    #[test]
    fn tables_keys_and_inline_tables() {
        let t = doc(
            "# top\n[package]\nname = \"game\" # trailing\n\n[dependencies]\nphysics = { path = \"../physics\", extra = 'x' }\n",
        );
        assert_eq!(t.len(), 3);
        assert_eq!(t[1].path, ["package"]);
        assert_eq!(t[1].entries[0].key, "name");
        assert_eq!(t[1].entries[0].value.kind, ValueKind::Str("game".into()));
        let ValueKind::Table(inline) = &t[2].entries[0].value.kind else {
            panic!("an inline table");
        };
        assert_eq!(inline[0].key, "path");
        assert_eq!(inline[1].value.kind, ValueKind::Str("x".into()));
    }

    #[test]
    fn dotted_headers_quoted_keys_and_escapes() {
        let t = doc("[dependencies.\"my-dep\"]\npath = \"a\\\\b\\\"c\"\r\n");
        assert_eq!(t[1].path, ["dependencies", "my-dep"]);
        assert_eq!(t[1].entries[0].value.kind, ValueKind::Str("a\\b\"c".into()));
    }

    #[test]
    fn errors_say_what_is_wrong() {
        assert!(err("a = 1\n").contains("numbers are not supported"));
        assert!(err("a = tru\n").contains("`true`"));
        assert!(err("a = [1]\n").contains("holds strings"));
        assert!(err("a = [\"x\" \"y\"]\n").contains("expected `,` or `]`"));
        assert!(err("a = [\"x\",\n").contains("holds strings"));
        assert!(err("a.b = \"x\"\n").contains("dotted keys"));
        assert!(err("a = \"x\nb = \"y\"\n").contains("not closed"));
        assert!(err("a = \"x\" b\n").contains("end of the line"));
        assert!(err("a \"x\"\n").contains("expected `=`"));
        assert!(err("[a]\n[a]\n").contains("more than once"));
        assert!(err("a = \"1\"\na = \"2\"\n").contains("more than once"));
        assert!(err("a = { b = \"1\"\n").contains("inline table"));
        assert!(err("[[a]]\n").contains("arrays of tables"));
        assert!(err("a = \"\"\"x\"\"\"\n").contains("multi-line"));
        assert!(err("a = \"\\q\"\n").contains("escape"));
    }

    #[test]
    fn an_empty_inline_table_and_an_empty_file_parse() {
        assert_eq!(doc("").len(), 1);
        let t = doc("a = {}\n");
        assert_eq!(t[0].entries[0].value.kind, ValueKind::Table(Vec::new()));
    }

    #[test]
    fn arrays_of_strings() {
        let t = parse(FileId(0), "a = [\"x\", 'y',]\nb = []\nc = [ \"z\" ]\n").unwrap();
        let strs = |i: usize| -> Vec<String> {
            let ValueKind::Array(items) = &t[0].entries[i].value.kind else {
                panic!("not an array")
            };
            items
                .iter()
                .map(|v| match &v.kind {
                    ValueKind::Str(s) => s.clone(),
                    _ => panic!("not a string"),
                })
                .collect()
        };
        assert_eq!(strs(0), ["x", "y"]);
        assert!(strs(1).is_empty());
        assert_eq!(strs(2), ["z"]);
    }

    // ---- read_config ----

    fn read(text: &str) -> Config {
        read_config(FileId(0), text, "colibri.toml")
    }

    fn same_lengths(text: &str, display: &str) {
        assert_eq!(display.len(), text.len(), "{text}");
        assert_eq!(
            display.encode_utf16().count(),
            text.encode_utf16().count(),
            "{text}"
        );
    }

    #[test]
    fn a_comment_never_refuses_a_file() {
        for comment in [
            "# docs: https://medium.com/@me/post",
            "# mirror of https://www.npmjs.com/package/@scope/pkg",
            "# see http://example.com/~user@host",
            "# TODO(jane@example.com): pin a tag",
            "# Maintainer: Jane Doe <jane@example.com> (license: MIT)",
            "# clone git@github.com:me/x.git",
            "# was https://u:SeCrEt@h/r",
        ] {
            let text = format!("[package]\nname = \"app\"\n{comment}\n[dependencies]\n");
            let c = read(&text);
            assert!(c.tables.is_ok() && c.credentials.is_empty(), "{comment}");
            same_lengths(&text, &c.display);
            assert!(!c.display.contains('@'), "{comment}: {}", c.display);
            assert!(c.display.contains("name = \"app\""), "{}", c.display);
        }
        // A comment after a value, with a `#` and `@` in strings before it that stay.
        let text = "[package]\nname = \"a#b\" # contact jane.doe+x@example.com: thanks\n";
        let c = read(text);
        assert!(c.tables.is_ok(), "{:?}", c.tables.err());
        assert!(c.display.contains("name = \"a#b\" #"), "{}", c.display);
        assert!(!c.display.contains('@'), "{}", c.display);
        // A legitimate ssh url with a comment holding a url.
        let text = "[dependencies]\nlib = { git = \"ssh://git@example.invalid/r.git\" } # fork of https://github.com/@someone/lib\nb = { path = \"../@scope/lib\" }\n";
        let c = read(text);
        let tables = c.tables.expect("accepted");
        assert!(c.credentials.is_empty());
        assert_eq!(tables[1].entries[0].key, "lib");
        assert!(c.display.contains("ssh://git@example.invalid/r.git"));
        assert!(c.display.contains("../@scope/lib"));
        assert!(!c.display.contains("someone"));
    }

    #[test]
    fn a_credential_in_any_parsed_value_refuses_the_file_and_is_hidden() {
        for text in [
            "a = { path = \"https://u:SeCrEt@h/r\" }\n",
            "a = { path = \"git:SeCrEt@h:r\" }\n",
            "a = { git = \"https://SeCrEt@h/r\" }\n",
            "\"https://v:SeCrEt@h\" = { path = \"p\" }\n",
            "[dependencies.\"https://u:SeCrEt@h\"]\n",
            "a = { b = [\"https://é:SeCrEt@h\"], git = [\"u:SeCrEt@h\"] }\n",
        ] {
            let c = read(text);
            assert!(!c.credentials.is_empty(), "{text}");
            assert!(c.tables.is_err(), "{text}");
            assert!(!c.display.contains("SeCrEt"), "{text}: {}", c.display);
            same_lengths(text, &c.display);
        }
    }

    #[test]
    fn a_file_that_does_not_parse_hides_every_line_with_an_at_and_reports_the_true_error() {
        // The error is on line 3, which has no `@`; line 4 has a credential.
        let text = "[package]\nname = \"app\"\nffi = 1\n# owner: me@example.com = Jane\nlib = { git = \"https://u:SeCrEt@h\" }\n";
        let c = read(text);
        let errors = c.tables.expect_err("does not parse");
        assert_eq!(errors.len(), 1);
        assert!(
            errors[0].message.contains("numbers are not supported"),
            "{}",
            errors[0].message
        );
        assert_eq!(&text[errors[0].span.start as usize..][..1], "1");
        assert!(
            !c.display.contains("SeCrEt") && !c.display.contains("me@"),
            "{}",
            c.display
        );
        assert!(c.display.contains("ffi = 1"), "the line of the error stays");
        same_lengths(text, &c.display);
        // The error is on a line with an `@` (a legitimate one): fixed text, no line text.
        let text = "[package]\nname = \"app\"\nlib = { git = \"git@github.com:me/x.git\" \n";
        let c = read(text);
        let errors = c.tables.expect_err("does not parse");
        assert!(
            errors[0].message.contains("cannot be read"),
            "{}",
            errors[0].message
        );
        assert!(!errors[0].message.contains("github") && errors[0].hint.is_none());
        assert!(!c.display.contains("github"), "{}", c.display);
        let line_start = text.find("lib").unwrap();
        assert!(
            errors[0].span.start as usize >= line_start,
            "the true position"
        );
        // Escapes for the colon and the at-sign.
        for text in [
            "lib = { git = \"https\\u003a//ghp_SeCrEtToKeN@github.com/me/r\" }\n",
            "lib = { git = \"https\\x3a//ghp_SeCrEtToKeN@github.com/me/r\" }\n",
            "lib = { git = \"ghp\\u003aSeCrEt@github.com\" }\n",
            "lib = { git = \"u:SeCrEt\\u0040github.com\" }\n",
            "lib = { git = \"u:SeCrEt\\U00000040github.com\" }\n",
            "lib = { git = \"u:SeCrEt\\x40github.com\" }\n",
        ] {
            let c = read(text);
            let errors = c.tables.expect_err(text);
            let shown = format!("{} {:?} {}", errors[0].message, errors[0].hint, c.display);
            assert!(
                !shown.contains("SeCrEt") && !shown.contains("ghp"),
                "{text}: {shown}"
            );
            same_lengths(text, &c.display);
        }
        // A backslash without an at-sign stays readable, with its own error.
        let text = "path = \"C:\\Users\\x\"\n";
        let c = read(text);
        assert!(c.display.contains("Users"), "{}", c.display);
        assert!(
            !c.tables.expect_err("bad escape")[0]
                .message
                .contains("cannot be read")
        );
    }

    #[test]
    fn what_is_hidden_has_the_length_of_the_text_in_utf8_and_utf16() {
        for text in [
            "# é😀 jane@example.com ü\nname = \"😀é\"\nx = { path = \"https://é😀:SeCrEt@h/r\" } # ü@\n",
            "# é😀 jane@example.com ü\nname = \"😀é\" 1 \nx = \"é😀 u:SeCrEt@h ü\" \n",
            "é = \"😀\" # a@b\r\n# c@d\r\n",
        ] {
            let c = read(text);
            same_lengths(text, &c.display);
            assert_eq!(text.lines().count(), c.display.lines().count());
        }
    }

    #[test]
    fn reading_takes_time_in_proportion_to_the_size() {
        for parses in [true, false] {
            let text = |n: usize| {
                let body: String = (0..n)
                    .map(|i| format!("\"https://u{i}:SeCrEt@h{i}\" = \"x\", "))
                    .collect();
                if parses {
                    format!("a = {{ {body}z = \"1\" }}\n# c@d\n")
                } else {
                    format!("a = {{ {body}\n")
                }
            };
            let time = |n: usize| {
                let text = text(n);
                let t = std::time::Instant::now();
                let c = read(&text);
                assert!(!c.display.contains("SeCrEt"));
                t.elapsed().as_secs_f64()
            };
            time(100);
            // The best of a few runs: a busy machine slows one run, not all of them.
            let best = |n: usize| (0..5).map(|_| time(n)).fold(f64::INFINITY, f64::min);
            let (small, big) = (best(1000), best(8000));
            // Eight times the text: about eight times the time, not sixty-four.
            assert!(big < 20.0 * small.max(0.002), "{parses}: {small} {big}");
        }
    }
}
