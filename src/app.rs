use std::{
    path::PathBuf,
    sync::{Arc, Mutex, mpsc::Receiver},
    time::Duration,
};

use gpui::{
    AnyElement, App, Context, Div, ExternalPaths, Focusable, FollowMode, FontWeight, KeyBinding,
    ListAlignment, ListState, ObjectFit, PathPromptOptions, Render, RenderImage, SharedString,
    Stateful, Task, Window, actions, div, img, list, prelude::*, px, relative, rgb,
};
use image::{Frame, RgbaImage};
use rpc::{
    daemon::{
        frame::{ClientFrame, DaemonFrame, Operation, RequestOutcome},
        model::{BulkTransferId, RequestId, RoomKind, TrustState},
    },
    ids::RoomId,
};

use crate::{
    composer::Composer,
    daemon::{
        client::{DaemonClient, DaemonEvent},
        reducer,
    },
    media_cache::MediaCache,
    model::{ChatModel, ConnectionPhase, PendingRequest},
    mpv_player::MpvPlayer,
    timeline::{self, Attachment},
};

const SIDEBAR_WIDTH: f32 = 232.0;
const TOP_BAR_HEIGHT: f32 = 52.0;
const MIN_COMPOSER_HEIGHT: f32 = 82.0;
const MIN_COMPOSER_FRAME_HEIGHT: f32 = 54.0;
const VIDEO_WIDTH: usize = 704;
const VIDEO_HEIGHT: usize = 396;

actions!(
    chatt_gui,
    [
        OpenMedia,
        SendMessage,
        TogglePlayback,
        SeekBack,
        SeekForward,
        ToggleMute,
        ToggleDeafen,
        ToggleVoice
    ]
);

pub fn bind_keys(cx: &mut App) {
    crate::composer::bind_keys(cx);
    cx.bind_keys([
        KeyBinding::new("cmd-o", OpenMedia, Some("Chatt")),
        KeyBinding::new("enter", SendMessage, Some("ChattComposer")),
        KeyBinding::new(
            "space",
            TogglePlayback,
            Some("Chatt && !ChattComposer"),
        ),
        KeyBinding::new("left", SeekBack, Some("Chatt")),
        KeyBinding::new("right", SeekForward, Some("Chatt")),
    ]);
}

pub struct ChattView {
    model: ChatModel,
    daemon: DaemonClient,
    daemon_events: Receiver<DaemonEvent>,
    next_request_id: u64,
    next_transfer_id: u64,
    editing: Option<(RoomId, rpc::ids::MessageId, String)>,
    composer: gpui::Entity<Composer>,
    media_cache: Arc<Mutex<MediaCache>>,
    list_state: ListState,
    player: Option<MpvPlayer>,
    active_video: Option<u64>,
    frame: Option<Arc<RenderImage>>,
    position: f64,
    duration: f64,
    paused: bool,
    media_volume: f64,
    status: SharedString,
    tick_count: u64,
    _tick_task: Task<()>,
}

