//! Support code for the `colibri` command line tool.

pub mod golden;
mod render;

use std::io::{self, Write};
use std::path::Path;

use colibri_syntax::{Diagnostic, SourceMap, parse_file};

pub use render::render_diagnostic;

/// Reads and parses `path`, writing every diagnostic to `out` (rendered with
/// source snippets). Returns whether any of them is an error.
pub fn check_syntax(path: &Path, color: bool, out: &mut impl Write) -> io::Result<bool> {
    let text = std::fs::read_to_string(path)?;
    let mut map = SourceMap::new();
    let file = map.add_file(path.display().to_string(), text);
    let (_, diagnostics) = parse_file(file, map.text(file));
    for diagnostic in &diagnostics {
        out.write_all(render_diagnostic(diagnostic, &map, color).as_bytes())?;
    }
    Ok(diagnostics.iter().any(Diagnostic::is_error))
}
