use gpui::Keystroke;
use std::ops::Range;

use super::schema::{BindingTable, GUI_SCHEMA_VERSION, GuiConfig};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DiagnosticSeverity {
    Warning,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ConfigDiagnostic {
    pub(crate) severity: DiagnosticSeverity,
    pub(crate) path: String,
    pub(crate) message: String,
    pub(crate) source_range: Option<Range<usize>>,
}

pub(crate) struct SourceExcerpt<'a> {
    pub(crate) line: usize,
    pub(crate) column: usize,
    pub(crate) line_text: &'a str,
    pub(crate) marker_start: usize,
    pub(crate) marker_len: usize,
}

impl ConfigDiagnostic {
    pub(crate) fn error(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            severity: DiagnosticSeverity::Error,
            path: path.into(),
            message: message.into(),
            source_range: None,
        }
    }

    pub(crate) fn warning(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            severity: DiagnosticSeverity::Warning,
            path: path.into(),
            message: message.into(),
            source_range: None,
        }
    }

    pub(crate) fn with_source_range(mut self, source_range: Range<usize>) -> Self {
        self.source_range = Some(source_range);
        self
    }

    pub(crate) fn source_excerpt<'a>(&self, source: &'a str) -> Option<SourceExcerpt<'a>> {
        let range = self.source_range.as_ref()?;
        if range.start > source.len() || !source.is_char_boundary(range.start) {
            return None;
        }
        let end = range.end.min(source.len());
        if !source.is_char_boundary(end) {
            return None;
        }
        let line_start = source[..range.start]
            .rfind('\n')
            .map_or(0, |offset| offset + 1);
        let line_end = source[range.start..]
            .find('\n')
            .map_or(source.len(), |offset| range.start + offset);
        let marker_end = end.min(line_end);
        Some(SourceExcerpt {
            line: source[..line_start]
                .bytes()
                .filter(|byte| *byte == b'\n')
                .count()
                + 1,
            column: source[line_start..range.start].chars().count() + 1,
            line_text: &source[line_start..line_end],
            marker_start: source[line_start..range.start].chars().count(),
            marker_len: source[range.start..marker_end].chars().count().max(1),
        })
    }
}

pub(crate) fn validate(config: &GuiConfig) -> Vec<ConfigDiagnostic> {
    let mut diagnostics = Vec::new();
    if config.schema_version > GUI_SCHEMA_VERSION {
        diagnostics.push(ConfigDiagnostic::error(
            "schema-version",
            format!(
                "schema version {} is newer than the supported version {GUI_SCHEMA_VERSION}",
                config.schema_version
            ),
        ));
    }

    for (path, family) in [
        (
            "fonts.interface-family",
            config.fonts.interface_family.as_str(),
        ),
        ("fonts.message-family", config.fonts.message_family.as_str()),
        ("fonts.code-family", config.fonts.code_family.as_str()),
    ] {
        if family.trim().is_empty() {
            diagnostics.push(ConfigDiagnostic::error(
                path,
                "font family must not be empty",
            ));
        }
    }

    for (path, size) in [
        ("fonts.interface-size", config.fonts.interface_size),
        ("fonts.message-size", config.fonts.message_size),
        ("fonts.code-size", config.fonts.code_size),
    ] {
        if !size.is_finite() || !(8.0..=48.0).contains(&size) {
            diagnostics.push(ConfigDiagnostic::error(
                path,
                "font size must be finite and between 8 and 48 px",
            ));
        }
    }

    for (scope, table) in [
        ("application", &config.bindings.application),
        ("composer", &config.bindings.composer),
        ("completion", &config.bindings.completion),
        ("vim", &config.bindings.vim),
        ("code-search", &config.bindings.code_search),
        ("code-viewer", &config.bindings.code_viewer),
        ("formatted-message", &config.bindings.formatted_message),
        ("non-input", &config.bindings.non_input),
    ] {
        validate_binding_table(scope, table, &mut diagnostics);
    }

    diagnostics
}

fn validate_binding_table(
    scope: &str,
    table: &BindingTable,
    diagnostics: &mut Vec<ConfigDiagnostic>,
) {
    for sequence in table.keys() {
        let path = format!("bindings.{scope}.{sequence}");
        if sequence.trim().is_empty() {
            diagnostics.push(ConfigDiagnostic::error(
                path,
                "binding sequence must not be empty",
            ));
            continue;
        }
        for chord in sequence.split_whitespace() {
            if let Err(error) = Keystroke::parse(chord) {
                diagnostics.push(ConfigDiagnostic::error(
                    path.clone(),
                    format!("invalid keystroke `{chord}`: {error}"),
                ));
            }
        }
    }
}

pub(crate) fn has_errors(diagnostics: &[ConfigDiagnostic]) -> bool {
    diagnostics
        .iter()
        .any(|diagnostic| diagnostic.severity == DiagnosticSeverity::Error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::{BindCommand, GuiConfig};

    #[test]
    fn rejects_invalid_font_values_and_binding_sequences() {
        let mut config = GuiConfig::default();
        config.fonts.interface_family = "   ".into();
        config.fonts.code_size = f32::INFINITY;
        config
            .bindings
            .composer
            .insert("not-a-real-modifier-x".into(), BindCommand::Copy);

        let diagnostics = validate(&config);
        assert!(has_errors(&diagnostics));
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.path == "fonts.interface-family")
        );
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.path == "fonts.code-size")
        );
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.path.starts_with("bindings.composer"))
        );
    }

    #[test]
    fn accepts_font_size_boundaries() {
        let mut config = GuiConfig::default();
        config.fonts.interface_size = 8.0;
        config.fonts.message_size = 48.0;
        assert!(!has_errors(&validate(&config)));
    }

    #[test]
    fn source_excerpt_reports_unicode_safe_line_and_column() {
        let source = "title = 'héllo'\nfuture-key = true\n";
        let start = source.find("future-key").unwrap();
        let diagnostic = ConfigDiagnostic::warning("future-key", "unexpected key")
            .with_source_range(start..start + "future-key".len());
        let excerpt = diagnostic.source_excerpt(source).unwrap();
        assert_eq!(excerpt.line, 2);
        assert_eq!(excerpt.column, 1);
        assert_eq!(excerpt.line_text, "future-key = true");
        assert_eq!(excerpt.marker_start, 0);
        assert_eq!(excerpt.marker_len, "future-key".len());
    }
}
