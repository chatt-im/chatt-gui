use std::{ops::Range, sync::Arc};

use gpui::{
    App, Bounds, ClipboardEntry, ClipboardItem, Context, CursorStyle, Element, ElementId,
    ElementInputHandler, Entity, EntityInputHandler, EventEmitter, FocusHandle, Focusable, Font,
    FontStyle, FontWeight, GlobalElementId, Hsla, KeyDownEvent, LayoutId, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, ShapedLine, SharedString,
    Style, TextRun, UTF16Selection, UnderlineStyle, Window, actions, div, fill, point, prelude::*,
    px, relative, rgb, rgba, size,
};

mod buffer;
pub(crate) mod completion;
mod cursor;
mod highlight;
mod history;
mod mode;
pub(crate) mod uploads;
mod vim;
mod visual;

use crate::{
    emoji,
    fonts::CODE_FONT_FAMILY,
    formatted_message::syntax_color,
    theme::{AppliedSettings, ResolvedSettings, ThemeRole, syntax_role},
};
use chatt_message_format::highlight::PaletteRole;
use highlight::{ComposerColor, ComposerSyntax, ComposerTextStyle, ComposerTypeface};
pub(crate) use mode::Mode;
use vim::{VimEditor, VimKey};

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

pub struct ComposerChanged;
pub struct ComposerStateChanged;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PastedImage {
    pub format: gpui::ImageFormat,
    pub bytes: Arc<Vec<u8>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ComposerImagePaste {
    pub images: Arc<[PastedImage]>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ComposerSnapshot {
    pub text: String,
    pub selection: Range<usize>,
    pub accepts_completion: bool,
    pub composing: bool,
}

pub struct TextEditor {
    focus: FocusHandle,
    editor: VimEditor,
    placeholder: SharedString,
    key_context: &'static str,
    multiline: bool,
    vim_enabled: bool,
    accepts_image_paste: bool,
    min_height: Pixels,
    selected: Range<usize>,
    reversed: bool,
    mouse_anchor: Option<usize>,
    last_yank_revision: u64,
    marked: Option<Range<usize>>,
    completion_open: bool,
    completion_engaged: bool,
    expand_emoji_shortcodes: bool,
    last_layout: Vec<ComposerLine>,
    last_bounds: Option<Bounds<Pixels>>,
    last_line_height: Option<Pixels>,
    syntax: Option<ComposerSyntax>,
}

pub use TextEditor as Composer;

impl TextEditor {
    #[cfg(test)]
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self::with_binding_mode(crate::config::schema::BindingMode::Vim, cx)
    }

    pub(crate) fn with_binding_mode(
        binding_mode: crate::config::schema::BindingMode,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            focus: cx.focus_handle(),
            editor: VimEditor::new(),
            placeholder: "Message".into(),
            key_context: "ChattComposer",
            multiline: true,
            vim_enabled: binding_mode == crate::config::schema::BindingMode::Vim,
            accepts_image_paste: true,
            min_height: px(42.),
            selected: 0..0,
            reversed: false,
            mouse_anchor: None,
            last_yank_revision: 0,
            marked: None,
            completion_open: false,
            completion_engaged: false,
            expand_emoji_shortcodes: true,
            last_layout: Vec::new(),
            last_bounds: None,
            last_line_height: None,
            syntax: Some(ComposerSyntax::default()),
        }
    }

    pub fn search(cx: &mut Context<Self>) -> Self {
        let mut editor = VimEditor::new();
        editor.set_single_line(true);
        editor.set_text("", Mode::Insert, true);
        Self {
            focus: cx.focus_handle(),
            editor,
            placeholder: "Find in file".into(),
            key_context: "ChattCodeSearch",
            multiline: false,
            vim_enabled: false,
            accepts_image_paste: false,
            min_height: px(28.),
            selected: 0..0,
            reversed: false,
            mouse_anchor: None,
            last_yank_revision: 0,
            marked: None,
            completion_open: false,
            completion_engaged: false,
            expand_emoji_shortcodes: false,
            last_layout: Vec::new(),
            last_bounds: None,
            last_line_height: None,
            syntax: None,
        }
    }

    pub fn server_search(cx: &mut Context<Self>) -> Self {
        let mut input = Self::search(cx);
        input.placeholder = "Search servers".into();
        input.key_context = "ChattServerSearch";
        input
    }

    pub(crate) fn settings_input(
        placeholder: impl Into<SharedString>,
        binding_mode: crate::config::schema::BindingMode,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut input = Self::search(cx);
        input.placeholder = placeholder.into();
        input.key_context = "ChattSettingsInput";
        input.vim_enabled = binding_mode == crate::config::schema::BindingMode::Vim;
        input
            .editor
            .set_primary_mode_preserving_history(if input.vim_enabled {
                Mode::Normal
            } else {
                Mode::Insert
            });
        input
    }

    pub(crate) fn mode(&self) -> Mode {
        self.editor.mode()
    }

    pub(crate) fn enter_insert_mode(&mut self, cx: &mut Context<Self>) {
        if self.vim_enabled && self.editor.mode() == Mode::Normal {
            self.editor
                .set_primary_mode_preserving_history(Mode::Insert);
            self.finish_vim_action(self.editor.text_version(), cx);
        }
    }

