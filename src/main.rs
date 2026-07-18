mod mpv_player;

use std::{path::PathBuf, sync::Arc, time::Duration};

use gpui::{
    App, Bounds, Context, Div, ExternalPaths, FocusHandle, FontWeight, KeyBinding, MouseButton,
    MouseDownEvent, PathPromptOptions, Render, RenderImage, SharedString, Stateful, Task, Window,
    WindowBounds, WindowOptions, actions, div, img, prelude::*, px, relative, rgb, size,
};
use gpui_platform::application;
use image::{Frame, RgbaImage};

use crate::mpv_player::MpvPlayer;

const HEADER_HEIGHT: f32 = 52.0;
const CONTROLS_HEIGHT: f32 = 112.0;
const MAX_RENDER_WIDTH: f32 = 1280.0;
const MAX_RENDER_HEIGHT: f32 = 720.0;

actions!(video_player, [OpenFile, TogglePlayback, SeekBack, SeekForward, ToggleFullscreen]);

struct PlayerView {
    player: Option<MpvPlayer>,
    frame: Option<Arc<RenderImage>>,
    focus_handle: FocusHandle,
    current_path: Option<PathBuf>,
    title: SharedString,
    status: SharedString,
    position: f64,
    duration: f64,
    volume: f64,
    paused: bool,
    tick_count: u64,
    _tick_task: Task<()>,
}

impl PlayerView {
    fn new(
        initial_path: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (player, status) = match MpvPlayer::new() {
            Ok(player) => (Some(player), SharedString::from("Ready")),
            Err(error) => (
                None,
                SharedString::from(format!("libmpv initialization failed: {error}")),
            ),
        };

        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle, cx);

        let tick_task = cx.spawn_in(window, async move |this, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(16))
                .await;
            if this
                .update_in(cx, |this, window, cx| this.tick(window, cx))
                .is_err()
            {
                return;
            }
        });

        let mut view = Self {
            player,
            frame: None,
            focus_handle,
            current_path: None,
            title: "GPUI Video Player".into(),
            status,
            position: 0.0,
            duration: 0.0,
            volume: 100.0,
            paused: false,
            tick_count: 0,
            _tick_task: tick_task,
        };

        if let Some(path) = initial_path {
            view.load_path(path, cx);
        }
        view
    }

    fn tick(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.tick_count = self.tick_count.wrapping_add(1);
        let Some(player) = self.player.as_mut() else {
            return;
        };
        if self.current_path.is_none() {
            return;
        }

        let (width, height) = render_dimensions(window);
        match player.render_frame(width, height) {
            Ok(Some(frame)) => {
                let Some(buffer) = RgbaImage::from_raw(frame.width, frame.height, frame.pixels) else {
                    self.status = "libmpv returned an invalid frame".into();
                    cx.notify();
                    return;
                };
                self.frame = Some(Arc::new(RenderImage::new(vec![Frame::new(buffer)])));
                cx.notify();
            }
            Ok(None) => {}
            Err(error) => {
                self.status = format!("Render error: {error}").into();
                cx.notify();
            }
        }

        if self.tick_count.is_multiple_of(6) {
            let old_position = self.position;
            let old_duration = self.duration;
            let old_paused = self.paused;

            self.position = player.position().unwrap_or(self.position).max(0.0);
            self.duration = player.duration().unwrap_or(self.duration).max(0.0);
            self.paused = player.paused().unwrap_or(self.paused);
            if let Some(title) = player.title().filter(|title| !title.is_empty()) {
                self.title = title.into();
            }
            self.status = if self.paused { "Paused" } else { "Playing" }.into();

            if old_position != self.position
                || old_duration != self.duration
                || old_paused != self.paused
            {
                cx.notify();
            }
        }
    }

    fn load_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let Some(player) = self.player.as_mut() else {
            return;
        };
        let display_path = path.to_string_lossy();
        match player.load(&display_path) {
            Ok(()) => {
                self.title = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| display_path.into_owned())
                    .into();
                self.current_path = Some(path);
                self.frame = None;
                self.position = 0.0;
                self.duration = 0.0;
                self.paused = false;
                self.status = "Loading…".into();
            }
            Err(error) => self.status = format!("Could not open video: {error}").into(),
        }
        cx.notify();
    }

    fn open_file(&mut self, _: &OpenFile, window: &mut Window, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Open Video".into()),
        });

        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(mut paths))) = receiver.await else {
                return;
            };
            let Some(path) = paths.pop() else {
                return;
            };
            let _ = this.update_in(cx, |this, _, cx| this.load_path(path, cx));
        })
        .detach();
    }

    fn toggle_playback(
        &mut self,
        _: &TogglePlayback,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(player) = self.player.as_ref() else {
            return;
        };
        match player.toggle_pause() {
            Ok(paused) => {
                self.paused = paused;
                self.status = if paused { "Paused" } else { "Playing" }.into();
            }
            Err(error) => self.status = format!("Playback error: {error}").into(),
        }
        cx.notify();
    }

    fn stop(&mut self, cx: &mut Context<Self>) {
        let Some(player) = self.player.as_mut() else {
            return;
        };
        match player.stop() {
            Ok(()) => {
                self.frame = None;
                self.position = 0.0;
                self.duration = 0.0;
                self.status = "Stopped".into();
            }
            Err(error) => self.status = format!("Stop failed: {error}").into(),
        }
        cx.notify();
    }

    fn seek_relative(&mut self, seconds: f64, cx: &mut Context<Self>) {
        let Some(player) = self.player.as_ref() else {
            return;
        };
        if let Err(error) = player.seek_relative(seconds) {
            self.status = format!("Seek failed: {error}").into();
        }
        cx.notify();
    }

    fn seek_back(&mut self, _: &SeekBack, _: &mut Window, cx: &mut Context<Self>) {
        self.seek_relative(-10.0, cx);
    }

    fn seek_forward(&mut self, _: &SeekForward, _: &mut Window, cx: &mut Context<Self>) {
        self.seek_relative(10.0, cx);
    }

    fn set_volume(&mut self, volume: f64, cx: &mut Context<Self>) {
        self.volume = volume.clamp(0.0, 100.0);
        if let Some(player) = self.player.as_ref()
            && let Err(error) = player.set_volume(self.volume)
        {
            self.status = format!("Volume error: {error}").into();
        }
        cx.notify();
    }

    fn seek_from_mouse(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.button != MouseButton::Left || self.duration <= 0.0 {
            return;
        }
        let viewport_width = f32::from(window.viewport_size().width);
        let fraction = ((f32::from(event.position.x) - 16.0) / (viewport_width - 32.0))
            .clamp(0.0, 1.0);
        let new_position = self.duration * f64::from(fraction);
        if let Some(player) = self.player.as_ref() {
            if let Err(error) = player.seek_absolute(new_position) {
                self.status = format!("Seek failed: {error}").into();
            } else {
                self.position = new_position;
            }
        }
        cx.notify();
    }

    fn toggle_fullscreen(
        &mut self,
        _: &ToggleFullscreen,
        window: &mut Window,
        _: &mut Context<Self>,
    ) {
        window.toggle_fullscreen();
    }
}

