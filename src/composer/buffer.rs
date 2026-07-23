use std::{borrow::Cow, ops::Range};

use unicode_segmentation::UnicodeSegmentation;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Edit {
    pub offset: u32,
    pub old_len: u32,
    pub new: String,
}

impl Edit {
    pub fn insert(offset: u32, new: String) -> Self {
        Self {
            offset,
            old_len: 0,
            new,
        }
    }

    pub fn delete(offset: u32, old_len: u32) -> Self {
        Self {
            offset,
            old_len,
            new: String::new(),
        }
    }

    pub fn replace(offset: u32, old_len: u32, new: String) -> Self {
        Self {
            offset,
            old_len,
            new,
        }
    }
}

const DEFAULT_GAP: usize = 128;

#[derive(Clone, Debug)]
struct GapBuffer {
    bytes: Vec<u8>,
    gap_start: usize,
    gap_end: usize,
}

impl Default for GapBuffer {
    fn default() -> Self {
        Self::with_text("")
    }
}

impl GapBuffer {
    fn with_text(s: &str) -> Self {
        let mut bytes = vec![0; s.len() + DEFAULT_GAP];
        bytes[..s.len()].copy_from_slice(s.as_bytes());
        Self {
            bytes,
            gap_start: s.len(),
            gap_end: s.len() + DEFAULT_GAP,
        }
    }

    fn len(&self) -> usize {
        self.bytes.len() - self.gap_len()
    }

    fn gap_len(&self) -> usize {
        self.gap_end - self.gap_start
    }

    pub(crate) fn as_slices(&self) -> (&str, &str) {
        (
            Self::bytes_to_str(&self.bytes[0..self.gap_start]),
            Self::bytes_to_str(&self.bytes[self.gap_end..]),
        )
    }

    fn page(&self, offset: usize) -> (usize, &[u8]) {
        let len = self.len();
        if offset >= len {
            return (len, &[]);
        }
        if offset < self.gap_start {
            (0, &self.bytes[..self.gap_start])
        } else {
            (self.gap_start, &self.bytes[self.gap_end..])
        }
    }

    fn slice(&self, range: Range<usize>) -> Cow<'_, str> {
        assert!(range.start <= range.end, "invalid slice range");
        assert!(range.end <= self.len(), "slice past end of gap buffer");

        if range.is_empty() {
            return Cow::Borrowed("");
        }

        if range.end <= self.gap_start {
            return Cow::Borrowed(Self::bytes_to_str(&self.bytes[range.start..range.end]));
        }

        let gap_len = self.gap_len();
        if range.start >= self.gap_start {
            let start = range.start + gap_len;
            let end = range.end + gap_len;
            return Cow::Borrowed(Self::bytes_to_str(&self.bytes[start..end]));
        }

        let mut out = String::with_capacity(range.end - range.start);
        out.push_str(Self::bytes_to_str(&self.bytes[range.start..self.gap_start]));
        let tail_len = range.end - self.gap_start;
        out.push_str(Self::bytes_to_str(
            &self.bytes[self.gap_end..self.gap_end + tail_len],
        ));
        Cow::Owned(out)
    }

    fn replace_range(&mut self, start: usize, end: usize, replacement: &str) {
        self.move_gap(start);
        self.gap_end += end - start;
        self.ensure_gap(replacement.len());

        let insert_end = self.gap_start + replacement.len();
        self.bytes[self.gap_start..insert_end].copy_from_slice(replacement.as_bytes());
        self.gap_start = insert_end;
    }

    fn move_gap(&mut self, offset: usize) {
        assert!(offset <= self.len(), "gap move past end of buffer");

        if offset < self.gap_start {
            let shift = self.gap_start - offset;
            self.bytes
                .copy_within(offset..self.gap_start, self.gap_end - shift);
            self.gap_start = offset;
            self.gap_end -= shift;
        } else if offset > self.gap_start {
            let shift = offset - self.gap_start;
            self.bytes
                .copy_within(self.gap_end..self.gap_end + shift, self.gap_start);
            self.gap_start += shift;
            self.gap_end += shift;
        }
    }

    fn ensure_gap(&mut self, min_size: usize) {
        if self.gap_len() >= min_size {
            return;
        }

        let live_len = self.len();
        let grow_by = min_size.max(live_len / 2).max(DEFAULT_GAP);
        let before = self.gap_start;
        let after = self.bytes.len() - self.gap_end;
        let mut bytes = vec![0; live_len + grow_by];
        bytes[..before].copy_from_slice(&self.bytes[..before]);
        let new_gap_end = before + grow_by;
        bytes[new_gap_end..new_gap_end + after].copy_from_slice(&self.bytes[self.gap_end..]);
        self.bytes = bytes;
        self.gap_end = new_gap_end;
    }

    fn byte_at(&self, offset: usize) -> Option<u8> {
        if offset >= self.len() {
            None
        } else {
            Some(self.bytes[self.logical_to_physical(offset)])
        }
    }

    fn logical_to_physical(&self, offset: usize) -> usize {
        if offset < self.gap_start {
            offset
        } else {
            offset + self.gap_len()
        }
    }

    fn is_char_boundary(&self, offset: usize) -> bool {
        if offset == 0 || offset == self.len() {
            return true;
        }
        matches!(self.byte_at(offset), Some(b) if !is_utf8_continuation(b))
    }

    fn bytes_to_str(bytes: &[u8]) -> &str {
        std::str::from_utf8(bytes).expect("gap buffer must contain valid UTF-8")
    }
}