    pub fn text(&self) -> String {
        self.editor.text()
    }
    pub fn snapshot(&self) -> ComposerSnapshot {
        ComposerSnapshot {
            text: self.editor.text(),
            selection: self.selected.clone(),
            accepts_completion: !self.vim_enabled || self.editor.mode() == Mode::Insert,
            composing: self.marked.is_some(),
        }
    }
    pub fn is_empty(&self) -> bool {
        self.editor.is_blank()
    }
    pub fn set_completion_open(&mut self, open: bool) {
        self.completion_open = open;
        if !open {
            self.completion_engaged = false;
        }
    }
    pub fn set_completion_state(&mut self, open: bool, engaged: bool) {
        self.completion_open = open;
        self.completion_engaged = open && engaged;
    }
    pub fn replace_completion(
        &mut self,
        range: Range<usize>,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = self.normalize_range(range);
        self.selected = range.clone();
        self.reversed = false;
        self.editor.set_cursor_offset(range.end);
        self.marked = None;
        self.replace_text(None, text, false, false, window, cx);
    }

    pub(crate) fn insert_message_reference(
        &mut self,
        reference: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.vim_enabled && self.editor.mode() != Mode::Insert {
            self.editor
                .set_primary_mode_preserving_history(Mode::Insert);
            let cursor = self.editor.cursor_offset();
            self.selected = cursor..cursor;
            self.reversed = false;
            self.marked = None;
        }
        let range = self.normalize_range(self.selected.clone());
        let insertion = message_ref_insertion(&self.editor.text(), range.start, reference);
        self.replace_text(None, &insertion, false, false, window, cx);
    }
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.editor.set_text("", Mode::Insert, true);
        self.selected = 0..0;
        self.reversed = false;
        self.marked = None;
        self.last_layout.clear();
        self.refresh_syntax();
        cx.emit(ComposerChanged);
        cx.emit(ComposerStateChanged);
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
        self.refresh_syntax();
        cx.emit(ComposerChanged);
        cx.emit(ComposerStateChanged);
        cx.notify();
    }

    pub(crate) fn set_value(&mut self, value: impl Into<String>, cx: &mut Context<Self>) {
        self.restore(value.into(), cx);
    }

    pub(crate) fn set_binding_mode(
        &mut self,
        mode: crate::config::schema::BindingMode,
        cx: &mut Context<Self>,
    ) {
        let vim_enabled = mode == crate::config::schema::BindingMode::Vim;
        if self.vim_enabled == vim_enabled {
            return;
        }
        self.vim_enabled = vim_enabled;
        self.editor
            .set_primary_mode_preserving_history(if vim_enabled {
                Mode::Normal
            } else {
                Mode::Insert
            });
        let cursor = self.editor.cursor_offset();
        self.selected = cursor..cursor;
        self.reversed = false;
        self.marked = None;
        self.last_layout.clear();
        cx.emit(ComposerStateChanged);
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
        cx.emit(ComposerStateChanged);
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
        cx.emit(ComposerStateChanged);
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
        cx.emit(ComposerStateChanged);
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
        let Some(item) = cx.read_from_clipboard() else {
            log::info!("clipboard paste returned no clipboard item");
            return;
        };
        let metadata = item.metadata().cloned();
        let item = match self.emit_clipboard_images(item, cx) {
            Ok(()) => return,
            Err(item) => item,
        };
        let Some(text) = item.text() else {
            log::info!("clipboard paste contained no usable text or image data");
            return;
        };
        if self.vim_enabled && self.editor.mode() != Mode::Insert {
            self.paste_in_vim_mode(&text, metadata.as_deref(), VimKey::Char('p'), cx);
        } else {
            self.replace_text(None, &text, false, true, window, cx);
        }
    }
    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        let ranges = if self.vim_enabled && self.editor.mode().is_visual() {
            self.editor.visual_ranges()
        } else if !self.selected.is_empty() {
            vec![self.normalize_range(self.selected.clone())]
        } else {
            Vec::new()
        };
        if !ranges.is_empty() {
            let text = ranges
                .into_iter()
                .map(|range| self.editor.slice(range).into_owned())
                .collect::<Vec<_>>()
                .join("\n");
            if self.vim_enabled && self.editor.mode().is_visual() {
                cx.write_to_clipboard(ClipboardItem::new_string_with_metadata(
                    text,
                    self.editor.visual_clipboard_metadata().to_string(),
                ));
            } else {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
            }
        }
    }
    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        self.copy(&Copy, window, cx);
        if self.vim_enabled && self.editor.mode().is_visual() {
            let version = self.editor.text_version();
            if self.editor.delete_visual_selection() {
                self.finish_vim_action(version, cx);
            }
        } else if !self.selected.is_empty() {
            self.replace_text_in_range(None, "", window, cx);
        }
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

    fn replace_text(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        auto_close_fence: bool,
        expand_shortcode: bool,
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
        let before = self.editor.slice(0..range.start);
        let after = self.editor.slice(range.end..self.editor.len());
        let close_fence = auto_close_fence
            && self.multiline
            && self.marked.is_none()
            && should_auto_close_code_fence(&before, text, &after);
        let replacement;
        let text = if close_fence {
            replacement = format!("{text}\n```");
            replacement.as_str()
        } else {
            text
        };
        self.editor.replace_offsets(range.clone(), text);
        let mut end = if close_fence {
            range.start + 1
        } else {
            range.start + text.len()
        };
        if expand_shortcode && self.expand_emoji_shortcodes {
            let value = self.editor.text();
            if let Some(completed) = emoji::find_completed_shortcode(&value, end)
                && let Some(record) = emoji::exact_shortcode(completed.shortcode)
            {
                let range = completed.range;
                self.editor.replace_offsets(range.clone(), &record.unicode);
                end = range.start + record.unicode.len();
            }
        }
        self.selected = end..end;
        self.editor.set_cursor_offset(end);
        self.marked = None;
        self.last_layout.clear();
        self.refresh_syntax();
        cx.emit(ComposerChanged);
        cx.emit(ComposerStateChanged);
        cx.notify();
    }

    fn refresh_syntax(&mut self) {
        let Some(syntax) = &mut self.syntax else {
            return;
        };
        let version = self.editor.text_version();
        let text = self.editor.text();
        syntax.refresh(version, &text);
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

    fn set_mouse_selection(&mut self, anchor: usize, head: usize, cx: &mut Context<Self>) {
        let anchor = self.clamp_offset(anchor);
        let head = self.clamp_offset(head);
        self.selected = anchor.min(head)..anchor.max(head);
        self.reversed = head < anchor;
        self.editor.set_cursor_offset(head);
        self.marked = None;
        cx.emit(ComposerStateChanged);
        cx.notify();
    }

    fn begin_mouse_selection(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus, cx);
        let offset = self
            .offset_for_point(event.position)
            .unwrap_or(self.editor.len());
        let text = self.editor.text();
        if event.click_count >= 3 {
            let range = logical_line_range(&text, offset);
            self.mouse_anchor = Some(range.start);
            self.set_mouse_selection(range.start, range.end, cx);
        } else if event.click_count == 2 {
            let range = word_range(&text, offset);
            self.mouse_anchor = Some(range.start);
            self.set_mouse_selection(range.start, range.end, cx);
        } else if event.modifiers.shift {
            let anchor = if self.reversed {
                self.selected.end
            } else {
                self.selected.start
            };
            self.mouse_anchor = Some(anchor);
            self.set_mouse_selection(anchor, offset, cx);
        } else {
            self.mouse_anchor = Some(offset);
            self.set_mouse_selection(offset, offset, cx);
        }
    }

    fn drag_mouse_selection(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        if !event.dragging() {
            self.mouse_anchor = None;
            return;
        }
        let Some(anchor) = self.mouse_anchor else {
            return;
        };
        let offset = self.offset_for_point(event.position).unwrap_or_else(|| {
            self.last_bounds.map_or(self.editor.len(), |bounds| {
                if event.position.y < bounds.top()
                    || (event.position.y <= bounds.bottom() && event.position.x < bounds.left())
                {
                    0
                } else {
                    self.editor.len()
                }
            })
        });
        self.set_mouse_selection(anchor, offset, cx);
    }

    fn finish_mouse_selection(&mut self, _: &MouseUpEvent, _: &mut Context<Self>) {
        self.mouse_anchor = None;
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
        if self.completion_open && self.editor.mode() == Mode::Insert {
            let action = match key {
                VimKey::Control('j') => {
                    Some(Box::new(crate::app::CompletionNext) as Box<dyn gpui::Action>)
                }
                VimKey::Control('k') => {
                    Some(Box::new(crate::app::CompletionPrevious) as Box<dyn gpui::Action>)
                }
                _ => None,
            };
            if let Some(action) = action {
                window.dispatch_action(action, cx);
                window.prevent_default();
                cx.stop_propagation();
                return;
            }
        }
        if self.completion_open && key == VimKey::Escape {
            return;
        }
        if self.editor.mode() == Mode::Insert && key != VimKey::Escape {
            return;
        }
        if matches!(key, VimKey::Char('p' | 'P')) {
            let item = cx.read_from_clipboard();
            if item.is_none() {
                log::info!("Vim clipboard paste returned no clipboard item");
            }
            let item = if let Some(item) = item {
                let metadata = item.metadata().cloned();
                match self.emit_clipboard_images(item, cx) {
                    Ok(()) => {
                        self.editor.set_paste_text("");
                        let version = self.editor.text_version();
                        if self.editor.send_key(key) {
                            self.finish_vim_action(version, cx);
                        }
                        window.prevent_default();
                        cx.stop_propagation();
                        return;
                    }
                    Err(item) => Some((item, metadata)),
                }
            } else {
                None
            };
            let (text, metadata) = item
                .map(|(item, metadata)| (item.text().unwrap_or_default(), metadata))
                .unwrap_or_default();
            self.editor
                .set_paste_text_with_metadata(&text, metadata.as_deref());
        }
        let version = self.editor.text_version();
        if !self.editor.send_key(key) {
            return;
        }
        self.finish_vim_action(version, cx);
        window.prevent_default();
        cx.stop_propagation();
    }

    fn emit_clipboard_images(
        &self,
        item: ClipboardItem,
        cx: &mut Context<Self>,
    ) -> Result<(), ClipboardItem> {
        if !self.accepts_image_paste
            || !item
                .entries()
                .iter()
                .any(|entry| matches!(entry, ClipboardEntry::Image(_)))
        {
            return Err(item);
        }
        let images = item
            .into_entries()
            .filter_map(|entry| match entry {
                ClipboardEntry::Image(image) => Some(PastedImage {
                    format: image.format,
                    bytes: Arc::new(image.bytes),
                }),
                _ => None,
            })
            .collect::<Vec<_>>();
        let byte_len = images.iter().map(|image| image.bytes.len()).sum::<usize>();
        log::info!(
            "clipboard image paste detected images={} bytes={byte_len}",
            images.len(),
        );
        cx.emit(ComposerImagePaste {
            images: images.into(),
        });
        Ok(())
    }

    fn paste_in_vim_mode(
        &mut self,
        text: &str,
        metadata: Option<&str>,
        key: VimKey,
        cx: &mut Context<Self>,
    ) {
        self.editor.set_paste_text_with_metadata(text, metadata);
        let version = self.editor.text_version();
        if self.editor.send_key(key) {
            self.finish_vim_action(version, cx);
        }
    }

    fn finish_vim_action(&mut self, version: u64, cx: &mut Context<Self>) {
        let yank_revision = self.editor.yank_revision();
        if yank_revision != self.last_yank_revision {
            self.last_yank_revision = yank_revision;
            cx.write_to_clipboard(ClipboardItem::new_string_with_metadata(
                self.editor.yank_text_for_clipboard(),
                self.editor.yank_clipboard_metadata().to_string(),
            ));
        }
        let cursor = self.editor.cursor_offset();
        self.selected = cursor..cursor;
        self.reversed = false;
        self.marked = None;
        self.last_layout.clear();
        if self.editor.text_version() != version {
            self.refresh_syntax();
            cx.emit(ComposerChanged);
        }
        cx.emit(ComposerStateChanged);
        cx.notify();
    }
}

