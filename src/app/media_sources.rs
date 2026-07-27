use super::*;

impl ChattView {
    pub(super) fn advance_video(&mut self, cx: &mut Context<Self>) {
        let drain = self.videos.drain();
        let playback_changed = drain.changed || !drain.errors.is_empty();
        let audio_drain = self.audios.drain();
        let audio_view_changed = audio_drain.view_changed || !audio_drain.errors.is_empty();
        let audio_source_changed =
            audio_drain.source_changed || !audio_drain.transport_failures.is_empty();
        let thumbnails_changed = self.video_thumbnails.drain_results();
        let finished_sources = self.video_thumbnails.take_finished_sources();
        let transport_failures = self.video_thumbnails.take_transport_failures();
        let source_work_changed =
            thumbnails_changed || !finished_sources.is_empty() || !transport_failures.is_empty();
        let transport_failure_keys = transport_failures
            .iter()
            .map(|(key, _)| *key)
            .collect::<HashSet<_>>();
        for key in finished_sources {
            if transport_failure_keys.contains(&key) {
                self.video_sources
                    .set_pin(key, VideoSourcePin::Thumbnail, false);
            } else {
                self.video_sources.thumbnail_finished(key);
            }
        }
        for (key, error) in transport_failures {
            self.video_sources.source_failed(key, error, Instant::now());
        }
        self.apply_video_drain(drain);
        self.apply_audio_drain(audio_drain);
        let media_source_work_changed =
            source_work_changed || playback_changed || audio_source_changed;
        if media_source_work_changed {
            self.sync_video_source_pins();
            self.pump_video_sources(cx);
        }
        if thumbnails_changed || audio_view_changed {
            cx.notify();
        }
    }

    pub(super) fn source_key(
        &self,
        room_id: RoomId,
        attachment_id: AttachmentId,
    ) -> AttachmentSourceKey {
        AttachmentSourceKey {
            namespace: self.media_namespace_generation,
            room_id,
            attachment_id,
        }
    }

    pub(super) fn video_descriptor(&self, key: VideoKey) -> Option<AttachmentDescriptor> {
        self.theater_video
            .as_ref()
            .filter(|video| video.key == key)
            .map(|video| video.descriptor.clone())
            .or_else(|| {
                self.model
                    .messages
                    .iter()
                    .find(|message| message.room_id == key.room_id && message.id == key.message_id)
                    .and_then(|message| message.attachment.as_ref())
                    .filter(|attachment| attachment.is_video())
                    .map(|attachment| attachment.descriptor.clone())
            })
    }

    pub(super) fn audio_descriptor(&self, key: AudioKey) -> Option<AttachmentDescriptor> {
        self.model
            .messages
            .iter()
            .find(|message| message.room_id == key.room_id && message.id == key.message_id)
            .and_then(|message| message.attachment.as_ref())
            .filter(|attachment| attachment.is_audio())
            .map(|attachment| attachment.descriptor.clone())
    }

    pub(super) fn reset_attachment_source_state(&mut self) {
        self.audios.clear_sessions();
        self.videos.clear_sessions();
        self.video_thumbnails.clear();
        self.clear_video_interactions();
        self.pending_video_plays.clear();
        self.pending_audio_plays.clear();
        self.visible_video_keys.clear();
        self.visible_audio_keys.clear();
        self.media_interactions.clear();
        self.audio_scrub = None;
        self.audio_volume_drag = None;
        self.video_source_retry_task.take();
        self.media_namespace_generation = self.media_namespace_generation.wrapping_add(1).max(1);
        let canceled = self.video_sources.reset(
            self.media_namespace_generation,
            self.model.limits.concurrent_attachment_streams,
        );
        for request_id in canceled {
            self.model.pending.remove(&request_id);
        }
        self.attachment_source_registry
            .clear(self.media_namespace_generation);
    }

    pub(super) fn attachment_source_protocol_error(
        &mut self,
        reason: &str,
        cx: &mut Context<Self>,
    ) {
        kvlog::error!("daemon attachment source protocol error", err = %reason);
        self.reset_attachment_source_state();
        self.daemon.disconnect_protocol(reason);
        self.status = format!("Attachment source protocol error · {reason}").into();
        cx.notify();
    }

    pub(super) fn pump_video_sources(&mut self, cx: &mut Context<Self>) {
        if !self.model.is_ready() {
            return;
        }
        self.video_sources
            .update_limits(self.model.limits.concurrent_attachment_streams);
        loop {
            let request_id = self.request_id();
            let Some(open) = self.video_sources.start_next(request_id, Instant::now()) else {
                break;
            };
            self.model.pending.insert(
                request_id,
                PendingRequest {
                    operation: Operation::OpenAttachmentSource,
                    room_id: Some(open.key.room_id),
                    draft: None,
                    transfer_id: None,
                },
            );
            let frame = ClientFrame::OpenAttachmentSource {
                request_id: open.request_id,
                room_id: open.key.room_id,
                attachment_id: open.key.attachment_id,
            };
            if let Err(error) = self.daemon.send(frame) {
                self.model.pending.remove(&request_id);
                self.video_sources
                    .failed_to_send(request_id, error.clone(), Instant::now());
                self.status = format!("Could not request attachment source · {error}").into();
            }
        }
        self.schedule_video_source_retry(cx);
    }