fn is_utf8_continuation(b: u8) -> bool {
    (b & 0b1100_0000) == 0b1000_0000
}

/// The underlying gap-buffered text store with a line index.
///
/// Exposed by [`Editor::text_buffer`] for integrations that drive an
/// external highlighter (`tinyhl`, tree-sitter, …) and need direct
/// paged access to the buffer bytes via [`Self::page`].
///
/// Lines follow nvim's line model: a trailing `\n` introduces a
/// trailing empty line, and the buffer always exposes at least one
/// (possibly empty) line.
///
/// [`Editor::text_buffer`]: crate::Editor::text_buffer
#[derive(Clone, Debug)]
pub struct TextBuffer {
    text: GapBuffer,
    line_starts: Vec<u32>,
}

impl Default for TextBuffer {
    fn default() -> Self {
        Self {
            text: GapBuffer::default(),
            line_starts: vec![0],
        }
    }
}

impl TextBuffer {
    /// Creates an empty buffer with a single empty line.
    pub fn new() -> Self {
        Self::default()
    }

    #[cfg(test)]
    pub fn from_str(s: &str) -> Self {
        let mut b = Self::default();
        b.set_text(s);
        b
    }

    /// Returns the total byte length of the buffer.
    pub fn len(&self) -> usize {
        self.text.len()
    }

    pub fn clamp_offset(&self, offset: usize) -> usize {
        let mut offset = offset.min(self.len());
        while !self.text.is_char_boundary(offset) {
            offset -= 1;
        }
        offset
    }

    /// Returns the number of lines, always at least 1.
    pub fn line_count(&self) -> usize {
        self.line_starts.len()
    }

    /// Returns the index of the last row.
    pub fn max_row(&self) -> usize {
        self.line_count().saturating_sub(1)
    }