fn message_ref_insertion(source: &str, cursor: usize, reference: &str) -> String {
    let needs_leading_space = cursor
        .checked_sub(1)
        .and_then(|index| source.as_bytes().get(index))
        .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'@');
    let mut insertion = String::with_capacity(reference.len() + 2);
    if needs_leading_space {
        insertion.push(' ');
    }
    insertion.push_str(reference);
    insertion.push(' ');
    insertion
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

fn logical_line_range(text: &str, offset: usize) -> Range<usize> {
    let offset = clamp_offset(text, offset);
    let start = text[..offset].rfind('\n').map_or(0, |index| index + 1);
    let end = text[offset..]
        .find('\n')
        .map_or(text.len(), |index| offset + index);
    start..end
}

fn word_range(text: &str, offset: usize) -> Range<usize> {
    if text.is_empty() {
        return 0..0;
    }
    let mut offset = clamp_offset(text, offset);
    if offset == text.len() {
        offset = text[..offset]
            .char_indices()
            .next_back()
            .map_or(0, |(index, _)| index);
    }
    let Some(ch) = text[offset..].chars().next() else {
        return offset..offset;
    };
    let class = word_class(ch);
    let mut start = offset;
    for (index, candidate) in text[..offset].char_indices().rev() {
        if word_class(candidate) != class {
            break;
        }
        start = index;
    }
    let mut end = offset + ch.len_utf8();
    let suffix_start = end;
    for (index, candidate) in text[suffix_start..].char_indices() {
        if word_class(candidate) != class {
            break;
        }
        end = suffix_start + index + candidate.len_utf8();
    }
    start..end.min(text.len())
}