impl ChattView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let model = ChatModel::default();
        let list_state = ListState::new(0, ListAlignment::Bottom, px(1_600.0));
        list_state.set_follow_mode(FollowMode::Tail);
        let media_cache = Arc::new(Mutex::new(
            MediaCache::new(512 * 1024 * 1024).expect("failed to create private media cache"),
        ));
        let (daemon, daemon_events) = DaemonClient::spawn(media_cache.clone());
        let composer = cx.new(Composer::new);
        window.focus(&composer.focus_handle(cx), cx);
        let (player, status) = match MpvPlayer::new() {
            Ok(player) => (Some(player), "Discovering Chatt daemon…".into()),
            Err(error) => (
                None,
                format!("Discovering daemon · video unavailable: {error}").into(),
            ),
        };
        let tick_task = cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(25))
                    .await;
                if this.update_in(cx, |this, _, cx| this.tick(cx)).is_err() {
                    return;
                }
            }
        });
        Self {
            model,
            daemon,
            daemon_events,
            next_request_id: 1,
            next_transfer_id: 1,
            editing: None,
            composer,
            media_cache,
            list_state,
            player,
            active_video: None,
            frame: None,
            position: 0.0,
            duration: 0.0,
            paused: true,
            media_volume: 100.0,
            status,
            tick_count: 0,
            _tick_task: tick_task,
        }
    }

    fn request_id(&mut self) -> RequestId {
        let id = self.next_request_id.clamp(1, (1u64 << 63) - 1);
        self.next_request_id = if id == (1u64 << 63) - 1 { 1 } else { id + 1 };
        RequestId(id)
    }

    fn transfer_id(&mut self) -> BulkTransferId {
        let id = self.next_transfer_id.clamp(1, (1u64 << 63) - 1);
        self.next_transfer_id = if id == (1u64 << 63) - 1 { 1 } else { id + 1 };
        BulkTransferId(id)
    }

    fn tick(&mut self, cx: &mut Context<Self>) {
        let mut changed = self.drain_daemon(cx);
        self.tick_count = self.tick_count.wrapping_add(1);
        if let Some(player) = self.player.as_mut()
            && self.active_video.is_some()
        {
            match player.render_frame(VIDEO_WIDTH, VIDEO_HEIGHT) {
                Ok(Some(frame)) => {
                    if let Some(buffer) =
                        RgbaImage::from_raw(frame.width, frame.height, frame.pixels)
                    {
                        self.frame = Some(Arc::new(RenderImage::new(vec![Frame::new(buffer)])));
                        changed = true;
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    self.status = format!("Video render failed: {error}").into();
                    changed = true;
                }
            }
            if self.tick_count.is_multiple_of(6) {
                self.position = player.position().unwrap_or(self.position).max(0.0);
                self.duration = player.duration().unwrap_or(self.duration).max(0.0);
                self.paused = player.paused().unwrap_or(self.paused);
                changed = true;
            }
        }
        if changed {
            cx.notify();
        }
    }

    fn drain_daemon(&mut self, cx: &mut Context<Self>) -> bool {
        let mut changed = false;
        while let Ok(event) = self.daemon_events.try_recv() {
            changed = true;
            match event {
                DaemonEvent::Discovering => self.model.phase = ConnectionPhase::Discovering,
                DaemonEvent::Connecting => self.model.phase = ConnectionPhase::Connecting,
                DaemonEvent::TransportConnected => self.model.phase = ConnectionPhase::Syncing,
                DaemonEvent::Disconnected(reason) => {
                    self.media_cache
                        .lock()
                        .expect("media cache lock poisoned")
                        .cancel_all();
                    self.model.resync_requested = false;
                    self.model.phase = ConnectionPhase::Disconnected {
                        reason: reason.clone(),
                    };
                    if !self.model.pending.is_empty() {
                        self.model.pending.clear();
                        self.model.last_error =
                            Some("Connection changed; pending operations were not replayed".into());
                    }
                    self.status = format!("Offline · {reason}").into();
                }
                DaemonEvent::Incompatible(details) => {
                    self.model.phase = ConnectionPhase::Incompatible {
                        details: details.clone(),
                    };
                    self.status = format!("Cannot connect · {details}").into();
                }
                DaemonEvent::UploadPreparationFailed {
                    begin_request,
                    finish_request,
                    reason,
                } => {
                    self.model.pending.remove(&begin_request);
                    self.model.pending.remove(&finish_request);
                    self.status = format!("Could not prepare upload · {reason}").into();
                }
                DaemonEvent::MediaTransferStarted => {
                    self.status = "Receiving attachment…".into();
                }
                DaemonEvent::MediaCached(descriptor) => {
                    self.status = format!("Cached {}", descriptor.file_name).into();
                }
                DaemonEvent::MediaTransferFailed {
                    transfer_id,
                    reason,
                } => {
                    self.media_cache
                        .lock()
                        .expect("media cache lock poisoned")
                        .cancel(transfer_id);
                    self.status = reason.into();
                }
                DaemonEvent::Frame(frame) => {
                    self.apply_daemon_state_frame(frame, cx);
                }
            }
        }
        changed
    }

    fn apply_daemon_state_frame(&mut self, frame: DaemonFrame, cx: &mut Context<Self>) {
        let pending = match &frame {
            DaemonFrame::RequestResult(result) => {
                self.model.pending.get(&result.request_id).cloned()
            }
            _ => None,
        };
        let old_len = self.model.messages.len();
        let effect = reducer::apply(&mut self.model, frame);
        if effect.replace_messages {
            self.list_state
                .splice(0..old_len, self.model.messages.len());
        }
        for (start, end, count) in effect.splices {
            self.list_state.splice(start..end, count);
        }
        if effect.request_resync {
            let request_id = self.request_id();
            if let Err(error) = self
                .daemon
                .send(ClientFrame::RequestSnapshot { request_id })
            {
                self.model.resync_requested = false;
                self.status = format!("Could not request daemon resync · {error}").into();
            }
        }
        if let Some(result) = effect.request_result {
            match result.outcome {
                RequestOutcome::Accepted => {
                    self.status = format!("{} accepted", operation_label(&result.operation)).into();
                    if let Some(pending) = pending.as_ref()
                        && matches!(
                            pending.operation,
                            Operation::SendMessage | Operation::EditMessage
                        )
                        && pending.draft.as_deref() == Some(self.composer.read(cx).text().as_str())
                    {
                        self.composer.update(cx, |composer, cx| composer.clear(cx));
                        if pending.operation == Operation::EditMessage {
                            self.editing = None;
                        }
                    }
                }
                RequestOutcome::Rejected { message, .. } => {
                    if let Some(transfer_id) = pending.as_ref().and_then(|pending| {
                        (pending.operation == Operation::BeginAttachmentRead)
                            .then_some(pending.transfer_id)
                            .flatten()
                    }) {
                        self.media_cache
                            .lock()
                            .expect("media cache lock poisoned")
                            .cancel(transfer_id);
                    }
                    self.status = if pending.as_ref().is_some_and(|pending| {
                        pending.room_id.is_some() && pending.room_id != self.model.selected_room
                    }) {
                        format!("Request for another room failed · {message}").into()
                    } else {
                        message.into()
                    };
                }
            }
        } else if self.model.is_ready() && self.model.last_error.is_none() {
            self.status = connection_label(&self.model).into();
        }
    }

    fn send_message(&mut self, _: &SendMessage, _: &mut Window, cx: &mut Context<Self>) {
        if !self.model.is_ready() {
            self.status = "Messages are disabled until daemon state is synced".into();
            cx.notify();
            return;
        }
        let Some(selected_room) = self.model.selected_room else {
            self.status = "Select a room before sending".into();
            cx.notify();
            return;
        };
        if self.composer.read(cx).is_empty() {
            return;
        }
        let draft = self.composer.read(cx).text();
        let request_id = self.request_id();
        let (operation, room_id, frame) = if let Some((room_id, target, _)) = self.editing {
            (
                Operation::EditMessage,
                room_id,
                ClientFrame::EditMessage {
                    request_id,
                    room_id,
                    target,
                    body: draft.clone(),
                },
            )
        } else {
            (
                Operation::SendMessage,
                selected_room,
                ClientFrame::SendMessage {
                    request_id,
                    room_id: selected_room,
                    body: draft.clone(),
                },
            )
        };
        self.model.pending.insert(
            request_id,
            PendingRequest {
                operation,
                room_id: Some(room_id),
                draft: Some(draft),
                transfer_id: None,
            },
        );
        if let Err(error) = self.daemon.send(frame) {
            self.model.pending.remove(&request_id);
            self.status = error.into();
        } else {
            self.status = "Sending…".into();
        }
        cx.notify();
    }

    fn begin_edit(
        &mut self,
        room_id: RoomId,
        message_id: rpc::ids::MessageId,
        body: String,
        cx: &mut Context<Self>,
    ) {
        if !self.model.is_ready() {
            return;
        }
        self.editing = Some((room_id, message_id, body.clone()));
        self.composer
            .update(cx, |composer, cx| composer.restore(body, cx));
        self.status = "Editing message · Enter saves".into();
        cx.notify();
    }

    fn delete_message(
        &mut self,
        room_id: RoomId,
        message_id: rpc::ids::MessageId,
        cx: &mut Context<Self>,
    ) {
        if !self.model.is_ready() {
            return;
        }
        let request_id = self.request_id();
        self.model.pending.insert(
            request_id,
            PendingRequest {
                operation: Operation::DeleteMessage,
                room_id: Some(room_id),
                draft: None,
                transfer_id: None,
            },
        );
        if let Err(error) = self.daemon.send(ClientFrame::DeleteMessage {
            request_id,
            room_id,
            target: message_id,
        }) {
            self.model.pending.remove(&request_id);
            self.status = error.into();
        } else {
            self.status = "Deleting…".into();
        }
        cx.notify();
    }

    fn select_room(&mut self, room_id: RoomId, cx: &mut Context<Self>) {
        if !self.model.is_ready() {
            return;
        }
        let request_id = self.request_id();
        self.model.pending.insert(
            request_id,
            PendingRequest {
                operation: Operation::SelectRoom,
                room_id: Some(room_id),
                draft: None,
                transfer_id: None,
            },
        );
        if let Err(error) = self.daemon.send(ClientFrame::SelectRoom {
            request_id,
            room_id,
        }) {
            self.model.pending.remove(&request_id);
            self.status = error.into();
        } else {
            self.status = "Switching room…".into();
        }
        cx.notify();
    }

    fn load_older(&mut self, cx: &mut Context<Self>) {
        if !self.model.is_ready() || self.model.at_start {
            return;
        }
        let (Some(room_id), before) = (self.model.selected_room, self.model.older_cursor) else {
            return;
        };
        let request_id = self.request_id();
        self.model.pending.insert(
            request_id,
            PendingRequest {
                operation: Operation::LoadOlder,
                room_id: Some(room_id),
                draft: None,
                transfer_id: None,
            },
        );
        if let Err(error) = self.daemon.send(ClientFrame::LoadOlder {
            request_id,
            room_id,
            before,
            limit: 200,
        }) {
            self.model.pending.remove(&request_id);
            self.status = error.into();
        } else {
            self.status = "Loading older messages…".into();
        }
        cx.notify();
    }

    fn retry(&mut self, cx: &mut Context<Self>) {
        self.daemon.retry();
        self.model.phase = ConnectionPhase::Discovering;
        self.status = "Retrying daemon connection…".into();
        cx.notify();
    }

    fn open_media(&mut self, _: &OpenMedia, window: &mut Window, cx: &mut Context<Self>) {
        if !self.model.is_ready() || self.model.selected_room.is_none() {
            self.status = "Uploads are disabled until a room is synced".into();
            cx.notify();
            return;
        }
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Upload files through Chatt".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = receiver.await else {
                return;
            };
            let _ = this.update_in(cx, |this, _, cx| this.queue_uploads(paths, cx));
        })
        .detach();
    }

    fn queue_uploads(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        let Some(room_id) = self.model.selected_room else {
            return;
        };
        for path in paths {
            let begin_request = self.request_id();
            let finish_request = self.request_id();
            let transfer_id = self.transfer_id();
            self.model.pending.insert(
                begin_request,
                PendingRequest {
                    operation: Operation::BeginUpload,
                    room_id: Some(room_id),
                    draft: None,
                    transfer_id: Some(transfer_id),
                },
            );
            self.model.pending.insert(
                finish_request,
                PendingRequest {
                    operation: Operation::FinishUpload,
                    room_id: Some(room_id),
                    draft: None,
                    transfer_id: Some(transfer_id),
                },
            );
            self.daemon.upload_file(
                path,
                room_id,
                transfer_id,
                begin_request,
                finish_request,
                self.model.limits.upload_bytes,
            );
        }
        self.status = "Preparing daemon upload…".into();
        cx.notify();
    }

    fn toggle_mute(&mut self, _: &ToggleMute, _: &mut Window, cx: &mut Context<Self>) {
        if !self.model.is_ready() {
            return;
        }
        let request_id = self.request_id();
        let muted = !self.model.voice.muted;
        self.model.pending.insert(
            request_id,
            PendingRequest {
                operation: Operation::SetMuted,
                room_id: self.model.selected_room,
                draft: None,
                transfer_id: None,
            },
        );
        if let Err(error) = self
            .daemon
            .send(ClientFrame::SetMuted { request_id, muted })
        {
            self.model.pending.remove(&request_id);
            self.status = error.into();
        }
        cx.notify();
    }

    fn toggle_deafen(&mut self, _: &ToggleDeafen, _: &mut Window, cx: &mut Context<Self>) {
        if !self.model.is_ready() {
            return;
        }
        let request_id = self.request_id();
        let deafened = !self.model.voice.deafened;
        self.model.pending.insert(
            request_id,
            PendingRequest {
                operation: Operation::SetDeafened,
                room_id: self.model.selected_room,
                draft: None,
                transfer_id: None,
            },
        );
        if let Err(error) = self.daemon.send(ClientFrame::SetDeafened {
            request_id,
            deafened,
        }) {
            self.model.pending.remove(&request_id);
            self.status = error.into();
        }
        cx.notify();
    }

    fn toggle_voice(&mut self, _: &ToggleVoice, _: &mut Window, cx: &mut Context<Self>) {
        if !self.model.is_ready() {
            return;
        }
        let Some(room_id) = self.model.selected_room else {
            return;
        };
        let request_id = self.request_id();
        let (operation, frame) = if self.model.voice.joined_room == Some(room_id) {
            (
                Operation::LeaveVoice,
                ClientFrame::LeaveVoice { request_id },
            )
        } else {
            (
                Operation::JoinVoice,
                ClientFrame::JoinVoice {
                    request_id,
                    room_id,
                },
            )
        };
        self.model.pending.insert(
            request_id,
            PendingRequest {
                operation,
                room_id: Some(room_id),
                draft: None,
                transfer_id: None,
            },
        );
        if let Err(error) = self.daemon.send(frame) {
            self.model.pending.remove(&request_id);
            self.status = error.into();
        }
        cx.notify();
    }

    fn adjust_output_volume(&mut self, delta: f32, cx: &mut Context<Self>) {
        if !self.model.is_ready() {
            return;
        }
        let request_id = self.request_id();
        let volume = (self.model.voice.output_volume + delta)
            .clamp(0., rpc::daemon::MAX_OUTPUT_VOLUME_PERCENT);
        self.model.pending.insert(
            request_id,
            PendingRequest {
                operation: Operation::SetOutputVolume,
                room_id: self.model.selected_room,
                draft: None,
                transfer_id: None,
            },
        );
        if let Err(error) = self
            .daemon
            .send(ClientFrame::SetOutputVolume { request_id, volume })
        {
            self.model.pending.remove(&request_id);
            self.status = error.into();
        }
        cx.notify();
    }

    fn cancel_file_transfer(
        &mut self,
        transfer_id: rpc::ids::FileTransferId,
        cx: &mut Context<Self>,
    ) {
        if !self.model.is_ready() {
            return;
        }
        let request_id = self.request_id();
        self.model.pending.insert(
            request_id,
            PendingRequest {
                operation: Operation::CancelFileTransfer,
                room_id: self.model.selected_room,
                draft: None,
                transfer_id: None,
            },
        );
        if let Err(error) = self.daemon.send(ClientFrame::CancelFileTransfer {
            request_id,
            transfer_id,
        }) {
            self.model.pending.remove(&request_id);
            self.status = error.into();
        }
        cx.notify();
    }

    fn cancel_bulk_read(&mut self, transfer_id: BulkTransferId, cx: &mut Context<Self>) {
        if !self.model.is_ready() {
            return;
        }
        let request_id = self.request_id();
        self.model.pending.insert(
            request_id,
            PendingRequest {
                operation: Operation::CancelBulkTransfer,
                room_id: self.model.selected_room,
                draft: None,
                transfer_id: Some(transfer_id),
            },
        );
        if let Err(error) = self.daemon.send(ClientFrame::CancelBulkTransfer {
            request_id,
            transfer_id,
        }) {
            self.model.pending.remove(&request_id);
            self.status = error.into();
        }
        cx.notify();
    }

    fn render_message(
        &mut self,
        index: usize,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(message) = self.model.messages.get(index).cloned() else {
            return div().into_any_element();
        };
        let continuation = timeline::is_continuation(&self.model.messages, index);
        let accent = sender_color(&message.sender, message.local);
        let background = if message.notice {
            0x15181c
        } else if message.local {
            0x171a20
        } else {
            0x111317
        };
        div()
            .id(("message", message.id as usize))
            .w_full()
            .pl(px(64.))
            .pr(px(28.))
            .py(px(if continuation { 3. } else { 10. }))
            .bg(rgb(background))
            .hover(|row| row.bg(rgb(0x1b1e24)))
            .child(
                div()
                    .w_full()
                    .max_w(px(860.))
                    .flex()
                    .child(
                        div()
                            .w(px(3.))
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
                                        .h(px(24.))
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
                                        .when(message.unverified, |meta| {
                                            meta.child(
                                                div()
                                                    .text_xs()
                                                    .text_color(rgb(0xc49a74))
                                                    .child("unverified"),
                                            )
                                        })
                                        .child(div().flex_1())
                                        .when(message.local && !message.notice, |header| {
                                            let edit_body = message.body.clone();
                                            let edit_room = message.room_id;
                                            let edit_id = rpc::ids::MessageId(message.id);
                                            header
                                                .child(
                                                    mini_button(
                                                        ("edit", message.id as usize),
                                                        "Edit",
                                                    )
                                                    .on_click(cx.listener(move |this, _, _, cx| {
                                                        this.begin_edit(
                                                            edit_room,
                                                            edit_id,
                                                            edit_body.clone(),
                                                            cx,
                                                        )
                                                    })),
                                                )
                                                .child(
                                                    mini_button(
                                                        ("delete", message.id as usize),
                                                        "Delete",
                                                    )
                                                    .on_click(cx.listener(move |this, _, _, cx| {
                                                        this.delete_message(edit_room, edit_id, cx)
                                                    })),
                                                )
                                        })
                                        .child(div().text_xs().text_color(rgb(0x777d87)).child(
                                            timeline::format_age(
                                                message.timestamp_ms,
                                                timeline::now_ms(),
                                            ),
                                        )),
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
                                content.child(self.render_attachment(message.id, attachment, cx))
                            }),
                    ),
            )
            .into_any_element()
    }

    fn render_attachment(
        &mut self,
        message_id: u64,
        attachment: Attachment,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let descriptor = attachment.descriptor.clone();
        let (cache_path, active_transfer) = {
            let mut cache = self.media_cache.lock().expect("media cache lock poisoned");
            (
                cache.path_for(&descriptor),
                cache.active_transfer(&descriptor),
            )
        };
        if let Some(transfer_id) = active_transfer {
            return div()
                .id(("attachment-active", message_id as usize))
                .mt_2()
                .px_3()
                .py_2()
                .flex()
                .items_center()
                .gap_3()
                .border_1()
                .border_color(rgb(0x596a90))
                .bg(rgb(0x171a20))
                .child(
                    div()
                        .flex_1()
                        .text_sm()
                        .text_color(rgb(0x9aa1ac))
                        .child(format!("Fetching {}…", descriptor.file_name)),
                )
                .child(
                    mini_button(("cancel-read", transfer_id.0 as usize), "Cancel").on_click(
                        cx.listener(move |this, _, _, cx| this.cancel_bulk_read(transfer_id, cx)),
                    ),
                )
                .into_any_element();
        }
        if attachment.is_image()
            && let Some(path) = cache_path.clone()
        {
            let (width, height) = timeline::media_box_size(
                descriptor.width.unwrap_or(4),
                descriptor.height.unwrap_or(3),
            );
            return img(path)
                .id(("image", message_id as usize))
                .mt_2()
                .w(px(width))
                .h(px(height))
                .max_w_full()
                .object_fit(ObjectFit::Contain)
                .into_any_element();
        }
        if attachment.is_video()
            && let Some(path) = cache_path
        {
            let active = self.active_video == Some(message_id);
            let frame = active.then(|| self.frame.clone()).flatten();
            let progress = if active && self.duration > 0. {
                (self.position / self.duration).clamp(0., 1.) as f32
            } else {
                0.
            };
            let play_path = path.clone();
            return div()
                .id(("video", message_id as usize))
                .mt_2()
                .w(px(704.))
                .max_w_full()
                .border_1()
                .border_color(rgb(if active { 0x596a90 } else { 0x292d34 }))
                .bg(rgb(0x08090b))
                .child(
                    div()
                        .h(px(396.))
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
                                    .child(div().text_sm().child(descriptor.file_name.clone())),
                            )
                        }),
                )
                .child(
                    div()
                        .h(px(48.))
                        .flex()
                        .items_center()
                        .gap_3()
                        .px_3()
                        .border_t_1()
                        .border_color(rgb(0x24272d))
                        .child(
                            mini_button(("video-back", message_id as usize), "−10").on_click(
                                cx.listener(|this, _, _, cx| this.seek_relative(-10., cx)),
                            ),
                        )
                        .child(
                            mini_button(
                                ("video-play", message_id as usize),
                                if active && !self.paused { "Ⅱ" } else { "▶" },
                            )
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    if this.active_video == Some(message_id) {
                                        this.toggle_playback_inner(cx);
                                    } else {
                                        this.activate_video(message_id, play_path.clone(), cx);
                                    }
                                },
                            )),
                        )
                        .child(
                            mini_button(("video-forward", message_id as usize), "+10").on_click(
                                cx.listener(|this, _, _, cx| this.seek_relative(10., cx)),
                            ),
                        )
                        .child(
                            div()
                                .flex_1()
                                .h(px(4.))
                                .bg(rgb(0x30343b))
                                .child(div().h_full().w(relative(progress)).bg(rgb(0x748bbd))),
                        )
                        .child(
                            div()
                                .w(px(94.))
                                .text_right()
                                .text_xs()
                                .text_color(rgb(0x989ea8))
                                .child(format!(
                                    "{} / {}",
                                    format_time(if active { self.position } else { 0. }),
                                    format_time(if active { self.duration } else { 0. })
                                )),
                        )
                        .child(
                            mini_button(("volume-down", message_id as usize), "−").on_click(
                                cx.listener(|this, _, _, cx| this.adjust_media_volume(-5., cx)),
                            ),
                        )
                        .child(
                            div()
                                .w(px(34.))
                                .text_center()
                                .text_xs()
                                .text_color(rgb(0x989ea8))
                                .child(self.media_volume.round().to_string()),
                        )
                        .child(
                            mini_button(("volume-up", message_id as usize), "+").on_click(
                                cx.listener(|this, _, _, cx| this.adjust_media_volume(5., cx)),
                            ),
                        ),
                )
                .into_any_element();
        }
        let fetch = descriptor.clone();
        div()
            .id(("attachment", message_id as usize))
            .mt_2()
            .px_3()
            .py_2()
            .border_1()
            .border_color(rgb(0x30343b))
            .bg(rgb(0x171a20))
            .cursor_pointer()
            .hover(|item| item.bg(rgb(0x20242a)))
            .text_sm()
            .text_color(rgb(0x9aa1ac))
            .child(format!(
                "{} · {} bytes · click to fetch",
                descriptor.file_name, descriptor.byte_len
            ))
            .on_click(cx.listener(move |this, _, _, cx| this.fetch_attachment(fetch.clone(), cx)))
            .into_any_element()
    }

    fn fetch_attachment(
        &mut self,
        descriptor: rpc::daemon::model::AttachmentDescriptor,
        cx: &mut Context<Self>,
    ) {
        if !self.model.is_ready()
            || self
                .media_cache
                .lock()
                .expect("media cache lock poisoned")
                .path_for(&descriptor)
                .is_some()
        {
            return;
        }
        let Some(room_id) = self.model.selected_room else {
            return;
        };
        let request_id = self.request_id();
        let transfer_id = self.transfer_id();
        if let Err(error) = self
            .media_cache
            .lock()
            .expect("media cache lock poisoned")
            .reserve(transfer_id, &descriptor)
        {
            self.status = error.into();
            cx.notify();
            return;
        }
        self.model.pending.insert(
            request_id,
            PendingRequest {
                operation: Operation::BeginAttachmentRead,
                room_id: Some(room_id),
                draft: None,
                transfer_id: Some(transfer_id),
            },
        );
        let read = rpc::daemon::bulk::BeginAttachmentRead {
            transfer_id,
            room_id,
            attachment_id: descriptor.id,
        };
        if let Err(error) = self
            .daemon
            .send(ClientFrame::BeginAttachmentRead { request_id, read })
        {
            self.model.pending.remove(&request_id);
            self.media_cache
                .lock()
                .expect("media cache lock poisoned")
                .cancel(transfer_id);
            self.status = error.into();
        } else {
            self.status = format!("Fetching {}…", descriptor.file_name).into();
        }
        cx.notify();
    }

    fn activate_video(&mut self, message_id: u64, path: PathBuf, cx: &mut Context<Self>) {
        let Some(player) = self.player.as_mut() else {
            return;
        };
        match player.load(&path.to_string_lossy()) {
            Ok(()) => {
                self.active_video = Some(message_id);
                self.frame = None;
                self.position = 0.;
                self.duration = 0.;
                self.paused = false;
                self.status = "Playing cached attachment".into();
            }
            Err(error) => self.status = format!("Could not open video: {error}").into(),
        }
        cx.notify();
    }

    fn toggle_playback_inner(&mut self, cx: &mut Context<Self>) {
        if let Some(player) = self.player.as_ref() {
            match player.toggle_pause() {
                Ok(paused) => self.paused = paused,
                Err(error) => self.status = format!("Playback failed: {error}").into(),
            }
            cx.notify();
        }
    }
    fn adjust_media_volume(&mut self, delta: f64, cx: &mut Context<Self>) {
        self.media_volume = (self.media_volume + delta).clamp(0., 100.);
        if let Some(player) = self.player.as_ref()
            && let Err(error) = player.set_volume(self.media_volume)
        {
            self.status = format!("Volume failed: {error}").into();
        }
        cx.notify();
    }
    fn toggle_playback(&mut self, _: &TogglePlayback, _: &mut Window, cx: &mut Context<Self>) {
        if self.active_video.is_some() {
            self.toggle_playback_inner(cx);
        }
    }
    fn seek_relative(&mut self, seconds: f64, cx: &mut Context<Self>) {
        if let Some(player) = self.player.as_ref()
            && let Err(error) = player.seek_relative(seconds)
        {
            self.status = format!("Seek failed: {error}").into();
        }
        cx.notify();
    }
    fn seek_back(&mut self, _: &SeekBack, _: &mut Window, cx: &mut Context<Self>) {
        if self.active_video.is_some() {
            self.seek_relative(-10., cx);
        }
    }
    fn seek_forward(&mut self, _: &SeekForward, _: &mut Window, cx: &mut Context<Self>) {
        if self.active_video.is_some() {
            self.seek_relative(10., cx);
        }
    }

    fn render_sidebar(&mut self, cx: &mut Context<Self>) -> Div {
        let mut sidebar = div()
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
            );
        for room in self.model.rooms.clone() {
            let active = self.model.selected_room == Some(room.id);
            let sigil = match room.kind {
                RoomKind::Direct => "●",
                RoomKind::Private => "◆",
                RoomKind::Public => "#",
            };
            let unread = room.unread;
            let room_id = room.id;
            let label = if room.voice_active {
                format!("{}  ◉", room.name)
            } else {
                room.name
            };
            sidebar = sidebar.child(
                room_button(("room", room.id.0 as usize), sigil, label, active, unread)
                    .on_click(cx.listener(move |this, _, _, cx| this.select_room(room_id, cx))),
            );
        }
        if !self.model.transfers.is_empty() {
            sidebar = sidebar.child(
                div()
                    .mt_4()
                    .px_3()
                    .pb_2()
                    .text_xs()
                    .text_color(rgb(0x676d77))
                    .child("TRANSFERS"),
            );
            for transfer in self.model.transfers.clone() {
                let percent = if transfer.byte_len == 0 {
                    0
                } else {
                    transfer.transferred.saturating_mul(100) / transfer.byte_len
                };
                let transfer_id = transfer.transfer_id;
                sidebar = sidebar.child(
                    div()
                        .mx_2()
                        .px_2()
                        .py_2()
                        .flex()
                        .items_center()
                        .gap_2()
                        .text_xs()
                        .text_color(rgb(0x969ca6))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .child(format!("{} · {percent}%", transfer.file_name)),
                        )
                        .child(
                            mini_button(("cancel-transfer", transfer_id.0 as usize), "×").on_click(
                                cx.listener(move |this, _, _, cx| {
                                    this.cancel_file_transfer(transfer_id, cx)
                                }),
                            ),
                        ),
                );
            }
        }
        let identity = self
            .model
            .local_identity
            .clone()
            .unwrap_or_else(|| "No identity".into());
        let connection = connection_label(&self.model);
        sidebar.child(div().flex_1()).child(
            div()
                .h(px(58.))
                .flex_none()
                .flex()
                .items_center()
                .gap_3()
                .px_3()
                .border_t_1()
                .border_color(rgb(0x272a30))
                .child(
                    div()
                        .size(px(30.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .bg(rgb(0x33412f))
                        .text_color(rgb(0xaacb93))
                        .child(identity.chars().next().unwrap_or('?').to_string()),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .child(div().text_sm().child(identity))
                        .child(div().text_xs().text_color(rgb(0x6f7580)).child(connection)),
                ),
        )
    }
}

