use super::*;

impl ChattView {
    fn begin_preview_session(&mut self, window: &Window, cx: &App) {
        if self.preview_history.active().is_none() {
            self.preview_return_focus = window.focused(cx).map(|focus| focus.downgrade());
            // A body too narrow to split is narrower than any split the user
            // would pick, so seeding from it would shrink the chat once the
            // window grows back.
            if self.preview_layout(window) == PreviewLayout::Split {
                let body_width = self.chat_body_width(window);
                self.preview_chat_width = default_chat_width(body_width, window.rem_size());
            }
        }
    }

    pub(super) fn chat_body_width(&self, window: &Window) -> Pixels {
        window.viewport_size().width
            - if self.show_rooms_sidebar {
                crate::ui_scale::scaled_px(SIDEBAR_WIDTH, window.rem_size())
            } else {
                px(0.)
            }
    }

    pub(super) fn preview_layout(&self, window: &Window) -> PreviewLayout {
        preview_layout(
            self.chat_body_width(window),
            window.viewport_size().height,
            if self.live_players.is_empty() {
                LiveVideo::Idle
            } else {
                LiveVideo::Playing
            },
            window.rem_size(),
        )
    }

    /// Distance from the top of the window to the top of the viewer surface.
    /// The tab bar sits above it in every layout, and the live share pane sits
    /// above the tab bar in the layouts that share the body column with it.
    pub(super) fn preview_chrome_top(&self, layout: PreviewLayout, window: &Window) -> Pixels {
        let tab_bar = crate::ui_scale::scaled_px(PREVIEW_TAB_BAR_HEIGHT, window.rem_size());
        if layout == PreviewLayout::Split || self.model.live_shares.is_empty() {
            return tab_bar;
        }
        // The cell keeps the last rendered bounds, so it is only meaningful
        // while the live pane is on screen.
        let live_pane_bottom = self
            .live_pane_bounds
            .get()
            .map_or(px(0.), |bounds| bounds.bottom());
        tab_bar + live_pane_bottom
    }

    /// What the stacked viewer and the chat below it share, once the live pane
    /// and the tab bar have taken their part of the window.
    fn stacked_body_height(&self, window: &Window) -> Pixels {
        (window.viewport_size().height - self.preview_chrome_top(PreviewLayout::Stacked, window))
            .max(px(0.))
    }

    /// Window height the stacked viewer holds above the chat, which the live
    /// pane must not be resized over. Zero unless that viewer is on screen.
    pub(super) fn stacked_preview_reserved_height(&self, window: &Window) -> Pixels {
        if self.preview_layout(window) != PreviewLayout::Stacked
            || self.preview_history.active().is_none()
        {
            return px(0.);
        }
        crate::ui_scale::scaled_px(
            MIN_STACKED_VIEWER_HEIGHT + PREVIEW_TAB_BAR_HEIGHT + PREVIEW_DIVIDER_WIDTH,
            window.rem_size(),
        )
    }

