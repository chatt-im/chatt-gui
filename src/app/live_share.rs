use super::*;

impl ChattView {
    pub(super) fn advance_live_video(&mut self, window: &mut Window) {
        let mut ended = Vec::new();
        for (stream_id, view) in &mut self.live_players {
            match view.player.drain_events() {
                Ok(playback) if playback.finished => {
                    ended.push((*stream_id, "Screen share ended".to_string()));
                }
                Ok(_) => {}
                Err(error) => {
                    kvlog::error!(
                        "screen-share playback failed",
                        stream_id,
                        err = %error
                    );
                    ended.push((*stream_id, format!("Screen share failed · {error:#}")));
                }
            }
        }
        for (stream_id, status) in ended {
            self.live_players.remove(&stream_id);
            if self.fullscreen_share == Some(stream_id) {
                self.fullscreen_share = None;
                self.fullscreen_live_controls_hovered = false;
            }
            self.send_stop_live_share(stream_id);
            self.status = status.into();
        }
        self.exit_native_media_fullscreen_if_inactive(window);
        if self.live_players.is_empty() {
            self.live_pane_resize = None;
        }
    }

    fn start_live_share(&mut self, stream_id: StreamId, cx: &mut Context<Self>) {
        if self.live_players.contains_key(&stream_id) || !self.model.is_ready() {
            return;
        }
        let request_id = self.request_id();
        self.model.pending.insert(
            request_id,
            PendingRequest {
                operation: Operation::StartLiveShare,
                room_id: self
                    .model
                    .live_shares
                    .iter()
                    .find(|share| share.stream_id == stream_id)
                    .map(|share| share.room_id),
                draft: None,
                transfer_id: None,
            },
        );
        if let Err(error) = self.daemon.send(ClientFrame::StartLiveShare {
            request_id,
            stream_id,
        }) {
            self.model.pending.remove(&request_id);
            self.status = error.into();
        } else {
            self.status = "Starting screen share…".into();
        }
        cx.notify();
    }

    fn stop_live_share(
        &mut self,
        stream_id: StreamId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.live_players.remove(&stream_id);
        if self.live_players.is_empty() {
            self.live_pane_resize = None;
        }
        if self.fullscreen_share == Some(stream_id) {
            self.fullscreen_share = None;
            self.fullscreen_live_controls_hovered = false;
        }
        self.exit_native_media_fullscreen_if_inactive(window);
        self.send_stop_live_share(stream_id);
        self.status = "Stopped screen share".into();
        cx.notify();
    }

    pub(super) fn send_stop_live_share(&mut self, stream_id: StreamId) {
        let request_id = self.request_id();
        self.model.pending.insert(
            request_id,
            PendingRequest {
                operation: Operation::StopLiveShare,
                room_id: None,
                draft: None,
                transfer_id: None,
            },
        );
        if self
            .daemon
            .send(ClientFrame::StopLiveShare {
                request_id,
                stream_id,
            })
            .is_err()
        {
            self.model.pending.remove(&request_id);
        }
    }

    fn reset_live_view(&mut self, stream_id: StreamId, cx: &mut Context<Self>) {
        if let Some(view) = self.live_players.get_mut(&stream_id) {
            view.zoom = MIN_LIVE_ZOOM;
            view.pan = point(px(0.), px(0.));
            view.last_mouse_position = None;
            cx.notify();
        }
    }

    fn zoom_live_view_at(
        &mut self,
        stream_id: StreamId,
        factor: f32,
        focal_point: Option<Point<Pixels>>,
        cx: &mut Context<Self>,
    ) {
        if let Some(view) = self.live_players.get_mut(&stream_id) {
            let old_zoom = view.zoom;
            let new_zoom = (old_zoom * factor).clamp(MIN_LIVE_ZOOM, MAX_LIVE_ZOOM);
            if let Some(viewport) = view.viewport_bounds.get() {
                let focal_point = focal_point.unwrap_or_else(|| viewport.center());
                view.pan = zoom_live_pan(
                    view.pan,
                    view.coded_size,
                    old_zoom,
                    new_zoom,
                    viewport,
                    focal_point,
                );
            } else if new_zoom == MIN_LIVE_ZOOM {
                view.pan = point(px(0.), px(0.));
            }
            view.zoom = new_zoom;
            cx.notify();
        }
    }

    fn zoom_live_view(&mut self, stream_id: StreamId, factor: f32, cx: &mut Context<Self>) {
        self.zoom_live_view_at(stream_id, factor, None, cx);
    }

