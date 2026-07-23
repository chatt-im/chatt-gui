use super::buffer::Edit;
use super::cursor::Cursor;

/// One reversible step: an inverse edit plus the cursor position
/// captured *before* the forward edit was applied. Undo replays the
/// inverse to restore the buffer, and restores the cursor.
#[derive(Clone, Debug)]
pub struct UndoStep {
    pub inverse: Edit,
    pub cursor_before: Cursor,
}

/// A transactional batch of [`UndoStep`]s. A vim-style atomic action
/// (e.g. an insert-mode session, or a `dd`) is exactly one group —
/// undo/redo always moves a whole group at a time.
#[derive(Clone, Debug, Default)]
pub struct UndoGroup {
    pub steps: Vec<UndoStep>,
}

impl UndoGroup {
    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }
}

const MAX_UNDO: usize = 128;

/// Edit-log history. Replaces whole-buffer snapshots with a chain of
/// inverse edits, grouped into transactions by [`checkpoint`].
///
/// The `pending` group accumulates inverses emitted by
/// [`Editor::commit`] between checkpoints; [`checkpoint`] seals it
/// and starts a fresh one.
#[derive(Default, Debug)]
pub struct History {
    undo: Vec<UndoGroup>,
    redo: Vec<UndoGroup>,
    /// The group currently being built. Sealed into `undo` on the
    /// next [`checkpoint`] or when [`pop_undo_group`] is called.
    pending: UndoGroup,
}

impl History {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seal any in-progress group and start a new transaction. Edits
    /// that haven't yet been recorded (`pending` empty) are cheaply
    /// skipped. Clears the redo stack, matching vim semantics.
    pub fn checkpoint(&mut self) {
        self.seal_pending();
        self.redo.clear();
    }

    /// Record one inverse edit into the in-progress transaction.
    pub fn record(&mut self, inverse: Edit, cursor_before: Cursor) {
        self.pending.steps.push(UndoStep {
            inverse,
            cursor_before,
        });
    }

    /// Drop the in-progress transaction. Called when a would-be
    /// action turned into a no-op after its checkpoint (e.g. `r` at
    /// end-of-line) — nothing to undo, so don't leave an empty
    /// group behind.
    pub fn abort(&mut self) {
        self.pending.steps.clear();
    }

    /// Seals any pending group, then pops the most recent
    /// transaction. The caller applies each `inverse` in reverse
    /// order and pushes the resulting forward group onto redo via
    /// [`push_redo_group`].
    pub fn pop_undo_group(&mut self) -> Option<UndoGroup> {
        self.seal_pending();
        self.undo.pop()
    }

    pub fn pop_redo_group(&mut self) -> Option<UndoGroup> {
        self.redo.pop()
    }

    pub fn push_redo_group(&mut self, group: UndoGroup) {
        if !group.is_empty() {
            self.redo.push(group);
        }
    }

    /// Push a group onto undo without clearing redo. Used when
    /// redo moves an entry back to the undo stack.
    pub fn push_undo_group(&mut self, group: UndoGroup) {
        if group.is_empty() {
            return;
        }
        self.undo.push(group);
        if self.undo.len() > MAX_UNDO {
            self.undo.remove(0);
        }
    }

    /// Discard every recorded edit. Used by whole-buffer replacements
    /// and editor-mode changes for which undo doesn't make sense.
    pub fn reset(&mut self) {
        self.undo.clear();
        self.redo.clear();
        self.pending.steps.clear();
    }

    fn seal_pending(&mut self) {
        if !self.pending.is_empty() {
            let group = std::mem::take(&mut self.pending);
            self.undo.push(group);
            if self.undo.len() > MAX_UNDO {
                self.undo.remove(0);
            }
        }
    }
}
