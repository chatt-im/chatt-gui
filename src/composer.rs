use std::ops::Range;

use gpui::{
    App, Bounds, ClipboardItem, Context, CursorStyle, Element, ElementId, ElementInputHandler,
    Entity, EntityInputHandler, EventEmitter, FocusHandle, Focusable, GlobalElementId,
    KeyDownEvent, LayoutId, MouseButton, MouseDownEvent, PaintQuad, Pixels, ShapedLine,
    SharedString, Style, TextRun, UTF16Selection, Window, actions, div, fill, point, prelude::*,
    px, relative, rgb, rgba, size,
};

mod buffer;
mod cursor;
mod history;
mod mode;
mod vim;
mod visual;

use mode::Mode;
use vim::{VimEditor, VimKey};

const VIM_MODE: bool = true;
const MAX_VISIBLE_LINES: usize = 8;

actions!(
    composer,
    [
        Backspace,
        Delete,
        Left,
        Right,
        SelectLeft,
        SelectRight,
        SelectAll,
        Paste,
        Copy,
        Cut,
        InsertTab,
        Newline
    ]
);

pub fn bind_keys(cx: &mut App) {
    use gpui::KeyBinding;
    cx.bind_keys([
        KeyBinding::new("backspace", Backspace, Some("ComposerInsert")),
        KeyBinding::new("backspace", Backspace, Some("ChattCodeSearch")),
        KeyBinding::new("delete", Delete, Some("ComposerInsert")),
        KeyBinding::new("delete", Delete, Some("ChattCodeSearch")),
        KeyBinding::new("left", Left, Some("ComposerInsert")),
        KeyBinding::new("left", Left, Some("ChattCodeSearch")),
        KeyBinding::new("right", Right, Some("ComposerInsert")),
        KeyBinding::new("right", Right, Some("ChattCodeSearch")),
        KeyBinding::new("shift-left", SelectLeft, Some("ComposerInsert")),
        KeyBinding::new("shift-left", SelectLeft, Some("ChattCodeSearch")),
        KeyBinding::new("shift-right", SelectRight, Some("ComposerInsert")),
        KeyBinding::new("shift-right", SelectRight, Some("ChattCodeSearch")),
        KeyBinding::new("cmd-a", SelectAll, Some("ComposerInsert")),
        KeyBinding::new("cmd-a", SelectAll, Some("ChattCodeSearch")),
        KeyBinding::new("cmd-v", Paste, Some("ComposerInsert")),
        KeyBinding::new("cmd-v", Paste, Some("ChattCodeSearch")),
        KeyBinding::new("cmd-c", Copy, Some("ComposerInsert")),
        KeyBinding::new("cmd-c", Copy, Some("ChattCodeSearch")),
        KeyBinding::new("cmd-x", Cut, Some("ComposerInsert")),
        KeyBinding::new("cmd-x", Cut, Some("ChattCodeSearch")),
        KeyBinding::new("tab", InsertTab, Some("ComposerInsert")),
        KeyBinding::new("shift-enter", Newline, Some("ComposerInsert")),
    ]);
}

pub struct ComposerChanged;

pub struct Composer {
    focus: FocusHandle,
    editor: VimEditor,
    placeholder: SharedString,
    key_context: &'static str,
    multiline: bool,
    vim_enabled: bool,
    min_height: Pixels,
    selected: Range<usize>,
    reversed: bool,
    marked: Option<Range<usize>>,
    last_layout: Vec<ComposerLine>,
    last_bounds: Option<Bounds<Pixels>>,
    last_line_height: Option<Pixels>,
}

