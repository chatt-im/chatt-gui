use gpui::{Length, Pixels, Styled};

/// Pins an element to an exact size along one axis so layout stops probing
/// inside it.
///
/// # Why this exists
///
/// Before laying a flex child out, Taffy asks it for its min-content size
/// (`determine_flex_base_size` in `taffy::compute::flexbox`), and that probe
/// recurses through the child's entire subtree. Its result lands in a different
/// cache slot than the final `RunMode::PerformLayout` pass, so it is pure extra
/// work — every nested flex container roughly doubles the walks of the subtree
/// beneath it. This window has six flex levels between its root and the code
/// viewer, which put `compute_flexbox_layout` at 27% of every frame.
///
/// Taffy skips the probe for a node whose size is already settled without
/// consulting its contents, which it decides by looking for a definite,
/// *equal* `min_size` and `max_size`. A definite `size` is not enough: the
/// probe runs under `SizingMode::ContentSize`, where Taffy deliberately ignores
/// the style size.
///
/// This is the layout half of CSS `contain`. It bounds how far a parent's
/// layout can see; it does not cache anything across frames.
///
/// # Which axis
///
/// Contain along the *parent's* main axis, since that is what the probe asks
/// about: [`Self::contain_w`] for a child of a flex row, [`Self::contain_h`]
/// for a child of a flex column. Containing both axes always works.
///
/// # When it is correct
///
/// Only where the size is already decided by something other than the child's
/// own content, and the value passed is the one flex would have arrived at
/// anyway. On a `flex_none` child that already carries an explicit `w`/`h` this
/// is a pure annotation — flex could not have resized it. Anywhere else,
/// passing a different number silently resizes the pane.
pub trait Contain: Styled + Sized {
    /// Fixes the width, for a child of a flex row.
    fn contain_w(mut self, width: Pixels) -> Self {
        let width = Length::from(width);
        let style = self.style();
        style.size.width = Some(width);
        style.min_size.width = Some(width);
        style.max_size.width = Some(width);
        self
    }

    /// Fixes the height, for a child of a flex column.
    fn contain_h(mut self, height: Pixels) -> Self {
        let height = Length::from(height);
        let style = self.style();
        style.size.height = Some(height);
        style.min_size.height = Some(height);
        style.max_size.height = Some(height);
        self
    }
}

impl<T: Styled + Sized> Contain for T {}
