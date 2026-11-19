//! Rendering of diagnostics for the terminal.

use ariadne::{Label, Report, ReportKind};
use colibri_syntax::{Diagnostic, Severity, SourceMap, Span};

/// Renders `diag` with source snippets, as plain text when `color` is false. Labels may point
/// into other files than the primary span (a cycle, "first defined here"); each file named is
/// shown with its own snippet.
pub fn render_diagnostic(diag: &Diagnostic, map: &SourceMap, color: bool) -> String {
    let file = map.name(diag.span.file).to_string();
    let kind = match diag.severity {
        Severity::Error => ReportKind::Error,
        Severity::Warning => ReportKind::Warning,
    };
    let range = |s: Span| {
        (
            map.name(s.file).to_string(),
            s.start as usize..s.end as usize,
        )
    };
    let mut report = Report::build(kind, range(diag.span))
        .with_config(ariadne::Config::default().with_color(color))
        .with_message(&diag.message)
        .with_label(Label::new(range(diag.span)).with_message(&diag.message));
    // Every file the report mentions, once. A label in a file the map does not have (the
    // prelude, a host interface) cannot be shown and is left out.
    let mut files: Vec<(String, String)> =
        vec![(file.clone(), map.text(diag.span.file).to_string())];
    for (span, text) in &diag.labels {
        if !map.contains(span.file) {
            continue;
        }
        let name = map.name(span.file).to_string();
        if !files.iter().any(|(n, _)| *n == name) {
            files.push((name, map.text(span.file).to_string()));
        }
        report = report.with_label(Label::new(range(*span)).with_message(text));
    }
    if let Some(hint) = &diag.hint {
        report = report.with_help(hint);
    }
    let mut out = Vec::new();
    let written = report.finish().write(ariadne::sources(files), &mut out);
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
