use gpui::{App, Entity, EntityId};

/// Handle that re-renders the one view owning a piece of shared element state.
///
/// Custom [`gpui::Element`] impls and the mouse handlers they install only get
/// `&mut App`, so they cannot reach [`gpui::Context::notify`]. Reaching for
/// [`App::refresh_windows`] instead sets `Window::refreshing`, which makes GPUI
/// discard *every* cached view's recycled subtree on the following frame — one
/// scrollbar drag then costs a full-window rebuild. A `Notify` keeps the
/// invalidation scoped to the view that owns the state, so unrelated panes stay
/// eligible for reuse.
///
/// Reserve `refresh_windows` for changes that really are global, such as a rem
/// size or theme palette change.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Notify(Option<EntityId>);

impl Notify {
    /// A handle that invalidates `view`.
    pub fn for_view<V: 'static>(view: &Entity<V>) -> Self {
        Self(Some(view.entity_id()))
    }

    /// Schedules a redraw of the owning view. Does nothing once that view is
    /// gone, or for a default-constructed handle.
    ///
    /// Deferred because most callers run inside `Window::draw`, where
    /// [`App::notify`] marks the view dirty but declines to schedule a frame —
    /// the redraw would be dropped. Running it as a pending effect means it
    /// lands after the draw, in the same place `App::refresh_windows` used to
    /// take effect, and behaves identically when called from an event handler.
    pub fn notify(self, cx: &mut App) {
        let Some(entity_id) = self.0 else {
            return;
        };
        cx.defer(move |cx| cx.notify(entity_id));
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc};

    use gpui::{
        App, Bounds, Element, ElementId, GlobalElementId, InspectorElementId, IntoElement,
        LayoutId, Pixels, Style, Window, div, prelude::*,
    };

    use super::Notify;

    /// Notifies from `prepaint`, the phase where `App::notify` alone would be
    /// swallowed, and counts how many times it was laid out.
    struct NotifyingElement {
        notify: Notify,
        prepaints: Rc<Cell<usize>>,
    }

    impl IntoElement for NotifyingElement {
        type Element = Self;

        fn into_element(self) -> Self::Element {
            self
        }
    }

    impl Element for NotifyingElement {
        type RequestLayoutState = ();
        type PrepaintState = ();

        fn id(&self) -> Option<ElementId> {
            None
        }

        fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
            None
        }

        fn request_layout(
            &mut self,
            _: Option<&GlobalElementId>,
            _: Option<&InspectorElementId>,
            window: &mut Window,
            cx: &mut App,
        ) -> (LayoutId, Self::RequestLayoutState) {
            (window.request_layout(Style::default(), [], cx), ())
        }

        fn prepaint(
            &mut self,
            _: Option<&GlobalElementId>,
            _: Option<&InspectorElementId>,
            _: Bounds<Pixels>,
            _: &mut Self::RequestLayoutState,
            _: &mut Window,
            cx: &mut App,
        ) {
            let prepaints = self.prepaints.get() + 1;
            self.prepaints.set(prepaints);
            // Only once, so a handle that works leaves the count at 2 rather
            // than spinning the window forever.
            if prepaints == 1 {
                self.notify.notify(cx);
            }
        }

        fn paint(
            &mut self,
            _: Option<&GlobalElementId>,
            _: Option<&InspectorElementId>,
            _: Bounds<Pixels>,
            _: &mut Self::RequestLayoutState,
            _: &mut Self::PrepaintState,
            _: &mut Window,
            _: &mut App,
        ) {
        }
    }

    struct TestView {
        prepaints: Rc<Cell<usize>>,
        connected: bool,
    }

    impl gpui::Render for TestView {
        fn render(&mut self, _: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
            let notify = if self.connected {
                Notify::for_view(&cx.entity())
            } else {
                Notify::default()
            };
            div().size_full().child(NotifyingElement {
                notify,
                prepaints: self.prepaints.clone(),
            })
        }
    }

    fn prepaints_after_notifying_once(connected: bool, cx: &mut gpui::TestAppContext) -> usize {
        let prepaints = Rc::new(Cell::new(0));
        let counter = prepaints.clone();
        let (_view, cx) = cx.add_window_view(move |_, _| TestView {
            prepaints: counter,
            connected,
        });
        cx.run_until_parked();
        prepaints.get()
    }

    #[gpui::test]
    fn notifying_from_prepaint_schedules_another_frame(cx: &mut gpui::TestAppContext) {
        assert_eq!(prepaints_after_notifying_once(true, cx), 2);
    }

    #[gpui::test]
    fn a_disconnected_handle_leaves_the_window_alone(cx: &mut gpui::TestAppContext) {
        assert_eq!(prepaints_after_notifying_once(false, cx), 1);
    }
}
