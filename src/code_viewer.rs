use std::{
    cell::{Cell, RefCell},
    fs::File,
    hash::{DefaultHasher, Hash, Hasher},
    io::Read,
    ops::Range,
    path::Path,
    rc::Rc,
    sync::Arc,
};

use chatt_message_format::highlight::{self, HlClass};
use gpui::{
    App, BorderStyle, Bounds, ClipboardItem, Corners, CursorStyle, DecorationRun, DispatchPhase,
    Edges, Element, ElementId, FocusHandle, FontId, FontRun, FontStyle, GlobalElementId, Hitbox,
    HitboxBehavior, Hsla, KeyBinding, KeyContext, LayoutId, LineLayout,
    ListHorizontalSizingBehavior, ListSizingBehavior, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, Pixels, Point, Position, SharedString, Style, TextAlign, UniformListScrollHandle,
    Window, actions, div, point, prelude::*, px, quad, relative, rgb, rgba, size, uniform_list,
};

use crate::{fonts::CODE_FONT_FAMILY, formatted_message::syntax_color};

pub const MAX_CODE_PREVIEW_BYTES: u64 = 2 * 1024 * 1024;
pub const MAX_CODE_PREVIEW_LINE_BYTES: usize = 32 * 1024;
pub const MAX_CODE_PREVIEW_LINES: usize = 200_000;

actions!(code_viewer, [Copy, SelectAll]);

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("cmd-c", Copy, Some("ChattCodeViewer && !ChattCodeSearch")),
        KeyBinding::new(
            "cmd-a",
            SelectAll,
            Some("ChattCodeViewer && !ChattCodeSearch"),
        ),
    ]);
}

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
    cache_key: u64,
}

#[derive(Clone, Copy, Debug)]
struct CodeWidthMeasurement {
    line: usize,
    width: Pixels,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CodeScrollbarAxis {
    Horizontal,
    Vertical,
}

#[derive(Clone, Copy, Debug)]
struct CodeScrollbarDrag {
    axis: CodeScrollbarAxis,
    pointer_offset: Pixels,
}

#[derive(Debug)]
struct CodeViewStateInner {
    width: Cell<CodeWidthMeasurement>,
    scrollbar_drag: Cell<Option<CodeScrollbarDrag>>,
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
            scrollbar_drag: Cell::new(None),
        }))
    }
}

impl CodeViewState {
    pub fn reset(&self) {
        self.0.width.set(CodeWidthMeasurement {
            line: 0,
            width: Pixels::ZERO,
        });
        self.0.scrollbar_drag.set(None);
    }

    fn widest_line(&self) -> usize {
        self.0.width.get().line
    }