impl Composer {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            focus: cx.focus_handle(),
            editor: VimEditor::new(),
            placeholder: "Message".into(),
            key_context: "ChattComposer",
            multiline: true,
            vim_enabled: VIM_MODE,
            min_height: px(42.),
            selected: 0..0,
            reversed: false,
            marked: None,
            last_layout: Vec::new(),
            last_bounds: None,
            last_line_height: None,
        }
    }

    pub fn search(cx: &mut Context<Self>) -> Self {
        let mut editor = VimEditor::new();
        editor.set_text("", Mode::Insert, true);
        Self {
            focus: cx.focus_handle(),
            editor,
            placeholder: "Find in file".into(),
            key_context: "ChattCodeSearch",
            multiline: false,
            vim_enabled: false,
            min_height: px(28.),
            selected: 0..0,
            reversed: false,
            marked: None,
            last_layout: Vec::new(),
            last_bounds: None,
            last_line_height: None,
        }
    }

    pub fn text(&self) -> String {
        self.editor.text()
    }
    pub fn is_empty(&self) -> bool {
        self.editor.is_blank()
    }
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.editor.set_text("", Mode::Insert, true);
        self.selected = 0..0;
        self.reversed = false;
        self.marked = None;
        self.last_layout.clear();
        cx.emit(ComposerChanged);
        cx.notify();
    }
    pub fn restore(&mut self, text: String, cx: &mut Context<Self>) {
        let at_end = !self.vim_enabled;
        let mode = if self.vim_enabled {
            Mode::Normal
        } else {
            Mode::Insert
        };
        self.editor.set_text(&text, mode, at_end);
        let cursor = self.editor.cursor_offset();
        self.selected = cursor..cursor;
        self.reversed = false;
        self.marked = None;
        self.last_layout.clear();
        cx.emit(ComposerChanged);
        cx.notify();
    }

    fn cursor(&self) -> usize {
        if self.vim_enabled && self.editor.mode() != Mode::Insert {
            return self.editor.cursor_offset();
        }
        if self.reversed {
            self.selected.start
        } else {
            self.selected.end
        }
    }
    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.editor.set_cursor_offset(offset);
        let offset = self.editor.cursor_offset();
        self.selected = offset..offset;
        self.reversed = false;
        cx.notify();
    }
    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        if self.reversed {
            self.selected.start = offset
        } else {
            self.selected.end = offset
        }
        if self.selected.end < self.selected.start {
            self.reversed = !self.reversed;
            self.selected = self.selected.end..self.selected.start;
        }
        cx.notify();
    }
    fn previous(&self, offset: usize) -> usize {
        self.editor.previous_offset(offset)
    }
    fn next(&self, offset: usize) -> usize {
        self.editor.next_offset(offset)
    }
    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        let to = if self.selected.is_empty() {
            self.previous(self.cursor())
        } else {
            self.selected.start
        };
        self.move_to(to, cx);
    }
    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        let to = if self.selected.is_empty() {
            self.next(self.cursor())
        } else {
            self.selected.end
        };
        self.move_to(to, cx);
    }
    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.previous(self.cursor()), cx);
    }
    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.next(self.cursor()), cx);
    }
    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.selected = 0..self.editor.len();
        cx.notify();
    }
    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected.is_empty() {
            self.select_to(self.previous(self.cursor()), cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }
    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected.is_empty() {
            self.select_to(self.next(self.cursor()), cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }
    fn newline(&mut self, _: &Newline, window: &mut Window, cx: &mut Context<Self>) {
        self.replace_text_in_range(None, "\n", window, cx);
    }
    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.replace_text_in_range(None, &text, window, cx);
        }
    }
    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selected.is_empty() {
            let selected = self.normalize_range(self.selected.clone());
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.editor.slice(selected).into_owned(),
            ));
        }
    }
    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        self.copy(&Copy, window, cx);
        self.replace_text_in_range(None, "", window, cx);
    }
    fn insert_tab(&mut self, _: &InsertTab, window: &mut Window, cx: &mut Context<Self>) {
        self.replace_text_in_range(None, "    ", window, cx);
    }
    fn offset_from_utf16(&self, offset: usize) -> usize {
        self.editor.offset_from_utf16(offset)
    }
    fn offset_to_utf16(&self, offset: usize) -> usize {
        self.editor.offset_to_utf16(self.clamp_offset(offset))
    }
    fn range_from_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.normalize_range(self.offset_from_utf16(range.start)..self.offset_from_utf16(range.end))
    }
    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        let range = self.normalize_range(range.clone());
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }
    fn clamp_offset(&self, offset: usize) -> usize {
        self.editor.clamp_offset(offset)
    }
    fn normalize_range(&self, range: Range<usize>) -> Range<usize> {
        let start = self.clamp_offset(range.start);
        let end = self.clamp_offset(range.end);
        start.min(end)..start.max(end)
    }
    fn accepted_text<'a>(&self, text: &'a str) -> &'a str {
        if self.multiline {
            text
        } else {
            text.split(['\r', '\n']).next().unwrap_or("")
        }
    }

    fn offset_for_point(&self, point: gpui::Point<Pixels>) -> Option<usize> {
        let local = self.last_bounds?.localize(&point)?;
        let line_height = self.last_line_height?;
        let line_index =
            ((local.y / line_height).floor() as usize).min(self.last_layout.len().checked_sub(1)?);
        let line = &self.last_layout[line_index];
        let offset = line.layout.closest_index_for_x(local.x);
        Some(line.range.start + offset)
    }

    fn handle_vim_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.vim_enabled {
            return;
        }
        let Some(key) = vim_key(event) else {
            return;
        };
        if self.editor.mode() == Mode::Insert && key != VimKey::Escape {
            return;
        }
        let version = self.editor.text_version();
        if !self.editor.send_key(key) {
            return;
        }
        let cursor = self.editor.cursor_offset();
        self.selected = cursor..cursor;
        self.reversed = false;
        self.marked = None;
        self.last_layout.clear();
        if self.editor.text_version() != version {
            cx.emit(ComposerChanged);
        }
        window.prevent_default();
        cx.stop_propagation();
        cx.notify();
    }
}

