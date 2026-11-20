//! `csv`: RFC 4180 text to records and back.
//!
//! - `parse(text) -> Result[List[List[str]]]`: every record as a list of fields.
//! - `parse_rows(text) -> Result[List[Map[str, str]]]`: the first record is the header; each
//!   other record becomes a map from column name to field, in column order. A record with a
//!   different number of fields than the header, or a repeated column name, is an error.
//! - `write(records) -> str` and `write_rows(header, rows) -> str` write records back, each
//!   ended by CRLF. A field is quoted only when it has to be (it holds the separator, a quote,
//!   CR or LF, or it is the first field of the text and starts with U+FEFF, which a reader
//!   would take for a byte order mark), and a record of one empty field or of no field is
//!   written `""` so it is not a blank line. So `parse_with(write_with(x, s), s) == x` for
//!   every `x` whose records have at least one field; a record with none reads back as
//!   `[""]`, because CSV cannot tell them apart.
//!   In `write_rows` a key that a row lacks is written as an empty field and a key that is
//!   not in the header is ignored.
//! - `parse_with`, `parse_rows_with`, `write_with` and `write_rows_with` take the separator (one
//!   character, such as `";"` or `"\t"`) as their second argument.
//!
//! Line numbers in errors count CRLF, LF and a lone CR as one line end each, also inside a
//! quoted field.
//!
//! Reading accepts CRLF, LF and a lone CR as line ends, a final line end or none, a leading
//! byte order mark, and quoted fields that hold separators, doubled quotes (`""`) and line
//! breaks. A blank line (nothing between two line ends) is skipped; to say a record of one
//! empty field, quote it. A quote in the middle of an unquoted field, text after the closing
//! quote of a field and an unclosed quote are syntax errors with a line and column.

use std::collections::HashMap;

use colibri_runtime::{
    Cx, Error, FromColibri, IntoColibri, TypeDesc, TypeRegistry, TypedError, Value, ValueKind, Vm,
};

use crate::errors::error_kind;
use crate::plain;

error_kind!(
    "csv",
    /// Why a csv call failed (`csv.Error.Syntax`).
    Kind {
        /// The text is not valid CSV (an unclosed or misplaced quote).
        Syntax,
        /// The CSV is fine but does not fit what was asked: a record with another number of
        /// fields than the header, or a column name that appears twice.
        Shape,
        /// The separator is not one character, or is a quote or a line break.
        Separator,
    }
);

/// The separator as a character: one character that is not a quote or a line break.
fn separator(sep: &str) -> Result<char, TypedError> {
    let mut chars = sep.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if !matches!(c, '"' | '\r' | '\n') => Ok(c),
        _ => Err(Kind::Separator.error(format!(
            "csv: the separator must be one character that is not a quote or a line break, found {sep:?}"
        ))),
    }
}

/// Reads records. `Err` is a complete message.
fn parse(text: &str, sep: char) -> Result<Vec<Vec<String>>, String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut records: Vec<Vec<String>> = Vec::new();
    let mut chars = text.chars().peekable();
    let (mut line, mut column) = (1usize, 1usize);
    let fail = |line: usize, column: usize, what: &str| {
        Err(format!(
            "csv: invalid syntax at line {line}, column {column}: {what}"
        ))
    };
    loop {
        // One record.
        let mut record: Vec<String> = Vec::new();
        let mut quoted_any = false;
        loop {
            let mut field = String::new();
            if chars.peek() == Some(&'"') {
                quoted_any = true;
                let (open_line, open_column) = (line, column);
                chars.next();
                column += 1;
                loop {
                    match chars.next() {
                        None => {
                            return fail(open_line, open_column, "this quote is never closed");
                        }
                        Some('"') => {
                            column += 1;
                            if chars.peek() == Some(&'"') {
                                chars.next();
                                column += 1;
                                field.push('"');
                            } else {
                                break;
                            }
                        }
                        Some('\n') => {
                            line += 1;
                            column = 1;
                            field.push('\n');
                        }
                        // CRLF is one line end, a lone CR is one too (both stay in the field).
                        Some('\r') => {
                            field.push('\r');
                            if chars.peek() == Some(&'\n') {
                                chars.next();
                                field.push('\n');
                            }
                            line += 1;
                            column = 1;
                        }
                        Some(c) => {
                            column += 1;
                            field.push(c);
                        }
                    }
                }
                match chars.peek() {
                    None | Some('\r' | '\n') => {}
                    Some(&c) if c == sep => {}
                    Some(_) => {
                        return fail(
                            line,
                            column,
                            "after the closing quote only a separator or the end of the line may follow",
                        );
                    }
                }
            } else {
                while let Some(&c) = chars.peek() {
                    if c == sep || c == '\r' || c == '\n' {
                        break;
                    }
                    if c == '"' {
                        return fail(
                            line,
                            column,
                            "a quote may only start a field (quote the whole field to put a quote in it)",
                        );
                    }
                    field.push(c);
                    chars.next();
                    column += 1;
                }
            }
            record.push(field);
            match chars.peek().copied() {
                Some(c) if c == sep => {
                    chars.next();
                    column += 1;
                }
                _ => break,
            }
        }
        // The line end (CRLF, LF or a lone CR) or the end of the text.
        let at_end = match chars.next() {
            None => true,
            Some('\r') => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                false
            }
            Some(_) => false,
        };
        line += 1;
        column = 1;
        let blank = record.len() == 1 && record[0].is_empty() && !quoted_any;
        if !blank {
            records.push(record);
        }
        if at_end || chars.peek().is_none() {
            return Ok(records);
        }
    }
}

