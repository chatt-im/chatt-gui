use std::{path::PathBuf, sync::Arc, time::Duration};

use gpui::{
    actions, div, img, list, prelude::*, px, relative, rgb, App, AnyElement, Context, Div,
    ExternalPaths, FocusHandle, FollowMode, FontWeight, KeyBinding, ListAlignment, ListState,
    ObjectFit, PathPromptOptions, Render, RenderImage, SharedString, Stateful,
    StyledImage, Task, Window,
};
use image::{Frame, RgbaImage};

use crate::{
    mpv_player::MpvPlayer,
    timeline::{self, Attachment, Message},
};

const SIDEBAR_WIDTH: f32 = 232.0;
const TOP_BAR_HEIGHT: f32 = 52.0;
const COMPOSER_HEIGHT: f32 = 82.0;
const VIDEO_WIDTH: usize = 704;
const VIDEO_HEIGHT: usize = 396;

actions!(chatt_gui, [OpenMedia, TogglePlayback, SeekBack, SeekForward]);

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("cmd-o", OpenMedia, Some("Chatt")),
        KeyBinding::new("space", TogglePlayback, Some("Chatt")),
        KeyBinding::new("left", SeekBack, Some("Chatt")),
        KeyBinding::new("right", SeekForward, Some("Chatt")),
    ]);
}

pub struct ChattView {
    messages: Vec<Message>,
    list_state: ListState,
    focus_handle: FocusHandle,
    player: Option<MpvPlayer>,
    active_video: Option<usize>,
    frame: Option<Arc<RenderImage>>,
    position: f64,
    duration: f64,
    paused: bool,
    volume: f64,
    media_status: SharedString,
    tick_count: u64,
    _tick_task: Task<()>,
}

