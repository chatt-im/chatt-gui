use unicode_segmentation::UnicodeSegmentation;

use super::{
    buffer::{self, TextBuffer},
    mode::Mode,
};

/// A cursor position in the buffer, identified by (row, byte_col).
///
/// `col` is a 0-based byte offset that always sits on a grapheme-cluster
/// start, never mid-cluster. In Normal / Visual* modes `col` is clamped
/// to the start byte of the last grapheme; in Insert mode it may equal
/// `line.len()`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Cursor {
    pub row: usize,
    pub col: usize,
}

impl Cursor {
    pub const ORIGIN: Cursor = Cursor { row: 0, col: 0 };
}

/// The kind of range a motion produces when used as an operator operand.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MotionKind {
    /// Range is `[start, end)` in byte order.
    CharExclusive,
    /// Range is `[start, next_grapheme(end))` — end position included.
    CharInclusive,
    /// Range covers whole lines from `min(rows)` to `max(rows)`.
    Linewise,
}

/// Classification of a grapheme cluster for word-motion purposes. The
/// classification follows nvim's default `iskeyword = @,48-57,_,192-255`
/// at the grapheme level — we categorize each cluster by its first
/// codepoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    /// Letter, digit, underscore, or latin-1 high char (keyword).
    Keyword,
    /// Space, tab, newline.
    Whitespace,
    /// Everything else (punctuation, symbols).
    Other,
}

/// Classify the grapheme cluster starting at `col` in `line`.
pub fn class_at(line: &str, col: usize) -> Class {
    let g = buffer::grapheme_at(line, col);
    class_of(g)
}

/// Classify a grapheme cluster by its first codepoint.
pub fn class_of(g: &str) -> Class {
    let Some(ch) = g.chars().next() else {
        return Class::Whitespace;
    };
    if ch.is_whitespace() {
        return Class::Whitespace;
    }
    if ch.is_alphanumeric() || ch == '_' {
        return Class::Keyword;
    }
    // Latin-1 high chars (\u{C0}..=\u{FF}) fall into `is_alphanumeric`
    // when they are letters; others land here as Other — matching
    // nvim's default for non-keyword punctuation.
    Class::Other
}

/// `h`: move left one grapheme, `n` times, without crossing line start.
pub fn motion_h(buf: &TextBuffer, c: Cursor, n: u32) -> Cursor {
    let mut col = c.col;
    let line = buf.line(c.row);
    let line = line.as_ref();
    for _ in 0..n {
        if col == 0 {
            break;
        }
        col = buffer::prev_grapheme_start(line, col);
    }
    Cursor { row: c.row, col }
}

/// `l`: move right one grapheme, `n` times, without crossing line end.
/// In Normal / Visual* modes the cursor stops on the last grapheme's
/// start byte; in Insert mode it may land at `line.len()`.
pub fn motion_l(buf: &TextBuffer, c: Cursor, n: u32, mode: Mode) -> Cursor {
    let line = buf.line(c.row);
    let line = line.as_ref();
    let bound = match mode {
        Mode::Insert => line.len(),
        _ => buffer::last_grapheme_start(line).unwrap_or(0),
    };
    let mut col = c.col;
    for _ in 0..n {
        if col >= bound {
            break;
        }
        let next = buffer::next_grapheme_start(line, col);
        if next > bound {
            break;
        }
        col = next;
    }
    Cursor { row: c.row, col }
}

/// `0`: move to first byte of the line.
pub fn motion_0(_buf: &TextBuffer, c: Cursor) -> Cursor {
    Cursor { row: c.row, col: 0 }
}

/// `^`: first non-blank column on the current line, clamped per mode.
pub fn motion_caret(buf: &TextBuffer, c: Cursor, mode: Mode) -> Cursor {
    let line = buf.line(c.row);
    let line = line.as_ref();
    let mut col = 0;
    while col < line.len() && class_at(line, col) == Class::Whitespace {
        col = buffer::next_grapheme_start(line, col);
    }
    // Empty / all-whitespace line: clamp per mode so Normal lands on
    // the last grapheme start instead of one past EOL.
    if col >= line.len() && !line.is_empty() {
        col = match mode {
            Mode::Insert => line.len(),
            _ => buffer::last_grapheme_start(line).unwrap_or(0),
        };
    }
    Cursor { row: c.row, col }
}