    /// Returns the text of `row`, excluding the trailing newline.
    ///
    /// Rows outside `0..line_count()` return the empty string. The
    /// returned [`Cow`] borrows when the line lies on one side of the
    /// internal gap and allocates when it straddles the gap.
    pub fn line(&self, row: usize) -> Cow<'_, str> {
        let Some(range) = self.line_range(row) else {
            return Cow::Borrowed("");
        };
        self.text.slice(range)
    }

    /// Returns the byte length of `row`, excluding the trailing newline.
    pub fn line_len(&self, row: usize) -> usize {
        self.line_range(row).map_or(0, |range| range.len())
    }

    fn line_range(&self, row: usize) -> Option<Range<usize>> {
        let start = *self.line_starts.get(row)? as usize;
        let end = self
            .line_starts
            .get(row + 1)
            .map(|&next| next as usize - 1)
            .unwrap_or(self.len());
        Some(start..end)
    }

    /// Returns the byte offset of `row`'s first byte, or
    /// [`Self::len`] when `row` is past the end.
    pub fn line_start(&self, row: usize) -> u32 {
        self.line_starts
            .get(row)
            .copied()
            .unwrap_or(self.len() as u32)
    }

    /// Maps a `(row, col)` position to a flat byte offset, clamping
    /// `col` to the row's byte length.
    pub fn rowcol_to_offset(&self, row: usize, col: usize) -> u32 {
        let start = self.line_start(row);
        let line_len = self.line_len(row) as u32;
        start + (col as u32).min(line_len)
    }

    /// Inverse of [`Self::rowcol_to_offset`].
    pub fn offset_to_rowcol(&self, offset: u32) -> (usize, usize) {
        let offset = offset.min(self.len() as u32);
        let row = self.row_of_offset(offset as usize);
        let col = offset as usize - self.line_starts[row] as usize;
        (row, col)
    }

    /// Returns the two contiguous halves of the buffer's text, in
    /// logical order. Either may be empty; concatenated they yield
    /// the full buffer without allocation.
    pub fn as_slices(&self) -> (&str, &str) {
        self.text.as_slices()
    }

    /// Returns the entire buffer as an owned [`String`]. Lines are
    /// separated by `\n`.
    pub fn text(&self) -> String {
        self.text.slice(0..self.len()).into_owned()
    }

    pub(crate) fn slice(&self, range: Range<usize>) -> Cow<'_, str> {
        self.text.slice(range)
    }

    /// Replaces the entire contents with `s`.
    ///
    /// A trailing `\n` in `s` introduces a trailing empty line,
    /// matching nvim's line model.
    pub fn set_text(&mut self, s: &str) {
        self.text = GapBuffer::with_text(s);
        self.rebuild_line_starts();
    }

    /// Applies `edit` to the buffer and returns its inverse, suitable
    /// for pushing onto an undo stack.
    ///
    /// # Panics
    ///
    /// Panics if `edit.offset` or `edit.offset + edit.old_len` fall
    /// past the end of the buffer or on a byte that is not a UTF-8
    /// character boundary.
    pub fn apply(&mut self, edit: &Edit) -> Edit {
        let start = edit.offset as usize;
        let old_len = edit.old_len as usize;
        let len_before = self.len();
        let end = start + old_len;

        assert!(end <= len_before, "edit past end of buffer");
        assert!(
            self.text.is_char_boundary(start),
            "edit offset must be on a UTF-8 boundary"
        );
        assert!(
            self.text.is_char_boundary(end),
            "edit end must be on a UTF-8 boundary"
        );

        let original = self.text.slice(start..end).into_owned();

        let start_row = self.row_of_offset(start);
        let end_row = self.row_of_offset(end);
        let replace_end = if end_row + 1 < self.line_starts.len() {
            end_row + 2
        } else {
            self.line_starts.len()
        };
        let region_start = self.line_starts[start_row] as usize;
        let region_end_old = if end_row + 1 < self.line_starts.len() {
            self.line_starts[end_row + 1] as usize
        } else {
            len_before
        };

        self.text.replace_range(start, end, &edit.new);

        let delta = edit.new.len() as isize - old_len as isize;
        let region_end_new = if region_end_old == len_before {
            self.len()
        } else {
            (region_end_old as isize + delta) as usize
        };
        self.refresh_line_index_after_edit(
            start_row,
            replace_end,
            region_start,
            region_end_new,
            delta,
        );

        Edit {
            offset: edit.offset,
            old_len: edit.new.len() as u32,
            new: original,
        }
    }

    fn row_of_offset(&self, offset: usize) -> usize {
        let offset = offset.min(self.len()) as u32;
        match self.line_starts.binary_search(&offset) {
            Ok(i) => i,
            Err(i) => i - 1,
        }
    }

    fn rebuild_line_starts(&mut self) {
        self.line_starts = self.collect_line_starts(0, self.len());
    }

    fn collect_line_starts(&self, start: usize, end: usize) -> Vec<u32> {
        let mut starts = vec![start as u32];
        self.extend_line_starts(start, end, &mut starts);
        starts
    }

    fn extend_line_starts(&self, start: usize, end: usize, starts: &mut Vec<u32>) {
        let mut offset = start;
        while offset < end {
            let (base, page) = self.text.page(offset);
            if page.is_empty() {
                break;
            }
            let page_start = offset - base;
            let page_end = (end - base).min(page.len());
            for (i, b) in page[page_start..page_end].iter().enumerate() {
                if *b == b'\n' {
                    starts.push((base + page_start + i + 1) as u32);
                }
            }
            offset = base + page_end;
        }
    }

    fn refresh_line_index_after_edit(
        &mut self,
        start_row: usize,
        replace_end: usize,
        region_start: usize,
        region_end_new: usize,
        delta: isize,
    ) {
        let new_starts = self.collect_line_starts(region_start, region_end_new);
        let inserted = new_starts.len();
        self.line_starts.splice(start_row..replace_end, new_starts);
        if delta != 0 {
            for start in &mut self.line_starts[start_row + inserted..] {
                *start = (*start as isize + delta) as u32;
            }
        }
    }
}

