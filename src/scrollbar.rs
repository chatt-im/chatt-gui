use std::{cell::Cell, rc::Rc};

use gpui::{
    App, BorderStyle, Bounds, Corners, CursorStyle, DispatchPhase, Edges, Element, ElementId,
    GlobalElementId, Hitbox, HitboxBehavior, Hsla, IntoElement, LayoutId, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point, Position, Style,
    UniformListScrollHandle, Window, point, px, quad, relative, rgba, size,
};

const SCROLLBAR_SIZE: Pixels = px(12.0);
const SCROLLBAR_PADDING: Pixels = px(2.0);
const SCROLLBAR_MIN_THUMB: Pixels = px(28.0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ScrollbarAxis {
    Horizontal,
    Vertical,
}

#[derive(Clone, Copy, Debug)]
struct ScrollbarDrag {
    axis: ScrollbarAxis,
    pointer_offset: Pixels,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct OverlayScrollbarState(Rc<Cell<Option<ScrollbarDrag>>>);

impl OverlayScrollbarState {
    pub(crate) fn reset(&self) {
        self.0.set(None);
    }

    #[cfg(test)]
    pub(crate) fn is_dragging(&self) -> bool {
        self.0.get().is_some()
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ScrollbarGeometry {
    pub(crate) axis: ScrollbarAxis,
    pub(crate) track_bounds: Bounds<Pixels>,
    pub(crate) thumb_bounds: Bounds<Pixels>,
    pub(crate) thumb_track_start: Pixels,
    pub(crate) thumb_travel: Pixels,
    pub(crate) max_offset: Pixels,
}

#[derive(Clone)]
pub(crate) struct ScrollbarLayout {
    geometry: ScrollbarGeometry,
    hitbox: Hitbox,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct OverlayScrollbarColors {
    pub(crate) track: Hsla,
    pub(crate) thumb: Hsla,
    pub(crate) thumb_hovered: Hsla,
}

impl Default for OverlayScrollbarColors {
    fn default() -> Self {
        Self {
            track: rgba(0x111317dd).into(),
            thumb: rgba(0x5f6670cc).into(),
            thumb_hovered: rgba(0x8b929ddd).into(),
        }
    }
}

pub(crate) struct OverlayScrollbars {
    id: ElementId,
    scroll_handle: UniformListScrollHandle,
    state: OverlayScrollbarState,
    colors: OverlayScrollbarColors,
}

impl OverlayScrollbars {
    pub(crate) fn new(
        id: impl Into<ElementId>,
        scroll_handle: UniformListScrollHandle,
        state: OverlayScrollbarState,
        colors: OverlayScrollbarColors,
    ) -> Self {
        Self {
            id: id.into(),
            scroll_handle,
            state,
            colors,
        }
    }
}

impl IntoElement for OverlayScrollbars {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for OverlayScrollbars {
    type RequestLayoutState = ();
    type PrepaintState = Vec<ScrollbarLayout>;

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
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
    ) -> (LayoutId, Self::RequestLayoutState) {
        let style = Style {
            position: Position::Absolute,
            size: size(relative(1.0), relative(1.0)).map(Into::into),
            ..Default::default()
        };
        (window.request_layout(style, None, cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        window: &mut Window,
        _: &mut App,
    ) -> Self::PrepaintState {
        let base_handle = self.scroll_handle.0.borrow().base_handle.clone();
        scrollbar_geometries(bounds, base_handle.max_offset(), base_handle.offset())
            .into_iter()
            .map(|geometry| ScrollbarLayout {
                hitbox: window.insert_hitbox(
                    geometry.track_bounds,
                    HitboxBehavior::BlockMouseExceptScroll,
                ),
                geometry,
            })
            .collect()
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        layouts: &mut Self::PrepaintState,
        window: &mut Window,
        _: &mut App,
    ) {
        let active_drag = self.state.0.get();
        for layout in layouts.iter() {
            let hovered = layout.hitbox.is_hovered(window)
                || active_drag.is_some_and(|drag| drag.axis == layout.geometry.axis);
            window.paint_quad(quad(
                layout.geometry.track_bounds,
                Pixels::ZERO,
                self.colors.track,
                Edges::default(),
                Hsla::transparent_black(),
                BorderStyle::default(),
            ));
            window.paint_quad(quad(
                layout.geometry.thumb_bounds,
                Corners::all(px(3.0)),
                if hovered {
                    self.colors.thumb_hovered
                } else {
                    self.colors.thumb
                },
                Edges::default(),
                Hsla::transparent_black(),
                BorderStyle::default(),
            ));
            window.set_cursor_style(CursorStyle::Arrow, &layout.hitbox);
        }
        if active_drag.is_some() {
            window.set_window_cursor_style(CursorStyle::Arrow);
        }

        let capture_phase = if active_drag.is_some() {
            DispatchPhase::Capture
        } else {
            DispatchPhase::Bubble
        };
        window.on_mouse_event({
            let layouts = layouts.clone();
            let scroll_handle = self.scroll_handle.clone();
            let state = self.state.clone();
            move |event: &MouseDownEvent, phase, window, cx| {
                if phase != capture_phase || event.button != MouseButton::Left {
                    return;
                }
                let Some(layout) = layouts
                    .iter()
                    .find(|layout| layout.hitbox.is_hovered(window))
                else {
                    return;
                };
                let geometry = layout.geometry;
                if geometry.thumb_bounds.contains(&event.position) {
                    state.0.set(Some(ScrollbarDrag {
                        axis: geometry.axis,
                        pointer_offset: axis_position(geometry.axis, event.position)
                            - axis_origin(geometry.axis, geometry.thumb_bounds),
                    }));
                    window.capture_pointer(layout.hitbox.id);
                } else {
                    let pointer_offset = axis_size(geometry.axis, geometry.thumb_bounds.size) / 2.0;
                    set_scrollbar_offset(
                        &scroll_handle,
                        geometry.axis,
                        offset_for_position(geometry, event.position, pointer_offset),
                    );
                }
                window.prevent_default();
                cx.stop_propagation();
                cx.refresh_windows();
            }
        });
        window.on_mouse_event({
            let layouts = layouts.clone();
            let scroll_handle = self.scroll_handle.clone();
            let state = self.state.clone();
            move |event: &MouseMoveEvent, phase, _window, cx| {
                if phase != capture_phase {
                    return;
                }
                let Some(drag) = state.0.get() else {
                    return;
                };
                if !event.dragging() {
                    state.0.set(None);
                    cx.refresh_windows();
                    return;
                }
                let Some(layout) = layouts
                    .iter()
                    .find(|layout| layout.geometry.axis == drag.axis)
                else {
                    return;
                };
                set_scrollbar_offset(
                    &scroll_handle,
                    drag.axis,
                    offset_for_position(layout.geometry, event.position, drag.pointer_offset),
                );
                cx.stop_propagation();
                cx.refresh_windows();
            }
        });
        window.on_mouse_event({
            let state = self.state.clone();
            move |event: &MouseUpEvent, phase, _window, cx| {
                if phase == capture_phase
                    && event.button == MouseButton::Left
                    && state.0.take().is_some()
                {
                    cx.stop_propagation();
                    cx.refresh_windows();
                }
            }
        });
    }
}

pub(crate) fn scrollbar_geometries(
    bounds: Bounds<Pixels>,
    max_offset: Point<Pixels>,
    offset: Point<Pixels>,
) -> Vec<ScrollbarGeometry> {
    let horizontal = max_offset.x > Pixels::ZERO;
    let vertical = max_offset.y > Pixels::ZERO;
    let corner = if horizontal && vertical {
        SCROLLBAR_SIZE
    } else {
        Pixels::ZERO
    };
    let mut geometries = Vec::with_capacity(2);
    if vertical
        && let Some(geometry) = scrollbar_geometry(
            ScrollbarAxis::Vertical,
            Bounds::new(
                point(bounds.right() - SCROLLBAR_SIZE, bounds.top()),
                size(
                    SCROLLBAR_SIZE,
                    (bounds.size.height - corner).max(Pixels::ZERO),
                ),
            ),
            bounds.size.height,
            max_offset.y,
            offset.y,
        )
    {
        geometries.push(geometry);
    }
    if horizontal
        && let Some(geometry) = scrollbar_geometry(
            ScrollbarAxis::Horizontal,
            Bounds::new(
                point(bounds.left(), bounds.bottom() - SCROLLBAR_SIZE),
                size(
                    (bounds.size.width - corner).max(Pixels::ZERO),
                    SCROLLBAR_SIZE,
                ),
            ),
            bounds.size.width,
            max_offset.x,
            offset.x,
        )
    {
        geometries.push(geometry);
    }
    geometries
}

fn scrollbar_geometry(
    axis: ScrollbarAxis,
    track_bounds: Bounds<Pixels>,
    viewport_length: Pixels,
    max_offset: Pixels,
    offset: Pixels,
) -> Option<ScrollbarGeometry> {
    let track_length = axis_size(axis, track_bounds.size);
    let available = track_length - 2.0 * SCROLLBAR_PADDING;
    if available <= Pixels::ZERO || viewport_length <= Pixels::ZERO || max_offset <= Pixels::ZERO {
        return None;
    }
    let content_length = viewport_length + max_offset;
    let thumb_length = (available * (viewport_length / content_length))
        .max(SCROLLBAR_MIN_THUMB)
        .min(available);
    let thumb_travel = (available - thumb_length).max(Pixels::ZERO);
    let fraction = (-offset / max_offset).clamp(0.0, 1.0);
    let thumb_offset = thumb_travel * fraction;
    let thumb_track_start = axis_origin(axis, track_bounds) + SCROLLBAR_PADDING;
    let thumb_bounds = match axis {
        ScrollbarAxis::Horizontal => Bounds::new(
            point(
                thumb_track_start + thumb_offset,
                track_bounds.top() + SCROLLBAR_PADDING,
            ),
            size(thumb_length, SCROLLBAR_SIZE - 2.0 * SCROLLBAR_PADDING),
        ),
        ScrollbarAxis::Vertical => Bounds::new(
            point(
                track_bounds.left() + SCROLLBAR_PADDING,
                thumb_track_start + thumb_offset,
            ),
            size(SCROLLBAR_SIZE - 2.0 * SCROLLBAR_PADDING, thumb_length),
        ),
    };
    Some(ScrollbarGeometry {
        axis,
        track_bounds,
        thumb_bounds,
        thumb_track_start,
        thumb_travel,
        max_offset,
    })
}

fn offset_for_position(
    geometry: ScrollbarGeometry,
    position: Point<Pixels>,
    pointer_offset: Pixels,
) -> Pixels {
    if geometry.thumb_travel <= Pixels::ZERO {
        return Pixels::ZERO;
    }
    let thumb_position =
        (axis_position(geometry.axis, position) - geometry.thumb_track_start - pointer_offset)
            .clamp(Pixels::ZERO, geometry.thumb_travel);
    if thumb_position <= px(0.01) {
        return Pixels::ZERO;
    }
    if geometry.thumb_travel - thumb_position <= px(0.01) {
        return -geometry.max_offset;
    }
    -geometry.max_offset * (thumb_position / geometry.thumb_travel)
}

fn set_scrollbar_offset(handle: &UniformListScrollHandle, axis: ScrollbarAxis, value: Pixels) {
    let base_handle = handle.0.borrow().base_handle.clone();
    let offset = base_handle.offset();
    base_handle.set_offset(match axis {
        ScrollbarAxis::Horizontal => point(value, offset.y),
        ScrollbarAxis::Vertical => point(offset.x, value),
    });
}

fn axis_position(axis: ScrollbarAxis, point: Point<Pixels>) -> Pixels {
    match axis {
        ScrollbarAxis::Horizontal => point.x,
        ScrollbarAxis::Vertical => point.y,
    }
}

fn axis_origin(axis: ScrollbarAxis, bounds: Bounds<Pixels>) -> Pixels {
    axis_position(axis, bounds.origin)
}

fn axis_size(axis: ScrollbarAxis, size: gpui::Size<Pixels>) -> Pixels {
    match axis {
        ScrollbarAxis::Horizontal => size.width,
        ScrollbarAxis::Vertical => size.height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_axes_reserve_the_corner() {
        let bounds = Bounds::new(point(px(0.0), px(0.0)), size(px(200.0), px(100.0)));
        let geometries = scrollbar_geometries(
            bounds,
            point(px(300.0), px(200.0)),
            point(px(-20.0), px(-40.0)),
        );
        assert_eq!(geometries.len(), 2);
        let vertical = geometries
            .iter()
            .find(|geometry| geometry.axis == ScrollbarAxis::Vertical)
            .unwrap();
        let horizontal = geometries
            .iter()
            .find(|geometry| geometry.axis == ScrollbarAxis::Horizontal)
            .unwrap();
        assert_eq!(
            vertical.track_bounds.bottom(),
            bounds.bottom() - SCROLLBAR_SIZE
        );
        assert_eq!(
            horizontal.track_bounds.right(),
            bounds.right() - SCROLLBAR_SIZE
        );
    }

    #[test]
    fn drag_positions_clamp_to_scroll_endpoints() {
        let geometry = scrollbar_geometry(
            ScrollbarAxis::Horizontal,
            Bounds::new(point(px(0.0), px(0.0)), size(px(200.0), SCROLLBAR_SIZE)),
            px(200.0),
            px(600.0),
            Pixels::ZERO,
        )
        .unwrap();
        let pointer_offset = geometry.thumb_bounds.size.width / 2.0;
        assert_eq!(
            offset_for_position(geometry, point(px(-100.0), px(0.0)), pointer_offset),
            Pixels::ZERO
        );
        assert_eq!(
            offset_for_position(geometry, point(px(1000.0), px(0.0)), pointer_offset),
            px(-600.0)
        );
    }
}
