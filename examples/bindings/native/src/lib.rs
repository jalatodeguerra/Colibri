//! The Rust side of the bindings example. Colibri has no tables of display widths or grapheme
//! rules; two crates that do are wrapped here, and a `Table` class keeps rows on the Rust side.

use colibri_runtime as colibri;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// How many columns of a terminal the text takes (a wide character takes two, a combining mark
/// none).
#[colibri::export]
pub fn display_width(text: &str) -> i64 {
    UnicodeWidthStr::width(text) as i64
}

/// The user-perceived characters of the text: `"e\u{301}"` is one, a family emoji is one.
#[colibri::export]
pub fn graphemes(text: &str) -> Vec<String> {
    text.graphemes(true).map(str::to_string).collect()
}

/// The text padded with spaces on the right to `width` columns. Text that is wider than that is
/// an error (nothing is cut), and so is a negative width.
#[colibri::export]
pub fn pad(text: &str, width: i64) -> Result<String, String> {
    let want = usize::try_from(width).map_err(|_| format!("a width of {width} columns"))?;
    let have = UnicodeWidthStr::width(text);
    if have > want {
        return Err(format!("`{text}` is {have} columns wide, more than {want}"));
    }
    Ok(format!("{text}{}", " ".repeat(want - have)))
}

/// Rows of text under headers; each column is as wide as its widest cell.
#[colibri::export]
pub struct Table {
    headers: Vec<String>,
    rows: Vec<Vec<String>>,
}

#[colibri::export]
impl Table {
    pub fn new(headers: Vec<String>) -> Table {
        Table {
            headers,
            rows: Vec::new(),
        }
    }

    /// Adds a row; it must have a cell for every header. Gives the number of rows.
    pub fn add_row(&mut self, cells: Vec<String>) -> Result<i64, String> {
        if cells.len() != self.headers.len() {
            return Err(format!(
                "a row of {} cells in a table of {} columns",
                cells.len(),
                self.headers.len()
            ));
        }
        self.rows.push(cells);
        Ok(self.rows.len() as i64)
    }

    pub fn row_count(&self) -> i64 {
        self.rows.len() as i64
    }

    /// The width of a column, in terminal columns. A column that is not there is a bug of the
    /// caller, and panics (the script sees a panic, not an `err`).
    pub fn column_width(&self, index: i64) -> i64 {
        let column = index as usize;
        let header = UnicodeWidthStr::width(self.headers[column].as_str());
        self.rows
            .iter()
            .map(|row| UnicodeWidthStr::width(row[column].as_str()))
            .fold(header, usize::max) as i64
    }

    /// The table as text: a header line, a rule, and a line for each row, cells aligned by
    /// display width.
    pub fn render(&self) -> String {
        let widths: Vec<usize> = (0..self.headers.len())
            .map(|i| self.column_width(i as i64) as usize)
            .collect();
        let line = |cells: &[String]| {
            let padded: Vec<String> = cells
                .iter()
                .zip(&widths)
                .map(|(cell, width)| {
                    let fill = width - UnicodeWidthStr::width(cell.as_str());
                    format!("{cell}{}", " ".repeat(fill))
                })
                .collect();
            padded.join(" | ").trim_end().to_string()
        };
        let mut out = vec![line(&self.headers)];
        out.push(
            widths
                .iter()
                .map(|w| "-".repeat(*w))
                .collect::<Vec<_>>()
                .join("-+-"),
        );
        out.extend(self.rows.iter().map(|row| line(row)));
        out.join("\n")
    }
}

#[colibri::module]
fn native(m: &mut colibri::Module<'_>) -> Result<(), colibri::Error> {
    m.add_class::<Table>()?;
    m.add_fn(display_width::export)?;
    m.add_fn(graphemes::export)?;
    m.add_fn(pad::export)?;
    Ok(())
}
