use std::time::{Duration, Instant};

use gpui::{Bounds, Pixels, ScrollDelta};

use crate::video_manager::VideoKey;

pub(crate) const CONTROLS_HIDE_DELAY: Duration = Duration::from_secs(2);
pub(crate) const CONTROLS_ANIMATION_DURATION: Duration = Duration::from_millis(140);
pub(crate) const VOLUME_ANIMATION_DURATION: Duration = Duration::from_millis(100);
pub(crate) const SCRUB_SEEK_INTERVAL: Duration = Duration::from_millis(16);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ControlsPhase {
    #[default]
    Hidden,
    Showing(u64),
    Visible,
    Hiding(u64),
}

impl ControlsPhase {
    pub(crate) fn rendered(self) -> bool {
        !matches!(self, Self::Hidden)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct VideoControlsState {
    pub active_key: Option<VideoKey>,
    pub phase: ControlsPhase,
    pub player_hovered: bool,
    pub bar_hovered: bool,
    pub volume_open: bool,
    pub volume_button_hovered: bool,
    pub volume_popup_hovered: bool,
    pub scrub_hover_fraction: Option<f64>,
    animation_serial: u64,
}

impl VideoControlsState {
    pub(crate) fn activate(&mut self, key: VideoKey) {
        if self.active_key == Some(key) {
            return;
        }
        self.active_key = Some(key);
        self.player_hovered = false;
        self.bar_hovered = false;
        self.volume_open = false;
        self.volume_button_hovered = false;
        self.volume_popup_hovered = false;
        self.scrub_hover_fraction = None;
        self.phase = ControlsPhase::Hidden;
    }

    pub(crate) fn show(&mut self, key: VideoKey) -> Option<u64> {
        self.activate(key);
        if matches!(
            self.phase,
            ControlsPhase::Visible | ControlsPhase::Showing(_)
        ) {
            return None;
        }
        self.animation_serial = self.animation_serial.wrapping_add(1);
        self.phase = ControlsPhase::Showing(self.animation_serial);
        Some(self.animation_serial)
    }

    pub(crate) fn hide(&mut self, key: VideoKey) -> Option<u64> {
        if self.active_key != Some(key)
            || matches!(self.phase, ControlsPhase::Hidden | ControlsPhase::Hiding(_))
        {
            return None;
        }
        self.animation_serial = self.animation_serial.wrapping_add(1);
        self.phase = ControlsPhase::Hiding(self.animation_serial);
        self.volume_open = false;
        self.volume_button_hovered = false;
        self.volume_popup_hovered = false;
        self.scrub_hover_fraction = None;
        Some(self.animation_serial)
    }

    pub(crate) fn finish_animation(&mut self, serial: u64) -> bool {
        match self.phase {
            ControlsPhase::Showing(active) if active == serial => {
                self.phase = ControlsPhase::Visible;
                true
            }
            ControlsPhase::Hiding(active) if active == serial => {
                self.phase = ControlsPhase::Hidden;
                true
            }
            _ => false,
        }
    }

    pub(crate) fn pinned(&self, key: VideoKey, paused: bool, ended: bool, dragging: bool) -> bool {
        self.active_key == Some(key)
            && (paused || ended || dragging || self.bar_hovered || self.volume_open)
    }

    pub(crate) fn clear(&mut self) {
        *self = Self::default();
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct VideoScrub {
    pub key: VideoKey,
    pub bounds: Bounds<Pixels>,
    pub duration: f64,
    pub last_fraction: f64,
    pub last_seek: Instant,
}

impl VideoScrub {
    pub(crate) fn position(self) -> f64 {
        self.duration * self.last_fraction
    }

    pub(crate) fn should_dispatch_seek(&mut self, now: Instant) -> bool {
        if now.saturating_duration_since(self.last_seek) < SCRUB_SEEK_INTERVAL {
            return false;
        }
        self.last_seek = now;
        true
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct VideoVolumeDrag {
    pub key: VideoKey,
    pub bounds: Bounds<Pixels>,
}

pub(crate) fn horizontal_fraction(
    bounds: Bounds<Pixels>,
    pointer_x: Pixels,
    duration: f64,
) -> Option<f64> {
    let width = bounds.size.width.as_f32();
    if width <= 0.0 || duration <= 0.0 || !duration.is_finite() {
        return None;
    }
    Some(((pointer_x - bounds.origin.x).as_f32() / width).clamp(0.0, 1.0) as f64)
}

pub(crate) fn vertical_fraction(bounds: Bounds<Pixels>, pointer_y: Pixels) -> Option<f64> {
    let height = bounds.size.height.as_f32();
    if height <= 0.0 {
        return None;
    }
    Some((1.0 - (pointer_y - bounds.origin.y).as_f32() / height).clamp(0.0, 1.0) as f64)
}

pub(crate) fn volume_scroll_delta(delta: ScrollDelta) -> f64 {
    match delta {
        ScrollDelta::Lines(delta) => f64::from(delta.y) * 5.0,
        ScrollDelta::Pixels(delta) => f64::from(delta.y.as_f32()) * 0.12,
    }
    .clamp(-20.0, 20.0)
}

#[cfg(test)]
mod tests {
    use gpui::{Bounds, point, px, size};
    use local_rpc::{
        ids::{FileTransferId, RoomId},
        model::AttachmentId,
    };

    use super::*;

    fn key(message_id: u64) -> VideoKey {
        VideoKey {
            room_id: RoomId(1),
            message_id,
            attachment_id: AttachmentId {
                timestamp_ms: message_id,
                transfer_id: FileTransferId(message_id),
            },
        }
    }

    #[test]
    fn horizontal_scrub_fraction_maps_and_clamps_coordinates() {
        let bounds = Bounds::new(point(px(100.0), px(0.0)), size(px(400.0), px(20.0)));
        assert_eq!(horizontal_fraction(bounds, px(100.0), 60.0), Some(0.0));
        assert_eq!(horizontal_fraction(bounds, px(300.0), 60.0), Some(0.5));
        assert_eq!(horizontal_fraction(bounds, px(900.0), 60.0), Some(1.0));
        assert_eq!(horizontal_fraction(bounds, px(300.0), 0.0), None);
    }

    #[test]
    fn vertical_volume_fraction_has_full_volume_at_the_top() {
        let bounds = Bounds::new(point(px(0.0), px(20.0)), size(px(10.0), px(80.0)));
        assert_eq!(vertical_fraction(bounds, px(20.0)), Some(1.0));
        assert_eq!(vertical_fraction(bounds, px(60.0)), Some(0.5));
        assert_eq!(vertical_fraction(bounds, px(120.0)), Some(0.0));
    }

    #[test]
    fn stale_control_animation_cannot_finish_a_newer_transition() {
        let key = key(1);
        let mut controls = VideoControlsState::default();
        let showing = controls.show(key).unwrap();
        let hiding = controls.hide(key).unwrap();

        assert!(!controls.finish_animation(showing));
        assert!(controls.finish_animation(hiding));
        assert_eq!(controls.phase, ControlsPhase::Hidden);
    }

    #[test]
    fn activating_another_video_drops_transient_popup_and_hover_state() {
        let mut controls = VideoControlsState::default();
        controls.activate(key(1));
        controls.volume_open = true;
        controls.volume_button_hovered = true;
        controls.scrub_hover_fraction = Some(0.4);

        controls.activate(key(2));

        assert!(!controls.volume_open);
        assert!(!controls.volume_button_hovered);
        assert_eq!(controls.scrub_hover_fraction, None);
        assert_eq!(controls.phase, ControlsPhase::Hidden);
    }

    #[test]
    fn paused_dragging_and_control_hover_pin_controls() {
        let key = key(2);
        let mut controls = VideoControlsState::default();
        controls.activate(key);
        assert!(controls.pinned(key, true, false, false));
        assert!(controls.pinned(key, false, false, true));
        controls.bar_hovered = true;
        assert!(controls.pinned(key, false, false, false));
    }

    #[test]
    fn volume_scroll_supports_wheels_and_precise_trackpads() {
        assert_eq!(
            volume_scroll_delta(ScrollDelta::Lines(point(0.0, 1.0))),
            5.0
        );
        assert_eq!(
            volume_scroll_delta(ScrollDelta::Pixels(point(px(0.0), px(-50.0)))),
            -6.0
        );
        assert_eq!(
            volume_scroll_delta(ScrollDelta::Pixels(point(px(0.0), px(1_000.0)))),
            20.0
        );
    }

    #[test]
    fn drag_seek_dispatch_is_coalesced_but_becomes_ready_again() {
        let now = Instant::now();
        let mut scrub = VideoScrub {
            key: key(3),
            bounds: Bounds::new(point(px(0.0), px(0.0)), size(px(100.0), px(20.0))),
            duration: 60.0,
            last_fraction: 0.25,
            last_seek: now,
        };

        assert!(!scrub.should_dispatch_seek(now + Duration::from_millis(8)));
        assert!(scrub.should_dispatch_seek(now + SCRUB_SEEK_INTERVAL));
        assert!(!scrub.should_dispatch_seek(now + Duration::from_millis(20)));
    }
}
