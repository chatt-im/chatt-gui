use super::*;

impl ChattView {
    pub(super) fn render_attachment_video(
        &mut self,
        key: VideoKey,
        descriptor: AttachmentDescriptor,
        registered_source: Option<RegisteredAttachmentSource>,
        theater: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let video = self.videos.view(key);
        let thumbnail_key = ThumbnailKey {
            source_key: self.source_key(key.room_id, descriptor.id),
        };
        let thumbnail = match registered_source.as_ref() {
            Some(source) if !self.video_sources.has_playback_pin(source.source().key()) => {
                let thumbnail = self.video_thumbnails.request(thumbnail_key, source.clone());
                if thumbnail.pending {
                    self.video_sources.set_pin(
                        source.source().key(),
                        VideoSourcePin::Thumbnail,
                        true,
                    );
                }
                thumbnail
            }
            _ => self.video_thumbnails.view(thumbnail_key),
        };
        let duration = if video.duration > 0.0 {
            video.duration
        } else {
            thumbnail.duration.unwrap_or(0.0)
        };
        let active_scrub = self.video_scrub.filter(|scrub| scrub.key == key);
        let display_position = active_scrub.map_or(video.position, VideoScrub::position);
        let active_controls = self.video_controls.active_key == Some(key);
        let controls_phase = active_controls
            .then_some(self.video_controls.phase)
            .unwrap_or_default();
        let controls_pinned = video.paused
            || video.finished
            || active_scrub.is_some()
            || self.video_volume_drag.is_some_and(|drag| drag.key == key)
            || (active_controls
                && (self.video_controls.bar_hovered || self.video_controls.volume_open));
        let scrub_hover_fraction = active_controls
            .then(|| {
                active_scrub
                    .map(|scrub| scrub.last_fraction)
                    .or(self.video_controls.scrub_hover_fraction)
            })
            .flatten();
        let source = TheaterVideo {
            key,
            descriptor: descriptor.clone(),
            source: registered_source,
        };
        let view = cx.entity().downgrade();
        let event_source = source.clone();
        let handler: VideoPlayerHandler = Rc::new(move |event, _, cx| {
            let source = event_source.clone();
            let _ = view.update(cx, |this, cx| {
                this.handle_video_player_event(source, duration, event, cx)
            });
        });
        let fallback_label = video.error.clone().unwrap_or_else(|| {
            if source.source.is_none() && thumbnail.image.is_none() {
                "Loading preview…".into()
            } else {
                descriptor.file_name.clone()
            }
        });
        let aspect_ratio = aspect_ratio(&video, (descriptor.width, descriptor.height));
        let effect_overlay = self
            .video_effect_overlay
            .filter(|overlay| overlay.key == key)
            .map(|overlay| VideoEffectDisplay {
                label: overlay.effect.label(),
                value: overlay.value,
            });
        render_video_player(
            VideoPlayerConfig {
                key,
                theater,
                video,
                thumbnail,
                duration,
                display_position,
                aspect_ratio,
                fallback_label,
                controls_phase,
                controls_pinned,
                scrub_hover_fraction,
                effect_overlay,
                volume_open: active_controls && self.video_controls.volume_open,
                measure_volume_bounds: active_controls,
            },
            handler,
            self.video_volume_popup_bounds.clone(),
            self.video_volume_button_bounds.clone(),
            AppliedSettings::get(cx),
        )
    }

