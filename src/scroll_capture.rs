use gpui::{
    App, Bounds, DispatchPhase, Element, ElementId, GlobalElementId, InspectorElementId,
    IntoElement, LayoutId, Pixels, ScrollWheelEvent, Window,
};

type ScrollHandler = Box<dyn FnMut(&ScrollWheelEvent, &mut Window, &mut App) -> bool>;

/// Captures wheel input over an element before its own bubble-phase handler runs.
///
/// GPUI's list applies wheel deltas directly in the bubble phase. Chatt uses this
/// wrapper to consume vertical deltas and advance them on display frames instead.
pub struct ScrollCapture<E> {
    inner: E,
    handler: Option<ScrollHandler>,
}

pub fn capture_scroll<E>(
    inner: E,
    handler: impl FnMut(&ScrollWheelEvent, &mut Window, &mut App) -> bool + 'static,
) -> ScrollCapture<E> {
    ScrollCapture {
        inner,
        handler: Some(Box::new(handler)),
    }
}

impl<E> Element for ScrollCapture<E>
where
    E: Element + IntoElement<Element = E>,
{
    type RequestLayoutState = E::RequestLayoutState;
    type PrepaintState = E::PrepaintState;

    fn id(&self) -> Option<ElementId> {
        self.inner.id()
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        self.inner.source_location()
    }

    fn request_layout(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        self.inner.request_layout(id, inspector_id, window, cx)
    }

    fn prepaint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        self.inner
            .prepaint(id, inspector_id, bounds, request_layout, window, cx)
    }

    fn paint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let mut handler = self
            .handler
            .take()
            .expect("scroll capture painted more than once");
        window.on_mouse_event(
            move |event: &ScrollWheelEvent, phase, window, cx| {
                if phase == DispatchPhase::Capture
                    && bounds.contains(&event.position)
                    && event.delta.pixel_delta(gpui::px(1.)).y != gpui::px(0.)
                    && handler(event, window, cx)
                {
                    cx.stop_propagation();
                }
            },
        );

        self.inner.paint(
            id,
            inspector_id,
            bounds,
            request_layout,
            prepaint,
            window,
            cx,
        );
    }
}

impl<E> IntoElement for ScrollCapture<E>
where
    E: Element + IntoElement<Element = E>,
{
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}
