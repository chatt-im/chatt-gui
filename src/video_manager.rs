use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::mpsc,
    thread,
};

use anyhow::{Result, anyhow};
use async_channel::Sender as AsyncSender;
use gpui::PlatformSurface;
use local_rpc::{ids::RoomId, model::AttachmentId};

use crate::attachment_source::{AttachmentSourceRegistry, RegisteredAttachmentSource};
use crate::mpv_player::{
    AttachmentRenderBackend, MpvPlayer, SeekMode, VideoAdjustment, VideoEffect,
};

const RETAINED_OFFSCREEN_LIMIT: usize = 4;
const MAX_SESSION_ENTRIES: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct VideoEffectChange {
    pub(crate) effect: VideoEffect,
    pub(crate) value: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum VideoControlChange {
    Effect(VideoEffectChange),
    PlaybackSpeed(f64),
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct VideoEffectValues([f64; 4]);

impl VideoEffectValues {
    fn adjusted(self, effect: VideoEffect, delta: f64) -> VideoEffectChange {
        VideoEffectChange {
            effect,
            value: (self.0[effect.index()] + delta).clamp(-100.0, 100.0),
        }
    }

    fn set(&mut self, change: VideoEffectChange) {
        self.0[change.effect.index()] = change.value;
    }

    fn get(self, effect: VideoEffect) -> f64 {
        self.0[effect.index()]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct VideoKey {
    pub room_id: RoomId,
    pub message_id: u64,
    pub attachment_id: AttachmentId,
}

#[derive(Clone)]
pub(crate) struct VideoView {
    pub surface: Option<PlatformSurface>,
    pub position: f64,
    pub duration: f64,
    pub paused: bool,
    pub finished: bool,
    pub looping: bool,
    pub volume: f64,
    pub loading: bool,
    pub error: Option<String>,
    pub display_size: Option<(u32, u32)>,
}

impl Default for VideoView {
    fn default() -> Self {
        Self {
            surface: None,
            position: 0.0,
            duration: 0.0,
            paused: true,
            finished: false,
            looping: false,
            volume: 100.0,
            loading: false,
            error: None,
            display_size: None,
        }
    }
}

struct VideoSession {
    source: Option<RegisteredAttachmentSource>,
    player: Option<MpvPlayer>,
    position: f64,
    duration: f64,
    paused: bool,
    finished: bool,
    looping: bool,
    frame_ready: bool,
    visible: bool,
    touched: u64,
    error: Option<String>,
    display_size: Option<(u32, u32)>,
    effects: VideoEffectValues,
    playback_speed: f64,
}

impl VideoSession {
    fn new(source: RegisteredAttachmentSource, touched: u64, looping: bool) -> Self {
        Self {
            source: Some(source),
            player: None,
            position: 0.0,
            duration: 0.0,
            paused: true,
            finished: false,
            looping,
            frame_ready: false,
            visible: false,
            touched,
            error: None,
            display_size: None,
            effects: VideoEffectValues::default(),
            playback_speed: 1.0,
        }
    }
}

struct BuildResult(Result<(MpvPlayer, AttachmentRenderBackend), String>);

#[derive(Default)]
pub(crate) struct VideoDrain {
    pub changed: bool,
    pub errors: Vec<String>,
}

/// Owns independent attachment sessions while sharing the expensive backend
/// decision. Players are constructed only in response to playback demand.
pub(crate) struct AttachmentVideoManager {
    sessions: HashMap<VideoKey, VideoSession>,
    queued: VecDeque<VideoKey>,
    queued_keys: HashSet<VideoKey>,
    backend: Option<AttachmentRenderBackend>,
    build_in_flight: bool,
    build_results: mpsc::Receiver<BuildResult>,
    build_result_sender: mpsc::Sender<BuildResult>,
    reaper: mpsc::Sender<MpvPlayer>,
    wakeup: AsyncSender<()>,
    source_registry: AttachmentSourceRegistry,
    last_interacted: Option<VideoKey>,
    volume: f64,
    last_audible_volume: f64,
    /// The loop state a session starts in, kept here rather than passed in per
    /// call so reading it never has to touch a session.
    loop_by_default: bool,
    clock: u64,
}

impl AttachmentVideoManager {
    pub(crate) fn new(wakeup: AsyncSender<()>, source_registry: AttachmentSourceRegistry) -> Self {
        let (build_result_sender, build_results) = mpsc::channel();
        let (reaper, retired_players) = mpsc::channel::<MpvPlayer>();
        if let Err(error) = thread::Builder::new()
            .name("mpv-reaper".into())
            .spawn(move || {
                for player in retired_players {
                    drop(player);
                }
            })
        {
            kvlog::error!("could not start mpv cleanup worker", err = %error);
        }
        Self {
            sessions: HashMap::new(),
            queued: VecDeque::new(),
            queued_keys: HashSet::new(),
            backend: None,
            build_in_flight: false,
            build_results,
            build_result_sender,
            reaper,
            wakeup,
            source_registry,
            last_interacted: None,
            volume: 100.0,
            last_audible_volume: 100.0,
            loop_by_default: false,
            clock: 0,
        }
    }

    /// Sets the loop state new sessions start in. Existing sessions keep
    /// whatever the viewer last chose for them.
    pub(crate) fn set_loop_by_default(&mut self, loop_by_default: bool) {
        self.loop_by_default = loop_by_default;
    }

    pub(crate) fn ensure_source(&mut self, key: VideoKey, source: RegisteredAttachmentSource) {
        self.clock = self.clock.wrapping_add(1);
        if !self.sessions.contains_key(&key) {
            self.trim_sessions_to(MAX_SESSION_ENTRIES.saturating_sub(1));
        }
        let looping = self.loop_by_default;
        let session = self
            .sessions
            .entry(key)
            .or_insert_with(|| VideoSession::new(source.clone(), self.clock, looping));
        session.source = Some(source);
        session.touched = self.clock;
    }

    pub(crate) fn view(&self, key: VideoKey) -> VideoView {
        let Some(session) = self.sessions.get(&key) else {
            // Reported before anything is playing, so the loop control shows the
            // state a session would actually start in.
            return VideoView {
                volume: self.volume,
                looping: self.loop_by_default,
                ..VideoView::default()
            };
        };
        VideoView {
            surface: session
                .frame_ready
                .then(|| session.player.as_ref().map(MpvPlayer::surface))
                .flatten(),
            position: session.position,
            duration: session.duration,
            paused: session.paused,
            finished: session.finished,
            looping: session.looping,
            volume: self.volume,
            loading: self.queued_keys.contains(&key)
                || (session.player.is_some() && !session.frame_ready && !session.paused),
            error: session.error.clone(),
            display_size: session.display_size,
        }
    }

    pub(crate) fn play(&mut self, key: VideoKey) -> Result<()> {
        self.touch(key);
        self.last_interacted = Some(key);
        let volume = self.volume;
        let Some(session) = self.sessions.get_mut(&key) else {
            return Err(anyhow!("video source is no longer cached"));
        };
        let source = session
            .source
            .as_ref()
            .ok_or_else(|| anyhow!("video source is not ready"))?;
        session.error = None;
        if let Some(player) = session.player.as_mut() {
            if session.finished {
                player.load_at(
                    source.url(),
                    false,
                    volume,
                    session.playback_speed,
                    0.0,
                    session.looping,
                )?;
                apply_video_effects(player, session.effects)?;
                session.position = 0.0;
                session.duration = 0.0;
                session.paused = false;
                session.finished = false;
                session.frame_ready = false;
            } else {
                session.paused = player.toggle_pause()?;
            }
            return Ok(());
        }

        session.paused = false;
        session.finished = false;
        if self.queued_keys.insert(key) {
            self.queued.push_back(key);
        }
        self.pump_builds();
        Ok(())
    }

    pub(crate) fn seek(&mut self, key: VideoKey, seconds: f64) -> Result<()> {
        self.touch(key);
        self.last_interacted = Some(key);
        let Some(session) = self.sessions.get_mut(&key) else {
            return Ok(());
        };
        let position = (session.position + seconds).clamp(0.0, session.duration.max(0.0));
        session.position = position;
        session.finished = false;
        if let Some(player) = session.player.as_ref() {
            player.seek_absolute(position)?;
        }
        Ok(())
    }

    pub(crate) fn scrub(
        &mut self,
        key: VideoKey,
        fraction: f64,
        duration_hint: f64,
        mode: SeekMode,
    ) -> Result<()> {
        self.touch(key);
        self.last_interacted = Some(key);
        let already_queued = self.queued_keys.contains(&key);
        let Some(session) = self.sessions.get_mut(&key) else {
            return Ok(());
        };
        let duration = if session.duration > 0.0 {
            session.duration
        } else {
            duration_hint.max(0.0)
        };
        if duration <= 0.0 {
            return Ok(());
        }

        let fraction = fraction.clamp(0.0, 1.0);
        let position = duration * fraction;
        session.position = position;
        session.finished = false;
        session.error = None;
        if let Some(player) = session.player.as_ref() {
            player.seek_percent(fraction * 100.0, position, mode)?;
            return Ok(());
        }

        if !already_queued {
            session.paused = true;
            if self.queued_keys.insert(key) {
                self.queued.push_back(key);
            }
        }
        self.pump_builds();
        Ok(())
    }

    pub(crate) fn adjust_volume(&mut self, key: VideoKey, delta: f64) -> Result<()> {
        self.touch(key);
        self.last_interacted = Some(key);
        self.set_volume(self.volume + delta)
    }

    pub(crate) fn adjust_video(
        &mut self,
        key: VideoKey,
        adjustment: VideoAdjustment,
    ) -> Result<Option<VideoControlChange>> {
        self.touch(key);
        self.last_interacted = Some(key);
        let Some(session) = self.sessions.get_mut(&key) else {
            return Ok(None);
        };
        let Some(player) = session.player.as_ref() else {
            return Ok(None);
        };
        match adjustment {
            VideoAdjustment::PlaybackSpeed(factor) => {
                let unbounded_speed = session.playback_speed * factor;
                let speed = adjusted_playback_speed(session.playback_speed, factor);
                if speed == unbounded_speed {
                    player.adjust_video(adjustment)?;
                } else {
                    player.set_speed(speed)?;
                }
                session.playback_speed = speed;
                Ok(Some(VideoControlChange::PlaybackSpeed(speed)))
            }
            adjustment => {
                let change = adjustment
                    .effect_delta()
                    .map(|(effect, delta)| session.effects.adjusted(effect, delta))
                    .expect("picture adjustment has an effect delta");
                player.adjust_video(adjustment)?;
                session.effects.set(change);
                Ok(Some(VideoControlChange::Effect(change)))
            }
        }
    }

    pub(crate) fn step_frame(&mut self, key: VideoKey, backwards: bool) -> Result<bool> {
        self.touch(key);
        self.last_interacted = Some(key);
        let Some(session) = self.sessions.get_mut(&key) else {
            return Ok(false);
        };
        let Some(player) = session.player.as_mut() else {
            return Ok(false);
        };
        player.step_frame(backwards)?;
        session.paused = true;
        session.finished = false;
        Ok(true)
    }

    pub(crate) fn set_frame_hold_playing(&mut self, key: VideoKey, playing: bool) -> Result<bool> {
        self.touch(key);
        self.last_interacted = Some(key);
        let Some(session) = self.sessions.get_mut(&key) else {
            return Ok(false);
        };
        let Some(player) = session.player.as_mut() else {
            return Ok(false);
        };
        player.set_paused(!playing)?;
        session.paused = !playing;
        Ok(true)
    }

    pub(crate) fn set_volume_for(&mut self, key: VideoKey, volume: f64) -> Result<()> {
        self.touch(key);
        self.last_interacted = Some(key);
        self.set_volume(volume)
    }

    pub(crate) fn toggle_mute(&mut self, key: VideoKey) -> Result<()> {
        self.touch(key);
        self.last_interacted = Some(key);
        if self.volume > 0.0 {
            self.last_audible_volume = self.volume;
            self.set_volume(0.0)
        } else {
            self.set_volume(self.last_audible_volume.max(1.0))
        }
    }

    pub(crate) fn toggle_looping(&mut self, key: VideoKey) -> Result<bool> {
        self.touch(key);
        self.last_interacted = Some(key);
        let Some(session) = self.sessions.get_mut(&key) else {
            return Err(anyhow!("video source is no longer cached"));
        };
        let looping = !session.looping;
        if let Some(player) = session.player.as_ref() {
            player.set_looping(looping)?;
        }
        session.looping = looping;
        Ok(looping)
    }

    fn set_volume(&mut self, volume: f64) -> Result<()> {
        let volume = volume.clamp(0.0, 100.0);
        self.volume = volume;
        if volume > 0.0 {
            self.last_audible_volume = volume;
        }
        let mut first_error = None;
        for session in self.sessions.values() {
            let Some(player) = session.player.as_ref() else {
                continue;
            };
            if let Err(error) = player.set_volume(volume)
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    pub(crate) fn retained_source_keys(
        &self,
    ) -> HashSet<crate::attachment_source::AttachmentSourceKey> {
        self.sessions
            .iter()
            .filter(|(key, session)| session.player.is_some() || self.queued_keys.contains(key))
            .filter_map(|(_, session)| session.source.as_ref().map(|source| source.source().key()))
            .collect()
    }

    pub(crate) fn update_visibility(&mut self, visible: &HashSet<VideoKey>) -> VideoDrain {
        let mut drain = VideoDrain::default();
        let keys = self.sessions.keys().copied().collect::<Vec<_>>();
        for key in keys {
            let is_visible = visible.contains(&key);
            let Some(session) = self.sessions.get_mut(&key) else {
                continue;
            };
            if session.visible && !is_visible {
                if !session.paused {
                    if let Some(player) = session.player.as_mut() {
                        match player.set_paused(true) {
                            Ok(()) => drain.changed = true,
                            Err(error) => {
                                drain.errors.push(format!("Playback pause failed: {error}"))
                            }
                        }
                    }
                    session.paused = true;
                }
                self.queued_keys.remove(&key);
            }
            session.visible = is_visible;
            if !is_visible
                && session.paused
                && session.player.is_none()
                && !self.queued_keys.contains(&key)
            {
                session.source = None;
            }
        }
        self.queued.retain(|key| self.queued_keys.contains(key));
        self.enforce_retained_limit(&mut drain);
        self.enforce_session_limit(&mut drain);
        self.pump_builds();
        drain
    }

    pub(crate) fn drain(&mut self) -> VideoDrain {
        let mut drain = VideoDrain::default();
        while let Ok(result) = self.build_results.try_recv() {
            self.build_in_flight = false;
            drain.changed = true;
            match result.0 {
                Ok((player, backend)) => {
                    self.backend = Some(backend);
                    self.assign_player(player, &mut drain);
                }
                Err(error) => {
                    if let Some(key) = self.pop_queued() {
                        if let Some(session) = self.sessions.get_mut(&key) {
                            session.paused = true;
                            session.error = Some(error.clone());
                        }
                        drain.errors.push(format!("Video unavailable: {error}"));
                    } else {
                        kvlog::warn!(
                            "video player build failed after playback request was canceled",
                            err = %error
                        );
                    }
                }
            }
        }

        let mut failed = Vec::new();
        for (key, session) in &mut self.sessions {
            let Some(player) = session.player.as_mut() else {
                continue;
            };
            match player.drain_events() {
                Ok(playback) => {
                    let changed = session.position != playback.position
                        || session.duration != playback.duration
                        || session.paused != playback.paused
                        || session.finished != playback.finished
                        || session.frame_ready != playback.frame_ready
                        || session.display_size != playback.display_size;
                    session.position = playback.position;
                    session.duration = playback.duration;
                    session.paused = playback.paused;
                    session.finished = playback.finished;
                    session.frame_ready = playback.frame_ready;
                    session.display_size = playback.display_size;
                    drain.changed |= changed;
                }
                Err(error) => failed.push((*key, format!("Video event failed: {error}"))),
            }
        }
        for (key, error) in failed {
            if let Some(session) = self.sessions.get_mut(&key) {
                if let Some(player) = session.player.take() {
                    let _ = self.reaper.send(player);
                }
                session.paused = true;
                session.frame_ready = false;
                session.display_size = None;
                session.error = Some(error.clone());
            }
            drain.errors.push(error);
            drain.changed = true;
        }
        self.enforce_session_limit(&mut drain);
        self.pump_builds();
        drain
    }

    pub(crate) fn retain_sources(&mut self, retained: &HashSet<VideoKey>) -> VideoDrain {
        let mut drain = VideoDrain::default();
        self.queued_keys.retain(|key| retained.contains(key));
        self.queued.retain(|key| self.queued_keys.contains(key));
        if self
            .last_interacted
            .is_some_and(|key| !retained.contains(&key))
        {
            self.last_interacted = None;
        }
        let removed = self
            .sessions
            .keys()
            .filter(|key| !retained.contains(key))
            .copied()
            .collect::<Vec<_>>();
        for key in removed {
            let Some(mut session) = self.sessions.remove(&key) else {
                continue;
            };
            if let Some(player) = session.player.take() {
                self.recycle(player);
            }
            drain.changed = true;
        }
        self.pump_builds();
        drain
    }

    pub(crate) fn clear_sessions(&mut self) {
        self.queued.clear();
        self.queued_keys.clear();
        self.last_interacted = None;
        let players = self
            .sessions
            .drain()
            .filter_map(|(_, mut session)| session.player.take())
            .collect::<Vec<_>>();
        for player in players {
            self.recycle(player);
        }
    }

    fn assign_player(&mut self, player: MpvPlayer, drain: &mut VideoDrain) {
        let Some(key) = self.pop_queued() else {
            let _ = self.reaper.send(player);
            return;
        };
        self.assign_specific_player(key, player, drain);
    }

    fn pop_queued(&mut self) -> Option<VideoKey> {
        while let Some(key) = self.queued.pop_front() {
            if self.queued_keys.remove(&key)
                && self
                    .sessions
                    .get(&key)
                    .is_some_and(|session| session.player.is_none())
            {
                return Some(key);
            }
        }
        None
    }

    fn pump_builds(&mut self) {
        if self.build_in_flight {
            return;
        }
        if self.queued_keys.is_empty() {
            return;
        }
        self.build_in_flight = true;
        let sender = self.build_result_sender.clone();
        let wakeup = self.wakeup.clone();
        let preferred_backend = self.backend.clone();
        let source_registry = self.source_registry.clone();
        if let Err(error) = thread::Builder::new()
            .name("mpv-builder".into())
            .spawn(move || {
                let started_at = std::time::Instant::now();
                kvlog::info!(
                    "asynchronous video player build started",
                    cached_backend = preferred_backend.is_some()
                );
                let result =
                    MpvPlayer::new_attachment(wakeup.clone(), preferred_backend, source_registry)
                        .map_err(|error| format!("{error:#}"));
                match &result {
                    Ok(_) => kvlog::info!(
                        "asynchronous video player build completed",
                        elapsed_ms = started_at.elapsed().as_secs_f64() * 1_000.0
                    ),
                    Err(error) => {
                        kvlog::error!(
                            "asynchronous video player build failed",
                            elapsed_ms = started_at.elapsed().as_secs_f64() * 1_000.0,
                            err = %error
                        )
                    }
                }
                let _ = sender.send(BuildResult(result));
                let _ = wakeup.try_send(());
            })
        {
            self.build_in_flight = false;
            let _ = self.build_result_sender.send(BuildResult(Err(format!(
                "could not start mpv builder: {error}"
            ))));
            let _ = self.wakeup.try_send(());
        }
    }

    fn assign_specific_player(
        &mut self,
        key: VideoKey,
        mut player: MpvPlayer,
        drain: &mut VideoDrain,
    ) {
        let Some(session) = self.sessions.get_mut(&key) else {
            let _ = self.reaper.send(player);
            return;
        };
        let Some(source) = session.source.as_ref() else {
            session.paused = true;
            session.error = Some("Video source is not ready".into());
            let _ = self.reaper.send(player);
            return;
        };
        if let Err(error) = player
            .load_at(
                source.url(),
                session.paused,
                self.volume,
                session.playback_speed,
                session.position,
                session.looping,
            )
            .and_then(|()| apply_video_effects(&player, session.effects))
        {
            let error = format!("Could not open video: {error}");
            session.paused = true;
            session.error = Some(error.clone());
            drain.errors.push(error);
            let _ = self.reaper.send(player);
            return;
        }
        session.frame_ready = false;
        session.finished = false;
        session.display_size = None;
        session.player = Some(player);
        drain.changed = true;
    }

    fn enforce_retained_limit(&mut self, drain: &mut VideoDrain) {
        let mut retained = self
            .sessions
            .iter()
            .filter(|(_, session)| !session.visible && session.player.is_some())
            .map(|(key, session)| (*key, session.touched))
            .collect::<Vec<_>>();
        retained.sort_by_key(|(_, touched)| *touched);
        let evict_count = retained.len().saturating_sub(RETAINED_OFFSCREEN_LIMIT);
        for (key, _) in retained.into_iter().take(evict_count) {
            let Some(session) = self.sessions.get_mut(&key) else {
                continue;
            };
            let Some(player) = session.player.take() else {
                continue;
            };
            session.frame_ready = false;
            session.display_size = None;
            session.source = None;
            self.recycle(player);
            drain.changed = true;
        }
    }

    fn trim_sessions_to(&mut self, limit: usize) {
        while self.sessions.len() > limit {
            let Some(key) = self
                .sessions
                .iter()
                .filter(|(_, session)| !session.visible && session.player.is_none())
                .min_by_key(|(_, session)| session.touched)
                .map(|(key, _)| *key)
            else {
                break;
            };
            self.sessions.remove(&key);
            if self.last_interacted == Some(key) {
                self.last_interacted = None;
            }
        }
    }

    fn enforce_session_limit(&mut self, drain: &mut VideoDrain) {
        let before = self.sessions.len();
        self.trim_sessions_to(MAX_SESSION_ENTRIES);
        drain.changed |= self.sessions.len() != before;
    }

    fn recycle(&self, player: MpvPlayer) {
        let _ = self.reaper.send(player);
    }

    fn touch(&mut self, key: VideoKey) {
        self.clock = self.clock.wrapping_add(1);
        if let Some(session) = self.sessions.get_mut(&key) {
            session.touched = self.clock;
        }
    }
}

fn apply_video_effects(player: &MpvPlayer, effects: VideoEffectValues) -> Result<()> {
    for effect in VideoEffect::ALL {
        player.set_video_effect(effect, effects.get(effect))?;
    }
    Ok(())
}

fn adjusted_playback_speed(speed: f64, factor: f64) -> f64 {
    (speed * factor).clamp(0.25, 4.0)
}

impl Drop for AttachmentVideoManager {
    fn drop(&mut self) {
        for (_, mut session) in self.sessions.drain() {
            if let Some(player) = session.player.take() {
                let _ = self.reaper.send(player);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn key(message_id: u64) -> VideoKey {
        VideoKey {
            room_id: RoomId(1),
            message_id,
            attachment_id: AttachmentId {
                timestamp_ms: message_id,
                transfer_id: local_rpc::ids::FileTransferId(message_id),
            },
        }
    }

    fn manager() -> AttachmentVideoManager {
        let (wakeup, _) = async_channel::bounded(1);
        AttachmentVideoManager::new(wakeup, AttachmentSourceRegistry::new(1))
    }

    fn ensure_source(videos: &mut AttachmentVideoManager, key: VideoKey) {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"x").unwrap();
        let source = crate::attachment_source::AttachmentSource::direct(
            crate::attachment_source::AttachmentSourceKey {
                namespace: 1,
                room_id: key.room_id,
                attachment_id: key.attachment_id,
            },
            file,
            1,
        );
        let source = videos.source_registry.register(source);
        videos.ensure_source(key, source);
    }

    #[test]
    fn default_video_view_is_paused_at_full_volume() {
        let view = VideoView::default();
        assert!(view.paused);
        assert!(!view.looping);
        assert_eq!(view.volume, 100.0);
        assert!(!view.loading);
    }

    /// The loop control is drawn before anything is playing, so a key with no
    /// session yet must still report the state a session would start in — the
    /// view is what the button reads, and it must not need a session to exist.
    #[test]
    fn a_key_without_a_session_reports_the_configured_loop_default() {
        let mut videos = manager();
        assert!(!videos.view(key(7)).looping);
        videos.set_loop_by_default(true);
        assert!(videos.view(key(7)).looping);
    }

    #[test]
    fn video_loop_default_initializes_each_session_and_can_be_overridden() {
        let mut videos = manager();
        let looping = key(8);
        let not_looping = key(9);
        videos.set_loop_by_default(true);
        ensure_source(&mut videos, looping);
        videos.set_loop_by_default(false);
        ensure_source(&mut videos, not_looping);

        assert!(videos.view(looping).looping);
        assert!(!videos.view(not_looping).looping);

        assert!(!videos.toggle_looping(looping).unwrap());
        assert!(!videos.view(looping).looping);
        assert!(videos.toggle_looping(not_looping).unwrap());
        assert!(videos.view(not_looping).looping);
    }

    #[test]
    fn playback_speed_adjustment_matches_mpv_steps_and_clamps() {
        assert_eq!(adjusted_playback_speed(1.0, 1.1), 1.1);
        assert_eq!(adjusted_playback_speed(1.0, 1.0 / 1.1), 1.0 / 1.1);
        assert_eq!(adjusted_playback_speed(4.0, 1.1), 4.0);
        assert_eq!(adjusted_playback_speed(0.25, 1.0 / 1.1), 0.25);
    }

    #[test]
    fn video_effect_values_are_independent_and_clamped_to_mpv_limits() {
        let mut effects = VideoEffectValues::default();
        effects.set(effects.adjusted(VideoEffect::Contrast, 12.0));
        effects.set(effects.adjusted(VideoEffect::Brightness, -7.0));
        effects.set(effects.adjusted(VideoEffect::Contrast, 500.0));

        assert_eq!(effects.get(VideoEffect::Contrast), 100.0);
        assert_eq!(effects.get(VideoEffect::Brightness), -7.0);
        assert_eq!(effects.get(VideoEffect::Gamma), 0.0);
        assert_eq!(effects.get(VideoEffect::Saturation), 0.0);
    }

    #[test]
    fn volume_is_shared_by_existing_and_future_video_sessions() {
        let mut videos = manager();
        let first = key(10);
        let second = key(11);
        ensure_source(&mut videos, first);

        videos.set_volume_for(first, 37.0).unwrap();
        ensure_source(&mut videos, second);

        assert_eq!(videos.view(first).volume, 37.0);
        assert_eq!(videos.view(second).volume, 37.0);
    }

    #[test]
    fn mute_restores_the_last_audible_shared_volume() {
        let mut videos = manager();
        let key = key(12);
        ensure_source(&mut videos, key);
        videos.set_volume_for(key, 64.0).unwrap();

        videos.toggle_mute(key).unwrap();
        assert_eq!(videos.view(key).volume, 0.0);
        videos.toggle_mute(key).unwrap();
        assert_eq!(videos.view(key).volume, 64.0);
    }

    #[test]
    fn shared_volume_is_clamped_to_mpv_range() {
        let mut videos = manager();
        let key = key(13);
        ensure_source(&mut videos, key);

        videos.set_volume_for(key, 180.0).unwrap();
        assert_eq!(videos.view(key).volume, 100.0);
        videos.set_volume_for(key, -20.0).unwrap();
        assert_eq!(videos.view(key).volume, 0.0);
    }

    #[test]
    fn discovering_cached_video_does_not_initialize_a_player() {
        let mut videos = manager();

        ensure_source(&mut videos, key(1));

        assert!(videos.backend.is_none());
        assert!(!videos.build_in_flight);
    }

    #[test]
    fn offscreen_transition_pauses_and_cancels_pending_start() {
        let mut videos = manager();
        let key = key(2);
        ensure_source(&mut videos, key);
        let session = videos.sessions.get_mut(&key).unwrap();
        session.visible = true;
        session.paused = false;
        videos.queued.push_back(key);
        videos.queued_keys.insert(key);

        videos.update_visibility(&HashSet::new());

        assert!(videos.sessions[&key].paused);
        assert!(!videos.queued_keys.contains(&key));
        assert!(videos.queued.is_empty());
    }

    #[test]
    fn canceled_on_demand_build_failure_does_not_restart_or_surface_an_error() {
        let mut videos = manager();
        videos.build_in_flight = true;
        videos
            .build_result_sender
            .send(BuildResult(Err("resource limit".into())))
            .unwrap();

        let drain = videos.drain();

        assert!(drain.errors.is_empty());
        assert!(!videos.build_in_flight);
    }

    #[test]
    fn removed_message_sources_are_discarded_immediately() {
        let mut videos = manager();
        let retained = key(1);
        let removed = key(2);
        ensure_source(&mut videos, retained);
        ensure_source(&mut videos, removed);

        let drain = videos.retain_sources(&HashSet::from([retained]));

        assert!(drain.changed);
        assert!(videos.sessions.contains_key(&retained));
        assert!(!videos.sessions.contains_key(&removed));
    }

    #[test]
    fn dormant_session_metadata_is_bounded() {
        let mut videos = manager();

        for message_id in 0..(MAX_SESSION_ENTRIES as u64 + 20) {
            ensure_source(&mut videos, key(message_id));
        }

        assert_eq!(videos.sessions.len(), MAX_SESSION_ENTRIES);
        assert!(
            videos
                .sessions
                .contains_key(&key(MAX_SESSION_ENTRIES as u64 + 19))
        );
    }

    #[test]
    fn scrubbing_unstarted_video_queues_paused_player_at_target() {
        let mut videos = manager();
        let key = key(3);
        ensure_source(&mut videos, key);
        videos.build_in_flight = true;
        videos.sessions.get_mut(&key).unwrap().finished = true;

        videos.scrub(key, 0.25, 120.0, SeekMode::Exact).unwrap();

        let session = &videos.sessions[&key];
        assert_eq!(session.position, 30.0);
        assert!(session.paused);
        assert!(!session.finished);
        assert!(videos.queued_keys.contains(&key));
        assert_eq!(videos.queued.front(), Some(&key));
    }

    #[test]
    fn scrub_clamps_thumbnail_duration_target_to_timeline_edges() {
        let mut videos = manager();
        let key = key(4);
        ensure_source(&mut videos, key);
        videos.build_in_flight = true;

        videos.scrub(key, 1.5, 80.0, SeekMode::Keyframes).unwrap();
        assert_eq!(videos.sessions[&key].position, 80.0);

        videos.scrub(key, -0.5, 80.0, SeekMode::Keyframes).unwrap();
        assert_eq!(videos.sessions[&key].position, 0.0);
    }
}