/// `f{char}` / `F{char}` / `t{char}` / `T{char}`: search the current
/// line for `ch`. Returns `None` if `ch` doesn't appear in the
/// requested direction within the line.
///
/// `forward` searches right of the cursor; `till` stops one grapheme
/// short of the target (so `t{char}` sits just before it).
pub fn motion_find_char(
    buf: &TextBuffer,
    c: Cursor,
    ch: char,
    n: u32,
    forward: bool,
    till: bool,
) -> Option<Cursor> {
    let line = buf.line(c.row);
    let line = line.as_ref();
    if line.is_empty() {
        return None;
    }
    let n = n.max(1) as usize;
    let start = c.col.min(line.len());
    let mut found = None;
    let mut hits = 0usize;
    if forward {
        let mut col = buffer::next_grapheme_start(line, start).min(line.len());
        while col < line.len() {
            if line[col..].starts_with(ch) {
                hits += 1;
                if hits == n {
                    found = Some(col);
                    break;
                }
            }
            col = buffer::next_grapheme_start(line, col);
        }
    } else {
        let mut col = if start > 0 {
            buffer::prev_grapheme_start(line, start)
        } else {
            return None;
        };
        loop {
            if line[col..].starts_with(ch) {
                hits += 1;
                if hits == n {
                    found = Some(col);
                    break;
                }
            }
            if col == 0 {
                break;
            }
            col = buffer::prev_grapheme_start(line, col);
        }
    }
    let target = found?;
    let landing = if till {
        if forward {
            buffer::prev_grapheme_start(line, target)
        } else {
            buffer::next_grapheme_start(line, target).min(line.len())
        }
    } else {
        target
    };
    Some(Cursor {
        row: c.row,
        col: landing,
    })
}

/// `{` / `}`: paragraph motions. Paragraphs are separated by runs of
/// blank lines. The motion lands on the *first* boundary line it
/// crosses — a blank line if any, else the first/last row of the
/// buffer.
pub fn motion_paragraph_forward(buf: &TextBuffer, c: Cursor, n: u32) -> Cursor {
    let last_row = buf.max_row();
    let mut row = c.row;
    let mut count = n.max(1) as usize;
    while count > 0 {
        let Some(next) = next_paragraph_boundary(buf, row, true) else {
            return Cursor {
                row: last_row,
                col: 0,
            };
        };
        row = next;
        count -= 1;
    }
    Cursor { row, col: 0 }
}

pub fn motion_paragraph_back(buf: &TextBuffer, c: Cursor, n: u32) -> Cursor {
    let mut row = c.row;
    let mut count = n.max(1) as usize;
    while count > 0 {
        let Some(prev) = next_paragraph_boundary(buf, row, false) else {
            return Cursor { row: 0, col: 0 };
        };
        row = prev;
        count -= 1;
    }
    Cursor { row, col: 0 }
}

fn next_paragraph_boundary(buf: &TextBuffer, from: usize, forward: bool) -> Option<usize> {
    let last_row = buf.max_row();
    let line_blank = |r: usize| -> bool { buf.line(r).as_ref().trim().is_empty() };
    if forward {
        // Skip any run of blank lines we're currently sitting on, then
        // land on the next blank line (or last line).
        let mut r = from;
        while r + 1 <= last_row && line_blank(r) {
            r += 1;
        }
        while r + 1 <= last_row {
            r += 1;
            if line_blank(r) {
                return Some(r);
            }
        }
        if r == last_row { Some(last_row) } else { None }
    } else {
        let mut r = from;
        while r > 0 && line_blank(r) {
            r -= 1;
        }
        while r > 0 {
            r -= 1;
            if line_blank(r) {
                return Some(r);
            }
        }
        if r == 0 { Some(0) } else { None }
    }
}

