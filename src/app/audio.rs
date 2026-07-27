use super::*;

impl ChattView {
    pub(super) fn render_attachment_audio(
        &mut self,
        room_id: RoomId,
        message_id: u64,
        descriptor: AttachmentDescriptor,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = audio_key(room_id, message_id, &descriptor);
        let source_key = self.source_key(room_id, descriptor.id);
        let mut audio = self.audios.view(key);
        match self.video_sources.view(source_key) {
            VideoSourceView::Loading if self.pending_audio_plays.contains(&key) => {
                audio.loading = true;
            }
            VideoSourceView::Failed { reason, .. } if audio.error.is_none() => {
                audio.loading = false;
                audio.error = Some(format!("Could not open audio · {reason}"));
            }
            _ => {}
        }
        let duration = audio.duration;
        let active_scrub = self.audio_scrub.filter(|scrub| scrub.key == key);
        let display_position = active_scrub.map_or(audio.position, AudioScrub::position);
        let view = cx.entity().downgrade();
        let handler: AudioPlayerHandler = Rc::new(move |event, _, cx| {
            let _ = view.update(cx, |this, cx| {
                this.handle_audio_player_event(key, duration, event, cx)
            });
        });
        render_audio_player(
            AudioPlayerConfig {
                key,
                audio,
                duration,
                display_position,
            },
            handler,
            AppliedSettings::get(cx),
        )
        .into_any_element()
    }

