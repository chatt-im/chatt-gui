use std::{cell::Cell, rc::Rc, sync::Arc};

use gpui::{
    App, Bounds, Div, ElementId, MouseButton, MouseDownEvent, Pixels, Stateful, Window, canvas,
    div, prelude::*, px, relative,
};

use crate::{
    audio_manager::{AudioKey, AudioView},
    icons::{IconName, icon},
    media_controls::format_time,
    theme::{ResolvedSettings, ThemeRole},
};

pub(crate) enum AudioPlayerEvent {
    Play,
    ScrubPressed {
        bounds: Bounds<Pixels>,
        event: MouseDownEvent,
    },
    CycleSpeed,
    ToggleMute,
    VolumePressed {
        bounds: Bounds<Pixels>,
        event: MouseDownEvent,
    },
}

pub(crate) type AudioPlayerHandler = Rc<dyn Fn(AudioPlayerEvent, &mut Window, &mut App) + 'static>;

pub(crate) struct AudioPlayerConfig {
    pub key: AudioKey,
    pub audio: AudioView,
    pub duration: f64,
    pub display_position: f64,
}

pub(crate) fn render_audio_player(
    config: AudioPlayerConfig,
    handler: AudioPlayerHandler,
    settings: Arc<ResolvedSettings>,
) -> Stateful<Div> {
    let AudioPlayerConfig {
        key,
        audio,
        duration,
        display_position,
    } = config;
    let player_id = ElementId::from(format!(
        "audio-{}-{}-{}-{}",
        key.room_id.0,
        key.message_id,
        key.attachment_id.timestamp_ms,
        key.attachment_id.transfer_id.0,
    ));
    let progress = if duration > 0.0 && duration.is_finite() {
        (display_position / duration).clamp(0.0, 1.0) as f32
    } else {
        0.0
    };
    let volume_fraction = (audio.volume / 100.0).clamp(0.0, 1.0) as f32;
    let scrub_bounds = Rc::new(Cell::new(None));
    let measured_scrub_bounds = scrub_bounds.clone();
    let down_scrub_bounds = scrub_bounds.clone();
    let volume_bounds = Rc::new(Cell::new(None));
    let measured_volume_bounds = volume_bounds.clone();
    let down_volume_bounds = volume_bounds.clone();

    let play = handler.clone();
    let play_button = audio_control_button(
        (player_id.clone(), "play"),
        if !audio.paused && !audio.finished {
            IconName::Pause
        } else {
            IconName::Play
        },
        &settings,
    )
    .opacity(if audio.loading { 0.55 } else { 1.0 })
    .when(!audio.loading, |button| {
        button.on_click(move |_, window, cx| {
            cx.stop_propagation();
            play(AudioPlayerEvent::Play, window, cx)
        })
    });

    let scrub = handler.clone();
    let timeline = div()
        .id((player_id.clone(), "timeline"))
        .relative()
        .min_w(px(80.0))
        .flex_1()
        .h(px(24.0))
        .flex()
        .items_center()
        .when(duration > 0.0 && duration.is_finite(), |timeline| {
            timeline
                .cursor_pointer()
                .on_mouse_down(MouseButton::Left, move |event, window, cx| {
                    if let Some(bounds) = down_scrub_bounds.get() {
                        scrub(
                            AudioPlayerEvent::ScrubPressed {
                                bounds,
                                event: event.clone(),
                            },
                            window,
                            cx,
                        );
                    }
                })
        })
        .child(
            canvas(
                move |bounds, _, _| measured_scrub_bounds.set(Some(bounds)),
                |_, _, _, _| {},
            )
            .absolute()
            .size_full(),
        )
        .child(
            div()
                .relative()
                .w_full()
                .h(px(3.0))
                .rounded_full()
                .bg(settings.theme.color(ThemeRole::MediaProgressTrack))
                .child(
                    div()
                        .h_full()
                        .w(relative(progress))
                        .rounded_full()
                        .bg(settings.theme.color(ThemeRole::MediaProgressFill)),
                )
                .when(duration > 0.0, |track| {
                    track.child(
                        div()
                            .absolute()
                            .left(relative(progress))
                            .ml(px(-4.0))
                            .top(px(-2.5))
                            .size(px(8.0))
                            .rounded_full()
                            .bg(settings.theme.color(ThemeRole::MediaProgressKnob)),
                    )
                }),
        );

    let mute = handler.clone();
    let volume_icon = if audio.volume <= 0.0 {
        IconName::VolumeMuted
    } else if audio.volume <= 50.0 {
        IconName::VolumeLow
    } else {
        IconName::VolumeHigh
    };
    let volume_down = handler.clone();
    let volume = div()
        .id((player_id.clone(), "volume-slider"))
        .relative()
        .w(px(64.0))
        .h(px(30.0))
        .flex_none()
        .flex()
        .items_center()
        .cursor_pointer()
        .on_mouse_down(MouseButton::Left, move |event, window, cx| {
            if let Some(bounds) = down_volume_bounds.get() {
                volume_down(
                    AudioPlayerEvent::VolumePressed {
                        bounds,
                        event: event.clone(),
                    },
                    window,
                    cx,
                );
            }
        })
        .child(
            canvas(
                move |bounds, _, _| measured_volume_bounds.set(Some(bounds)),
                |_, _, _, _| {},
            )
            .absolute()
            .size_full(),
        )
        .child(
            div()
                .relative()
                .w_full()
                .h(px(3.0))
                .rounded_full()
                .bg(settings.theme.color(ThemeRole::MediaProgressTrack))
                .child(
                    div()
                        .h_full()
                        .w(relative(volume_fraction))
                        .rounded_full()
                        .bg(settings.theme.color(ThemeRole::MediaProgressFill)),
                )
                .child(
                    div()
                        .absolute()
                        .left(relative(volume_fraction))
                        .ml(px(-4.0))
                        .top(px(-2.5))
                        .size(px(8.0))
                        .rounded_full()
                        .bg(settings.theme.color(ThemeRole::MediaProgressKnob)),
                ),
        );

    let total = if duration > 0.0 && duration.is_finite() {
        format_time(duration)
    } else {
        "--:--".into()
    };
    let status = audio
        .error
        .as_deref()
        .map(str::to_owned)
        .or_else(|| audio.loading.then(|| "Starting audio…".to_string()));
    let cycle_speed = handler.clone();
    let speed_button = div()
        .id((player_id.clone(), "speed"))
        .h(px(28.0))
        .min_w(px(44.0))
        .px_2()
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded_sm()
        .cursor_pointer()
        .text_xs()
        .text_color(settings.theme.color(ThemeRole::TextSecondary))
        .bg(settings.theme.color(ThemeRole::ControlSurface))
        .hover({
            let hover = settings.theme.color(ThemeRole::ControlSurfaceHover);
            move |button| button.bg(hover)
        })
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(move |_, window, cx| {
            cx.stop_propagation();
            cycle_speed(AudioPlayerEvent::CycleSpeed, window, cx)
        })
        .child(format_playback_speed(audio.playback_speed));

    div()
        .id(player_id.clone())
        .mt_2()
        .w_full()
        .px_3()
        .py_2()
        .flex()
        .flex_col()
        .gap_1()
        .rounded_sm()
        .border_1()
        .border_color(settings.theme.color(ThemeRole::MediaBorder))
        .bg(settings.theme.color(ThemeRole::Panel))
        .child(
            div()
                .w_full()
                .min_w_0()
                .flex()
                .items_center()
                .gap_2()
                .child(play_button)
                .child(timeline)
                .child(
                    div()
                        .min_w(px(74.0))
                        .flex_none()
                        .text_right()
                        .text_xs()
                        .text_color(settings.theme.color(ThemeRole::TextMuted))
                        .child(format!("{} / {total}", format_time(display_position))),
                )
                .child(speed_button)
                .child(
                    div()
                        .flex_none()
                        .flex()
                        .items_center()
                        .gap_1()
                        .child(
                            audio_control_button((player_id, "mute"), volume_icon, &settings)
                                .on_click(move |_, window, cx| {
                                    cx.stop_propagation();
                                    mute(AudioPlayerEvent::ToggleMute, window, cx)
                                }),
                        )
                        .child(volume),
                ),
        )
        .when_some(status, |player, status| {
            player.child(
                div()
                    .w_full()
                    .min_w_0()
                    .pl(px(40.0))
                    .text_xs()
                    .text_color(settings.theme.color(if audio.error.is_some() {
                        ThemeRole::StateDanger
                    } else {
                        ThemeRole::TextMuted
                    }))
                    .child(status),
            )
        })
}

fn format_playback_speed(speed: f64) -> String {
    if speed.fract().abs() < f64::EPSILON {
        format!("{speed:.0}×")
    } else {
        format!("{speed:.2}").trim_end_matches('0').to_owned() + "×"
    }
}

fn audio_control_button(
    id: impl Into<gpui::ElementId>,
    icon_name: IconName,
    settings: &ResolvedSettings,
) -> Stateful<Div> {
    div()
        .id(id)
        .size(px(30.0))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded_sm()
        .cursor_pointer()
        .hover({
            let hover = settings.theme.color(ThemeRole::ControlSurfaceHover);
            move |button| button.bg(hover)
        })
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .child(icon(
            icon_name,
            16.0,
            settings.theme.color(ThemeRole::TextSecondary),
        ))
}

#[cfg(test)]
mod tests {
    use super::format_playback_speed;

    #[test]
    fn playback_speed_labels_do_not_show_redundant_zeroes() {
        assert_eq!(format_playback_speed(1.0), "1×");
        assert_eq!(format_playback_speed(1.5), "1.5×");
        assert_eq!(format_playback_speed(1.25), "1.25×");
    }
}
