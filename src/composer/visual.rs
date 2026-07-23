use super::{cursor::Cursor, mode::Mode};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VisualKind {
    Char,
    Line,
    Block,
}

impl VisualKind {
    /// The editor [`Mode`] that drives this selection kind.
    pub fn mode(self) -> Mode {
        match self {
            VisualKind::Char => Mode::Visual,
            VisualKind::Line => Mode::VisualLine,
            VisualKind::Block => Mode::VisualBlock,
        }
    }

    /// Inverse of [`mode`]; non-visual modes fall back to `Char`.
    pub fn from_mode(mode: Mode) -> Self {
        match mode {
            Mode::VisualLine => VisualKind::Line,
            Mode::VisualBlock => VisualKind::Block,
            _ => VisualKind::Char,
        }
    }
}

/// A visual-mode selection anchored at `anchor`, with `head` following
/// motions. `kind` distinguishes charwise / linewise / blockwise.
#[derive(Clone, Copy, Debug)]
pub struct Selection {
    pub anchor: Cursor,
    pub head: Cursor,
    pub kind: VisualKind,
}

impl Selection {
    pub fn new(anchor: Cursor, kind: VisualKind) -> Self {
        Self {
            anchor,
            head: anchor,
            kind,
        }
    }

    /// Returns the (upper, lower) corners in row order.
    pub fn rows_ordered(&self) -> (usize, usize) {
        if self.anchor.row <= self.head.row {
            (self.anchor.row, self.head.row)
        } else {
            (self.head.row, self.anchor.row)
        }
    }

    /// Returns the (left, right) byte columns of a blockwise selection
    /// in column order. Meaningful only for `VisualKind::Block`.
    pub fn cols_ordered(&self) -> (usize, usize) {
        let a = self.anchor.col;
        let h = self.head.col;
        if a <= h { (a, h) } else { (h, a) }
    }
}
