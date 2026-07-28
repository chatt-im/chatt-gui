use std::{cell::Cell, rc::Rc, sync::Arc};

use gpui::{
    Animation, AnimationExt, AnyElement, App, Bounds, ClickEvent, Div, MouseButton, MouseDownEvent,
    MouseMoveEvent, ObjectFit, Pixels, Stateful, Window, canvas, div, img, linear_color_stop,
    linear_gradient, point, prelude::*, px, relative,
};

use crate::{
    icons::{IconName, icon},
    media_controls::format_time,
    theme::{ResolvedSettings, ThemeRole},
    ui_scale::rems_from_px,
    video_controls::{
        CONTROLS_ANIMATION_DURATION, ControlsPhase, VOLUME_ANIMATION_DURATION, horizontal_fraction,
    },
    video_manager::{VideoKey, VideoView},
    video_thumbnail::ThumbnailView,
};

pub(crate) enum VideoPlayerEvent {
    PlayerHovered(bool),
    PointerMoved,
    SurfaceClicked {
        click_count: usize,
        unstarted: bool,
    },
    Play,
    ScrubHovered(f64),
    ScrubHoverCleared,
    ScrubPressed {
        bounds: Bounds<Pixels>,
        event: MouseDownEvent,
    },
    ControlsHovered(bool),
    VolumeHovered(bool),
    VolumePopupHovered(bool),
    ToggleMute,
    ToggleLoop,
    VolumePressed {
        bounds: Bounds<Pixels>,
        event: MouseDownEvent,
    },
    ToggleTheater,
}

pub(crate) type VideoPlayerHandler = Rc<dyn Fn(VideoPlayerEvent, &mut Window, &mut App) + 'static>;

