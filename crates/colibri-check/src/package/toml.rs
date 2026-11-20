//! A small TOML subset, enough for `colibri.toml` (ARCHITECTURE §11.6).
//!
//! Understood: `#` comments, `[table]` and `[a.b]` headers, `key = "string"` (basic strings with
//! the usual escapes, and `'literal'` strings), `key = { inline = "tables" }` on one line, and
//! bare or quoted keys, the booleans `true` and `false`, and one-line arrays of strings
//! (`["a", "b"]`). Everything else (numbers, other arrays, dotted keys, multi-line
//! strings) is an error that says what was found, with its line and column. Duplicate keys and
//! tables are errors.

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

/// Parses `text` (the contents of file `file`).
pub fn parse(file: FileId, text: &str) -> Result<Vec<Table>, Diagnostic> {
    let mut p = Parser { file, text, pos: 0 };
    p.document()
}

struct Parser<'a> {
    file: FileId,
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
        loop {
            self.blanks();
            match self.peek() {
                None => break,
                Some('\n') => self.pos += 1,
                Some('\r') if self.rest().starts_with("\r\n") => self.pos += 2,
                Some('#') => self.end_of_line()?,
                Some('[') => {
                    let table = self.header()?;
                    if tables.iter().any(|t| t.path == table.path) {
                        return Err(Diagnostic::error(
                            format!(
                                "the table `[{}]` is defined more than once",
                                table.path.join(".")
                            ),
                            table.span,
                        ));
                    }
                    tables.push(table);
                    self.end_of_line()?;
                }
                Some(_) => {
                    let entry = self.entry()?;
                    let current = tables.last_mut().expect("the root table exists");
                    if let Some(prev) = current.entries.iter().find(|e| e.key == entry.key) {
                        return Err(Diagnostic::error(
                            format!("the key `{}` is defined more than once", entry.key),
                            entry.key_span,
                        )
                        .with_label(prev.key_span, "first defined here"));
                    }
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
            Some(c) if c.is_ascii_digit() || c == '+' || c == '-' => {
                Err(self.error("numbers are not supported in `colibri.toml`: use a string"))
            }
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
                    return Err(self.error(
                        "an array in `colibri.toml` holds strings, on one line: `[\"a\", \"b\"]`",
                    ));
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
        self.blanks();
        if self.peek() == Some('}') {
            self.pos += 1;
        } else {
            loop {
                self.blanks();
                let entry = self.entry()?;
                if let Some(prev) = entries.iter().find(|e| e.key == entry.key) {
                    return Err(Diagnostic::error(
                        format!("the key `{}` is defined more than once", entry.key),
                        entry.key_span,
                    )
                    .with_label(prev.key_span, "first defined here"));
                }
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
}