fn word_class(ch: char) -> u8 {
    if ch.is_whitespace() {
        0
    } else if ch.is_alphanumeric() || ch == '_' {
        1
    } else {
        2
    }
}

fn should_auto_close_code_fence(before: &str, inserted: &str, after: &str) -> bool {
    inserted == "`"
        && before.ends_with("``")
        && !before.ends_with("```")
        && (after.is_empty() || after.starts_with('\n'))
        && !after.starts_with("\n```")
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

impl EntityInputHandler for TextEditor {
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
    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.marked = None;
        cx.emit(ComposerStateChanged);
    }
    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.replace_text(range, text, true, true, window, cx);
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
        self.replace_text(range, text, false, false, window, cx);
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
        cx.emit(ComposerStateChanged);
        cx.notify();
    }

    fn text_length_utf16(&mut self, _: &mut Window, _: &mut Context<Self>) -> Option<usize> {
        Some(self.offset_to_utf16(self.editor.len()))
    }

    fn accepts_text_input(&self, _: &mut Window, _: &mut Context<Self>) -> bool {
        !self.vim_enabled || self.editor.mode() == Mode::Insert
    }
}

impl EventEmitter<ComposerChanged> for TextEditor {}
impl EventEmitter<ComposerStateChanged> for TextEditor {}
impl EventEmitter<ComposerImagePaste> for TextEditor {}

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