    pub(super) fn restore_preview_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(focus) = self
            .preview_return_focus
            .take()
            .and_then(|focus| focus.upgrade())
        {
            window.focus(&focus, cx);
        } else {
            window.blur();
        }
    }

    fn reveal_active_preview_tab(&self) {
        let Some(active_key) = self.preview_history.active_key() else {
            return;
        };
        if let Some(index) = self
            .preview_history
            .items()
            .iter()
            .position(|item| item.key() == active_key)
        {
            self.preview_tabs_scroll.scroll_to_item(index);
        }
    }

    fn cancel_code_load(&mut self, key: AttachmentId) {
        self.code_load_tasks.remove(&key);
    }

    fn apply_preview_eviction(&mut self, evicted: Option<AttachmentId>) {
        if let Some(evicted) = evicted {
            self.cancel_code_load(evicted);
        }
    }

    pub(super) fn open_image_preview(
        &mut self,
        descriptor: AttachmentDescriptor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(attachment) = self
            .media_cache
            .lock()
            .expect("media cache lock poisoned")
            .get(descriptor.id)
        else {
            self.status = format!("{} is not cached yet", descriptor.file_name).into();
            cx.notify();
            return;
        };
        let natural_size = match (descriptor.width, descriptor.height) {
            (Some(width), Some(height)) if width > 0 && height > 0 => (width, height),
            _ => image_dimensions_from_bytes(attachment.bytes()).unwrap_or((640, 480)),
        };
        self.begin_preview_session(window, cx);
        let item = PreviewItem::image(descriptor, natural_size);
        let opened = self.preview_history.open(item);
        self.apply_preview_eviction(opened.evicted);
        if opened.active_changed {
            self.code_selection.clear();
        }
        self.reveal_active_preview_tab();
        self.close_code_search(cx);
        window.focus(&self.code_viewer_focus, cx);
        if opened.active_changed {
            self.preview_image.reset(
                self.preview_history
                    .active()
                    .and_then(PreviewItem::image_size)
                    .expect("new image preview has image content"),
            );
            self.preview_image_viewport.set(None);
            self.preview_last_mouse_position = None;
        }
        cx.notify();
    }

    pub(super) fn open_code_preview(
        &mut self,
        room_id: RoomId,
        descriptor: AttachmentDescriptor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (cached_attachment, active_transfer) = {
            let mut cache = self.media_cache.lock().expect("media cache lock poisoned");
            (cache.get(descriptor.id), cache.active_transfer(&descriptor))
        };
        self.begin_preview_session(window, cx);
        let opened = self
            .preview_history
            .open(PreviewItem::code(descriptor.clone(), active_transfer));
        self.apply_preview_eviction(opened.evicted);
        if opened.active_changed {
            self.code_selection.clear();
        }
        self.reveal_active_preview_tab();
        self.close_code_search(cx);
        self.preview_image_viewport.set(None);
        self.preview_last_mouse_position = None;
        window.focus(&self.code_viewer_focus, cx);

        if let Some(reason) = code_preview_size_error(descriptor.byte_len) {
            self.cancel_code_load(descriptor.id);
            if let Some(preview) = self
                .preview_history
                .item_mut(descriptor.id)
                .and_then(PreviewItem::code_preview_mut)
            {
                preview.state = CodePreviewState::Error(reason.into());
            }
            cx.notify();
            return;
        }

        let ready_or_preparing = self
            .preview_history
            .item(descriptor.id)
            .and_then(PreviewItem::code_preview)
            .is_some_and(|preview| {
                matches!(
                    preview.state,
                    CodePreviewState::Ready(_) | CodePreviewState::Preparing { .. }
                )
            });
        if ready_or_preparing {
            cx.notify();
            return;
        }

        if let Some(attachment) = cached_attachment {
            self.start_code_preview_load(descriptor.id, attachment, cx);
        } else if let Some(transfer_id) = active_transfer {
            if let Some(preview) = self
                .preview_history
                .item_mut(descriptor.id)
                .and_then(PreviewItem::code_preview_mut)
            {
                preview.state = CodePreviewState::Fetching {
                    transfer_id: Some(transfer_id),
                };
            }
        } else {
            match self.begin_attachment_read(room_id, descriptor.clone(), cx) {
                Ok(Some(transfer_id)) => {
                    if let Some(preview) = self
                        .preview_history
                        .item_mut(descriptor.id)
                        .and_then(PreviewItem::code_preview_mut)
                    {
                        preview.state = CodePreviewState::Fetching {
                            transfer_id: Some(transfer_id),
                        };
                    }
                }
                Ok(None) => {
                    let attachment = self
                        .media_cache
                        .lock()
                        .expect("media cache lock poisoned")
                        .get(descriptor.id);
                    if let Some(attachment) = attachment {
                        self.start_code_preview_load(descriptor.id, attachment, cx);
                    } else if let Some(preview) = self
                        .preview_history
                        .item_mut(descriptor.id)
                        .and_then(PreviewItem::code_preview_mut)
                    {
                        preview.state = CodePreviewState::Error("attachment was not cached".into());
                    }
                }
                Err(reason) => {
                    if let Some(preview) = self
                        .preview_history
                        .item_mut(descriptor.id)
                        .and_then(PreviewItem::code_preview_mut)
                    {
                        preview.state = CodePreviewState::Error(reason.clone());
                    }
                    self.status = reason.into();
                }
            }
        }
        cx.notify();
    }

    fn start_code_preview_load(
        &mut self,
        key: AttachmentId,
        attachment: CachedAttachment,
        cx: &mut Context<Self>,
    ) {
        let Some(item) = self.preview_history.item(key) else {
            return;
        };
        let file_name = item.descriptor.file_name.clone();
        self.cancel_code_load(key);
        let load_id = self.next_code_load_id;
        self.next_code_load_id = self.next_code_load_id.wrapping_add(1).max(1);
        let Some(preview) = self
            .preview_history
            .item_mut(key)
            .and_then(PreviewItem::code_preview_mut)
        else {
            return;
        };
        preview.view_state.reset();
        preview.scrollbar_state.reset();
        preview.state = CodePreviewState::Preparing { load_id };
        if self.preview_history.active_key() == Some(key) {
            self.code_selection.clear();
        }

        let executor = cx.background_executor().clone();
        let task = cx.spawn(async move |this, cx| {
            let result = executor
                .spawn(async move { CodeDocument::load(attachment.bytes(), &file_name, load_id) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if !this
                    .code_load_tasks
                    .get(&key)
                    .is_some_and(|(active_load, _)| *active_load == load_id)
                {
                    return;
                }
                this.code_load_tasks.remove(&key);
                let Some(preview) = this
                    .preview_history
                    .item_mut(key)
                    .and_then(PreviewItem::code_preview_mut)
                else {
                    return;
                };
                if !matches!(
                    preview.state,
                    CodePreviewState::Preparing {
                        load_id: active_load
                    } if active_load == load_id
                ) {
                    return;
                }
                preview.state = match result {
                    Ok(document) => CodePreviewState::Ready(document),
                    Err(reason) => CodePreviewState::Error(reason),
                };
                cx.notify();
            });
        });
        self.code_load_tasks.insert(key, (load_id, task));
    }

    pub(super) fn resume_code_preview_load(
        &mut self,
        descriptor: &AttachmentDescriptor,
        cx: &mut Context<Self>,
    ) {
        let should_load = self
            .preview_history
            .item(descriptor.id)
            .and_then(PreviewItem::code_preview)
            .is_some_and(|preview| {
                matches!(
                    preview.state,
                    CodePreviewState::Fetching { .. } | CodePreviewState::Error(_)
                )
            });
        if !should_load {
            return;
        }
        let attachment = self
            .media_cache
            .lock()
            .expect("media cache lock poisoned")
            .get(descriptor.id);
        if let Some(attachment) = attachment {
            self.start_code_preview_load(descriptor.id, attachment, cx);
        }
    }

    pub(super) fn active_code_document(
        &self,
    ) -> Option<(Arc<CodeDocument>, UniformListScrollHandle, CodeViewState)> {
        let preview = self.preview_history.active()?.code_preview()?;
        let CodePreviewState::Ready(document) = &preview.state else {
            return None;
        };
        Some((
            document.clone(),
            preview.scroll_handle.clone(),
            preview.view_state.clone(),
        ))
    }

    fn select_preview(&mut self, key: AttachmentId, window: &mut Window, cx: &mut Context<Self>) {
        if self.preview_history.item(key).is_none() {
            return;
        }
        // Reached from the chat tab as well, so the session may need reopening.
        self.begin_preview_session(window, cx);
        if self.preview_history.select(key) {
            self.code_selection.clear();
            self.close_code_search(cx);
            if let Some(natural_size) = self
                .preview_history
                .active()
                .and_then(PreviewItem::image_size)
            {
                self.preview_image.reset(natural_size);
            }
            window.focus(&self.code_viewer_focus, cx);
            self.reveal_active_preview_tab();
            self.preview_image_viewport.set(None);
            self.preview_last_mouse_position = None;
            cx.notify();
        }
    }

    /// Teardown shared by every way of leaving the viewer.
    fn leave_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.code_selection.clear();
        self.close_code_search(cx);
        self.preview_image_viewport.set(None);
        self.preview_last_mouse_position = None;
        self.restore_preview_focus(window, cx);
    }

    /// Swaps the viewer for the chat while keeping the tab bar, which is what
    /// the pinned chat tab does in the tabbed layout.
    fn show_chat_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let was_active = self.preview_history.active().is_some();
        if !self.preview_history.select_chat() {
            return;
        }
        if was_active {
            self.leave_preview(window, cx);
        }
        cx.notify();
    }

    /// Dismisses the viewer entirely — the panel in the split layout, the tab
    /// bar in the tabbed one. The tabs survive for the next preview.
    fn close_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let was_active = self.preview_history.active().is_some();
        if !self.preview_history.close_panel() {
            return;
        }
        self.preview_pane_resize = None;
        self.preview_stack_resize = None;
        if was_active {
            self.leave_preview(window, cx);
        }
        cx.notify();
    }

    pub(super) fn close_preview_action(
        &mut self,
        _: &ClosePreview,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.exit_video_theater(cx) {
            return;
        }
        self.close_preview(window, cx);
    }

    fn close_preview_tab(
        &mut self,
        key: AttachmentId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cancel_code_load(key);
        if self.preview_history.close_tab(key) {
            self.code_selection.clear();
            self.close_code_search(cx);
            if let Some(natural_size) = self
                .preview_history
                .active()
                .and_then(PreviewItem::image_size)
            {
                self.preview_image.reset(natural_size);
            }
            if self.preview_history.active().is_some() {
                window.focus(&self.code_viewer_focus, cx);
                self.reveal_active_preview_tab();
            } else {
                self.restore_preview_focus(window, cx);
            }
            self.preview_image_viewport.set(None);
            self.preview_last_mouse_position = None;
        }
        cx.notify();
    }

    fn save_preview_attachment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = self.preview_history.active().cloned() else {
            return;
        };
        let Some(attachment) = self
            .media_cache
            .lock()
            .expect("media cache lock poisoned")
            .get(item.descriptor.id)
        else {
            self.status = format!("{} is no longer cached", item.descriptor.file_name).into();
            cx.notify();
            return;
        };
        let directory = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let receiver = cx.prompt_for_new_path(&directory, Some(&item.descriptor.file_name));
        let executor = cx.background_executor().clone();
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(destination)) = receiver.await else {
                return;
            };
            let result = executor
                .spawn(async move {
                    write_cached_attachment_to_user_selected_path(&attachment, destination)
                })
                .await;
            let destination = match result {
                Ok(Some(destination)) => destination,
                Ok(None) => return,
                Err(error) => {
                    let _ = this.update_in(cx, |this, _, cx| {
                        this.status = format!("Could not save file · {error}").into();
                        cx.notify();
                    });
                    return;
                }
            };
            let _ = this.update_in(cx, |this, _, cx| {
                this.status = format!("Saved to {}", destination.display()).into();
                cx.notify();
            });
        })
        .detach();
    }

    fn copy_preview_code(&mut self, cx: &mut Context<Self>) {
        let Some(document) = self
            .preview_history
            .active()
            .and_then(PreviewItem::code_preview)
            .and_then(|preview| match &preview.state {
                CodePreviewState::Ready(document) => Some(document),
                _ => None,
            })
        else {
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(document.source().to_string()));
        self.status = "Copied file".into();
        cx.notify();
    }

    fn preview_viewport(&self) -> Option<Bounds<Pixels>> {
        self.preview_image_viewport.get()
    }

    fn fit_preview_image(&mut self, cx: &mut Context<Self>) {
        if let Some(viewport) = self.preview_viewport() {
            self.preview_image.fit(viewport);
            cx.notify();
        }
    }

    fn actual_size_preview_image(&mut self, cx: &mut Context<Self>) {
        self.preview_image.actual_size();
        cx.notify();
    }

    fn zoom_preview_image(&mut self, delta: f32, cx: &mut Context<Self>) {
        if let Some(viewport) = self.preview_viewport() {
            self.preview_image.zoom_from_center(delta, viewport);
            cx.notify();
        }
    }

    fn scroll_preview_image(&mut self, event: &ScrollWheelEvent, cx: &mut Context<Self>) {
        let Some(viewport) = self.preview_viewport() else {
            return;
        };
        let delta = match event.delta {
            ScrollDelta::Pixels(delta) => f32::from(delta.y),
            ScrollDelta::Lines(delta) => delta.y * 16.0,
        };
        if delta == 0.0 {
            return;
        }
        self.preview_image
            .zoom_by_factor((delta * 0.002).exp(), viewport, event.position);
        cx.stop_propagation();
        cx.notify();
    }

    fn pinch_preview_image(&mut self, event: &PinchEvent, cx: &mut Context<Self>) {
        let Some(viewport) = self.preview_viewport() else {
            return;
        };
        self.preview_image
            .zoom_by_factor((1.0 + event.delta).max(0.01), viewport, event.position);
        cx.stop_propagation();
        cx.notify();
    }

    fn preview_image_mouse_down(&mut self, event: &MouseDownEvent, cx: &mut Context<Self>) {
        if event.button != MouseButton::Left {
            return;
        }
        self.preview_last_mouse_position = Some(event.position);
        cx.stop_propagation();
    }

    pub(super) fn preview_image_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        cx: &mut Context<Self>,
    ) {
        let Some(previous) = self.preview_last_mouse_position else {
            return;
        };
        if !event.dragging() {
            self.finish_preview_image_pan(cx);
            return;
        }
        if let Some(viewport) = self.preview_viewport() {
            self.preview_image
                .pan_by(event.position - previous, viewport);
            self.preview_last_mouse_position = Some(event.position);
            cx.stop_propagation();
            cx.notify();
        }
    }

    pub(super) fn finish_preview_image_pan(&mut self, cx: &mut Context<Self>) {
        if self.preview_last_mouse_position.take().is_some() {
            cx.notify();
        }
    }

    pub(super) fn begin_preview_pane_resize(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let body_width = self.chat_body_width(window);
        self.preview_chat_width =
            clamp_chat_width(self.preview_chat_width, body_width, window.rem_size());
        self.preview_pane_resize = Some(PreviewPaneResize {
            start_x: event.position.x,
            start_chat_width: self.preview_chat_width,
        });
        cx.stop_propagation();
        cx.notify();
    }

    pub(super) fn drag_preview_pane(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(resize) = self.preview_pane_resize else {
            return;
        };
        if !event.dragging() {
            self.finish_preview_pane_resize(cx);
            return;
        }
        let body_width = self.chat_body_width(window);
        self.preview_chat_width = clamp_chat_width(
            resize.start_chat_width + event.position.x - resize.start_x,
            body_width,
            window.rem_size(),
        );
        cx.stop_propagation();
        cx.notify();
    }

    pub(super) fn finish_preview_pane_resize(&mut self, cx: &mut Context<Self>) {
        if self.preview_pane_resize.take().is_some() {
            cx.notify();
        }
    }

    fn begin_preview_stack_resize(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let start_height = stacked_viewer_height(
            self.preview_stack_height,
            self.stacked_body_height(window),
            window.rem_size(),
        );
        self.preview_stack_height = Some(start_height);
        self.preview_stack_resize = Some(PreviewStackResize {
            start_y: event.position.y,
            start_height,
        });
        cx.stop_propagation();
        cx.notify();
    }

    pub(super) fn drag_preview_stack(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(resize) = self.preview_stack_resize else {
            return;
        };
        if !event.dragging() {
            self.finish_preview_stack_resize(cx);
            return;
        }
        self.preview_stack_height = Some(stacked_viewer_height(
            Some(resize.start_height + event.position.y - resize.start_y),
            self.stacked_body_height(window),
            window.rem_size(),
        ));
        cx.stop_propagation();
        cx.notify();
    }

    pub(super) fn finish_preview_stack_resize(&mut self, cx: &mut Context<Self>) {
        if self.preview_stack_resize.take().is_some() {
            cx.notify();
        }
    }

    fn render_preview_tabs(
        &mut self,
        active_key: Option<AttachmentId>,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let settings = AppliedSettings::get(cx);
        let close_hover = settings.theme.color(ThemeRole::StateHover);
        let close_hover_text = settings.theme.color(ThemeRole::TextPrimary);
        let history = self.preview_history.items().to_vec();
        let mut tabs = div()
            .id("preview-tabs")
            .h_full()
            .flex_1()
            .min_w_0()
            .flex()
            .overflow_x_scroll()
            .track_scroll(&self.preview_tabs_scroll);
        for item in history {
            let key = item.key();
            let selected = active_key == Some(key);
            let tab_icon = match item.content {
                PreviewContent::Image { .. } => IconName::Image,
                PreviewContent::Code(_) => IconName::FileText,
            };
            let tab_icon_color = settings.theme.color(if selected {
                ThemeRole::MediaProgressKnob
            } else {
                ThemeRole::TextMuted
            });
            let select_key = key;
            let close_key = key;
            let select_id: SharedString = format!(
                "preview-tab-select-{}-{}",
                key.timestamp_ms, key.transfer_id.0
            )
            .into();
            let close_id: SharedString = format!(
                "preview-tab-close-{}-{}",
                key.timestamp_ms, key.transfer_id.0
            )
            .into();
            tabs = tabs.child(
                preview_tab_shell(selected, &settings.theme)
                    .child(
                        div()
                            .id(select_id)
                            .h_full()
                            .min_w_0()
                            .flex_1()
                            .flex()
                            .items_center()
                            .gap_2()
                            .pl_3()
                            .pr_1()
                            .cursor_pointer()
                            .child(icon(tab_icon, PREVIEW_HEADER_ICON_SIZE, tab_icon_color))
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_xs()
                                    .child(item.descriptor.file_name),
                            )
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.select_preview(select_key, window, cx)
                            })),
                    )
                    .child(
                        div()
                            .id(close_id)
                            .w(rems_from_px(28.0))
                            .h_full()
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .cursor_pointer()
                            .hover(move |button| {
                                button.bg(close_hover).text_color(close_hover_text)
                            })
                            .child(icon(
                                IconName::Close,
                                PREVIEW_HEADER_ICON_SIZE,
                                settings.theme.color(ThemeRole::TextMuted),
                            ))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.close_preview_tab(close_key, window, cx)
                            })),
                    ),
            );
        }
        tabs
    }

    /// The pinned tab that brings the chat timeline back in the tabbed layout.
    fn render_chat_tab(&mut self, selected: bool, cx: &mut Context<Self>) -> Stateful<Div> {
        let settings = AppliedSettings::get(cx);
        let room_name = self
            .model
            .selected_room()
            .map(|room| room.name.clone())
            .unwrap_or_else(|| "Chat".into());
        preview_tab_shell(selected, &settings.theme)
            .id("preview-tab-chat")
            .min_w_0()
            .gap_2()
            .px_3()
            .cursor_pointer()
            .child(
                div()
                    .flex_none()
                    .text_xs()
                    .text_color(settings.theme.color(ThemeRole::TextDim))
                    .child("#"),
            )
            .child(div().min_w_0().truncate().text_xs().child(room_name))
            .on_click(cx.listener(|this, _, window, cx| this.show_chat_tab(window, cx)))
    }

    /// The controls at the right end of the tab bar. Everything but the close
    /// button acts on the active preview, so the chat tab keeps only that one —
    /// it dismisses the whole bar.
    fn render_preview_actions(
        &mut self,
        active: Option<&PreviewItem>,
        viewport: Bounds<Pixels>,
        cx: &mut Context<Self>,
    ) -> Div {
        let settings = AppliedSettings::get(cx);
        let actions = div()
            .h_full()
            .flex_none()
            .flex()
            .items_center()
            .gap_1()
            .px_2()
            .bg(settings.theme.color(ThemeRole::Panel));
        let Some(active) = active else {
            return actions.child(
                preview_action_button("preview-close", IconName::Close, &settings.theme)
                    .on_click(cx.listener(|this, _, window, cx| this.close_preview(window, cx))),
            );
        };
        let actions = match &active.content {
            PreviewContent::Image { .. } => {
                let zoom_percent = self.preview_image.zoom_percent(viewport);
                actions
                    .child(
                        preview_control_button("preview-fit", "Fit", &settings.theme)
                            .min_h(rems_from_px(PREVIEW_TAB_BAR_HEIGHT))
                            .on_click(cx.listener(|this, _, _, cx| this.fit_preview_image(cx))),
                    )
                    .child(
                        preview_control_button("preview-actual", "1:1", &settings.theme)
                            .min_h(rems_from_px(PREVIEW_TAB_BAR_HEIGHT))
                            .on_click(
                                cx.listener(|this, _, _, cx| this.actual_size_preview_image(cx)),
                            ),
                    )
                    .child(
                        preview_control_button("preview-zoom-out", "−", &settings.theme)
                            .min_h(rems_from_px(PREVIEW_TAB_BAR_HEIGHT))
                            .on_click(
                                cx.listener(|this, _, _, cx| this.zoom_preview_image(-0.25, cx)),
                            ),
                    )
                    .child(
                        div()
                            .w(rems_from_px(44.0))
                            .text_center()
                            .text_xs()
                            .text_color(settings.theme.color(ThemeRole::TextMuted))
                            .child(format!("{zoom_percent}%")),
                    )
                    .child(
                        preview_control_button("preview-zoom-in", "+", &settings.theme)
                            .min_h(rems_from_px(PREVIEW_TAB_BAR_HEIGHT))
                            .on_click(
                                cx.listener(|this, _, _, cx| this.zoom_preview_image(0.25, cx)),
                            ),
                    )
            }
            PreviewContent::Code(preview) => {
                let ready = matches!(preview.state, CodePreviewState::Ready(_));
                actions.when(ready, |actions| {
                    actions
                        .child(
                            preview_action_button(
                                "preview-find-code",
                                IconName::Search,
                                &settings.theme,
                            )
                            .on_click(
                                cx.listener(|this, _, window, cx| {
                                    this.open_code_search(window, cx)
                                }),
                            ),
                        )
                        .child(
                            preview_action_button(
                                "preview-copy-code",
                                IconName::Copy,
                                &settings.theme,
                            )
                            .on_click(cx.listener(|this, _, _, cx| this.copy_preview_code(cx))),
                        )
                })
            }
        };
        actions
            .child(
                preview_action_button("preview-save", IconName::Download, &settings.theme)
                    .on_click(
                        cx.listener(|this, _, window, cx| this.save_preview_attachment(window, cx)),
                    ),
            )
            .child(
                preview_action_button("preview-close", IconName::Close, &settings.theme)
                    .on_click(cx.listener(|this, _, window, cx| this.close_preview(window, cx))),
            )
    }

    /// The tab strip. In the tabbed layout it also owns the chat/viewer switch,
    /// so it is rendered above both of them rather than inside the panel.
    pub(super) fn render_preview_tab_bar(
        &mut self,
        active: Option<&PreviewItem>,
        tabbed: bool,
        viewport: Bounds<Pixels>,
        cx: &mut Context<Self>,
    ) -> Div {
        let settings = AppliedSettings::get(cx);
        let chat_tab = tabbed.then(|| self.render_chat_tab(active.is_none(), cx));
        let tabs = self.render_preview_tabs(active.map(PreviewItem::key), cx);
        let actions = self.render_preview_actions(active, viewport, cx);
        div()
            .min_h(rems_from_px(PREVIEW_TAB_BAR_HEIGHT))
            .flex_none()
            .flex()
            .flex_wrap()
            .items_center()
            .min_w_0()
            .border_b_1()
            .border_color(settings.theme.color(ThemeRole::BorderSubtle))
            .bg(settings.theme.color(ThemeRole::Panel))
            .when_some(chat_tab, |bar, chat_tab| bar.child(chat_tab))
            .child(tabs)
            .child(actions)
    }

    /// The viewer itself, without the tab bar. Owns the code viewer focus in
    /// both layouts.
    pub(super) fn render_preview_surface(
        &mut self,
        active: &PreviewItem,
        width: Pixels,
        viewport: Bounds<Pixels>,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let settings = AppliedSettings::get(cx);
        let code = matches!(active.content, PreviewContent::Code(_));
        let body = match active.content {
            PreviewContent::Image { .. } => self.render_image_preview_body(active, viewport, cx),
            PreviewContent::Code(_) => self.render_code_preview_body(active, width, cx),
        };
        div()
            .id("preview-surface")
            .flex_1()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(settings.theme.color(ThemeRole::Window))
            .when(code, |surface| surface.key_context("ChattCodeViewer"))
            .track_focus(&self.code_viewer_focus)
            .child(body)
    }

    /// The stacked layout's pane: tab bar over a viewer of its own height, with
    /// the chat below it. The chat needs no tab of its own there, and the tabs
    /// outlive a viewer dismissed from the tabbed layout.
    pub(super) fn render_stacked_preview(
        &mut self,
        active: Option<&PreviewItem>,
        width: Pixels,
        viewer_height: Pixels,
        viewport: Bounds<Pixels>,
        cx: &mut Context<Self>,
    ) -> Div {
        let settings = AppliedSettings::get(cx);
        let resizing = self.preview_stack_resize.is_some();
        let tab_bar = self.render_preview_tab_bar(active, false, viewport, cx);
        let surface = active.map(|active| self.render_preview_surface(active, width, viewport, cx));
        div()
            .w_full()
            .flex_none()
            .flex()
            .flex_col()
            .overflow_hidden()
            .child(tab_bar)
            .when_some(surface, |pane, surface| {
                pane.child(
                    div()
                        .w_full()
                        .h(viewer_height)
                        .flex_none()
                        .flex()
                        .flex_col()
                        .overflow_hidden()
                        .child(surface),
                )
                .child(
                    div()
                        .id("preview-stack-resize")
                        .h(rems_from_px(PREVIEW_DIVIDER_WIDTH))
                        .w_full()
                        .flex_none()
                        .flex()
                        .items_center()
                        .cursor_row_resize()
                        .hover({
                            let hover = settings.theme.color(ThemeRole::StateSelection);
                            move |divider| divider.bg(hover)
                        })
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, event: &MouseDownEvent, window, cx| {
                                this.begin_preview_stack_resize(event, window, cx)
                            }),
                        )
                        .on_mouse_up(
                            MouseButton::Left,
                            cx.listener(|this, _: &MouseUpEvent, _, cx| {
                                this.finish_preview_stack_resize(cx)
                            }),
                        )
                        .on_mouse_up_out(
                            MouseButton::Left,
                            cx.listener(|this, _: &MouseUpEvent, _, cx| {
                                this.finish_preview_stack_resize(cx)
                            }),
                        )
                        .child(div().h(rems_from_px(3.0)).w_full().bg(settings.theme.color(
                            if resizing {
                                ThemeRole::BorderFocus
                            } else {
                                ThemeRole::BorderSubtle
                            },
                        ))),
                )
            })
    }

    /// The side-by-side layout's panel: tab bar stacked over the viewer.
    pub(super) fn render_preview_panel(
        &mut self,
        active: &PreviewItem,
        width: Pixels,
        viewport: Bounds<Pixels>,
        cx: &mut Context<Self>,
    ) -> Div {
        let tab_bar = self.render_preview_tab_bar(Some(active), false, viewport, cx);
        let surface = self.render_preview_surface(active, width, viewport, cx);
        div()
            .w(width)
            .h_full()
            .min_w_0()
            .flex_none()
            .flex()
            .flex_col()
            .overflow_hidden()
            .child(tab_bar)
            .child(surface)
    }

    fn render_image_preview_body(
        &mut self,
        active: &PreviewItem,
        viewport: Bounds<Pixels>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let settings = AppliedSettings::get(cx);
        let muted = settings.theme.color(ThemeRole::TextMuted);
        let cached_attachment = self
            .media_cache
            .lock()
            .expect("media cache lock poisoned")
            .get(active.descriptor.id);
        let cache_missing = cached_attachment.is_none();
        self.preview_image_viewport.set(Some(viewport));
        let geometry = self.preview_image.geometry(viewport);
        let can_pan = self.preview_image.can_pan(viewport);
        let panning = self.preview_last_mouse_position.is_some() && can_pan;

        div()
            .id("preview-image-viewport")
            .relative()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .bg(settings.theme.color(ThemeRole::MediaViewport))
            .cursor(if panning {
                gpui::CursorStyle::ClosedHand
            } else if can_pan {
                gpui::CursorStyle::OpenHand
            } else {
                gpui::CursorStyle::Arrow
            })
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _, cx| {
                this.scroll_preview_image(event, cx)
            }))
            .on_pinch(
                cx.listener(|this, event: &PinchEvent, _, cx| this.pinch_preview_image(event, cx)),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, _, cx| {
                    this.preview_image_mouse_down(event, cx)
                }),
            )
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                this.preview_image_mouse_move(event, cx)
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, cx| this.finish_preview_image_pan(cx)),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, cx| this.finish_preview_image_pan(cx)),
            )
            .when_some(cached_attachment, |viewport_element, attachment| {
                viewport_element.child(
                    img(cached_attachment_image_source(
                        attachment,
                        self.preview_image_cache.clone(),
                    ))
                    .absolute()
                    .left(geometry.bounds.origin.x - viewport.origin.x)
                    .top(geometry.bounds.origin.y - viewport.origin.y)
                    .w(geometry.bounds.size.width)
                    .h(geometry.bounds.size.height)
                    .object_fit(ObjectFit::Contain)
                    .with_loading(move || {
                        div()
                            .size_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_sm()
                            .text_color(muted)
                            .child("loading…")
                            .into_any_element()
                    })
                    .with_fallback(move || {
                        div()
                            .size_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_sm()
                            .text_color(muted)
                            .child("failed to load image")
                            .into_any_element()
                    }),
                )
            })
            .when(cache_missing, |viewport_element| {
                viewport_element.child(
                    div()
                        .absolute()
                        .inset_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_sm()
                        .text_color(muted)
                        .child("image is no longer cached"),
                )
            })
            .into_any_element()
    }

    fn render_code_preview_body(
        &mut self,
        active: &PreviewItem,
        width: Pixels,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let settings = AppliedSettings::get(cx);
        let preview = active
            .code_preview()
            .expect("code preview panel requires code content")
            .clone();
        let ready = matches!(&preview.state, CodePreviewState::Ready(_));
        let compact_search =
            width < crate::ui_scale::scaled_px(360.0, crate::ui_scale::rem_size(cx));
        let active_match = self
            .code_search_open
            .then(|| {
                let CodePreviewState::Ready(document) = &preview.state else {
                    return None;
                };
                self.code_search_results
                    .get(self.code_search_result_index)
                    .map(|search_match| document.match_target(search_match))
            })
            .flatten();
        let active_match_hidden = active_match.is_some_and(|target| target.hidden);
        let search_status: SharedString = if self.code_search_input.read(cx).text().is_empty() {
            "".into()
        } else if self.code_search_pending {
            if compact_search {
                "…".into()
            } else {
                "Searching…".into()
            }
        } else if self.code_search_results.is_empty() {
            if compact_search {
                "0".into()
            } else {
                "No matches".into()
            }
        } else {
            if compact_search {
                format!(
                    "{}/{}{}",
                    self.code_search_result_index + 1,
                    self.code_search_results.len(),
                    if active_match_hidden { "*" } else { "" }
                )
                .into()
            } else {
                format!(
                    "{} / {}{}",
                    self.code_search_result_index + 1,
                    self.code_search_results.len(),
                    if active_match_hidden {
                        " · hidden"
                    } else {
                        ""
                    }
                )
                .into()
            }
        };
        let body = match preview.state {
            CodePreviewState::Fetching { .. } => preview_status("loading…", &settings.theme),
            CodePreviewState::Preparing { .. } => preview_status("highlighting…", &settings.theme),
            CodePreviewState::Error(reason) => preview_status(reason, &settings.theme),
            CodePreviewState::Ready(document) => div()
                .id("preview-code-viewport")
                .flex_1()
                .min_w_0()
                .min_h_0()
                .overflow_hidden()
                .bg(settings.theme.color(ThemeRole::MediaViewport))
                .child(render_code_document(
                    document,
                    preview.scroll_handle.clone(),
                    preview.view_state.clone(),
                    preview.scrollbar_state.clone(),
                    self.code_selection.clone(),
                    active_match,
                    Some(settings.clone()),
                ))
                .into_any_element(),
        };

        div()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .when(self.code_search_open && ready, |column| {
                column.child(
                    div()
                        .min_h(rems_from_px(PREVIEW_SEARCH_BAR_HEIGHT))
                        .flex_none()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap(rems_from_px(if compact_search { 4.0 } else { 8.0 }))
                        .px(rems_from_px(if compact_search { 4.0 } else { 8.0 }))
                        .border_b_1()
                        .border_color(settings.theme.color(ThemeRole::BorderSubtle))
                        .bg(settings.theme.color(ThemeRole::Window))
                        .child(
                            div()
                                .min_h(rems_from_px(30.0))
                                .min_w_0()
                                .flex_1()
                                .flex()
                                .items_center()
                                .px_2()
                                .border_1()
                                .border_color(settings.theme.color(ThemeRole::BorderStrong))
                                .bg(settings.theme.color(ThemeRole::Input))
                                .text_sm()
                                .child(self.code_search_input.clone()),
                        )
                        .child(
                            div()
                                .w(rems_from_px(if compact_search {
                                    48.0
                                } else if active_match_hidden {
                                    112.0
                                } else {
                                    70.0
                                }))
                                .flex_none()
                                .text_right()
                                .text_xs()
                                .text_color(settings.theme.color(ThemeRole::TextMuted))
                                .child(search_status),
                        )
                        .child(
                            preview_control_button("preview-find-previous", "↑", &settings.theme)
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.previous_code_match(cx)),
                                ),
                        )
                        .child(
                            preview_control_button("preview-find-next", "↓", &settings.theme)
                                .on_click(cx.listener(|this, _, _, cx| this.next_code_match(cx))),
                        )
                        .child(
                            preview_control_button("preview-find-close", "×", &settings.theme)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.close_code_search(cx);
                                    window.focus(&this.code_viewer_focus, cx);
                                })),
                        ),
                )
            })
            .child(body)
            .into_any_element()
    }
}

fn preview_tab_shell(selected: bool, palette: &ThemePalette) -> Div {
    let hover = palette.color(ThemeRole::Raised);
    let hover_text = palette.color(ThemeRole::TextPrimary);
    div()
        .h_full()
        .max_w(rems_from_px(210.0))
        .flex_none()
        .flex()
        .items_center()
        .border_r_1()
        .border_color(palette.color(ThemeRole::BorderSubtle))
        .bg(palette.color(if selected {
            ThemeRole::Window
        } else {
            ThemeRole::Panel
        }))
        .text_color(palette.color(if selected {
            ThemeRole::MediaProgressKnob
        } else {
            ThemeRole::TextMuted
        }))
        .hover(move |tab| tab.bg(hover).text_color(hover_text))
}