impl ChattView {
    pub fn new(paths: Vec<PathBuf>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut messages = timeline::sample_messages();
        append_paths_to_messages(&mut messages, paths);

        let list_state = ListState::new(messages.len(), ListAlignment::Bottom, px(1_600.0));
        list_state.set_follow_mode(FollowMode::Tail);

        let (player, media_status) = match MpvPlayer::new() {
            Ok(player) => (Some(player), "Media ready".into()),
            Err(error) => (
                None,
                format!("Video unavailable: {error}").into(),
            ),
        };

        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle, cx);
        let tick_task = cx.spawn_in(window, async move |this, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(16))
                .await;
            if this
                .update_in(cx, |this, _, cx| this.tick(cx))
                .is_err()
            {
                return;
            }
        });

        Self {
            messages,
            list_state,
            focus_handle,
            player,
            active_video: None,
            frame: None,
            position: 0.0,
            duration: 0.0,
            paused: true,
            volume: 100.0,
            media_status,
            tick_count: 0,
            _tick_task: tick_task,
        }
    }

    fn tick(&mut self, cx: &mut Context<Self>) {
        self.tick_count = self.tick_count.wrapping_add(1);
        let Some(player) = self.player.as_mut() else {
            return;
        };
        if self.active_video.is_none() {
            return;
        }

        match player.render_frame(VIDEO_WIDTH, VIDEO_HEIGHT) {
            Ok(Some(frame)) => {
                let Some(buffer) = RgbaImage::from_raw(frame.width, frame.height, frame.pixels)
                else {
                    self.media_status = "Video frame had an invalid size".into();
                    cx.notify();
                    return;
                };
                self.frame = Some(Arc::new(RenderImage::new(vec![Frame::new(buffer)])));
                cx.notify();
            }
            Ok(None) => {}
            Err(error) => {
                self.media_status = format!("Video render failed: {error}").into();
                cx.notify();
            }
        }

        if self.tick_count.is_multiple_of(6) {
            let old_state = (self.position, self.duration, self.paused);
            self.position = player.position().unwrap_or(self.position).max(0.0);
            self.duration = player.duration().unwrap_or(self.duration).max(0.0);
            self.paused = player.paused().unwrap_or(self.paused);
            if old_state != (self.position, self.duration, self.paused) {
                cx.notify();
            }
        }
    }

    fn add_paths(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        let old_len = self.messages.len();
        append_paths_to_messages(&mut self.messages, paths);
        let added = self.messages.len() - old_len;
        if added == 0 {
            self.media_status = "No supported images or videos in that selection".into();
            cx.notify();
            return;
        }

        self.list_state.splice(old_len..old_len, added);
        self.media_status = format!("Added {added} media item{}", if added == 1 { "" } else { "s" }).into();
        cx.notify();
    }

    fn open_media(&mut self, _: &OpenMedia, window: &mut Window, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Add images or videos".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = receiver.await else {
                return;
            };
            let _ = this.update_in(cx, |this, _, cx| this.add_paths(paths, cx));
        })
        .detach();
    }

    fn activate_video(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.active_video == Some(index) {
            self.toggle_playback_inner(cx);
            return;
        }
        let Some(Attachment::Video { path }) = self
            .messages
            .get(index)
            .and_then(|message| message.attachment.as_ref())
        else {
            return;
        };
        let Some(player) = self.player.as_mut() else {
            return;
        };

        match player.load(&path.to_string_lossy()) {
            Ok(()) => {
                self.active_video = Some(index);
                self.frame = None;
                self.position = 0.0;
                self.duration = 0.0;
                self.paused = false;
                self.media_status = path
                    .file_name()
                    .map(|name| format!("Playing {}", name.to_string_lossy()))
                    .unwrap_or_else(|| "Playing video".to_string())
                    .into();
            }
            Err(error) => self.media_status = format!("Could not open video: {error}").into(),
        }
        cx.notify();
    }

    fn toggle_playback_inner(&mut self, cx: &mut Context<Self>) {
        let Some(player) = self.player.as_ref() else {
            return;
        };
        match player.toggle_pause() {
            Ok(paused) => {
                self.paused = paused;
                self.media_status = if paused { "Video paused" } else { "Video playing" }.into();
            }
            Err(error) => self.media_status = format!("Playback failed: {error}").into(),
        }
        cx.notify();
    }

    fn toggle_playback(
        &mut self,
        _: &TogglePlayback,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.active_video.is_some() {
            self.toggle_playback_inner(cx);
        }
    }

    fn seek_relative(&mut self, seconds: f64, cx: &mut Context<Self>) {
        let Some(player) = self.player.as_ref() else {
            return;
        };
        if let Err(error) = player.seek_relative(seconds) {
            self.media_status = format!("Seek failed: {error}").into();
        }
        cx.notify();
    }

    fn adjust_volume(&mut self, delta: f64, cx: &mut Context<Self>) {
        self.volume = (self.volume + delta).clamp(0.0, 100.0);
        if let Some(player) = self.player.as_ref()
            && let Err(error) = player.set_volume(self.volume)
        {
            self.media_status = format!("Volume failed: {error}").into();
        }
        cx.notify();
    }

    fn seek_back(&mut self, _: &SeekBack, _: &mut Window, cx: &mut Context<Self>) {
        if self.active_video.is_some() {
            self.seek_relative(-10.0, cx);
        }
    }

    fn seek_forward(&mut self, _: &SeekForward, _: &mut Window, cx: &mut Context<Self>) {
        if self.active_video.is_some() {
            self.seek_relative(10.0, cx);
        }
    }

    fn render_message(
        &mut self,
        index: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(message) = self.messages.get(index).cloned() else {
            return div().into_any_element();
        };
        let continuation = timeline::is_continuation(&self.messages, index);
        let accent = sender_color(&message.sender, message.local);
        let background = if message.local { 0x171a20 } else { 0x111317 };
        let age = timeline::format_age(message.timestamp_ms, timeline::now_ms());

        div()
            .id(("message", message.id as usize))
            .w_full()
            .pl(px(64.0))
            .pr(px(28.0))
            .py(px(if continuation { 3.0 } else { 10.0 }))
            .bg(rgb(background))
            .hover(|row| row.bg(rgb(0x1b1e24)))
            .child(
                div()
                    .w_full()
                    .max_w(px(860.0))
                    .flex()
                    .child(
                        div()
                            .w(px(3.0))
                            .self_stretch()
                            .mr_3()
                            .bg(rgb(if continuation { background } else { accent })),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .when(!continuation, |content| {
                                content.child(
                                    div()
                                        .h(px(24.0))
                                        .flex()
                                        .items_center()
                                        .gap_2()
                                        .child(
                                            div()
                                                .font_weight(FontWeight::SEMIBOLD)
                                                .text_color(rgb(accent))
                                                .child(message.sender.clone()),
                                        )
                                        .when(message.edited, |meta| {
                                            meta.child(
                                                div()
                                                    .text_xs()
                                                    .text_color(rgb(0x777d87))
                                                    .child("edited"),
                                            )
                                        })
                                        .child(div().flex_1())
                                        .child(
                                            div()
                                                .text_xs()
                                                .text_color(rgb(0x777d87))
                                                .child(age),
                                        ),
                                )
                            })
                            .child(
                                div()
                                    .text_sm()
                                    .line_height(relative(1.55))
                                    .text_color(rgb(0xd7d9dd))
                                    .child(message.body.clone()),
                            )
                            .when_some(message.attachment.clone(), |content, attachment| {
                                content.child(self.render_attachment(index, attachment, cx))
                            }),
                    ),
            )
            .into_any_element()
    }

    fn render_attachment(
        &mut self,
        index: usize,
        attachment: Attachment,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match attachment {
            Attachment::Image {
                path,
                width,
                height,
            } => {
                let (render_width, render_height) = timeline::media_box_size(width, height);
                let fallback_name = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "image".to_string());
                img(path)
                    .id(("image", index))
                    .mt_2()
                    .w(px(render_width))
                    .h(px(render_height))
                    .max_w_full()
                    .bg(rgb(0x090a0c))
                    .border_1()
                    .border_color(rgb(0x292d34))
                    .object_fit(ObjectFit::Contain)
                    .with_loading(|| {
                        div()
                            .size_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_xs()
                            .text_color(rgb(0x686e78))
                            .child("decoding image…")
                            .into_any_element()
                    })
                    .with_fallback(move || {
                        div()
                            .size_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_sm()
                            .text_color(rgb(0x8b929d))
                            .child(format!("image unavailable · {fallback_name}"))
                            .into_any_element()
                    })
                    .into_any_element()
            }
            Attachment::Video { path } => {
                let active = self.active_video == Some(index);
                let frame = active.then(|| self.frame.clone()).flatten();
                let progress = if active && self.duration > 0.0 {
                    (self.position / self.duration).clamp(0.0, 1.0) as f32
                } else {
                    0.0
                };
                let label = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "video".to_string());
                let play_label = if active && !self.paused { "Ⅱ" } else { "▶" };

                div()
                    .id(("video", index))
                    .mt_2()
                    .w(px(704.0))
                    .max_w_full()
                    .border_1()
                    .border_color(rgb(if active { 0x596a90 } else { 0x292d34 }))
                    .bg(rgb(0x08090b))
                    .child(
                        div()
                            .h(px(396.0))
                            .max_h(px(396.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .overflow_hidden()
                            .when_some(frame, |viewport, frame| {
                                viewport.child(img(frame).size_full().object_fit(ObjectFit::Contain))
                            })
                            .when(!active || self.frame.is_none(), |viewport| {
                                viewport.child(
                                    div()
                                        .flex()
                                        .flex_col()
                                        .items_center()
                                        .gap_2()
                                        .text_color(rgb(0x8d939d))
                                        .child(div().text_2xl().child("▶"))
                                        .child(div().text_sm().child(label.clone())),
                                )
                            }),
                    )
                    .child(
                        div()
                            .h(px(48.0))
                            .flex()
                            .items_center()
                            .gap_3()
                            .px_3()
                            .border_t_1()
                            .border_color(rgb(0x24272d))
                            .child(
                                media_button(("video-back", index), "−10").on_click(
                                    cx.listener(|this, _, _, cx| this.seek_relative(-10.0, cx)),
                                ),
                            )
                            .child(
                                mini_button(("video-play", index), play_label).on_click(
                                    cx.listener(move |this, _, _, cx| {
                                        this.activate_video(index, cx)
                                    }),
                                ),
                            )
                            .child(
                                media_button(("video-forward", index), "+10").on_click(
                                    cx.listener(|this, _, _, cx| this.seek_relative(10.0, cx)),
                                ),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .h(px(4.0))
                                    .bg(rgb(0x30343b))
                                    .child(div().h_full().w(relative(progress)).bg(rgb(0x748bbd))),
                            )
                            .child(
                                div()
                                    .w(px(94.0))
                                    .text_right()
                                    .text_xs()
                                    .text_color(rgb(0x989ea8))
                                    .child(format!(
                                        "{} / {}",
                                        format_time(if active { self.position } else { 0.0 }),
                                        format_time(if active { self.duration } else { 0.0 })
                                    )),
                            )
                            .child(
                                mini_button(("volume-down", index), "−").on_click(
                                    cx.listener(|this, _, _, cx| this.adjust_volume(-5.0, cx)),
                                ),
                            )
                            .child(
                                div()
                                    .w(px(34.0))
                                    .text_center()
                                    .text_xs()
                                    .text_color(rgb(0x989ea8))
                                    .child(format!("{}", self.volume.round())),
                            )
                            .child(
                                mini_button(("volume-up", index), "+").on_click(
                                    cx.listener(|this, _, _, cx| this.adjust_volume(5.0, cx)),
                                ),
                            ),
                    )
                    .into_any_element()
            }
        }
    }

    fn render_sidebar(&self) -> Div {
        div()
            .w(px(SIDEBAR_WIDTH))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .border_r_1()
            .border_color(rgb(0x272a30))
            .bg(rgb(0x0d0f12))
            .child(
                div()
                    .h(px(TOP_BAR_HEIGHT))
                    .flex_none()
                    .flex()
                    .items_center()
                    .px_4()
                    .border_b_1()
                    .border_color(rgb(0x272a30))
                    .font_weight(FontWeight::BOLD)
                    .child("CHATT"),
            )
            .child(
                div()
                    .px_3()
                    .pt_4()
                    .pb_2()
                    .text_xs()
                    .text_color(rgb(0x676d77))
                    .child("ROOMS"),
            )
            .child(room_button("room-lobby", "#", "lobby", true))
            .child(room_button("room-design", "#", "design", false))
            .child(room_button("room-random", "#", "random", false))
            .child(
                div()
                    .mt_4()
                    .px_3()
                    .pb_2()
                    .text_xs()
                    .text_color(rgb(0x676d77))
                    .child("DIRECT MESSAGES"),
            )
            .child(room_button("room-mara", "●", "Mara", false))
            .child(room_button("room-theo", "●", "Theo", false))
            .child(div().flex_1())
            .child(
                div()
                    .h(px(58.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap_3()
                    .px_3()
                    .border_t_1()
                    .border_color(rgb(0x272a30))
                    .child(
                        div()
                            .size(px(30.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .bg(rgb(0x33412f))
                            .text_color(rgb(0xaacb93))
                            .child("Y"),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .child(div().text_sm().child("You"))
                            .child(div().text_xs().text_color(rgb(0x6f7580)).child("connected")),
                    ),
            )
    }
}

impl Render for ChattView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let count = self.messages.len();
        let timeline = list(
            self.list_state.clone(),
            cx.processor(Self::render_message),
        )
        .with_sizing_behavior(gpui::ListSizingBehavior::Auto)
        .flex_grow_1();

        div()
            .id("chatt")
            .key_context("Chatt")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::open_media))
            .on_action(cx.listener(Self::toggle_playback))
            .on_action(cx.listener(Self::seek_back))
            .on_action(cx.listener(Self::seek_forward))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                this.add_paths(paths.0.to_vec(), cx)
            }))
            .size_full()
            .flex()
            .bg(rgb(0x111317))
            .text_color(rgb(0xd9dbe0))
            .child(self.render_sidebar())
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .h(px(TOP_BAR_HEIGHT))
                            .flex_none()
                            .flex()
                            .items_center()
                            .gap_3()
                            .px_4()
                            .border_b_1()
                            .border_color(rgb(0x272a30))
                            .bg(rgb(0x14161a))
                            .child(div().text_color(rgb(0x777d87)).child("#"))
                            .child(
                                div()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("lobby"),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(rgb(0x747a84))
                                    .child(format!("{count} messages · end-to-end encrypted")),
                            )
                            .child(div().flex_1())
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(0x747a84))
                                    .child(self.media_status.clone()),
                            )
                            .child(
                                toolbar_button("add-media", "+  Add media").on_click(
                                    cx.listener(|this, _, window, cx| {
                                        this.open_media(&OpenMedia, window, cx)
                                    }),
                                ),
                            ),
                    )
                    .child(timeline)
                    .child(
                        div()
                            .h(px(COMPOSER_HEIGHT))
                            .flex_none()
                            .px_4()
                            .pt_3()
                            .pb_4()
                            .border_t_1()
                            .border_color(rgb(0x272a30))
                            .bg(rgb(0x111317))
                            .child(
                                div()
                                    .h_full()
                                    .flex()
                                    .items_center()
                                    .gap_3()
                                    .px_3()
                                    .border_1()
                                    .border_color(rgb(0x333740))
                                    .bg(rgb(0x191c21))
                                    .child(
                                        div()
                                            .text_color(rgb(0x747a84))
                                            .child("Message #lobby"),
                                    )
                                    .child(div().flex_1())
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(rgb(0x666c76))
                                            .child("Drop media anywhere · ⌘O"),
                                    ),
                            ),
                    ),
            )
    }
}