fn composer_text_runs(
    syntax: Option<&ComposerSyntax>,
    range: Range<usize>,
    base_font: &Font,
    base_color: Hsla,
    settings: Option<&ResolvedSettings>,
) -> Vec<TextRun> {
    let mut runs = Vec::new();
    let mut cursor = range.start;
    if let Some(syntax) = syntax {
        for styled in syntax.runs_for(range.clone()) {
            if cursor < styled.range.start {
                runs.push(composer_text_run(
                    styled.range.start - cursor,
                    ComposerTextStyle::default(),
                    base_font,
                    base_color,
                    settings,
                ));
            }
            runs.push(composer_text_run(
                styled.range.len(),
                styled.style,
                base_font,
                base_color,
                settings,
            ));
            cursor = styled.range.end;
        }
    }
    if cursor < range.end {
        runs.push(composer_text_run(
            range.end - cursor,
            ComposerTextStyle::default(),
            base_font,
            base_color,
            settings,
        ));
    }
    if runs.is_empty() {
        runs.push(composer_text_run(
            0,
            ComposerTextStyle::default(),
            base_font,
            base_color,
            settings,
        ));
    }
    runs
}

fn composer_text_run(
    len: usize,
    style: ComposerTextStyle,
    base_font: &Font,
    base_color: Hsla,
    settings: Option<&ResolvedSettings>,
) -> TextRun {
    let mut font = base_font.clone();
    if style.typeface == ComposerTypeface::Code {
        font.family = settings
            .map(|settings| settings.fonts.code_family.clone())
            .unwrap_or_else(|| CODE_FONT_FAMILY.into());
    }
    if style.bold {
        font.weight = FontWeight::BOLD;
    }
    if style.italic {
        font.style = FontStyle::Italic;
    }
    let color = match style.color {
        ComposerColor::Default => base_color,
        ComposerColor::Dim => settings
            .map(|settings| settings.theme.color(ThemeRole::SyntaxComment).into())
            .unwrap_or_else(|| rgb(syntax_color(PaletteRole::Comment)).into()),
        ComposerColor::Link => settings
            .map(|settings| settings.theme.color(ThemeRole::TextLink).into())
            .unwrap_or_else(|| rgb(0xf0f0f0).into()),
        ComposerColor::Syntax(role) => settings
            .map(|settings| settings.theme.color(syntax_role(role)).into())
            .unwrap_or_else(|| rgb(syntax_color(role)).into()),
    };
    TextRun {
        len,
        font,
        color,
        background_color: style.code_background.then(|| {
            settings
                .map(|settings| settings.theme.color(ThemeRole::StateInlineCode).into())
                .unwrap_or_else(|| rgba(0xffffff14).into())
        }),
        underline: style.underline.then(|| UnderlineStyle {
            color: Some(color),
            thickness: px(1.),
            ..Default::default()
        }),
        strikethrough: None,
    }
}