impl Render for PlayerView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let progress = if self.duration > 0.0 {
            (self.position / self.duration).clamp(0.0, 1.0) as f32
        } else {
            0.0
        };
        let play_label = if self.paused { "▶ Play" } else { "Ⅱ Pause" };
        let frame = self.frame.clone();
        let has_video = self.current_path.is_some();

        div()
            .id("player")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::open_file))
            .on_action(cx.listener(Self::toggle_playback))
            .on_action(cx.listener(Self::seek_back))
            .on_action(cx.listener(Self::seek_forward))
            .on_action(cx.listener(Self::toggle_fullscreen))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                if let Some(path) = paths.0.first() {
                    this.load_path(path.clone(), cx);
                }
            }))
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(0x0b0d12))
            .text_color(rgb(0xe8eaf0))
            .child(
                div()
                    .h(px(HEADER_HEIGHT))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap_3()
                    .px_4()
                    .border_b_1()
                    .border_color(rgb(0x252a35))
                    .bg(rgb(0x12151c))
                    .child(
                        div()
                            .flex_1()
                            .overflow_hidden()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(self.title.clone()),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(0x9299aa))
                            .child(self.status.clone()),
                    )
                    .child(
                        control_button("open", "Open video")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_file(&OpenFile, window, cx)
                            })),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h(px(180.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .overflow_hidden()
                    .bg(rgb(0x000000))
                    .when_some(frame, |video, frame| {
                        video.child(img(frame).size_full())
                    })
                    .when(!has_video, |video| {
                        video.child(
                            div()
                                .flex()
                                .flex_col()
                                .items_center()
                                .gap_2()
                                .text_color(rgb(0x747b8c))
                                .child(div().text_xl().child("No video loaded"))
                                .child(div().text_sm().child(
                                    "Open a file, drop one here, or pass a path on the command line",
                                )),
                        )
                    }),
            )
            .child(
                div()
                    .h(px(CONTROLS_HEIGHT))
                    .flex_none()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .px_4()
                    .py_3()
                    .border_t_1()
                    .border_color(rgb(0x252a35))
                    .bg(rgb(0x12151c))
                    .child(
                        div()
                            .id("timeline")
                            .w_full()
                            .h(px(10.0))
                            .flex_none()
                            .rounded_full()
                            .overflow_hidden()
                            .cursor_pointer()
                            .bg(rgb(0x2b303b))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(Self::seek_from_mouse),
                            )
                            .child(
                                div()
                                    .h_full()
                                    .w(relative(progress))
                                    .rounded_full()
                                    .bg(rgb(0x6c8cff)),
                            ),
                    )
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                control_button("seek-back", "↶ 10s").on_click(cx.listener(
                                    |this, _, _, cx| this.seek_relative(-10.0, cx),
                                )),
                            )
                            .child(
                                control_button("play-pause", play_label).on_click(cx.listener(
                                    |this, _, window, cx| {
                                        this.toggle_playback(&TogglePlayback, window, cx)
                                    },
                                )),
                            )
                            .child(
                                control_button("stop", "■ Stop")
                                    .on_click(cx.listener(|this, _, _, cx| this.stop(cx))),
                            )
                            .child(
                                control_button("seek-forward", "10s ↷").on_click(cx.listener(
                                    |this, _, _, cx| this.seek_relative(10.0, cx),
                                )),
                            )
                            .child(
                                div()
                                    .ml_2()
                                    .text_sm()
                                    .text_color(rgb(0xb7bdca))
                                    .child(format!(
                                        "{} / {}",
                                        format_time(self.position),
                                        format_time(self.duration)
                                    )),
                            )
                            .child(div().flex_1())
                            .child(
                                control_button("volume-down", "−")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.set_volume(this.volume - 5.0, cx)
                                    })),
                            )
                            .child(
                                div()
                                    .w(px(72.0))
                                    .text_center()
                                    .text_sm()
                                    .text_color(rgb(0xb7bdca))
                                    .child(format!("Vol {}%", self.volume.round())),
                            )
                            .child(
                                control_button("volume-up", "+")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.set_volume(this.volume + 5.0, cx)
                                    })),
                            )
                            .child(
                                control_button("fullscreen", "Fullscreen").on_click(cx.listener(
                                    |this, _, window, cx| {
                                        this.toggle_fullscreen(&ToggleFullscreen, window, cx)
                                    },
                                )),
                            ),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(0x697083))
                            .child("Space play/pause  •  ←/→ seek  •  F fullscreen"),
                    ),
            )
    }
}