    fn handle_audio_player_event(
        &mut self,
        key: AudioKey,
        duration: f64,
        event: AudioPlayerEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            AudioPlayerEvent::Play => self.play_audio(key, cx),
            AudioPlayerEvent::ScrubPressed { bounds, event } => {
                self.begin_audio_scrub(key, duration, bounds, &event, cx)
            }
            AudioPlayerEvent::CycleSpeed => self.cycle_audio_speed(key, cx),
            AudioPlayerEvent::ToggleMute => self.toggle_audio_mute(key, cx),
            AudioPlayerEvent::VolumePressed { bounds, event } => {
                self.begin_audio_volume_drag(key, bounds, &event, cx)
            }
        }
    }

    pub(super) fn play_audio(&mut self, key: AudioKey, cx: &mut Context<Self>) {
        self.note_media_interaction(MediaPlaybackTarget::Audio(key));
        self.pending_audio_plays.retain(|pending| *pending == key);
        let source_key = self.source_key(key.room_id, key.attachment_id);
        match self.video_sources.view(source_key) {
            VideoSourceView::Ready(source) => {
                self.pending_audio_plays.remove(&key);
                match self.audios.play(key, Some(source)) {
                    Ok(()) => self.status = "Starting audio playback…".into(),
                    Err(error) => {
                        kvlog::error!(
                            "embedded audio play failed",
                            room_id = key.room_id,
                            message_id = key.message_id,
                            attachment_timestamp_ms = key.attachment_id.timestamp_ms,
                            attachment_transfer_id = key.attachment_id.transfer_id,
                            err = %error
                        );
                        self.status = format!("Audio playback failed: {error}").into();
                    }
                }
            }
            VideoSourceView::Absent | VideoSourceView::Loading | VideoSourceView::Failed { .. } => {
                if let Err(error) = self.audios.play(key, None) {
                    kvlog::error!(
                        "embedded audio preparation failed",
                        room_id = key.room_id,
                        message_id = key.message_id,
                        attachment_timestamp_ms = key.attachment_id.timestamp_ms,
                        attachment_transfer_id = key.attachment_id.transfer_id,
                        err = %error
                    );
                    self.status = format!("Audio playback failed: {error}").into();
                } else if let Some(descriptor) = self.audio_descriptor(key) {
                    self.pending_audio_plays.insert(key);
                    self.video_sources.promote(source_key, descriptor);
                    self.video_sources
                        .set_pin(source_key, VideoSourcePin::PendingPlay, true);
                    self.video_sources.retry(source_key);
                    self.pump_video_sources(cx);
                    self.status = "Preparing audio playback…".into();
                } else {
                    self.status = "Audio source is no longer available".into();
                }
            }
        }
        self.sync_video_source_pins();
        cx.notify();
    }

    pub(super) fn seek_audio(&mut self, key: AudioKey, seconds: f64, cx: &mut Context<Self>) {
        self.note_media_interaction(MediaPlaybackTarget::Audio(key));
        if let Err(error) = self.audios.seek(key, seconds) {
            kvlog::error!(
                "embedded audio seek failed",
                room_id = key.room_id,
                message_id = key.message_id,
                attachment_timestamp_ms = key.attachment_id.timestamp_ms,
                attachment_transfer_id = key.attachment_id.transfer_id,
                seconds,
                err = %error
            );
            self.status = format!("Audio seek failed: {error}").into();
        }
        cx.notify();
    }

    fn begin_audio_scrub(
        &mut self,
        key: AudioKey,
        duration: f64,
        bounds: Bounds<Pixels>,
        event: &MouseDownEvent,
        cx: &mut Context<Self>,
    ) {
        let Some(fraction) = horizontal_fraction(bounds, event.position.x, duration) else {
            return;
        };
        self.note_media_interaction(MediaPlaybackTarget::Audio(key));
        self.audio_scrub = Some(AudioScrub {
            key,
            bounds,
            duration,
            last_fraction: fraction,
            last_seek: Instant::now(),
        });
        if let Err(error) = self.audios.scrub(key, fraction, duration) {
            kvlog::error!(
                "embedded audio initial scrub failed",
                room_id = key.room_id,
                message_id = key.message_id,
                attachment_timestamp_ms = key.attachment_id.timestamp_ms,
                attachment_transfer_id = key.attachment_id.transfer_id,
                fraction,
                err = %error
            );
            self.status = format!("Audio seek failed: {error}").into();
        }
        cx.stop_propagation();
        cx.notify();
    }

    pub(super) fn drag_audio_scrub(
        &mut self,
        event: &MouseMoveEvent,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(mut scrub) = self.audio_scrub else {
            return false;
        };
        if !event.dragging() {
            self.finish_audio_scrub(cx);
            return true;
        }
        let Some(fraction) = horizontal_fraction(scrub.bounds, event.position.x, scrub.duration)
        else {
            self.finish_audio_scrub(cx);
            return true;
        };
        if fraction != scrub.last_fraction {
            scrub.last_fraction = fraction;
            let dispatch = scrub.should_dispatch_seek(Instant::now());
            self.audio_scrub = Some(scrub);
            if dispatch && let Err(error) = self.audios.scrub(scrub.key, fraction, scrub.duration) {
                kvlog::error!(
                    "embedded audio drag scrub failed",
                    room_id = scrub.key.room_id,
                    message_id = scrub.key.message_id,
                    attachment_timestamp_ms = scrub.key.attachment_id.timestamp_ms,
                    attachment_transfer_id = scrub.key.attachment_id.transfer_id,
                    fraction,
                    err = %error
                );
                self.status = format!("Audio seek failed: {error}").into();
            }
            cx.notify();
        }
        cx.stop_propagation();
        true
    }

    pub(super) fn finish_audio_scrub(&mut self, cx: &mut Context<Self>) {
        let Some(scrub) = self.audio_scrub.take() else {
            return;
        };
        if let Err(error) = self
            .audios
            .scrub(scrub.key, scrub.last_fraction, scrub.duration)
        {
            kvlog::error!(
                "embedded audio final scrub failed",
                room_id = scrub.key.room_id,
                message_id = scrub.key.message_id,
                attachment_timestamp_ms = scrub.key.attachment_id.timestamp_ms,
                attachment_transfer_id = scrub.key.attachment_id.transfer_id,
                fraction = scrub.last_fraction,
                err = %error
            );
            self.status = format!("Audio seek failed: {error}").into();
        }
        cx.notify();
    }

    fn set_audio_volume(&mut self, key: AudioKey, volume: f64, cx: &mut Context<Self>) {
        self.note_media_interaction(MediaPlaybackTarget::Audio(key));
        if let Err(error) = self.audios.set_volume_for(key, volume) {
            kvlog::error!(
                "embedded audio volume failed",
                room_id = key.room_id,
                message_id = key.message_id,
                attachment_timestamp_ms = key.attachment_id.timestamp_ms,
                attachment_transfer_id = key.attachment_id.transfer_id,
                volume,
                err = %error
            );
            self.status = format!("Audio volume failed: {error}").into();
        }
        cx.notify();
    }

    fn toggle_audio_mute(&mut self, key: AudioKey, cx: &mut Context<Self>) {
        self.note_media_interaction(MediaPlaybackTarget::Audio(key));
        if let Err(error) = self.audios.toggle_mute(key) {
            kvlog::error!(
                "embedded audio mute failed",
                room_id = key.room_id,
                message_id = key.message_id,
                attachment_timestamp_ms = key.attachment_id.timestamp_ms,
                attachment_transfer_id = key.attachment_id.transfer_id,
                err = %error
            );
            self.status = format!("Audio volume failed: {error}").into();
        }
        cx.notify();
    }

    fn cycle_audio_speed(&mut self, key: AudioKey, cx: &mut Context<Self>) {
        self.note_media_interaction(MediaPlaybackTarget::Audio(key));
        if let Err(error) = self.audios.cycle_playback_speed(key) {
            kvlog::error!(
                "embedded audio speed change failed",
                room_id = key.room_id,
                message_id = key.message_id,
                attachment_timestamp_ms = key.attachment_id.timestamp_ms,
                attachment_transfer_id = key.attachment_id.transfer_id,
                err = %error
            );
            self.status = format!("Audio speed change failed: {error}").into();
        }
        cx.notify();
    }

    fn begin_audio_volume_drag(
        &mut self,
        key: AudioKey,
        bounds: Bounds<Pixels>,
        event: &MouseDownEvent,
        cx: &mut Context<Self>,
    ) {
        let Some(fraction) = horizontal_fraction(bounds, event.position.x, 1.0) else {
            return;
        };
        self.audio_volume_drag = Some(AudioVolumeDrag { key, bounds });
        self.set_audio_volume(key, fraction * 100.0, cx);
        cx.stop_propagation();
    }

    pub(super) fn drag_audio_volume(
        &mut self,
        event: &MouseMoveEvent,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(drag) = self.audio_volume_drag else {
            return false;
        };
        if !event.dragging() {
            self.finish_audio_volume_drag(cx);
            return true;
        }
        if let Some(fraction) = horizontal_fraction(drag.bounds, event.position.x, 1.0) {
            self.set_audio_volume(drag.key, fraction * 100.0, cx);
        }
        cx.stop_propagation();
        true
    }

    pub(super) fn finish_audio_volume_drag(&mut self, cx: &mut Context<Self>) {
        if self.audio_volume_drag.take().is_some() {
            cx.notify();
        }
    }
}
