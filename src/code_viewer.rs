use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    hash::{DefaultHasher, Hash, Hasher},
    ops::Range,
    rc::Rc,
    sync::Arc,
};
use unicode_width::UnicodeWidthChar;

use chatt_message_format::highlight::{self, HlClass};
use gpui::{
    App, BorderStyle, Bounds, ClipboardItem, CursorStyle, DecorationRun, DispatchPhase, Edges,
    Element, ElementId, FocusHandle, FontRun, FontStyle, GlobalElementId, Hitbox, HitboxBehavior,
    Hsla, KeyContext, LayoutId, LineLayout, ListHorizontalSizingBehavior, ListSizingBehavior,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point, SharedString, Style,
    TextAlign, UniformListScrollHandle, Window, actions, div, point, prelude::*, px, quad, rgb,
    rgba, uniform_list,
};

use crate::{
    fonts::CODE_FONT_FAMILY,
    formatted_message::syntax_color,
    scrollbar::{OverlayScrollbarColors, OverlayScrollbarState, OverlayScrollbars},
    theme::{ResolvedSettings, ThemeRole, syntax_role},
};

pub const MAX_CODE_PREVIEW_BYTES: u64 = 2 * 1024 * 1024;
pub const MAX_CODE_PREVIEW_LINES: usize = 200_000;
pub const MAX_CODE_RENDERED_LINE_BYTES: usize = 1024;

const CODE_TRUNCATION_MARKER: &str = " … line truncated";
const CODE_HIDDEN_MATCH_MARKER: &str = " … match in truncated text";

// Cosmic Text, used by GPUI off macOS, shapes tabs at four columns. Core Text
// uses the platform's conventional eight-column tab stops.
#[cfg(target_os = "macos")]
const CODE_TAB_WIDTH: usize = 8;
#[cfg(not(target_os = "macos"))]
const CODE_TAB_WIDTH: usize = 4;

actions!(code_viewer, [Copy, SelectAll]);

