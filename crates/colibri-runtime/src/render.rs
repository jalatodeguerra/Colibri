//! Rendering of diagnostics for the terminal.

use std::collections::HashMap;
use std::fmt;

use ariadne::{Cache, Label, Report, ReportKind, Source};
use colibri_syntax::{Diagnostic, FileId, Severity, SourceMap, Span};

/// The sources of a report for ariadne, each one's line index built when a diagnostic first
/// shows it, and kept for the diagnostics after it: a report with many diagnostics reads each
/// file once, not once per diagnostic. Files are told apart by their id in the map, and
/// shown by name.
struct Files<'a> {
    map: &'a SourceMap,
    built: HashMap<u32, Source<&'a str>>,
}

impl<'a> Files<'a> {
    fn new(map: &'a SourceMap) -> Self {
        Files {
            map,
            built: HashMap::new(),
        }
    }
}

impl<'a> Cache<u32> for Files<'a> {
    type Storage = &'a str;

    fn fetch(&mut self, id: &u32) -> Result<&Source<&'a str>, impl fmt::Debug> {
        let map = self.map;
        Ok::<_, ()>(
            self.built
                .entry(*id)
                .or_insert_with(|| Source::from(map.text(FileId(*id)))),
        )
    }

    fn display<'b>(&self, id: &'b u32) -> Option<impl fmt::Display + 'b> {
        Some(self.map.name(FileId(*id)).to_string())
    }
}

/// Renders `diag` with source snippets, as plain text when `color` is false. Labels may point
/// into other files than the primary span (a cycle, "first defined here"); each file named is
/// shown with its own snippet.
pub fn render_diagnostic(diag: &Diagnostic, map: &SourceMap, color: bool) -> String {
    render_one(diag, map, &mut Files::new(map), color)
}

/// Every diagnostic of `diags` rendered one after the other (see [`render_diagnostic`]). The
/// sources are prepared once for all of them, so the time is in proportion to their number.
pub fn render_diagnostics<'d>(
    diags: impl IntoIterator<Item = &'d Diagnostic>,
    map: &SourceMap,
    color: bool,
) -> String {
    let mut files = Files::new(map);
    diags
        .into_iter()
        .map(|d| render_one(d, map, &mut files, color))
        .collect()
}

fn render_one(diag: &Diagnostic, map: &SourceMap, files: &mut Files<'_>, color: bool) -> String {
    let file = map.name(diag.span.file).to_string();
    let kind = match diag.severity {
        Severity::Error => ReportKind::Error,
        Severity::Warning => ReportKind::Warning,
    };
    let range = |s: Span| (s.file.0, s.start as usize..s.end as usize);
    let mut report = Report::build(kind, range(diag.span))
        .with_config(ariadne::Config::default().with_color(color))
        .with_message(&diag.message)
        .with_label(Label::new(range(diag.span)).with_message(&diag.message));
    // A label in a file the map does not have (the prelude, a host interface) cannot be shown
    // and is left out.
    for (span, text) in &diag.labels {
        if map.contains(span.file) {
            report = report.with_label(Label::new(range(*span)).with_message(text));
        }
    }
    if let Some(hint) = &diag.hint {
        report = report.with_help(hint);
    }
    let mut out = Vec::new();
    let written = report.finish().write(files, &mut out);
    if written.is_err() {
        return format!("{}: {}\n", file, diag.message);
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_without_panicking() {
        let mut map = SourceMap::new();
        let f = map.add_file("t.colibri", "let x = 1\nprint(y)\n");
        let at = |s, e| Span::new(f, s, e);
        let d = Diagnostic::error("unknown name `y`", at(16, 17))
            .with_label(at(4, 5), "`x` is defined here")
            .with_hint("did you mean `x`?");
        let text = render_diagnostic(&d, &map, false);
        assert!(text.contains("unknown name `y`"));
        assert!(text.contains("t.colibri"));
        assert!(text.contains("did you mean"));
    }
}
