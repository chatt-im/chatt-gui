use std::ops::Range;

use gpui::{
    App, Bounds, ClipboardItem, Context, CursorStyle, Element, ElementId, ElementInputHandler,
    Entity, EntityInputHandler, FocusHandle, Focusable, GlobalElementId, LayoutId, MouseButton,
    MouseDownEvent, PaintQuad, Pixels, ShapedLine, SharedString, Style, TextRun, UTF16Selection,
    Window, actions, div, fill, point, prelude::*, px, relative, rgb, rgba, size,
};
use unicode_segmentation::UnicodeSegmentation;

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
        Newline
    ]
);

pub fn bind_keys(cx: &mut App) {
    use gpui::KeyBinding;
    cx.bind_keys([
        KeyBinding::new("backspace", Backspace, Some("ChattComposer")),
        KeyBinding::new("delete", Delete, Some("ChattComposer")),
        KeyBinding::new("left", Left, Some("ChattComposer")),
        KeyBinding::new("right", Right, Some("ChattComposer")),
        KeyBinding::new("shift-left", SelectLeft, Some("ChattComposer")),
        KeyBinding::new("shift-right", SelectRight, Some("ChattComposer")),
        KeyBinding::new("cmd-a", SelectAll, Some("ChattComposer")),
        KeyBinding::new("cmd-v", Paste, Some("ChattComposer")),
        KeyBinding::new("cmd-c", Copy, Some("ChattComposer")),
        KeyBinding::new("cmd-x", Cut, Some("ChattComposer")),
        KeyBinding::new("shift-enter", Newline, Some("ChattComposer")),
    ]);
}

pub struct Composer {
    focus: FocusHandle,
    content: SharedString,
    placeholder: SharedString,
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
            content: "".into(),
            placeholder: "Message".into(),
            selected: 0..0,
            reversed: false,
            marked: None,
            last_layout: Vec::new(),
            last_bounds: None,
            last_line_height: None,
        }
    }

    pub fn text(&self) -> String {
        self.content.to_string()
    }
    pub fn is_empty(&self) -> bool {
        self.content.trim().is_empty()
    }
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.content = "".into();
        self.selected = 0..0;
        self.marked = None;
        self.last_layout.clear();
        cx.notify();
    }
    pub fn restore(&mut self, text: String, cx: &mut Context<Self>) {
        let end = text.len();
        self.content = text.into();
        self.selected = end..end;
        self.last_layout.clear();
        cx.notify();
    }

    fn cursor(&self) -> usize {
        if self.reversed {
            self.selected.start
        } else {
            self.selected.end
        }
    }
    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
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
        self.content
            .grapheme_indices(true)
            .rev()
            .find_map(|(index, _)| (index < offset).then_some(index))
            .unwrap_or(0)
    }
    fn next(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .find_map(|(index, _)| (index > offset).then_some(index))
            .unwrap_or(self.content.len())
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
        self.selected = 0..self.content.len();
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
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected.clone()].to_string(),
            ));
        }
    }
    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        self.copy(&Copy, window, cx);
        self.replace_text_in_range(None, "", window, cx);
    }
    fn offset_from_utf16(&self, offset: usize) -> usize {
        self.content
            .chars()
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
    fn offset_to_utf16(&self, offset: usize) -> usize {
        self.content[..offset.min(self.content.len())]
            .encode_utf16()
            .count()
    }
    fn range_from_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range.start)..self.offset_from_utf16(range.end)
    }
    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    fn offset_for_point(&self, point: gpui::Point<Pixels>) -> Option<usize> {
        let local = self.last_bounds?.localize(&point)?;
        let line_height = self.last_line_height?;
        let line_index = ((local.y / line_height).floor() as usize)
            .min(self.last_layout.len().checked_sub(1)?);
        let line = &self.last_layout[line_index];
        let offset = line.layout.closest_index_for_x(local.x);
        Some(line.range.start + offset)
    }
}

fn logical_lines(text: &str) -> impl Iterator<Item = (Range<usize>, &str)> {
    let mut start = 0;
    text.split('\n')
        .map(move |line| {
            let end = start + line.len();
            let range = start..end;
            start = end + 1;
            (range, line)
        })
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
        Some(self.content[range].to_string())
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
        self.content =
            (self.content[..range.start].to_owned() + text + &self.content[range.end..]).into();
        let end = range.start + text.len();
        self.selected = end..end;
        self.marked = None;
        self.last_layout.clear();
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
        self.replace_text_in_range(range, text, window, cx);
        let end = self.selected.end;
        self.marked = (!text.is_empty()).then_some(end - text.len()..end);
        if let Some(selected) = selected {
            let selected = self.range_from_utf16(&selected);
            self.selected = end - text.len() + selected.start..end - text.len() + selected.end;
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
}

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
        let line_count = self.input.read(cx).content.split('\n').count().max(1);
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
        let is_placeholder = input.content.is_empty();
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
        let lines = if is_placeholder {
            vec![shape_line(0..0, input.placeholder.clone())]
        } else {
            logical_lines(&input.content)
                .map(|(range, text)| shape_line(range, text.to_string().into()))
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
        let (selection, cursor) = if input.selected.is_empty() {
            (
                Vec::new(),
                Some(fill(
                    Bounds::new(
                        point(
                            bounds.left() + cursor_x,
                            bounds.top() + line_height * cursor_line_offset as f32,
                        ),
                        size(px(2.), line_height),
                    ),
                    rgb(0x8ca9d8),
                )),
            )
        } else {
            let mut line_top = bounds.top();
            let selection = lines
                .iter()
                .filter_map(|line| {
                    let top = line_top;
                    line_top += line_height;
                    let selects_text = input.selected.start < line.range.end
                        && input.selected.end > line.range.start;
                    let selects_newline = line.range.end < input.content.len()
                        && input.selected.start <= line.range.end
                        && input.selected.end > line.range.end;
                    if !selects_text && !selects_newline {
                        return None;
                    }
                    let start = line.local_offset(input.selected.start);
                    let end = line.local_offset(input.selected.end);
                    let left = bounds.left() + line.layout.x_for_index(start);
                    let mut right = bounds.left() + line.layout.x_for_index(end);
                    if selects_newline {
                        right += px(4.);
                    }
                    Some(fill(
                        Bounds::from_corners(
                            point(left, top),
                            point(right, top + line_height),
                        ),
                        rgba(0x6f8fc044),
                    ))
                })
                .collect();
            (selection, None)
        };
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
        });
    }
}

impl Render for Composer {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .key_context("ChattComposer")
            .track_focus(&self.focus)
            .cursor(CursorStyle::IBeam)
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
            .on_action(cx.listener(Self::newline))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    window.focus(&this.focus, cx);
                    let offset = this
                        .offset_for_point(event.position)
                        .unwrap_or(this.content.len());
                    this.move_to(offset, cx);
                }),
            )
            .w_full()
            .min_h(px(42.))
            .child(ComposerElement { input: cx.entity() })
    }
}

impl Focusable for Composer {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::{ComposerLine, line_for_offset, logical_lines};

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
        assert!(lines
            .iter()
            .all(|(_, line)| !line.contains('\n')));
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
}