impl Render for ChattView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let count = self.model.messages.len();
        let online = self
            .model
            .participants
            .iter()
            .filter(|participant| participant.online)
            .count();
        let selected_name = self
            .model
            .selected_room()
            .map(|room| room.name.clone())
            .unwrap_or_else(|| "No room selected".into());
        let security = self
            .model
            .selected_room()
            .map(|room| security_label(room.trust))
            .unwrap_or("");
        let ready = self.model.is_ready();
        let timeline = list(self.list_state.clone(), cx.processor(Self::render_message))
            .with_sizing_behavior(gpui::ListSizingBehavior::Auto)
            .flex_grow_1();
        div()
            .id("chatt")
            .key_context("Chatt")
            .on_action(cx.listener(Self::open_media))
            .on_action(cx.listener(Self::send_message))
            .on_action(cx.listener(Self::toggle_playback))
            .on_action(cx.listener(Self::seek_back))
            .on_action(cx.listener(Self::seek_forward))
            .on_action(cx.listener(Self::toggle_mute))
            .on_action(cx.listener(Self::toggle_deafen))
            .on_action(cx.listener(Self::toggle_voice))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                this.queue_uploads(paths.0.to_vec(), cx)
            }))
            .size_full()
            .flex()
            .bg(rgb(0x111317))
            .text_color(rgb(0xd9dbe0))
            .child(self.render_sidebar(cx))
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
                                    .child(selected_name.clone()),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(rgb(0x747a84))
                                    .child(format!("{count} messages")),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(rgb(0x747a84))
                                    .child(format!("{online} online")),
                            )
                            .when(!security.is_empty(), |bar| {
                                bar.child(div().text_xs().text_color(rgb(0x8b929d)).child(security))
                            })
                            .child(div().flex_1())
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(if ready { 0x7f9c70 } else { 0xc49a74 }))
                                    .child(self.status.clone()),
                            )
                            .when(!ready, |bar| {
                                bar.child(
                                    toolbar_button("retry", "Retry")
                                        .on_click(cx.listener(|this, _, _, cx| this.retry(cx))),
                                )
                            })
                            .child(
                                toolbar_button(
                                    "mute",
                                    if self.model.voice.muted {
                                        "Unmute"
                                    } else {
                                        "Mute"
                                    },
                                )
                                .on_click(cx.listener(
                                    |this, _, window, cx| this.toggle_mute(&ToggleMute, window, cx),
                                )),
                            )
                            .child(
                                toolbar_button(
                                    "deafen",
                                    if self.model.voice.deafened {
                                        "Undeafen"
                                    } else {
                                        "Deafen"
                                    },
                                )
                                .on_click(cx.listener(
                                    |this, _, window, cx| {
                                        this.toggle_deafen(&ToggleDeafen, window, cx)
                                    },
                                )),
                            )
                            .child(toolbar_button("output-down", "Vol −").on_click(
                                cx.listener(|this, _, _, cx| this.adjust_output_volume(-5., cx)),
                            ))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(0x8b929d))
                                    .child(format!("{}", self.model.voice.output_volume.round())),
                            )
                            .child(toolbar_button("output-up", "Vol +").on_click(
                                cx.listener(|this, _, _, cx| this.adjust_output_volume(5., cx)),
                            ))
                            .child(
                                toolbar_button(
                                    "voice",
                                    if self.model.voice.joined_room == self.model.selected_room
                                        && self.model.selected_room.is_some()
                                    {
                                        "Leave voice"
                                    } else {
                                        "Join voice"
                                    },
                                )
                                .on_click(cx.listener(
                                    |this, _, window, cx| {
                                        this.toggle_voice(&ToggleVoice, window, cx)
                                    },
                                )),
                            )
                            .child(
                                toolbar_button("add-media", "+ Upload").on_click(cx.listener(
                                    |this, _, window, cx| this.open_media(&OpenMedia, window, cx),
                                )),
                            ),
                    )
                    .when(
                        !self.model.at_start && self.model.older_cursor.is_some(),
                        |panel| {
                            panel.child(
                                div()
                                    .h(px(34.))
                                    .flex_none()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .border_b_1()
                                    .border_color(rgb(0x272a30))
                                    .child(
                                        toolbar_button("load-older", "Load older messages")
                                            .on_click(
                                                cx.listener(|this, _, _, cx| this.load_older(cx)),
                                            ),
                                    ),
                            )
                        },
                    )
                    .child(timeline)
                    .when(count == 0, |panel| {
                        panel.child(
                            div()
                                .absolute()
                                .left(px(SIDEBAR_WIDTH + 30.))
                                .top(px(TOP_BAR_HEIGHT + 40.))
                                .text_color(rgb(0x747a84))
                                .child(empty_state(&self.model)),
                        )
                    })
                    .child(
                        div()
                            .min_h(px(MIN_COMPOSER_HEIGHT))
                            .flex_none()
                            .px_4()
                            .pt_3()
                            .pb_4()
                            .border_t_1()
                            .border_color(rgb(0x272a30))
                            .bg(rgb(0x111317))
                            .child(
                                div()
                                    .min_h(px(MIN_COMPOSER_FRAME_HEIGHT))
                                    .flex()
                                    .items_center()
                                    .gap_3()
                                    .px_3()
                                    .border_1()
                                    .border_color(rgb(if ready { 0x333740 } else { 0x292c32 }))
                                    .bg(rgb(0x191c21))
                                    .child(self.composer.clone())
                                    .child(
                                        toolbar_button(
                                            "send",
                                            if !ready {
                                                "Offline"
                                            } else if self.editing.is_some() {
                                                "Save edit"
                                            } else {
                                                "Send"
                                            },
                                        )
                                        .on_click(
                                            cx.listener(|this, _, window, cx| {
                                                this.send_message(&SendMessage, window, cx)
                                            }),
                                        ),
                                    ),
                            ),
                    ),
            )
    }
}

