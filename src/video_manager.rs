use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::PathBuf,
    sync::mpsc,
    thread,
};

use anyhow::{Result, anyhow};
use async_channel::Sender as AsyncSender;
use gpui::PlatformSurface;
use local_rpc::{ids::RoomId, model::AttachmentId};

use crate::mpv_player::{AttachmentRenderBackend, MpvPlayer, SeekMode};

const WARM_PLAYER_TARGET: usize = 2;
const RETAINED_OFFSCREEN_LIMIT: usize = 4;
const MAX_SESSION_ENTRIES: usize = 256;

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
    pub volume: f64,
    pub loading: bool,
    pub error: Option<String>,
}

impl Default for VideoView {
    fn default() -> Self {
        Self {
            surface: None,
            position: 0.0,
            duration: 0.0,
            paused: true,
            finished: false,
            volume: 100.0,
            loading: false,
            error: None,
        }
    }
}

struct VideoSession {
    path: PathBuf,
    player: Option<MpvPlayer>,
    position: f64,
    duration: f64,
    paused: bool,
    finished: bool,
    frame_ready: bool,
    volume: f64,
    visible: bool,
    touched: u64,
    error: Option<String>,
}

impl VideoSession {
    fn new(path: PathBuf, touched: u64) -> Self {
        Self {
            path,
            player: None,
            position: 0.0,
            duration: 0.0,
            paused: true,
            finished: false,
            frame_ready: false,
            volume: 100.0,
            visible: false,
            touched,
            error: None,
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
/// decision and retaining a small number of initialized libmpv cores.
pub(crate) struct AttachmentVideoManager {
    sessions: HashMap<VideoKey, VideoSession>,
    standby: Vec<MpvPlayer>,
    queued: VecDeque<VideoKey>,
    queued_keys: HashSet<VideoKey>,
    backend: Option<AttachmentRenderBackend>,
    build_in_flight: bool,
    warm_build_suppressed: bool,
    build_results: mpsc::Receiver<BuildResult>,
    build_result_sender: mpsc::Sender<BuildResult>,
    reaper: mpsc::Sender<MpvPlayer>,
    wakeup: AsyncSender<()>,
    last_interacted: Option<VideoKey>,
    clock: u64,
}

impl AttachmentVideoManager {
    pub(crate) fn new(wakeup: AsyncSender<()>) -> Self {
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
            log::error!("could not start mpv cleanup worker: {error}");
        }
        Self {
            sessions: HashMap::new(),
            standby: Vec::new(),
            queued: VecDeque::new(),
            queued_keys: HashSet::new(),
            backend: None,
            build_in_flight: false,
            warm_build_suppressed: false,
            build_results,
            build_result_sender,
            reaper,
            wakeup,
            last_interacted: None,
            clock: 0,
        }
    }

    pub(crate) fn ensure_source(&mut self, key: VideoKey, path: PathBuf) {
        self.clock = self.clock.wrapping_add(1);
        if !self.sessions.contains_key(&key) {
            self.trim_sessions_to(MAX_SESSION_ENTRIES.saturating_sub(1));
        }
        let session = self
            .sessions
            .entry(key)
            .or_insert_with(|| VideoSession::new(path.clone(), self.clock));
        session.path = path;
        session.touched = self.clock;
    }

    pub(crate) fn view(&self, key: VideoKey) -> VideoView {
        let Some(session) = self.sessions.get(&key) else {
            return VideoView::default();
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
            volume: session.volume,
            loading: self.queued_keys.contains(&key)
                || (session.player.is_some() && !session.frame_ready && !session.paused),
            error: session.error.clone(),
        }
    }

    pub(crate) fn play(&mut self, key: VideoKey) -> Result<()> {
        self.touch(key);
        self.last_interacted = Some(key);
        let Some(session) = self.sessions.get_mut(&key) else {
            return Err(anyhow!("video source is no longer cached"));
        };
        session.error = None;
        if let Some(player) = session.player.as_mut() {
            if session.finished {
                player.load_at(&session.path.to_string_lossy(), false, session.volume, 0.0)?;
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
        let mut drain = VideoDrain::default();
        self.pump_builds(&mut drain);
        if let Some(error) = drain.errors.pop() {
            return Err(anyhow!(error));
        }
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
        let mut drain = VideoDrain::default();
        self.pump_builds(&mut drain);
        if let Some(error) = drain.errors.pop() {
            return Err(anyhow!(error));
        }
        Ok(())
    }

    pub(crate) fn adjust_volume(&mut self, key: VideoKey, delta: f64) -> Result<()> {
        self.touch(key);
        self.last_interacted = Some(key);
        let Some(session) = self.sessions.get_mut(&key) else {
            return Ok(());
        };
        session.volume = (session.volume + delta).clamp(0.0, 100.0);
        if let Some(player) = session.player.as_ref() {
            player.set_volume(session.volume)?;
        }
        Ok(())
    }

    pub(crate) fn toggle_last_visible(&mut self) -> Result<bool> {
        let Some(key) = self.last_visible_interaction() else {
            return Ok(false);
        };
        self.play(key)?;
        Ok(true)
    }

    pub(crate) fn seek_last_visible(&mut self, seconds: f64) -> Result<bool> {
        let Some(key) = self.last_visible_interaction() else {
            return Ok(false);
        };
        self.seek(key, seconds)?;
        Ok(true)
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
        }
        self.queued.retain(|key| self.queued_keys.contains(key));
        self.enforce_retained_limit(&mut drain);
        self.enforce_session_limit(&mut drain);
        self.pump_builds(&mut drain);
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
                    self.warm_build_suppressed = false;
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
                        self.warm_build_suppressed = true;
                        log::warn!(
                            "could not replenish warm video player pool; retrying on demand: {error}"
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
                        || session.frame_ready != playback.frame_ready;
                    session.position = playback.position;
                    session.duration = playback.duration;
                    session.paused = playback.paused;
                    session.finished = playback.finished;
                    session.frame_ready = playback.frame_ready;
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
                session.error = Some(error.clone());
            }
            drain.errors.push(error);
            drain.changed = true;
        }
        self.enforce_session_limit(&mut drain);
        self.pump_builds(&mut drain);
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
        self.pump_builds(&mut drain);
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
        let mut drain = VideoDrain::default();
        self.pump_builds(&mut drain);
        for error in drain.errors {
            log::error!("{error}");
        }
    }

    fn assign_player(&mut self, player: MpvPlayer, drain: &mut VideoDrain) {
        let Some(key) = self.pop_queued() else {
            self.standby.push(player);
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

    fn pump_builds(&mut self, drain: &mut VideoDrain) {
        if self.build_in_flight {
            return;
        }
        while let Some(player) = self.standby.pop() {
            let Some(key) = self.pop_queued() else {
                self.standby.push(player);
                break;
            };
            self.assign_specific_player(key, player, drain);
        }

        let needs_queued_player = !self.queued_keys.is_empty();
        let needs_warm_player = self.backend.is_some()
            && !self.warm_build_suppressed
            && self.standby.len() < WARM_PLAYER_TARGET;
        if !needs_queued_player && !needs_warm_player {
            return;
        }
        self.build_in_flight = true;
        let sender = self.build_result_sender.clone();
        let wakeup = self.wakeup.clone();
        let preferred_backend = self.backend.clone();
        if let Err(error) = thread::Builder::new()
            .name("mpv-builder".into())
            .spawn(move || {
                log::info!(
                    "asynchronous video player build started cached_backend={}",
                    preferred_backend.is_some()
                );
                let result = MpvPlayer::new_attachment(wakeup.clone(), preferred_backend)
                    .map_err(|error| format!("{error:#}"));
                match &result {
                    Ok(_) => log::info!("asynchronous video player build completed"),
                    Err(error) => {
                        log::error!("asynchronous video player build failed: {error}")
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
            self.standby.push(player);
            return;
        };
        if let Err(error) = player.load_at(
            &session.path.to_string_lossy(),
            session.paused,
            session.volume,
            session.position,
        ) {
            let error = format!("Could not open video: {error}");
            session.paused = true;
            session.error = Some(error.clone());
            drain.errors.push(error);
            let _ = self.reaper.send(player);
            return;
        }
        session.frame_ready = false;
        session.finished = false;
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

    fn recycle(&mut self, mut player: MpvPlayer) {
        if let Err(error) = player.stop() {
            log::warn!("could not stop retained video player: {error}");
            let _ = self.reaper.send(player);
        } else if self.standby.len() < WARM_PLAYER_TARGET {
            self.standby.push(player);
        } else {
            let _ = self.reaper.send(player);
        }
    }

    fn touch(&mut self, key: VideoKey) {
        self.clock = self.clock.wrapping_add(1);
        if let Some(session) = self.sessions.get_mut(&key) {
            session.touched = self.clock;
        }
    }

    fn last_visible_interaction(&self) -> Option<VideoKey> {
        self.last_interacted.filter(|key| {
            self.sessions
                .get(key)
                .is_some_and(|session| session.visible)
        })
    }
}

impl Drop for AttachmentVideoManager {
    fn drop(&mut self) {
        for (_, mut session) in self.sessions.drain() {
            if let Some(player) = session.player.take() {
                let _ = self.reaper.send(player);
            }
        }
        for player in self.standby.drain(..) {
            let _ = self.reaper.send(player);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn default_video_view_is_paused_at_full_volume() {
        let view = VideoView::default();
        assert!(view.paused);
        assert_eq!(view.volume, 100.0);
        assert!(!view.loading);
    }

    #[test]
    fn discovering_cached_video_does_not_initialize_a_player() {
        let (wakeup, _) = async_channel::bounded(1);
        let mut videos = AttachmentVideoManager::new(wakeup);

        videos.ensure_source(key(1), PathBuf::from("video.mp4"));

        assert!(videos.backend.is_none());
        assert!(videos.standby.is_empty());
        assert!(!videos.build_in_flight);
    }

    #[test]
    fn offscreen_transition_pauses_and_cancels_pending_start() {
        let (wakeup, _) = async_channel::bounded(1);
        let mut videos = AttachmentVideoManager::new(wakeup);
        let key = key(2);
        videos.ensure_source(key, PathBuf::from("video.mp4"));
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
    fn failed_warm_replenishment_waits_for_real_demand() {
        let (wakeup, _) = async_channel::bounded(1);
        let mut videos = AttachmentVideoManager::new(wakeup);
        videos.backend = Some(AttachmentRenderBackend::Software);
        videos.build_in_flight = true;
        videos
            .build_result_sender
            .send(BuildResult(Err("resource limit".into())))
            .unwrap();

        let drain = videos.drain();

        assert!(drain.errors.is_empty());
        assert!(videos.warm_build_suppressed);
        assert!(!videos.build_in_flight);
    }

    #[test]
    fn removed_message_sources_are_discarded_immediately() {
        let (wakeup, _) = async_channel::bounded(1);
        let mut videos = AttachmentVideoManager::new(wakeup);
        let retained = key(1);
        let removed = key(2);
        videos.ensure_source(retained, PathBuf::from("retained.mp4"));
        videos.ensure_source(removed, PathBuf::from("removed.mp4"));

        let drain = videos.retain_sources(&HashSet::from([retained]));

        assert!(drain.changed);
        assert!(videos.sessions.contains_key(&retained));
        assert!(!videos.sessions.contains_key(&removed));
    }

    #[test]
    fn dormant_session_metadata_is_bounded() {
        let (wakeup, _) = async_channel::bounded(1);
        let mut videos = AttachmentVideoManager::new(wakeup);

        for message_id in 0..(MAX_SESSION_ENTRIES as u64 + 20) {
            videos.ensure_source(key(message_id), PathBuf::from("video.mp4"));
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
        let (wakeup, _) = async_channel::bounded(1);
        let mut videos = AttachmentVideoManager::new(wakeup);
        let key = key(3);
        videos.ensure_source(key, PathBuf::from("video.mp4"));
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
        let (wakeup, _) = async_channel::bounded(1);
        let mut videos = AttachmentVideoManager::new(wakeup);
        let key = key(4);
        videos.ensure_source(key, PathBuf::from("video.mp4"));
        videos.build_in_flight = true;

        videos.scrub(key, 1.5, 80.0, SeekMode::Keyframes).unwrap();
        assert_eq!(videos.sessions[&key].position, 80.0);

        videos.scrub(key, -0.5, 80.0, SeekMode::Keyframes).unwrap();
        assert_eq!(videos.sessions[&key].position, 0.0);
    }
}
