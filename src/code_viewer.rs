use std::{
    fs::File,
    hash::{DefaultHasher, Hash, Hasher},
    io::Read,
    ops::Range,
    path::Path,
    sync::Arc,
};

use chatt_message_format::highlight::{self, HlClass};
use gpui::{
    App, DecorationRun, Element, ElementId, FontId, FontRun, FontStyle, GlobalElementId, LayoutId,
    LineLayout, ListHorizontalSizingBehavior, ListSizingBehavior, Pixels, SharedString, Style,
    TextAlign, UniformListScrollHandle, Window, div, prelude::*, px, rgb, uniform_list,
};
use unicode_width::UnicodeWidthChar;

use crate::{fonts::CODE_FONT_FAMILY, formatted_message::syntax_color};

pub const MAX_CODE_PREVIEW_BYTES: u64 = 2 * 1024 * 1024;
pub const MAX_CODE_PREVIEW_LINE_BYTES: usize = 32 * 1024;
pub const MAX_CODE_PREVIEW_LINES: usize = 200_000;

// Cosmic Text, used by GPUI off macOS, shapes tabs at four columns. Keep the
// existing eight-column estimate for the Core Text backend.
#[cfg(target_os = "macos")]
const CODE_TAB_WIDTH: usize = 8;
#[cfg(not(target_os = "macos"))]
const CODE_TAB_WIDTH: usize = 4;

#[derive(Clone, Copy, Debug)]
enum CodeRecord {
    Line {
        source_start: u32,
        source_end: u32,
        spans_start: u32,
        spans_end: u32,
    },
    Span {
        start: u32,
        end: u32,
        class: HlClass,
    },
}

#[derive(Clone, Copy)]
struct CodeSpan {
    start: usize,
    end: usize,
    class: HlClass,
}

/// A highlighted file represented by exactly two variable-sized owned buffers:
/// the UTF-8 source and one flat line-header/span record buffer.
#[derive(Debug)]
pub struct CodeDocument {
    source: String,
    records: Box<[CodeRecord]>,
    line_count: usize,
    widest_line: usize,
    cache_key: u64,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct CodeSearchResults {
    matching_lines: Vec<CodeSearchLine>,
    match_count: usize,
}

#[derive(Debug, PartialEq, Eq)]
struct CodeSearchLine {
    line: u32,
    matches_through_line: u32,
}

impl CodeSearchResults {
    pub fn is_empty(&self) -> bool {
        self.match_count == 0
    }

    pub fn len(&self) -> usize {
        self.match_count
    }

    pub fn line_for_match(&self, match_index: usize) -> Option<usize> {
        if match_index >= self.match_count {
            return None;
        }
        let ordinal = u32::try_from(match_index + 1).ok()?;
        let index = self
            .matching_lines
            .partition_point(|line| line.matches_through_line < ordinal);
        self.matching_lines
            .get(index)
            .map(|line| line.line as usize)
    }

    fn push(&mut self, line: usize) {
        self.match_count += 1;
        let matches_through_line =
            u32::try_from(self.match_count).expect("preview source bounds match count to u32");
        if let Some(last) = self.matching_lines.last_mut()
            && last.line as usize == line
        {
            last.matches_through_line = matches_through_line;
        } else {
            self.matching_lines.push(CodeSearchLine {
                line: u32::try_from(line).expect("preview source bounds line count to u32"),
                matches_through_line,
            });
        }
    }
}

impl CodeDocument {
    pub fn load(path: &Path, file_name: &str, cache_key: u64) -> Result<Arc<Self>, String> {
        let file = File::open(path).map_err(|error| format!("failed to load · {error}"))?;
        let metadata = file
            .metadata()
            .map_err(|error| format!("failed to load · {error}"))?;
        if metadata.len() > MAX_CODE_PREVIEW_BYTES {
            return Err("file too large to preview".into());
        }

        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.take(MAX_CODE_PREVIEW_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| format!("failed to load · {error}"))?;
        if bytes.len() as u64 > MAX_CODE_PREVIEW_BYTES {
            return Err("file too large to preview".into());
        }
        let source = String::from_utf8(bytes).map_err(|_| "not a text file".to_string())?;
        validate_source_geometry(&source)?;
        Ok(Arc::new(Self::prepare(source, file_name, cache_key)))
    }