fn append_paths_to_messages(messages: &mut Vec<Message>, paths: Vec<PathBuf>) {
    let mut timestamp_ms = timeline::now_ms();
    let mut next_id = messages.last().map_or(1, |message| message.id + 1);
    for path in paths {
        let Some(attachment) = timeline::media_from_path(path) else {
            continue;
        };
        let name = attachment
            .path()
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "media".to_string());
        messages.push(Message {
            id: next_id,
            sender: "You".to_string(),
            body: name,
            timestamp_ms,
            local: true,
            edited: false,
            attachment: Some(attachment),
        });
        next_id += 1;
        timestamp_ms += 1;
    }
}

fn sender_color(sender: &str, local: bool) -> u32 {
    if local {
        return 0x9fbd89;
    }
    match sender.as_bytes().first().copied().unwrap_or_default() % 4 {
        0 => 0x8ca9d8,
        1 => 0xc49acb,
        2 => 0xd1a477,
        _ => 0x79b9b0,
    }
}

fn room_button(id: &'static str, sigil: &'static str, label: &'static str, active: bool) -> Stateful<Div> {
    div()
        .id(id)
        .mx_2()
        .h(px(34.0))
        .px_2()
        .flex()
        .items_center()
        .gap_2()
        .cursor_pointer()
        .bg(rgb(if active { 0x252930 } else { 0x0d0f12 }))
        .hover(|button| button.bg(rgb(0x202329)))
        .text_color(rgb(if active { 0xe0e2e6 } else { 0x969ca6 }))
        .child(div().w(px(16.0)).text_center().child(sigil))
        .child(label)
}

fn toolbar_button(id: &'static str, label: &'static str) -> Stateful<Div> {
    div()
        .id(id)
        .h(px(30.0))
        .px_3()
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .border_1()
        .border_color(rgb(0x363b44))
        .bg(rgb(0x202329))
        .hover(|button| button.bg(rgb(0x2b2f37)))
        .text_sm()
        .child(label)
}

fn mini_button(id: impl Into<gpui::ElementId>, label: &'static str) -> Stateful<Div> {
    div()
        .id(id)
        .size(px(28.0))
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .bg(rgb(0x262a31))
        .hover(|button| button.bg(rgb(0x343943)))
        .child(label)
}

fn media_button(id: impl Into<gpui::ElementId>, label: &'static str) -> Stateful<Div> {
    div()
        .id(id)
        .h(px(28.0))
        .px_2()
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .bg(rgb(0x262a31))
        .hover(|button| button.bg(rgb(0x343943)))
        .text_xs()
        .child(label)
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

#[cfg(test)]
mod tests {
    use super::format_time;

    #[test]
    fn formats_media_timestamps() {
        assert_eq!(format_time(0.0), "00:00");
        assert_eq!(format_time(65.9), "01:05");
        assert_eq!(format_time(3661.0), "01:01:01");
    }
}