    pub(super) fn pan_live_view(
        &mut self,
        stream_id: StreamId,
        x: f32,
        y: f32,
        cx: &mut Context<Self>,
    ) {
        if let Some(view) = self.live_players.get_mut(&stream_id) {
            view.pan += point(px(x), px(y));
            if let Some(viewport) = view.viewport_bounds.get() {
                view.pan = clamp_live_pan(view.pan, view.coded_size, viewport, view.zoom);
            }
            cx.notify();
        }
    }

    pub(super) fn live_zoom_in_action(
        &mut self,
        _: &LiveZoomIn,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(stream_id) = self.fullscreen_share {
            self.zoom_live_view(stream_id, 1.25, cx);
        }
    }

    pub(super) fn live_zoom_out_action(
        &mut self,
        _: &LiveZoomOut,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(stream_id) = self.fullscreen_share {
            self.zoom_live_view(stream_id, 1.0 / 1.25, cx);
        }
    }

    pub(super) fn live_reset_action(
        &mut self,
        _: &LiveReset,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(stream_id) = self.fullscreen_share {
            self.reset_live_view(stream_id, cx);
        }
    }

    pub(super) fn live_pan_up_action(
        &mut self,
        _: &LivePanUp,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(stream_id) = self.fullscreen_share {
            self.pan_live_view(stream_id, 0.0, 30.0, cx);
        }
    }

    pub(super) fn live_pan_down_action(
        &mut self,
        _: &LivePanDown,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(stream_id) = self.fullscreen_share {
            self.pan_live_view(stream_id, 0.0, -30.0, cx);
        }
    }

    fn scroll_live_view(
        &mut self,
        stream_id: StreamId,
        event: &ScrollWheelEvent,
        cx: &mut Context<Self>,
    ) {
        let delta = match event.delta {
            ScrollDelta::Pixels(delta) => f32::from(delta.y),
            ScrollDelta::Lines(delta) => delta.y * 20.0,
        };
        if delta == 0.0 {
            return;
        }
        let factor = if delta > 0.0 {
            1.0 + delta.abs() * 0.01
        } else {
            1.0 / (1.0 + delta.abs() * 0.01)
        };
        self.zoom_live_view_at(stream_id, factor, Some(event.position), cx);
        cx.stop_propagation();
    }

    fn pinch_live_view(&mut self, stream_id: StreamId, event: &PinchEvent, cx: &mut Context<Self>) {
        self.zoom_live_view_at(
            stream_id,
            (1.0 + event.delta).max(0.01),
            Some(event.position),
            cx,
        );
        cx.stop_propagation();
    }

    fn live_mouse_down(
        &mut self,
        stream_id: StreamId,
        event: &MouseDownEvent,
        cx: &mut Context<Self>,
    ) {
        if event.button != MouseButton::Left {
            return;
        }
        if event.click_count == 2 {
            let factor = if event.modifiers.shift { 0.5 } else { 2.0 };
            self.zoom_live_view_at(stream_id, factor, Some(event.position), cx);
            return;
        }
        if let Some(view) = self.live_players.get_mut(&stream_id) {
            view.last_mouse_position = Some(event.position);
        }
    }

    fn live_mouse_move(
        &mut self,
        stream_id: StreamId,
        event: &MouseMoveEvent,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.live_players.get_mut(&stream_id) else {
            return;
        };
        if let Some(last) = view.last_mouse_position {
            view.pan += event.position - last;
            if let Some(viewport) = view.viewport_bounds.get() {
                view.pan = clamp_live_pan(view.pan, view.coded_size, viewport, view.zoom);
            }
            view.last_mouse_position = Some(event.position);
            cx.notify();
        }
    }

    fn live_mouse_up(&mut self, stream_id: StreamId, cx: &mut Context<Self>) {
        if let Some(view) = self.live_players.get_mut(&stream_id) {
            view.last_mouse_position = None;
            cx.notify();
        }
    }

    fn begin_live_pane_resize(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(bounds) = self.live_pane_bounds.get() else {
            return;
        };
        let start_height = clamp_live_pane_height(
            bounds.size.height,
            window.viewport_size().height,
            self.stacked_preview_reserved_height(window),
            window.rem_size(),
        );
        self.live_pane_height = Some(start_height);
        self.live_pane_resize = Some(LivePaneResize {
            start_y: event.position.y,
            start_height,
        });
        cx.stop_propagation();
        cx.notify();
    }

    pub(super) fn drag_live_pane(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(resize) = self.live_pane_resize else {
            return;
        };
        if !event.dragging() {
            self.finish_live_pane_resize(cx);
            return;
        }
        self.live_pane_height = Some(clamp_live_pane_height(
            resize.start_height + event.position.y - resize.start_y,
            window.viewport_size().height,
            self.stacked_preview_reserved_height(window),
            window.rem_size(),
        ));
        cx.stop_propagation();
        cx.notify();
    }