    pub fn prepare(source: String, file_name: &str, cache_key: u64) -> Self {
        let language = highlight::language_for_path(file_name);
        let runs = highlight::source_runs(&(&*source), language);
        let line_count = if source.is_empty() {
            1
        } else {
            source.bytes().filter(|byte| *byte == b'\n').count()
                + usize::from(!source.ends_with('\n'))
        };
        let mut records = Vec::with_capacity(line_count + runs.len());
        records.resize(
            line_count,
            CodeRecord::Line {
                source_start: 0,
                source_end: 0,
                spans_start: 0,
                spans_end: 0,
            },
        );

        let bytes = source.as_bytes();
        let mut line_index = 0usize;
        let mut line_start = 0usize;
        let mut run_index = 0usize;
        let mut widest_line = 0usize;
        let mut widest_columns = 0usize;

        loop {
            let line_end = bytes[line_start..]
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(bytes.len(), |offset| line_start + offset);
            while run_index < runs.len() && runs[run_index].1 as usize <= line_start {
                run_index += 1;
            }

            let spans_start = records.len();
            let mut candidate = run_index;
            while candidate < runs.len() && (runs[candidate].0 as usize) < line_end {
                let (run_start, run_end, class) = runs[candidate];
                let start = (run_start as usize).max(line_start);
                let end = (run_end as usize).min(line_end);
                if end > start {
                    let can_merge = records.len() > spans_start;
                    match records.last_mut() {
                        Some(CodeRecord::Span {
                            end: previous_end,
                            class: previous_class,
                            ..
                        }) if can_merge
                            && *previous_end as usize == start
                            && *previous_class == class =>
                        {
                            *previous_end = end as u32;
                        }
                        _ => records.push(CodeRecord::Span {
                            start: start as u32,
                            end: end as u32,
                            class,
                        }),
                    }
                }
                candidate += 1;
            }

            records[line_index] = CodeRecord::Line {
                source_start: line_start as u32,
                source_end: line_end as u32,
                spans_start: spans_start as u32,
                spans_end: records.len() as u32,
            };
            let columns = display_columns(&source[line_start..line_end]);
            if columns > widest_columns {
                widest_columns = columns;
                widest_line = line_index;
            }
            line_index += 1;

            if line_end == bytes.len() {
                break;
            }
            line_start = line_end + 1;
            if line_start == bytes.len() {
                break;
            }
        }
        debug_assert_eq!(line_index, line_count);

        Self {
            source,
            records: records.into_boxed_slice(),
            line_count,
            widest_line,
            cache_key,
        }
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn line_count(&self) -> usize {
        self.line_count
    }

    pub fn widest_line(&self) -> usize {
        self.widest_line
    }

    fn line(&self, index: usize) -> (Range<usize>, Range<usize>) {
        match self.records[index] {
            CodeRecord::Line {
                source_start,
                source_end,
                spans_start,
                spans_end,
            } => (
                source_start as usize..source_end as usize,
                spans_start as usize..spans_end as usize,
            ),
            CodeRecord::Span { .. } => unreachable!("line table precedes span records"),
        }
    }

    fn line_text(&self, index: usize) -> &str {
        let (source, _) = self.line(index);
        &self.source[source]
    }

    fn line_spans(&self, index: usize) -> impl Iterator<Item = CodeSpan> + '_ {
        let (_, spans) = self.line(index);
        self.records[spans].iter().map(|record| match *record {
            CodeRecord::Span { start, end, class } => CodeSpan {
                start: start as usize,
                end: end as usize,
                class,
            },
            CodeRecord::Line { .. } => unreachable!("span table follows line records"),
        })
    }