fn control_button(id: &'static str, label: impl Into<SharedString>) -> Stateful<Div> {
    div()
        .id(id)
        .h(px(32.0))
        .px_3()
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .cursor_pointer()
        .bg(rgb(0x242936))
        .hover(|button| button.bg(rgb(0x32394a)))
        .active(|button| button.bg(rgb(0x1d2230)))
        .text_sm()
        .child(label.into())
}

fn render_dimensions(window: &Window) -> (usize, usize) {
    let viewport = window.viewport_size();
    let scale = window.scale_factor();
    let available_width = (f32::from(viewport.width) * scale).max(16.0);
    let available_height = ((f32::from(viewport.height) - HEADER_HEIGHT - CONTROLS_HEIGHT) * scale)
        .max(16.0);
    let downscale = (MAX_RENDER_WIDTH / available_width)
        .min(MAX_RENDER_HEIGHT / available_height)
        .min(1.0);

    let width = ((available_width * downscale) as usize).max(16) & !15;
    let height = ((available_height * downscale) as usize).max(16);
    (width, height)
}

fn format_time(seconds: f64) -> String {
    if !seconds.is_finite() || seconds <= 0.0 {
        return "00:00".to_string();
    }
    let total = seconds.floor() as u64;
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let seconds = total % 60;
    if hours > 0 {
        format!("{hours:02}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes:02}:{seconds:02}")
    }
}

fn main() {
    env_logger::init();
    let initial_path = std::env::args_os().nth(1).map(PathBuf::from);

    application().run(move |cx: &mut App| {
        cx.bind_keys([
            KeyBinding::new("space", TogglePlayback, Some("Player")),
            KeyBinding::new("left", SeekBack, Some("Player")),
            KeyBinding::new("right", SeekForward, Some("Player")),
            KeyBinding::new("f", ToggleFullscreen, Some("Player")),
            KeyBinding::new("cmd-o", OpenFile, Some("Player")),
        ]);

        let bounds = Bounds::centered(None, size(px(1100.0), px(720.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            move |window, cx| {
                cx.new(|cx| PlayerView::new(initial_path.clone(), window, cx))
            },
        )
        .expect("failed to open GPUI player window");

        cx.activate(true);
    });
}

#[cfg(test)]
mod tests {
    use super::format_time;

    #[test]
    fn formats_player_timestamps() {
        assert_eq!(format_time(0.0), "00:00");
        assert_eq!(format_time(65.9), "01:05");
        assert_eq!(format_time(3661.0), "01:01:01");
    }
}