#[derive(Clone, Copy, Debug)]
enum CodeRecord {
    Line {
        source_start: u32,
        source_end: u32,
        rendered_end: u32,
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

#[derive(Clone, Copy, Debug)]
struct CodeWidthMeasurement {
    line: usize,
    width: Pixels,
}

#[derive(Debug)]
struct CodeViewStateInner {
    width: Cell<CodeWidthMeasurement>,
    pending_reveal: Cell<Option<CodeSearchMatch>>,
}

#[derive(Clone, Debug)]
pub struct CodeViewState(Rc<CodeViewStateInner>);

impl Default for CodeViewState {
    fn default() -> Self {
        Self(Rc::new(CodeViewStateInner {
            width: Cell::new(CodeWidthMeasurement {
                line: 0,
                width: Pixels::ZERO,
            }),
            pending_reveal: Cell::new(None),
        }))
    }
}

impl CodeViewState {
    pub fn reset(&self) {
        self.0.width.set(CodeWidthMeasurement {
            line: 0,
            width: Pixels::ZERO,
        });
        self.0.pending_reveal.set(None);
    }

    fn widest_line(&self, document: &CodeDocument) -> usize {
        let measurement = self.0.width.get();
        if measurement.width == Pixels::ZERO {
            document.widest_line()
        } else {
            measurement.line
        }
    }

    fn record_width(&self, line: usize, width: Pixels) -> bool {
        let measurement = self.0.width.get();
        if width <= measurement.width {
            return false;
        }
        self.0.width.set(CodeWidthMeasurement { line, width });
        true
    }

    pub fn request_match_reveal(&self, search_match: CodeSearchMatch) {
        self.0.pending_reveal.set(Some(search_match));
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CodeSearchMatch {
    start: u32,
    end: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CodeMatchTarget {
    pub line: usize,
    pub start: usize,
    pub end: usize,
    pub hidden: bool,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct CodeSearchResults {
    matches: Box<[CodeSearchMatch]>,
}

impl CodeSearchResults {
    pub fn is_empty(&self) -> bool {
        self.matches.is_empty()
    }

    pub fn len(&self) -> usize {
        self.matches.len()
    }

    pub fn get(&self, match_index: usize) -> Option<CodeSearchMatch> {
        self.matches.get(match_index).copied()
    }
}

impl CodeDocument {
    pub fn load(bytes: &[u8], file_name: &str, cache_key: u64) -> Result<Arc<Self>, String> {
        if bytes.len() as u64 > MAX_CODE_PREVIEW_BYTES {
            return Err("file too large to preview".into());
        }
        let source =
            String::from_utf8(bytes.to_vec()).map_err(|_| "not a text file".to_string())?;
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
                rendered_end: 0,
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
            let mut rendered_end = (line_start + MAX_CODE_RENDERED_LINE_BYTES).min(line_end);
            while !source.is_char_boundary(rendered_end) {
                rendered_end -= 1;
            }
            while run_index < runs.len() && runs[run_index].1 as usize <= line_start {
                run_index += 1;
            }

            let spans_start = records.len();
            let mut candidate = run_index;
            while candidate < runs.len() && (runs[candidate].0 as usize) < rendered_end {
                let (run_start, run_end, class) = runs[candidate];
                let start = (run_start as usize).max(line_start);
                let end = (run_end as usize).min(rendered_end);
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
                rendered_end: rendered_end as u32,
                spans_start: spans_start as u32,
                spans_end: records.len() as u32,
            };
            let mut columns = display_columns(&source[line_start..rendered_end]);
            if rendered_end < line_end {
                columns += display_columns(CODE_HIDDEN_MATCH_MARKER);
            }
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

    fn line(&self, index: usize) -> (Range<usize>, Range<usize>, Range<usize>) {
        match self.records[index] {
            CodeRecord::Line {
                source_start,
                source_end,
                rendered_end,
                spans_start,
                spans_end,
            } => (
                source_start as usize..source_end as usize,
                source_start as usize..rendered_end as usize,
                spans_start as usize..spans_end as usize,
            ),
            CodeRecord::Span { .. } => unreachable!("line table precedes span records"),
        }
    }

    fn line_text(&self, index: usize) -> &str {
        let (_, rendered, _) = self.line(index);
        &self.source[rendered]
    }

    fn full_line_text(&self, index: usize) -> &str {
        let (source, _, _) = self.line(index);
        &self.source[source]
    }

    fn line_source_range(&self, index: usize) -> Range<usize> {
        self.line(index).0
    }

    fn line_rendered_range(&self, index: usize) -> Range<usize> {
        self.line(index).1
    }

    fn line_is_truncated(&self, index: usize) -> bool {
        self.line_rendered_range(index).end < self.line_source_range(index).end
    }

    fn line_selection_range(&self, index: usize) -> Range<usize> {
        let rendered = self.line_rendered_range(index);
        if self.line_is_truncated(index) {
            return rendered;
        }
        let end = if index + 1 < self.line_count {
            self.line_source_range(index + 1).start
        } else {
            self.source.len()
        };
        rendered.start..end
    }

    fn line_spans(&self, index: usize) -> impl Iterator<Item = CodeSpan> + '_ {
        let (_, _, spans) = self.line(index);
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

    pub fn match_target(&self, search_match: CodeSearchMatch) -> CodeMatchTarget {
        let offset = search_match.start as usize;
        let line = self.line_for_offset(offset);
        let source = self.line_source_range(line);
        let rendered = self.line_rendered_range(line);
        CodeMatchTarget {
            line,
            start: offset.saturating_sub(source.start),
            end: (search_match.end as usize).saturating_sub(source.start),
            hidden: search_match.end as usize > rendered.end,
        }
    }

    fn line_for_offset(&self, offset: usize) -> usize {
        self.records[..self.line_count]
            .partition_point(|record| match record {
                CodeRecord::Line { source_start, .. } => *source_start as usize <= offset,
                CodeRecord::Span { .. } => unreachable!("line table precedes span records"),
            })
            .saturating_sub(1)
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

        let mut matches = Vec::new();
        let mut source_ranges = VecDeque::with_capacity(needle.len());
        for line in 0..self.line_count {
            let mut matched = 0usize;
            source_ranges.clear();
            let line_start = self.line_source_range(line).start;
            for (local_start, original) in self.full_line_text(line).char_indices() {
                let absolute_start = line_start + local_start;
                let absolute_end = absolute_start + original.len_utf8();
                let absolute_start = u32::try_from(absolute_start)
                    .expect("preview source bounds match offset to u32");
                let absolute_end =
                    u32::try_from(absolute_end).expect("preview source bounds match offset to u32");
                for character in original.to_lowercase() {
                    source_ranges.push_back((absolute_start, absolute_end));
                    if source_ranges.len() > needle.len() {
                        source_ranges.pop_front();
                    }
                    while matched > 0 && character != needle[matched] {
                        matched = prefix[matched - 1];
                    }
                    if character == needle[matched] {
                        matched += 1;
                    }
                    if matched == needle.len() {
                        let start = source_ranges
                            .front()
                            .expect("matching source window covers the needle")
                            .0;
                        matches.push(CodeSearchMatch {
                            start,
                            end: absolute_end,
                        });
                        // Match the web viewer and conventional find behavior by
                        // counting non-overlapping occurrences.
                        matched = 0;
                    }
                }
            }
        }
        CodeSearchResults {
            matches: matches.into_boxed_slice(),
        }
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
    let bytes = line.as_bytes();
    let mut byte_offset = 0usize;
    let mut column = 0usize;
    while byte_offset < bytes.len() {
        let byte = bytes[byte_offset];
        if byte.is_ascii() {
            column += match byte {
                b'\t' => CODE_TAB_WIDTH - column % CODE_TAB_WIDTH,
                b' '..=b'~' => 1,
                _ => 0,
            };
            byte_offset += 1;
        } else {
            let character = line[byte_offset..]
                .chars()
                .next()
                .expect("non-ASCII byte begins a UTF-8 character");
            column += UnicodeWidthChar::width(character).unwrap_or(0);
            byte_offset += character.len_utf8();
        }
    }
    column
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CodeSelectMode {
    Character,
    Word,
    Line,
    All,
}

#[derive(Clone, Debug)]
struct ActiveCodeSelection {
    anchor_range: Range<usize>,
    head_line: usize,
    head_offset: usize,
    start: usize,
    end: usize,
    reversed: bool,
    pending: bool,
    mode: CodeSelectMode,
}

#[derive(Clone)]
struct CodeSelectionParticipant {
    line: usize,
    bounds: Bounds<Pixels>,
    text_bounds: Bounds<Pixels>,
    layout: Arc<LineLayout>,
    marker_x: Option<Range<Pixels>>,
}

struct CodeSelectionState {
    focus_handle: FocusHandle,
    document: Option<Arc<CodeDocument>>,
    viewport: Option<Bounds<Pixels>>,
    current: Vec<CodeSelectionParticipant>,
    active: Option<ActiveCodeSelection>,
}

#[derive(Clone)]
pub struct CodeSelection(Rc<RefCell<CodeSelectionState>>);

impl CodeSelection {
    pub fn new(focus_handle: FocusHandle) -> Self {
        Self(Rc::new(RefCell::new(CodeSelectionState {
            focus_handle,
            document: None,
            viewport: None,
            current: Vec::new(),
            active: None,
        })))
    }

    pub fn clear(&self) {
        let state = &mut *self.0.borrow_mut();
        state.document = None;
        state.viewport = None;
        state.active = None;
        state.current.clear();
    }

    fn begin_frame(&self, document: Arc<CodeDocument>, viewport: Bounds<Pixels>) {
        let state = &mut *self.0.borrow_mut();
        if !state
            .document
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &document))
        {
            state.active = None;
        }
        state.document = Some(document);
        state.viewport = Some(viewport);
        state.current.clear();
    }

    fn focus_handle(&self) -> FocusHandle {
        self.0.borrow().focus_handle.clone()
    }

    fn viewport(&self) -> Option<Bounds<Pixels>> {
        self.0.borrow().viewport
    }

    fn register(
        &self,
        line: usize,
        bounds: Bounds<Pixels>,
        text_bounds: Bounds<Pixels>,
        layout: Arc<LineLayout>,
        marker_x: Option<Range<Pixels>>,
    ) {
        self.0.borrow_mut().current.push(CodeSelectionParticipant {
            line,
            bounds,
            text_bounds,
            layout,
            marker_x,
        });
    }

    fn contains_position(&self, position: Point<Pixels>) -> bool {
        self.0
            .borrow()
            .current
            .iter()
            .any(|participant| participant.bounds.contains(&position))
    }

    fn begin_selection(&self, line: usize, offset: usize, click_count: usize, extend: bool) {
        let state = &mut *self.0.borrow_mut();
        let Some(document) = state.document.as_ref() else {
            return;
        };
        let extending = extend && click_count == 1 && state.active.is_some();
        let (anchor_range, mode) = if extending {
            let active = state.active.as_ref().unwrap();
            let anchor = if active.reversed {
                active.end
            } else {
                active.start
            };
            (anchor..anchor, CodeSelectMode::Character)
        } else {
            match click_count {
                1 => (offset..offset, CodeSelectMode::Character),
                2 => (document.word_range_at(line, offset), CodeSelectMode::Word),
                3 => (document.line_selection_range(line), CodeSelectMode::Line),
                _ => (0..document.source.len(), CodeSelectMode::All),
            }
        };
        state.active = Some(ActiveCodeSelection {
            anchor_range: anchor_range.clone(),
            head_line: line,
            head_offset: offset,
            start: anchor_range.start,
            end: anchor_range.end,
            reversed: false,
            pending: true,
            mode,
        });
        state.recompute_active_selection();
    }

    fn update_head_for_position(&self, position: Point<Pixels>) -> bool {
        let participant = {
            let state = self.0.borrow();
            if !state.active.as_ref().is_some_and(|active| active.pending) {
                return false;
            }
            state.participant_nearest(position)
        };
        let Some(participant) = participant else {
            return false;
        };
        let line_start = {
            let state = self.0.borrow();
            let Some(document) = state.document.as_ref() else {
                return false;
            };
            document.line_source_range(participant.line).start
        };
        let local = participant
            .layout
            .closest_index_for_x(position.x - participant.text_bounds.left());
        self.update_head(participant.line, line_start + local)
    }

    fn update_head(&self, line: usize, offset: usize) -> bool {
        let state = &mut *self.0.borrow_mut();
        let Some(active) = state.active.as_mut() else {
            return false;
        };
        if !active.pending || (active.head_line == line && active.head_offset == offset) {
            return false;
        }
        active.head_line = line;
        active.head_offset = offset;
        state.recompute_active_selection();
        true
    }

    fn finish_selection(&self) -> Option<String> {
        let state = &mut *self.0.borrow_mut();
        let active = state.active.as_mut()?;
        if !active.pending {
            return None;
        }
        active.pending = false;
        state.selected_text()
    }

    fn select_all(&self) -> bool {
        let state = &mut *self.0.borrow_mut();
        let Some(document) = state.document.as_ref() else {
            return false;
        };
        let end = document.source.len();
        state.active = Some(ActiveCodeSelection {
            anchor_range: 0..end,
            head_line: document.line_count.saturating_sub(1),
            head_offset: end,
            start: 0,
            end,
            reversed: false,
            pending: false,
            mode: CodeSelectMode::All,
        });
        end > 0
    }

    fn is_pending(&self) -> bool {
        self.0
            .borrow()
            .active
            .as_ref()
            .is_some_and(|active| active.pending)
    }

    fn is_active(&self) -> bool {
        self.0.borrow().active.is_some()
    }

    fn active_line_height(&self) -> Option<Pixels> {
        let state = self.0.borrow();
        let line = state.active.as_ref()?.head_line;
        state
            .current
            .iter()
            .find(|participant| participant.line == line)
            .map(|participant| participant.bounds.size.height)
    }

    fn selected_text(&self) -> Option<String> {
        self.0.borrow().selected_text()
    }

    fn selected_line_range(&self, line: usize) -> Option<(Range<usize>, bool)> {
        self.0.borrow().selected_line_range(line)
    }
}

impl CodeSelectionState {
    fn participant_nearest(&self, position: Point<Pixels>) -> Option<CodeSelectionParticipant> {
        self.current
            .iter()
            .min_by_key(|participant| {
                if position.y < participant.bounds.top() {
                    participant.bounds.top() - position.y
                } else if position.y > participant.bounds.bottom() {
                    position.y - participant.bounds.bottom()
                } else {
                    Pixels::ZERO
                }
            })
            .cloned()
    }

    fn recompute_active_selection(&mut self) {
        let Some(document) = self.document.as_ref() else {
            return;
        };
        let Some(active) = self.active.as_mut() else {
            return;
        };
        match active.mode {
            CodeSelectMode::Character => {
                if active.head_offset < active.anchor_range.start {
                    active.start = active.head_offset;
                    active.end = active.anchor_range.start;
                    active.reversed = true;
                } else {
                    active.start = active.anchor_range.start;
                    active.end = active.head_offset;
                    active.reversed = false;
                }
            }
            CodeSelectMode::Word | CodeSelectMode::Line => {
                let head_range = if active.mode == CodeSelectMode::Word {
                    document.word_range_at(active.head_line, active.head_offset)
                } else {
                    document.line_selection_range(active.head_line)
                };
                if active.head_offset < active.anchor_range.start {
                    active.start = head_range.start;
                    active.end = active.anchor_range.end;
                    active.reversed = true;
                } else if active.head_offset >= active.anchor_range.end {
                    active.start = active.anchor_range.start;
                    active.end = head_range.end;
                    active.reversed = false;
                } else {
                    active.start = active.anchor_range.start;
                    active.end = active.anchor_range.end;
                    active.reversed = false;
                }
            }
            CodeSelectMode::All => {
                active.start = 0;
                active.end = document.source.len();
                active.reversed = false;
            }
        }
    }

    fn selected_text(&self) -> Option<String> {
        let active = self.active.as_ref()?;
        if active.start >= active.end {
            return None;
        }
        let document = self.document.as_ref()?;
        if active.mode == CodeSelectMode::All {
            return Some(document.source.clone());
        }

        let start_line = document.line_for_offset(active.start);
        let end_line = document.line_for_offset(active.end.saturating_sub(1));
        let mut selected = String::with_capacity(active.end - active.start);
        for line in start_line..=end_line {
            let rendered = document.line_rendered_range(line);
            let start = active.start.max(rendered.start).min(rendered.end);
            let end = active.end.min(rendered.end).max(rendered.start);
            if start < end {
                selected.push_str(
                    document
                        .source
                        .get(start..end)
                        .expect("selection endpoints remain UTF-8 boundaries"),
                );
            }
            let source = document.line_source_range(line);
            let logical_end = if line + 1 < document.line_count {
                document.line_source_range(line + 1).start
            } else {
                document.source.len()
            };
            if logical_end > source.end && active.start <= source.end && active.end > source.end {
                selected.push_str(
                    document
                        .source
                        .get(source.end..active.end.min(logical_end))
                        .expect("line ending is valid UTF-8"),
                );
            }
        }
        (!selected.is_empty()).then_some(selected)
    }

    fn selected_line_range(&self, line: usize) -> Option<(Range<usize>, bool)> {
        let active = self.active.as_ref()?;
        if active.start >= active.end {
            return None;
        }
        let document = self.document.as_ref()?;
        let content = document.line_rendered_range(line);
        let logical_end = if line + 1 < document.line_count {
            document.line_source_range(line + 1).start
        } else {
            document.source.len()
        };
        if active.end <= content.start || active.start >= logical_end {
            return None;
        }
        let start = active.start.max(content.start).min(content.end);
        let end = active.end.min(content.end).max(content.start);
        let newline_selected = logical_end > document.line_source_range(line).end
            && active.start <= content.end
            && active.end >= logical_end;
        if start >= end && !newline_selected {
            return None;
        }
        Some((start - content.start..end - content.start, newline_selected))
    }
}

impl CodeDocument {
    fn word_range_at(&self, line: usize, offset: usize) -> Range<usize> {
        let source = self.line_source_range(line);
        let local = offset.saturating_sub(source.start).min(source.len());
        let range = surrounding_word_range(self.line_text(line), local);
        source.start + range.start..source.start + range.end
    }
}

fn surrounding_word_range(text: &str, index: usize) -> Range<usize> {
    if text.is_empty() {
        return 0..0;
    }
    let mut index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    if index == text.len() {
        index = text[..index]
            .char_indices()
            .next_back()
            .map_or(0, |(offset, _)| offset);
    }
    let Some(current) = text[index..].chars().next() else {
        return index..index;
    };
    let class = word_class(current);
    let mut start = index;
    for (offset, character) in text[..index].char_indices().rev() {
        if word_class(character) != class {
            break;
        }
        start = offset;
    }
    let mut end = index + current.len_utf8();
    for character in text[end..].chars() {
        if word_class(character) != class {
            break;
        }
        end += character.len_utf8();
    }
    start..end
}

fn word_class(character: char) -> u8 {
    if character.is_whitespace() {
        0
    } else if character.is_alphanumeric() || character == '_' {
        1
    } else {
        2
    }
}

struct CodeSelectionArea<E> {
    inner: E,
    document: Arc<CodeDocument>,
    selection: CodeSelection,
    scroll_handle: UniformListScrollHandle,
    view_state: CodeViewState,
}

impl<E> CodeSelectionArea<E> {
    fn new(
        inner: E,
        document: Arc<CodeDocument>,
        selection: CodeSelection,
        scroll_handle: UniformListScrollHandle,
        view_state: CodeViewState,
    ) -> Self {
        Self {
            inner,
            document,
            selection,
            scroll_handle,
            view_state,
        }
    }
}

impl<E> Element for CodeSelectionArea<E>
where
    E: Element + IntoElement<Element = E>,
{
    type RequestLayoutState = E::RequestLayoutState;
    type PrepaintState = E::PrepaintState;

    fn id(&self) -> Option<ElementId> {
        self.inner.id()
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        self.inner.source_location()
    }

    fn request_layout(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        self.inner.request_layout(id, inspector_id, window, cx)
    }

    fn prepaint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        request: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        self.selection.begin_frame(self.document.clone(), bounds);
        window.set_focus_handle(&self.selection.focus_handle(), cx);
        let prepaint = self
            .inner
            .prepaint(id, inspector_id, bounds, request, window, cx);
        if reveal_pending_match(
            &self.document,
            &self.selection,
            &self.scroll_handle,
            &self.view_state,
            bounds,
        ) {
            cx.refresh_windows();
        }
        if self
            .selection
            .update_head_for_position(window.mouse_position())
        {
            cx.refresh_windows();
        }
        if self.selection.is_pending()
            && autoscroll_selection(
                &self.scroll_handle,
                bounds,
                window.mouse_position(),
                self.selection.active_line_height().unwrap_or(px(16.0)),
            )
        {
            cx.refresh_windows();
        }
        prepaint
    }

    fn paint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        request: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let mut context = KeyContext::default();
        context.add("ChattCodeViewer");
        window.set_key_context(context);
        window.on_action(std::any::TypeId::of::<Copy>(), {
            let selection = self.selection.clone();
            move |_, phase, _window, cx| {
                if phase == DispatchPhase::Bubble
                    && let Some(text) = selection.selected_text()
                {
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                    cx.stop_propagation();
                }
            }
        });
        window.on_action(std::any::TypeId::of::<SelectAll>(), {
            let selection = self.selection.clone();
            move |_, phase, _window, cx| {
                if phase == DispatchPhase::Bubble && selection.select_all() {
                    cx.stop_propagation();
                    cx.refresh_windows();
                }
            }
        });
        window.on_mouse_event({
            let selection = self.selection.clone();
            move |event: &MouseDownEvent, phase, _window, cx| {
                if phase.capture()
                    && event.button == MouseButton::Left
                    && !selection.contains_position(event.position)
                    && selection.is_active()
                {
                    selection.clear();
                    cx.refresh_windows();
                }
            }
        });
        window.on_mouse_event({
            let selection = self.selection.clone();
            move |event: &MouseMoveEvent, phase, _window, cx| {
                if phase.bubble() && selection.update_head_for_position(event.position) {
                    cx.refresh_windows();
                }
            }
        });
        window.on_mouse_event({
            let selection = self.selection.clone();
            move |event: &MouseUpEvent, phase, _window, cx| {
                if phase.capture()
                    && event.button == MouseButton::Left
                    && let Some(text) = selection.finish_selection()
                {
                    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
                    cx.write_to_primary(ClipboardItem::new_string(text));
                    cx.refresh_windows();
                }
            }
        });
        self.inner
            .paint(id, inspector_id, bounds, request, prepaint, window, cx);
    }
}

fn reveal_pending_match(
    document: &CodeDocument,
    selection: &CodeSelection,
    scroll_handle: &UniformListScrollHandle,
    view_state: &CodeViewState,
    viewport: Bounds<Pixels>,
) -> bool {
    let Some(search_match) = view_state.0.pending_reveal.get() else {
        return false;
    };
    let target = document.match_target(search_match);
    let participant = {
        let state = selection.0.borrow();
        state
            .current
            .iter()
            .find(|participant| participant.line == target.line)
            .cloned()
    };
    let Some(participant) = participant else {
        return false;
    };
    view_state.0.pending_reveal.set(None);

    let (start_x, end_x) = if target.hidden {
        participant
            .marker_x
            .as_ref()
            .map(|range| (range.start, range.end))
            .unwrap_or((participant.layout.width, participant.layout.width))
    } else {
        (
            participant.layout.x_for_index(target.start),
            participant.layout.x_for_index(target.end),
        )
    };
    let absolute_start = participant.text_bounds.left() + start_x.min(end_x);
    let absolute_end = participant.text_bounds.left() + start_x.max(end_x);
    let margin = px(16.0).min(viewport.size.width / 4.0);
    let visible_left = viewport.left() + margin;
    let visible_right = viewport.right() - margin;

    let base_handle = scroll_handle.0.borrow().base_handle.clone();
    let offset = base_handle.offset();
    let max_offset = base_handle.max_offset();
    let next_x = horizontal_offset_to_reveal(
        visible_left..visible_right,
        absolute_start..absolute_end,
        offset.x,
        max_offset.x,
    );
    if next_x == offset.x {
        return false;
    }
    base_handle.set_offset(point(next_x, offset.y));
    true
}

fn horizontal_offset_to_reveal(
    viewport: Range<Pixels>,
    target: Range<Pixels>,
    current_offset: Pixels,
    max_offset: Pixels,
) -> Pixels {
    let mut next = current_offset;
    if target.start < viewport.start {
        next += viewport.start - target.start;
    } else if target.end > viewport.end {
        next -= target.end - viewport.end;
    }
    next.max(-max_offset).min(Pixels::ZERO)
}

impl<E> IntoElement for CodeSelectionArea<E>
where
    E: Element + IntoElement<Element = E>,
{
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

fn autoscroll_selection(
    handle: &UniformListScrollHandle,
    bounds: Bounds<Pixels>,
    pointer: Point<Pixels>,
    line_height: Pixels,
) -> bool {
    let margin = line_height.min(bounds.size.width.min(bounds.size.height) / 3.0);
    let dx = selection_autoscroll_axis(
        pointer.x,
        bounds.left(),
        bounds.right(),
        margin,
        line_height,
    );
    let dy = selection_autoscroll_axis(
        pointer.y,
        bounds.top(),
        bounds.bottom(),
        margin,
        line_height,
    );
    if dx == Pixels::ZERO && dy == Pixels::ZERO {
        return false;
    }
    let base_handle = handle.0.borrow().base_handle.clone();
    let offset = base_handle.offset();
    let max_offset = base_handle.max_offset();
    let next = point(
        (offset.x - dx).max(-max_offset.x).min(Pixels::ZERO),
        (offset.y - dy).max(-max_offset.y).min(Pixels::ZERO),
    );
    if next == offset {
        return false;
    }
    base_handle.set_offset(next);
    true
}

fn selection_autoscroll_axis(
    position: Pixels,
    start: Pixels,
    end: Pixels,
    margin: Pixels,
    line_height: Pixels,
) -> Pixels {
    if position < start + margin {
        -selection_autoscroll_delta(start + margin - position, line_height)
    } else if position > end - margin {
        selection_autoscroll_delta(position - (end - margin), line_height)
    } else {
        Pixels::ZERO
    }
}

fn selection_autoscroll_delta(distance: Pixels, line_height: Pixels) -> Pixels {
    let lines = f32::from((distance.pow(1.2) / 100.0).min(px(3.0)));
    line_height * lines
}

pub fn render_code_document(
    document: Arc<CodeDocument>,
    scroll_handle: UniformListScrollHandle,
    view_state: CodeViewState,
    scrollbar_state: OverlayScrollbarState,
    selection: CodeSelection,
    active_match: Option<CodeMatchTarget>,
    settings: Option<Arc<ResolvedSettings>>,
) -> impl IntoElement {
    let line_count = document.line_count();
    let widest_line = view_state.widest_line(&document);
    let digits = line_count.max(1).ilog10() as f32 + 1.0;
    let gutter_width = px(18.0 + digits * 8.5);
    let list_document = document.clone();
    let list_view_state = view_state.clone();
    let list_selection = selection.clone();
    let list_settings = settings.clone();
    let list = uniform_list(
        ("code-viewer-lines", document.cache_key as usize),
        line_count,
        move |range, window, _| {
            let settings = list_settings.clone();
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
                        .when(
                            active_match.is_some_and(|target| target.line == line),
                            |row| {
                                row.bg(settings
                                    .as_ref()
                                    .map(|settings| settings.theme.color(ThemeRole::Raised))
                                    .unwrap_or_else(|| rgb(0x242832)))
                            },
                        )
                        .child(
                            div()
                                .w(gutter_width)
                                .flex_none()
                                .pr_2()
                                .text_right()
                                .text_color(
                                    settings
                                        .as_ref()
                                        .map(|settings| settings.theme.color(ThemeRole::TextDim))
                                        .unwrap_or_else(|| rgb(0x6f7680)),
                                )
                                .child(number),
                        )
                        .child(CodeLineElement {
                            document: list_document.clone(),
                            line,
                            view_state: list_view_state.clone(),
                            selection: list_selection.clone(),
                            active_match,
                            settings: settings.clone(),
                        })
                })
                .collect::<Vec<_>>()
        },
    )
    .track_scroll(&scroll_handle)
    .with_width_from_item(Some(widest_line))
    .with_sizing_behavior(ListSizingBehavior::Auto)
    .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
    .size_full();
    let contents = div()
        .relative()
        .size_full()
        .font_family(
            settings
                .as_ref()
                .map(|settings| settings.fonts.code_family.clone())
                .unwrap_or_else(|| CODE_FONT_FAMILY.into()),
        )
        .text_size(px(settings
            .as_ref()
            .map_or(14.0, |settings| settings.fonts.code_size)))
        .text_color(
            settings
                .as_ref()
                .map(|settings| settings.theme.color(ThemeRole::SyntaxForeground))
                .unwrap_or_else(|| {
                    rgb(syntax_color(
                        chatt_message_format::highlight::PaletteRole::Foreground,
                    ))
                }),
        )
        .child(list)
        .child(OverlayScrollbars::new(
            "code-viewer-scrollbars",
            scroll_handle.clone(),
            scrollbar_state,
            settings
                .as_ref()
                .map(|settings| OverlayScrollbarColors::from_settings(settings))
                .unwrap_or_default(),
        ));
    CodeSelectionArea::new(contents, document, selection, scroll_handle, view_state)
}

struct CodeLineElement {
    document: Arc<CodeDocument>,
    line: usize,
    view_state: CodeViewState,
    selection: CodeSelection,
    active_match: Option<CodeMatchTarget>,
    settings: Option<Arc<ResolvedSettings>>,
}

struct CodeLineLayout {
    text: Arc<LineLayout>,
    marker: Option<Arc<LineLayout>>,
    marker_width: Pixels,
}

impl CodeLineLayout {
    fn width(&self) -> Pixels {
        self.text.width + self.marker_width
    }
}

impl gpui::IntoElement for CodeLineElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for CodeLineElement {
    type RequestLayoutState = CodeLineLayout;
    type PrepaintState = Hitbox;

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
        let document = self.document.clone();
        let line = self.line;
        let text_layout = window.text_system().layout_line_by_hash_with_font_runs(
            self.document.content_hash(self.line),
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
        let active_hidden = self
            .active_match
            .is_some_and(|target| target.line == self.line && target.hidden);
        let (marker, marker_width) = if self.document.line_is_truncated(self.line) {
            let hidden_marker =
                layout_marker(CODE_HIDDEN_MATCH_MARKER, normal_id, font_size, window);
            let marker = if active_hidden {
                hidden_marker.clone()
            } else {
                layout_marker(CODE_TRUNCATION_MARKER, normal_id, font_size, window)
            };
            (Some(marker), hidden_marker.width)
        } else {
            (None, Pixels::ZERO)
        };
        let layout = CodeLineLayout {
            text: text_layout,
            marker,
            marker_width,
        };
        if self.view_state.record_width(self.line, layout.width()) {
            cx.refresh_windows();
        }
        let mut style = Style::default();
        style.size.width = layout.width().into();
        style.size.height = window.line_height().into();
        (window.request_layout(style, [], cx), layout)
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        _: &mut App,
    ) -> Self::PrepaintState {
        let right = self
            .selection
            .viewport()
            .map_or(bounds.right(), |viewport| {
                viewport.right().max(bounds.right())
            });
        let selectable_bounds = Bounds::from_corners(bounds.origin, point(right, bounds.bottom()));
        let hitbox = window.insert_hitbox(selectable_bounds, HitboxBehavior::Normal);
        let marker_x = layout
            .marker
            .as_ref()
            .map(|marker| layout.text.width..layout.text.width + marker.width);
        self.selection.register(
            self.line,
            selectable_bounds,
            bounds,
            layout.text.clone(),
            marker_x,
        );
        hitbox
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: gpui::Bounds<Pixels>,
        layout: &mut Self::RequestLayoutState,
        hitbox: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        window.set_cursor_style(CursorStyle::IBeam, hitbox);
        window.on_mouse_event({
            let hitbox = hitbox.clone();
            let selection = self.selection.clone();
            let line = self.line;
            let line_start = self.document.line_source_range(line).start;
            let text_layout = layout.text.clone();
            move |event: &MouseDownEvent, phase, window, cx| {
                if !phase.bubble()
                    || event.button != MouseButton::Left
                    || !hitbox.is_hovered(window)
                {
                    return;
                }
                let offset =
                    line_start + text_layout.closest_index_for_x(event.position.x - bounds.left());
                selection.begin_selection(line, offset, event.click_count, event.modifiers.shift);
                window.focus(&selection.focus_handle(), cx);
                window.prevent_default();
                cx.refresh_windows();
            }
        });
        if let Some(target) = self
            .active_match
            .filter(|target| target.line == self.line && !target.hidden)
        {
            let start_x = layout.text.x_for_index(target.start.min(layout.text.len));
            let end_x = layout.text.x_for_index(target.end.min(layout.text.len));
            let left_x = start_x.min(end_x);
            let right_x = start_x.max(end_x);
            window.paint_quad(quad(
                Bounds::from_corners(
                    point(bounds.left() + left_x, bounds.top()),
                    point(
                        bounds.left() + right_x.max(left_x + px(1.0)),
                        bounds.bottom(),
                    ),
                ),
                px(2.0),
                self.settings
                    .as_ref()
                    .map(|settings| settings.theme.color(ThemeRole::StateCurrentSearch))
                    .unwrap_or_else(|| rgba(0xd9a44166)),
                Edges::default(),
                Hsla::transparent_black(),
                BorderStyle::default(),
            ));
        }
        let line = self.line;
        let document = self.document.clone();
        let decorations = document.line_spans(line).map(|span| DecorationRun {
            len: (span.end - span.start) as u32,
            color: self
                .settings
                .as_ref()
                .map(|settings| settings.theme.color(syntax_role(span.class.palette_role())))
                .unwrap_or_else(|| rgb(syntax_color(span.class.palette_role())))
                .into(),
            background_color: None,
            underline: None,
            strikethrough: None,
        });
        layout
            .text
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
        if let Some(marker) = &layout.marker {
            let active_hidden = self
                .active_match
                .is_some_and(|target| target.line == self.line && target.hidden);
            if active_hidden {
                window.paint_quad(quad(
                    Bounds::new(
                        point(bounds.left() + layout.text.width, bounds.top()),
                        gpui::size(marker.width, bounds.size.height),
                    ),
                    px(2.0),
                    self.settings
                        .as_ref()
                        .map(|settings| settings.theme.color(ThemeRole::StateSearch))
                        .unwrap_or_else(|| rgba(0xd9a44144)),
                    Edges::default(),
                    Hsla::transparent_black(),
                    BorderStyle::default(),
                ));
            }
            let marker_color = if active_hidden {
                self.settings
                    .as_ref()
                    .map(|settings| settings.theme.color(ThemeRole::StateWarning))
                    .unwrap_or_else(|| rgb(0xd9a441))
            } else {
                self.settings
                    .as_ref()
                    .map(|settings| settings.theme.color(ThemeRole::TextDim))
                    .unwrap_or_else(|| rgb(0x6f7680))
            };
            marker
                .paint_with_decorations(
                    point(bounds.left() + layout.text.width, bounds.top()),
                    window.line_height(),
                    TextAlign::Left,
                    None,
                    [DecorationRun {
                        len: marker.len as u32,
                        color: marker_color.into(),
                        background_color: None,
                        underline: None,
                        strikethrough: None,
                    }],
                    window,
                    cx,
                )
                .expect("code truncation marker painting failed");
        }
        if let Some((range, newline_selected)) = self.selection.selected_line_range(line) {
            let start_x = layout.text.x_for_index(range.start);
            let mut end_x = layout.text.x_for_index(range.end);
            if newline_selected {
                end_x = end_x.max(layout.text.width + px(4.0));
            } else {
                end_x = end_x.max(start_x + px(1.0));
            }
            window.paint_quad(quad(
                Bounds::from_corners(
                    point(bounds.left() + start_x, bounds.top()),
                    point(bounds.left() + end_x, bounds.bottom()),
                ),
                Pixels::ZERO,
                self.settings
                    .as_ref()
                    .map(|settings| settings.theme.color(ThemeRole::StateSelection))
                    .unwrap_or_else(|| rgba(0xffffff1f)),
                Edges::default(),
                Hsla::transparent_black(),
                BorderStyle::default(),
            ));
        }
    }
}

fn marker_content_hash(marker: &str) -> u64 {
    let mut hash = DefaultHasher::new();
    "chatt-code-truncation-marker".hash(&mut hash);
    marker.hash(&mut hash);
    hash.finish()
}

fn layout_marker(
    marker: &'static str,
    font_id: gpui::FontId,
    font_size: Pixels,
    window: &mut Window,
) -> Arc<LineLayout> {
    window.text_system().layout_line_by_hash_with_font_runs(
        marker_content_hash(marker),
        marker,
        font_size,
        None,
        |push| {
            push(FontRun {
                len: marker.len(),
                font_id,
            });
        },
    )
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
    use super::*;

    struct TestCodeView {
        document: Arc<CodeDocument>,
        scroll_handle: UniformListScrollHandle,
        view_state: CodeViewState,
        scrollbar_state: OverlayScrollbarState,
        selection: CodeSelection,
        active_match: Option<CodeMatchTarget>,
        focus: FocusHandle,
    }

    impl gpui::Render for TestCodeView {
        fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .track_focus(&self.focus)
                .child(div().w(px(240.0)).h(px(120.0)).child(render_code_document(
                    self.document.clone(),
                    self.scroll_handle.clone(),
                    self.view_state.clone(),
                    self.scrollbar_state.clone(),
                    self.selection.clone(),
                    self.active_match,
                    None,
                )))
        }
    }

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
            let text_range = document.line(line).1;
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
                .map(|index| document.match_target(alpha.get(index).unwrap()).line)
                .collect::<Vec<_>>(),
            [0, 0, 2]
        );
        assert_eq!(
            alpha
                .matches
                .iter()
                .map(|search_match| {
                    &document.source()[search_match.start as usize..search_match.end as usize]
                })
                .collect::<Vec<_>>(),
            ["Alpha", "alpha", "ALPHA"]
        );
        let street = document.search("straße");
        assert_eq!(street.len(), 1);
        assert_eq!(document.match_target(street.get(0).unwrap()).line, 1);
        assert!(document.search("").is_empty());
        assert!(document.search("ha\nst").is_empty());

        let overlapping = CodeDocument::prepare("aaa".to_string(), "notes.txt", 2).search("aa");
        assert_eq!(overlapping.len(), 1);
        assert_eq!(
            overlapping.get(0),
            Some(CodeSearchMatch { start: 0, end: 2 })
        );

        let expanded = CodeDocument::prepare("İ".to_string(), "notes.txt", 3);
        let dotted_i = expanded.search("i");
        assert_eq!(dotted_i.get(0), Some(CodeSearchMatch { start: 0, end: 2 }));
    }

    #[test]
    fn shaped_width_tracker_only_ratchets_to_wider_lines() {
        let document = CodeDocument::prepare("x\nwidest".to_string(), "notes.txt", 1);
        let tracker = CodeViewState::default();
        assert_eq!(tracker.widest_line(&document), 1);
        assert!(tracker.record_width(0, px(80.0)));
        assert!(!tracker.record_width(1, px(60.0)));
        assert!(!tracker.record_width(2, px(80.0)));
        assert_eq!(tracker.widest_line(&document), 0);
        assert!(tracker.record_width(3, px(81.0)));
        assert_eq!(tracker.widest_line(&document), 3);

        tracker.reset();
        assert_eq!(tracker.widest_line(&document), 1);
        assert!(tracker.record_width(1, px(1.0)));
        assert_eq!(tracker.widest_line(&document), 1);
    }

    #[test]
    fn horizontal_match_reveal_scrolls_minimally_and_clamps() {
        assert_eq!(
            horizontal_offset_to_reveal(
                px(10.0)..px(90.0),
                px(30.0)..px(60.0),
                px(-40.0),
                px(200.0)
            ),
            px(-40.0)
        );
        assert_eq!(
            horizontal_offset_to_reveal(
                px(10.0)..px(90.0),
                px(0.0)..px(20.0),
                px(-40.0),
                px(200.0)
            ),
            px(-30.0)
        );
        assert_eq!(
            horizontal_offset_to_reveal(
                px(10.0)..px(90.0),
                px(80.0)..px(120.0),
                px(-40.0),
                px(200.0)
            ),
            px(-70.0)
        );
        assert_eq!(
            horizontal_offset_to_reveal(
                px(10.0)..px(90.0),
                px(250.0)..px(280.0),
                Pixels::ZERO,
                px(120.0)
            ),
            px(-120.0)
        );
    }

    #[test]
    fn line_selection_ranges_include_existing_line_endings() {
        let document = CodeDocument::prepare("first\r\nsecond\nthird".to_string(), "notes.txt", 1);
        assert_eq!(document.line_selection_range(0), 0..7);
        assert_eq!(
            &document.source()[document.line_selection_range(0)],
            "first\r\n"
        );
        assert_eq!(
            &document.source()[document.line_selection_range(1)],
            "second\n"
        );
        assert_eq!(
            &document.source()[document.line_selection_range(2)],
            "third"
        );
    }

    #[test]
    fn word_ranges_respect_utf8_boundaries_and_identifier_characters() {
        let document = CodeDocument::prepare("let café_value = 1;".to_string(), "notes.txt", 1);
        let cafe = document.word_range_at(0, "let caf".len());
        assert_eq!(&document.source()[cafe], "café_value");
        let punctuation = document.word_range_at(0, document.source().find('=').unwrap());
        assert_eq!(&document.source()[punctuation], "=");
    }

    #[test]
    fn display_columns_handles_ascii_and_tabs_before_unicode_fallback() {
        assert_eq!(display_columns("ascii"), 5);
        assert_eq!(display_columns("\tx"), CODE_TAB_WIDTH + 1);
        assert_eq!(display_columns("界x"), 3);
    }

    #[gpui::test]
    fn selection_copies_exact_source_for_forward_reverse_and_line_modes(
        cx: &mut gpui::TestAppContext,
    ) {
        let document = Arc::new(CodeDocument::prepare(
            "aé\nbeta gamma\nlast".to_string(),
            "notes.txt",
            1,
        ));
        let selection = CodeSelection::new(cx.update(|cx| cx.focus_handle()));
        selection.begin_frame(document.clone(), Bounds::default());

        let beta_end = document.line_source_range(1).start + "beta".len();
        selection.begin_selection(0, 1, 1, false);
        assert!(selection.update_head(1, beta_end));
        assert_eq!(selection.finish_selection().as_deref(), Some("é\nbeta"));

        selection.begin_selection(1, beta_end, 1, false);
        assert!(selection.update_head(0, 1));
        assert_eq!(selection.finish_selection().as_deref(), Some("é\nbeta"));

        selection.begin_selection(1, document.line_source_range(1).start + 6, 2, false);
        assert_eq!(selection.finish_selection().as_deref(), Some("gamma"));

        selection.begin_selection(0, 1, 3, false);
        assert_eq!(selection.finish_selection().as_deref(), Some("aé\n"));
    }

    #[gpui::test]
    fn shift_extend_select_all_and_document_change_follow_viewer_lifetime(
        cx: &mut gpui::TestAppContext,
    ) {
        let first = Arc::new(CodeDocument::prepare(
            "alpha\nbeta".to_string(),
            "notes.txt",
            1,
        ));
        let selection = CodeSelection::new(cx.update(|cx| cx.focus_handle()));
        selection.begin_frame(first.clone(), Bounds::default());
        selection.begin_selection(0, 1, 1, false);
        assert_eq!(selection.finish_selection(), None);

        let beta_end = first.source().len();
        selection.begin_selection(1, beta_end, 1, true);
        assert_eq!(selection.finish_selection().as_deref(), Some("lpha\nbeta"));

        assert!(selection.select_all());
        assert_eq!(selection.selected_text().as_deref(), Some("alpha\nbeta"));

        let second = Arc::new(CodeDocument::prepare(
            "replacement".to_string(),
            "notes.txt",
            2,
        ));
        selection.begin_frame(second, Bounds::default());
        assert_eq!(selection.selected_text(), None);
    }

    #[gpui::test]
    fn truncated_mouse_selection_copies_only_visible_segments(cx: &mut gpui::TestAppContext) {
        let prefix = "x".repeat(MAX_CODE_RENDERED_LINE_BYTES);
        let source = format!("{prefix}hidden\nnext");
        let document = Arc::new(CodeDocument::prepare(source.clone(), "notes.txt", 1));
        let selection = CodeSelection::new(cx.update(|cx| cx.focus_handle()));
        selection.begin_frame(document.clone(), Bounds::default());

        selection.begin_selection(0, 0, 1, false);
        let next_end = document.line_source_range(1).start + "next".len();
        assert!(selection.update_head(1, next_end));
        assert_eq!(
            selection.finish_selection().as_deref(),
            Some(format!("{prefix}\nnext").as_str())
        );

        selection.begin_selection(0, 10, 3, false);
        assert_eq!(
            selection.finish_selection().as_deref(),
            Some(prefix.as_str())
        );

        assert!(selection.select_all());
        assert_eq!(selection.selected_text().as_deref(), Some(source.as_str()));
    }

    #[gpui::test]
    fn rendered_viewer_ratchets_to_shaped_width_and_handles_copy_actions(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(crate::fonts::init);
        let document = Arc::new(CodeDocument::prepare(
            format!("short\n{}", "wide text ".repeat(32)),
            "notes.txt",
            1,
        ));
        let drawn_document = document.clone();
        let (view, cx) = cx.add_window_view(move |window, cx| {
            let focus = cx.focus_handle();
            window.focus(&focus, cx);
            TestCodeView {
                document: drawn_document,
                scroll_handle: UniformListScrollHandle::new(),
                view_state: CodeViewState::default(),
                scrollbar_state: OverlayScrollbarState::default(),
                selection: CodeSelection::new(focus.clone()),
                active_match: None,
                focus,
            }
        });
        let first_layout = view.read_with(cx, |view, _| {
            assert_eq!(view.view_state.widest_line(&view.document), 1);
            view.selection
                .0
                .borrow()
                .current
                .iter()
                .find(|participant| participant.line == 0)
                .expect("first line is visible")
                .layout
                .clone()
        });
        view.update(cx, |_, cx| cx.notify());
        view.read_with(cx, |view, _| {
            let current_layout = view
                .selection
                .0
                .borrow()
                .current
                .iter()
                .find(|participant| participant.line == 0)
                .expect("first line remains visible")
                .layout
                .clone();
            assert!(Arc::ptr_eq(&first_layout, &current_layout));
            let item_size = view
                .scroll_handle
                .0
                .borrow()
                .last_item_size
                .expect("rendered uniform list records its content size");
            assert!(item_size.contents.width > item_size.item.width);
        });

        cx.dispatch_action(SelectAll);
        cx.dispatch_action(Copy);
        assert_eq!(
            cx.read_from_clipboard()
                .and_then(|item| item.text())
                .as_deref(),
            Some(document.source())
        );
    }

    #[gpui::test]
    fn hidden_search_match_reveals_the_truncation_marker(cx: &mut gpui::TestAppContext) {
        cx.update(crate::fonts::init);
        let document = Arc::new(CodeDocument::prepare(
            format!("{} hidden-needle", "x".repeat(MAX_CODE_RENDERED_LINE_BYTES)),
            "notes.txt",
            1,
        ));
        let search_match = document
            .search("hidden-needle")
            .get(0)
            .expect("hidden search match");
        let target = document.match_target(search_match);
        assert!(target.hidden);
        let view_state = CodeViewState::default();
        view_state.request_match_reveal(search_match);
        let drawn_document = document.clone();
        let drawn_view_state = view_state.clone();
        let (view, cx) = cx.add_window_view(move |window, cx| {
            let focus = cx.focus_handle();
            window.focus(&focus, cx);
            TestCodeView {
                document: drawn_document,
                scroll_handle: UniformListScrollHandle::new(),
                view_state: drawn_view_state,
                scrollbar_state: OverlayScrollbarState::default(),
                selection: CodeSelection::new(focus.clone()),
                active_match: Some(target),
                focus,
            }
        });

        view.read_with(cx, |view, _| {
            let base_handle = view.scroll_handle.0.borrow().base_handle.clone();
            assert!(base_handle.offset().x < Pixels::ZERO);
            assert!(view.view_state.0.pending_reveal.get().is_none());
        });
    }

    #[gpui::test]
    fn rendered_viewer_drag_selects_across_virtualized_rows(cx: &mut gpui::TestAppContext) {
        cx.update(crate::fonts::init);
        let document = Arc::new(CodeDocument::prepare(
            "alpha\nbeta gamma".to_string(),
            "notes.txt",
            1,
        ));
        let drawn_document = document.clone();
        let (view, cx) = cx.add_window_view(move |window, cx| {
            let focus = cx.focus_handle();
            window.focus(&focus, cx);
            TestCodeView {
                document: drawn_document,
                scroll_handle: UniformListScrollHandle::new(),
                view_state: CodeViewState::default(),
                scrollbar_state: OverlayScrollbarState::default(),
                selection: CodeSelection::new(focus.clone()),
                active_match: None,
                focus,
            }
        });
        let (start, end) = view.read_with(cx, |view, _| {
            let state = view.selection.0.borrow();
            let first = state
                .current
                .iter()
                .find(|participant| participant.line == 0)
                .expect("first line is visible");
            let second = state
                .current
                .iter()
                .find(|participant| participant.line == 1)
                .expect("second line is visible");
            (
                point(
                    first.text_bounds.left() + first.layout.x_for_index(1),
                    first.bounds.center().y,
                ),
                point(
                    second.text_bounds.left() + second.layout.x_for_index(4),
                    second.bounds.center().y,
                ),
            )
        });

        cx.simulate_mouse_down(start, MouseButton::Left, gpui::Modifiers::default());
        cx.simulate_mouse_move(end, MouseButton::Left, gpui::Modifiers::default());
        cx.simulate_mouse_up(end, MouseButton::Left, gpui::Modifiers::default());

        view.read_with(cx, |view, _| {
            assert_eq!(
                view.selection.selected_text().as_deref(),
                Some("lpha\nbeta")
            );
        });
    }

    #[gpui::test]
    fn rendered_scrollbar_drag_reaches_the_end_of_the_code_list(cx: &mut gpui::TestAppContext) {
        cx.update(crate::fonts::init);
        let document = Arc::new(CodeDocument::prepare(
            (0..100)
                .map(|line| format!("line {line}"))
                .collect::<Vec<_>>()
                .join("\n"),
            "notes.txt",
            1,
        ));
        let drawn_document = document.clone();
        let (view, cx) = cx.add_window_view(move |window, cx| {
            let focus = cx.focus_handle();
            window.focus(&focus, cx);
            TestCodeView {
                document: drawn_document,
                scroll_handle: UniformListScrollHandle::new(),
                view_state: CodeViewState::default(),
                scrollbar_state: OverlayScrollbarState::default(),
                selection: CodeSelection::new(focus.clone()),
                active_match: None,
                focus,
            }
        });
        let (start, end, maximum) = view.read_with(cx, |view, _| {
            let base_handle = view.scroll_handle.0.borrow().base_handle.clone();
            let maximum = base_handle.max_offset();
            let geometry = crate::scrollbar::scrollbar_geometries(
                base_handle.bounds(),
                maximum,
                base_handle.offset(),
            )
            .into_iter()
            .find(|geometry| geometry.axis == crate::scrollbar::ScrollbarAxis::Vertical)
            .expect("long code document has a vertical scrollbar");
            let start = geometry.thumb_bounds.center();
            let end = point(
                start.x,
                geometry.thumb_track_start
                    + geometry.thumb_travel
                    + geometry.thumb_bounds.size.height / 2.0,
            );
            (start, end, maximum.y)
        });

        cx.simulate_mouse_down(start, MouseButton::Left, gpui::Modifiers::default());
        cx.simulate_mouse_move(end, MouseButton::Left, gpui::Modifiers::default());
        cx.simulate_mouse_up(end, MouseButton::Left, gpui::Modifiers::default());

        view.read_with(cx, |view, _| {
            let state = view.scroll_handle.0.borrow();
            assert_eq!(state.base_handle.offset().y, -maximum);
            assert!(!view.scrollbar_state.is_dragging());
        });
    }

    #[test]
    fn bounded_load_rejects_oversized_and_non_utf8_files() {
        let oversized = vec![b'x'; MAX_CODE_PREVIEW_BYTES as usize + 1];
        assert_eq!(
            CodeDocument::load(&oversized, "oversized.rs", 1).unwrap_err(),
            "file too large to preview"
        );

        assert_eq!(
            CodeDocument::load(&[0xff, 0xfe, 0xfd], "binary.rs", 2).unwrap_err(),
            "not a text file"
        );
    }

    #[test]
    fn bounded_load_accepts_a_file_at_the_limit() {
        let contents = vec![b'x'; MAX_CODE_PREVIEW_BYTES as usize];

        let document = CodeDocument::load(&contents, "limit.txt", 1).expect("load text at limit");
        assert_eq!(document.source().len(), MAX_CODE_PREVIEW_BYTES as usize);
        assert_eq!(document.line_count(), 1);
        assert!(document.line_is_truncated(0));
        assert_eq!(document.line_text(0).len(), MAX_CODE_RENDERED_LINE_BYTES);
    }

    #[test]
    fn bounded_load_truncates_pathological_lines_and_rejects_excessive_line_counts() {
        let long_line_source = vec![b'x'; MAX_CODE_RENDERED_LINE_BYTES * 2];
        let document =
            CodeDocument::load(&long_line_source, "long-line.txt", 1).expect("load truncated line");
        assert_eq!(document.source().as_bytes(), long_line_source);
        assert_eq!(document.line_text(0).len(), MAX_CODE_RENDERED_LINE_BYTES);
        assert!(document.line_is_truncated(0));

        let many_lines = vec![b'\n'; MAX_CODE_PREVIEW_LINES + 1];
        assert_eq!(
            CodeDocument::load(&many_lines, "many-lines.txt", 2).unwrap_err(),
            "too many lines to preview"
        );
    }

    #[test]
    fn rendered_prefix_stops_at_a_utf8_boundary_and_keeps_full_search_results() {
        let source = format!(
            "{}é hidden-needle",
            "x".repeat(MAX_CODE_RENDERED_LINE_BYTES - 1)
        );
        let document = CodeDocument::prepare(source.clone(), "notes.txt", 1);
        assert_eq!(document.source(), source);
        assert_eq!(
            document.line_text(0).len(),
            MAX_CODE_RENDERED_LINE_BYTES - 1
        );
        assert!(document.line_is_truncated(0));

        let results = document.search("hidden-needle");
        let target = document.match_target(results.get(0).expect("hidden match"));
        assert_eq!(target.line, 0);
        assert!(target.hidden);
    }
}
