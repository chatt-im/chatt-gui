use std::{cell::RefCell, collections::VecDeque, ops::Range, rc::Rc, sync::Arc};

use gpui::{
    App, AvailableSpace, Bounds, ClipboardEntry, ClipboardItem, ContentMask, Context, CursorStyle,
    Element, ElementId, ElementInputHandler, Entity, EntityInputHandler, EventEmitter, FocusHandle,
    Focusable, Font, FontStyle, FontWeight, GlobalElementId, Hsla, KeyDownEvent, LayoutId,
    LineLayout, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Rems,
    SharedString, Style, TextRun, UTF16Selection, UnderlineStyle, Window, WrappedLine, actions,
    div, fill, point, prelude::*, px, relative, rgb, rgba, size,
};
use unicode_segmentation::UnicodeSegmentation;

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
    ui_scale::rems_from_px,
};
use chatt_message_format::highlight::PaletteRole;
use highlight::{ComposerColor, ComposerSyntax, ComposerTextStyle, ComposerTypeface};
pub(crate) use mode::Mode;
use vim::{DisplayLine as VimDisplayLine, DisplayPoint as VimDisplayPoint, VimEditor, VimKey};

const MAX_VISIBLE_LINES: usize = 8;
pub(crate) const MIN_COMPOSER_HEIGHT: f32 = 64.0;

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
    min_height: Rems,
    selected: Range<usize>,
    reversed: bool,
    mouse_anchor: Option<usize>,
    last_yank_revision: u64,
    marked: Option<Range<usize>>,
    completion_open: bool,
    completion_engaged: bool,
    expand_emoji_shortcodes: bool,
    last_layout: Option<ComposerGeometry>,
    layout_width: Option<Pixels>,
    layout_invalidated: bool,
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
            min_height: rems_from_px(MIN_COMPOSER_HEIGHT),
            selected: 0..0,
            reversed: false,
            mouse_anchor: None,
            last_yank_revision: 0,
            marked: None,
            completion_open: false,
            completion_engaged: false,
            expand_emoji_shortcodes: true,
            last_layout: None,
            layout_width: None,
            layout_invalidated: false,
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
            min_height: rems_from_px(28.),
            selected: 0..0,
            reversed: false,
            mouse_anchor: None,
            last_yank_revision: 0,
            marked: None,
            completion_open: false,
            completion_engaged: false,
            expand_emoji_shortcodes: false,
            last_layout: None,
            layout_width: None,
            layout_invalidated: false,
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
        self.last_layout = None;
        self.layout_invalidated = true;
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
        self.last_layout = None;
        self.layout_invalidated = true;
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
        self.last_layout = None;
        self.layout_invalidated = true;
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
        self.layout_invalidated = true;
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
        self.layout_invalidated = true;
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
        self.layout_invalidated = true;
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
            #[cfg(feature = "diagnostic-logs")]
            if crate::logger::media_logging_enabled() {
                kvlog::info!("clipboard paste returned no item", group = "media");
            }
            return;
        };
        let metadata = item.metadata().cloned();
        let item = match self.emit_clipboard_images(item, cx) {
            Ok(()) => return,
            Err(item) => item,
        };
        let Some(text) = item.text() else {
            #[cfg(feature = "diagnostic-logs")]
            if crate::logger::media_logging_enabled() {
                kvlog::info!("clipboard paste contained no usable data", group = "media");
            }
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
        self.last_layout = None;
        self.layout_invalidated = true;
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
        self.last_layout.as_ref()?.offset_for_point(point)
    }

    fn set_mouse_selection(&mut self, anchor: usize, head: usize, cx: &mut Context<Self>) {
        let anchor = self.clamp_offset(anchor);
        let head = self.clamp_offset(head);
        self.selected = anchor.min(head)..anchor.max(head);
        self.reversed = head < anchor;
        self.editor.set_cursor_offset(head);
        self.marked = None;
        self.layout_invalidated = true;
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
            self.last_layout
                .as_ref()
                .map(|layout| layout.bounds)
                .map_or(self.editor.len(), |bounds| {
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
                #[cfg(feature = "diagnostic-logs")]
                if crate::logger::media_logging_enabled() {
                    kvlog::info!("Vim clipboard paste returned no item", group = "media");
                }
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
        if self.editor.send_key(key) {
            self.finish_vim_action(version, cx);
        }
        // Reaching here means the keystroke began outside Insert mode (or was
        // Escape while in Insert mode). Even keys this Vim implementation does
        // not handle must not fall through to the platform text input.
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
        #[cfg(feature = "diagnostic-logs")]
        if crate::logger::media_logging_enabled() {
            let byte_len = images.iter().map(|image| image.bytes.len()).sum::<usize>();
            kvlog::info!(
                "clipboard image paste detected",
                group = "media",
                count = images.len(),
                size = byte_len
            );
        }
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
        self.last_layout = None;
        self.layout_invalidated = true;
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
        _: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let range = self.range_from_utf16(&range);
        self.last_layout.as_ref()?.bounds_for_range(range)
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
        self.layout_invalidated = true;
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

#[derive(Clone, Debug)]
struct ComposerVisualLine {
    range: Range<usize>,
    logical_start: usize,
    logical_end: usize,
    layout: Arc<LineLayout>,
    unwrapped_start_x: Pixels,
    is_last_in_logical_line: bool,
}

impl ComposerVisualLine {
    fn local_range(&self) -> Range<usize> {
        self.range.start.saturating_sub(self.logical_start)
            ..self.range.end.saturating_sub(self.logical_start)
    }

    fn local_offset(&self, offset: usize) -> usize {
        let range = self.local_range();
        offset
            .saturating_sub(self.logical_start)
            .clamp(range.start, range.end)
    }

    fn x_for_offset(&self, offset: usize) -> Pixels {
        self.layout.x_for_index(self.local_offset(offset)) - self.unwrapped_start_x
    }
}

#[derive(Clone, Debug)]
struct ComposerGeometry {
    bounds: Bounds<Pixels>,
    text_bounds: Bounds<Pixels>,
    line_height: Pixels,
    horizontal_scroll: Pixels,
    lines: Vec<ComposerVisualLine>,
}

impl ComposerGeometry {
    fn line_index_for_offset(&self, offset: usize) -> Option<usize> {
        self.lines.iter().position(|line| {
            offset >= line.range.start
                && (offset < line.range.end
                    || (line.is_last_in_logical_line && offset <= line.range.end))
        })
    }

    fn line_index_for_range_end(&self, offset: usize) -> Option<usize> {
        self.lines.iter().position(|line| {
            offset > line.range.start && offset <= line.range.end
                || (line.range.is_empty()
                    && line.is_last_in_logical_line
                    && offset == line.range.end)
        })
    }

    fn screen_x_for_offset(&self, line: &ComposerVisualLine, offset: usize) -> Pixels {
        self.text_bounds.left() - self.horizontal_scroll + line.x_for_offset(offset)
    }

    fn offset_for_point(&self, point: gpui::Point<Pixels>) -> Option<usize> {
        if !self.bounds.contains(&point) {
            return None;
        }
        let line_count = self.lines.len();
        if line_count == 0 {
            return None;
        }
        let row = if point.y <= self.text_bounds.top() {
            0
        } else if point.y >= self.text_bounds.bottom() {
            line_count - 1
        } else {
            ((point.y - self.text_bounds.top()) / self.line_height)
                .floor()
                .max(0.) as usize
        }
        .min(line_count - 1);
        let line = &self.lines[row];
        let local_range = line.local_range();
        let shaped_x =
            point.x - self.text_bounds.left() + self.horizontal_scroll + line.unwrapped_start_x;
        let local = line
            .layout
            .closest_index_for_x(shaped_x)
            .clamp(local_range.start, local_range.end);
        Some(line.logical_start + local)
    }

    fn bounds_for_range(&self, range: Range<usize>) -> Option<Bounds<Pixels>> {
        let first = self.lines.first()?;
        let last = self.lines.last()?;
        let start_line = self
            .line_index_for_offset(range.start)
            .or_else(|| (range.start <= first.range.start).then_some(0))?;
        let end_line = if range.is_empty() {
            self.line_index_for_offset(range.end)
        } else {
            self.line_index_for_range_end(range.end)
                .or_else(|| self.line_index_for_offset(range.end))
        }
        .or_else(|| (range.end >= last.range.end).then_some(self.lines.len() - 1))?;
        let start = &self.lines[start_line];
        let end = &self.lines[end_line];
        if start_line == end_line {
            Some(Bounds::from_corners(
                point(
                    self.screen_x_for_offset(start, range.start),
                    self.text_bounds.top() + self.line_height * start_line as f32,
                ),
                point(
                    self.screen_x_for_offset(end, range.end),
                    self.text_bounds.top() + self.line_height * (end_line + 1) as f32,
                ),
            ))
        } else {
            Some(Bounds::from_corners(
                point(
                    self.text_bounds.left(),
                    self.text_bounds.top() + self.line_height * start_line as f32,
                ),
                point(
                    self.text_bounds.right(),
                    self.text_bounds.top() + self.line_height * (end_line + 1) as f32,
                ),
            ))
        }
    }
}

struct ComposerLogicalLine {
    range: Range<usize>,
    layout: WrappedLine,
    first_visual_line: usize,
    visual_line_count: usize,
}

struct ComposerLayout {
    logical_lines: Vec<ComposerLogicalLine>,
    visual_lines: Vec<ComposerVisualLine>,
    visible_lines: Range<usize>,
    line_height: Pixels,
    vertical_inset: Pixels,
    horizontal_scroll: Pixels,
    width: Pixels,
    font_size: Pixels,
}

impl ComposerLayout {
    fn height(&self) -> Pixels {
        self.vertical_inset * 2.
            + self.line_height * self.visible_lines.len().max(1) as f32
    }

    fn geometry(&self, bounds: Bounds<Pixels>) -> ComposerGeometry {
        let text_bounds = Bounds::new(
            point(bounds.left(), bounds.top() + self.vertical_inset),
            size(
                bounds.size.width,
                self.line_height * self.visible_lines.len().max(1) as f32,
            ),
        );
        ComposerGeometry {
            bounds,
            text_bounds,
            line_height: self.line_height,
            horizontal_scroll: self.horizontal_scroll,
            lines: self.visual_lines[self.visible_lines.clone()].to_vec(),
        }
    }

    fn vim_display_lines(&self, input: &TextEditor) -> Vec<VimDisplayLine> {
        self.visual_lines
            .iter()
            .map(|line| {
                let mut points = input
                    .editor
                    .slice(line.range.clone())
                    .grapheme_indices(true)
                    .map(|(index, _)| {
                        let offset = line.range.start + index;
                        VimDisplayPoint {
                            offset,
                            x: f32::from(line.x_for_offset(offset)),
                        }
                    })
                    .collect::<Vec<_>>();
                if points
                    .first()
                    .is_none_or(|point| point.offset != line.range.start)
                {
                    points.insert(
                        0,
                        VimDisplayPoint {
                            offset: line.range.start,
                            x: f32::from(line.x_for_offset(line.range.start)),
                        },
                    );
                }
                if line.is_last_in_logical_line
                    && points
                        .last()
                        .is_none_or(|point| point.offset != line.range.end)
                {
                    points.push(VimDisplayPoint {
                        offset: line.range.end,
                        x: f32::from(line.x_for_offset(line.range.end)),
                    });
                }
                VimDisplayLine {
                    range: line.range.clone(),
                    logical_end: line.logical_end,
                    is_last_in_logical_line: line.is_last_in_logical_line,
                    points,
                }
            })
            .collect()
    }
}

#[derive(Clone, Default)]
struct ComposerLayoutState(Rc<RefCell<Option<ComposerLayout>>>);

#[derive(Clone)]
struct ComposerLayoutStyle {
    applied: Option<Arc<ResolvedSettings>>,
    font: Font,
    font_size: Pixels,
    line_height: Pixels,
    base_color: Hsla,
    min_height: Pixels,
    cursor_width: Pixels,
}

impl ComposerLayoutStyle {
    fn capture(input: &TextEditor, window: &Window, cx: &App) -> Self {
        Self {
            applied: cx
                .try_global::<AppliedSettings>()
                .map(|settings| settings.0.clone()),
            font: window.text_style().font(),
            font_size: window.text_style().font_size.to_pixels(window.rem_size()),
            line_height: window.line_height(),
            base_color: window.text_style().color,
            min_height: input.min_height.to_pixels(window.rem_size()),
            cursor_width: crate::ui_scale::scaled_px(2.0, window.rem_size()),
        }
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

fn build_composer_layout(
    input: &TextEditor,
    available_width: Option<Pixels>,
    style: &ComposerLayoutStyle,
    window: &mut Window,
) -> ComposerLayout {
    let is_placeholder = input.editor.len() == 0;
    let color = if is_placeholder {
        style
            .applied
            .as_ref()
            .map(|settings| settings.theme.color(ThemeRole::TextDim).into())
            .unwrap_or_else(|| rgb(0x747a84).into())
    } else {
        style.base_color
    };
    let wrap_width = if input.multiline && !is_placeholder {
        available_width.map(|width| if width > px(1.) { width } else { px(1.) })
    } else {
        None
    };

    let shape_line = |row: usize, range: Range<usize>, text: SharedString| {
        let runs = if is_placeholder {
            vec![TextRun {
                len: text.len(),
                font: style.font.clone(),
                color,
                background_color: None,
                underline: None,
                strikethrough: None,
            }]
        } else {
            composer_text_runs(
                input.syntax.as_ref(),
                range.clone(),
                &style.font,
                color,
                style.applied.as_deref(),
            )
        };
        let layout =
            match window
                .text_system()
                .shape_text(text, style.font_size, &runs, wrap_width, None)
            {
                Ok(mut lines) => lines.pop().unwrap_or_default(),
                Err(error) => {
                    kvlog::error!("failed to shape composer text", row, err = %error);
                    WrappedLine::default()
                }
            };
        ComposerLogicalLine {
            range,
            layout,
            first_visual_line: 0,
            visual_line_count: 0,
        }
    };

    let mut logical_lines = VecDeque::new();
    if is_placeholder {
        logical_lines.push_back(shape_line(0, 0..0, input.placeholder.clone()));
    } else {
        let cursor_row = input.editor.offset_to_rowcol(input.cursor()).0;
        let line_for_row = |row| {
            let start = input.editor.line_start(row);
            let text = input.editor.line(row);
            let range = start..start + text.len();
            shape_line(row, range, text.into_owned().into())
        };
        logical_lines.push_back(line_for_row(cursor_row));
        let mut preceding_visual_count = 0;
        let mut first_row = cursor_row;
        while preceding_visual_count < MAX_VISIBLE_LINES && first_row > 0 {
            first_row -= 1;
            let line = line_for_row(first_row);
            preceding_visual_count += line.layout.wrap_boundaries().len() + 1;
            logical_lines.push_front(line);
        }
        let mut following_visual_count = 0;
        let mut next_row = cursor_row + 1;
        while following_visual_count < MAX_VISIBLE_LINES && next_row < input.editor.line_count() {
            let line = line_for_row(next_row);
            following_visual_count += line.layout.wrap_boundaries().len() + 1;
            logical_lines.push_back(line);
            next_row += 1;
        }
    }

    let mut logical_lines = logical_lines.into_iter().collect::<Vec<_>>();
    let mut visual_lines = Vec::new();
    for logical in &mut logical_lines {
        logical.first_visual_line = visual_lines.len();
        let mut starts = Vec::with_capacity(logical.layout.wrap_boundaries().len() + 1);
        starts.push(0);
        for boundary in logical.layout.wrap_boundaries() {
            let Some(run) = logical.layout.runs().get(boundary.run_ix) else {
                continue;
            };
            let Some(glyph) = run.glyphs.get(boundary.glyph_ix) else {
                continue;
            };
            starts.push(glyph.index);
        }
        starts.sort_unstable();
        starts.dedup();
        let local_end = logical.range.len();
        for (index, start) in starts.iter().copied().enumerate() {
            let end = starts.get(index + 1).copied().unwrap_or(local_end);
            visual_lines.push(ComposerVisualLine {
                range: logical.range.start + start..logical.range.start + end,
                logical_start: logical.range.start,
                logical_end: logical.range.end,
                layout: logical.layout.unwrapped_layout.clone(),
                unwrapped_start_x: logical.layout.unwrapped_layout.x_for_index(start),
                is_last_in_logical_line: index + 1 == starts.len(),
            });
        }
        logical.visual_line_count = starts.len().max(1);
    }

    if visual_lines.is_empty() {
        visual_lines.push(ComposerVisualLine {
            range: 0..0,
            logical_start: 0,
            logical_end: 0,
            layout: Arc::new(LineLayout::default()),
            unwrapped_start_x: Pixels::ZERO,
            is_last_in_logical_line: true,
        });
    }
    let cursor = input.cursor();
    let cursor_visual_line = visual_lines
        .iter()
        .position(|line| {
            cursor >= line.range.start
                && (cursor < line.range.end
                    || (line.is_last_in_logical_line && cursor <= line.range.end))
        })
        .unwrap_or_else(|| visual_lines.len() - 1);
    let visible_lines = visible_line_range(visual_lines.len(), cursor_visual_line);
    let natural_width = logical_lines
        .iter()
        .map(|line| line.layout.width())
        .fold(Pixels::ZERO, |width, line_width| width.max(line_width));
    let width = available_width.unwrap_or(natural_width);
    let horizontal_scroll = if input.multiline {
        Pixels::ZERO
    } else {
        let cursor_x = visual_lines[cursor_visual_line].x_for_offset(cursor);
        let cursor_right = cursor_x + style.cursor_width;
        if cursor_right > width {
            cursor_right - width
        } else {
            Pixels::ZERO
        }
    };
    let vertical_inset = if style.min_height > style.line_height {
        (style.min_height - style.line_height) / 2.
    } else {
        Pixels::ZERO
    };

    ComposerLayout {
        logical_lines,
        visual_lines,
        visible_lines,
        line_height: style.line_height,
        vertical_inset,
        horizontal_scroll,
        width,
        font_size: style.font_size,
    }
}

struct ComposerElement {
    input: Entity<TextEditor>,
}
struct Prepaint {
    geometry: ComposerGeometry,
    cursor: Option<PaintQuad>,
    selection: Vec<PaintQuad>,
    needs_relayout: bool,
}
impl IntoElement for ComposerElement {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}
impl Element for ComposerElement {
    type RequestLayoutState = ComposerLayoutState;
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
    ) -> (LayoutId, ComposerLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        let state = ComposerLayoutState::default();
        let input = self.input.clone();
        let (layout_style, width_hint) = {
            let input_ref = input.read(cx);
            (
                ComposerLayoutStyle::capture(input_ref, window, cx),
                input_ref
                    .layout_invalidated
                    .then_some(input_ref.layout_width)
                    .flatten(),
            )
        };
        let layout_id = if let Some(width) = width_hint {
            let layout = build_composer_layout(input.read(cx), Some(width), &layout_style, window);
            style.size.height = layout.height().into();
            state.0.borrow_mut().replace(layout);
            window.request_layout(style, [], cx)
        } else {
            let measured_state = state.clone();
            window.request_measured_layout(
                style,
                move |known_dimensions, available_space, window, cx| {
                    let available_width = known_dimensions.width.or(match available_space.width {
                        AvailableSpace::Definite(width) => Some(width),
                        AvailableSpace::MinContent | AvailableSpace::MaxContent => None,
                    });
                    let layout = build_composer_layout(
                        input.read(cx),
                        available_width,
                        &layout_style,
                        window,
                    );
                    let measured = size(
                        available_width.unwrap_or(layout.width),
                        layout.height(),
                    );
                    measured_state.0.borrow_mut().replace(layout);
                    measured
                },
            )
        };
        (layout_id, state)
    }
    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        layout_state: &mut ComposerLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Prepaint {
        let input = self.input.read(cx);
        let applied = cx
            .try_global::<AppliedSettings>()
            .map(|settings| settings.0.clone());
        let needs_width_refresh = layout_state
            .0
            .borrow()
            .as_ref()
            .is_none_or(|layout| layout.width != bounds.size.width);
        if needs_width_refresh {
            let style = ComposerLayoutStyle::capture(input, window, cx);
            layout_state.0.borrow_mut().replace(build_composer_layout(
                input,
                Some(bounds.size.width),
                &style,
                window,
            ));
        }
        let layout = layout_state.0.borrow();
        let layout = layout
            .as_ref()
            .expect("composer layout is measured before prepaint");
        let needs_relayout =
            (f32::from(layout.height()) - f32::from(bounds.size.height)).abs() > 0.5;
        let font_size = layout.font_size;
        let geometry = layout.geometry(bounds);
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
                geometry
                    .lines
                    .iter()
                    .enumerate()
                    .filter_map(|(row, line)| {
                        let selects_text =
                            selected.start < line.range.end && selected.end > line.range.start;
                        let selects_newline = line.is_last_in_logical_line
                            && line.logical_end < input.editor.len()
                            && selected.start <= line.logical_end
                            && selected.end > line.logical_end;
                        if !selects_text && !selects_newline {
                            return None;
                        }
                        let left = geometry.screen_x_for_offset(line, selected.start);
                        let mut right = geometry.screen_x_for_offset(line, selected.end);
                        if selects_newline {
                            right += font_size * 0.25;
                        }
                        let top = geometry.text_bounds.top() + geometry.line_height * row as f32;
                        Some(fill(
                            Bounds::from_corners(
                                point(left, top),
                                point(right, top + geometry.line_height),
                            ),
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
        let cursor_line_offset = geometry
            .line_index_for_offset(input.cursor())
            .expect("the measured composer viewport contains its cursor");
        let cursor_line = &geometry.lines[cursor_line_offset];
        let cursor_x = geometry.screen_x_for_offset(cursor_line, input.cursor());
        let cursor_width = if input.vim_enabled && input.editor.mode() != Mode::Insert {
            let next = input.editor.next_offset(input.cursor());
            let width = if next <= cursor_line.range.end {
                geometry.screen_x_for_offset(cursor_line, next) - cursor_x
            } else {
                font_size * 0.5
            };
            if width > font_size * 0.125 {
                width
            } else {
                font_size * 0.5
            }
        } else {
            crate::ui_scale::scaled_px(2.0, window.rem_size())
        };
        let cursor = (input.vim_enabled || input.selected.is_empty()).then(|| {
            fill(
                Bounds::new(
                    point(
                        cursor_x,
                        geometry.text_bounds.top()
                            + geometry.line_height * cursor_line_offset as f32,
                    ),
                    size(cursor_width, geometry.line_height),
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
            geometry,
            cursor,
            selection,
            needs_relayout,
        }
    }
    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        layout_state: &mut ComposerLayoutState,
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
        let layout = layout_state.0.borrow();
        let layout = layout
            .as_ref()
            .expect("composer layout remains available through paint");
        window.with_content_mask(
            Some(ContentMask {
                bounds: state.geometry.text_bounds,
            }),
            |window| {
                for selection in state.selection.drain(..) {
                    window.paint_quad(selection);
                }
                for line in &layout.logical_lines {
                    let line_end = line.first_visual_line + line.visual_line_count;
                    if line.first_visual_line >= layout.visible_lines.end
                        || line_end <= layout.visible_lines.start
                    {
                        continue;
                    }
                    let origin = point(
                        state.geometry.text_bounds.left() - layout.horizontal_scroll,
                        state.geometry.text_bounds.top()
                            + layout.line_height
                                * (line.first_visual_line as f32
                                    - layout.visible_lines.start as f32),
                    );
                    if let Err(error) = line.layout.paint(
                        origin,
                        layout.line_height,
                        gpui::TextAlign::Left,
                        Some(state.geometry.text_bounds),
                        window,
                        cx,
                    ) {
                        kvlog::error!("failed to paint composer text", err = %error);
                    }
                }
                if focus.is_focused(window)
                    && let Some(cursor) = state.cursor.take()
                {
                    window.paint_quad(cursor);
                }
            },
        );
        self.input.update(cx, |input, _| {
            let display_lines = layout.vim_display_lines(input);
            input.last_layout = Some(state.geometry.clone());
            input.layout_width = Some(bounds.size.width);
            input.layout_invalidated = false;
            input.editor.set_display_lines(
                display_lines,
                state.geometry.lines.len().max(1) as u16,
            );
        });
        if state.needs_relayout {
            cx.notify(self.input.entity_id());
        }
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
                fonts.as_ref().map_or_else(
                    || crate::ui_scale::font_rems(16.0, 16.0),
                    |fonts| crate::ui_scale::font_rems(fonts.message_size, fonts.interface_size),
                ),
            )
        } else {
            (
                fonts
                    .as_ref()
                    .map(|fonts| fonts.interface_family.clone())
                    .unwrap_or_else(|| ".SystemUIFont".into()),
                gpui::rems(1.0),
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
            .font_family(family)
            .text_size(size)
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
        prelude::*, px,
    };

    use super::{
        Composer, ComposerImagePaste, logical_line_range, logical_lines, message_ref_insertion,
        normalize_range, range_from_utf16, should_auto_close_code_fence, visible_line_range,
        word_range,
    };

    struct CompletionKeyHarness {
        composer: Entity<Composer>,
        actions: Rc<RefCell<Vec<&'static str>>>,
    }

    struct ComposerLayoutHarness {
        composer: Entity<Composer>,
        comparison: Option<Entity<Composer>>,
        width: f32,
    }

    impl Render for ComposerLayoutHarness {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .w(px(self.width))
                .flex()
                .flex_col()
                .child(self.composer.clone())
                .when_some(self.comparison.clone(), |layout, comparison| {
                    layout.child(comparison)
                })
        }
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
    fn vim_normal_mode_does_not_insert_unhandled_printable_keys(cx: &mut gpui::TestAppContext) {
        cx.update(crate::fonts::init);
        cx.update(|cx| {
            crate::key_bindings::install(&crate::config::schema::GuiConfig::default(), cx).unwrap()
        });
        let (composer, cx) = cx.add_window_view(|window, cx| {
            let mut composer = Composer::new(cx);
            composer.set_value("message", cx);
            window.focus(&composer.focus, cx);
            composer
        });

        cx.simulate_keystrokes(". ,");

        assert_eq!(
            composer.read_with(cx, |composer, _| composer.text()),
            "message"
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
            let layout = editor
                .last_layout
                .as_ref()
                .expect("editor has been painted");
            let line = &layout.lines[0];
            let y = layout.text_bounds.top() + layout.line_height / 2.;
            (
                point(layout.screen_x_for_offset(line, 0), y),
                point(layout.screen_x_for_offset(line, "alpha".len()), y),
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

    #[gpui::test]
    fn wraps_long_unbroken_text_to_the_measured_composer_width(cx: &mut gpui::TestAppContext) {
        cx.update(crate::fonts::init);
        let text = "abcdefghijklmnopqrstuvwxyz0123456789abcdefghijklmnopqrstuvwxyz";
        let (harness, cx) = cx.add_window_view(move |_, cx| {
            let composer = cx.new(|cx| {
                let mut composer =
                    Composer::with_binding_mode(crate::config::schema::BindingMode::Standard, cx);
                composer.set_value(text, cx);
                composer
            });
            ComposerLayoutHarness {
                composer,
                comparison: None,
                width: 120.,
            }
        });
        let composer = harness.read_with(cx, |harness, _| harness.composer.clone());
        let layout = composer.read_with(cx, |composer, _| {
            composer
                .last_layout
                .clone()
                .expect("composer has been painted")
        });

        assert!(layout.lines.len() > 1);
        for line in &layout.lines {
            assert!(
                line.x_for_offset(line.range.end) <= layout.text_bounds.size.width + px(1.),
                "visual line {:?} escaped width {:?}",
                line.range,
                layout.text_bounds.size.width
            );
        }
    }

    #[gpui::test]
    fn empty_composer_remains_valid_when_available_width_collapses(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(crate::fonts::init);
        let (harness, cx) = cx.add_window_view(|_, cx| {
            let composer = cx.new(|cx| {
                Composer::with_binding_mode(crate::config::schema::BindingMode::Standard, cx)
            });
            ComposerLayoutHarness {
                composer,
                comparison: None,
                width: 0.,
            }
        });
        let composer = harness.read_with(cx, |harness, _| harness.composer.clone());

        composer.read_with(cx, |composer, _| {
            let layout = composer
                .last_layout
                .as_ref()
                .expect("empty composer has been painted");
            assert_eq!(layout.lines.len(), 1);
            assert_eq!(layout.lines[0].range, 0..0);
            assert_eq!(layout.line_index_for_offset(0), Some(0));
        });
    }

    #[gpui::test]
    fn gj_and_gk_follow_the_shaped_visual_rows(cx: &mut gpui::TestAppContext) {
        cx.update(crate::fonts::init);
        let text = "proportional words ".repeat(20);
        let (harness, cx) = cx.add_window_view(move |window, cx| {
            let composer = cx.new(|cx| {
                let mut composer = Composer::new(cx);
                composer.set_value(text, cx);
                composer
            });
            window.focus(&composer.focus_handle(cx), cx);
            ComposerLayoutHarness {
                composer,
                comparison: None,
                width: 120.,
            }
        });
        let composer = harness.read_with(cx, |harness, _| harness.composer.clone());
        let second_row_start = composer.read_with(cx, |composer, _| {
            let layout = composer
                .last_layout
                .as_ref()
                .expect("composer has been painted");
            assert!(layout.lines.len() > 1);
            layout.lines[1].range.start
        });

        cx.simulate_keystrokes("g j");
        assert_eq!(
            composer.read_with(cx, |composer, _| composer.snapshot().selection),
            second_row_start..second_row_start
        );

        cx.simulate_keystrokes("g k");
        assert_eq!(
            composer.read_with(cx, |composer, _| composer.snapshot().selection),
            0..0
        );
    }

    #[gpui::test]
    fn typing_after_first_layout_grows_using_the_cached_width(cx: &mut gpui::TestAppContext) {
        cx.update(crate::fonts::init);
        let (harness, cx) = cx.add_window_view(|window, cx| {
            let composer = cx.new(|cx| {
                Composer::with_binding_mode(crate::config::schema::BindingMode::Standard, cx)
            });
            window.focus(&composer.focus_handle(cx), cx);
            ComposerLayoutHarness {
                composer,
                comparison: None,
                width: 100.,
            }
        });
        let composer = harness.read_with(cx, |harness, _| harness.composer.clone());
        let initial_height = composer.read_with(cx, |composer, _| {
            composer
                .last_layout
                .as_ref()
                .expect("composer has been painted")
                .bounds
                .size
                .height
        });

        cx.simulate_input("abcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyz");

        composer.read_with(cx, |composer, _| {
            let layout = composer
                .last_layout
                .as_ref()
                .expect("composer has been repainted");
            assert!(layout.lines.len() > 1);
            assert!(layout.bounds.size.height > initial_height);
            assert!(!composer.layout_invalidated);
        });
    }

    #[gpui::test]
    fn preserves_single_line_vertical_insets_as_the_composer_grows(cx: &mut gpui::TestAppContext) {
        cx.update(crate::fonts::init);
        let (harness, cx) = cx.add_window_view(|_, cx| {
            let single = cx.new(|cx| {
                let mut composer =
                    Composer::with_binding_mode(crate::config::schema::BindingMode::Standard, cx);
                composer.set_value("one", cx);
                composer
            });
            let multiline = cx.new(|cx| {
                let mut composer =
                    Composer::with_binding_mode(crate::config::schema::BindingMode::Standard, cx);
                composer.set_value("one\ntwo\nthree", cx);
                composer
            });
            ComposerLayoutHarness {
                composer: single,
                comparison: Some(multiline),
                width: 240.,
            }
        });
        let (single, multiline) = harness.read_with(cx, |harness, _| {
            (
                harness.composer.clone(),
                harness.comparison.clone().expect("comparison composer"),
            )
        });
        let single = single.read_with(cx, |composer, _| {
            composer.last_layout.clone().expect("single-line layout")
        });
        let multiline = multiline.read_with(cx, |composer, _| {
            composer.last_layout.clone().expect("multiline layout")
        });
        let single_top = single.text_bounds.top() - single.bounds.top();
        let single_bottom = single.bounds.bottom() - single.text_bounds.bottom();
        let multiline_top = multiline.text_bounds.top() - multiline.bounds.top();
        let multiline_bottom = multiline.bounds.bottom() - multiline.text_bounds.bottom();

        assert_eq!(single_top, single_bottom);
        assert_eq!(multiline_top, multiline_bottom);
        assert_eq!(single_top, multiline_top);
        assert_eq!(
            multiline.bounds.size.height - single.bounds.size.height,
            single.line_height * 2.
        );
    }

    #[gpui::test]
    fn caps_long_wrapped_drafts_at_eight_visual_rows_and_keeps_the_cursor_visible(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(crate::fonts::init);
        let text = "x".repeat(600);
        let expected_end = text.len();
        let newline_text = (0..20)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let newline_end = newline_text.len();
        let (harness, cx) = cx.add_window_view(move |_, cx| {
            let composer = cx.new(|cx| {
                let mut composer =
                    Composer::with_binding_mode(crate::config::schema::BindingMode::Standard, cx);
                composer.set_value(text, cx);
                composer
            });
            let newline_composer = cx.new(|cx| {
                let mut composer =
                    Composer::with_binding_mode(crate::config::schema::BindingMode::Standard, cx);
                composer.set_value(newline_text, cx);
                composer
            });
            ComposerLayoutHarness {
                composer,
                comparison: Some(newline_composer),
                width: 100.,
            }
        });
        let (composer, newline_composer) = harness.read_with(cx, |harness, _| {
            (
                harness.composer.clone(),
                harness.comparison.clone().expect("newline composer"),
            )
        });
        let layout = composer.read_with(cx, |composer, _| {
            composer
                .last_layout
                .clone()
                .expect("composer has been painted")
        });

        assert_eq!(layout.lines.len(), super::MAX_VISIBLE_LINES);
        assert_eq!(
            layout.line_index_for_offset(expected_end),
            Some(super::MAX_VISIBLE_LINES - 1)
        );
        assert_eq!(
            layout.lines.last().map(|line| line.range.end),
            Some(expected_end)
        );
        let newline_layout = newline_composer.read_with(cx, |composer, _| {
            composer
                .last_layout
                .clone()
                .expect("newline composer has been painted")
        });
        assert_eq!(newline_layout.lines.len(), super::MAX_VISIBLE_LINES);
        assert_eq!(
            newline_layout.line_index_for_offset(newline_end),
            Some(super::MAX_VISIBLE_LINES - 1)
        );
    }

    #[gpui::test]
    fn wrapped_hit_testing_uses_visual_row_geometry(cx: &mut gpui::TestAppContext) {
        cx.update(crate::fonts::init);
        let (harness, cx) = cx.add_window_view(|_, cx| {
            let composer = cx.new(|cx| {
                let mut composer =
                    Composer::with_binding_mode(crate::config::schema::BindingMode::Standard, cx);
                composer.set_value("alpha beta gamma delta epsilon", cx);
                composer
            });
            ComposerLayoutHarness {
                composer,
                comparison: None,
                width: 110.,
            }
        });
        let composer = harness.read_with(cx, |harness, _| harness.composer.clone());
        let layout = composer.read_with(cx, |composer, _| {
            composer
                .last_layout
                .clone()
                .expect("composer has been painted")
        });
        let line = &layout.lines[1];
        let point = point(
            layout.screen_x_for_offset(line, line.range.start),
            layout.text_bounds.top() + layout.line_height * 1.5,
        );

        assert_eq!(layout.offset_for_point(point), Some(line.range.start));
        let wrapped_range = layout.lines[0].range.start..line.range.end;
        let bounds = layout
            .bounds_for_range(wrapped_range)
            .expect("wrapped range has platform bounds");
        assert_eq!(bounds.top(), layout.text_bounds.top());
        assert_eq!(
            bounds.bottom(),
            layout.text_bounds.top() + layout.line_height * 2.
        );
        assert!(bounds.left() >= layout.text_bounds.left());
        assert!(bounds.right() <= layout.text_bounds.right());
    }

    #[gpui::test]
    fn remeasures_wrapping_and_height_when_the_available_width_changes(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(crate::fonts::init);
        let text = "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu".to_owned();
        let (harness, cx) = cx.add_window_view(move |_, cx| {
            let composer = cx.new(|cx| {
                let mut composer =
                    Composer::with_binding_mode(crate::config::schema::BindingMode::Standard, cx);
                composer.set_value(text, cx);
                composer
            });
            ComposerLayoutHarness {
                composer,
                comparison: None,
                width: 120.,
            }
        });
        let composer = harness.read_with(cx, |harness, _| harness.composer.clone());
        let narrow = composer.read_with(cx, |composer, _| {
            composer
                .last_layout
                .clone()
                .expect("narrow composer layout")
        });

        harness.update(cx, |harness, cx| {
            harness.width = 320.;
            cx.notify();
        });
        let wide = composer.read_with(cx, |composer, _| {
            composer.last_layout.clone().expect("wide composer layout")
        });

        assert!(wide.lines.len() < narrow.lines.len());
        assert!(wide.bounds.size.height < narrow.bounds.size.height);
    }

    #[gpui::test]
    fn single_line_inputs_scroll_horizontally_instead_of_escaping(cx: &mut gpui::TestAppContext) {
        cx.update(crate::fonts::init);
        let text = "a very long settings value that cannot fit in the input";
        let text_end = text.len();
        let (harness, cx) = cx.add_window_view(move |_, cx| {
            let composer = cx.new(|cx| {
                let mut composer = Composer::settings_input(
                    "Edit value",
                    crate::config::schema::BindingMode::Standard,
                    cx,
                );
                composer.set_value(text, cx);
                composer
            });
            ComposerLayoutHarness {
                composer,
                comparison: None,
                width: 100.,
            }
        });
        let composer = harness.read_with(cx, |harness, _| harness.composer.clone());
        let layout = composer.read_with(cx, |composer, _| {
            composer
                .last_layout
                .clone()
                .expect("composer has been painted")
        });
        let line = &layout.lines[0];

        assert_eq!(layout.lines.len(), 1);
        assert!(layout.horizontal_scroll > px(0.));
        assert!(layout.screen_x_for_offset(line, text_end) <= layout.text_bounds.right() + px(0.5));
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