struct ComposerElement {
    input: Entity<TextEditor>,
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
        let applied = cx
            .try_global::<AppliedSettings>()
            .map(|settings| settings.0.clone());
        let is_placeholder = input.editor.len() == 0;
        let color = if is_placeholder {
            applied
                .as_ref()
                .map(|settings| settings.theme.color(ThemeRole::TextDim).into())
                .unwrap_or_else(|| rgb(0x747a84).into())
        } else {
            window.text_style().color
        };
        let font = window.text_style().font();
        let font_size = window.text_style().font_size.to_pixels(window.rem_size());
        let shape_line = |range: Range<usize>, text: SharedString| {
            let runs = if is_placeholder {
                vec![TextRun {
                    len: text.len(),
                    font: font.clone(),
                    color,
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                }]
            } else {
                composer_text_runs(
                    input.syntax.as_ref(),
                    range.clone(),
                    &font,
                    color,
                    applied.as_deref(),
                )
            };
            ComposerLine {
                range,
                layout: window
                    .text_system()
                    .shape_line(text, font_size, &runs, None),
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
                            applied
                                .as_ref()
                                .map(|settings| {
                                    settings.theme.color(ThemeRole::StateComposerSelection)
                                })
                                .unwrap_or_else(|| rgba(0x6f8fc044)),
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
                    applied
                        .as_ref()
                        .map(|settings| settings.theme.color(ThemeRole::StateCursorNormal))
                        .unwrap_or_else(|| rgba(0x8ca9d888))
                } else {
                    applied
                        .as_ref()
                        .map(|settings| settings.theme.color(ThemeRole::StateCursorInsert))
                        .unwrap_or_else(|| rgba(0x8ca9d8ff))
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
            if let Err(error) = line.layout.paint(
                origin,
                window.line_height(),
                gpui::TextAlign::Left,
                None,
                window,
                cx,
            ) {
                log::error!("failed to paint composer text: {error:#}");
            }
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

impl Render for TextEditor {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let fonts = cx
            .try_global::<AppliedSettings>()
            .map(|settings| settings.0.fonts.clone());
        let (family, size) = if self.key_context == "ChattComposer" {
            (
                fonts
                    .as_ref()
                    .map(|fonts| fonts.message_family.clone())
                    .unwrap_or_else(|| "IBM Plex Sans".into()),
                fonts.as_ref().map_or(16.0, |fonts| fonts.message_size),
            )
        } else {
            (
                fonts
                    .as_ref()
                    .map(|fonts| fonts.interface_family.clone())
                    .unwrap_or_else(|| ".SystemUIFont".into()),
                fonts.as_ref().map_or(16.0, |fonts| fonts.interface_size),
            )
        };
        let key_context = if self.key_context != "ChattComposer" {
            self.key_context
        } else if !self.vim_enabled || self.editor.mode() == Mode::Insert {
            if self.completion_engaged {
                "ChattComposer ComposerInsert CompletionOpen CompletionEngaged"
            } else if self.completion_open {
                "ChattComposer ComposerInsert CompletionOpen"
            } else {
                "ChattComposer ComposerInsert"
            }
        } else {
            "ChattComposer VimMode"
        };
        div()
            .flex()
            .items_center()
            .font_family(family)
            .text_size(px(size))
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
                    this.begin_mouse_selection(event, window, cx);
                }),
            )
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                this.drag_mouse_selection(event, cx)
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, event: &MouseUpEvent, _, cx| {
                    this.finish_mouse_selection(event, cx)
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, event: &MouseUpEvent, _, cx| {
                    this.finish_mouse_selection(event, cx)
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

impl Focusable for TextEditor {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use gpui::{
        Context, Entity, Focusable, IntoElement, MouseButton, Render, Window, div, point,
        prelude::*,
    };

    use super::{
        Composer, ComposerImagePaste, ComposerLine, line_for_offset, logical_line_range,
        logical_lines, message_ref_insertion, normalize_range, range_from_utf16,
        should_auto_close_code_fence, visible_line_range, word_range,
    };

    struct CompletionKeyHarness {
        composer: Entity<Composer>,
        actions: Rc<RefCell<Vec<&'static str>>>,
    }

    impl Render for CompletionKeyHarness {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .on_action(cx.listener(|this, _: &crate::app::CompletionNext, _, _| {
                    this.actions.borrow_mut().push("next")
                }))
                .on_action(
                    cx.listener(|this, _: &crate::app::CompletionPrevious, _, _| {
                        this.actions.borrow_mut().push("previous")
                    }),
                )
                .on_action(cx.listener(|this, _: &crate::app::CompletionAccept, _, _| {
                    this.actions.borrow_mut().push("accept")
                }))
                .on_action(
                    cx.listener(|this, _: &crate::app::CompletionAcceptEngaged, _, _| {
                        this.actions.borrow_mut().push("accept-engaged")
                    }),
                )
                .on_action(cx.listener(|this, _: &crate::app::SendMessage, _, _| {
                    this.actions.borrow_mut().push("send")
                }))
                .child(self.composer.clone())
        }
    }

    #[gpui::test]
    fn focused_composer_dispatches_completion_navigation_and_acceptance(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(crate::fonts::init);
        cx.update(|cx| {
            crate::key_bindings::install(&crate::config::schema::GuiConfig::default(), cx).unwrap()
        });
        let actions = Rc::new(RefCell::new(Vec::new()));
        let (_, cx) = cx.add_window_view({
            let actions = actions.clone();
            move |window, cx| {
                let composer = cx.new(|cx| {
                    let mut composer = Composer::new(cx);
                    composer.set_completion_state(true, true);
                    composer
                });
                window.focus(&composer.focus_handle(cx), cx);
                CompletionKeyHarness { composer, actions }
            }
        });

        cx.simulate_keystrokes("down up ctrl-j ctrl-k tab enter");

        assert_eq!(
            actions.borrow().as_slice(),
            [
                "next",
                "previous",
                "next",
                "previous",
                "accept",
                "accept-engaged"
            ]
        );
    }

    #[gpui::test]
    fn platform_paste_shortcut_and_normal_p_read_the_system_clipboard(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(crate::fonts::init);
        cx.update(|cx| {
            crate::key_bindings::install(&crate::config::schema::GuiConfig::default(), cx).unwrap()
        });
        let (composer, cx) = cx.add_window_view(|window, cx| {
            let composer = Composer::new(cx);
            window.focus(&composer.focus, cx);
            composer
        });

        cx.write_to_clipboard(gpui::ClipboardItem::new_string("first".to_string()));
        cx.simulate_keystrokes("secondary-v");
        assert_eq!(
            composer.read_with(cx, |composer, _| composer.text()),
            "first"
        );

        cx.simulate_keystrokes("escape");
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(" second".to_string()));
        cx.simulate_keystrokes("p");
        assert_eq!(
            composer.read_with(cx, |composer, _| composer.text()),
            "first second"
        );
    }

    #[gpui::test]
    fn visual_p_replaces_the_selection_with_system_clipboard_text(cx: &mut gpui::TestAppContext) {
        cx.update(crate::fonts::init);
        cx.update(|cx| {
            crate::key_bindings::install(&crate::config::schema::GuiConfig::default(), cx).unwrap()
        });
        let (composer, cx) = cx.add_window_view(|window, cx| {
            let mut composer = Composer::new(cx);
            composer.set_value("alpha beta", cx);
            window.focus(&composer.focus, cx);
            composer
        });

        cx.write_to_clipboard(gpui::ClipboardItem::new_string("clipboard".into()));
        cx.simulate_keystrokes("v e p");

        assert_eq!(
            composer.read_with(cx, |composer, _| composer.text()),
            "clipboard beta"
        );
        assert_eq!(
            cx.read_from_clipboard()
                .and_then(|item| item.text())
                .as_deref(),
            Some("clipboard")
        );
    }

    #[gpui::test]
    fn image_paste_emits_memory_payload_for_shortcut_and_normal_p(cx: &mut gpui::TestAppContext) {
        cx.update(crate::fonts::init);
        cx.update(|cx| {
            crate::key_bindings::install(&crate::config::schema::GuiConfig::default(), cx).unwrap()
        });
        let (composer, cx) = cx.add_window_view(|window, cx| {
            let composer = Composer::new(cx);
            window.focus(&composer.focus, cx);
            composer
        });
        let clipboard_item = || gpui::ClipboardItem {
            entries: vec![
                gpui::ClipboardEntry::from("text fallback".to_string()),
                gpui::ClipboardEntry::Image(gpui::Image {
                    format: gpui::ImageFormat::Png,
                    bytes: vec![1, 2, 3],
                    id: 7,
                }),
            ],
        };
        let events = Rc::new(RefCell::new(Vec::<ComposerImagePaste>::new()));
        let _subscription = cx.update({
            let composer = composer.clone();
            let events = events.clone();
            move |_, cx| {
                cx.subscribe(&composer, move |_, event: &ComposerImagePaste, _| {
                    events.borrow_mut().push(event.clone())
                })
            }
        });

        cx.write_to_clipboard(clipboard_item());
        cx.simulate_keystrokes("secondary-v");
        let shortcut_event = events.borrow()[0].clone();
        assert_eq!(shortcut_event.images.len(), 1);
        assert_eq!(shortcut_event.images[0].format, gpui::ImageFormat::Png);
        assert_eq!(shortcut_event.images[0].bytes.as_slice(), &[1, 2, 3]);
        assert_eq!(composer.read_with(cx, |composer, _| composer.text()), "");

        cx.simulate_keystrokes("escape");
        cx.write_to_clipboard(clipboard_item());
        cx.simulate_keystrokes("p");
        let normal_event = events.borrow()[1].clone();
        assert_eq!(normal_event.images[0].bytes.as_slice(), &[1, 2, 3]);
        assert_eq!(composer.read_with(cx, |composer, _| composer.text()), "");
    }

    #[gpui::test]
    fn text_only_composer_uses_text_fallback_from_mixed_clipboard_item(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(crate::fonts::init);
        cx.update(|cx| {
            crate::key_bindings::install(&crate::config::schema::GuiConfig::default(), cx).unwrap()
        });
        let (composer, cx) = cx.add_window_view(|window, cx| {
            let composer = Composer::search(cx);
            window.focus(&composer.focus, cx);
            composer
        });
        cx.write_to_clipboard(gpui::ClipboardItem {
            entries: vec![
                gpui::ClipboardEntry::from("fallback".to_string()),
                gpui::ClipboardEntry::Image(gpui::Image {
                    format: gpui::ImageFormat::Png,
                    bytes: vec![1, 2, 3],
                    id: 8,
                }),
            ],
        });

        cx.simulate_keystrokes("secondary-v");

        assert_eq!(
            composer.read_with(cx, |composer, _| composer.text()),
            "fallback"
        );
    }

    #[gpui::test]
    fn message_composer_expands_a_completed_emoji_shortcode(cx: &mut gpui::TestAppContext) {
        cx.update(crate::fonts::init);
        cx.update(|cx| {
            crate::key_bindings::install(&crate::config::schema::GuiConfig::default(), cx).unwrap()
        });
        let (composer, cx) = cx.add_window_view(|window, cx| {
            let composer = Composer::new(cx);
            window.focus(&composer.focus, cx);
            composer
        });

        cx.simulate_keystrokes(": s m i l e :");

        assert_eq!(composer.read_with(cx, |composer, _| composer.text()), "😄");
        assert_eq!(
            composer.read_with(cx, |composer, _| composer.snapshot().selection),
            "😄".len().."😄".len()
        );
    }

    #[gpui::test]
    fn secondary_text_inputs_keep_emoji_shortcodes_literal(cx: &mut gpui::TestAppContext) {
        cx.update(crate::fonts::init);
        cx.update(|cx| {
            crate::key_bindings::install(&crate::config::schema::GuiConfig::default(), cx).unwrap()
        });
        let (editor, cx) = cx.add_window_view(|window, cx| {
            let editor = Composer::search(cx);
            window.focus(&editor.focus, cx);
            editor
        });

        cx.simulate_keystrokes(": s m i l e :");

        assert_eq!(editor.read_with(cx, |editor, _| editor.text()), ":smile:");
    }

    #[gpui::test]
    fn vim_yanks_and_deletes_replace_the_system_clipboard(cx: &mut gpui::TestAppContext) {
        cx.update(crate::fonts::init);
        cx.update(|cx| {
            crate::key_bindings::install(&crate::config::schema::GuiConfig::default(), cx).unwrap()
        });
        let (composer, cx) = cx.add_window_view(|window, cx| {
            let mut composer = Composer::new(cx);
            composer.set_value("alpha beta", cx);
            window.focus(&composer.focus, cx);
            composer
        });

        cx.write_to_clipboard(gpui::ClipboardItem::new_string("before".into()));
        cx.simulate_keystrokes("y i w");
        assert_eq!(
            cx.read_from_clipboard()
                .and_then(|item| item.text())
                .as_deref(),
            Some("alpha")
        );

        cx.simulate_keystrokes("w d i w");
        assert_eq!(
            cx.read_from_clipboard()
                .and_then(|item| item.text())
                .as_deref(),
            Some("beta")
        );
        assert_eq!(
            composer.read_with(cx, |composer, _| composer.text()),
            "alpha "
        );
    }

    #[gpui::test]
    fn vim_clipboard_preserves_linewise_paste_shape(cx: &mut gpui::TestAppContext) {
        cx.update(crate::fonts::init);
        cx.update(|cx| {
            crate::key_bindings::install(&crate::config::schema::GuiConfig::default(), cx).unwrap()
        });
        let (composer, cx) = cx.add_window_view(|window, cx| {
            let mut composer = Composer::new(cx);
            composer.set_value("one\ntwo", cx);
            window.focus(&composer.focus, cx);
            composer
        });

        cx.simulate_keystrokes("d d");
        let clipboard = cx
            .read_from_clipboard()
            .expect("delete populates clipboard");
        assert_eq!(clipboard.text().as_deref(), Some("one"));
        assert_eq!(
            clipboard.metadata().map(String::as_str),
            Some("chatt-vim-register:linewise")
        );

        cx.simulate_keystrokes("p");
        assert_eq!(
            composer.read_with(cx, |composer, _| composer.text()),
            "two\none"
        );
    }

    #[gpui::test]
    fn mouse_drag_selects_text_for_platform_copy(cx: &mut gpui::TestAppContext) {
        cx.update(crate::fonts::init);
        cx.update(|cx| {
            crate::key_bindings::install(&crate::config::schema::GuiConfig::default(), cx).unwrap()
        });
        let (editor, cx) = cx.add_window_view(|window, cx| {
            let mut editor = Composer::settings_input(
                "Edit value",
                crate::config::schema::BindingMode::Standard,
                cx,
            );
            editor.set_value("alpha beta", cx);
            window.focus(&editor.focus, cx);
            editor
        });
        let (start, end) = editor.read_with(cx, |editor, _| {
            let bounds = editor.last_bounds.expect("editor has been painted");
            let line = &editor.last_layout[0];
            (
                point(
                    bounds.left() + line.layout.x_for_index(0),
                    bounds.center().y,
                ),
                point(
                    bounds.left() + line.layout.x_for_index("alpha".len()),
                    bounds.center().y,
                ),
            )
        });

        cx.simulate_mouse_down(start, MouseButton::Left, gpui::Modifiers::default());
        cx.simulate_mouse_move(end, MouseButton::Left, gpui::Modifiers::default());
        cx.simulate_mouse_up(end, MouseButton::Left, gpui::Modifiers::default());
        assert_eq!(
            editor.read_with(cx, |editor, _| editor.snapshot().selection),
            0.."alpha".len()
        );

        cx.simulate_keystrokes("secondary-c");
        assert_eq!(
            cx.read_from_clipboard()
                .and_then(|item| item.text())
                .as_deref(),
            Some("alpha")
        );
    }

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
    fn mouse_word_and_line_ranges_follow_text_boundaries() {
        assert_eq!(word_range("éclair ++", 2), 0.."éclair".len());
        assert_eq!(word_range("éclair ++", "éclair ".len()), 8..10);
        assert_eq!(word_range("alpha", "alpha".len()), 0..5);
        assert_eq!(logical_line_range("one\ntwo\nthree", 5), 4..7);
        assert_eq!(logical_line_range("one\ntwo\n", 8), 8..8);
    }

    #[test]
    fn bounds_the_shaped_viewport_for_ten_thousand_line_messages() {
        assert_eq!(visible_line_range(10_000, 0), 0..8);
        assert_eq!(visible_line_range(10_000, 5_000), 4_993..5_001);
        assert_eq!(visible_line_range(10_000, 9_999), 9_992..10_000);
    }

    #[test]
    fn typed_third_backtick_auto_closes_only_at_a_line_tail() {
        assert!(should_auto_close_code_fence("``", "`", ""));
        assert!(should_auto_close_code_fence("Hello ``", "`", "\nnext"));
        assert!(!should_auto_close_code_fence("```", "`", ""));
        assert!(!should_auto_close_code_fence("``", "`", "tail"));
        assert!(!should_auto_close_code_fence("``", "```", ""));
        assert!(!should_auto_close_code_fence("``", "`", "\n```"));
    }

    #[test]
    fn message_reference_insertion_preserves_token_boundaries() {
        let reference = "@@0410800";
        assert_eq!(message_ref_insertion("before", 6, reference), " @@0410800 ");
        assert_eq!(message_ref_insertion("(", 1, reference), "@@0410800 ");
        assert_eq!(message_ref_insertion("@", 1, reference), " @@0410800 ");
    }
}