/// `$`: move to the last grapheme start of the current line. A count
/// first moves down `n-1` lines, then to end of that line.
pub fn motion_dollar(buf: &TextBuffer, c: Cursor, n: u32, mode: Mode) -> Cursor {
    let target_row = (c.row + (n.saturating_sub(1) as usize)).min(buf.max_row());
    let line = buf.line(target_row);
    let line = line.as_ref();
    let col = match mode {
        Mode::Insert => line.len(),
        _ => buffer::last_grapheme_start(line).unwrap_or(0),
    };
    Cursor {
        row: target_row,
        col,
    }
}

/// `j`: move down `n` lines, attempting to preserve the caller's
/// `desired_display_col` (as in vim's `curswant`). Falls back to
/// clamping at the line's last valid position.
pub fn motion_j(
    buf: &TextBuffer,
    c: Cursor,
    n: u32,
    mode: Mode,
    desired_display_col: u16,
    tabstop: u16,
) -> Cursor {
    let target_row = (c.row + n as usize).min(buf.max_row());
    let line = buf.line(target_row);
    let col = col_from_display(line.as_ref(), desired_display_col, mode, tabstop);
    Cursor {
        row: target_row,
        col,
    }
}

/// `k`: move up `n` lines, preserving `desired_display_col`.
pub fn motion_k(
    buf: &TextBuffer,
    c: Cursor,
    n: u32,
    mode: Mode,
    desired_display_col: u16,
    tabstop: u16,
) -> Cursor {
    let n = n as usize;
    let target_row = c.row.saturating_sub(n);
    let line = buf.line(target_row);
    let col = col_from_display(line.as_ref(), desired_display_col, mode, tabstop);
    Cursor {
        row: target_row,
        col,
    }
}

/// `gg`: go to line `n` (1-based), or line 1 if `None`. Cursor col
/// defaults to column 0.
pub fn motion_gg(buf: &TextBuffer, n: Option<u32>) -> Cursor {
    let target_row = match n {
        Some(v) if v >= 1 => (v as usize - 1).min(buf.max_row()),
        _ => 0,
    };
    Cursor {
        row: target_row,
        col: 0,
    }
}

/// `G`: go to line `n` (1-based), or the last line if `None`. Cursor
/// col defaults to column 0.
#[allow(non_snake_case)]
pub fn motion_G(buf: &TextBuffer, n: Option<u32>) -> Cursor {
    let target_row = match n {
        Some(v) if v >= 1 => (v as usize - 1).min(buf.max_row()),
        _ => buf.max_row(),
    };
    Cursor {
        row: target_row,
        col: 0,
    }
}

/// `w`: advance to the start of the next word, `n` times.
pub fn motion_w(buf: &TextBuffer, c: Cursor, n: u32, mode: Mode) -> Cursor {
    let mut cur = c;
    for _ in 0..n {
        match word_forward_once(buf, cur, false) {
            Some(next) => cur = next,
            None => {
                // Clamp at buffer end.
                let last_row = buf.max_row();
                let line = buf.line(last_row);
                let line = line.as_ref();
                let col = match mode {
                    Mode::Insert => line.len(),
                    _ => buffer::last_grapheme_start(line).unwrap_or(0),
                };
                return Cursor { row: last_row, col };
            }
        }
    }
    cur
}

/// `e`: advance to the end (last grapheme start) of the current or
/// next word, `n` times.
pub fn motion_e(buf: &TextBuffer, c: Cursor, n: u32) -> Cursor {
    let mut cur = c;
    for _ in 0..n {
        match word_end_once(buf, cur) {
            Some(next) => cur = next,
            None => {
                let last_row = buf.max_row();
                let line = buf.line(last_row);
                let line = line.as_ref();
                return Cursor {
                    row: last_row,
                    col: buffer::last_grapheme_start(line).unwrap_or(0),
                };
            }
        }
    }
    cur
}