/// A field is quoted when it has to be: it holds the separator, a quote, CR or LF, or it is
/// the first field of the text and starts with U+FEFF (a reader would take that for a byte
/// order mark and drop it).
fn quote(field: &str, sep: char, first: bool) -> String {
    if field.contains([sep, '"', '\r', '\n']) || (first && field.starts_with('\u{feff}')) {
        format!("\"{}\"", field.replace('"', "\"\""))
    } else {
        field.to_string()
    }
}

fn write<'a>(records: impl IntoIterator<Item = Vec<&'a str>>, sep: char) -> String {
    let mut out = String::new();
    let mut first_record = true;
    for record in records {
        match record.as_slice() {
            // A record of one empty field, or of none, is `""`, so it is not a blank line
            // (which a reader skips). `[]` reads back as `[""]`: CSV cannot tell them apart.
            [] | [""] => out.push_str("\"\""),
            _ => {
                let fields: Vec<String> = record
                    .iter()
                    .enumerate()
                    .map(|(i, f)| quote(f, sep, first_record && i == 0))
                    .collect();
                out.push_str(&fields.join(&sep.to_string()));
            }
        }
        first_record = false;
        out.push_str("\r\n");
    }
    out
}

/// A `Map[str, str]` that keeps its entries in order, as the rows of `parse_rows` need.
struct Row(Vec<(String, String)>);

impl IntoColibri for Row {
    fn ty(_: &mut TypeRegistry) -> TypeDesc {
        TypeDesc::Map(Box::new(TypeDesc::Str), Box::new(TypeDesc::Str))
    }
    fn into_colibri(self, _: &mut Cx<'_>) -> Value {
        Value::map(
            self.0
                .into_iter()
                .map(|(k, v)| (Value::str(&k), Value::str(&v)))
                .collect(),
        )
    }
}

impl FromColibri for Row {
    fn ty(_: &mut TypeRegistry) -> TypeDesc {
        TypeDesc::Map(Box::new(TypeDesc::Str), Box::new(TypeDesc::Str))
    }
    fn from_colibri(value: &Value, _: &mut Cx<'_>) -> Result<Self, Error> {
        let ValueKind::Map(entries) = value.kind() else {
            return Err(Error::Convert("expected a Map[str, str]".into()));
        };
        let mut out = Vec::with_capacity(entries.len());
        for (k, v) in entries {
            match (k.kind(), v.kind()) {
                (ValueKind::Str(k), ValueKind::Str(v)) => out.push((k, v)),
                _ => return Err(Error::Convert("expected a Map[str, str]".into())),
            }
        }
        Ok(Row(out))
    }
}

fn parse_rows(text: &str, sep: char) -> Result<Vec<Row>, TypedError> {
    let mut records = parse(text, sep)
        .map_err(|m| Kind::Syntax.error(m))?
        .into_iter();
    let Some(header) = records.next() else {
        return Ok(Vec::new());
    };
    let mut seen = std::collections::HashSet::with_capacity(header.len());
    for name in &header {
        if !seen.insert(name.as_str()) {
            return Err(Kind::Shape.error(format!(
                "csv: the column `{name}` appears twice in the header"
            )));
        }
    }
    records
        .enumerate()
        .map(|(n, record)| {
            if record.len() != header.len() {
                return Err(Kind::Shape.error(format!(
                    "csv: record {} has {} fields, but the header has {}",
                    n + 2,
                    record.len(),
                    header.len()
                )));
            }
            Ok(Row(header.iter().cloned().zip(record).collect()))
        })
        .collect()
}

