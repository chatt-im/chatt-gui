use super::*;

impl ChattView {
    fn message_reference_literal(target: MessageRef) -> String {
        format!("{REF_PREFIX}{}", target.encode())
    }

    pub(super) fn quote_message_reference(
        &mut self,
        target: MessageRef,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let reference = Self::message_reference_literal(target);
        self.composer.update(cx, |composer, cx| {
            composer.insert_message_reference(&reference, window, cx)
        });
        window.focus(&self.composer.focus_handle(cx), cx);
        self.status = "Reference inserted".into();
        cx.notify();
    }

    pub(super) fn copy_message_reference(&mut self, target: MessageRef, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(Self::message_reference_literal(
            target,
        )));
        self.status = "Message reference copied".into();
        cx.notify();
    }

    pub(super) fn hover_message_reference(
        &mut self,
        hovered: Option<(MessageRef, Bounds<Pixels>)>,
        cx: &mut Context<Self>,
    ) {
        self.message_reference_hover_task = None;
        let Some((target, anchor)) = hovered else {
            self.message_reference_hover = None;
            cx.notify();
            return;
        };
        if let Some(state) = self.message_reference_hover.as_mut()
            && state.target == target
        {
            state.anchor = anchor;
            return;
        }
        self.message_reference_hover = Some(MessageReferenceHover {
            target,
            anchor,
            visible: false,
            message: None,
            formatted: None,
            missing: false,
            request_id: None,
        });
        let executor = cx.background_executor().clone();
        self.message_reference_hover_task = Some(cx.spawn(async move |this, cx| {
            executor.timer(REFERENCE_HOVER_DELAY).await;
            let _ = this.update(cx, |this, cx| {
                if this
                    .message_reference_hover
                    .as_ref()
                    .is_some_and(|hover| hover.target == target)
                {
                    this.begin_message_reference_preview(target, cx);
                }
            });
        }));
    }

    fn begin_message_reference_preview(&mut self, target: MessageRef, cx: &mut Context<Self>) {
        let loaded = (self.model.selected_room == Some(target.room_id))
            .then(|| {
                self.model
                    .messages
                    .binary_search_by_key(&target.message_id.0, |message| message.id)
                    .ok()
                    .map(|index| self.model.messages[index].clone())
            })
            .flatten();
        if let Some(message) = loaded {
            self.install_message_reference_preview(target, Some(message), None, cx);
            return;
        }
        if let Some(cached) = self.message_reference_cache.get(&target).cloned() {
            self.install_message_reference_preview(target, cached, None, cx);
            return;
        }
        let Some(room_generation) = self.model.room_generation else {
            return;
        };
        let request_id = self.request_id();
        let Some(hover) = self
            .message_reference_hover
            .as_mut()
            .filter(|hover| hover.target == target)
        else {
            return;
        };
        hover.visible = true;
        hover.request_id = Some(request_id);
        if let Err(error) = self.daemon.send(ClientFrame::ResolveMessageReference {
            request_id,
            room_id: target.room_id,
            room_generation,
            message_id: target.message_id,
        }) {
            hover.request_id = None;
            hover.missing = true;
            kvlog::error!("could not resolve message reference", err = %error);
        }
        cx.notify();
    }

    pub(super) fn install_message_reference_preview(
        &mut self,
        target: MessageRef,
        message: Option<timeline::Message>,
        request_id: Option<RequestId>,
        cx: &mut Context<Self>,
    ) {
        let Some(hover) = self
            .message_reference_hover
            .as_mut()
            .filter(|hover| hover.target == target)
        else {
            return;
        };
        if request_id.is_some() && hover.request_id != request_id {
            return;
        }
        let formatted = message.as_ref().map(|message| {
            Rc::new(FormattedMessage::from_prepared(FormattedMessage::prepare(
                message.body.clone(),
            )))
        });
        hover.visible = true;
        hover.message = message;
        hover.formatted = formatted;
        hover.missing = hover.message.is_none();
        hover.request_id = None;
        cx.notify();
    }

    pub(super) fn activate_message_reference(
        &mut self,
        target: MessageRef,
        shift: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let hover_request = self
            .message_reference_hover
            .as_ref()
            .filter(|hover| hover.target == target)
            .and_then(|hover| hover.request_id);
        self.message_reference_hover = None;
        self.message_reference_hover_task = None;
        self.pending_message_reference_click = None;
        self.pending_reference_media_preview = None;
        if shift {
            self.jump_to_message_reference(target, cx);
            return;
        }

        let loaded = (self.model.selected_room == Some(target.room_id))
            .then(|| {
                self.model
                    .messages
                    .binary_search_by_key(&target.message_id.0, |message| message.id)
                    .ok()
                    .map(|index| self.model.messages[index].clone())
            })
            .flatten();
        if let Some(message) = loaded {
            self.open_message_reference_target(target, message, window, cx);
            return;
        }
        if let Some(cached) = self.message_reference_cache.get(&target).cloned() {
            if let Some(message) = cached {
                self.open_message_reference_target(target, message, window, cx);
            } else {
                self.jump_to_message_reference(target, cx);
            }
            return;
        }

        let request_id = hover_request.unwrap_or_else(|| self.request_id());
        let Some(room_generation) = self.model.room_generation else {
            self.status = "Current room history is not ready".into();
            cx.notify();
            return;
        };
        self.pending_message_reference_click =
            Some(PendingMessageReferenceClick { target, request_id });
        if hover_request.is_none()
            && let Err(error) = self.daemon.send(ClientFrame::ResolveMessageReference {
                request_id,
                room_id: target.room_id,
                room_generation,
                message_id: target.message_id,
            })
        {
            self.pending_message_reference_click = None;
            self.status = error.into();
        } else {
            self.status = "Opening referenced attachment…".into();
        }
        cx.notify();
    }

    pub(super) fn open_message_reference_target(
        &mut self,
        target: MessageRef,
        message: timeline::Message,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(attachment) = message.attachment else {
            self.jump_to_message_reference(target, cx);
            return;
        };
        self.open_message_reference_attachment(target, attachment, window, cx);
    }

    pub(super) fn open_message_reference_attachment(
        &mut self,
        target: MessageRef,
        attachment: Attachment,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let descriptor = attachment.descriptor.clone();
        let render_kind = attachment.render_kind();
        if render_kind == AttachmentRenderKind::Image {
            let cached = self
                .media_cache
                .lock()
                .expect("media cache lock poisoned")
                .contains(descriptor.id);
            if cached {
                self.pending_reference_media_preview = None;
                self.open_image_preview(descriptor, window, cx);
            } else {
                self.pending_reference_media_preview = Some(PendingReferenceMediaPreview {
                    target,
                    attachment: attachment.clone(),
                });
                let fetch = EagerImageFetch::new(target.room_id, descriptor.clone());
                if self.eager_image_fetches.failure(fetch.key).is_some() {
                    self.retry_eager_image(fetch, window, cx);
                } else {
                    self.enqueue_eager_image(fetch, window, cx);
                }
                self.status = format!("Loading {}…", descriptor.file_name).into();
                cx.notify();
            }
            return;
        }
        if render_kind == AttachmentRenderKind::Audio {
            self.pending_reference_media_preview = None;
            self.jump_to_message_reference(target, cx);
            return;
        }
        if descriptor.media_kind == MediaKind::File && render_kind == AttachmentRenderKind::Other {
            self.pending_reference_media_preview = None;
            self.open_code_preview(target.room_id, descriptor, window, cx);
            return;
        }
        if render_kind == AttachmentRenderKind::Video {
            self.pending_reference_media_preview = None;
            let key = video_key(target.room_id, target.message_id.0, &descriptor);
            let source_key = self.source_key(target.room_id, descriptor.id);
            let source = match self.video_sources.view(source_key) {
                VideoSourceView::Ready(source) => Some(source),
                VideoSourceView::Absent
                | VideoSourceView::Loading
                | VideoSourceView::Failed { .. } => None,
            };
            self.video_sources.promote(source_key, descriptor.clone());
            self.toggle_video_theater(
                TheaterVideo {
                    key,
                    descriptor,
                    source,
                },
                window,
                cx,
            );
            self.pump_video_sources(cx);
            return;
        }

        self.pending_reference_media_preview = None;
        self.status = format!("{} cannot be previewed yet", descriptor.file_name).into();
        cx.notify();
    }

    pub(super) fn jump_to_message_reference(&mut self, target: MessageRef, cx: &mut Context<Self>) {
        self.message_reference_hover = None;
        self.message_reference_hover_task = None;
        if !self.model.is_ready()
            || !self
                .model
                .rooms
                .iter()
                .any(|room| room.id == target.room_id)
        {
            self.status = "Referenced room is not available".into();
            cx.notify();
            return;
        }
        self.pending_message_jump = Some(PendingMessageJump {
            target,
            pages_requested: 0,
            page_request_id: None,
            room_request_id: None,
        });
        if self.model.selected_room != Some(target.room_id) {
            let request_id = self.request_id();
            self.model.pending.insert(
                request_id,
                PendingRequest {
                    operation: Operation::SelectRoom,
                    room_id: Some(target.room_id),
                    draft: None,
                    transfer_id: None,
                },
            );
            if let Some(jump) = self.pending_message_jump.as_mut() {
                jump.room_request_id = Some(request_id);
            }
            if let Err(error) = self.daemon.send(ClientFrame::SelectRoom {
                request_id,
                room_id: target.room_id,
            }) {
                self.model.pending.remove(&request_id);
                self.pending_message_jump = None;
                self.status = error.into();
            } else {
                self.status = "Opening referenced room…".into();
            }
            cx.notify();
            return;
        }
        self.resume_message_reference_jump(cx);
    }

    fn flash_message_reference(&mut self, target: MessageRef, cx: &mut Context<Self>) {
        let flash_id = self.next_message_reference_flash_id;
        self.next_message_reference_flash_id =
            self.next_message_reference_flash_id.wrapping_add(1).max(1);
        self.message_reference_flash = Some((target, flash_id));
        self.message_reference_flash_task = None;
        let executor = cx.background_executor().clone();
        self.message_reference_flash_task = Some(cx.spawn(async move |this, cx| {
            executor.timer(Duration::from_millis(1_600)).await;
            let _ = this.update(cx, |this, cx| {
                if this
                    .message_reference_flash
                    .is_some_and(|(_, active_id)| active_id == flash_id)
                {
                    this.message_reference_flash = None;
                    this.message_reference_flash_task = None;
                    cx.notify();
                }
            });
        }));
    }

    pub(super) fn resume_message_reference_jump(&mut self, cx: &mut Context<Self>) {
        let Some(target) = self.pending_message_jump.as_ref().map(|jump| jump.target) else {
            return;
        };
        if self.model.selected_room != Some(target.room_id) {
            return;
        }
        if self
            .pending_message_jump
            .as_ref()
            .is_some_and(|jump| jump.page_request_id.is_some())
        {
            return;
        }
        let pages_requested = self
            .pending_message_jump
            .as_ref()
            .map_or(0, |jump| jump.pages_requested);
        let before = match message_reference_jump_decision(
            &self.model.messages,
            target.message_id,
            self.model.older_cursor,
            self.model.at_start,
            pages_requested,
        ) {
            MessageReferenceJumpDecision::Found => {
                if timeline::reveal_message(
                    &self.model.messages,
                    &mut self.collapsed_sections,
                    target.message_id.0,
                ) {
                    self.rebuild_message_list();
                }
                if let Some(index) = self
                    .message_list
                    .iter()
                    .position(|item| item.message_id() == Some(target.message_id.0))
                {
                    self.pending_scroll = px(0.);
                    self.scroll_animation_active = false;
                    self.last_scroll_frame = None;
                    scroll_message_reference_to_start(&self.list_state, index);
                    self.flash_message_reference(target, cx);
                    self.pending_message_jump = None;
                    self.status = "Jumped to referenced message".into();
                    cx.notify();
                }
                return;
            }
            MessageReferenceJumpDecision::Unavailable => {
                self.pending_message_jump = None;
                self.status = "Referenced message is not available".into();
                cx.notify();
                return;
            }
            MessageReferenceJumpDecision::SearchWindowExhausted => {
                self.pending_message_jump = None;
                self.status = "Referenced message is outside the current jump search window".into();
                cx.notify();
                return;
            }
            MessageReferenceJumpDecision::LoadOlder(before) => before,
        };
        let Some(room_generation) = self.model.room_generation else {
            return;
        };

        let request_id = self.request_id();
        self.model.pending.insert(
            request_id,
            PendingRequest {
                operation: Operation::LoadOlder,
                room_id: Some(target.room_id),
                draft: None,
                transfer_id: None,
            },
        );
        let jump = self
            .pending_message_jump
            .as_mut()
            .expect("reference jump remains pending");
        jump.pages_requested += 1;
        jump.page_request_id = Some(request_id);
        if let Err(error) = self.daemon.send(ClientFrame::LoadOlder {
            request_id,
            room_id: target.room_id,
            room_generation,
            before: Some(before),
            limit: REFERENCE_JUMP_PAGE_SIZE,
        }) {
            self.model.pending.remove(&request_id);
            self.pending_message_jump = None;
            self.status = error.into();
        } else {
            self.status = format!(
                "Finding referenced message… ({}/{REFERENCE_JUMP_PAGE_LIMIT})",
                jump.pages_requested
            )
            .into();
        }
        cx.notify();
    }

    pub(super) fn render_message_reference_preview(
        &mut self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let hover = self
            .message_reference_hover
            .as_ref()
            .filter(|hover| hover.visible)?;
        let target = hover.target;
        let anchor = hover.anchor;
        let message = hover.message.clone();
        let formatted = hover.formatted.clone();
        let missing = hover.missing;
        let applied = AppliedSettings::get(cx);
        let room_name = self
            .model
            .rooms
            .iter()
            .find(|room| room.id == target.room_id)
            .map(|room| room.name.clone())
            .unwrap_or_else(|| "Unknown room".into());
        let attachment = message
            .as_ref()
            .and_then(|message| message.attachment.as_ref())
            .map(|attachment| {
                self.render_message_reference_attachment(target, attachment.clone(), window, cx)
            });
        let card = div()
            .w(rems_from_px(400.))
            .max_w(rems_from_px(400.))
            .max_h(rems_from_px(480.))
            .overflow_hidden()
            .p_3()
            .border_1()
            .border_color(applied.theme.color(ThemeRole::BorderStrong))
            .bg(applied.theme.color(ThemeRole::Raised))
            .shadow_lg()
            .text_color(applied.theme.color(ThemeRole::TextBody))
            .child(
                div()
                    .mb_2()
                    .flex()
                    .items_center()
                    .gap_2()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .when_some(message.as_ref(), |header, message| {
                        header.child(message.sender.clone())
                    })
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::NORMAL)
                            .text_color(applied.theme.color(ThemeRole::TextDim))
                            .child(format!("in {room_name}")),
                    ),
            )
            .when_some(
                message.as_ref().zip(formatted.as_ref()),
                |card, (message, formatted)| {
                    let _ = message;
                    card.child(FormattedMessageElement::new(formatted.clone()))
                },
            )
            .when_some(attachment, |card, attachment| card.child(attachment))
            .when(message.is_none(), |card| {
                card.child(
                    div()
                        .text_sm()
                        .text_color(applied.theme.color(ThemeRole::TextMuted))
                        .child(if missing {
                            "Referenced message is not available"
                        } else {
                            "Loading referenced message…"
                        }),
                )
            });
        let position = point(
            anchor.origin.x,
            anchor.origin.y - crate::ui_scale::scaled_px(6.0, window.rem_size()),
        );
        Some(
            deferred(
                anchored()
                    .anchor(Anchor::BottomLeft)
                    .position(position)
                    .snap_to_window_with_margin(crate::ui_scale::scaled_px(8.0, window.rem_size()))
                    .child(card),
            )
            .into_any_element(),
        )
    }

    fn render_message_reference_attachment(
        &mut self,
        target: MessageRef,
        attachment: Attachment,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let is_image = attachment.is_image();
        let descriptor = attachment.descriptor;
        let settings = AppliedSettings::get(cx);
        if is_image {
            let (cached_attachment, active_transfer) = {
                let mut cache = self.media_cache.lock().expect("media cache lock poisoned");
                (cache.get(descriptor.id), cache.active_transfer(&descriptor))
            };
            if let Some(attachment) = cached_attachment {
                let source_width = descriptor.width.unwrap_or(4).max(1) as f32;
                let source_height = descriptor.height.unwrap_or(3).max(1) as f32;
                let aspect_ratio = source_width / source_height;
                let width = 368.0_f32.min(220.0 * aspect_ratio);
                let height = width / aspect_ratio;
                return div()
                    .id(("reference-preview-image", target.message_id.0 as usize))
                    .mt_2()
                    .w(rems_from_px(width))
                    .max_w_full()
                    .h(rems_from_px(height))
                    .overflow_hidden()
                    .bg(settings.theme.color(ThemeRole::Panel))
                    .child(
                        img(cached_attachment_image_source(
                            attachment,
                            self.image_cache.clone(),
                        ))
                        .absolute()
                        .inset_0()
                        .size_full()
                        .object_fit(ObjectFit::Contain),
                    )
                    .into_any_element();
            }
            let fetch = EagerImageFetch::new(target.room_id, descriptor.clone());
            let status = if active_transfer.is_some() {
                format!("Loading {}…", descriptor.file_name)
            } else if let Some(reason) = self.eager_image_fetches.failure(fetch.key) {
                format!("Could not load {} · {reason}", descriptor.file_name)
            } else {
                self.enqueue_eager_image(fetch, window, cx);
                format!("Loading {}…", descriptor.file_name)
            };
            return div()
                .mt_2()
                .min_h(rems_from_px(96.))
                .w_full()
                .px_2()
                .flex()
                .items_center()
                .justify_center()
                .bg(settings.theme.color(ThemeRole::Panel))
                .text_sm()
                .text_color(settings.theme.color(ThemeRole::TextMuted))
                .child(status)
                .into_any_element();
        }

        div()
            .mt_2()
            .px_2()
            .py_1()
            .bg(settings.theme.color(ThemeRole::ControlSurface))
            .text_sm()
            .text_color(settings.theme.color(ThemeRole::TextSecondary))
            .child(format!(
                "{} · {} bytes",
                descriptor.file_name, descriptor.byte_len
            ))
            .into_any_element()
    }
}
