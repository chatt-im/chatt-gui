#[cfg(feature = "accessibility")]
pub(crate) use accesskit_unix::Adapter as AccessibilityAdapter;

#[cfg(not(feature = "accessibility"))]
pub(crate) struct AccessibilityAdapter;

#[cfg(not(feature = "accessibility"))]
impl AccessibilityAdapter {
    pub(crate) fn new<A, B, C>(
        _activation_handler: A,
        _action_handler: B,
        _deactivation_handler: C,
    ) -> Self {
        Self
    }

    pub(crate) fn update_window_focus_state(&mut self, _focus: bool) {}

    pub(crate) fn update_if_active<F>(&mut self, _update_factory: F)
    where
        F: FnOnce() -> accesskit::TreeUpdate,
    {
    }

    #[cfg_attr(not(feature = "x11"), allow(dead_code))]
    pub(crate) fn set_root_window_bounds(
        &mut self,
        _outer: accesskit::Rect,
        _inner: accesskit::Rect,
    ) {
    }
}