fn clamp_offset(text: &str, offset: usize) -> usize {
    let mut offset = offset.min(text.len());
    while !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

fn normalize_range(text: &str, range: Range<usize>) -> Range<usize> {
    let start = clamp_offset(text, range.start);
    let end = clamp_offset(text, range.end);
    start.min(end)..start.max(end)
}

fn offset_from_utf16(text: &str, offset: usize) -> usize {
    text.chars()
        .scan((0, 0), |state, ch| {
            if state.1 >= offset {
                return None;
            }
            state.0 += ch.len_utf8();
            state.1 += ch.len_utf16();
            Some(state.0)
        })
        .last()
        .unwrap_or(0)
}

fn range_from_utf16(text: &str, range: &Range<usize>) -> Range<usize> {
    normalize_range(
        text,
        offset_from_utf16(text, range.start)..offset_from_utf16(text, range.end),
    )
}

#[cfg(test)]
fn logical_lines(text: &str) -> impl Iterator<Item = (Range<usize>, &str)> {
    let mut start = 0;
    text.split('\n').map(move |line| {
        let end = start + line.len();
        let range = start..end;
        start = end + 1;
        (range, line)
    })
}

fn visible_line_range(line_count: usize, cursor_row: usize) -> Range<usize> {
    let visible_count = line_count.clamp(1, MAX_VISIBLE_LINES);
    let start = cursor_row
        .saturating_add(1)
        .saturating_sub(visible_count)
        .min(line_count.saturating_sub(visible_count));
    start..start + visible_count
}

fn line_for_offset(lines: &[ComposerLine], offset: usize) -> Option<&ComposerLine> {
    lines
        .iter()
        .find(|line| offset <= line.range.end)
        .or_else(|| lines.last())
}

impl EntityInputHandler for Composer {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        actual: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range);
        actual.replace(self.range_to_utf16(&range));
        Some(self.editor.slice(range).into_owned())
    }
    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.selected),
            reversed: self.reversed,
        })
    }
    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked.as_ref().map(|range| self.range_to_utf16(range))
    }
    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {
        self.marked = None;
    }
    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range
            .as_ref()
            .map(|range| self.range_from_utf16(range))
            .or(self.marked.clone())
            .unwrap_or(self.selected.clone());
        let range = self.normalize_range(range);
        let text = self.accepted_text(text);
        self.editor.replace_offsets(range.clone(), text);
        let end = range.start + text.len();
        self.selected = end..end;
        self.editor.set_cursor_offset(end);
        self.marked = None;
        self.last_layout.clear();
        cx.emit(ComposerChanged);
        cx.notify();
    }
    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        selected: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = self.accepted_text(text);
        self.replace_text_in_range(range, text, window, cx);
        let end = self.selected.end;
        let inserted = end - text.len()..end;
        self.marked = (!text.is_empty()).then_some(inserted.clone());
        if let Some(selected) = selected {
            let selected = range_from_utf16(text, &selected);
            self.selected = inserted.start + selected.start..inserted.start + selected.end;
            self.editor.set_cursor_offset(self.selected.end);
        }
    }
    fn bounds_for_range(
        &mut self,
        range: Range<usize>,
        bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let range = self.range_from_utf16(&range);
        let start_line = line_for_offset(&self.last_layout, range.start)?;
        let end_line = line_for_offset(&self.last_layout, range.end)?;
        let start_offset = start_line.local_offset(range.start);
        let end_offset = end_line.local_offset(range.end);
        let start_line_offset = self.last_layout.element_offset(start_line)?;
        let end_line_offset = self.last_layout.element_offset(end_line)?;
        let line_height = self.last_line_height?;
        if std::ptr::eq(start_line, end_line) {
            Some(Bounds::from_corners(
                point(
                    bounds.left() + start_line.layout.x_for_index(start_offset),
                    bounds.top() + line_height * start_line_offset as f32,
                ),
                point(
                    bounds.left() + end_line.layout.x_for_index(end_offset),
                    bounds.top() + line_height * (end_line_offset + 1) as f32,
                ),
            ))
        } else {
            Some(Bounds::from_corners(
                point(
                    bounds.left(),
                    bounds.top() + line_height * start_line_offset as f32,
                ),
                point(
                    bounds.right(),
                    bounds.top() + line_height * (end_line_offset + 1) as f32,
                ),
            ))
        }
    }
    fn character_index_for_point(
        &mut self,
        point: gpui::Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        self.offset_for_point(point)
            .map(|offset| self.offset_to_utf16(offset))
    }

    fn set_selected_text_range(
        &mut self,
        range: Range<usize>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = self.range_from_utf16(&range);
        self.selected = range.clone();
        self.reversed = false;
        self.editor.set_cursor_offset(range.end);
        cx.notify();
    }

    fn text_length_utf16(&mut self, _: &mut Window, _: &mut Context<Self>) -> Option<usize> {
        Some(self.offset_to_utf16(self.editor.len()))
    }

    fn accepts_text_input(&self, _: &mut Window, _: &mut Context<Self>) -> bool {
        !self.vim_enabled || self.editor.mode() == Mode::Insert
    }
}