fn connection_label(model: &ChatModel) -> String {
    match &model.phase {
        ConnectionPhase::Discovering => "Discovering daemon…".into(),
        ConnectionPhase::Connecting => "Connecting…".into(),
        ConnectionPhase::Syncing => "Syncing…".into(),
        ConnectionPhase::Ready => match model.server_connection {
            rpc::daemon::model::ConnectionState::Online => {
                model.active_server.as_ref().map_or_else(
                    || "Connected".into(),
                    |server| format!("Connected · {server}"),
                )
            }
            rpc::daemon::model::ConnectionState::Connecting => {
                "Daemon ready · server connecting…".into()
            }
            rpc::daemon::model::ConnectionState::Offline => "Daemon ready · server offline".into(),
        },
        ConnectionPhase::Disconnected { .. } => "Daemon offline".into(),
        ConnectionPhase::Incompatible { .. } => "Daemon incompatible".into(),
    }
}

fn empty_state(model: &ChatModel) -> String {
    match &model.phase {
        ConnectionPhase::Disconnected { reason } => {
            format!("Chatt daemon is unavailable. Start it with `chatt daemon`.\n{reason}")
        }
        ConnectionPhase::Incompatible { details } => {
            format!("This GUI cannot use the daemon: {details}")
        }
        ConnectionPhase::Ready if model.selected_room.is_none() => {
            "Choose a room from the sidebar.".into()
        }
        ConnectionPhase::Ready => "No messages in this room.".into(),
        _ => "Connecting to the private Chatt control socket…".into(),
    }
}