    fn record_width(&self, line: usize, width: Pixels) -> bool {
        let measurement = self.0.width.get();
        if width <= measurement.width {
            return false;
        }
        self.0.width.set(CodeWidthMeasurement { line, width });
        true
    }
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
            cache_key,
        }
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn line_count(&self) -> usize {
        self.line_count
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

    fn line_source_range(&self, index: usize) -> Range<usize> {
        self.line(index).0
    }

    fn line_selection_range(&self, index: usize) -> Range<usize> {
        let source = self.line_source_range(index);
        let end = if index + 1 < self.line_count {
            self.line_source_range(index + 1).start
        } else {
            self.source.len()
        };
        source.start..end
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
    ) {
        self.0.borrow_mut().current.push(CodeSelectionParticipant {
            line,
            bounds,
            text_bounds,
            layout,
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
        self.document
            .as_ref()?
            .source
            .get(active.start..active.end)
            .map(ToOwned::to_owned)
    }

    fn selected_line_range(&self, line: usize) -> Option<(Range<usize>, bool)> {
        let active = self.active.as_ref()?;
        if active.start >= active.end {
            return None;
        }
        let document = self.document.as_ref()?;
        let content = document.line_source_range(line);
        let selectable = document.line_selection_range(line);
        if active.end <= selectable.start || active.start >= selectable.end {
            return None;
        }
        let start = active.start.max(content.start).min(content.end);
        let end = active.end.min(content.end).max(content.start);
        let newline_selected =
            selectable.end > content.end && active.start <= content.end && active.end > content.end;
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
}

impl<E> CodeSelectionArea<E> {
    fn new(
        inner: E,
        document: Arc<CodeDocument>,
        selection: CodeSelection,
        scroll_handle: UniformListScrollHandle,
    ) -> Self {
        Self {
            inner,
            document,
            selection,
            scroll_handle,
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

const CODE_SCROLLBAR_SIZE: Pixels = px(12.0);
const CODE_SCROLLBAR_PADDING: Pixels = px(2.0);
const CODE_SCROLLBAR_MIN_THUMB: Pixels = px(28.0);

#[derive(Clone, Copy, Debug)]
struct CodeScrollbarGeometry {
    axis: CodeScrollbarAxis,
    track_bounds: Bounds<Pixels>,
    thumb_bounds: Bounds<Pixels>,
    thumb_track_start: Pixels,
    thumb_travel: Pixels,
    max_offset: Pixels,
}

#[derive(Clone)]
struct CodeScrollbarLayout {
    geometry: CodeScrollbarGeometry,
    hitbox: Hitbox,
}

struct CodeScrollbarsElement {
    scroll_handle: UniformListScrollHandle,
    view_state: CodeViewState,
}

impl IntoElement for CodeScrollbarsElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for CodeScrollbarsElement {
    type RequestLayoutState = ();
    type PrepaintState = Vec<CodeScrollbarLayout>;

    fn id(&self) -> Option<ElementId> {
        Some("code-viewer-scrollbars".into())
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
        let style = Style {
            position: Position::Absolute,
            size: size(relative(1.0), relative(1.0)).map(Into::into),
            ..Default::default()
        };
        (window.request_layout(style, None, cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        window: &mut Window,
        _: &mut App,
    ) -> Self::PrepaintState {
        let base_handle = self.scroll_handle.0.borrow().base_handle.clone();
        code_scrollbar_geometries(bounds, base_handle.max_offset(), base_handle.offset())
            .into_iter()
            .map(|geometry| CodeScrollbarLayout {
                hitbox: window.insert_hitbox(
                    geometry.track_bounds,
                    HitboxBehavior::BlockMouseExceptScroll,
                ),
                geometry,
            })
            .collect()
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        layouts: &mut Self::PrepaintState,
        window: &mut Window,
        _cx: &mut App,
    ) {
        let active_drag = self.view_state.0.scrollbar_drag.get();
        for layout in layouts.iter() {
            let hovered = layout.hitbox.is_hovered(window)
                || active_drag.is_some_and(|drag| drag.axis == layout.geometry.axis);
            window.paint_quad(quad(
                layout.geometry.track_bounds,
                Pixels::ZERO,
                rgba(0x111317dd),
                Edges::default(),
                Hsla::transparent_black(),
                BorderStyle::default(),
            ));
            window.paint_quad(quad(
                layout.geometry.thumb_bounds,
                Corners::all(px(3.0)),
                rgba(if hovered { 0x8b929ddd } else { 0x5f6670cc }),
                Edges::default(),
                Hsla::transparent_black(),
                BorderStyle::default(),
            ));
            window.set_cursor_style(CursorStyle::Arrow, &layout.hitbox);
        }
        if active_drag.is_some() {
            window.set_window_cursor_style(CursorStyle::Arrow);
        }

        let capture_phase = if active_drag.is_some() {
            DispatchPhase::Capture
        } else {
            DispatchPhase::Bubble
        };
        window.on_mouse_event({
            let layouts = layouts.clone();
            let scroll_handle = self.scroll_handle.clone();
            let view_state = self.view_state.clone();
            move |event: &MouseDownEvent, phase, window, cx| {
                if phase != capture_phase || event.button != MouseButton::Left {
                    return;
                }
                let Some(layout) = layouts
                    .iter()
                    .find(|layout| layout.hitbox.is_hovered(window))
                else {
                    return;
                };
                let geometry = layout.geometry;
                if geometry.thumb_bounds.contains(&event.position) {
                    view_state.0.scrollbar_drag.set(Some(CodeScrollbarDrag {
                        axis: geometry.axis,
                        pointer_offset: scrollbar_axis_position(geometry.axis, event.position)
                            - scrollbar_axis_origin(geometry.axis, geometry.thumb_bounds),
                    }));
                    window.capture_pointer(layout.hitbox.id);
                } else {
                    let pointer_offset =
                        scrollbar_axis_size(geometry.axis, geometry.thumb_bounds.size) / 2.0;
                    set_code_scrollbar_offset(
                        &scroll_handle,
                        geometry.axis,
                        scrollbar_offset_for_position(geometry, event.position, pointer_offset),
                    );
                }
                window.prevent_default();
                cx.stop_propagation();
                cx.refresh_windows();
            }
        });
        window.on_mouse_event({
            let layouts = layouts.clone();
            let scroll_handle = self.scroll_handle.clone();
            let view_state = self.view_state.clone();
            move |event: &MouseMoveEvent, phase, _window, cx| {
                if phase != capture_phase {
                    return;
                }
                let Some(drag) = view_state.0.scrollbar_drag.get() else {
                    return;
                };
                if !event.dragging() {
                    view_state.0.scrollbar_drag.set(None);
                    cx.refresh_windows();
                    return;
                }
                let Some(layout) = layouts
                    .iter()
                    .find(|layout| layout.geometry.axis == drag.axis)
                else {
                    return;
                };
                set_code_scrollbar_offset(
                    &scroll_handle,
                    drag.axis,
                    scrollbar_offset_for_position(
                        layout.geometry,
                        event.position,
                        drag.pointer_offset,
                    ),
                );
                cx.stop_propagation();
                cx.refresh_windows();
            }
        });
        window.on_mouse_event({
            let view_state = self.view_state.clone();
            move |event: &MouseUpEvent, phase, _window, cx| {
                if phase == capture_phase
                    && event.button == MouseButton::Left
                    && view_state.0.scrollbar_drag.take().is_some()
                {
                    cx.stop_propagation();
                    cx.refresh_windows();
                }
            }
        });
    }
}

fn code_scrollbar_geometries(
    bounds: Bounds<Pixels>,
    max_offset: Point<Pixels>,
    offset: Point<Pixels>,
) -> Vec<CodeScrollbarGeometry> {
    let horizontal = max_offset.x > Pixels::ZERO;
    let vertical = max_offset.y > Pixels::ZERO;
    let corner = if horizontal && vertical {
        CODE_SCROLLBAR_SIZE
    } else {
        Pixels::ZERO
    };
    let mut geometries = Vec::with_capacity(2);
    if vertical
        && let Some(geometry) = code_scrollbar_geometry(
            CodeScrollbarAxis::Vertical,
            Bounds::new(
                point(bounds.right() - CODE_SCROLLBAR_SIZE, bounds.top()),
                size(
                    CODE_SCROLLBAR_SIZE,
                    (bounds.size.height - corner).max(Pixels::ZERO),
                ),
            ),
            bounds.size.height,
            max_offset.y,
            offset.y,
        )
    {
        geometries.push(geometry);
    }
    if horizontal
        && let Some(geometry) = code_scrollbar_geometry(
            CodeScrollbarAxis::Horizontal,
            Bounds::new(
                point(bounds.left(), bounds.bottom() - CODE_SCROLLBAR_SIZE),
                size(
                    (bounds.size.width - corner).max(Pixels::ZERO),
                    CODE_SCROLLBAR_SIZE,
                ),
            ),
            bounds.size.width,
            max_offset.x,
            offset.x,
        )
    {
        geometries.push(geometry);
    }
    geometries
}

fn code_scrollbar_geometry(
    axis: CodeScrollbarAxis,
    track_bounds: Bounds<Pixels>,
    viewport_length: Pixels,
    max_offset: Pixels,
    offset: Pixels,
) -> Option<CodeScrollbarGeometry> {
    let track_length = scrollbar_axis_size(axis, track_bounds.size);
    let available = track_length - 2.0 * CODE_SCROLLBAR_PADDING;
    if available <= Pixels::ZERO || viewport_length <= Pixels::ZERO || max_offset <= Pixels::ZERO {
        return None;
    }
    let content_length = viewport_length + max_offset;
    let thumb_length = (available * (viewport_length / content_length))
        .max(CODE_SCROLLBAR_MIN_THUMB)
        .min(available);
    let thumb_travel = (available - thumb_length).max(Pixels::ZERO);
    let fraction = (-offset / max_offset).clamp(0.0, 1.0);
    let thumb_offset = thumb_travel * fraction;
    let thumb_track_start = scrollbar_axis_origin(axis, track_bounds) + CODE_SCROLLBAR_PADDING;
    let thumb_bounds = match axis {
        CodeScrollbarAxis::Horizontal => Bounds::new(
            point(
                thumb_track_start + thumb_offset,
                track_bounds.top() + CODE_SCROLLBAR_PADDING,
            ),
            size(
                thumb_length,
                CODE_SCROLLBAR_SIZE - 2.0 * CODE_SCROLLBAR_PADDING,
            ),
        ),
        CodeScrollbarAxis::Vertical => Bounds::new(
            point(
                track_bounds.left() + CODE_SCROLLBAR_PADDING,
                thumb_track_start + thumb_offset,
            ),
            size(
                CODE_SCROLLBAR_SIZE - 2.0 * CODE_SCROLLBAR_PADDING,
                thumb_length,
            ),
        ),
    };
    Some(CodeScrollbarGeometry {
        axis,
        track_bounds,
        thumb_bounds,
        thumb_track_start,
        thumb_travel,
        max_offset,
    })
}

fn scrollbar_offset_for_position(
    geometry: CodeScrollbarGeometry,
    position: Point<Pixels>,
    pointer_offset: Pixels,
) -> Pixels {
    if geometry.thumb_travel <= Pixels::ZERO {
        return Pixels::ZERO;
    }
    let thumb_position = (scrollbar_axis_position(geometry.axis, position)
        - geometry.thumb_track_start
        - pointer_offset)
        .clamp(Pixels::ZERO, geometry.thumb_travel);
    if thumb_position <= px(0.01) {
        return Pixels::ZERO;
    }
    if geometry.thumb_travel - thumb_position <= px(0.01) {
        return -geometry.max_offset;
    }
    -geometry.max_offset * (thumb_position / geometry.thumb_travel)
}

fn set_code_scrollbar_offset(
    handle: &UniformListScrollHandle,
    axis: CodeScrollbarAxis,
    value: Pixels,
) {
    let base_handle = handle.0.borrow().base_handle.clone();
    let offset = base_handle.offset();
    base_handle.set_offset(match axis {
        CodeScrollbarAxis::Horizontal => point(value, offset.y),
        CodeScrollbarAxis::Vertical => point(offset.x, value),
    });
}

fn scrollbar_axis_position(axis: CodeScrollbarAxis, point: Point<Pixels>) -> Pixels {
    match axis {
        CodeScrollbarAxis::Horizontal => point.x,
        CodeScrollbarAxis::Vertical => point.y,
    }
}

fn scrollbar_axis_origin(axis: CodeScrollbarAxis, bounds: Bounds<Pixels>) -> Pixels {
    scrollbar_axis_position(axis, bounds.origin)
}

fn scrollbar_axis_size(axis: CodeScrollbarAxis, size: gpui::Size<Pixels>) -> Pixels {
    match axis {
        CodeScrollbarAxis::Horizontal => size.width,
        CodeScrollbarAxis::Vertical => size.height,
    }
}

pub fn render_code_document(
    document: Arc<CodeDocument>,
    scroll_handle: UniformListScrollHandle,
    view_state: CodeViewState,
    selection: CodeSelection,
    active_match: Option<usize>,
) -> impl IntoElement {
    let line_count = document.line_count();
    let widest_line = view_state.widest_line();
    let digits = line_count.max(1).ilog10() as f32 + 1.0;
    let gutter_width = px(18.0 + digits * 8.5);
    let list_document = document.clone();
    let list_view_state = view_state.clone();
    let list_selection = selection.clone();
    let list = uniform_list(
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
                            view_state: list_view_state.clone(),
                            selection: list_selection.clone(),
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
        .font_family(CODE_FONT_FAMILY)
        .text_size(px(14.0))
        .text_color(rgb(syntax_color(
            chatt_message_format::highlight::PaletteRole::Foreground,
        )))
        .child(list)
        .child(CodeScrollbarsElement {
            scroll_handle: scroll_handle.clone(),
            view_state,
        });
    CodeSelectionArea::new(contents, document, selection, scroll_handle)
}

struct CodeLineElement {
    document: Arc<CodeDocument>,
    line: usize,
    view_state: CodeViewState,
    selection: CodeSelection,
}

impl gpui::IntoElement for CodeLineElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for CodeLineElement {
    type RequestLayoutState = Arc<LineLayout>;
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
        if self.view_state.record_width(self.line, layout.width) {
            cx.refresh_windows();
        }
        let mut style = Style::default();
        style.size.width = layout.width.into();
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
        self.selection
            .register(self.line, selectable_bounds, bounds, layout.clone());
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
            let layout = layout.clone();
            move |event: &MouseDownEvent, phase, window, cx| {
                if !phase.bubble()
                    || event.button != MouseButton::Left
                    || !hitbox.is_hovered(window)
                {
                    return;
                }
                let offset =
                    line_start + layout.closest_index_for_x(event.position.x - bounds.left());
                selection.begin_selection(line, offset, event.click_count, event.modifiers.shift);
                window.focus(&selection.focus_handle(), cx);
                window.prevent_default();
                cx.refresh_windows();
            }
        });
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
        if let Some((range, newline_selected)) = self.selection.selected_line_range(line) {
            let start_x = layout.x_for_index(range.start);
            let mut end_x = layout.x_for_index(range.end);
            if newline_selected {
                end_x = end_x.max(layout.width + px(4.0));
            } else {
                end_x = end_x.max(start_x + px(1.0));
            }
            window.paint_quad(quad(
                Bounds::from_corners(
                    point(bounds.left() + start_x, bounds.top()),
                    point(bounds.left() + end_x, bounds.bottom()),
                ),
                Pixels::ZERO,
                rgba(0x5277a866),
                Edges::default(),
                Hsla::transparent_black(),
                BorderStyle::default(),
            ));
        }
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

    struct TestCodeView {
        document: Arc<CodeDocument>,
        scroll_handle: UniformListScrollHandle,
        view_state: CodeViewState,
        selection: CodeSelection,
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
                    self.selection.clone(),
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
    fn shaped_width_tracker_only_ratchets_to_wider_lines() {
        let tracker = CodeViewState::default();
        assert_eq!(tracker.widest_line(), 0);
        assert!(tracker.record_width(0, px(80.0)));
        assert!(!tracker.record_width(1, px(60.0)));
        assert!(!tracker.record_width(2, px(80.0)));
        assert_eq!(tracker.widest_line(), 0);
        assert!(tracker.record_width(3, px(81.0)));
        assert_eq!(tracker.widest_line(), 3);

        tracker.reset();
        assert_eq!(tracker.widest_line(), 0);
        assert!(tracker.record_width(1, px(1.0)));
        assert_eq!(tracker.widest_line(), 1);
    }

    #[test]
    fn scrollbar_geometry_maps_tracks_to_the_full_scroll_range() {
        let bounds = Bounds::new(point(px(10.0), px(20.0)), gpui::size(px(240.0), px(120.0)));
        let geometries = code_scrollbar_geometries(
            bounds,
            point(px(480.0), px(360.0)),
            point(px(-240.0), px(-180.0)),
        );
        assert_eq!(geometries.len(), 2);
        for geometry in geometries {
            assert!(bounds.contains(&geometry.thumb_bounds.center()));
            let pointer_offset =
                scrollbar_axis_size(geometry.axis, geometry.thumb_bounds.size) / 2.0;
            let mut start = geometry.thumb_bounds.center();
            match geometry.axis {
                CodeScrollbarAxis::Horizontal => {
                    start.x = geometry.thumb_track_start + pointer_offset
                }
                CodeScrollbarAxis::Vertical => {
                    start.y = geometry.thumb_track_start + pointer_offset
                }
            }
            assert_eq!(
                scrollbar_offset_for_position(geometry, start, pointer_offset),
                Pixels::ZERO
            );
            let mut end = start;
            match geometry.axis {
                CodeScrollbarAxis::Horizontal => end.x += geometry.thumb_travel,
                CodeScrollbarAxis::Vertical => end.y += geometry.thumb_travel,
            }
            assert_eq!(
                scrollbar_offset_for_position(geometry, end, pointer_offset),
                -geometry.max_offset
            );
        }
        assert!(code_scrollbar_geometries(bounds, Point::default(), Point::default()).is_empty());
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
                selection: CodeSelection::new(focus.clone()),
                focus,
            }
        });
        view.read_with(cx, |view, _| {
            assert_eq!(view.view_state.widest_line(), 1);
        });
        view.update(cx, |_, cx| cx.notify());
        view.read_with(cx, |view, _| {
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
                selection: CodeSelection::new(focus.clone()),
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
                selection: CodeSelection::new(focus.clone()),
                focus,
            }
        });
        let (start, end, maximum) = view.read_with(cx, |view, _| {
            let base_handle = view.scroll_handle.0.borrow().base_handle.clone();
            let maximum = base_handle.max_offset();
            let geometry =
                code_scrollbar_geometries(base_handle.bounds(), maximum, base_handle.offset())
                    .into_iter()
                    .find(|geometry| geometry.axis == CodeScrollbarAxis::Vertical)
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
            assert!(view.view_state.0.scrollbar_drag.get().is_none());
        });
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
        for index in
            (MAX_CODE_PREVIEW_LINE_BYTES..contents.len()).step_by(MAX_CODE_PREVIEW_LINE_BYTES)
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
        fs::write(&many_lines, vec![b'\n'; MAX_CODE_PREVIEW_LINES + 1]).expect("write many lines");
        assert_eq!(
            CodeDocument::load(&many_lines, "many-lines.txt", 2).unwrap_err(),
            "too many lines to preview"
        );
    }
}