    fn handle_video_player_event(
        &mut self,
        source: TheaterVideo,
        duration: f64,
        event: VideoPlayerEvent,
        cx: &mut Context<Self>,
    ) {
        let key = source.key;
        match event {
            VideoPlayerEvent::PlayerHovered(hovered) => self.hover_video_player(key, hovered, cx),
            VideoPlayerEvent::PointerMoved => self.video_pointer_moved(key, cx),
            VideoPlayerEvent::SurfaceClicked {
                click_count,
                unstarted,
            } => self.click_video_surface(source, click_count, unstarted, cx),
            VideoPlayerEvent::Play => {
                self.video_surface_click_task.take();
                self.play_video(key, cx);
            }
            VideoPlayerEvent::ScrubHovered(fraction) => self.hover_video_scrub(key, fraction, cx),
            VideoPlayerEvent::ScrubHoverCleared => self.clear_video_scrub_hover(key, cx),
            VideoPlayerEvent::ScrubPressed { bounds, event } => {
                self.begin_video_scrub(key, duration, bounds, &event, cx)
            }
            VideoPlayerEvent::ControlsHovered(hovered) => {
                self.hover_video_controls(key, hovered, cx)
            }
            VideoPlayerEvent::VolumeHovered(hovered) => self.hover_video_volume(key, hovered, cx),
            VideoPlayerEvent::VolumePopupHovered(hovered) => {
                self.hover_video_volume_popup(key, hovered, cx)
            }
            VideoPlayerEvent::ToggleMute => self.toggle_video_mute(key, cx),
            VideoPlayerEvent::VolumePressed { bounds, event } => {
                self.begin_video_volume_drag(key, bounds, &event, cx)
            }
            VideoPlayerEvent::ToggleTheater => self.toggle_video_theater(source, cx),
        }
    }

    pub(super) fn play_video(&mut self, key: VideoKey, cx: &mut Context<Self>) {
        self.note_media_interaction(MediaPlaybackTarget::Video(key));
        self.show_video_controls(key, cx);
        let source_key = self.source_key(key.room_id, key.attachment_id);
        match self.video_sources.view(source_key) {
            VideoSourceView::Ready(source) => {
                self.pending_video_plays.remove(&key);
                self.videos.ensure_source(key, source);
                match self.videos.play(key) {
                    Ok(()) => self.status = "Starting attachment playback…".into(),
                    Err(error) => {
                        kvlog::error!(
                            "embedded video play failed",
                            room_id = key.room_id,
                            message_id = key.message_id,
                            attachment_timestamp_ms = key.attachment_id.timestamp_ms,
                            attachment_transfer_id = key.attachment_id.transfer_id,
                            err = %error
                        );
                        self.status = format!("Playback failed: {error}").into();
                    }
                }
            }
            VideoSourceView::Absent | VideoSourceView::Loading | VideoSourceView::Failed { .. } => {
                if let Some(descriptor) = self.video_descriptor(key) {
                    self.pending_video_plays.insert(key);
                    self.video_sources.promote(source_key, descriptor);
                    self.video_sources
                        .set_pin(source_key, VideoSourcePin::PendingPlay, true);
                    self.video_sources.retry(source_key);
                    self.pump_video_sources(cx);
                    self.status = "Preparing attachment playback…".into();
                } else {
                    self.status = "Video source is no longer available".into();
                }
            }
        }
        self.sync_video_source_pins();
        self.schedule_video_controls_hide(key, cx);
        cx.notify();
    }

    pub(super) fn seek_video(&mut self, key: VideoKey, seconds: f64, cx: &mut Context<Self>) {
        self.note_media_interaction(MediaPlaybackTarget::Video(key));
        if let Err(error) = self.videos.seek(key, seconds) {
            kvlog::error!(
                "embedded video seek failed",
                room_id = key.room_id,
                message_id = key.message_id,
                attachment_timestamp_ms = key.attachment_id.timestamp_ms,
                attachment_transfer_id = key.attachment_id.transfer_id,
                seconds,
                err = %error
            );
            self.status = format!("Seek failed: {error}").into();
        }
        self.show_video_controls(key, cx);
        self.schedule_video_controls_hide(key, cx);
        cx.notify();
    }

    pub(super) fn decrease_video_contrast(
        &mut self,
        _: &DecreaseContrast,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.adjust_active_video(VideoAdjustment::Contrast(-1.0), cx);
    }

    pub(super) fn increase_video_contrast(
        &mut self,
        _: &IncreaseContrast,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.adjust_active_video(VideoAdjustment::Contrast(1.0), cx);
    }

    pub(super) fn decrease_video_brightness(
        &mut self,
        _: &DecreaseBrightness,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.adjust_active_video(VideoAdjustment::Brightness(-1.0), cx);
    }

    pub(super) fn increase_video_brightness(
        &mut self,
        _: &IncreaseBrightness,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.adjust_active_video(VideoAdjustment::Brightness(1.0), cx);
    }

