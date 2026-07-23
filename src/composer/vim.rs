use std::{borrow::Cow, collections::HashMap, ops::Range};

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::{
    buffer::{self, Edit, TextBuffer},
    cursor::{self, Cursor, MotionKind},
    history::History,
    mode::Mode,
    visual::{Selection, VisualKind},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Motion {
    Left,
    Right,
    Up,
    Down,
    DisplayUp,
    DisplayDown,
    WordForward,
    WordBack,
    WordEnd,
    LineStart,
    LineEnd,
    LineFirstNonblank,
    FirstLine,
    LastLine,
    ParagraphForward,
    ParagraphBack,
    HalfPageDown,
    HalfPageUp,
    FindChar { forward: bool, till: bool },
    Mark { linewise: bool },
}

impl VimEditor {
    fn execute_inner(&mut self, action: VimAction, count: u32, captured: Option<char>) {
        let n = count.max(1) as usize;
        match action {
            VimAction::EnterInsert => {
                self.checkpoint();
                self.enter_insert_at_cursor();
            }
            VimAction::EnterInsertAfter => {
                self.checkpoint();
                self.enter_insert_after();
            }
            VimAction::EnterInsertFirstNonblank => {
                self.checkpoint();
                self.enter_insert_at_first_nonblank();
            }
            VimAction::EnterInsertEol => {
                self.checkpoint();
                self.enter_insert_at_eol();
            }
            VimAction::OpenBelow => {
                self.checkpoint();
                self.open_line_below_and_insert();
            }
            VimAction::OpenAbove => {
                self.checkpoint();
                self.open_line_above_and_insert();
            }

            VimAction::EnterVisualChar => self.toggle_or_enter_visual(VisualKind::Char),
            VimAction::EnterVisualLine => self.toggle_or_enter_visual(VisualKind::Line),
            VimAction::EnterVisualBlock => self.toggle_or_enter_visual(VisualKind::Block),
            VimAction::ExitVisual => self.exit_visual(),

            VimAction::Motion(m) => self.exec_motion(m, count, captured),

            VimAction::DeleteChar => self.exec_x(n),
            VimAction::Substitute => self.exec_s(n),
            VimAction::SubstituteLine => self.exec_big_s(n),
            VimAction::JoinLines => self.exec_join_lines(n),
            VimAction::PasteAfter => self.exec_paste_after(n),
            VimAction::PasteBefore => self.exec_paste_before(n),
            VimAction::Undo => self.exec_undo(),
            VimAction::Redo => self.exec_redo(),
            VimAction::Replace => {
                if let Some(c) = captured {
                    self.exec_replace(c, n);
                }
            }

            VimAction::DeleteMotion(m) => {
                self.exec_operator_motion(Operator::Delete, m, count, captured)
            }
            VimAction::ChangeMotion(m) => {
                self.exec_operator_motion(Operator::Change, m, count, captured)
            }
            VimAction::YankMotion(m) => {
                self.exec_operator_motion(Operator::Yank, m, count, captured)
            }
            VimAction::DeleteTextObject(t) => {
                self.exec_operator_text_object(Operator::Delete, t, count)
            }
            VimAction::ChangeTextObject(t) => {
                self.exec_operator_text_object(Operator::Change, t, count)
            }
            VimAction::YankTextObject(t) => {
                self.exec_operator_text_object(Operator::Yank, t, count)
            }
            VimAction::DeleteLine => self.exec_operator_line(Operator::Delete, n),
            VimAction::ChangeLine => self.exec_operator_line(Operator::Change, n),
            VimAction::YankLine => self.exec_operator_line(Operator::Yank, n),

            VimAction::ToggleCaseChar => self.exec_toggle_case_char(n),
            VimAction::CaseMotion(t, m) => self.exec_case_motion(t, m, count, captured),
            VimAction::CaseTextObject(t, obj) => self.exec_case_text_object(t, obj, count),
            VimAction::CaseLine(t) => self.exec_case_line(t, n),
            VimAction::CaseSelection(t) => self.exec_case_selection(t),

            VimAction::IncrementNumber => self.exec_increment(n, 1),
            VimAction::DecrementNumber => self.exec_increment(n, -1),

            VimAction::SetMark => {
                if let Some(c) = captured {
                    self.exec_set_mark(c);
                }
            }

            VimAction::ExitInsert => self.exec_exit_insert(),
            VimAction::InsertChar => {
                if let Some(c) = captured {
                    self.exec_insert_char(c);
                }
            }
            VimAction::InsertNewline => self.exec_insert_newline(),
            VimAction::InsertTab => self.exec_insert_tab(),
            VimAction::BackspaceDelete => self.exec_backspace(),

            VimAction::DeleteSelection => self.apply_visual_operator(Operator::Delete),
            VimAction::ChangeSelection => self.apply_visual_operator(Operator::Change),
            VimAction::YankSelection => self.apply_visual_operator(Operator::Yank),
            VimAction::SurroundSelection => {
                if let Some(c) = captured {
                    self.exec_surround_selection(c);
                }
            }
            VimAction::SelectTextObject(obj) => self.exec_select_text_object(obj, count),

            VimAction::NoOp => {}
        }
        if self.single_line {
            self.scroll_offset = 0;
            self.fixup_cursor();
        }
    }

    fn exec_motion(&mut self, motion: Motion, count: u32, captured: Option<char>) {
        let n = count.max(1);
        // Visual modes use Insert-mode clamping so the selection head can
        // reach the line end.
        let motion_mode = if self.mode.is_visual() {
            Mode::Insert
        } else {
            self.mode
        };
        let Some((target, _kind)) =
            self.resolve_motion(motion, self.cursor, n, motion_mode, captured)
        else {
            self.dirty = true;
            return;
        };
        self.cursor = target;
        match motion {
            Motion::LineEnd => self.desired_display_col = u16::MAX,
            Motion::Up
            | Motion::Down
            | Motion::DisplayUp
            | Motion::DisplayDown
            | Motion::HalfPageDown
            | Motion::HalfPageUp => {}
            _ => self.update_desired_display_col(),
        }
        if self.mode.is_visual() {
            self.update_selection_head();
        }
        self.dirty = true;
    }

    fn exec_operator_motion(
        &mut self,
        op: Operator,
        motion: Motion,
        count: u32,
        captured: Option<char>,
    ) {
        let start = self.cursor;
        // Vim's "cw" special case: on a non-blank grapheme, behaves as "ce".
        let effective_motion = if op == Operator::Change && motion == Motion::WordForward {
            let line = self.buf.line(start.row);
            if !line.is_empty()
                && cursor::class_at(line.as_ref(), start.col) != cursor::Class::Whitespace
            {
                Motion::WordEnd
            } else {
                Motion::WordForward
            }
        } else {
            motion
        };

        let n = count.max(1);
        let Some((target, kind)) =
            self.resolve_motion(effective_motion, start, n, self.mode, captured)
        else {
            self.dirty = true;
            return;
        };

        // Vim's `w` with operator: if the motion crosses a line boundary,
        // clamp to the end of the start line so `dw` at EOL does not
        // swallow the newline.
        let target = if matches!(effective_motion, Motion::WordForward)
            && target.row != start.row
            && start.row < self.buf.line_count()
        {
            let line = self.buf.line(start.row);
            Cursor {
                row: start.row,
                col: line.len(),
            }
        } else {
            target
        };

        self.apply_operator_motion(op, start, target, kind);
        self.dirty = true;
    }

    fn exec_operator_line(&mut self, op: Operator, n: usize) {
        let max_row = self.buf.max_row();
        let start_row = self.cursor.row.min(max_row);
        let end_row = start_row.saturating_add(n.saturating_sub(1)).min(max_row);
        self.apply_operator_linewise(op, start_row, end_row);
        self.dirty = true;
    }

    fn exec_operator_text_object(&mut self, op: Operator, object: TextObject, count: u32) {
        if let Some((start, end)) = resolve_text_object(&self.buf, self.cursor, object, count) {
            self.apply_operator_charwise(op, start, end);
            self.dirty = true;
        }
    }

    fn resolve_motion(
        &self,
        motion: Motion,
        start: Cursor,
        n: u32,
        mode: Mode,
        captured: Option<char>,
    ) -> Option<(Cursor, MotionKind)> {
        let cursor = match motion {
            Motion::Left => cursor::motion_h(&self.buf, start, n),
            Motion::Right => cursor::motion_l(&self.buf, start, n, mode),
            Motion::WordForward => cursor::motion_w(&self.buf, start, n, mode),
            Motion::WordBack => cursor::motion_b(&self.buf, start, n),
            Motion::WordEnd => cursor::motion_e(&self.buf, start, n),
            Motion::LineStart => cursor::motion_0(&self.buf, start),
            Motion::LineEnd => cursor::motion_dollar(&self.buf, start, n, mode),
            Motion::LineFirstNonblank => cursor::motion_caret(&self.buf, start, mode),
            Motion::Down => self.motion_vertical(start, n, mode, true),
            Motion::Up => self.motion_vertical(start, n, mode, false),
            Motion::DisplayDown => self.motion_display_line(start, n, mode, true),
            Motion::DisplayUp => self.motion_display_line(start, n, mode, false),
            Motion::HalfPageDown => {
                self.motion_vertical(start, n.saturating_mul(self.half_page_rows()), mode, true)
            }
            Motion::HalfPageUp => {
                self.motion_vertical(start, n.saturating_mul(self.half_page_rows()), mode, false)
            }
            // `{motion} alone` (count<=1) is the bare motion; N{motion}
            // takes a 1-based line number.
            Motion::FirstLine => cursor::motion_gg(&self.buf, if n <= 1 { None } else { Some(n) }),
            Motion::LastLine => cursor::motion_G(&self.buf, if n <= 1 { None } else { Some(n) }),
            Motion::ParagraphForward => cursor::motion_paragraph_forward(&self.buf, start, n),
            Motion::ParagraphBack => cursor::motion_paragraph_back(&self.buf, start, n),
            Motion::FindChar { forward, till } => {
                cursor::motion_find_char(&self.buf, start, captured?, n, forward, till)?
            }
            Motion::Mark { linewise } => {
                let (row, col) = self.mark_position(captured?)?;
                if linewise {
                    // `'m` jumps to the first non-blank on the target line.
                    cursor::motion_caret(&self.buf, Cursor { row, col: 0 }, mode)
                } else {
                    Cursor { row, col }
                }
            }
        };
        Some((cursor, motion_kind(motion)))
    }

    /// `j`/`k`: move `n` rows down or up preserving `curswant`.
    fn motion_vertical(&self, start: Cursor, n: u32, mode: Mode, down: bool) -> Cursor {
        let step = if down {
            cursor::motion_j
        } else {
            cursor::motion_k
        };
        step(
            &self.buf,
            start,
            n,
            mode,
            self.desired_display_col,
            self.tab_settings.tabstop,
        )
    }

    fn mark_position(&self, ch: char) -> Option<(usize, usize)> {
        let offset = *self.marks.get(&ch)?;
        Some(self.buf.offset_to_rowcol(offset.min(self.buf.len() as u32)))
    }

    fn motion_display_line(&self, start: Cursor, n: u32, mode: Mode, down: bool) -> Cursor {
        let width = self.width.max(1);
        if !self.effective_wrap() || width == 0 {
            return self.motion_vertical(start, n, mode, down);
        }

        let desired_display_col = if self.desired_display_col == u16::MAX {
            self.cursor_display().1
        } else {
            self.desired_display_col
        };
        let target_visual_col = desired_display_col % width;
        let mut cursor = start;

        for _ in 0..n.max(1) {
            let line = self.buf.line(cursor.row);
            let line = line.as_ref();
            let display_col = cursor_display_col(line, cursor.col, mode, self.tab_settings.tabstop);
            let segment = display_col / width;
            let line_rows = wrapped_line_rows(line, width, self.tab_settings.tabstop);

            let (target_row, target_segment) = if down {
                if segment + 1 < line_rows {
                    (cursor.row, segment + 1)
                } else if cursor.row + 1 < self.buf.line_count() {
                    (cursor.row + 1, 0)
                } else {
                    break;
                }
            } else if segment > 0 {
                (cursor.row, segment - 1)
            } else if cursor.row > 0 {
                let prev_row = cursor.row - 1;
                let prev_line = self.buf.line(prev_row);
                let prev_rows =
                    wrapped_line_rows(prev_line.as_ref(), width, self.tab_settings.tabstop);
                (prev_row, prev_rows.saturating_sub(1))
            } else {
                break;
            };

            let target_line = self.buf.line(target_row);
            let target_abs_col = target_segment
                .saturating_mul(width)
                .saturating_add(target_visual_col);
            let target_col = cursor::col_from_display(
                target_line.as_ref(),
                target_abs_col,
                mode,
                self.tab_settings.tabstop,
            );
            cursor = Cursor {
                row: target_row,
                col: target_col,
            };
        }

        cursor
    }

    /// Puts the editor into Insert mode at the current cursor position.
    ///
    /// Use when the host wants "new buffer, start typing" semantics
    /// without synthesizing a keypress.
    pub fn enter_insert_mode(&mut self) {
        self.enter_insert_at_cursor();
    }

    fn enter_insert_at_cursor(&mut self) {
        self.set_mode_ctx(Mode::Insert);
        self.dirty = true;
    }

    fn enter_insert_after(&mut self) {
        // `a`: move cursor right one grapheme (clamped to line.len()) then Insert.
        let line = self.buf.line(self.cursor.row);
        let line = line.as_ref();
        if !line.is_empty() {
            self.cursor.col = buffer::next_grapheme_start(line, self.cursor.col);
        }
        self.enter_insert_at_cursor();
    }

    fn enter_insert_at_first_nonblank(&mut self) {
        let line = self.buf.line(self.cursor.row);
        self.cursor.col = leading_whitespace_end(line.as_ref());
        self.enter_insert_at_cursor();
    }

    fn enter_insert_at_eol(&mut self) {
        let line = self.buf.line(self.cursor.row);
        let line = line.as_ref();
        self.cursor.col = line.len();
        self.enter_insert_at_cursor();
    }

    fn open_line_below_and_insert(&mut self) {
        self.open_line_and_insert(true);
    }

    fn open_line_above_and_insert(&mut self) {
        self.open_line_and_insert(false);
    }

    fn open_line_and_insert(&mut self, below: bool) {
        if self.single_line {
            return;
        }
        let row = self.cursor.row;
        let (offset, new_row) = if below {
            let end = self.buf.line_start(row) + self.buf.line(row).len() as u32;
            (end, row + 1)
        } else {
            (self.buf.line_start(row), row)
        };
        self.commit(Edit::insert(offset, "\n".to_string()));
        self.cursor = Cursor {
            row: new_row,
            col: 0,
        };
        self.enter_insert_at_cursor();
    }

    fn exec_exit_insert(&mut self) {
        self.finish_pending_block_change();
        self.reset_to_primary_mode();

        if true {
            // Vim moves the cursor back by one grapheme on Esc (unless at col
            // 0). Then clamp to the last grapheme start of the line.
            let line = self.buf.line(self.cursor.row);
            let line = line.as_ref();
            if self.cursor.col > 0 {
                self.cursor.col = buffer::prev_grapheme_start(line, self.cursor.col);
            }
            let line = self.buf.line(self.cursor.row);
            let line = line.as_ref();
            if !line.is_empty() {
                let max = buffer::last_grapheme_start(line).unwrap_or(0);
                if self.cursor.col > max {
                    self.cursor.col = max;
                }
            }
        }
        self.update_desired_display_col();
        self.dirty = true;
    }

    fn finish_pending_block_change(&mut self) {
        let Some(pending) = self.pending_block_change.take() else {
            return;
        };
        if self.cursor.row != pending.row_start {
            return;
        }
        let line = self.buf.line(pending.row_start);
        let line = line.as_ref();
        let start_col = pending.col.min(line.len());
        let end_col = self.cursor.col.min(line.len());
        if end_col <= start_col {
            return;
        }
        let inserted = line[start_col..end_col].to_string();
        for row in ((pending.row_start + 1)..=pending.row_end).rev() {
            let line = self.buf.line(row);
            let col = pending.col.min(line.len());
            let offset = self.buf.rowcol_to_offset(row, col);
            self.commit(Edit::insert(offset, inserted.clone()));
        }
    }

    fn toggle_or_enter_visual(&mut self, target: VisualKind) {
        if !self.mode.is_visual() {
            self.enter_visual_fresh(target);
            return;
        }
        if VisualKind::from_mode(self.mode) == target {
            self.exit_visual();
            return;
        }
        if let Some(sel) = self.selection.as_mut() {
            sel.kind = target;
        }
        self.set_mode_ctx(target.mode());
        self.dirty = true;
    }

    fn enter_visual_fresh(&mut self, kind: VisualKind) {
        self.selection = Some(Selection::new(self.cursor, kind));
        self.set_mode_ctx(kind.mode());
        self.dirty = true;
    }

    fn exit_visual(&mut self) {
        self.selection = None;
        self.reset_to_primary_mode();
        self.dirty = true;
    }

    fn update_selection_head(&mut self) {
        if let Some(sel) = self.selection.as_mut() {
            sel.head = self.cursor;
        }
    }

    fn apply_operator_motion(
        &mut self,
        op: Operator,
        start: Cursor,
        end: Cursor,
        kind: MotionKind,
    ) {
        let (s, e) = self.normalize_motion_range(start, end, kind);
        if matches!(kind, MotionKind::Linewise) {
            self.apply_operator_linewise(op, s.0, e.0);
        } else {
            self.apply_operator_charwise(op, s, e);
        }
    }

    /// Returns the `(start, end)` byte endpoints produced by a motion
    /// of `kind`: exclusive kinds pass through, `CharInclusive` expands
    /// `end` by one grapheme so the motion's landing grapheme is
    /// included in the range.
    fn normalize_motion_range(
        &self,
        start: Cursor,
        end: Cursor,
        kind: MotionKind,
    ) -> ((usize, usize), (usize, usize)) {
        let ((sr, sc), (er, ec)) = order_range(start, end);
        let end = if matches!(kind, MotionKind::CharInclusive) {
            (er, buffer::grapheme_end(self.buf.line(er).as_ref(), ec))
        } else {
            (er, ec)
        };
        ((sr, sc), end)
    }

    fn apply_operator_charwise(
        &mut self,
        op: Operator,
        start: (usize, usize),
        end: (usize, usize),
    ) {
        let text = extract_charwise(&self.buf, start, end);
        self.yank = Yank {
            lines: split_to_lines(&text),
            kind: YankKind::Charwise,
        };
        self.cursor = Cursor {
            row: start.0,
            col: start.1,
        };
        match op {
            Operator::Yank => {}
            Operator::Delete => {
                self.checkpoint();
                self.commit_delete_range(start, end);
                self.fixup_cursor();
            }
            Operator::Change => {
                self.checkpoint();
                self.commit_delete_range(start, end);
                self.enter_insert_at_cursor();
            }
        }
        self.update_desired_display_col();
    }

    /// Deletes `buf.text()[start_offset..end_offset]` where the two
    /// endpoints are given in `(row, col)` form. No-op on empty range.
    fn commit_delete_range(&mut self, start: (usize, usize), end: (usize, usize)) {
        let s_off = self.buf.rowcol_to_offset(start.0, start.1);
        let e_off = self.buf.rowcol_to_offset(end.0, end.1);
        if e_off > s_off {
            self.commit(Edit::delete(s_off, e_off - s_off));
        }
    }

    fn apply_operator_linewise(&mut self, op: Operator, row_start: usize, row_end: usize) {
        let max_row = self.buf.max_row();
        let (rs, re) = if row_start <= row_end {
            (row_start.min(max_row), row_end.min(max_row))
        } else {
            (row_end.min(max_row), row_start.min(max_row))
        };
        // rs <= re and both <= max_row, so slicing is always valid.
        let mut lines: Vec<String> = Vec::with_capacity(re - rs + 1);
        for r in rs..=re {
            lines.push(self.buf.line(r).to_string());
        }
        self.yank = Yank {
            lines,
            kind: YankKind::Linewise,
        };
        self.cursor = Cursor { row: rs, col: 0 };
        match op {
            Operator::Yank => {}
            Operator::Delete => {
                self.checkpoint();
                self.commit_delete_lines(rs, re);
                self.cursor.row = rs.min(self.buf.line_count() - 1);
                self.fixup_cursor();
            }
            Operator::Change => {
                self.checkpoint();
                // Delete the content of rows `rs..=re` (including their
                // separating newlines) but leave row `rs` present as an
                // empty line — that's where insert-mode resumes.
                let start_off = self.buf.line_start(rs);
                let end_off = if re + 1 < self.buf.line_count() {
                    self.buf.line_start(re + 1) - 1
                } else {
                    self.buf.len() as u32
                };
                if end_off > start_off {
                    self.commit(Edit::delete(start_off, end_off - start_off));
                }
                self.enter_insert_at_cursor();
            }
        }
        self.update_desired_display_col();
    }

    /// Delete entire lines `rs..=re`. Ensures the buffer retains at
    /// least one (possibly empty) line.
    fn commit_delete_lines(&mut self, rs: usize, re: usize) {
        let max_row = self.buf.max_row();
        let end = re.min(max_row);
        let start = rs.min(end);
        let start_off = self.buf.line_start(start);
        let (apply_start, apply_len) = if end + 1 < self.buf.line_count() {
            // Consume the `\n` after line `end` so the split falls on a
            // true line boundary.
            let end_off = self.buf.line_start(end + 1);
            (start_off, end_off - start_off)
        } else if start > 0 {
            // Deleting through EOF from a non-first row: also consume
            // the preceding `\n` so no trailing empty row survives.
            (start_off - 1, self.buf.len() as u32 - (start_off - 1))
        } else {
            // Deleting the whole buffer.
            (0, self.buf.len() as u32)
        };
        if apply_len > 0 {
            self.commit(Edit::delete(apply_start, apply_len));
        }
    }

    fn exec_insert_char(&mut self, c: char) {
        if self.single_line && matches!(c, '\n' | '\r') {
            return;
        }
        self.commit_insert_at_cursor(c.to_string());
        self.update_desired_display_col();
    }

    fn exec_insert_tab(&mut self) {
        let text = if self.tab_settings.expandtab {
            let line = self.buf.line(self.cursor.row);
            let display_col =
                byte_col_to_display_col(line.as_ref(), self.cursor.col, self.tab_settings.tabstop);
            let width = self.tab_settings.expandtab_width(display_col);
            Cow::Owned(" ".repeat(width as usize))
        } else {
            self.tab_settings.tab_input_text()
        };
        self.commit_insert_at_cursor(text.into_owned());
        self.update_desired_display_col();
    }

    fn exec_insert_newline(&mut self) {
        if self.single_line {
            return;
        }
        self.commit_insert_at_cursor("\n".to_string());
        self.desired_display_col = 0;
    }

    /// Inserts `text` at the cursor and advances the cursor past the
    /// insertion. Does NOT refresh `desired_display_col`.
    fn commit_insert_at_cursor(&mut self, text: String) {
        let offset = self.buf.rowcol_to_offset(self.cursor.row, self.cursor.col);
        let len = text.len() as u32;
        self.commit(Edit::insert(offset, text));
        let (r, col) = self.buf.offset_to_rowcol(offset + len);
        self.cursor = Cursor { row: r, col };
    }

    fn exec_backspace(&mut self) {
        let (row, col) = (self.cursor.row, self.cursor.col);
        if col == 0 {
            if row == 0 {
                return;
            }
            let prev_row = row - 1;
            let prev_end = self.buf.line(prev_row).len();
            let nl_offset = self.buf.line_start(row) - 1;
            self.commit(Edit::delete(nl_offset, 1));
            self.cursor = Cursor {
                row: prev_row,
                col: prev_end,
            };
        } else {
            let line = self.buf.line(row);
            let line = line.as_ref();
            let start_col = buffer::prev_grapheme_start(line, col);
            let offset = self.buf.line_start(row) + start_col as u32;
            let len = (col - start_col) as u32;
            self.commit(Edit::delete(offset, len));
            self.cursor = Cursor {
                row,
                col: start_col,
            };
        }
        self.update_desired_display_col();
    }

    fn exec_x(&mut self, n: usize) {
        let max_row = self.buf.max_row();
        let row = self.cursor.row.min(max_row);
        let line = self.buf.line(row);
        let line = line.as_ref();
        if line.is_empty() {
            return;
        }
        let start_col = buffer::align_to_grapheme_start(line, self.cursor.col.min(line.len()));
        let mut end_col = start_col;
        for _ in 0..n {
            if end_col >= line.len() {
                break;
            }
            end_col = buffer::next_grapheme_start(line, end_col);
        }
        self.apply_operator_charwise(Operator::Delete, (row, start_col), (row, end_col));
    }

    fn exec_replace(&mut self, ch: char, n: usize) {
        let max_row = self.buf.max_row();
        let r = self.cursor.row.min(max_row);
        let line = self.buf.line(r);
        let line = line.as_ref();
        if line.is_empty() {
            return;
        }
        let start = buffer::align_to_grapheme_start(line, self.cursor.col.min(line.len()));
        self.checkpoint();
        let mut col = start;
        let mut remaining = n;
        while remaining > 0 {
            let line = self.buf.line(r);
            let line = line.as_ref();
            if col >= line.len() {
                self.history.abort();
                return;
            }
            col = buffer::next_grapheme_start(line, col);
            remaining -= 1;
        }
        let replacement: String = std::iter::repeat(ch).take(n).collect();
        let offset = self.buf.line_start(r) + start as u32;
        let old_len = (col - start) as u32;
        self.commit(Edit::replace(offset, old_len, replacement));
        let last_start = if n > 0 {
            start + (n - 1) * ch.len_utf8()
        } else {
            start
        };
        self.cursor = Cursor {
            row: r,
            col: last_start,
        };
    }

    fn exec_s(&mut self, n: usize) {
        self.exec_x(n);
        self.enter_insert_at_cursor();
    }

    fn exec_big_s(&mut self, _n: usize) {
        let max_row = self.buf.max_row();
        let r = self.cursor.row.min(max_row);
        self.apply_operator_linewise(Operator::Change, r, r);
    }

    fn exec_join_lines(&mut self, count: usize) {
        let max_row = self.buf.max_row();
        let row = self.cursor.row.min(max_row);
        if row >= max_row {
            return;
        }

        // Vim's `[count]J` joins `count` lines total; bare `J` joins the
        // current line with the next one.
        let joins = if count <= 1 { 1 } else { count - 1 };
        self.checkpoint();
        for _ in 0..joins {
            if row + 1 >= self.buf.line_count() {
                break;
            }

            let line = self.buf.line(row);
            let line = line.as_ref();
            let next = self.buf.line(row + 1);
            let next = next.as_ref();
            let line_len = line.len();
            let next_content_col = leading_whitespace_end(next);
            let replacement = join_separator(line, next, next_content_col);
            let offset = self.buf.line_start(row) + line_len as u32;
            let old_len = 1 + next_content_col as u32;
            self.commit(Edit::replace(offset, old_len, replacement.to_string()));
            self.cursor = Cursor { row, col: line_len };
            self.fixup_cursor();
        }
        self.update_desired_display_col();
    }

    fn exec_paste_after(&mut self, count: usize) {
        self.exec_paste(count, true);
    }

    fn exec_paste_before(&mut self, count: usize) {
        self.exec_paste(count, false);
    }

    fn exec_paste(&mut self, count: usize, after: bool) {
        if self.yank.lines.is_empty() {
            return;
        }
        self.checkpoint();
        match self.yank.kind {
            YankKind::Charwise => {
                let insert_col = if after {
                    let line = self.buf.line(self.cursor.row);
                    let line = line.as_ref();
                    if line.is_empty() {
                        0
                    } else {
                        buffer::next_grapheme_start(line, self.cursor.col)
                    }
                } else {
                    self.cursor.col
                };
                let text = self.paste_text();
                let end_off =
                    self.commit_insert_repeated(self.cursor.row, insert_col, &text, count);
                self.cursor = self.cursor_one_grapheme_before(end_off);
            }
            YankKind::Linewise if self.single_line => {
                let insert_col = if after {
                    self.buf.line(self.cursor.row).len()
                } else {
                    0
                };
                let text = self.paste_text();
                self.commit_insert_repeated(self.cursor.row, insert_col, &text, count);
                self.cursor = Cursor {
                    row: self.cursor.row,
                    col: insert_col,
                };
            }
            YankKind::Linewise => {
                let insert_at = self.cursor.row + after as usize;
                let mut to_insert: Vec<String> = Vec::new();
                for _ in 0..count {
                    to_insert.extend(self.yank.lines.iter().cloned());
                }
                self.commit_insert_lines(insert_at, &to_insert);
                self.cursor = Cursor {
                    row: insert_at,
                    col: first_nonblank(self.buf.line(insert_at).as_ref()),
                };
            }
            YankKind::Blockwise => {
                let start_col = self.cursor.col;
                let start_row = self.cursor.row;
                let lines = self.yank.lines.clone();
                for (i, text) in lines.iter().enumerate() {
                    let r = start_row + i;
                    self.commit_ensure_row_exists(r);
                    let col = start_col.min(self.buf.line(r).len());
                    let offset = self.buf.rowcol_to_offset(r, col);
                    self.commit(Edit::insert(offset, text.clone()));
                }
                self.cursor = Cursor {
                    row: start_row,
                    col: start_col,
                };
            }
        }
        self.update_desired_display_col();
    }

    fn yank_text_joined(&self) -> String {
        self.yank.lines.join("\n")
    }

    fn paste_text(&self) -> String {
        let text = self.yank_text_joined();
        self.normalize_text_for_mode(&text).into_owned()
    }

    /// THE mutation entry point. Applies `edit` to the buffer,
    /// records the resulting inverse into the in-progress undo group
    /// (paired with the pre-edit cursor), and marks the editor dirty.
    /// Every mutation — user-driven or undo/redo-driven — must go
    /// through `commit` so that extension hooks (syntax highlighting,
    /// history) see every change in a uniform way.
    fn commit(&mut self, edit: Edit) -> Edit {
        let cursor_before = self.cursor;
        let inverse = self.apply_with_hooks(&edit);
        self.history.record(inverse.clone(), cursor_before);
        inverse
    }

    /// Low-level mutation: applies `edit`, notifies the syntax
    /// highlighter, flips `dirty`. Does NOT touch history.
    ///
    /// Used by `commit` (which layers history on top) and by
    /// `exec_undo` / `exec_redo` (which own their own history
    /// bookkeeping and would otherwise double-record).
    fn apply_with_hooks(&mut self, edit: &Edit) -> Edit {
        let normalized_edit;
        let edit = if self.single_line {
            normalized_edit = Edit {
                offset: edit.offset,
                old_len: edit.old_len,
                new: strip_single_line_breaks(&edit.new).into_owned(),
            };
            &normalized_edit
        } else {
            edit
        };
        let edit_is_noop = edit.old_len == 0 && edit.new.is_empty();
        if !edit_is_noop {
            self.shift_marks(edit.offset, edit.old_len, edit.new.len() as u32);
        }
        let inverse = self.buf.apply(edit);
        self.dirty = true;
        self.text_version = self.text_version.wrapping_add(1);
        inverse
    }

    /// Inserts `text` `count` times at `(row, col)` and returns the
    /// byte offset one past the final insertion.
    fn commit_insert_repeated(&mut self, row: usize, col: usize, text: &str, count: usize) -> u32 {
        let text = self.normalize_text_for_mode(text).into_owned();
        let mut offset = self.buf.rowcol_to_offset(row, col);
        if text.is_empty() {
            return offset;
        }
        for _ in 0..count {
            self.commit(Edit::insert(offset, text.to_string()));
            offset += text.len() as u32;
        }
        offset
    }

    /// Splices `lines` in as full rows at `row` — the equivalent of
    /// `Vec::insert_many(row, lines)` on the old line-per-entry
    /// storage.
    ///
    /// When `row < line_count`, inserts `"L0\nL1\n...Ln\n"` at that
    /// row's start. When `row >= line_count`, appends `"\nL0\n...Ln"`
    /// at end-of-buffer so the existing trailing row isn't merged
    /// with `L0`.
    fn commit_insert_lines(&mut self, row: usize, lines: &[String]) {
        if lines.is_empty() {
            return;
        }
        let at_end = row >= self.buf.line_count();
        let offset = if at_end {
            self.buf.len() as u32
        } else {
            self.buf.line_start(row)
        };
        if self.single_line {
            self.commit(Edit::insert(offset, lines.concat()));
            return;
        }
        // When inserting at end-of-buffer we prepend the separator so the
        // existing trailing row isn't merged with `lines[0]`; for inline
        // inserts the separator follows each line so the splice lands on
        // a fresh row.
        let mut content = String::new();
        for line in lines {
            if at_end {
                content.push('\n');
                content.push_str(line);
            } else {
                content.push_str(line);
                content.push('\n');
            }
        }
        self.commit(Edit::insert(offset, content));
    }

    /// Extends the buffer with empty rows until `row` is a valid index.
    fn commit_ensure_row_exists(&mut self, row: usize) {
        if self.single_line {
            return;
        }
        while self.buf.line_count() <= row {
            let offset = self.buf.len() as u32;
            self.commit(Edit::insert(offset, "\n".to_string()));
        }
    }

    fn normalize_text_for_mode<'a>(&self, text: &'a str) -> Cow<'a, str> {
        if self.single_line {
            strip_single_line_breaks(text)
        } else {
            Cow::Borrowed(text)
        }
    }

    fn enforce_single_line_mode(&mut self) {
        let old_text = self.buf.text();
        let offset = self.buf.rowcol_to_offset(self.cursor.row, self.cursor.col) as usize;
        let normalized = strip_single_line_breaks(&old_text);
        let cursor_offset = single_line_prefix_len(&old_text, offset) as u32;
        if normalized.as_ref() != old_text {
            self.buf.set_text(normalized.as_ref());
        }
        self.history.reset();
        self.yank = single_line_yank(&self.yank);
        self.selection = None;
        let target_mode = if self.mode.is_visual() {
            Mode::Normal
        } else {
            self.mode
        };
        self.set_mode_ctx(target_mode);
        let (row, col) = self.buf.offset_to_rowcol(cursor_offset);
        self.cursor = Cursor { row, col };
        self.fixup_cursor();
        self.update_desired_display_col();
        self.scroll_offset = 0;
    }

    /// Deletes bytes `[c_lo..c_hi)` from each row in `[r0..=r1]`.
    /// Applies row-by-row in reverse so edits on earlier rows don't
    /// shift the offsets needed for later rows.
    fn commit_delete_block(&mut self, r0: usize, r1: usize, c_lo: usize, c_hi: usize) {
        for r in (r0..=r1).rev() {
            let line = self.buf.line(r);
            let line = line.as_ref();
            let (lo, hi) = block_span(line, c_lo, c_hi);
            if lo < hi {
                let offset = self.buf.line_start(r) + lo as u32;
                self.commit(Edit::delete(offset, (hi - lo) as u32));
            }
        }
    }

    /// Helper for paste: given `end_offset` (one past the last
    /// inserted byte), returns the cursor position on the grapheme
    /// immediately preceding that offset. Mirrors vim's post-paste
    /// cursor placement on non-empty pastes.
    fn cursor_one_grapheme_before(&self, end_offset: u32) -> Cursor {
        let (r, c) = self.buf.offset_to_rowcol(end_offset);
        let final_col = if c == 0 {
            0
        } else {
            let line = self.buf.line(r);
            let line = line.as_ref();
            buffer::prev_grapheme_start(line, c)
        };
        Cursor {
            row: r,
            col: final_col,
        }
    }

    fn checkpoint(&mut self) {
        self.history.checkpoint();
    }

    /// Ensure cursor is within a valid position for Normal mode after
    /// an edit: clamp to last grapheme start; clamp row; handle empty buffer.
    fn fixup_cursor(&mut self) {
        // `TextBuffer` maintains at least one line, so `line_count >= 1`.
        let max_row = self.buf.line_count() - 1;
        if self.cursor.row > max_row {
            self.cursor.row = max_row;
        }
        let line = self.buf.line(self.cursor.row);
        let line = line.as_ref();
        if line.is_empty() {
            self.cursor.col = 0;
        } else if matches!(
            self.mode,
            Mode::Normal | Mode::Visual | Mode::VisualLine | Mode::VisualBlock
        ) {
            let max = buffer::last_grapheme_start(line).unwrap_or(0);
            if self.cursor.col > max {
                self.cursor.col = max;
            } else {
                self.cursor.col = buffer::align_to_grapheme_start(line, self.cursor.col);
            }
        } else if self.cursor.col > line.len() {
            self.cursor.col = line.len();
        }
    }

    fn update_desired_display_col(&mut self) {
        let line = self.buf.line(self.cursor.row);
        let line = line.as_ref();
        // `curswant` in vim tracks the display col of the cursor's
        // visible position, not the grapheme's start col — the two
        // differ for hard tabs in Normal/Visual mode.
        self.desired_display_col =
            cursor_display_col(line, self.cursor.col, self.mode, self.tab_settings.tabstop);
    }

    fn apply_visual_operator(&mut self, op: Operator) {
        let Some(sel) = self.selection.clone() else {
            return;
        };
        match sel.kind {
            VisualKind::Char => {
                let (start, end) = char_range(&sel);
                let end_byte = buffer::grapheme_end(self.buf.line(end.row).as_ref(), end.col);
                self.apply_operator_charwise(op, (start.row, start.col), (end.row, end_byte));
            }
            VisualKind::Line => {
                let (r0, r1) = sel.rows_ordered();
                self.apply_operator_linewise(op, r0, r1);
            }
            VisualKind::Block => {
                self.apply_block_operator(op, &sel);
            }
        }
        self.selection = None;
        if op != Operator::Change {
            // Change already transitioned into Insert via apply_operator_*,
            // which `set_mode_ctx`'d the current layer back to BASE.
            self.reset_to_primary_mode();
        }
        self.dirty = true;
    }

    fn apply_block_operator(&mut self, op: Operator, sel: &Selection) {
        let max_row = self.buf.max_row();
        let (r0, r1) = sel.rows_ordered();
        let r0 = r0.min(max_row);
        let r1 = r1.min(max_row);
        let (c_lo, c_hi) = sel.cols_ordered();

        let mut block_lines: Vec<String> = Vec::with_capacity(r1 - r0 + 1);
        for r in r0..=r1 {
            let line = self.buf.line(r);
            let line = line.as_ref();
            let (lo, hi) = block_span(line, c_lo, c_hi);
            block_lines.push(line[lo..hi].to_string());
        }

        self.yank = Yank {
            lines: block_lines,
            kind: YankKind::Blockwise,
        };
        self.cursor = Cursor { row: r0, col: c_lo };
        if matches!(op, Operator::Delete | Operator::Change) {
            self.checkpoint();
            self.commit_delete_block(r0, r1, c_lo, c_hi);
            self.fixup_cursor();
        }
        if matches!(op, Operator::Change) {
            self.pending_block_change = Some(PendingBlockChange {
                row_start: r0,
                row_end: r1,
                col: c_lo,
            });
            self.cursor = Cursor { row: r0, col: c_lo };
            self.enter_insert_at_cursor();
        }
        self.update_desired_display_col();
    }

    /// Grow (or establish) the visual selection to cover `object`.
    ///
    /// Matches nvim's `vip`, `viw`, `va"`, etc.: the head lands on the
    /// last grapheme of the object; the anchor moves to its first
    /// byte. If the object is line-shaped (`paragraph`), the mode
    /// switches to VisualLine so subsequent operators act on whole
    /// rows.
    fn exec_select_text_object(&mut self, object: TextObject, count: u32) {
        if !self.mode.is_visual() {
            return;
        }
        let Some((start, end)) = resolve_text_object(&self.buf, self.cursor, object, count) else {
            return;
        };

        // `end` is exclusive; step back one grapheme so the head sits on
        // the object's last selected grapheme.
        let head = {
            let line = self.buf.line(end.0);
            let line = line.as_ref();
            if end.1 == 0 && end.0 > 0 {
                // Object ended at the start of a row — back up to the
                // previous row's last grapheme.
                let prev = self.buf.line(end.0 - 1);
                let prev = prev.as_ref();
                Cursor {
                    row: end.0 - 1,
                    col: buffer::last_grapheme_start(prev).unwrap_or(0),
                }
            } else if end.1 == 0 {
                Cursor { row: 0, col: 0 }
            } else {
                let aligned = buffer::align_to_grapheme_start(line, end.1);
                Cursor {
                    row: end.0,
                    col: buffer::prev_grapheme_start(line, end.1.min(aligned + 1)),
                }
            }
        };
        let anchor = Cursor {
            row: start.0,
            col: start.1,
        };

        // Paragraphs are line-shaped; switch to VisualLine so subsequent
        // operators act on whole rows.
        if matches!(object, TextObject::Paragraph { .. }) && self.mode != Mode::VisualLine {
            self.set_mode_ctx(Mode::VisualLine);
        }

        self.selection = Some(Selection {
            anchor,
            head,
            kind: VisualKind::from_mode(self.mode),
        });
        self.cursor = head;
        self.update_desired_display_col();
        self.dirty = true;
    }

    fn exec_surround_selection(&mut self, delimiter: char) {
        let pair = surround_pair(delimiter);
        let Some(sel) = self.selection else {
            return;
        };

        match sel.kind {
            VisualKind::Char => self.surround_visual_charwise(&sel, pair),
            VisualKind::Line => self.surround_visual_linewise(&sel, pair),
            VisualKind::Block => self.surround_visual_blockwise(&sel, pair),
        }

        self.selection = None;
        self.reset_to_primary_mode();
        self.dirty = true;
        self.fixup_cursor();
        self.update_desired_display_col();
    }

    fn surround_visual_charwise(&mut self, sel: &Selection, pair: SurroundPair) {
        let (start, end) = char_range(sel);
        let end_byte = buffer::grapheme_end(self.buf.line(end.row).as_ref(), end.col);
        let start_off = self.buf.rowcol_to_offset(start.row, start.col);
        let end_off = self.buf.rowcol_to_offset(end.row, end_byte);
        self.checkpoint();
        self.commit(Edit::insert(end_off, pair.close.to_string()));
        self.commit(Edit::insert(start_off, pair.open.to_string()));
        self.cursor = Cursor {
            row: start.row,
            col: start.col,
        };
    }

    fn surround_visual_linewise(&mut self, sel: &Selection, pair: SurroundPair) {
        let max_row = self.buf.max_row();
        let (r0, r1) = sel.rows_ordered();
        let r0 = r0.min(max_row);
        let r1 = r1.min(max_row);
        let start_off = self.buf.line_start(r0);
        let end_off = if r1 + 1 < self.buf.line_count() {
            self.buf.line_start(r1 + 1) - 1
        } else {
            self.buf.len() as u32
        };
        self.checkpoint();
        self.commit(Edit::insert(end_off, pair.close.to_string()));
        self.commit(Edit::insert(start_off, pair.open.to_string()));
        self.cursor = Cursor { row: r0, col: 0 };
    }

    fn surround_visual_blockwise(&mut self, sel: &Selection, pair: SurroundPair) {
        let max_row = self.buf.max_row();
        let (r0, r1) = sel.rows_ordered();
        let r0 = r0.min(max_row);
        let r1 = r1.min(max_row);
        let (c_lo, c_hi) = sel.cols_ordered();

        self.checkpoint();
        for r in (r0..=r1).rev() {
            let line = self.buf.line(r);
            let line = line.as_ref();
            let (lo, hi) = block_span(line, c_lo, c_hi);
            if lo == hi {
                continue;
            }
            let close_off = self.buf.rowcol_to_offset(r, hi);
            let open_off = self.buf.rowcol_to_offset(r, lo);
            self.commit(Edit::insert(close_off, pair.close.to_string()));
            self.commit(Edit::insert(open_off, pair.open.to_string()));
        }
        self.cursor = Cursor { row: r0, col: c_lo };
    }

    fn exec_set_mark(&mut self, ch: char) {
        if !is_valid_mark(ch) {
            return;
        }
        let offset = self.buf.rowcol_to_offset(self.cursor.row, self.cursor.col);
        self.marks.insert(ch, offset);
    }

    fn shift_marks(&mut self, edit_offset: u32, old_len: u32, new_len: u32) {
        let old_end = edit_offset.saturating_add(old_len);
        let delta = i64::from(new_len) - i64::from(old_len);
        for offset in self.marks.values_mut() {
            if *offset >= old_end {
                *offset = (i64::from(*offset) + delta).max(0) as u32;
            } else if *offset > edit_offset {
                *offset = edit_offset;
            }
        }
    }

    fn exec_toggle_case_char(&mut self, n: usize) {
        if n == 0 {
            return;
        }
        let r = self.cursor.row.min(self.buf.max_row());
        let line = self.buf.line(r);
        let line = line.as_ref();
        if line.is_empty() {
            return;
        }
        let start = buffer::align_to_grapheme_start(line, self.cursor.col.min(line.len()));
        let mut end = start;
        for _ in 0..n {
            if end >= line.len() {
                break;
            }
            end = buffer::next_grapheme_start(line, end);
        }
        if end <= start {
            return;
        }
        self.apply_case_charwise(CaseTransform::Toggle, (r, start), (r, end));
        // Vim parks the cursor just past the transformed range, unlike
        // the generic case operator which lands on `start`.
        let new_line_len = self.buf.line(r).len();
        self.cursor = Cursor {
            row: r,
            col: end.min(new_line_len),
        };
        self.fixup_cursor();
        self.update_desired_display_col();
    }

    fn exec_case_motion(
        &mut self,
        transform: CaseTransform,
        motion: Motion,
        count: u32,
        captured: Option<char>,
    ) {
        let start = self.cursor;
        let n = count.max(1);
        let Some((target, kind)) = self.resolve_motion(motion, start, n, self.mode, captured)
        else {
            return;
        };
        self.apply_case_motion(transform, start, target, kind);
    }

    fn exec_case_text_object(&mut self, transform: CaseTransform, object: TextObject, count: u32) {
        let Some((start, end)) = resolve_text_object(&self.buf, self.cursor, object, count) else {
            return;
        };
        self.apply_case_charwise(transform, start, end);
    }

    fn exec_case_line(&mut self, transform: CaseTransform, n: usize) {
        let max_row = self.buf.max_row();
        let start_row = self.cursor.row.min(max_row);
        let end_row = start_row.saturating_add(n.saturating_sub(1)).min(max_row);
        self.apply_case_linewise(transform, start_row, end_row);
    }

    fn exec_case_selection(&mut self, transform: CaseTransform) {
        let Some(sel) = self.selection.clone() else {
            return;
        };
        match sel.kind {
            VisualKind::Char => {
                let (start, end) = char_range(&sel);
                let end_byte = buffer::grapheme_end(self.buf.line(end.row).as_ref(), end.col);
                self.apply_case_charwise(transform, (start.row, start.col), (end.row, end_byte));
            }
            VisualKind::Line => {
                let (r0, r1) = sel.rows_ordered();
                self.apply_case_linewise(transform, r0, r1);
            }
            VisualKind::Block => {
                self.apply_case_block(transform, &sel);
            }
        }
        self.selection = None;
        self.reset_to_primary_mode();
        self.dirty = true;
    }

    fn apply_case_motion(
        &mut self,
        transform: CaseTransform,
        start: Cursor,
        end: Cursor,
        kind: MotionKind,
    ) {
        let (s, e) = self.normalize_motion_range(start, end, kind);
        if matches!(kind, MotionKind::Linewise) {
            self.apply_case_linewise(transform, s.0, e.0);
        } else {
            self.apply_case_charwise(transform, s, e);
        }
    }

    fn apply_case_charwise(
        &mut self,
        transform: CaseTransform,
        start: (usize, usize),
        end: (usize, usize),
    ) {
        let s_off = self.buf.rowcol_to_offset(start.0, start.1);
        let e_off = self.buf.rowcol_to_offset(end.0, end.1);
        if e_off <= s_off {
            return;
        }
        let text = extract_charwise(&self.buf, start, end);
        let transformed = case_transform_text(transform, &text);
        if transformed == text {
            self.cursor = Cursor {
                row: start.0,
                col: start.1,
            };
            self.fixup_cursor();
            self.update_desired_display_col();
            self.dirty = true;
            return;
        }
        self.checkpoint();
        self.commit(Edit::replace(s_off, e_off - s_off, transformed));
        self.cursor = Cursor {
            row: start.0,
            col: start.1,
        };
        self.fixup_cursor();
        self.update_desired_display_col();
        self.dirty = true;
    }

    fn apply_case_linewise(&mut self, transform: CaseTransform, rs: usize, re: usize) {
        let max_row = self.buf.max_row();
        let (rs, re) = (rs.min(max_row), re.min(max_row));
        let (rs, re) = if rs <= re { (rs, re) } else { (re, rs) };
        self.checkpoint();
        for r in rs..=re {
            let line = self.buf.line(r);
            let line = line.as_ref();
            if line.is_empty() {
                continue;
            }
            let transformed = case_transform_text(transform, line);
            if transformed == line {
                continue;
            }
            let offset = self.buf.line_start(r);
            self.commit(Edit::replace(offset, line.len() as u32, transformed));
        }
        self.cursor = Cursor { row: rs, col: 0 };
        self.fixup_cursor();
        self.update_desired_display_col();
        self.dirty = true;
    }

    fn apply_case_block(&mut self, transform: CaseTransform, sel: &Selection) {
        let max_row = self.buf.max_row();
        let (r0, r1) = sel.rows_ordered();
        let r0 = r0.min(max_row);
        let r1 = r1.min(max_row);
        let (c_lo, c_hi) = sel.cols_ordered();

        self.checkpoint();
        for r in (r0..=r1).rev() {
            let line = self.buf.line(r);
            let line = line.as_ref();
            let (lo, hi) = block_span(line, c_lo, c_hi);
            if lo >= hi {
                continue;
            }
            let slice = &line[lo..hi];
            let transformed = case_transform_text(transform, slice);
            if transformed == slice {
                continue;
            }
            let offset = self.buf.line_start(r) + lo as u32;
            self.commit(Edit::replace(offset, (hi - lo) as u32, transformed));
        }
        self.cursor = Cursor { row: r0, col: c_lo };
        self.fixup_cursor();
        self.update_desired_display_col();
    }

    fn exec_increment(&mut self, n: usize, sign: i64) {
        let r = self.cursor.row.min(self.buf.max_row());
        let line = self.buf.line(r);
        let line = line.as_ref();
        let Some((start, end, value)) = find_number_from(line, self.cursor.col) else {
            return;
        };
        let delta = (n as i64).saturating_mul(sign);
        let new_value = value.saturating_add(delta);
        let replacement = new_value.to_string();
        self.checkpoint();
        let offset = self.buf.line_start(r) + start as u32;
        self.commit(Edit::replace(
            offset,
            (end - start) as u32,
            replacement.clone(),
        ));
        // Vim parks the cursor on the last digit of the new number.
        let last_digit_offset = offset as usize + replacement.len().saturating_sub(1);
        let line_start = self.buf.line_start(r) as usize;
        self.cursor = Cursor {
            row: r,
            col: last_digit_offset - line_start,
        };
        self.fixup_cursor();
        self.update_desired_display_col();
    }

    fn exec_undo(&mut self) {
        self.replay_history(false);
    }

    fn exec_redo(&mut self) {
        self.replay_history(true);
    }

    /// Replay one history group. `redo == false` pops from undo and
    /// applies steps in reverse; `true` pops from redo and applies
    /// them forward. Either way, the cursor lands on the group's
    /// first-recorded `cursor_before` (where the original action
    /// started) and the inverse group is pushed onto the opposite
    /// stack in forward order.
    fn replay_history(&mut self, redo: bool) {
        let group = if redo {
            self.history.pop_redo_group()
        } else {
            self.history.pop_undo_group()
        };
        let Some(group) = group else {
            return;
        };
        let restore_cursor = group
            .steps
            .first()
            .map(|s| s.cursor_before)
            .unwrap_or(self.cursor);
        let n = group.steps.len();
        let mut new_steps: Vec<super::history::UndoStep> = Vec::with_capacity(n);
        for i in 0..n {
            let idx = if redo { i } else { n - 1 - i };
            let step = &group.steps[idx];
            let inverse = self.apply_with_hooks(&step.inverse);
            new_steps.push(super::history::UndoStep {
                inverse,
                cursor_before: step.cursor_before,
            });
        }
        if !redo {
            new_steps.reverse();
        }
        let new_group = super::history::UndoGroup { steps: new_steps };
        if redo {
            self.history.push_undo_group(new_group);
        } else {
            self.history.push_redo_group(new_group);
        }
        self.cursor = restore_cursor;
        self.fixup_cursor();
        self.update_desired_display_col();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PairKind {
    Paren,
    Bracket,
    Brace,
    Angle,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextObject {
    Word { around: bool, big: bool },
    Quote { around: bool, delimiter: char },
    Pair { around: bool, kind: PairKind },
    Paragraph { around: bool },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaseTransform {
    Upper,
    Lower,
    Toggle,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VimAction {
    EnterInsert,
    EnterInsertAfter,
    EnterInsertFirstNonblank,
    EnterInsertEol,
    OpenBelow,
    OpenAbove,
    EnterVisualChar,
    EnterVisualLine,
    EnterVisualBlock,
    ExitVisual,
    Motion(Motion),
    DeleteChar,
    Substitute,
    SubstituteLine,
    JoinLines,
    PasteAfter,
    PasteBefore,
    Undo,
    Redo,
    Replace,
    DeleteMotion(Motion),
    ChangeMotion(Motion),
    YankMotion(Motion),
    DeleteTextObject(TextObject),
    ChangeTextObject(TextObject),
    YankTextObject(TextObject),
    DeleteLine,
    ChangeLine,
    YankLine,
    ToggleCaseChar,
    CaseMotion(CaseTransform, Motion),
    CaseTextObject(CaseTransform, TextObject),
    CaseLine(CaseTransform),
    CaseSelection(CaseTransform),
    IncrementNumber,
    DecrementNumber,
    SetMark,
    ExitInsert,
    InsertChar,
    InsertNewline,
    InsertTab,
    BackspaceDelete,
    DeleteSelection,
    ChangeSelection,
    YankSelection,
    SurroundSelection,
    SelectTextObject(TextObject),
    NoOp,
}

#[derive(Clone, Debug, Default)]
struct Yank {
    lines: Vec<String>,
    kind: YankKind,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum YankKind {
    #[default]
    Charwise,
    Linewise,
    Blockwise,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Operator {
    Delete,
    Change,
    Yank,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pending {
    None,
    G(Option<PendingOperator>),
    Operator(PendingOperator),
    TextObject {
        operator: Option<PendingOperator>,
        around: bool,
    },
    Capture(VimAction),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingOperator {
    Delete,
    Change,
    Yank,
    Case(CaseTransform),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VimKey {
    Char(char),
    Control(char),
    Escape,
    Backspace,
    Enter,
    Tab,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PendingBlockChange {
    row_start: usize,
    row_end: usize,
    col: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TabSettings {
    pub expandtab: bool,
    pub tabstop: u16,
    pub softtabstop: u16,
}

impl Default for TabSettings {
    fn default() -> Self {
        Self {
            expandtab: true,
            tabstop: 4,
            softtabstop: 4,
        }
    }
}

impl TabSettings {
    fn normalized(self) -> Self {
        Self {
            expandtab: self.expandtab,
            tabstop: self.tabstop.max(1),
            softtabstop: self.softtabstop,
        }
    }

    fn effective_softtabstop(self) -> u16 {
        let settings = self.normalized();
        if settings.softtabstop == 0 {
            settings.tabstop
        } else {
            settings.softtabstop
        }
    }

    fn tab_input_text(self) -> Cow<'static, str> {
        let settings = self.normalized();
        if settings.expandtab {
            Cow::Owned(" ".repeat(settings.effective_softtabstop() as usize))
        } else {
            Cow::Borrowed("\t")
        }
    }

    fn expandtab_width(self, display_col: u16) -> u16 {
        let stop = self.effective_softtabstop().max(1);
        let rem = display_col % stop;
        if rem == 0 { stop } else { stop - rem }
    }
}

fn grapheme_width(grapheme: &str) -> u16 {
    if grapheme.contains(char::is_control) {
        0
    } else {
        UnicodeWidthStr::width(grapheme) as u16
    }
}

fn next_tab_stop(col: u16, tabstop: u16) -> u16 {
    let tabstop = tabstop.max(1);
    (col / tabstop).saturating_add(1).saturating_mul(tabstop)
}

fn byte_col_to_display_col(line: &str, byte_col: usize, tabstop: u16) -> u16 {
    let limit = byte_col.min(line.len());
    let mut col = 0u16;
    for (index, grapheme) in line.grapheme_indices(true) {
        if index >= limit {
            break;
        }
        if grapheme == "\t" {
            col = next_tab_stop(col, tabstop);
        } else {
            col = col.saturating_add(grapheme_width(grapheme));
        }
    }
    col
}

fn cursor_display_col(line: &str, byte_col: usize, mode: Mode, tabstop: u16) -> u16 {
    let start = byte_col_to_display_col(line, byte_col, tabstop);
    if mode == Mode::Insert {
        return start;
    }
    match line
        .get(byte_col..)
        .and_then(|tail| tail.graphemes(true).next())
    {
        Some("\t") => next_tab_stop(start, tabstop).saturating_sub(1),
        _ => start,
    }
}

fn wrapped_line_rows(line: &str, width: u16, tabstop: u16) -> u16 {
    if width == 0 {
        return 0;
    }
    let cells = byte_col_to_display_col(line, line.len(), tabstop).max(1) as u32;
    ((cells - 1) / width as u32 + 1) as u16
}

fn char_range(selection: &Selection) -> (Cursor, Cursor) {
    if (selection.anchor.row, selection.anchor.col) <= (selection.head.row, selection.head.col) {
        (selection.anchor, selection.head)
    } else {
        (selection.head, selection.anchor)
    }
}

/// The [`MotionKind`] each [`Motion`] produces when fed into an
/// operator. Derived purely from the motion variant — it does not
/// depend on the cursor or buffer.
fn motion_kind(motion: Motion) -> MotionKind {
    match motion {
        Motion::WordEnd | Motion::LineEnd | Motion::FindChar { .. } => MotionKind::CharInclusive,
        Motion::Up
        | Motion::Down
        | Motion::HalfPageUp
        | Motion::HalfPageDown
        | Motion::FirstLine
        | Motion::LastLine => MotionKind::Linewise,
        Motion::Mark { linewise: true } => MotionKind::Linewise,
        _ => MotionKind::CharExclusive,
    }
}

fn order_range(a: Cursor, b: Cursor) -> ((usize, usize), (usize, usize)) {
    let (pa, pb) = ((a.row, a.col), (b.row, b.col));
    if pa <= pb { (pa, pb) } else { (pb, pa) }
}

fn first_nonblank(line: &str) -> usize {
    let mut col = 0;
    while col < line.len() {
        if cursor::class_at(line, col) != cursor::Class::Whitespace {
            return col;
        }
        col = buffer::next_grapheme_start(line, col);
    }
    0
}

fn leading_whitespace_end(line: &str) -> usize {
    let mut col = 0;
    while col < line.len() && cursor::class_at(line, col) == cursor::Class::Whitespace {
        col = buffer::next_grapheme_start(line, col);
    }
    col
}

fn ends_with_whitespace(line: &str) -> bool {
    if line.is_empty() {
        return false;
    }
    let col = buffer::prev_grapheme_start(line, line.len());
    cursor::class_at(line, col) == cursor::Class::Whitespace
}

fn join_separator(current: &str, next: &str, next_content_col: usize) -> &'static str {
    let next_content = &next[next_content_col..];
    if next_content.is_empty() || current.is_empty() || ends_with_whitespace(current) {
        ""
    } else if next_content.starts_with(')') {
        ""
    } else {
        " "
    }
}

fn extract_charwise(buf: &TextBuffer, start: (usize, usize), end: (usize, usize)) -> String {
    let max_row = buf.max_row();
    let sr = start.0.min(max_row);
    let er = end.0.min(max_row);
    let clamp = |line: &str, col: usize| buffer::align_to_grapheme_start(line, col.min(line.len()));
    if sr == er {
        let line = buf.line(sr);
        let line = line.as_ref();
        let sc = clamp(line, start.1);
        let ec = clamp(line, end.1).max(sc);
        return line[sc..ec].to_string();
    }
    let mut out = String::new();
    let first_line = buf.line(sr);
    let first_line = first_line.as_ref();
    let sc = clamp(first_line, start.1);
    out.push_str(&first_line[sc..]);
    out.push('\n');
    for r in (sr + 1)..er {
        let line = buf.line(r);
        out.push_str(line.as_ref());
        out.push('\n');
    }
    let last_line = buf.line(er);
    let last_line = last_line.as_ref();
    let ec = clamp(last_line, end.1);
    out.push_str(&last_line[..ec]);
    out
}

/// Returns `(lo, hi)` clamped byte indices for a block-selection span on
/// `line` between display-column anchors `c_lo` and `c_hi` (inclusive),
/// both given as byte offsets. `lo == hi` means the line falls outside
/// the selection on this row.
fn block_span(line: &str, c_lo: usize, c_hi: usize) -> (usize, usize) {
    let len = line.len();
    if c_lo >= len {
        return (len, len);
    }
    let lo = buffer::align_to_grapheme_start(line, c_lo);
    let hi = if c_hi >= len {
        len
    } else {
        let aligned = buffer::align_to_grapheme_start(line, c_hi);
        buffer::next_grapheme_start(line, aligned).min(len)
    };
    (lo, hi)
}

fn split_to_lines(text: &str) -> Vec<String> {
    text.split('\n').map(|s| s.to_string()).collect()
}

fn strip_single_line_breaks(text: &str) -> Cow<'_, str> {
    if !text.contains(['\n', '\r']) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if !matches!(ch, '\n' | '\r') {
            out.push(ch);
        }
    }
    Cow::Owned(out)
}

fn single_line_prefix_len(text: &str, offset: usize) -> usize {
    text[..offset.min(text.len())]
        .chars()
        .filter(|ch| !matches!(ch, '\n' | '\r'))
        .map(char::len_utf8)
        .sum()
}

fn single_line_yank(yank: &Yank) -> Yank {
    if yank.lines.is_empty() {
        return Yank::default();
    }
    Yank {
        lines: vec![strip_single_line_breaks(&yank.lines.join("\n")).into_owned()],
        kind: match yank.kind {
            YankKind::Blockwise => YankKind::Charwise,
            kind => kind,
        },
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SurroundPair {
    open: char,
    close: char,
}

fn surround_pair(delimiter: char) -> SurroundPair {
    match delimiter {
        '(' | ')' | 'b' => SurroundPair {
            open: '(',
            close: ')',
        },
        '[' | ']' => SurroundPair {
            open: '[',
            close: ']',
        },
        '{' | '}' | 'B' => SurroundPair {
            open: '{',
            close: '}',
        },
        '<' | '>' => SurroundPair {
            open: '<',
            close: '>',
        },
        _ => SurroundPair {
            open: delimiter,
            close: delimiter,
        },
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TextRunClass {
    Whitespace,
    Keyword,
    Other,
    BigWord,
}

impl TextRunClass {
    fn is_whitespace(self) -> bool {
        matches!(self, TextRunClass::Whitespace)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TextRun {
    class: TextRunClass,
    start: usize,
    end: usize,
}

impl TextRun {
    fn is_whitespace(self) -> bool {
        self.class.is_whitespace()
    }
}

fn resolve_text_object(
    buf: &TextBuffer,
    cursor: Cursor,
    object: TextObject,
    count: u32,
) -> Option<((usize, usize), (usize, usize))> {
    let count = count.max(1) as usize;
    let text = buf.text();
    let offset = buf.rowcol_to_offset(cursor.row, cursor.col) as usize;
    let (start, end) = match object {
        TextObject::Word { around, big } => {
            Some(word_text_object_offsets(&text, offset, count, big, around))
        }
        TextObject::Quote { around, delimiter } => {
            quote_text_object_offsets(buf, cursor, delimiter, around)
        }
        TextObject::Pair { around, kind } => {
            pair_text_object_offsets(&text, offset, kind, around, count)
        }
        TextObject::Paragraph { around } => {
            paragraph_text_object_offsets(buf, cursor.row, around, count)
        }
    }?;
    Some((
        buf.offset_to_rowcol(start as u32),
        buf.offset_to_rowcol(end as u32),
    ))
}

/// Build an `ip` / `ap` text object around the line `row`. Paragraphs
/// are runs of consecutive non-blank *or* blank lines; `around`
/// extends the range with the trailing blank (or non-blank) block.
fn paragraph_text_object_offsets(
    buf: &TextBuffer,
    row: usize,
    around: bool,
    count: usize,
) -> Option<(usize, usize)> {
    let line_count = buf.line_count();
    if line_count == 0 {
        return None;
    }
    let max_row = line_count - 1;
    let row = row.min(max_row);
    let starts_blank = line_is_blank(buf, row);

    let mut start = row;
    while start > 0 && line_is_blank(buf, start - 1) == starts_blank {
        start -= 1;
    }
    let mut end = row;
    while end < max_row && line_is_blank(buf, end + 1) == starts_blank {
        end += 1;
    }

    // Extend to `count` paragraphs (same-class + trailing opposite-class runs).
    let mut remaining = count.saturating_sub(1);
    while remaining > 0 && end < max_row {
        let next_class = line_is_blank(buf, end + 1);
        let mut r = end + 1;
        while r < max_row && line_is_blank(buf, r + 1) == next_class {
            r += 1;
        }
        end = r;
        remaining -= 1;
    }

    if around {
        // Add the following opposite-class run. If there is none, fall
        // back to the preceding opposite-class run.
        if end < max_row && line_is_blank(buf, end + 1) != starts_blank {
            end += 1;
            while end < max_row && line_is_blank(buf, end + 1) != starts_blank {
                end += 1;
            }
        } else if start > 0 && line_is_blank(buf, start - 1) != starts_blank {
            start -= 1;
            while start > 0 && line_is_blank(buf, start - 1) != starts_blank {
                start -= 1;
            }
        }
    }

    let start_off = buf.line_start(start) as usize;
    // Include each selected row's trailing newline so `dap`/`dip`
    // behave linewise (consuming whole rows including terminators).
    let end_off = if end + 1 < line_count {
        buf.line_start(end + 1) as usize
    } else {
        buf.len()
    };
    Some((start_off, end_off))
}

fn line_is_blank(buf: &TextBuffer, row: usize) -> bool {
    buf.line(row).as_ref().trim().is_empty()
}

fn is_valid_mark(ch: char) -> bool {
    ch.is_ascii_lowercase()
}

fn case_transform_text(transform: CaseTransform, text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match transform {
            CaseTransform::Upper => out.extend(ch.to_uppercase()),
            CaseTransform::Lower => out.extend(ch.to_lowercase()),
            CaseTransform::Toggle => {
                if ch.is_uppercase() {
                    out.extend(ch.to_lowercase());
                } else if ch.is_lowercase() {
                    out.extend(ch.to_uppercase());
                } else {
                    out.push(ch);
                }
            }
        }
    }
    out
}

/// Vim's `<C-a>` / `<C-x>` scans for a non-negative decimal run either
/// at the cursor or rightward on the same line. Negative sign handling
/// matches nvim: if the digit run is immediately preceded by `-`, the
/// sign is included.
fn find_number_from(line: &str, col: usize) -> Option<(usize, usize, i64)> {
    if line.is_empty() {
        return None;
    }
    let bytes = line.as_bytes();
    let mut i = col.min(line.len());
    while i < bytes.len() && !bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i >= bytes.len() {
        return None;
    }
    let mut end = i;
    while end < bytes.len() && bytes[end].is_ascii_digit() {
        end += 1;
    }
    let mut start = i;
    while start > 0 && bytes[start - 1].is_ascii_digit() {
        start -= 1;
    }
    if start > 0 && bytes[start - 1] == b'-' {
        start -= 1;
    }
    let value: i64 = line[start..end].parse().ok()?;
    Some((start, end, value))
}

fn word_text_object_offsets(
    text: &str,
    offset: usize,
    count: usize,
    big: bool,
    around: bool,
) -> (usize, usize) {
    let offset = offset.min(text.len());
    if text.is_empty() || offset == text.len() {
        return (offset, offset);
    }

    let runs = word_runs(text, big);
    let Some(idx) = runs
        .iter()
        .position(|run| run.start <= offset && offset < run.end)
    else {
        return (offset, offset);
    };

    if around {
        around_word_offsets(&runs, idx, count)
    } else {
        inner_word_offsets(&runs, idx, count)
    }
}

fn word_runs(text: &str, big: bool) -> Vec<TextRun> {
    let mut runs: Vec<TextRun> = Vec::new();
    for (start, grapheme) in text.grapheme_indices(true) {
        let class = word_run_class(grapheme, big);
        let end = start + grapheme.len();
        if let Some(last) = runs.last_mut() {
            if last.class == class {
                last.end = end;
                continue;
            }
        }
        runs.push(TextRun { class, start, end });
    }
    runs
}

fn word_run_class(grapheme: &str, big: bool) -> TextRunClass {
    match cursor::class_of(grapheme) {
        cursor::Class::Whitespace => TextRunClass::Whitespace,
        cursor::Class::Keyword if big => TextRunClass::BigWord,
        cursor::Class::Other if big => TextRunClass::BigWord,
        cursor::Class::Keyword => TextRunClass::Keyword,
        cursor::Class::Other => TextRunClass::Other,
    }
}

fn inner_word_offsets(runs: &[TextRun], idx: usize, count: usize) -> (usize, usize) {
    let start = runs[idx].start;
    let mut end_idx = idx;
    for _ in 1..count.max(1) {
        let Some(next_idx) = next_run_idx(runs, end_idx) else {
            break;
        };
        end_idx = next_idx;
    }
    (start, runs[end_idx].end)
}

fn around_word_offsets(runs: &[TextRun], idx: usize, count: usize) -> (usize, usize) {
    let count = count.max(1);
    if runs[idx].is_whitespace() {
        if let Some(first_word_idx) = next_word_run_idx(runs, idx) {
            let last_word_idx = advance_word_runs(runs, first_word_idx, count - 1);
            return (runs[idx].start, runs[last_word_idx].end);
        }
        if let Some(last_word_idx) = prev_word_run_idx(runs, idx) {
            let first_word_idx = retreat_word_runs(runs, last_word_idx, count - 1);
            return (runs[first_word_idx].start, runs[idx].end);
        }
        return (runs[idx].start, runs[idx].end);
    }

    let first_word_idx = idx;
    let last_word_idx = advance_word_runs(runs, first_word_idx, count - 1);
    let mut start = runs[first_word_idx].start;
    let end = with_trailing_whitespace(runs, last_word_idx);
    if end == runs[last_word_idx].end {
        if let Some(ws_before_idx) = immediate_whitespace_before(runs, first_word_idx) {
            start = runs[ws_before_idx].start;
        }
    }
    (start, end)
}

fn next_run_idx(runs: &[TextRun], idx: usize) -> Option<usize> {
    idx.checked_add(1).filter(|i| *i < runs.len())
}

fn next_word_run_idx(runs: &[TextRun], idx: usize) -> Option<usize> {
    ((idx + 1)..runs.len()).find(|i| !runs[*i].is_whitespace())
}

fn prev_word_run_idx(runs: &[TextRun], idx: usize) -> Option<usize> {
    (0..idx).rev().find(|i| !runs[*i].is_whitespace())
}

fn advance_word_runs(runs: &[TextRun], idx: usize, additional_words: usize) -> usize {
    let mut word_idx = idx;
    for _ in 0..additional_words {
        let Some(next_idx) = next_word_run_idx(runs, word_idx) else {
            break;
        };
        word_idx = next_idx;
    }
    word_idx
}

fn retreat_word_runs(runs: &[TextRun], idx: usize, additional_words: usize) -> usize {
    let mut word_idx = idx;
    for _ in 0..additional_words {
        let Some(prev_idx) = prev_word_run_idx(runs, word_idx) else {
            break;
        };
        word_idx = prev_idx;
    }
    word_idx
}

fn immediate_whitespace_before(runs: &[TextRun], idx: usize) -> Option<usize> {
    idx.checked_sub(1).filter(|i| runs[*i].is_whitespace())
}

fn with_trailing_whitespace(runs: &[TextRun], idx: usize) -> usize {
    if let Some(ws_idx) = next_run_idx(runs, idx).filter(|i| runs[*i].is_whitespace()) {
        runs[ws_idx].end
    } else {
        runs[idx].end
    }
}

fn quote_text_object_offsets(
    buf: &TextBuffer,
    cursor: Cursor,
    delimiter: char,
    around: bool,
) -> Option<(usize, usize)> {
    let line = buf.line(cursor.row);
    let line = line.as_ref();
    let line_start = buf.line_start(cursor.row) as usize;
    let col = cursor.col.min(line.len());
    let (start, end) = quote_pair_in_line(line, col, delimiter)?;
    if around {
        let mut start = start;
        let mut end = end;
        let trailing = extend_whitespace_forward(line, end);
        if trailing > end {
            end = trailing;
        } else {
            start = extend_whitespace_backward(line, start);
        }
        Some((line_start + start, line_start + end))
    } else {
        let width = delimiter.len_utf8();
        Some((line_start + start + width, line_start + end - width))
    }
}

fn quote_pair_in_line(line: &str, col: usize, delimiter: char) -> Option<(usize, usize)> {
    let mut open: Option<usize> = None;
    let mut pairs = Vec::new();
    for (idx, ch) in line.char_indices() {
        if ch != delimiter || is_escaped_delimiter(line, idx) {
            continue;
        }
        if let Some(open_idx) = open.take() {
            pairs.push((open_idx, idx + ch.len_utf8()));
        } else {
            open = Some(idx);
        }
    }

    let col = col.min(line.len());
    pairs.into_iter().find(|(start, end)| {
        let close_start = end.saturating_sub(delimiter.len_utf8());
        *start <= col && col <= close_start
    })
}

fn is_escaped_delimiter(line: &str, idx: usize) -> bool {
    let bytes = line.as_bytes();
    let mut slash_count = 0;
    let mut i = idx;
    while i > 0 {
        i -= 1;
        if bytes[i] != b'\\' {
            break;
        }
        slash_count += 1;
    }
    slash_count % 2 == 1
}

fn extend_whitespace_forward(line: &str, mut col: usize) -> usize {
    while col < line.len() && cursor::class_at(line, col) == cursor::Class::Whitespace {
        col = buffer::next_grapheme_start(line, col);
    }
    col
}

fn extend_whitespace_backward(line: &str, mut col: usize) -> usize {
    while col > 0 {
        let prev = buffer::prev_grapheme_start(line, col);
        if cursor::class_at(line, prev) != cursor::Class::Whitespace {
            break;
        }
        col = prev;
    }
    col
}

fn pair_text_object_offsets(
    text: &str,
    offset: usize,
    kind: PairKind,
    around: bool,
    count: usize,
) -> Option<(usize, usize)> {
    let (open, close) = pair_delimiters(kind);
    let mut stack = Vec::new();
    let mut containing = Vec::new();

    for (idx, ch) in text.char_indices() {
        if ch == open {
            stack.push(idx);
        } else if ch == close {
            if let Some(start) = stack.pop() {
                if start <= offset && offset <= idx {
                    containing.push((start, idx + ch.len_utf8()));
                }
            }
        }
    }

    let (start, end) = *containing.get(count.checked_sub(1)?)?;
    if around {
        Some((start, end))
    } else {
        Some((start + open.len_utf8(), end - close.len_utf8()))
    }
}

fn pair_delimiters(kind: PairKind) -> (char, char) {
    match kind {
        PairKind::Paren => ('(', ')'),
        PairKind::Bracket => ('[', ']'),
        PairKind::Brace => ('{', '}'),
        PairKind::Angle => ('<', '>'),
    }
}

pub struct VimEditor {
    buf: TextBuffer,
    cursor: Cursor,
    mode: Mode,
    selection: Option<Selection>,
    yank: Yank,
    history: History,
    width: u16,
    wrap: bool,
    scroll_offset: u16,
    last_viewport_h: u16,
    single_line: bool,
    dirty: bool,
    text_version: u64,
    desired_display_col: u16,
    tab_settings: TabSettings,
    marks: HashMap<char, u32>,
    pending_block_change: Option<PendingBlockChange>,
    pending: Pending,
    pending_count: Option<u32>,
}

impl VimEditor {
    pub fn new() -> Self {
        Self {
            buf: TextBuffer::new(),
            cursor: Cursor::ORIGIN,
            mode: Mode::Insert,
            selection: None,
            yank: Yank::default(),
            history: History::new(),
            width: 1,
            wrap: true,
            scroll_offset: 0,
            last_viewport_h: 8,
            single_line: false,
            dirty: true,
            text_version: 0,
            desired_display_col: 0,
            tab_settings: TabSettings::default(),
            marks: HashMap::new(),
            pending_block_change: None,
            pending: Pending::None,
            pending_count: None,
        }
    }

    fn set_mode_ctx(&mut self, mode: Mode) {
        self.mode = mode;
    }

    fn reset_to_primary_mode(&mut self) {
        self.set_mode_ctx(Mode::Normal);
    }

    fn effective_wrap(&self) -> bool {
        self.wrap && !self.single_line
    }

    fn half_page_rows(&self) -> u32 {
        (u32::from(self.last_viewport_h) / 2).max(1)
    }

    fn cursor_display(&self) -> (usize, u16) {
        let line = self.buf.line(self.cursor.row);
        (
            self.cursor.row,
            cursor_display_col(
                line.as_ref(),
                self.cursor.col,
                self.mode,
                self.tab_settings.tabstop,
            ),
        )
    }

    pub fn execute(&mut self, action: VimAction, count: u32, captured: Option<char>) {
        self.execute_inner(action, count, captured);
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn text(&self) -> String {
        self.buf.text()
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn clamp_offset(&self, offset: usize) -> usize {
        self.buf.clamp_offset(offset)
    }

    pub fn line_count(&self) -> usize {
        self.buf.line_count()
    }

    pub fn line(&self, row: usize) -> Cow<'_, str> {
        self.buf.line(row)
    }

    pub fn line_start(&self, row: usize) -> usize {
        self.buf.line_start(row) as usize
    }

    pub fn cursor_offset(&self) -> usize {
        self.buf.rowcol_to_offset(self.cursor.row, self.cursor.col) as usize
    }

    pub fn set_cursor_offset(&mut self, offset: usize) {
        let (row, col) = self.buf.offset_to_rowcol(offset.min(self.buf.len()) as u32);
        self.cursor = Cursor { row, col };
        self.fixup_cursor();
        self.update_desired_display_col();
        if self.mode.is_visual() {
            self.update_selection_head();
        }
    }

    pub fn set_text(&mut self, text: &str, mode: Mode, at_end: bool) {
        self.buf.set_text(text);
        self.cursor = if at_end {
            let (row, col) = self.buf.offset_to_rowcol(self.buf.len() as u32);
            Cursor { row, col }
        } else {
            Cursor::ORIGIN
        };
        self.mode = mode;
        self.selection = None;
        self.pending_block_change = None;
        self.pending = Pending::None;
        self.pending_count = None;
        self.history.reset();
        self.marks.clear();
        self.desired_display_col = 0;
        self.dirty = true;
        self.text_version = self.text_version.wrapping_add(1);
        self.fixup_cursor();
    }

    pub fn set_layout(&mut self, width: u16, viewport_rows: u16) {
        self.width = width.max(1);
        self.last_viewport_h = viewport_rows.max(1);
    }

    pub fn insert_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let text = self.normalize_text_for_mode(text).into_owned();
        let offset = self.cursor_offset() as u32;
        self.commit(Edit::insert(offset, text.clone()));
        let (row, col) = self.buf.offset_to_rowcol(offset + text.len() as u32);
        self.cursor = Cursor { row, col };
        self.update_desired_display_col();
    }

    pub fn replace_offsets(&mut self, range: Range<usize>, text: &str) {
        let start = range.start.min(self.buf.len());
        let end = range.end.min(self.buf.len()).max(start);
        self.commit(Edit::replace(
            start as u32,
            (end - start) as u32,
            text.to_string(),
        ));
        let (row, col) = self.buf.offset_to_rowcol((start + text.len()) as u32);
        self.cursor = Cursor { row, col };
        self.fixup_cursor();
        self.update_desired_display_col();
    }

    pub fn slice(&self, range: Range<usize>) -> Cow<'_, str> {
        let start = range.start.min(self.buf.len());
        let end = range.end.min(self.buf.len()).max(start);
        self.buf.slice(start..end)
    }

    pub fn offset_to_rowcol(&self, offset: usize) -> (usize, usize) {
        self.buf.offset_to_rowcol(offset.min(self.buf.len()) as u32)
    }

    pub fn rowcol_to_offset(&self, row: usize, col: usize) -> usize {
        self.buf.rowcol_to_offset(row, col) as usize
    }

    pub fn text_version(&self) -> u64 {
        self.text_version
    }

    pub fn is_blank(&self) -> bool {
        let (before, after) = self.buf.as_slices();
        before.chars().chain(after.chars()).all(char::is_whitespace)
    }

    pub fn previous_offset(&self, offset: usize) -> usize {
        let offset = offset.min(self.buf.len());
        let (row, col) = self.buf.offset_to_rowcol(offset as u32);
        if col == 0 {
            return offset.saturating_sub(1);
        }
        let line = self.buf.line(row);
        self.buf.line_start(row) as usize + buffer::prev_grapheme_start(line.as_ref(), col)
    }

    pub fn next_offset(&self, offset: usize) -> usize {
        let offset = offset.min(self.buf.len());
        let (row, col) = self.buf.offset_to_rowcol(offset as u32);
        let line = self.buf.line(row);
        if col >= line.len() {
            return (offset + usize::from(offset < self.buf.len())).min(self.buf.len());
        }
        self.buf.line_start(row) as usize + buffer::next_grapheme_start(line.as_ref(), col)
    }

    pub fn offset_from_utf16(&self, target: usize) -> usize {
        let (before, after) = self.buf.as_slices();
        let mut bytes = 0;
        let mut utf16 = 0;
        for ch in before.chars().chain(after.chars()) {
            if utf16 >= target {
                break;
            }
            bytes += ch.len_utf8();
            utf16 += ch.len_utf16();
        }
        bytes
    }

    pub fn offset_to_utf16(&self, target: usize) -> usize {
        let target = target.min(self.buf.len());
        let (before, after) = self.buf.as_slices();
        if target <= before.len() {
            before[..target].encode_utf16().count()
        } else {
            before.encode_utf16().count() + after[..target - before.len()].encode_utf16().count()
        }
    }

    pub fn send_key(&mut self, key: VimKey) -> bool {
        if key == VimKey::Escape {
            let had_pending = self.pending != Pending::None || self.pending_count.is_some();
            self.pending = Pending::None;
            self.pending_count = None;
            if self.mode == Mode::Insert {
                self.execute(VimAction::ExitInsert, 1, None);
            } else if self.mode.is_visual() {
                self.execute(VimAction::ExitVisual, 1, None);
            }
            return had_pending || self.mode != Mode::Insert;
        }

        if self.mode == Mode::Insert {
            let action = match key {
                VimKey::Backspace => VimAction::BackspaceDelete,
                VimKey::Enter => VimAction::InsertNewline,
                VimKey::Tab => VimAction::InsertTab,
                VimKey::Left => VimAction::Motion(Motion::Left),
                VimKey::Right => VimAction::Motion(Motion::Right),
                VimKey::Up => VimAction::Motion(Motion::Up),
                VimKey::Down => VimAction::Motion(Motion::Down),
                VimKey::Home => VimAction::Motion(Motion::LineStart),
                VimKey::End => VimAction::Motion(Motion::LineEnd),
                _ => return false,
            };
            self.execute(action, 1, None);
            return true;
        }

        if let Pending::Capture(action) = self.pending {
            self.pending = Pending::None;
            let count = self.take_count();
            self.execute(action, count, key_character(key));
            return true;
        }

        if self.mode == Mode::Normal
            && let VimKey::Char(ch @ '0'..='9') = key
            && (ch != '0' || self.pending_count.is_some())
        {
            let digit = ch.to_digit(10).expect("ASCII digit");
            let count = self.pending_count.unwrap_or(0);
            self.pending_count = Some(count.saturating_mul(10).saturating_add(digit));
            return true;
        }

        let consumed = match self.pending {
            Pending::Operator(operator) => self.handle_operator_key(operator, key),
            Pending::G(operator) => self.handle_g_key(operator, key),
            Pending::TextObject { operator, around } => {
                self.handle_text_object_key(operator, around, key)
            }
            Pending::None if self.mode.is_visual() => self.handle_visual_key(key),
            Pending::None => self.handle_normal_key(key),
            Pending::Capture(_) => unreachable!(),
        };
        if !consumed {
            self.pending = Pending::None;
            self.pending_count = None;
        }
        consumed
    }

    fn take_count(&mut self) -> u32 {
        self.pending_count.take().unwrap_or(1).max(1)
    }

    fn dispatch(&mut self, action: VimAction) {
        self.pending = Pending::None;
        if action_needs_capture(action) {
            self.pending = Pending::Capture(action);
        } else {
            let count = self.take_count();
            self.execute(action, count, None);
        }
    }

    fn handle_normal_key(&mut self, key: VimKey) -> bool {
        let action = match key {
            VimKey::Left | VimKey::Char('h') => VimAction::Motion(Motion::Left),
            VimKey::Right | VimKey::Char('l') => VimAction::Motion(Motion::Right),
            VimKey::Up | VimKey::Char('k') => VimAction::Motion(Motion::Up),
            VimKey::Down | VimKey::Char('j') => VimAction::Motion(Motion::Down),
            VimKey::Home | VimKey::Char('0') => VimAction::Motion(Motion::LineStart),
            VimKey::End | VimKey::Char('$') => VimAction::Motion(Motion::LineEnd),
            VimKey::Char('^') => VimAction::Motion(Motion::LineFirstNonblank),
            VimKey::Char('w') => VimAction::Motion(Motion::WordForward),
            VimKey::Char('b') => VimAction::Motion(Motion::WordBack),
            VimKey::Char('e') => VimAction::Motion(Motion::WordEnd),
            VimKey::Char('G') => VimAction::Motion(Motion::LastLine),
            VimKey::Char('{') => VimAction::Motion(Motion::ParagraphBack),
            VimKey::Char('}') => VimAction::Motion(Motion::ParagraphForward),
            VimKey::Char('i') => VimAction::EnterInsert,
            VimKey::Char('a') => VimAction::EnterInsertAfter,
            VimKey::Char('I') => VimAction::EnterInsertFirstNonblank,
            VimKey::Char('A') => VimAction::EnterInsertEol,
            VimKey::Char('o') => VimAction::OpenBelow,
            VimKey::Char('O') => VimAction::OpenAbove,
            VimKey::Char('v') => VimAction::EnterVisualChar,
            VimKey::Char('V') => VimAction::EnterVisualLine,
            VimKey::Control('v') => VimAction::EnterVisualBlock,
            VimKey::Char('x') => VimAction::DeleteChar,
            VimKey::Char('s') => VimAction::Substitute,
            VimKey::Char('S') => VimAction::SubstituteLine,
            VimKey::Char('J') => VimAction::JoinLines,
            VimKey::Char('p') => VimAction::PasteAfter,
            VimKey::Char('P') => VimAction::PasteBefore,
            VimKey::Char('u') => VimAction::Undo,
            VimKey::Control('r') => VimAction::Redo,
            VimKey::Char('D') | VimKey::Control('k') => VimAction::DeleteMotion(Motion::LineEnd),
            VimKey::Char('C') => VimAction::ChangeMotion(Motion::LineEnd),
            VimKey::Char('Y') => VimAction::YankMotion(Motion::LineEnd),
            VimKey::Char('~') => VimAction::ToggleCaseChar,
            VimKey::Control('a') => VimAction::IncrementNumber,
            VimKey::Control('x') => VimAction::DecrementNumber,
            VimKey::Control('d') => VimAction::Motion(Motion::HalfPageDown),
            VimKey::Control('u') => VimAction::Motion(Motion::HalfPageUp),
            VimKey::Char('r') => VimAction::Replace,
            VimKey::Char('m') => VimAction::SetMark,
            VimKey::Char('f') => VimAction::Motion(Motion::FindChar {
                forward: true,
                till: false,
            }),
            VimKey::Char('F') => VimAction::Motion(Motion::FindChar {
                forward: false,
                till: false,
            }),
            VimKey::Char('t') => VimAction::Motion(Motion::FindChar {
                forward: true,
                till: true,
            }),
            VimKey::Char('T') => VimAction::Motion(Motion::FindChar {
                forward: false,
                till: true,
            }),
            VimKey::Char('`') => VimAction::Motion(Motion::Mark { linewise: false }),
            VimKey::Char('\'') => VimAction::Motion(Motion::Mark { linewise: true }),
            VimKey::Char('d') => {
                self.pending = Pending::Operator(PendingOperator::Delete);
                return true;
            }
            VimKey::Char('c') => {
                self.pending = Pending::Operator(PendingOperator::Change);
                return true;
            }
            VimKey::Char('y') => {
                self.pending = Pending::Operator(PendingOperator::Yank);
                return true;
            }
            VimKey::Char('g') => {
                self.pending = Pending::G(None);
                return true;
            }
            VimKey::Char('/') => {
                self.pending_count = None;
                return true;
            }
            _ => return false,
        };
        self.dispatch(action);
        true
    }

    fn handle_visual_key(&mut self, key: VimKey) -> bool {
        let action = match key {
            VimKey::Left | VimKey::Char('h') => VimAction::Motion(Motion::Left),
            VimKey::Right | VimKey::Char('l') => VimAction::Motion(Motion::Right),
            VimKey::Up | VimKey::Char('k') => VimAction::Motion(Motion::Up),
            VimKey::Down | VimKey::Char('j') => VimAction::Motion(Motion::Down),
            VimKey::Home | VimKey::Char('0') => VimAction::Motion(Motion::LineStart),
            VimKey::End | VimKey::Char('$') => VimAction::Motion(Motion::LineEnd),
            VimKey::Char('^') => VimAction::Motion(Motion::LineFirstNonblank),
            VimKey::Char('w') => VimAction::Motion(Motion::WordForward),
            VimKey::Char('b') => VimAction::Motion(Motion::WordBack),
            VimKey::Char('e') => VimAction::Motion(Motion::WordEnd),
            VimKey::Char('G') => VimAction::Motion(Motion::LastLine),
            VimKey::Char('{') => VimAction::Motion(Motion::ParagraphBack),
            VimKey::Char('}') => VimAction::Motion(Motion::ParagraphForward),
            VimKey::Control('d') => VimAction::Motion(Motion::HalfPageDown),
            VimKey::Control('u') => VimAction::Motion(Motion::HalfPageUp),
            VimKey::Char('v') => VimAction::EnterVisualChar,
            VimKey::Char('V') => VimAction::EnterVisualLine,
            VimKey::Control('v') => VimAction::EnterVisualBlock,
            VimKey::Char('d') | VimKey::Char('x') => VimAction::DeleteSelection,
            VimKey::Char('c') => VimAction::ChangeSelection,
            VimKey::Char('s') | VimKey::Char('S') => VimAction::SurroundSelection,
            VimKey::Char('y') => VimAction::YankSelection,
            VimKey::Char('u') => VimAction::CaseSelection(CaseTransform::Lower),
            VimKey::Char('U') => VimAction::CaseSelection(CaseTransform::Upper),
            VimKey::Char('~') => VimAction::CaseSelection(CaseTransform::Toggle),
            VimKey::Char('m') => VimAction::SetMark,
            VimKey::Char('f') => VimAction::Motion(Motion::FindChar {
                forward: true,
                till: false,
            }),
            VimKey::Char('F') => VimAction::Motion(Motion::FindChar {
                forward: false,
                till: false,
            }),
            VimKey::Char('t') => VimAction::Motion(Motion::FindChar {
                forward: true,
                till: true,
            }),
            VimKey::Char('T') => VimAction::Motion(Motion::FindChar {
                forward: false,
                till: true,
            }),
            VimKey::Char('`') => VimAction::Motion(Motion::Mark { linewise: false }),
            VimKey::Char('\'') => VimAction::Motion(Motion::Mark { linewise: true }),
            VimKey::Char('i') | VimKey::Char('a') => {
                self.pending = Pending::TextObject {
                    operator: None,
                    around: matches!(key, VimKey::Char('a')),
                };
                return true;
            }
            VimKey::Char('g') => {
                self.pending = Pending::G(None);
                return true;
            }
            _ => return false,
        };
        self.dispatch(action);
        true
    }

    fn handle_operator_key(&mut self, operator: PendingOperator, key: VimKey) -> bool {
        if let VimKey::Char(ch) = key {
            if operator_double_key(operator) == ch {
                self.dispatch(operator_line_action(operator));
                return true;
            }
            if matches!(ch, 'i' | 'a') {
                self.pending = Pending::TextObject {
                    operator: Some(operator),
                    around: ch == 'a',
                };
                return true;
            }
            if ch == 'g' {
                self.pending = Pending::G(Some(operator));
                return true;
            }
        }
        let Some(motion) = motion_for_key(key) else {
            self.pending = Pending::None;
            self.pending_count = None;
            return true;
        };
        self.dispatch(operator_motion_action(operator, motion));
        true
    }

    fn handle_g_key(&mut self, operator: Option<PendingOperator>, key: VimKey) -> bool {
        let action = match (operator, key) {
            (Some(operator), VimKey::Char('g')) => {
                operator_motion_action(operator, Motion::FirstLine)
            }
            (None, VimKey::Char('g')) => VimAction::Motion(Motion::FirstLine),
            (None, VimKey::Char('j')) => VimAction::Motion(Motion::DisplayDown),
            (None, VimKey::Char('k')) => VimAction::Motion(Motion::DisplayUp),
            (None, VimKey::Char('U')) => {
                self.pending = Pending::Operator(PendingOperator::Case(CaseTransform::Upper));
                return true;
            }
            (None, VimKey::Char('u')) => {
                self.pending = Pending::Operator(PendingOperator::Case(CaseTransform::Lower));
                return true;
            }
            (None, VimKey::Char('~')) => {
                self.pending = Pending::Operator(PendingOperator::Case(CaseTransform::Toggle));
                return true;
            }
            _ => {
                self.pending = Pending::None;
                self.pending_count = None;
                return true;
            }
        };
        self.dispatch(action);
        true
    }

    fn handle_text_object_key(
        &mut self,
        operator: Option<PendingOperator>,
        around: bool,
        key: VimKey,
    ) -> bool {
        let Some(ch) = key_character(key) else {
            self.pending = Pending::None;
            self.pending_count = None;
            return true;
        };
        let Some(object) = text_object_for_char(around, ch) else {
            self.pending = Pending::None;
            self.pending_count = None;
            return true;
        };
        let action = match operator {
            Some(operator) => operator_text_object_action(operator, object),
            None => VimAction::SelectTextObject(object),
        };
        self.dispatch(action);
        true
    }

    pub fn visual_ranges(&self) -> Vec<Range<usize>> {
        let Some(selection) = self.selection else {
            return Vec::new();
        };
        match selection.kind {
            VisualKind::Char => {
                let (start, end) = char_range(&selection);
                let end_line = self.buf.line(end.row);
                let end_col = buffer::grapheme_end(end_line.as_ref(), end.col);
                vec![
                    self.buf.rowcol_to_offset(start.row, start.col) as usize
                        ..self.buf.rowcol_to_offset(end.row, end_col) as usize,
                ]
            }
            VisualKind::Line => {
                let (start, end) = selection.rows_ordered();
                let range_start = self.buf.line_start(start) as usize;
                let range_end = if end + 1 < self.buf.line_count() {
                    self.buf.line_start(end + 1) as usize
                } else {
                    self.buf.len()
                };
                vec![range_start..range_end]
            }
            VisualKind::Block => {
                let (row_start, row_end) = selection.rows_ordered();
                let (col_start, col_end) = selection.cols_ordered();
                (row_start..=row_end)
                    .filter_map(|row| {
                        let line = self.buf.line(row);
                        let (start, end) = block_span(line.as_ref(), col_start, col_end);
                        (start < end).then(|| {
                            let base = self.buf.line_start(row) as usize;
                            base + start..base + end
                        })
                    })
                    .collect()
            }
        }
    }
}

fn action_needs_capture(action: VimAction) -> bool {
    matches!(
        action,
        VimAction::Replace
            | VimAction::SetMark
            | VimAction::SurroundSelection
            | VimAction::Motion(Motion::FindChar { .. } | Motion::Mark { .. })
            | VimAction::DeleteMotion(Motion::FindChar { .. } | Motion::Mark { .. })
            | VimAction::ChangeMotion(Motion::FindChar { .. } | Motion::Mark { .. })
            | VimAction::YankMotion(Motion::FindChar { .. } | Motion::Mark { .. })
            | VimAction::CaseMotion(_, Motion::FindChar { .. } | Motion::Mark { .. })
    )
}

fn key_character(key: VimKey) -> Option<char> {
    match key {
        VimKey::Char(ch) | VimKey::Control(ch) => Some(ch),
        VimKey::Enter => Some('\n'),
        VimKey::Tab => Some('\t'),
        _ => None,
    }
}

fn motion_for_key(key: VimKey) -> Option<Motion> {
    Some(match key {
        VimKey::Left | VimKey::Char('h') => Motion::Left,
        VimKey::Right | VimKey::Char('l') => Motion::Right,
        VimKey::Up | VimKey::Char('k') => Motion::Up,
        VimKey::Down | VimKey::Char('j') => Motion::Down,
        VimKey::Home | VimKey::Char('0') => Motion::LineStart,
        VimKey::End | VimKey::Char('$') => Motion::LineEnd,
        VimKey::Char('^') => Motion::LineFirstNonblank,
        VimKey::Char('w') => Motion::WordForward,
        VimKey::Char('b') => Motion::WordBack,
        VimKey::Char('e') => Motion::WordEnd,
        VimKey::Char('G') => Motion::LastLine,
        VimKey::Char('{') => Motion::ParagraphBack,
        VimKey::Char('}') => Motion::ParagraphForward,
        VimKey::Control('d') => Motion::HalfPageDown,
        VimKey::Control('u') => Motion::HalfPageUp,
        VimKey::Char('f') => Motion::FindChar {
            forward: true,
            till: false,
        },
        VimKey::Char('F') => Motion::FindChar {
            forward: false,
            till: false,
        },
        VimKey::Char('t') => Motion::FindChar {
            forward: true,
            till: true,
        },
        VimKey::Char('T') => Motion::FindChar {
            forward: false,
            till: true,
        },
        VimKey::Char('`') => Motion::Mark { linewise: false },
        VimKey::Char('\'') => Motion::Mark { linewise: true },
        _ => return None,
    })
}

fn operator_double_key(operator: PendingOperator) -> char {
    match operator {
        PendingOperator::Delete => 'd',
        PendingOperator::Change => 'c',
        PendingOperator::Yank => 'y',
        PendingOperator::Case(CaseTransform::Upper) => 'U',
        PendingOperator::Case(CaseTransform::Lower) => 'u',
        PendingOperator::Case(CaseTransform::Toggle) => '~',
    }
}

fn operator_motion_action(operator: PendingOperator, motion: Motion) -> VimAction {
    match operator {
        PendingOperator::Delete => VimAction::DeleteMotion(motion),
        PendingOperator::Change => VimAction::ChangeMotion(motion),
        PendingOperator::Yank => VimAction::YankMotion(motion),
        PendingOperator::Case(transform) => VimAction::CaseMotion(transform, motion),
    }
}

fn operator_text_object_action(operator: PendingOperator, object: TextObject) -> VimAction {
    match operator {
        PendingOperator::Delete => VimAction::DeleteTextObject(object),
        PendingOperator::Change => VimAction::ChangeTextObject(object),
        PendingOperator::Yank => VimAction::YankTextObject(object),
        PendingOperator::Case(transform) => VimAction::CaseTextObject(transform, object),
    }
}

fn operator_line_action(operator: PendingOperator) -> VimAction {
    match operator {
        PendingOperator::Delete => VimAction::DeleteLine,
        PendingOperator::Change => VimAction::ChangeLine,
        PendingOperator::Yank => VimAction::YankLine,
        PendingOperator::Case(transform) => VimAction::CaseLine(transform),
    }
}

fn text_object_for_char(around: bool, ch: char) -> Option<TextObject> {
    Some(match ch {
        'w' => TextObject::Word { around, big: false },
        'W' => TextObject::Word { around, big: true },
        '"' | '\'' | '`' => TextObject::Quote {
            around,
            delimiter: ch,
        },
        '(' | ')' | 'b' => TextObject::Pair {
            around,
            kind: PairKind::Paren,
        },
        '[' | ']' => TextObject::Pair {
            around,
            kind: PairKind::Bracket,
        },
        '{' | '}' | 'B' => TextObject::Pair {
            around,
            kind: PairKind::Brace,
        },
        '<' | '>' => TextObject::Pair {
            around,
            kind: PairKind::Angle,
        },
        'p' => TextObject::Paragraph { around },
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::{Mode, VimEditor, VimKey};

    fn normal(text: &str) -> VimEditor {
        let mut editor = VimEditor::new();
        editor.set_text(text, Mode::Normal, false);
        editor
    }

    fn keys(editor: &mut VimEditor, sequence: &str) {
        for ch in sequence.chars() {
            assert!(editor.send_key(VimKey::Char(ch)), "unhandled key {ch:?}");
        }
    }

    #[test]
    fn routes_counts_and_linewise_operators() {
        let mut editor = normal("one\ntwo\nthree\nfour");
        keys(&mut editor, "2dd");

        assert_eq!(editor.text(), "three\nfour");
        assert_eq!(editor.mode(), Mode::Normal);
        assert_eq!(editor.cursor_offset(), 0);
    }

    #[test]
    fn change_text_object_enters_insert_and_undo_is_transactional() {
        let mut editor = normal("one two");
        editor.set_cursor_offset(1);
        keys(&mut editor, "ciw");
        assert_eq!(editor.mode(), Mode::Insert);

        editor.insert_text("changed");
        assert!(editor.send_key(VimKey::Escape));
        assert_eq!(editor.text(), "changed two");
        assert_eq!(editor.mode(), Mode::Normal);

        keys(&mut editor, "u");
        assert_eq!(editor.text(), "one two");
        assert!(editor.send_key(VimKey::Control('r')));
        assert_eq!(editor.text(), "changed two");
    }

    #[test]
    fn supports_captured_find_replace_and_marks() {
        let mut editor = normal("abc def");
        keys(&mut editor, "fd");
        assert_eq!(editor.cursor_offset(), 4);
        keys(&mut editor, "rX");
        assert_eq!(editor.text(), "abc Xef");
        keys(&mut editor, "ma0`a");
        assert_eq!(editor.cursor_offset(), 4);
    }

    #[test]
    fn visual_line_delete_uses_the_visual_selection() {
        let mut editor = normal("one\ntwo\nthree");
        keys(&mut editor, "Vjd");

        assert_eq!(editor.text(), "three");
        assert_eq!(editor.mode(), Mode::Normal);
    }

    #[test]
    fn lowercase_s_surrounds_a_visual_selection() {
        let mut editor = normal("word");
        keys(&mut editor, "ves\"");

        assert_eq!(editor.text(), "\"word\"");
        assert_eq!(editor.mode(), Mode::Normal);
    }

    #[test]
    fn slash_is_consumed_without_editing_in_normal_mode() {
        let mut editor = normal("word");

        assert!(editor.send_key(VimKey::Char('/')));
        assert_eq!(editor.text(), "word");
        assert_eq!(editor.mode(), Mode::Normal);
    }

    #[test]
    fn utf16_offsets_cross_the_gap_without_flattening_the_buffer() {
        let mut editor = normal("a😀\nβ");
        editor.set_cursor_offset(1);
        editor.replace_offsets(1..1, "x");

        assert_eq!(editor.offset_from_utf16(4), 6);
        assert_eq!(editor.offset_to_utf16(6), 4);
    }
}