/// `b`: move back to the start of the current or previous word, `n` times.
pub fn motion_b(buf: &TextBuffer, c: Cursor, n: u32) -> Cursor {
    let mut cur = c;
    for _ in 0..n {
        match word_back_once(buf, cur) {
            Some(prev) => cur = prev,
            None => return Cursor { row: 0, col: 0 },
        }
    }
    cur
}

/// Advance exactly one word. Returns `None` at end of buffer.
///
/// Rules:
/// - If current grapheme is non-whitespace, skip the rest of its
///   class-run (keyword or other).
/// - Skip whitespace (possibly crossing lines).
/// - An empty line is a word stop.
/// - The result is the first grapheme after the transition.
fn word_forward_once(buf: &TextBuffer, c: Cursor, _stop_at_empty: bool) -> Option<Cursor> {
    let mut row = c.row;
    let mut col = c.col;

    {
        let line = buf.line(row);
        let line = line.as_ref();
        if col > line.len() {
            col = line.len();
        }
    }

    {
        let line = buf.line(row);
        let line = line.as_ref();
        if col < line.len() {
            let cls = class_at(line, col);
            if cls != Class::Whitespace {
                while col < line.len() && class_at(line, col) == cls {
                    col = buffer::next_grapheme_start(line, col);
                }
            }
        }
    }

    loop {
        let line = buf.line(row);
        let line = line.as_ref();
        while col < line.len() && class_at(line, col) == Class::Whitespace {
            col = buffer::next_grapheme_start(line, col);
        }
        if col < line.len() {
            return Some(Cursor { row, col });
        }
        if row + 1 >= buf.line_count() {
            return None;
        }
        row += 1;
        col = 0;
        let new_line = buf.line(row);
        let new_line = new_line.as_ref();
        if new_line.is_empty() {
            return Some(Cursor { row, col: 0 });
        }
    }
}

/// Step to the end of the current or next word.
fn word_end_once(buf: &TextBuffer, c: Cursor) -> Option<Cursor> {
    let mut row = c.row;
    let mut col = c.col;

    // Advance at least one grapheme — `e` never stays put.
    let line = buf.line(row);
    let line = line.as_ref();
    if col < line.len() {
        col = buffer::next_grapheme_start(line, col);
    } else if row + 1 < buf.line_count() {
        row += 1;
        col = 0;
    } else {
        return None;
    }

    loop {
        let line = buf.line(row);
        let line = line.as_ref();
        while col < line.len() && class_at(line, col) == Class::Whitespace {
            col = buffer::next_grapheme_start(line, col);
        }
        if col < line.len() {
            break;
        }
        if row + 1 >= buf.line_count() {
            return None;
        }
        row += 1;
        col = 0;
    }

    let line = buf.line(row);
    let line = line.as_ref();
    let start_class = class_at(line, col);
    loop {
        let next = buffer::next_grapheme_start(line, col);
        if next >= line.len() {
            break;
        }
        let nc = class_at(line, next);
        if nc != start_class {
            break;
        }
        col = next;
    }
    Some(Cursor { row, col })
}