    pub(super) fn decrease_video_gamma(
        &mut self,
        _: &DecreaseGamma,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.adjust_active_video(VideoAdjustment::Gamma(-1.0), cx);
    }

    pub(super) fn increase_video_gamma(
        &mut self,
        _: &IncreaseGamma,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.adjust_active_video(VideoAdjustment::Gamma(1.0), cx);
    }

    pub(super) fn decrease_video_saturation(
        &mut self,
        _: &DecreaseSaturation,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.adjust_active_video(VideoAdjustment::Saturation(-1.0), cx);
    }

    pub(super) fn increase_video_saturation(
        &mut self,
        _: &IncreaseSaturation,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.adjust_active_video(VideoAdjustment::Saturation(1.0), cx);
    }

    pub(super) fn decrease_video_volume(
        &mut self,
        _: &DecreaseVolume,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(key) = self.active_video_target() {
            self.adjust_video_volume(key, -2.0, cx);
        }
    }

    pub(super) fn increase_video_volume(
        &mut self,
        _: &IncreaseVolume,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(key) = self.active_video_target() {
            self.adjust_video_volume(key, 2.0, cx);
        }
    }

    pub(super) fn decrease_video_playback_speed(
        &mut self,
        _: &DecreasePlaybackSpeed,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.adjust_active_video(VideoAdjustment::PlaybackSpeed(1.0 / 1.1), cx);
    }

    pub(super) fn increase_video_playback_speed(
        &mut self,
        _: &IncreasePlaybackSpeed,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.adjust_active_video(VideoAdjustment::PlaybackSpeed(1.1), cx);
    }

    pub(super) fn previous_video_frame(
        &mut self,
        _: &PreviousFrame,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(key) = self.active_video_target() else {
            return;
        };
        self.note_media_interaction(MediaPlaybackTarget::Video(key));
        match self.videos.step_frame(key, true) {
            Ok(true) => {
                self.show_video_controls(key, cx);
                cx.notify();
            }
            Ok(false) => {}
            Err(error) => self.report_video_keyboard_control_error(key, error, cx),
        }
    }

    pub(super) fn next_video_frame(
        &mut self,
        _: &NextFrame,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(key) = self.active_video_target() else {
            return;
        };
        let repeated = matches!(
            self.next_frame_hold,
            Some(NextFrameHold::Active { target, .. }) if target == key
        );
        if !repeated {
            self.finish_active_frame_hold();
        }
        self.note_media_interaction(MediaPlaybackTarget::Video(key));
        let result = if repeated {
            self.videos.set_frame_hold_playing(key, true)
        } else {
            self.videos.step_frame(key, false)
        };
        match result {
            Ok(true) => {
                if !repeated {
                    self.next_frame_hold = Some(NextFrameHold::AwaitingKey { target: key });
                }
                self.show_video_controls(key, cx);
                // Let the low-level key-down listener record the physical key
                // that dispatched this configurable action.
                cx.propagate();
                cx.notify();
            }
            Ok(false) => {}
            Err(error) => self.report_video_keyboard_control_error(key, error, cx),
        }
    }

    pub(super) fn capture_next_frame_key_down(
        &mut self,
        event: &KeyDownEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.next_frame_hold.take() {
            Some(NextFrameHold::AwaitingKey { target })
            | Some(NextFrameHold::Active { target, .. }) => {
                self.next_frame_hold = Some(NextFrameHold::Active {
                    target,
                    key: event.keystroke.key.clone(),
                });
                cx.stop_propagation();
            }
            None => {}
        }
    }