    fn schedule_video_source_retry(&mut self, cx: &mut Context<Self>) {
        self.video_source_retry_task.take();
        let Some(retry_at) = self.video_sources.next_retry_at() else {
            return;
        };
        let delay = retry_at.saturating_duration_since(Instant::now());
        self.video_source_retry_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            let _ = this.update(cx, |this, cx| {
                this.video_source_retry_task.take();
                this.pump_video_sources(cx);
                cx.notify();
            });
        }));
    }

    pub(super) fn sync_video_source_pins(&mut self) {
        let mut playing = self.videos.retained_source_keys();
        playing.extend(self.audios.retained_source_keys());
        self.video_sources
            .sync_pins(VideoSourcePin::Playing, &playing);
        let theater = self
            .theater_video
            .as_ref()
            .map(|video| self.source_key(video.key.room_id, video.key.attachment_id))
            .into_iter()
            .collect::<HashSet<_>>();
        self.video_sources
            .sync_pins(VideoSourcePin::Theater, &theater);
        let mut pending = self
            .pending_video_plays
            .iter()
            .map(|video| self.source_key(video.room_id, video.attachment_id))
            .collect::<HashSet<_>>();
        pending.extend(
            self.pending_audio_plays
                .iter()
                .map(|audio| self.source_key(audio.room_id, audio.attachment_id)),
        );
        self.video_sources
            .sync_pins(VideoSourcePin::PendingPlay, &pending);
    }

    pub(super) fn apply_video_drain(&mut self, drain: VideoDrain) {
        for error in &drain.errors {
            kvlog::error!("embedded video failed", err = %error);
        }
        if let Some(error) = drain.errors.last() {
            self.status = error.clone().into();
        }
    }

    pub(super) fn apply_audio_drain(&mut self, drain: AudioDrain) {
        for (key, error) in drain.transport_failures {
            self.video_sources.source_failed(key, error, Instant::now());
        }
        for error in &drain.errors {
            kvlog::error!("embedded audio failed", err = %error);
        }
        if let Some(error) = drain.errors.last() {
            self.status = error.clone().into();
        }
    }

    pub(super) fn clear_video_interactions(&mut self) {
        self.video_scrub = None;
        self.video_volume_drag = None;
        self.video_controls.clear();
        self.theater_video = None;
        self.video_controls_animation_task.take();
        self.video_controls_hide_task.take();
        self.video_volume_hide_task.take();
        self.video_surface_click_task.take();
        self.video_volume_popup_bounds.set(None);
        self.video_volume_button_bounds.set(None);
    }

    pub(super) fn update_video_visibility(
        &mut self,
        range: std::ops::Range<usize>,
        cx: &mut Context<Self>,
    ) {
        let list_len = self.message_list.len();
        let visible_start = range.start.min(list_len);
        let visible_end = range.end.min(list_len);
        let mut visible = (visible_start..visible_end)
            .filter_map(|index| self.message_list.get(index))
            .filter(|item| !item.is_collapsed())
            .filter_map(|item| item.message_index())
            .filter_map(|index| self.model.messages.get(index))
            .filter_map(message_video_key)
            .collect::<HashSet<_>>();
        let visible_audio = (visible_start..visible_end)
            .filter_map(|index| self.message_list.get(index))
            .filter(|item| !item.is_collapsed())
            .filter_map(|item| item.message_index())
            .filter_map(|index| self.model.messages.get(index))
            .filter_map(message_audio_key)
            .collect::<HashSet<_>>();
        if let Some(theater) = self.theater_video.as_ref() {
            visible.insert(theater.key);
        }
        self.visible_video_keys = visible.clone();
        self.visible_audio_keys = visible_audio.clone();

        let mut ordered_rows = (visible_start..visible_end)
            .rev()
            .map(|index| (index, true))
            .collect::<Vec<_>>();
        for distance in 0..4 {
            if let Some(index) = visible_end
                .checked_add(distance)
                .filter(|index| *index < list_len)
            {
                ordered_rows.push((index, false));
            }
            if let Some(index) = visible_start.checked_sub(distance + 1) {
                ordered_rows.push((index, false));
            }
        }
        let mut seen = HashSet::new();
        let theater_source_key = self
            .theater_video
            .as_ref()
            .map(|video| self.source_key(video.key.room_id, video.key.attachment_id));
        let mut candidates = ordered_rows
            .into_iter()
            .filter_map(|(index, visible)| {
                let item = self.message_list.get(index)?;
                if item.is_collapsed() {
                    return None;
                }
                let message = self.model.messages.get(item.message_index()?)?;
                let attachment = message
                    .attachment
                    .as_ref()?
                    .is_video()
                    .then_some(message.attachment.as_ref()?)?;
                let key = self.source_key(message.room_id, attachment.descriptor.id);
                seen.insert(key).then(|| VideoSourceCandidate {
                    key,
                    descriptor: attachment.descriptor.clone(),
                    visible,
                })
            })
            .collect::<Vec<_>>();
        candidates.retain(|candidate| {
            theater_source_key == Some(candidate.key)
                || self
                    .video_thumbnails
                    .view(ThumbnailKey {
                        source_key: candidate.key,
                    })
                    .image
                    .is_none()
        });
        if !candidates.is_empty() {
            self.video_thumbnails.warm();
        }
        let canceled = self.video_sources.update_visibility(candidates);
        for request_id in canceled {
            self.model.pending.remove(&request_id);
        }
        let drain = self.videos.update_visibility(&visible);
        let audio_changed = self.audios.update_visibility(&visible_audio);
        let changed = drain.changed || !drain.errors.is_empty() || audio_changed;
        self.apply_video_drain(drain);
        self.sync_video_source_pins();
        self.pump_video_sources(cx);
        if changed {
            cx.notify();
        }
    }
}