/// Step backward to the start of the current or previous word.
fn word_back_once(buf: &TextBuffer, c: Cursor) -> Option<Cursor> {
    let mut row = c.row;
    let mut col = c.col;

    if row == 0 && col == 0 {
        return None;
    }

    if col == 0 {
        if row == 0 {
            return None;
        }
        row -= 1;
        let line = buf.line(row);
        let line = line.as_ref();
        col = if line.is_empty() {
            // Empty line is a word; land on it.
            return Some(Cursor { row, col: 0 });
        } else {
            buffer::last_grapheme_start(line).unwrap_or(0)
        };
    } else {
        let line = buf.line(row);
        let line = line.as_ref();
        col = buffer::prev_grapheme_start(line, col);
    }

    // Skip whitespace backward across lines. An empty line is a word stop.
    loop {
        let line = buf.line(row);
        let line = line.as_ref();
        if line.is_empty() {
            return Some(Cursor { row, col: 0 });
        }
        let cls = class_at(line, col);
        if cls != Class::Whitespace {
            break;
        }
        if col == 0 {
            if row == 0 {
                return Some(Cursor { row: 0, col: 0 });
            }
            row -= 1;
            let prev_line = buf.line(row);
            let prev_line = prev_line.as_ref();
            if prev_line.is_empty() {
                return Some(Cursor { row, col: 0 });
            }
            col = buffer::last_grapheme_start(prev_line).unwrap_or(0);
            continue;
        }
        col = buffer::prev_grapheme_start(line, col);
    }

    let line = buf.line(row);
    let line = line.as_ref();
    let start_class = class_at(line, col);
    while col > 0 {
        let prev = buffer::prev_grapheme_start(line, col);
        if class_at(line, prev) != start_class {
            break;
        }
        col = prev;
    }
    Some(Cursor { row, col })
}