fn operation_label(operation: &Operation) -> &'static str {
    match operation {
        Operation::SelectRoom => "Room selection",
        Operation::SendMessage => "Message",
        Operation::EditMessage => "Edit",
        Operation::DeleteMessage => "Delete",
        Operation::SetMuted => "Mute change",
        Operation::SetDeafened => "Deafen change",
        Operation::JoinVoice => "Voice join",
        Operation::LeaveVoice => "Voice leave",
        Operation::SetOutputVolume => "Volume change",
        Operation::BeginUpload => "Upload",
        Operation::CancelBulkTransfer => "Attachment cancellation",
        Operation::CancelFileTransfer => "File transfer cancellation",
        _ => "Request",
    }
}

fn security_label(trust: TrustState) -> &'static str {
    match trust {
        TrustState::NotApplicable => "",
        TrustState::Unverified => "E2E unverified",
        TrustState::Verified => "E2E verified",
        TrustState::Changed => "E2E identity changed",
    }
}

fn sender_color(sender: &str, local: bool) -> u32 {
    if local {
        0x9fbd89
    } else {
        match sender.as_bytes().first().copied().unwrap_or_default() % 4 {
            0 => 0x8ca9d8,
            1 => 0xc49acb,
            2 => 0xd1a477,
            _ => 0x79b9b0,
        }
    }
}

