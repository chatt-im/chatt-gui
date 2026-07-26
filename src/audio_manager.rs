use std::{
    collections::{HashMap, HashSet},
    sync::mpsc,
    thread,
};

use anyhow::{Result, anyhow};
use async_channel::Sender as AsyncSender;
use local_rpc::{ids::RoomId, model::AttachmentId};

use crate::{
    attachment_source::{
        AttachmentSourceKey, AttachmentSourceRegistry, RegisteredAttachmentSource,
    },
    mpv_player::{MpvAudioPlayer, PlaybackState},
};

const MAX_SESSION_ENTRIES: usize = 256;
const PLAYBACK_SPEEDS: [f64; 5] = [0.75, 1.0, 1.25, 1.5, 2.0];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct AudioKey {
    pub room_id: RoomId,
    pub message_id: u64,
    pub attachment_id: AttachmentId,
}

#[derive(Clone)]
pub(crate) struct AudioView {
    pub position: f64,
    pub duration: f64,
    pub paused: bool,
    pub finished: bool,
    pub volume: f64,
    pub playback_speed: f64,
    pub loading: bool,
    pub error: Option<String>,
}

impl Default for AudioView {
    fn default() -> Self {
        Self {
            position: 0.0,
            duration: 0.0,
            paused: true,
            finished: false,
            volume: 100.0,
            playback_speed: 1.0,
            loading: false,
            error: None,
        }
    }
}

struct AudioSession {
    source: Option<RegisteredAttachmentSource>,
    position: f64,
    duration: f64,
    paused: bool,
    finished: bool,
    ready: bool,
    visible: bool,
    touched: u64,
    error: Option<String>,
}

impl AudioSession {
    fn new(touched: u64) -> Self {
        Self {
            source: None,
            position: 0.0,
            duration: 0.0,
            paused: true,
            finished: false,
            ready: false,
            visible: false,
            touched,
            error: None,
        }
    }
}

struct BuildResult(Result<MpvAudioPlayer, String>);

#[derive(Default)]
pub(crate) struct AudioDrain {
    pub view_changed: bool,
    pub source_changed: bool,
    pub errors: Vec<String>,
    pub transport_failures: Vec<(AttachmentSourceKey, String)>,
}

/// Owns one reusable headless libmpv core and lightweight resume state for
/// audio attachments in the current room.
pub(crate) struct AttachmentAudioManager {
    sessions: HashMap<AudioKey, AudioSession>,
    active_key: Option<AudioKey>,
    active_source: Option<RegisteredAttachmentSource>,
    queued_key: Option<AudioKey>,
    player: Option<MpvAudioPlayer>,
    build_in_flight: bool,
    build_results: mpsc::Receiver<BuildResult>,
    build_result_sender: mpsc::Sender<BuildResult>,
    reaper: mpsc::Sender<MpvAudioPlayer>,
    wakeup: AsyncSender<()>,
    source_registry: AttachmentSourceRegistry,
    volume: f64,
    last_audible_volume: f64,
    playback_speed: f64,
    clock: u64,
}

impl AttachmentAudioManager {
    pub(crate) fn new(wakeup: AsyncSender<()>, source_registry: AttachmentSourceRegistry) -> Self {
        let (build_result_sender, build_results) = mpsc::channel();
        let (reaper, retired_players) = mpsc::channel::<MpvAudioPlayer>();
        if let Err(error) = thread::Builder::new()
            .name("mpv-audio-reaper".into())
            .spawn(move || {
                for player in retired_players {
                    drop(player);
                }
            })
        {
            log::error!("could not start mpv audio cleanup worker: {error}");
        }
        Self {
            sessions: HashMap::new(),
            active_key: None,
            active_source: None,
            queued_key: None,
            player: None,
            build_in_flight: false,
            build_results,
            build_result_sender,
            reaper,
            wakeup,
            source_registry,
            volume: 100.0,
            last_audible_volume: 100.0,
            playback_speed: 1.0,
            clock: 0,
        }
    }

    pub(crate) fn view(&self, key: AudioKey) -> AudioView {
        let Some(session) = self.sessions.get(&key) else {
            return AudioView {
                volume: self.volume,
                playback_speed: self.playback_speed,
                ..AudioView::default()
            };
        };
        AudioView {
            position: session.position,
            duration: session.duration,
            paused: session.paused,
            finished: session.finished,
            volume: self.volume,
            playback_speed: self.playback_speed,
            loading: self.queued_key == Some(key)
                || (self.active_key == Some(key) && !session.ready && !session.paused),
            error: session.error.clone(),
        }
    }