impl EventEmitter<ComposerChanged> for Composer {}

#[derive(Clone)]
struct ComposerLine {
    range: Range<usize>,
    layout: ShapedLine,
}

impl ComposerLine {
    fn local_offset(&self, offset: usize) -> usize {
        offset
            .saturating_sub(self.range.start)
            .min(self.range.len())
    }
}

struct ComposerElement {
    input: Entity<Composer>,
}
struct Prepaint {
    lines: Vec<ComposerLine>,
    cursor: Option<PaintQuad>,
    selection: Vec<PaintQuad>,
}
impl IntoElement for ComposerElement {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}
impl Element for ComposerElement {
    type RequestLayoutState = ();
    type PrepaintState = Prepaint;
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
    ) -> (LayoutId, ()) {
        let input = self.input.read(cx);
        let line_count = if input.multiline {
            input.editor.line_count().clamp(1, MAX_VISIBLE_LINES)
        } else {
            1
        };
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = (window.line_height() * line_count as f32).into();
        (window.request_layout(style, [], cx), ())
    }
    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Prepaint {
        let input = self.input.read(cx);
        let is_placeholder = input.editor.len() == 0;
        let color = if is_placeholder {
            rgb(0x747a84).into()
        } else {
            window.text_style().color
        };
        let font = window.text_style().font();
        let font_size = window.text_style().font_size.to_pixels(window.rem_size());
        let shape_line = |range, text: SharedString| {
            let run = TextRun {
                len: text.len(),
                font: font.clone(),
                color,
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            ComposerLine {
                range,
                layout: window
                    .text_system()
                    .shape_line(text, font_size, &[run], None),
            }
        };
        let cursor_row = input.editor.offset_to_rowcol(input.cursor()).0;
        let visible_rows = visible_line_range(input.editor.line_count(), cursor_row);
        let lines = if is_placeholder {
            vec![shape_line(0..0, input.placeholder.clone())]
        } else {
            visible_rows
                .map(|row| {
                    let start = input.editor.line_start(row);
                    let text = input.editor.line(row);
                    let range = start..start + text.len();
                    shape_line(range, text.into_owned().into())
                })
                .collect::<Vec<_>>()
        };
        let line_height = window.line_height();
        let cursor_line = line_for_offset(&lines, input.cursor())
            .expect("composer always lays out at least one logical line");
        let cursor_line_offset = lines
            .element_offset(cursor_line)
            .expect("cursor line belongs to the composer layout");
        let cursor_x = cursor_line
            .layout
            .x_for_index(cursor_line.local_offset(input.cursor()));
        let selection_ranges = if input.vim_enabled && input.editor.mode().is_visual() {
            input.editor.visual_ranges()
        } else {
            (!input.selected.is_empty())
                .then(|| vec![input.selected.clone()])
                .unwrap_or_default()
        };
        let selection = selection_ranges
            .iter()
            .flat_map(|selected| {
                let mut line_top = bounds.top();
                lines
                    .iter()
                    .filter_map(|line| {
                        let top = line_top;
                        line_top += line_height;
                        let selects_text =
                            selected.start < line.range.end && selected.end > line.range.start;
                        let selects_newline = line.range.end < input.editor.len()
                            && selected.start <= line.range.end
                            && selected.end > line.range.end;
                        if !selects_text && !selects_newline {
                            return None;
                        }
                        let start = line.local_offset(selected.start);
                        let end = line.local_offset(selected.end);
                        let left = bounds.left() + line.layout.x_for_index(start);
                        let mut right = bounds.left() + line.layout.x_for_index(end);
                        if selects_newline {
                            right += px(4.);
                        }
                        Some(fill(
                            Bounds::from_corners(point(left, top), point(right, top + line_height)),
                            rgba(0x6f8fc044),
                        ))
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        let cursor_width = if input.vim_enabled && input.editor.mode() != Mode::Insert {
            let next = input.editor.next_offset(input.cursor());
            let width = if next <= cursor_line.range.end {
                cursor_line
                    .layout
                    .x_for_index(cursor_line.local_offset(next))
                    - cursor_x
            } else {
                px(8.)
            };
            if width > px(2.) { width } else { px(8.) }
        } else {
            px(2.)
        };
        let cursor = (input.vim_enabled || input.selected.is_empty()).then(|| {
            fill(
                Bounds::new(
                    point(
                        bounds.left() + cursor_x,
                        bounds.top() + line_height * cursor_line_offset as f32,
                    ),
                    size(cursor_width, line_height),
                ),
                if input.vim_enabled && input.editor.mode() != Mode::Insert {
                    rgba(0x8ca9d888)
                } else {
                    rgba(0x8ca9d8ff)
                },
            )
        });
        Prepaint {
            lines,
            cursor,
            selection,
        }
    }
    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        state: &mut Prepaint,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus = self.input.read(cx).focus.clone();
        window.handle_input(
            &focus,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );
        for selection in state.selection.drain(..) {
            window.paint_quad(selection);
        }
        let line_height = window.line_height();
        let mut origin = bounds.origin;
        for line in &state.lines {
            line.layout
                .paint(
                    origin,
                    window.line_height(),
                    gpui::TextAlign::Left,
                    None,
                    window,
                    cx,
                )
                .unwrap();
            origin.y += line_height;
        }
        if focus.is_focused(window)
            && let Some(cursor) = state.cursor.take()
        {
            window.paint_quad(cursor);
        }
        self.input.update(cx, |input, _| {
            input.last_layout = state.lines.clone();
            input.last_bounds = Some(bounds);
            input.last_line_height = Some(line_height);
            let columns = (bounds.size.width / px(8.)).max(1.) as u16;
            input
                .editor
                .set_layout(columns, state.lines.len().max(1) as u16);
        });
    }
}

impl Render for Composer {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let key_context = if self.key_context == "ChattCodeSearch" {
            self.key_context
        } else if !self.vim_enabled || self.editor.mode() == Mode::Insert {
            "ChattComposer ComposerInsert"
        } else {
            "ChattComposer VimMode"
        };
        div()
            .flex()
            .items_center()
            .key_context(key_context)
            .track_focus(&self.focus)
            .cursor(CursorStyle::IBeam)
            .on_key_down(cx.listener(Self::handle_vim_key))
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::cut))
            .when(self.multiline, |input| {
                input
                    .on_action(cx.listener(Self::insert_tab))
                    .on_action(cx.listener(Self::newline))
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    window.focus(&this.focus, cx);
                    let offset = this
                        .offset_for_point(event.position)
                        .unwrap_or(this.editor.len());
                    this.move_to(offset, cx);
                }),
            )
            .w_full()
            .min_h(self.min_height)
            .child(ComposerElement { input: cx.entity() })
    }
}