fn room_button(
    id: impl Into<gpui::ElementId>,
    sigil: &'static str,
    label: String,
    active: bool,
    unread: u32,
) -> Stateful<Div> {
    div()
        .id(id)
        .mx_2()
        .h(px(34.))
        .px_2()
        .flex()
        .items_center()
        .gap_2()
        .cursor_pointer()
        .bg(rgb(if active { 0x252930 } else { 0x0d0f12 }))
        .hover(|button| button.bg(rgb(0x202329)))
        .text_color(rgb(if active { 0xe0e2e6 } else { 0x969ca6 }))
        .child(div().w(px(16.)).text_center().child(sigil))
        .child(div().flex_1().child(label))
        .when(unread > 0, |button| {
            button.child(
                div()
                    .text_xs()
                    .px_2()
                    .bg(rgb(0x536987))
                    .child(unread.to_string()),
            )
        })
}

fn toolbar_button(id: &'static str, label: &'static str) -> Stateful<Div> {
    div()
        .id(id)
        .h(px(30.))
        .px_3()
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .border_1()
        .border_color(rgb(0x363b44))
        .bg(rgb(0x202329))
        .hover(|button| button.bg(rgb(0x292d34)))
        .text_xs()
        .child(label)
}
fn mini_button(id: impl Into<gpui::ElementId>, label: &'static str) -> Stateful<Div> {
    div()
        .id(id)
        .h(px(28.))
        .px_2()
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .border_1()
        .border_color(rgb(0x353a43))
        .bg(rgb(0x22262c))
        .text_xs()
        .child(label)
}
fn format_time(seconds: f64) -> String {
    let seconds = seconds.max(0.).round() as u64;
    format!("{}:{:02}", seconds / 60, seconds % 60)
}