    /// Requests playback and optionally supplies an already-open source.
    ///
    /// Passing `None` still pauses the previous audio immediately and starts
    /// constructing the headless player while the daemon opens the source.
    pub(crate) fn play(
        &mut self,
        key: AudioKey,
        source: Option<RegisteredAttachmentSource>,
    ) -> Result<()> {
        self.ensure_session(key);
        self.touch(key);
        if self.active_key == Some(key) && self.queued_key != Some(key) {
            self.cancel_queued();
        }
        if self.active_key == Some(key)
            && let Some(player) = self.player.as_mut()
        {
            let session = self.sessions.get_mut(&key).expect("active audio session");
            session.error = None;
            if session.finished {
                let source = source
                    .or_else(|| self.active_source.clone())
                    .ok_or_else(|| anyhow!("audio source is not ready"))?;
                player.load_at(source.url(), false, self.volume, self.playback_speed, 0.0)?;
                self.active_source = Some(source);
                session.position = 0.0;
                session.duration = 0.0;
                session.paused = false;
                session.finished = false;
                session.ready = false;
            } else {
                session.paused = player.toggle_pause()?;
            }
            return Ok(());
        }

        self.cancel_queued();
        self.pause_active()?;
        let session = self.sessions.get_mut(&key).expect("ensured audio session");
        if let Some(source) = source {
            session.source = Some(source);
        }
        session.paused = false;
        session.error = None;
        self.queued_key = Some(key);
        let mut drain = AudioDrain::default();
        self.pump(&mut drain);
        if let Some(error) = drain.errors.pop() {
            return Err(anyhow!(error));
        }
        Ok(())
    }

    pub(crate) fn provide_source(
        &mut self,
        key: AudioKey,
        source: RegisteredAttachmentSource,
    ) -> AudioDrain {
        if self.queued_key != Some(key) {
            return AudioDrain::default();
        }
        self.ensure_session(key);
        self.sessions
            .get_mut(&key)
            .expect("ensured audio session")
            .source = Some(source);
        let mut drain = AudioDrain::default();
        drain.source_changed = true;
        self.pump(&mut drain);
        drain
    }

    pub(crate) fn seek(&mut self, key: AudioKey, seconds: f64) -> Result<()> {
        self.touch(key);
        let Some(session) = self.sessions.get_mut(&key) else {
            return Ok(());
        };
        let position = (session.position + seconds).clamp(0.0, session.duration.max(0.0));
        session.position = position;
        session.finished = false;
        if self.active_key == Some(key)
            && let Some(player) = self.player.as_ref()
        {
            player.seek_absolute(position)?;
        }
        Ok(())
    }

    pub(crate) fn scrub(&mut self, key: AudioKey, fraction: f64, duration_hint: f64) -> Result<()> {
        self.touch(key);
        let Some(session) = self.sessions.get_mut(&key) else {
            return Ok(());
        };
        let duration = if session.duration > 0.0 {
            session.duration
        } else {
            duration_hint.max(0.0)
        };
        if duration <= 0.0 || !duration.is_finite() {
            return Ok(());
        }
        let position = duration * fraction.clamp(0.0, 1.0);
        session.position = position;
        session.finished = false;
        if self.active_key == Some(key)
            && let Some(player) = self.player.as_ref()
        {
            player.seek_absolute(position)?;
        }
        Ok(())
    }

    pub(crate) fn set_volume_for(&mut self, key: AudioKey, volume: f64) -> Result<()> {
        self.touch(key);
        self.set_volume(volume)
    }

    pub(crate) fn toggle_mute(&mut self, key: AudioKey) -> Result<()> {
        self.touch(key);
        if self.volume > 0.0 {
            self.last_audible_volume = self.volume;
            self.set_volume(0.0)
        } else {
            self.set_volume(self.last_audible_volume.max(1.0))
        }
    }

    pub(crate) fn cycle_playback_speed(&mut self, key: AudioKey) -> Result<()> {
        self.touch(key);
        let current = PLAYBACK_SPEEDS
            .iter()
            .position(|speed| (*speed - self.playback_speed).abs() < f64::EPSILON)
            .unwrap_or(1);
        self.playback_speed = PLAYBACK_SPEEDS[(current + 1) % PLAYBACK_SPEEDS.len()];
        if let Some(player) = self.player.as_ref() {
            player.set_speed(self.playback_speed)?;
        }
        Ok(())
    }