fn vim_key(event: &KeyDownEvent) -> Option<VimKey> {
    let keystroke = &event.keystroke;
    let modifiers = keystroke.modifiers;
    if modifiers.platform || modifiers.alt || modifiers.function {
        return None;
    }
    if modifiers.control {
        let ch = keystroke.key.chars().next()?.to_ascii_lowercase();
        return Some(VimKey::Control(ch));
    }
    match keystroke.key.as_str() {
        "escape" => Some(VimKey::Escape),
        "backspace" => Some(VimKey::Backspace),
        "enter" => Some(VimKey::Enter),
        "tab" => Some(VimKey::Tab),
        "left" => Some(VimKey::Left),
        "right" => Some(VimKey::Right),
        "up" => Some(VimKey::Up),
        "down" => Some(VimKey::Down),
        "home" => Some(VimKey::Home),
        "end" => Some(VimKey::End),
        _ => keystroke
            .key_char
            .as_deref()
            .or(Some(keystroke.key.as_str()))
            .and_then(|text| {
                let mut chars = text.chars();
                let ch = chars.next()?;
                chars.next().is_none().then_some(VimKey::Char(ch))
            }),
    }
}

impl Focusable for Composer {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ComposerLine, line_for_offset, logical_lines, normalize_range, range_from_utf16,
        visible_line_range,
    };

    #[test]
    fn splits_multiline_content_before_single_line_shaping() {
        let text = "first\n\nthird\n";
        let lines = logical_lines(text).collect::<Vec<_>>();

        assert_eq!(
            lines
                .iter()
                .map(|(range, _)| range.clone())
                .collect::<Vec<_>>(),
            vec![0..5, 6..6, 7..12, 13..13]
        );
        assert!(lines.iter().all(|(_, line)| !line.contains('\n')));
    }

    #[test]
    fn maps_offsets_on_newlines_to_the_adjacent_logical_lines() {
        let lines = logical_lines("one\n\nthree")
            .map(|(range, _)| ComposerLine {
                range,
                layout: Default::default(),
            })
            .collect::<Vec<_>>();
        let line_at = |offset| {
            line_for_offset(&lines, offset)
                .map(|line| (line.range.clone(), line.local_offset(offset)))
        };

        assert_eq!(line_at(3), Some((0..3, 3)));
        assert_eq!(line_at(4), Some((4..4, 0)));
        assert_eq!(line_at(5), Some((5..10, 0)));
        assert_eq!(line_at(10), Some((5..10, 5)));
        assert!(line_for_offset(&[], 0).is_none());
    }

    #[test]
    fn clamps_stale_platform_replacement_ranges() {
        assert_eq!(normalize_range("", 7..7), 0..0);
        assert_eq!(normalize_range("é", 1..99), 0..2);
    }

    #[test]
    fn maps_utf16_ranges_relative_to_composition_text() {
        assert_eq!(range_from_utf16("a😀b", &(1..3)), 1..5);
    }

    #[test]
    fn bounds_the_shaped_viewport_for_ten_thousand_line_messages() {
        assert_eq!(visible_line_range(10_000, 0), 0..8);
        assert_eq!(visible_line_range(10_000, 5_000), 4_993..5_001);
        assert_eq!(visible_line_range(10_000, 9_999), 9_992..10_000);
    }
}