    fn content_hash(&self, line: usize) -> u64 {
        let mut hash = DefaultHasher::new();
        self.cache_key.hash(&mut hash);
        line.hash(&mut hash);
        hash.finish()
    }

    pub fn search(&self, query: &str) -> CodeSearchResults {
        let needle: Vec<char> = query.chars().flat_map(char::to_lowercase).collect();
        if needle.is_empty() {
            return CodeSearchResults::default();
        }
        let mut prefix = vec![0usize; needle.len()];
        for index in 1..needle.len() {
            let mut matched = prefix[index - 1];
            while matched > 0 && needle[index] != needle[matched] {
                matched = prefix[matched - 1];
            }
            if needle[index] == needle[matched] {
                matched += 1;
            }
            prefix[index] = matched;
        }

        let mut results = CodeSearchResults::default();
        for line in 0..self.line_count {
            let mut matched = 0usize;
            for character in self.line_text(line).chars().flat_map(char::to_lowercase) {
                while matched > 0 && character != needle[matched] {
                    matched = prefix[matched - 1];
                }
                if character == needle[matched] {
                    matched += 1;
                }
                if matched == needle.len() {
                    results.push(line);
                    // Match the web viewer and conventional find behavior by
                    // counting non-overlapping occurrences.
                    matched = 0;
                }
            }
        }
        results
    }
}

fn validate_source_geometry(source: &str) -> Result<(), String> {
    let bytes = source.as_bytes();
    let mut line_count = 0usize;
    let mut line_start = 0usize;
    loop {
        let line_end = bytes[line_start..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(bytes.len(), |offset| line_start + offset);
        if line_end - line_start > MAX_CODE_PREVIEW_LINE_BYTES {
            return Err("line too long to preview".into());
        }
        line_count += 1;
        if line_count > MAX_CODE_PREVIEW_LINES {
            return Err("too many lines to preview".into());
        }
        if line_end == bytes.len() {
            break;
        }
        line_start = line_end + 1;
        if line_start == bytes.len() {
            break;
        }
    }
    Ok(())
}

fn display_columns(line: &str) -> usize {
    line.chars().fold(0usize, |column, character| {
        if character == '\t' {
            column + (CODE_TAB_WIDTH - column % CODE_TAB_WIDTH)
        } else {
            column + UnicodeWidthChar::width(character).unwrap_or(0)
        }
    })
}

pub fn render_code_document(
    document: Arc<CodeDocument>,
    scroll_handle: UniformListScrollHandle,
    active_match: Option<usize>,
) -> impl IntoElement {
    let line_count = document.line_count();
    let widest_line = document.widest_line();
    let digits = line_count.max(1).ilog10() as f32 + 1.0;
    let gutter_width = px(18.0 + digits * 8.5);
    let list_document = document.clone();
    uniform_list(
        ("code-viewer-lines", document.cache_key as usize),
        line_count,
        move |range, window, _| {
            let line_height = window.line_height();
            range
                .map(|line| {
                    let number = line_number(line + 1);
                    div()
                        .h(line_height)
                        .min_w_full()
                        .flex()
                        .items_center()
                        .whitespace_nowrap()
                        .when(active_match == Some(line), |row| row.bg(rgb(0x242832)))
                        .child(
                            div()
                                .w(gutter_width)
                                .flex_none()
                                .pr_2()
                                .text_right()
                                .text_color(rgb(0x6f7680))
                                .child(number),
                        )
                        .child(CodeLineElement {
                            document: list_document.clone(),
                            line,
                        })
                })
                .collect::<Vec<_>>()
        },
    )
    .track_scroll(&scroll_handle)
    .with_width_from_item(Some(widest_line))
    .with_sizing_behavior(ListSizingBehavior::Auto)
    .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
    .size_full()
    .font_family(CODE_FONT_FAMILY)
    .text_size(px(14.0))
    .text_color(rgb(syntax_color(
        chatt_message_format::highlight::PaletteRole::Foreground,
    )))
}

struct CodeLineElement {
    document: Arc<CodeDocument>,
    line: usize,
}

impl gpui::IntoElement for CodeLineElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for CodeLineElement {
    type RequestLayoutState = Arc<LineLayout>;
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let text = self.document.line_text(self.line);
        let font_size = window.text_style().font_size.to_pixels(window.rem_size());
        let normal_font = window.text_style().font();
        let mut italic_font = normal_font.clone();
        italic_font.style = FontStyle::Italic;
        let normal_id = window.text_system().resolve_font(&normal_font);
        let italic_id = window.text_system().resolve_font(&italic_font);
        let style_hash = line_style_hash(&self.document, self.line, normal_id, italic_id);
        let document = self.document.clone();
        let line = self.line;
        let layout = window.text_system().layout_borrowed_line(
            self.document.content_hash(self.line),
            style_hash,
            text,
            font_size,
            None,
            move |push| {
                for span in document.line_spans(line) {
                    push(FontRun {
                        len: span.end - span.start,
                        font_id: if is_comment(span.class) {
                            italic_id
                        } else {
                            normal_id
                        },
                    });
                }
            },
        );
        let mut style = Style::default();
        style.size.width = layout.width.into();
        style.size.height = window.line_height().into();
        (window.request_layout(style, [], cx), layout)
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        _: gpui::Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        _: &mut Window,
        _: &mut App,
    ) {
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: gpui::Bounds<Pixels>,
        layout: &mut Self::RequestLayoutState,
        _: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let line = self.line;
        let document = self.document.clone();
        let decorations = document.line_spans(line).map(|span| DecorationRun {
            len: (span.end - span.start) as u32,
            color: rgb(syntax_color(span.class.palette_role())).into(),
            background_color: None,
            underline: None,
            strikethrough: None,
        });
        layout
            .paint_with_decorations(
                bounds.origin,
                window.line_height(),
                TextAlign::Left,
                None,
                decorations,
                window,
                cx,
            )
            .expect("code line painting failed");
    }
}

fn line_style_hash(document: &CodeDocument, line: usize, normal: FontId, italic: FontId) -> u64 {
    let mut hash = DefaultHasher::new();
    normal.hash(&mut hash);
    italic.hash(&mut hash);
    for span in document.line_spans(line) {
        (span.end - span.start).hash(&mut hash);
        is_comment(span.class).hash(&mut hash);
    }
    hash.finish()
}

const fn is_comment(class: HlClass) -> bool {
    matches!(class, HlClass::Comment | HlClass::DocComment)
}

fn line_number(value: usize) -> SharedString {
    let mut bytes = [0u8; <usize as itoap::Integer>::MAX_LEN];
    // SAFETY: `bytes` has the maximum capacity required for any `usize`.
    let len = unsafe { itoap::write_to_ptr(bytes.as_mut_ptr(), value) };
    SharedString::new(std::str::from_utf8(&bytes[..len]).expect("integer formatting is ASCII"))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn lines(document: &CodeDocument) -> Vec<&str> {
        (0..document.line_count())
            .map(|line| document.line_text(line))
            .collect()
    }

    #[test]
    fn records_index_lines_without_owning_line_strings() {
        let document = CodeDocument::prepare(
            "fn main() {\n\tprintln!(\"hi\");\n}\n".to_string(),
            "main.rs",
            1,
        );
        assert_eq!(
            lines(&document),
            ["fn main() {", "\tprintln!(\"hi\");", "}"]
        );
        assert_eq!(document.line_count(), 3);
        for line in 0..document.line_count() {
            let text_range = document.line(line).0;
            let covered = document
                .line_spans(line)
                .map(|span| span.end - span.start)
                .sum::<usize>();
            assert_eq!(covered, text_range.len());
        }
    }

    #[test]
    fn empty_and_trailing_newline_rules_match_the_web_viewer() {
        let empty = CodeDocument::prepare(String::new(), "empty.txt", 1);
        assert_eq!(lines(&empty), [""]);

        let trailing = CodeDocument::prepare("one\n\n".to_string(), "notes.txt", 2);
        assert_eq!(lines(&trailing), ["one", ""]);
    }

    #[test]
    fn streaming_search_is_case_folded_and_counts_occurrences() {
        let document =
            CodeDocument::prepare("Alpha alpha\nStraße\nALPHA".to_string(), "notes.txt", 1);
        let alpha = document.search("alpha");
        assert_eq!(
            (0..alpha.len())
                .map(|index| alpha.line_for_match(index).unwrap())
                .collect::<Vec<_>>(),
            [0, 0, 2]
        );
        assert_eq!(alpha.matching_lines.len(), 2);
        let street = document.search("straße");
        assert_eq!(street.len(), 1);
        assert_eq!(street.line_for_match(0), Some(1));
        assert!(document.search("").is_empty());
        assert!(document.search("ha\nst").is_empty());

        let overlapping = CodeDocument::prepare("aaa".to_string(), "notes.txt", 2).search("aa");
        assert_eq!(overlapping.len(), 1);
        assert_eq!(overlapping.line_for_match(0), Some(0));
    }

    #[test]
    fn tab_width_matches_the_platform_text_backend_when_selecting_the_widest_line() {
        let document = CodeDocument::prepare(
            format!("{}\n\tx", "1".repeat(CODE_TAB_WIDTH + 2)),
            "notes.txt",
            1,
        );
        assert_eq!(document.widest_line(), 0);

        let document = CodeDocument::prepare(
            format!("{}\n\tx", "1".repeat(CODE_TAB_WIDTH)),
            "notes.txt",
            2,
        );
        assert_eq!(document.widest_line(), 1);
    }

    #[test]
    fn bounded_load_rejects_oversized_and_non_utf8_files() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let oversized = directory.path().join("oversized.rs");
        let file = File::create(&oversized).expect("create oversized file");
        file.set_len(MAX_CODE_PREVIEW_BYTES + 1)
            .expect("grow oversized file");
        assert_eq!(
            CodeDocument::load(&oversized, "oversized.rs", 1).unwrap_err(),
            "file too large to preview"
        );

        let binary = directory.path().join("binary.rs");
        fs::write(&binary, [0xff, 0xfe, 0xfd]).expect("write binary file");
        assert_eq!(
            CodeDocument::load(&binary, "binary.rs", 2).unwrap_err(),
            "not a text file"
        );
    }