    fn set_volume(&mut self, volume: f64) -> Result<()> {
        self.volume = volume.clamp(0.0, 100.0);
        if self.volume > 0.0 {
            self.last_audible_volume = self.volume;
        }
        if let Some(player) = self.player.as_ref() {
            player.set_volume(self.volume)?;
        }
        Ok(())
    }

    pub(crate) fn update_visibility(&mut self, visible: &HashSet<AudioKey>) -> bool {
        let mut changed = false;
        for (key, session) in &mut self.sessions {
            let is_visible = visible.contains(key);
            changed |= session.visible != is_visible;
            session.visible = is_visible;
        }
        changed
    }

    pub(crate) fn retained_source_keys(&self) -> HashSet<AttachmentSourceKey> {
        self.active_source
            .iter()
            .map(|source| source.source().key())
            .chain(
                self.queued_key
                    .and_then(|key| self.sessions.get(&key))
                    .and_then(|session| session.source.as_ref())
                    .map(|source| source.source().key()),
            )
            .collect()
    }

    pub(crate) fn drain(&mut self) -> AudioDrain {
        let mut drain = AudioDrain::default();
        while let Ok(result) = self.build_results.try_recv() {
            self.build_in_flight = false;
            match result.0 {
                Ok(player) => self.player = Some(player),
                Err(error) => {
                    if let Some(key) = self.queued_key.take()
                        && let Some(session) = self.sessions.get_mut(&key)
                    {
                        session.paused = true;
                        session.error = Some(error.clone());
                        session.source = None;
                    }
                    drain.errors.push(format!("Audio unavailable: {error}"));
                    drain.view_changed = true;
                    drain.source_changed = true;
                }
            }
        }
        self.pump(&mut drain);

        let Some(key) = self.active_key else {
            self.enforce_session_limit();
            return drain;
        };
        let Some(player) = self.player.as_mut() else {
            return drain;
        };
        match player.drain_events() {
            Ok(playback) => self.apply_playback(key, playback, &mut drain),
            Err(error) => self.fail_active_playback(key, error.to_string(), &mut drain),
        }
        self.enforce_session_limit();
        drain
    }

    pub(crate) fn retain_sources(&mut self, retained: &HashSet<AudioKey>) -> AudioDrain {
        let mut drain = AudioDrain::default();
        let sources_before = self.retained_source_keys();
        if self.queued_key.is_some_and(|key| !retained.contains(&key)) {
            self.queued_key = None;
        }
        if self.active_key.is_some_and(|key| !retained.contains(&key)) {
            if let Some(player) = self.player.as_mut()
                && let Err(error) = player.stop()
            {
                drain
                    .errors
                    .push(format!("Could not stop removed audio: {error}"));
            }
            self.active_key = None;
            self.active_source = None;
        }
        let before = self.sessions.len();
        self.sessions.retain(|key, _| retained.contains(key));
        drain.view_changed = before != self.sessions.len();
        drain.source_changed = sources_before != self.retained_source_keys();
        drain
    }

    pub(crate) fn clear_sessions(&mut self) {
        self.sessions.clear();
        self.active_key = None;
        self.active_source = None;
        self.queued_key = None;
        if let Some(player) = self.player.take() {
            let _ = self.reaper.send(player);
        }
    }

    fn ensure_session(&mut self, key: AudioKey) {
        self.clock = self.clock.wrapping_add(1);
        if !self.sessions.contains_key(&key) {
            self.trim_sessions_to(MAX_SESSION_ENTRIES.saturating_sub(1));
        }
        self.sessions
            .entry(key)
            .or_insert_with(|| AudioSession::new(self.clock));
    }

    fn pause_active(&mut self) -> Result<()> {
        let Some(key) = self.active_key else {
            return Ok(());
        };
        let Some(session) = self.sessions.get_mut(&key) else {
            return Ok(());
        };
        if !session.paused {
            if let Some(player) = self.player.as_mut() {
                player.set_paused(true)?;
            }
            session.paused = true;
        }
        Ok(())
    }