/// Given a target display column `target`, return the byte offset in
/// `line` of the grapheme whose display column is <= target and whose
/// next grapheme's display column is > target. Respects mode clamping.
pub fn col_from_display(line: &str, target: u16, mode: Mode, tabstop: u16) -> usize {
    if line.is_empty() {
        return 0;
    }
    let tabstop = tabstop.max(1);
    let mut acc: u16 = 0;
    let mut last_start = 0;
    for (i, g) in line.grapheme_indices(true) {
        last_start = i;
        let w = if g == "\t" {
            tabstop
                .saturating_mul((acc / tabstop).saturating_add(1))
                .saturating_sub(acc)
        } else if g.contains(char::is_control) {
            0
        } else {
            unicode_width::UnicodeWidthStr::width(g) as u16
        };
        if acc.saturating_add(w) > target {
            return i;
        }
        acc = acc.saturating_add(w);
    }
    // `target` is past the last grapheme: clamp per mode.
    match mode {
        Mode::Insert => line.len(),
        _ => last_start,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(row: usize, col: usize) -> Cursor {
        Cursor { row, col }
    }

    fn b(s: &str) -> TextBuffer {
        TextBuffer::from_str(s)
    }

    #[test]
    fn h_steps_single_grapheme() {
        let t = b("hello");
        assert_eq!(motion_h(&t, c(0, 3), 1), c(0, 2));
        assert_eq!(motion_h(&t, c(0, 3), 2), c(0, 1));
        // Does not cross line start.
        assert_eq!(motion_h(&t, c(0, 0), 1), c(0, 0));
    }

    #[test]
    fn l_steps_grapheme_in_normal_mode() {
        let t = b("abc");
        assert_eq!(motion_l(&t, c(0, 0), 1, Mode::Normal), c(0, 1));
        assert_eq!(motion_l(&t, c(0, 0), 10, Mode::Normal), c(0, 2));
        // Insert mode allows one past last grapheme.
        assert_eq!(motion_l(&t, c(0, 0), 10, Mode::Insert), c(0, 3));
    }

    #[test]
    fn l_crosses_zalgo_cluster_in_one_step() {
        // "a" + "b" with many combining marks + "c" — one `l` should cross
        // the entire Zalgo stack in a single step.
        let zalgo_b =
            "b\u{0301}\u{0300}\u{0302}\u{0303}\u{0304}\u{0305}\u{0306}\u{0307}\u{0308}\u{030A}";
        let s = format!("a{}c", zalgo_b);
        let t = b(&s);
        // From col 0 (on 'a'), one l should land on the zalgo cluster start.
        let after_one = motion_l(&t, c(0, 0), 1, Mode::Normal);
        assert_eq!(after_one, c(0, 1));
        // Another l should land on 'c'.
        let after_two = motion_l(&t, after_one, 1, Mode::Normal);
        assert_eq!(after_two.col, 1 + zalgo_b.len());
    }

    #[test]
    fn h_crosses_zalgo_cluster_in_one_step() {
        let zalgo_b = "b\u{0301}\u{0300}\u{0302}\u{0303}";
        let s = format!("a{}c", zalgo_b);
        let t = b(&s);
        let at_c = c(0, 1 + zalgo_b.len());
        let back = motion_h(&t, at_c, 1);
        assert_eq!(back.col, 1);
        let back2 = motion_h(&t, back, 1);
        assert_eq!(back2.col, 0);
    }

    #[test]
    fn motion_0_goes_to_column_zero() {
        let t = b("hello");
        assert_eq!(motion_0(&t, c(0, 4)), c(0, 0));
    }

    #[test]
    fn dollar_to_last_grapheme_normal() {
        let t = b("hello");
        assert_eq!(motion_dollar(&t, c(0, 0), 1, Mode::Normal), c(0, 4));
        assert_eq!(motion_dollar(&t, c(0, 0), 1, Mode::Insert), c(0, 5));
    }

    #[test]
    fn dollar_with_count_moves_down_lines() {
        let t = b("one\ntwo\nthree");
        assert_eq!(motion_dollar(&t, c(0, 0), 2, Mode::Normal), c(1, 2));
        assert_eq!(motion_dollar(&t, c(0, 0), 3, Mode::Normal), c(2, 4));
    }

    #[test]
    fn gg_and_big_g() {
        let t = b("a\nb\nc\nd");
        assert_eq!(motion_gg(&t, None), c(0, 0));
        assert_eq!(motion_gg(&t, Some(3)), c(2, 0));
        assert_eq!(motion_G(&t, None), c(3, 0));
        assert_eq!(motion_G(&t, Some(2)), c(1, 0));
    }

    #[test]
    fn w_skips_to_next_word() {
        let t = b("foo bar baz");
        assert_eq!(motion_w(&t, c(0, 0), 1, Mode::Normal), c(0, 4));
        assert_eq!(motion_w(&t, c(0, 0), 2, Mode::Normal), c(0, 8));
    }

    #[test]
    fn w_transitions_between_keyword_and_other() {
        let t = b("foo, bar");
        assert_eq!(motion_w(&t, c(0, 0), 1, Mode::Normal), c(0, 3));
        assert_eq!(motion_w(&t, c(0, 3), 1, Mode::Normal), c(0, 5));
    }

    #[test]
    fn w_crosses_lines_including_empty_line_as_word() {
        let t = b("foo\n\nbar");
        // From 'f' on line 0, w → empty line 1 (it's a word).
        assert_eq!(motion_w(&t, c(0, 0), 1, Mode::Normal), c(1, 0));
        // Another w → 'b' on line 2.
        assert_eq!(motion_w(&t, c(1, 0), 1, Mode::Normal), c(2, 0));
    }

    #[test]
    fn b_goes_back_to_word_start() {
        let t = b("foo bar baz");
        assert_eq!(motion_b(&t, c(0, 10), 1), c(0, 8));
        assert_eq!(motion_b(&t, c(0, 10), 2), c(0, 4));
    }

    #[test]
    fn e_to_word_end() {
        let t = b("foo bar");
        assert_eq!(motion_e(&t, c(0, 0), 1), c(0, 2));
        assert_eq!(motion_e(&t, c(0, 0), 2), c(0, 6));
    }

    #[test]
    fn col_from_display_handles_wide_chars() {
        let line = "abc";
        assert_eq!(col_from_display(line, 0, Mode::Normal, 4), 0);
        assert_eq!(col_from_display(line, 2, Mode::Normal, 4), 2);
        // "嗨" is wide (2 cols). Targeting column 1 should still land at
        // the start of the wide grapheme (byte 0).
        let wide = "嗨x";
        assert_eq!(col_from_display(wide, 0, Mode::Normal, 4), 0);
        assert_eq!(col_from_display(wide, 1, Mode::Normal, 4), 0);
        // Display col 2 points at 'x'.
        assert_eq!(col_from_display(wide, 2, Mode::Normal, 4), "嗨".len());
    }
}