    pub(super) fn release_next_frame_key(
        &mut self,
        event: &KeyUpEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(NextFrameHold::Active { target, key }) = self.next_frame_hold.as_ref() else {
            return;
        };
        if event.keystroke.key != *key {
            return;
        }
        let target = *target;
        self.next_frame_hold = None;
        if let Err(error) = self.videos.set_frame_hold_playing(target, false) {
            self.report_video_keyboard_control_error(target, error, cx);
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn finish_active_frame_hold(&mut self) {
        let Some(NextFrameHold::Active { target, .. }) = self.next_frame_hold.take() else {
            self.next_frame_hold = None;
            return;
        };
        if let Err(error) = self.videos.set_frame_hold_playing(target, false) {
            kvlog::error!(
                "embedded video frame hold release failed",
                room_id = target.room_id,
                message_id = target.message_id,
                attachment_timestamp_ms = target.attachment_id.timestamp_ms,
                attachment_transfer_id = target.attachment_id.transfer_id,
                err = %error
            );
        }
    }

    fn adjust_active_video(&mut self, adjustment: VideoAdjustment, cx: &mut Context<Self>) {
        let Some(key) = self.active_video_target() else {
            return;
        };
        self.note_media_interaction(MediaPlaybackTarget::Video(key));
        match self.videos.adjust_video(key, adjustment) {
            Ok(Some(change)) => self.show_video_effect_overlay(key, change, cx),
            Ok(None) => cx.notify(),
            Err(error) => self.report_video_keyboard_control_error(key, error, cx),
        }
    }

    fn show_video_effect_overlay(
        &mut self,
        key: VideoKey,
        change: VideoEffectChange,
        cx: &mut Context<Self>,
    ) {
        self.video_effect_overlay_hide_task.take();
        let serial = self.next_video_effect_overlay_serial;
        self.next_video_effect_overlay_serial =
            self.next_video_effect_overlay_serial.wrapping_add(1).max(1);
        self.video_effect_overlay = Some(VideoEffectOverlay {
            key,
            effect: change.effect,
            value: change.value,
            serial,
        });
        self.video_effect_overlay_hide_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(VIDEO_EFFECT_OVERLAY_HOLD)
                .await;
            let _ = this.update(cx, |this, cx| {
                this.video_effect_overlay_hide_task.take();
                if this
                    .video_effect_overlay
                    .is_some_and(|overlay| overlay.serial == serial)
                {
                    this.video_effect_overlay = None;
                    cx.notify();
                }
            });
        }));
        cx.notify();
    }

    fn report_video_keyboard_control_error(
        &mut self,
        key: VideoKey,
        error: anyhow::Error,
        cx: &mut Context<Self>,
    ) {
        kvlog::error!(
            "embedded video keyboard control failed",
            room_id = key.room_id,
            message_id = key.message_id,
            attachment_timestamp_ms = key.attachment_id.timestamp_ms,
            attachment_transfer_id = key.attachment_id.transfer_id,
            err = %error
        );
        self.status = format!("Video control failed: {error}").into();
        cx.notify();
    }

    fn show_video_controls(&mut self, key: VideoKey, cx: &mut Context<Self>) {
        if self.video_controls.active_key != Some(key) {
            self.video_controls_hide_task.take();
            self.video_volume_hide_task.take();
            self.video_volume_popup_bounds.set(None);
            self.video_volume_button_bounds.set(None);
        }
        let Some(serial) = self.video_controls.show(key) else {
            return;
        };
        self.video_controls_animation_task.take();
        self.video_controls_animation_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(CONTROLS_ANIMATION_DURATION)
                .await;
            let _ = this.update(cx, |this, cx| {
                this.video_controls_animation_task.take();
                if this.video_controls.finish_animation(serial) {
                    cx.notify();
                }
            });
        }));
        cx.notify();
    }

    fn hide_video_controls(&mut self, key: VideoKey, cx: &mut Context<Self>) {
        let Some(serial) = self.video_controls.hide(key) else {
            return;
        };
        self.video_controls_hide_task.take();
        self.video_volume_hide_task.take();
        self.video_volume_popup_bounds.set(None);
        self.video_controls_animation_task.take();
        self.video_controls_animation_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(CONTROLS_ANIMATION_DURATION)
                .await;
            let _ = this.update(cx, |this, cx| {
                this.video_controls_animation_task.take();
                if this.video_controls.finish_animation(serial) {
                    cx.notify();
                }
            });
        }));
        cx.notify();
    }

    fn video_controls_pinned(&self, key: VideoKey) -> bool {
        let video = self.videos.view(key);
        let dragging = self.video_scrub.is_some_and(|scrub| scrub.key == key)
            || self.video_volume_drag.is_some_and(|drag| drag.key == key);
        self.video_controls
            .pinned(key, video.paused, video.finished, dragging)
    }

    fn schedule_video_controls_hide(&mut self, key: VideoKey, cx: &mut Context<Self>) {
        self.video_controls_hide_task.take();
        if self.video_controls.active_key != Some(key) || self.video_controls_pinned(key) {
            return;
        }
        if !self.video_controls.player_hovered {
            self.hide_video_controls(key, cx);
            return;
        }
        self.video_controls_hide_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(CONTROLS_HIDE_DELAY).await;
            let _ = this.update(cx, |this, cx| {
                this.video_controls_hide_task.take();
                if this.video_controls.active_key == Some(key) && !this.video_controls_pinned(key) {
                    this.hide_video_controls(key, cx);
                }
            });
        }));
    }

    fn hover_video_player(&mut self, key: VideoKey, hovered: bool, cx: &mut Context<Self>) {
        self.show_video_controls(key, cx);
        self.video_controls.player_hovered = hovered;
        if hovered {
            self.schedule_video_controls_hide(key, cx);
        } else {
            self.video_controls.scrub_hover_fraction = None;
            self.schedule_video_controls_hide(key, cx);
        }
        cx.notify();
    }

    fn video_pointer_moved(&mut self, key: VideoKey, cx: &mut Context<Self>) {
        self.show_video_controls(key, cx);
        self.video_controls.player_hovered = true;
        self.schedule_video_controls_hide(key, cx);
    }

    fn hover_video_controls(&mut self, key: VideoKey, hovered: bool, cx: &mut Context<Self>) {
        self.show_video_controls(key, cx);
        self.video_controls.bar_hovered = hovered;
        if hovered {
            self.video_controls_hide_task.take();
        } else {
            self.schedule_video_controls_hide(key, cx);
        }
        cx.notify();
    }

    fn click_video_surface(
        &mut self,
        source: TheaterVideo,
        click_count: usize,
        unstarted: bool,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        if click_count >= 2 {
            self.video_surface_click_task.take();
            self.toggle_video_theater(source, cx);
            return;
        }
        if click_count != 1 {
            return;
        }
        if unstarted {
            self.video_surface_click_task.take();
            self.play_video(source.key, cx);
            return;
        }
        self.video_surface_click_task.take();
        let key = source.key;
        self.video_surface_click_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(220))
                .await;
            let _ = this.update(cx, |this, cx| {
                this.video_surface_click_task.take();
                this.play_video(key, cx);
            });
        }));
    }

    pub(super) fn toggle_video_theater(&mut self, source: TheaterVideo, cx: &mut Context<Self>) {
        if self
            .theater_video
            .as_ref()
            .is_some_and(|active| active.key == source.key)
        {
            self.exit_video_theater(cx);
            return;
        }
        self.theater_video = Some(source.clone());
        let source_key = self.source_key(source.key.room_id, source.key.attachment_id);
        self.video_sources
            .promote(source_key, source.descriptor.clone());
        self.video_sources
            .set_pin(source_key, VideoSourcePin::Theater, true);
        self.pump_video_sources(cx);
        self.show_video_controls(source.key, cx);
        self.video_surface_click_task.take();
        cx.notify();
    }

    pub(super) fn exit_video_theater(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(theater) = self.theater_video.take() else {
            return false;
        };
        self.video_surface_click_task.take();
        self.finish_video_scrub(cx);
        self.finish_video_volume_drag(cx);
        self.video_volume_popup_bounds.set(None);
        self.video_controls.player_hovered = false;
        self.schedule_video_controls_hide(theater.key, cx);
        self.sync_video_source_pins();
        self.pump_video_sources(cx);
        cx.notify();
        true
    }

    fn hover_video_scrub(&mut self, key: VideoKey, fraction: f64, cx: &mut Context<Self>) {
        self.show_video_controls(key, cx);
        if self.video_controls.scrub_hover_fraction != Some(fraction) {
            self.video_controls.scrub_hover_fraction = Some(fraction);
            cx.notify();
        }
    }

    fn clear_video_scrub_hover(&mut self, key: VideoKey, cx: &mut Context<Self>) {
        if self.video_controls.active_key == Some(key)
            && self.video_controls.scrub_hover_fraction.take().is_some()
        {
            cx.notify();
        }
    }

    fn begin_video_scrub(
        &mut self,
        key: VideoKey,
        duration: f64,
        bounds: Bounds<Pixels>,
        event: &MouseDownEvent,
        cx: &mut Context<Self>,
    ) {
        let Some(fraction) = horizontal_fraction(bounds, event.position.x, duration) else {
            return;
        };
        self.note_media_interaction(MediaPlaybackTarget::Video(key));
        self.show_video_controls(key, cx);
        self.video_controls_hide_task.take();
        self.video_scrub = Some(VideoScrub {
            key,
            bounds,
            duration,
            last_fraction: fraction,
            last_seek: Instant::now(),
        });
        if let Err(error) = self.videos.scrub(key, fraction, duration, SeekMode::Exact) {
            kvlog::error!(
                "embedded video initial scrub failed",
                room_id = key.room_id,
                message_id = key.message_id,
                attachment_timestamp_ms = key.attachment_id.timestamp_ms,
                attachment_transfer_id = key.attachment_id.transfer_id,
                fraction,
                err = %error
            );
            self.status = format!("Seek failed: {error}").into();
        }
        cx.stop_propagation();
        cx.notify();
    }

    pub(super) fn drag_video_scrub(
        &mut self,
        event: &MouseMoveEvent,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(mut scrub) = self.video_scrub else {
            return false;
        };
        if !event.dragging() {
            self.finish_video_scrub(cx);
            return true;
        }
        let Some(fraction) = horizontal_fraction(scrub.bounds, event.position.x, scrub.duration)
        else {
            self.finish_video_scrub(cx);
            return true;
        };
        if fraction != scrub.last_fraction {
            scrub.last_fraction = fraction;
            let dispatch_seek = scrub.should_dispatch_seek(Instant::now());
            self.video_scrub = Some(scrub);
            self.video_controls.scrub_hover_fraction = Some(fraction);
            if dispatch_seek
                && let Err(error) =
                    self.videos
                        .scrub(scrub.key, fraction, scrub.duration, SeekMode::Exact)
            {
                kvlog::error!(
                    "embedded video drag scrub failed",
                    room_id = scrub.key.room_id,
                    message_id = scrub.key.message_id,
                    attachment_timestamp_ms = scrub.key.attachment_id.timestamp_ms,
                    attachment_transfer_id = scrub.key.attachment_id.transfer_id,
                    fraction,
                    err = %error
                );
                self.status = format!("Seek failed: {error}").into();
            }
            cx.notify();
        }
        cx.stop_propagation();
        true
    }

    pub(super) fn finish_video_scrub(&mut self, cx: &mut Context<Self>) {
        let Some(scrub) = self.video_scrub.take() else {
            return;
        };
        self.schedule_video_controls_hide(scrub.key, cx);
        cx.notify();
    }

    fn hover_video_volume(&mut self, key: VideoKey, hovered: bool, cx: &mut Context<Self>) {
        self.show_video_controls(key, cx);
        self.video_controls.volume_button_hovered = hovered;
        if hovered {
            self.video_volume_hide_task.take();
            self.video_controls.volume_open = true;
            self.video_controls_hide_task.take();
        } else {
            self.schedule_video_volume_close(key, cx);
        }
        cx.notify();
    }

    fn hover_video_volume_popup(&mut self, key: VideoKey, hovered: bool, cx: &mut Context<Self>) {
        self.video_controls.volume_popup_hovered = hovered;
        if hovered {
            self.video_volume_hide_task.take();
            self.video_controls.volume_open = true;
        } else {
            self.schedule_video_volume_close(key, cx);
        }
        cx.notify();
    }

    fn schedule_video_volume_close(&mut self, key: VideoKey, cx: &mut Context<Self>) {
        self.video_volume_hide_task.take();
        self.video_volume_hide_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(160))
                .await;
            let _ = this.update(cx, |this, cx| {
                this.video_volume_hide_task.take();
                if this.video_controls.active_key == Some(key)
                    && !this.video_controls.volume_button_hovered
                    && !this.video_controls.volume_popup_hovered
                    && !this.video_volume_drag.is_some_and(|drag| drag.key == key)
                {
                    this.video_controls.volume_open = false;
                    this.video_volume_popup_bounds.set(None);
                    this.schedule_video_controls_hide(key, cx);
                    cx.notify();
                }
            });
        }));
    }

    fn set_video_volume(&mut self, key: VideoKey, volume: f64, cx: &mut Context<Self>) {
        self.note_media_interaction(MediaPlaybackTarget::Video(key));
        if let Err(error) = self.videos.set_volume_for(key, volume) {
            kvlog::error!(
                "embedded video volume change failed",
                room_id = key.room_id,
                message_id = key.message_id,
                attachment_timestamp_ms = key.attachment_id.timestamp_ms,
                attachment_transfer_id = key.attachment_id.transfer_id,
                volume,
                err = %error
            );
            self.status = format!("Volume failed: {error}").into();
        }
        self.show_video_controls(key, cx);
        cx.notify();
    }

    fn toggle_video_mute(&mut self, key: VideoKey, cx: &mut Context<Self>) {
        self.note_media_interaction(MediaPlaybackTarget::Video(key));
        if let Err(error) = self.videos.toggle_mute(key) {
            kvlog::error!(
                "embedded video mute toggle failed",
                room_id = key.room_id,
                message_id = key.message_id,
                attachment_timestamp_ms = key.attachment_id.timestamp_ms,
                attachment_transfer_id = key.attachment_id.transfer_id,
                err = %error
            );
            self.status = format!("Volume failed: {error}").into();
        }
        self.show_video_controls(key, cx);
        self.video_controls.volume_open = true;
        cx.notify();
    }

    fn begin_video_volume_drag(
        &mut self,
        key: VideoKey,
        bounds: Bounds<Pixels>,
        event: &MouseDownEvent,
        cx: &mut Context<Self>,
    ) {
        let Some(fraction) = vertical_fraction(bounds, event.position.y) else {
            return;
        };
        self.video_volume_drag = Some(VideoVolumeDrag { key, bounds });
        self.video_controls.volume_open = true;
        self.video_controls_hide_task.take();
        self.set_video_volume(key, fraction * 100.0, cx);
        cx.stop_propagation();
    }

    pub(super) fn drag_video_volume(
        &mut self,
        event: &MouseMoveEvent,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(drag) = self.video_volume_drag else {
            return false;
        };
        if !event.dragging() {
            self.finish_video_volume_drag(cx);
            return true;
        }
        if let Some(fraction) = vertical_fraction(drag.bounds, event.position.y) {
            self.set_video_volume(drag.key, fraction * 100.0, cx);
        }
        cx.stop_propagation();
        true
    }

    pub(super) fn finish_video_volume_drag(&mut self, cx: &mut Context<Self>) {
        let Some(drag) = self.video_volume_drag.take() else {
            return;
        };
        self.schedule_video_volume_close(drag.key, cx);
        cx.notify();
    }

    pub(super) fn scroll_video_volume(
        &mut self,
        event: &ScrollWheelEvent,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(key) = self.video_controls.active_key else {
            return false;
        };
        if !self.video_controls.volume_open
            || !self
                .video_volume_popup_bounds
                .get()
                .is_some_and(|bounds| bounds.contains(&event.position))
                && !self
                    .video_volume_button_bounds
                    .get()
                    .is_some_and(|bounds| bounds.contains(&event.position))
        {
            return false;
        }
        let delta = volume_scroll_delta(event.delta);
        if delta == 0.0 {
            return false;
        }
        self.adjust_video_volume(key, delta, cx);
        self.video_controls.volume_open = true;
        self.video_controls_hide_task.take();
        true
    }

    fn adjust_video_volume(&mut self, key: VideoKey, delta: f64, cx: &mut Context<Self>) {
        self.note_media_interaction(MediaPlaybackTarget::Video(key));
        if let Err(error) = self.videos.adjust_volume(key, delta) {
            kvlog::error!(
                "embedded video volume change failed",
                room_id = key.room_id,
                message_id = key.message_id,
                attachment_timestamp_ms = key.attachment_id.timestamp_ms,
                attachment_transfer_id = key.attachment_id.transfer_id,
                delta,
                err = %error
            );
            self.status = format!("Volume failed: {error}").into();
        }
        self.show_video_controls(key, cx);
        cx.notify();
    }
}