    #[test]
    fn bounded_load_accepts_a_file_at_the_limit() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("limit.txt");
        let mut contents = vec![b'x'; MAX_CODE_PREVIEW_BYTES as usize];
        for index in (MAX_CODE_PREVIEW_LINE_BYTES..contents.len())
            .step_by(MAX_CODE_PREVIEW_LINE_BYTES)
        {
            contents[index] = b'\n';
        }
        fs::write(&path, contents).expect("write limit-sized file");

        let document = CodeDocument::load(&path, "limit.txt", 1).expect("load text at limit");
        assert_eq!(document.source().len(), MAX_CODE_PREVIEW_BYTES as usize);
        assert_eq!(document.line_count(), 64);
    }

    #[test]
    fn bounded_load_rejects_pathological_line_and_line_count_geometry() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let long_line = directory.path().join("long-line.txt");
        fs::write(&long_line, vec![b'x'; MAX_CODE_PREVIEW_LINE_BYTES + 1])
            .expect("write long line");
        assert_eq!(
            CodeDocument::load(&long_line, "long-line.txt", 1).unwrap_err(),
            "line too long to preview"
        );

        let many_lines = directory.path().join("many-lines.txt");
        fs::write(&many_lines, vec![b'\n'; MAX_CODE_PREVIEW_LINES + 1])
            .expect("write many lines");
        assert_eq!(
            CodeDocument::load(&many_lines, "many-lines.txt", 2).unwrap_err(),
            "too many lines to preview"
        );
    }
}