    fn cancel_queued(&mut self) {
        let Some(key) = self.queued_key.take() else {
            return;
        };
        if let Some(session) = self.sessions.get_mut(&key) {
            session.paused = true;
            session.source = None;
        }
    }

    fn pump(&mut self, drain: &mut AudioDrain) {
        let Some(key) = self.queued_key else {
            return;
        };
        let has_source = self
            .sessions
            .get(&key)
            .is_some_and(|session| session.source.is_some());
        if !has_source {
            self.start_build();
            return;
        }
        let Some(player) = self.player.as_mut() else {
            self.start_build();
            return;
        };
        let session = self.sessions.get_mut(&key).expect("queued audio session");
        let source = session.source.take().expect("checked queued audio source");
        let position = if session.finished {
            0.0
        } else {
            session.position.max(0.0)
        };
        if let Err(error) = player.load_at(
            source.url(),
            false,
            self.volume,
            self.playback_speed,
            position,
        ) {
            let error = format!("Could not open audio: {error}");
            session.paused = true;
            session.error = Some(error.clone());
            self.queued_key = None;
            drain.errors.push(error);
            drain.view_changed = true;
            drain.source_changed = true;
            return;
        }
        session.position = position;
        session.duration = 0.0;
        session.paused = false;
        session.finished = false;
        session.ready = false;
        session.error = None;
        self.active_key = Some(key);
        self.active_source = Some(source);
        self.queued_key = None;
        drain.view_changed = true;
        drain.source_changed = true;
    }

    fn apply_playback(&mut self, key: AudioKey, playback: PlaybackState, drain: &mut AudioDrain) {
        let session = self.sessions.get_mut(&key).expect("active audio session");
        let changed = session.position != playback.position
            || session.duration != playback.duration
            || session.paused != playback.paused
            || session.finished != playback.finished
            || session.ready != playback.ready;
        session.position = playback.position;
        session.duration = playback.duration;
        session.paused = playback.paused;
        session.finished = playback.finished;
        session.ready = playback.ready;
        drain.view_changed |= changed;
    }

    fn fail_active_playback(&mut self, key: AudioKey, reason: String, drain: &mut AudioDrain) {
        let error = format!("Audio playback failed: {reason}");
        if let Some(source) = self
            .active_source
            .as_ref()
            .filter(|source| source.source().has_failed())
        {
            drain
                .transport_failures
                .push((source.source().key(), error.clone()));
        }
        if let Some(player) = self.player.take() {
            let _ = self.reaper.send(player);
        }
        if let Some(session) = self.sessions.get_mut(&key) {
            session.paused = true;
            session.ready = false;
            session.error = Some(error.clone());
            session.source = None;
        }
        self.active_key = None;
        self.active_source = None;
        drain.errors.push(error);
        drain.view_changed = true;
        drain.source_changed = true;
    }

    fn start_build(&mut self) {
        if self.player.is_some() || self.build_in_flight {
            return;
        }
        self.build_in_flight = true;
        let sender = self.build_result_sender.clone();
        let wakeup = self.wakeup.clone();
        let source_registry = self.source_registry.clone();
        if let Err(error) = thread::Builder::new()
            .name("mpv-audio-builder".into())
            .spawn(move || {
                let result = MpvAudioPlayer::new_attachment(wakeup.clone(), source_registry)
                    .map_err(|error| format!("{error:#}"));
                let _ = sender.send(BuildResult(result));
                let _ = wakeup.try_send(());
            })
        {
            self.build_in_flight = false;
            let _ = self.build_result_sender.send(BuildResult(Err(format!(
                "could not start mpv audio builder: {error}"
            ))));
            let _ = self.wakeup.try_send(());
        }
    }

    fn touch(&mut self, key: AudioKey) {
        self.ensure_session(key);
        self.clock = self.clock.wrapping_add(1);
        if let Some(session) = self.sessions.get_mut(&key) {
            session.touched = self.clock;
        }
    }

    fn enforce_session_limit(&mut self) {
        self.trim_sessions_to(MAX_SESSION_ENTRIES);
    }

    fn trim_sessions_to(&mut self, limit: usize) {
        while self.sessions.len() > limit {
            let Some(key) = self
                .sessions
                .iter()
                .filter(|(key, _)| self.active_key != Some(**key) && self.queued_key != Some(**key))
                .min_by_key(|(_, session)| session.touched)
                .map(|(key, _)| *key)
            else {
                break;
            };
            self.sessions.remove(&key);
        }
    }
}

