//! The diagnostic type every compiler stage reports through.

use crate::Span;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

/// A message about a source location, with optional extra labels and a hint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub severity: Severity,
    pub message: String,
    pub span: Span,
    /// Secondary labels pointing at related code.
    pub labels: Vec<(Span, String)>,
    pub hint: Option<String>,
}

impl Diagnostic {
    pub fn error(message: impl Into<String>, span: Span) -> Self {
        Self::new(Severity::Error, message, span)
    }

    pub fn warning(message: impl Into<String>, span: Span) -> Self {
        Self::new(Severity::Warning, message, span)
    }

    fn new(severity: Severity, message: impl Into<String>, span: Span) -> Self {
        Diagnostic {
            severity,
            message: message.into(),
            span,
            labels: Vec::new(),
            hint: None,
        }
    }

    /// Adds a secondary label.
    pub fn with_label(mut self, span: Span, text: impl Into<String>) -> Self {
        self.labels.push((span, text.into()));
        self
    }

    /// Sets the hint shown after the snippet.
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FileId;

    #[test]
    fn builder() {
        let s = Span::new(FileId(0), 0, 1);
        let d = Diagnostic::error("bad", s)
            .with_label(s, "here")
            .with_hint("fix it");
        assert!(d.is_error());
        assert_eq!(d.labels.len(), 1);
        assert_eq!(d.hint.as_deref(), Some("fix it"));
        assert!(!Diagnostic::warning("w", s).is_error());
    }
}