pub(crate) const INLINE_VIDEO_ASPECT_RATIO: f32 = 16.0 / 9.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum VideoControlDisplay {
    Effect { label: &'static str, value: f64 },
    Volume(f64),
    PlaybackSpeed(f64),
}

pub(crate) struct VideoPlayerConfig {
    pub key: VideoKey,
    pub theater: bool,
    pub video: VideoView,
    pub thumbnail: ThumbnailView,
    pub duration: f64,
    pub display_position: f64,
    pub aspect_ratio: f32,
    pub fallback_label: String,
    pub controls_phase: ControlsPhase,
    pub controls_pinned: bool,
    pub scrub_hover_fraction: Option<f64>,
    pub control_overlay: Option<VideoControlDisplay>,
    pub volume_open: bool,
    pub measure_volume_bounds: bool,
}

pub(crate) fn render_video_player(
    config: VideoPlayerConfig,
    handler: VideoPlayerHandler,
    volume_popup_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    volume_button_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    settings: Arc<ResolvedSettings>,
) -> AnyElement {
    let VideoPlayerConfig {
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
        control_overlay,
        volume_open,
        measure_volume_bounds,
    } = config;
    let progress = if duration > 0.0 {
        (display_position / duration).clamp(0.0, 1.0) as f32
    } else {
        0.0
    };
    let has_thumbnail = thumbnail.image.is_some();
    let has_surface = video.surface.is_some();
    let show_center_play = !has_surface && !video.loading;
    let show_controls = controls_pinned || controls_phase.rendered();
    let scrub_active = scrub_hover_fraction.is_some();
    let scrub_bounds = Rc::new(Cell::new(None));
    let measured_scrub_bounds = scrub_bounds.clone();
    let hover_scrub_bounds = scrub_bounds.clone();
    let down_scrub_bounds = scrub_bounds.clone();
    let volume_slider_bounds = Rc::new(Cell::new(None));
    let measured_volume_slider_bounds = volume_slider_bounds.clone();
    let down_volume_slider_bounds = volume_slider_bounds.clone();

    let thumbnail_layer = thumbnail.image.map(|thumbnail| {
        img(thumbnail)
            .absolute()
            .inset_0()
            .size_full()
            .object_fit(ObjectFit::Contain)
    });
    let surface_layer = video.surface.clone().map(|video_surface| {
        canvas(
            move |bounds, _, _| fit_video_bounds(bounds, aspect_ratio),
            move |_, fitted, window, _| {
                if let Some(fitted) = fitted {
                    window.paint_platform_surface(fitted, video_surface.clone());
                }
            },
        )
        .absolute()
        .size_full()
        .into_any_element()
    });

    let player_hover = handler.clone();
    let pointer_move = handler.clone();
    let surface_click = handler.clone();
    let mut viewport = div()
        .id(("video-viewport", key.message_id as usize))
        .relative()
        .w_full()
        .flex()
        .items_center()
        .justify_center()
        .overflow_hidden()
        .bg(settings.theme.color(ThemeRole::MediaViewport))
        .when(theater, |viewport| viewport.size_full())
        .when(!theater, |viewport| {
            viewport.aspect_ratio(INLINE_VIDEO_ASPECT_RATIO)
        })
        .on_hover(move |hovered, window, cx| {
            player_hover(VideoPlayerEvent::PlayerHovered(*hovered), window, cx)
        })
        .on_mouse_move(move |_: &MouseMoveEvent, window, cx| {
            pointer_move(VideoPlayerEvent::PointerMoved, window, cx)
        })
        .on_click(move |event: &ClickEvent, window, cx| {
            surface_click(
                VideoPlayerEvent::SurfaceClicked {
                    click_count: event.click_count(),
                    unstarted: !has_surface,
                },
                window,
                cx,
            )
        })
        .when_some(thumbnail_layer, |viewport, thumbnail| {
            viewport.child(thumbnail)
        })
        .when_some(surface_layer, |viewport, surface| viewport.child(surface));

    if !has_thumbnail && !has_surface {
        viewport = viewport.child(
            div()
                .flex()
                .flex_col()
                .items_center()
                .gap_2()
                .text_color(settings.theme.color(ThemeRole::MediaMutedText))
                .child(div().text_sm().child(if video.loading {
                    "Starting video…".to_string()
                } else if thumbnail.failed {
                    format!("{} · no preview", fallback_label)
                } else {
                    fallback_label
                })),
        );
    }

    if show_center_play {
        let play = handler.clone();
        viewport = viewport.child(
            div()
                .id(("video-center-play", key.message_id as usize))
                .absolute()
                .size(rems_from_px(52.0))
                .flex()
                .items_center()
                .justify_center()
                .border_1()
                .border_color(settings.theme.color(ThemeRole::MediaBorder))
                .bg(settings.theme.color(ThemeRole::MediaOverlay))
                .cursor_pointer()
                .hover({
                    let hover = settings.theme.color(ThemeRole::MediaOverlayStrong);
                    let border = settings.theme.color(ThemeRole::BorderFocus);
                    move |button| button.bg(hover).border_color(border)
                })
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(move |_, window, cx| {
                    cx.stop_propagation();
                    play(VideoPlayerEvent::Play, window, cx)
                })
                .child(icon(
                    IconName::Play,
                    22.0,
                    settings.theme.color(ThemeRole::ControlActiveText),
                )),
        );
    }

    if let Some(control) = control_overlay {
        let presentation = control_overlay_presentation(control);
        viewport = viewport.child(
            div()
                .absolute()
                .top_0()
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .pt(rems_from_px(if theater { 18.0 } else { 8.0 }))
                .px(rems_from_px(10.0))
                .child(
                    div()
                        .w(rems_from_px(if theater { 320.0 } else { 220.0 }))
                        .max_w_full()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .px_3()
                        .py_2()
                        .rounded_sm()
                        .border_1()
                        .border_color(settings.theme.color(ThemeRole::MediaBorder))
                        .bg(settings.theme.color(ThemeRole::MediaOverlayStrong))
                        .shadow_sm()
                        .child(
                            div()
                                .w_full()
                                .flex()
                                .items_center()
                                .justify_between()
                                .text_xs()
                                .text_color(settings.theme.color(ThemeRole::MediaText))
                                .child(presentation.label)
                                .child(presentation.value),
                        )
                        .child(
                            div()
                                .relative()
                                .w_full()
                                .h(rems_from_px(5.0))
                                .rounded_full()
                                .bg(settings.theme.color(ThemeRole::MediaProgressTrack))
                                .child(
                                    div()
                                        .absolute()
                                        .left_0()
                                        .top_0()
                                        .h_full()
                                        .w(relative(presentation.progress))
                                        .rounded_full()
                                        .bg(settings.theme.color(ThemeRole::MediaProgressFill)),
                                )
                                .when_some(presentation.neutral, |bar, neutral| {
                                    bar.child(
                                        div()
                                            .absolute()
                                            .left(relative(neutral))
                                            .top(rems_from_px(-2.0))
                                            .ml(rems_from_px(-0.5))
                                            .w(rems_from_px(1.0))
                                            .h(rems_from_px(9.0))
                                            .bg(settings.theme.color(ThemeRole::MediaText)),
                                    )
                                })
                                .child(
                                    div()
                                        .absolute()
                                        .left(relative(presentation.progress))
                                        .top(rems_from_px(-2.0))
                                        .ml(rems_from_px(-4.5))
                                        .size(rems_from_px(9.0))
                                        .rounded_full()
                                        .bg(settings.theme.color(ThemeRole::MediaProgressKnob)),
                                ),
                        ),
                ),
        );
    }

    if show_controls {
        let tooltip_fraction = scrub_hover_fraction.unwrap_or(progress as f64) as f32;
        let tooltip_position = duration * f64::from(tooltip_fraction);
        let scrub_move = handler.clone();
        let scrub_exit = handler.clone();
        let scrub_down = handler.clone();
        let timeline = div()
            .id(("video-timeline", key.message_id as usize))
            .relative()
            .w_full()
            .h(rems_from_px(20.0))
            .flex()
            .items_center()
            .when(duration > 0.0, |timeline| {
                timeline
                    .cursor_pointer()
                    .on_mouse_move(move |event: &MouseMoveEvent, window, cx| {
                        if let Some(bounds) = hover_scrub_bounds.get()
                            && let Some(fraction) =
                                horizontal_fraction(bounds, event.position.x, duration)
                        {
                            scrub_move(VideoPlayerEvent::ScrubHovered(fraction), window, cx);
                        }
                    })
                    .on_hover(move |hovered, window, cx| {
                        if !*hovered {
                            scrub_exit(VideoPlayerEvent::ScrubHoverCleared, window, cx);
                        }
                    })
                    .on_mouse_down(MouseButton::Left, move |event, window, cx| {
                        if let Some(bounds) = down_scrub_bounds.get() {
                            scrub_down(
                                VideoPlayerEvent::ScrubPressed {
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
            .when_some(scrub_hover_fraction, |timeline, _| {
                timeline.child(
                    div()
                        .absolute()
                        .bottom(rems_from_px(18.0))
                        .left(relative(tooltip_fraction))
                        .ml(rems_from_px(-24.0))
                        .min_w(rems_from_px(48.0))
                        .px_2()
                        .py_1()
                        .border_1()
                        .border_color(settings.theme.color(ThemeRole::MediaBorder))
                        .bg(settings.theme.color(ThemeRole::MediaOverlayStrong))
                        .text_center()
                        .text_xs()
                        .text_color(settings.theme.color(ThemeRole::MediaText))
                        .child(format_time(tooltip_position)),
                )
            })
            .child(
                div()
                    .relative()
                    .w_full()
                    .h(rems_from_px(if scrub_active { 5.0 } else { 3.0 }))
                    .bg(settings.theme.color(ThemeRole::MediaProgressTrack))
                    .child(
                        div()
                            .relative()
                            .h_full()
                            .w(relative(progress))
                            .bg(settings.theme.color(ThemeRole::MediaProgressFill))
                            .when(scrub_active, |progress| {
                                progress.child(
                                    div()
                                        .absolute()
                                        .right(rems_from_px(-4.0))
                                        .top(rems_from_px(-2.0))
                                        .size(rems_from_px(9.0))
                                        .bg(settings.theme.color(ThemeRole::MediaProgressKnob)),
                                )
                            }),
                    ),
            );

        let volume_icon = if video.volume <= 0.0 {
            IconName::VolumeMuted
        } else if video.volume <= 50.0 {
            IconName::VolumeLow
        } else {
            IconName::VolumeHigh
        };
        let volume_fraction = (video.volume / 100.0).clamp(0.0, 1.0) as f32;
        let volume_hover = handler.clone();
        let mute = handler.clone();
        let mut volume_control = div()
            .id(("video-volume", key.message_id as usize))
            .relative()
            .size(rems_from_px(36.0))
            .flex_none()
            .on_hover(move |hovered, window, cx| {
                volume_hover(VideoPlayerEvent::VolumeHovered(*hovered), window, cx)
            })
            .child(
                canvas(
                    move |bounds, _, _| {
                        if measure_volume_bounds {
                            volume_button_bounds.set(Some(bounds));
                        }
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .child(
                video_control_button(
                    ("video-volume-button", key.message_id as usize),
                    volume_icon,
                    &settings,
                )
                .on_click(move |_, window, cx| {
                    cx.stop_propagation();
                    mute(VideoPlayerEvent::ToggleMute, window, cx)
                }),
            );
        if volume_open {
            let popup_hover = handler.clone();
            let volume_down = handler.clone();
            let popup = div()
                .id(("video-volume-popup", key.message_id as usize))
                .absolute()
                .left(rems_from_px(-2.0))
                .bottom(rems_from_px(40.0))
                .w(rems_from_px(40.0))
                .h(rems_from_px(112.0))
                .flex()
                .flex_col()
                .items_center()
                .gap_2()
                .px_2()
                .py_2()
                .border_1()
                .border_color(settings.theme.color(ThemeRole::MediaBorder))
                .bg(settings.theme.color(ThemeRole::MediaOverlayStrong))
                .shadow_sm()
                .on_hover(move |hovered, window, cx| {
                    popup_hover(VideoPlayerEvent::VolumePopupHovered(*hovered), window, cx)
                })
                .child(
                    canvas(
                        move |bounds, _, _| volume_popup_bounds.set(Some(bounds)),
                        |_, _, _, _| {},
                    )
                    .absolute()
                    .size_full(),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(settings.theme.color(ThemeRole::TextSecondary))
                        .child(format!("{:.0}", video.volume)),
                )
                .child(
                    div()
                        .relative()
                        .w(rems_from_px(12.0))
                        .flex_1()
                        .flex()
                        .justify_center()
                        .cursor_pointer()
                        .on_mouse_down(MouseButton::Left, move |event, window, cx| {
                            if let Some(bounds) = down_volume_slider_bounds.get() {
                                volume_down(
                                    VideoPlayerEvent::VolumePressed {
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
                                move |bounds, _, _| measured_volume_slider_bounds.set(Some(bounds)),
                                |_, _, _, _| {},
                            )
                            .absolute()
                            .size_full(),
                        )
                        .child(
                            div()
                                .relative()
                                .w(rems_from_px(4.0))
                                .h_full()
                                .bg(settings.theme.color(ThemeRole::MediaProgressTrack))
                                .child(
                                    div()
                                        .absolute()
                                        .left_0()
                                        .bottom_0()
                                        .w_full()
                                        .h(relative(volume_fraction))
                                        .bg(settings.theme.color(ThemeRole::MediaProgressFill)),
                                )
                                .child(
                                    div()
                                        .absolute()
                                        .left(rems_from_px(-3.0))
                                        .bottom(relative(volume_fraction))
                                        .mb(rems_from_px(-4.0))
                                        .size(rems_from_px(10.0))
                                        .bg(settings.theme.color(ThemeRole::MediaProgressKnob)),
                                ),
                        ),
                )
                .with_animation(
                    ("video-volume-popup-in", key.message_id as usize),
                    Animation::new(VOLUME_ANIMATION_DURATION).with_easing(gpui::ease_out_quint()),
                    |popup, delta| {
                        popup
                            .opacity(delta)
                            .bottom(rems_from_px(36.0 + 4.0 * delta))
                    },
                );
            volume_control = volume_control.child(popup);
        }

        let play = handler.clone();
        let loop_toggle = handler.clone();
        let theater_toggle = handler.clone();
        let control_row = div()
            .min_h(rems_from_px(38.0))
            .flex()
            .flex_wrap()
            .items_center()
            .gap_1()
            .child(
                video_control_button(
                    ("video-play", key.message_id as usize),
                    if !video.paused && !video.finished {
                        IconName::Pause
                    } else {
                        IconName::Play
                    },
                    &settings,
                )
                .opacity(if video.loading { 0.55 } else { 1.0 })
                .when(!video.loading, |button| {
                    button.on_click(move |_, window, cx| {
                        cx.stop_propagation();
                        play(VideoPlayerEvent::Play, window, cx)
                    })
                }),
            )
            .child(volume_control)
            .child(
                div()
                    .ml_1()
                    .text_xs()
                    .text_color(settings.theme.color(ThemeRole::MediaText))
                    .child(format!(
                        "{} / {}",
                        format_time(display_position),
                        format_time(duration)
                    )),
            )
            .child(div().flex_1())
            .child(
                video_control_button(
                    ("video-loop", key.message_id as usize),
                    if video.looping {
                        IconName::Repeat1
                    } else {
                        IconName::RepeatOff
                    },
                    &settings,
                )
                .when(video.looping, |button| {
                    button.bg(settings.theme.color(ThemeRole::StateInlineCode))
                })
                .on_click(move |_, window, cx| {
                    cx.stop_propagation();
                    loop_toggle(VideoPlayerEvent::ToggleLoop, window, cx)
                }),
            )
            .child(
                video_control_button(
                    ("video-theater", key.message_id as usize),
                    if theater {
                        IconName::Minimize
                    } else {
                        IconName::Maximize
                    },
                    &settings,
                )
                .on_click(move |_, window, cx| {
                    cx.stop_propagation();
                    theater_toggle(VideoPlayerEvent::ToggleTheater, window, cx)
                }),
            );

        let controls_hover = handler.clone();
        let controls = div()
            .id(("video-controls", key.message_id as usize))
            .absolute()
            .left_0()
            .right_0()
            .bottom_0()
            .pt(rems_from_px(if theater { 48.0 } else { 34.0 }))
            .px(rems_from_px(if theater { 18.0 } else { 10.0 }))
            .pb(rems_from_px(if theater { 12.0 } else { 7.0 }))
            .bg(linear_gradient(
                180.0,
                linear_color_stop(settings.theme.color(ThemeRole::MediaGradientStart), 0.0),
                linear_color_stop(settings.theme.color(ThemeRole::MediaGradientEnd), 1.0),
            ))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_hover(move |hovered, window, cx| {
                controls_hover(VideoPlayerEvent::ControlsHovered(*hovered), window, cx)
            })
            .child(timeline)
            .child(control_row);
        let controls: AnyElement = if controls_pinned {
            controls.into_any_element()
        } else {
            match controls_phase {
                ControlsPhase::Showing(serial) => controls
                    .with_animation(
                        ("video-controls-show", serial as usize),
                        Animation::new(CONTROLS_ANIMATION_DURATION)
                            .with_easing(gpui::ease_out_quint()),
                        |controls, delta| {
                            controls
                                .opacity(delta)
                                .bottom(rems_from_px(-4.0 * (1.0 - delta)))
                        },
                    )
                    .into_any_element(),
                ControlsPhase::Hiding(serial) => controls
                    .with_animation(
                        ("video-controls-hide", serial as usize),
                        Animation::new(CONTROLS_ANIMATION_DURATION)
                            .with_easing(gpui::ease_out_quint()),
                        |controls, delta| {
                            controls
                                .opacity(1.0 - delta)
                                .bottom(rems_from_px(-4.0 * delta))
                        },
                    )
                    .into_any_element(),
                _ => controls.into_any_element(),
            }
        };
        viewport = viewport.child(controls);
    }

    div()
        .id((
            if theater { "video-theater" } else { "video" },
            key.message_id as usize,
        ))
        .relative()
        .w_full()
        .bg(settings.theme.color(ThemeRole::MediaViewport))
        .when(theater, |frame| frame.size_full())
        .when(!theater, |frame| frame.mt_2())
        .child(viewport)
        .into_any_element()
}

struct VideoControlPresentation {
    label: &'static str,
    value: String,
    progress: f32,
    neutral: Option<f32>,
}

fn control_overlay_presentation(control: VideoControlDisplay) -> VideoControlPresentation {
    match control {
        VideoControlDisplay::Effect { label, value } => {
            let value = value.clamp(-100.0, 100.0);
            VideoControlPresentation {
                label,
                value: format!("{:+.0}", value),
                progress: ((value + 100.0) / 200.0) as f32,
                neutral: Some(0.5),
            }
        }
        VideoControlDisplay::Volume(value) => VideoControlPresentation {
            label: "Volume",
            value: format!("{:.0}%", value.clamp(0.0, 100.0)),
            progress: (value.clamp(0.0, 100.0) / 100.0) as f32,
            neutral: None,
        },
        VideoControlDisplay::PlaybackSpeed(value) => {
            let value = value.clamp(0.25, 4.0);
            VideoControlPresentation {
                label: "Speed",
                value: format!("{value:.2}×"),
                progress: ((value.log2() + 2.0) / 4.0) as f32,
                neutral: Some(0.5),
            }
        }
    }
}

fn video_control_button(
    id: impl Into<gpui::ElementId>,
    icon_name: IconName,
    settings: &ResolvedSettings,
) -> Stateful<Div> {
    div()
        .id(id)
        .size(rems_from_px(36.0))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .text_color(settings.theme.color(ThemeRole::MediaText))
        .hover({
            let hover = settings.theme.color(ThemeRole::StateInlineCode);
            move |button| button.bg(hover)
        })
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .child(icon(
            icon_name,
            18.0,
            settings.theme.color(ThemeRole::MediaText),
        ))
}

pub(crate) fn aspect_ratio(video: &VideoView, fallback: (Option<u32>, Option<u32>)) -> f32 {
    video
        .display_size
        .filter(|(width, height)| *width > 0 && *height > 0)
        .map(|(width, height)| width as f32 / height as f32)
        .or_else(|| match fallback {
            (Some(width), Some(height)) if width > 0 && height > 0 => {
                Some(width as f32 / height as f32)
            }
            _ => None,
        })
        .unwrap_or(16.0 / 9.0)
}

fn fit_video_bounds(viewport: Bounds<Pixels>, aspect_ratio: f32) -> Option<Bounds<Pixels>> {
    let viewport_width = viewport.size.width.as_f32();
    let viewport_height = viewport.size.height.as_f32();
    if viewport_width <= 0.0
        || viewport_height <= 0.0
        || aspect_ratio <= 0.0
        || !aspect_ratio.is_finite()
    {
        return None;
    }
    let viewport_aspect = viewport_width / viewport_height;
    let (width, height) = if viewport_aspect > aspect_ratio {
        (viewport_height * aspect_ratio, viewport_height)
    } else {
        (viewport_width, viewport_width / aspect_ratio)
    };
    let center = viewport.center();
    Some(Bounds::new(
        point(center.x - px(width / 2.0), center.y - px(height / 2.0)),
        gpui::size(px(width), px(height)),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theater_video_bounds_letterbox_to_the_decoded_aspect_ratio() {
        let viewport = Bounds {
            origin: point(px(0.0), px(0.0)),
            size: gpui::size(px(1_000.0), px(500.0)),
        };
        let portrait = fit_video_bounds(viewport, 9.0 / 16.0).unwrap();

        assert_eq!(portrait.size.height, px(500.0));
        assert_eq!(portrait.size.width, px(281.25));
        assert_eq!(portrait.origin.x, px(359.375));
    }

    #[test]
    fn decoded_dimensions_override_attachment_metadata() {
        let video = VideoView {
            display_size: Some((1_080, 1_920)),
            ..VideoView::default()
        };
        assert_eq!(aspect_ratio(&video, (Some(1_920), Some(1_080))), 9.0 / 16.0);
    }

    #[test]
    fn video_control_presentations_format_values_and_clamp_progress() {
        let effect = control_overlay_presentation(VideoControlDisplay::Effect {
            label: "Gamma",
            value: 200.0,
        });
        assert_eq!(effect.label, "Gamma");
        assert_eq!(effect.value, "+100");
        assert_eq!(effect.progress, 1.0);
        assert_eq!(effect.neutral, Some(0.5));

        let volume = control_overlay_presentation(VideoControlDisplay::Volume(-20.0));
        assert_eq!(volume.value, "0%");
        assert_eq!(volume.progress, 0.0);
        assert_eq!(volume.neutral, None);

        let speed = control_overlay_presentation(VideoControlDisplay::PlaybackSpeed(1.0));
        assert_eq!(speed.value, "1.00×");
        assert_eq!(speed.progress, 0.5);
        assert_eq!(speed.neutral, Some(0.5));
    }
}