/// Rounds `col` down to the nearest grapheme-cluster start in `line`.
pub fn align_to_grapheme_start(line: &str, col: usize) -> usize {
    if col >= line.len() {
        return line.len();
    }
    let mut last = 0;
    for (i, _) in line.grapheme_indices(true) {
        if i > col {
            break;
        }
        last = i;
    }
    last
}

/// Returns the byte index of the last grapheme cluster start in `line`,
/// or `None` if the line is empty.
pub fn last_grapheme_start(line: &str) -> Option<usize> {
    line.grapheme_indices(true).last().map(|(i, _)| i)
}

/// Returns the byte index of the next grapheme after `col`, or `line.len()`
/// if none. Panics if `col` is past the end.
pub fn next_grapheme_start(line: &str, col: usize) -> usize {
    let mut it = line.grapheme_indices(true).skip_while(|(i, _)| *i <= col);
    it.next().map(|(i, _)| i).unwrap_or(line.len())
}

/// Byte index one past the grapheme cluster containing `col`.
///
/// Clamps to `line.len()` so it's safe to call with an unaligned `col`
/// that may sit past EOL. Equivalent to aligning to the grapheme start
/// then stepping to the next grapheme.
pub fn grapheme_end(line: &str, col: usize) -> usize {
    next_grapheme_start(line, align_to_grapheme_start(line, col))
}

/// Returns the byte index of the previous grapheme before `col`, or `0`
/// if none.
pub fn prev_grapheme_start(line: &str, col: usize) -> usize {
    let mut prev = 0;
    for (i, _) in line.grapheme_indices(true) {
        if i >= col {
            break;
        }
        prev = i;
    }
    prev
}

/// Returns the grapheme cluster at `col` (possibly empty if `col` is
/// past the end).
pub fn grapheme_at<'a>(line: &'a str, col: usize) -> &'a str {
    let start = align_to_grapheme_start(line, col);
    let end = next_grapheme_start(line, start);
    &line[start..end]
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{Edit, TextBuffer};

    #[test]
    fn incrementally_repairs_line_index_across_newline_edits() {
        let mut buffer = TextBuffer::from_str("one\ntwo\nthree");
        buffer.apply(&Edit::replace(2, 6, "X\nY\n".to_string()));

        assert_eq!(buffer.text(), "onX\nY\nthree");
        assert_eq!(buffer.line_count(), 3);
        assert_eq!(buffer.line(0), "onX");
        assert_eq!(buffer.line(1), "Y");
        assert_eq!(buffer.line(2), "three");
        assert_eq!(buffer.line_start(2), 6);
    }

    #[test]
    fn local_insertions_remain_fast_with_ten_thousand_lines() {
        let text = (0..10_000)
            .map(|row| format!("line {row}\n"))
            .collect::<String>();
        let mut buffer = TextBuffer::from_str(&text);
        let started = Instant::now();

        for _ in 0..500 {
            buffer.apply(&Edit::insert(0, "x".to_string()));
        }

        assert_eq!(buffer.line_count(), 10_001);
        assert_eq!(buffer.line(0).len(), 506);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "500 local edits in a 10K-line message took {:?}",
            started.elapsed()
        );
    }
}