    pub(super) fn finish_live_pane_resize(&mut self, cx: &mut Context<Self>) {
        if self.live_pane_resize.take().is_none() {
            return;
        }
        for view in self.live_players.values_mut() {
            if let Some(viewport) = view.viewport_bounds.get() {
                view.pan = clamp_live_pan(view.pan, view.coded_size, viewport, view.zoom);
            }
        }
        cx.notify();
    }

    fn toggle_live_fullscreen(
        &mut self,
        stream_id: StreamId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.fullscreen_live_controls_hovered = false;
        let exiting = self.fullscreen_share == Some(stream_id);
        self.fullscreen_share = if exiting { None } else { Some(stream_id) };
        if exiting {
            self.exit_native_media_fullscreen_if_inactive(window);
        } else {
            self.enter_native_media_fullscreen(window, cx);
        }
        cx.notify();
    }

    pub(super) fn release_live_players(&mut self, window: &mut Window) {
        self.live_players.clear();
        self.live_pane_resize = None;
        self.fullscreen_live_controls_hovered = false;
        self.fullscreen_share = None;
        self.exit_native_media_fullscreen_if_inactive(window);
    }

    pub(super) fn render_live_shares(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let settings = AppliedSettings::get(cx);
        let shares = self.model.live_shares.clone();
        let resizable = !self.live_players.is_empty();
        let reserved = self.stacked_preview_reserved_height(window);
        let pane_height = resizable
            .then_some(self.live_pane_height)
            .flatten()
            .map(|height| {
                clamp_live_pane_height(
                    height,
                    window.viewport_size().height,
                    reserved,
                    window.rem_size(),
                )
            });
        if resizable {
            self.live_pane_height = pane_height;
        }
        let constrained = pane_height.is_some();
        let resizing = self.live_pane_resize.is_some();
        let pane_bounds = self.live_pane_bounds.clone();
        let mut panel = div()
            .relative()
            .flex_none()
            .flex()
            .flex_col()
            .min_h_0()
            .overflow_hidden()
            .gap_0()
            .bg(settings.theme.color(ThemeRole::Raised))
            .when(!resizable, |panel| {
                panel
                    .border_b_1()
                    .border_color(settings.theme.color(ThemeRole::BorderSubtle))
            })
            .when_some(pane_height, |panel, height| panel.h(height))
            .child(
                canvas(
                    move |bounds, _, _| pane_bounds.set(Some(bounds)),
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            );
        for share in shares {
            panel = panel.child(self.render_live_share_card(share, false, constrained, cx));
        }
        if !resizable {
            return panel;
        }
        let divider = div()
            .id("live-pane-resize")
            .h(rems_from_px(PREVIEW_DIVIDER_WIDTH))
            .w_full()
            .flex_none()
            .flex()
            .items_center()
            .cursor_row_resize()
            .hover({
                let hover = settings.theme.color(ThemeRole::StateSelection);
                move |handle| handle.bg(hover)
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    this.begin_live_pane_resize(event, window, cx)
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, cx| this.finish_live_pane_resize(cx)),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, cx| this.finish_live_pane_resize(cx)),
            )
            .child(
                div()
                    .h(rems_from_px(3.0))
                    .w_full()
                    .bg(settings.theme.color(if resizing {
                        ThemeRole::BorderFocus
                    } else {
                        ThemeRole::BorderSubtle
                    })),
            );
        div()
            .w_full()
            .flex_none()
            .flex()
            .flex_col()
            .overflow_hidden()
            .child(panel)
            .child(divider)
    }