impl Drop for AttachmentAudioManager {
    fn drop(&mut self) {
        if let Some(player) = self.player.take() {
            let _ = self.reaper.send(player);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attachment_source::AttachmentSource;
    use local_rpc::{
        ids::FileTransferId,
        model::{AttachmentDescriptor, MediaKind},
    };
    use std::{io::Write, os::unix::net::UnixStream};

    fn key(message_id: u64) -> AudioKey {
        AudioKey {
            room_id: RoomId(1),
            message_id,
            attachment_id: AttachmentId {
                timestamp_ms: message_id,
                transfer_id: FileTransferId(message_id),
            },
        }
    }

    fn manager() -> AttachmentAudioManager {
        let (wakeup, _) = async_channel::bounded(1);
        AttachmentAudioManager::new(wakeup, AttachmentSourceRegistry::new(1))
    }

    fn source(manager: &AttachmentAudioManager, value: u64) -> RegisteredAttachmentSource {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"audio").unwrap();
        let descriptor = AttachmentDescriptor {
            id: key(value).attachment_id,
            file_name: format!("audio-{value}.wav"),
            media_kind: MediaKind::Audio,
            content_type: "audio/wav".into(),
            byte_len: 5,
            width: None,
            height: None,
        };
        manager.source_registry.register(AttachmentSource::direct(
            AttachmentSourceKey {
                namespace: 1,
                room_id: RoomId(1),
                attachment_id: descriptor.id,
            },
            file,
            descriptor.byte_len,
        ))
    }

    fn failed_source(manager: &AttachmentAudioManager, value: u64) -> RegisteredAttachmentSource {
        let (client, server) = UnixStream::pair().unwrap();
        drop(server);
        manager.source_registry.register(AttachmentSource::remote(
            AttachmentSourceKey {
                namespace: 1,
                room_id: RoomId(1),
                attachment_id: key(value).attachment_id,
            },
            client,
            6,
            6,
        ))
    }

    #[test]
    fn default_audio_view_is_paused_at_full_volume() {
        let audios = manager();
        let view = audios.view(key(1));
        assert!(view.paused);
        assert_eq!(view.volume, 100.0);
        assert_eq!(view.playback_speed, 1.0);
        assert!(!view.loading);
    }

    #[test]
    fn requesting_another_audio_pauses_the_current_session_immediately() {
        let mut audios = manager();
        let first = key(1);
        audios.ensure_session(first);
        audios.active_key = Some(first);
        audios.sessions.get_mut(&first).unwrap().paused = false;

        audios.play(key(2), None).unwrap();

        assert!(audios.sessions[&first].paused);
        assert_eq!(audios.queued_key, Some(key(2)));
    }

    #[test]
    fn inactive_audio_scrub_updates_its_resume_position() {
        let mut audios = manager();
        let key = key(3);
        audios.ensure_session(key);
        audios.sessions.get_mut(&key).unwrap().duration = 80.0;

        audios.scrub(key, 0.25, 0.0).unwrap();

        assert_eq!(audios.sessions[&key].position, 20.0);
    }

    #[test]
    fn audio_continues_when_it_becomes_invisible() {
        let mut audios = manager();
        let key = key(4);
        audios.ensure_session(key);
        audios.sessions.get_mut(&key).unwrap().paused = false;
        audios.sessions.get_mut(&key).unwrap().visible = true;

        audios.update_visibility(&HashSet::new());

        assert!(!audios.sessions[&key].paused);
        assert!(!audios.sessions[&key].visible);
    }

    #[test]
    fn mute_restores_last_audible_audio_volume() {
        let mut audios = manager();
        let key = key(5);
        audios.set_volume_for(key, 63.0).unwrap();
        audios.toggle_mute(key).unwrap();
        assert_eq!(audios.view(key).volume, 0.0);
        audios.toggle_mute(key).unwrap();
        assert_eq!(audios.view(key).volume, 63.0);
    }

    #[test]
    fn switching_audio_preserves_each_saved_resume_position() {
        let mut audios = manager();
        let first = key(7);
        audios.ensure_session(first);
        audios.sessions.get_mut(&first).unwrap().position = 12.5;
        audios.sessions.get_mut(&first).unwrap().duration = 30.0;
        audios.sessions.get_mut(&first).unwrap().paused = false;
        audios.active_key = Some(first);

        audios.play(key(8), None).unwrap();
        audios.play(first, None).unwrap();

        assert_eq!(audios.sessions[&first].position, 12.5);
    }

    #[test]
    fn audio_volume_is_clamped_to_mpv_range() {
        let mut audios = manager();
        let key = key(9);
        audios.set_volume_for(key, 140.0).unwrap();
        assert_eq!(audios.view(key).volume, 100.0);
        audios.set_volume_for(key, -20.0).unwrap();
        assert_eq!(audios.view(key).volume, 0.0);
    }

    #[test]
    fn playback_speed_cycles_through_supported_rates() {
        let mut audios = manager();
        let key = key(16);

        for expected in [1.25, 1.5, 2.0, 0.75, 1.0] {
            audios.cycle_playback_speed(key).unwrap();
            assert_eq!(audios.view(key).playback_speed, expected);
        }
    }

    #[test]
    fn removed_active_audio_is_stopped_and_discarded() {
        let mut audios = manager();
        let key = key(10);
        audios.ensure_session(key);
        audios.active_key = Some(key);

        let drain = audios.retain_sources(&HashSet::new());

        assert!(drain.view_changed);
        assert!(audios.active_key.is_none());
        assert!(!audios.sessions.contains_key(&key));
    }

    #[test]
    fn dormant_audio_session_metadata_is_bounded() {
        let mut audios = manager();
        for message_id in 0..(MAX_SESSION_ENTRIES as u64 + 20) {
            audios.ensure_session(key(message_id));
        }
        assert_eq!(audios.sessions.len(), MAX_SESSION_ENTRIES);
        assert!(
            audios
                .sessions
                .contains_key(&key(MAX_SESSION_ENTRIES as u64 + 19))
        );
    }

    #[test]
    fn player_build_failure_becomes_a_retryable_session_error() {
        let mut audios = manager();
        let key = key(11);
        audios.ensure_session(key);
        audios.queued_key = Some(key);
        audios.build_in_flight = true;
        audios
            .build_result_sender
            .send(BuildResult(Err("audio device unavailable".into())))
            .unwrap();

        let drain = audios.drain();

        assert!(!drain.errors.is_empty());
        assert!(audios.sessions[&key].error.is_some());
        assert!(audios.queued_key.is_none());
    }

    #[test]
    fn playback_progress_does_not_report_source_lifecycle_work() {
        let mut audios = manager();
        let key = key(14);
        audios.ensure_session(key);
        let mut drain = AudioDrain::default();

        audios.apply_playback(
            key,
            PlaybackState {
                position: 4.0,
                duration: 20.0,
                paused: true,
                ..PlaybackState::default()
            },
            &mut drain,
        );

        assert!(drain.view_changed);
        assert!(!drain.source_changed);
        assert!(drain.transport_failures.is_empty());
    }

    #[test]
    fn failed_attachment_reads_invalidate_the_exact_cached_source() {
        let mut audios = manager();
        let key = key(15);
        let source = failed_source(&audios, 15);
        let source_key = source.source().key();
        let mut bytes = [0; 6];
        assert!(source.source().read_at(0, &mut bytes).is_err());
        audios.ensure_session(key);
        audios.active_key = Some(key);
        audios.active_source = Some(source);
        let mut drain = AudioDrain::default();

        audios.fail_active_playback(key, "mpv could not read media".into(), &mut drain);

        assert_eq!(drain.transport_failures.len(), 1);
        assert_eq!(drain.transport_failures[0].0, source_key);
        assert!(drain.source_changed);
        assert!(audios.active_source.is_none());
    }

    #[test]
    fn queued_source_is_pinned_until_playback_starts() {
        let mut audios = manager();
        let key = key(6);
        let source = source(&audios, 6);
        let source_key = source.source().key();
        audios.ensure_session(key);
        audios.sessions.get_mut(&key).unwrap().source = Some(source);
        audios.queued_key = Some(key);

        assert_eq!(audios.retained_source_keys(), HashSet::from([source_key]));
    }

    #[test]
    fn stale_opened_source_is_not_retained_after_another_audio_was_requested() {
        let mut audios = manager();
        let stale = key(12);
        audios.ensure_session(stale);
        audios.queued_key = Some(key(13));

        audios.provide_source(stale, source(&audios, 12));

        assert!(audios.sessions[&stale].source.is_none());
    }
}