fn write_rows(header: &[String], rows: &[Row], sep: char) -> String {
    let mut records: Vec<Vec<&str>> = vec![header.iter().map(String::as_str).collect()];
    for row in rows {
        // One hash lookup per header cell instead of a scan of the row.
        let by_name: HashMap<&str, &str> = row
            .0
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        records.push(
            header
                .iter()
                .map(|name| by_name.get(name.as_str()).copied().unwrap_or(""))
                .collect(),
        );
    }
    write(records, sep)
}

fn write_lists(records: &[Vec<String>], sep: char) -> String {
    write(
        records
            .iter()
            .map(|r| r.iter().map(String::as_str).collect()),
        sep,
    )
}

pub(crate) fn register(vm: &mut Vm) -> Result<(), Error> {
    Kind::declare(vm)?;
    let mut m = vm.std_module("csv");
    let syntax = |m: String| Kind::Syntax.error(m);
    plain(m.register_fn_named("parse", ["text"], move |text: String| {
        parse(&text, ',').map_err(syntax)
    }))?;
    plain(m.register_fn_named(
        "parse_with",
        ["text", "separator"],
        move |text: String, sep: String| parse(&text, separator(&sep)?).map_err(syntax),
    ))?;
    plain(m.register_fn_named("parse_rows", ["text"], |text: String| {
        parse_rows(&text, ',')
    }))?;
    plain(m.register_fn_named(
        "parse_rows_with",
        ["text", "separator"],
        |text: String, sep: String| parse_rows(&text, separator(&sep)?),
    ))?;
    plain(
        m.register_fn_named("write", ["records"], |records: Vec<Vec<String>>| {
            write_lists(&records, ',')
        }),
    )?;
    plain(m.register_fn_named(
        "write_with",
        ["records", "separator"],
        |records: Vec<Vec<String>>, sep: String| {
            Ok::<_, TypedError>(write_lists(&records, separator(&sep)?))
        },
    ))?;
    plain(m.register_fn_named(
        "write_rows",
        ["header", "rows"],
        |header: Vec<String>, rows: Vec<Row>| write_rows(&header, &rows, ','),
    ))?;
    plain(m.register_fn_named(
        "write_rows_with",
        ["header", "rows", "separator"],
        |header: Vec<String>, rows: Vec<Row>, sep: String| {
            Ok::<_, TypedError>(write_rows(&header, &rows, separator(&sep)?))
        },
    ))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(text: &str) -> Vec<Vec<String>> {
        parse(text, ',').unwrap()
    }

    fn rows(r: &[&[&str]]) -> Vec<Vec<String>> {
        r.iter()
            .map(|r| r.iter().map(|s| s.to_string()).collect())
            .collect()
    }

    #[test]
    fn plain_records_and_line_ends() {
        let want = rows(&[&["a", "b"], &["1", "2"]]);
        for text in [
            "a,b\r\n1,2\r\n",
            "a,b\n1,2\n",
            "a,b\n1,2",
            "a,b\r1,2",
            "\u{feff}a,b\n1,2",
        ] {
            assert_eq!(p(text), want, "{text:?}");
        }
        assert_eq!(p(""), rows(&[]));
        assert_eq!(p("\n\n"), rows(&[]));
        assert_eq!(p("a,,\n,"), rows(&[&["a", "", ""], &["", ""]]));
        assert_eq!(p("a\n\nb"), rows(&[&["a"], &["b"]]));
    }

    #[test]
    fn quoted_fields() {
        assert_eq!(
            p("\"a,b\",\"say \"\"hi\"\"\",\"line1\r\nline2\"\n\"\",x"),
            rows(&[&["a,b", "say \"hi\"", "line1\r\nline2"], &["", "x"]])
        );
        // A quoted empty field is a record, not a blank line.
        assert_eq!(p("\"\"\n"), rows(&[&[""]]));
    }

    #[test]
    fn syntax_errors_say_where() {
        let e = parse("a,b\n1,\"x", ',').unwrap_err();
        assert!(
            e.contains("line 2, column 3") && e.contains("never closed"),
            "{e}"
        );
        let e = parse("a,b\"c\n", ',').unwrap_err();
        assert!(e.contains("line 1, column 4"), "{e}");
        let e = parse("\"a\"b\n", ',').unwrap_err();
        assert!(e.contains("line 1, column 4"), "{e}");
    }

    #[test]
    fn other_separators() {
        assert_eq!(
            parse("a;b\n\"c;d\";e", ';').unwrap(),
            rows(&[&["a", "b"], &["c;d", "e"]])
        );
        assert_eq!(parse("a\tb", '\t').unwrap(), rows(&[&["a", "b"]]));
        assert!(separator("").is_err());
        assert!(separator("ab").is_err());
        assert!(separator("\"").is_err());
        assert!(separator("\n").is_err());
        assert_eq!(separator("é").unwrap(), 'é');
    }

    #[test]
    fn line_numbers_count_every_kind_of_line_end_in_quoted_fields() {
        for (text, line) in [
            ("\"a\nb\"x", 2),
            ("\"a\r\nb\"x", 2),
            ("\"a\rb\"x", 2),
            ("\"a\r\r\nb\nc\"x", 4),
            ("a\r\"b\r\nc\"x", 3),
        ] {
            let e = parse(text, ',').unwrap_err();
            assert!(
                e.contains(&format!("line {line}, column 3")),
                "{text:?}: {e}"
            );
        }
        // The content keeps its line ends.
        assert_eq!(p("\"a\r\nb\rc\""), rows(&[&["a\r\nb\rc"]]));
    }

    #[test]
    fn many_columns_are_checked_and_written_in_linear_time() {
        let n = 50_000;
        let header: Vec<String> = (0..n).map(|i| format!("c{i}")).collect();
        let text = format!("{}\n{}\n", header.join(","), vec!["x"; n].join(","));
        let started = std::time::Instant::now();
        assert_eq!(parse_rows(&text, ',').unwrap().len(), 1);
        let dup = format!("{},c0\n", header.join(","));
        assert!(parse_rows(&dup, ',').is_err());
        let row = Row(header
            .iter()
            .map(|h| (h.clone(), "v".to_string()))
            .collect());
        let out = write_rows(&header, &[row], ',');
        assert!(out.ends_with(&format!("{}\r\n", vec!["v"; n].join(","))));
        assert!(started.elapsed().as_secs() < 5, "{:?}", started.elapsed());
    }

    #[test]
    fn write_then_parse_gives_back_the_records() {
        let awkward = [
            "",
            " ",
            "a",
            "\u{feff}",
            "\u{feff}a",
            "a\u{feff}",
            ",",
            ";",
            "\"",
            "\"\"",
            "\r",
            "\n",
            "\r\n",
            "a\rb",
            "x\ny",
            "\t",
            "é\u{7f}\u{0}",
            "line\r\n",
            "\"q\",\r\n\"",
        ];
        for sep in [',', ';', '\t'] {
            // Every field first, alone, and in a pair, in the first and in a later record.
            let mut data: Vec<Vec<String>> = Vec::new();
            for a in awkward {
                data.push(vec![a.to_string()]);
                for b in awkward {
                    data.push(vec![a.to_string(), b.to_string()]);
                }
            }
            for later in [false, true] {
                let mut d = data.clone();
                if later {
                    d.insert(0, vec!["head".to_string()]);
                }
                let text = write_lists(&d, sep);
                assert_eq!(parse(&text, sep).unwrap(), d, "separator {sep:?}");
            }
        }
        // A BOM at the start is quoted; one in a later record is not needed.
        assert_eq!(
            write_lists(&rows(&[&["\u{feff}a"]]), ','),
            "\"\u{feff}a\"\r\n"
        );
        // No fields is written like one empty field.
        assert_eq!(write_lists(&[vec![]], ','), "\"\"\r\n");
        assert_eq!(p(&write_lists(&[vec![]], ',')), rows(&[&[""]]));
    }

    #[test]
    fn writing_quotes_only_when_needed_and_round_trips() {
        let data = rows(&[
            &["plain", "a,b", "q\"q", "multi\nline", ""],
            &[""],
            &["x", ""],
        ]);
        let text = write_lists(&data, ',');
        assert_eq!(
            text,
            "plain,\"a,b\",\"q\"\"q\",\"multi\nline\",\r\n\"\"\r\nx,\r\n"
        );
        assert_eq!(p(&text), data);
    }
}