    pub(super) fn render_live_share_card(
        &mut self,
        share: local_rpc::model::LiveShare,
        fullscreen: bool,
        constrained: bool,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let settings = AppliedSettings::get(cx);
        let stream_id = share.stream_id;
        let active = self.live_players.get(&stream_id).map(|view| {
            let viewport_bounds = view.viewport_bounds.clone();
            (
                view.player.surface(),
                view.zoom,
                view.pan,
                view.last_mouse_position.is_some(),
                viewport_bounds,
                view.coded_size,
            )
        });
        let active_share = active.is_some();
        let mut card = div()
            .id(("live-share", stream_id.0 as usize))
            .flex()
            .flex_col()
            .bg(settings.theme.color(ThemeRole::MediaViewport))
            .when(fullscreen, |card| card.relative().size_full())
            .when(constrained && active_share, |card| card.flex_1().min_h_0())
            .when(constrained && !active_share, |card| card.flex_none());
        if let Some((video_surface, zoom, pan, dragging, viewport_bounds, coded_size)) = active {
            let stop_id = stream_id;
            let reset_id = stream_id;
            let zoom_out_id = stream_id;
            let zoom_in_id = stream_id;
            let fullscreen_id = stream_id;
            let controls_hover_id = stream_id;
            let controls_visible = dragging || self.fullscreen_live_controls_hovered;
            let header = div()
                .id(("live-share-controls", stream_id.0 as usize))
                .flex()
                .flex_wrap()
                .items_center()
                .gap_1()
                .px_3()
                .py_2()
                .bg(settings.theme.color(ThemeRole::Window))
                .when(fullscreen, |header| {
                    header
                        .absolute()
                        .top_0()
                        .left_0()
                        .right_0()
                        .opacity(if controls_visible { 1.0 } else { 0.0 })
                        .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                            if this.fullscreen_share == Some(controls_hover_id)
                                && this.fullscreen_live_controls_hovered != *hovered
                            {
                                this.fullscreen_live_controls_hovered = *hovered;
                                cx.notify();
                            }
                        }))
                })
                .child(live_share_title(&share, &settings.theme))
                .child(
                    icon_button(
                        ("live-stop", stream_id.0 as usize),
                        IconName::Stop,
                        &settings.theme,
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.stop_live_share(stop_id, window, cx)
                    })),
                )
                .child(
                    icon_button(
                        ("live-reset", stream_id.0 as usize),
                        IconName::RotateCcw,
                        &settings.theme,
                    )
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.reset_live_view(reset_id, cx)),
                    ),
                )
                .child(
                    icon_button(
                        ("live-zoom-out", stream_id.0 as usize),
                        IconName::ZoomOut,
                        &settings.theme,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.zoom_live_view(zoom_out_id, 1.0 / 1.25, cx)
                    })),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(settings.theme.color(ThemeRole::TextMuted))
                        .child(format!("{:.0}%", zoom * 100.0)),
                )
                .child(
                    icon_button(
                        ("live-zoom-in", stream_id.0 as usize),
                        IconName::ZoomIn,
                        &settings.theme,
                    )
                    .on_click(
                        cx.listener(move |this, _, _, cx| {
                            this.zoom_live_view(zoom_in_id, 1.25, cx)
                        }),
                    ),
                )
                .child(
                    icon_button(
                        ("live-fullscreen", stream_id.0 as usize),
                        if fullscreen {
                            IconName::Minimize
                        } else {
                            IconName::Maximize
                        },
                        &settings.theme,
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.toggle_live_fullscreen(fullscreen_id, window, cx)
                    })),
                );
            let scroll_id = stream_id;
            let down_id = stream_id;
            let move_id = stream_id;
            let up_id = stream_id;
            let pinch_id = stream_id;
            let viewport = div()
                .relative()
                .overflow_hidden()
                .w_full()
                .when(fullscreen || constrained, |viewport| {
                    viewport.flex_1().min_h_0()
                })
                .when(!fullscreen && !constrained, |viewport| {
                    viewport.h(rems_from_px(320.))
                })
                .bg(settings.theme.color(ThemeRole::MediaViewport))
                .cursor(if dragging {
                    gpui::CursorStyle::ClosedHand
                } else {
                    gpui::CursorStyle::OpenHand
                })
                .on_scroll_wheel(cx.listener(move |this, event: &ScrollWheelEvent, _, cx| {
                    this.scroll_live_view(scroll_id, event, cx)
                }))
                .on_pinch(cx.listener(move |this, event: &PinchEvent, _, cx| {
                    this.pinch_live_view(pinch_id, event, cx)
                }))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                        this.live_mouse_down(down_id, event, cx)
                    }),
                )
                .on_mouse_move(cx.listener(move |this, event: &MouseMoveEvent, _, cx| {
                    this.live_mouse_move(move_id, event, cx)
                }))
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(move |this, _: &MouseUpEvent, _, cx| this.live_mouse_up(up_id, cx)),
                )
                .child(
                    canvas(
                        move |bounds, _, _| {
                            viewport_bounds.set(Some(bounds));
                            let pan = clamp_live_pan(pan, coded_size, bounds, zoom);
                            LiveVideoGeometry::new(coded_size, bounds, zoom, pan)
                        },
                        move |_, geometry, window, _| {
                            if let Some(geometry) = geometry {
                                window.paint_platform_surface(geometry.bounds, video_surface);
                            }
                        },
                    )
                    .absolute()
                    .size_full(),
                );
            card = if fullscreen {
                card.child(viewport).child(header)
            } else {
                card.child(header).child(viewport)
            };
        } else {
            card = card.child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_1()
                    .px_3()
                    .py_2()
                    .bg(settings.theme.color(ThemeRole::Window))
                    .child(live_share_title(&share, &settings.theme))
                    .child(
                        icon_button(
                            ("live-play", stream_id.0 as usize),
                            IconName::Play,
                            &settings.theme,
                        )
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.start_live_share(stream_id, cx)),
                        ),
                    ),
            );
        }
        card
    }
}
