use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Instant,
};

use gpui::{
    AnyElement, App, Context, Div, ExternalPaths, Focusable, FollowMode, FontWeight, KeyBinding,
    ListAlignment, ListState, LruImageCache, ObjectFit, PathPromptOptions, Render, RenderImage,
    ScrollWheelEvent, SharedString, Stateful, Task, Window, actions, div, img, list, prelude::*, px,
    relative, rgb, rgba,
};
use image::{Frame, RgbaImage};
use markdown::{
    Markdown, MarkdownElement, MarkdownFont, MarkdownSelectionArea, MarkdownSelectionGroup,
    MarkdownSelectionKey, MarkdownStyle,
};
use rpc::{
    daemon::{
        frame::{ClientFrame, DaemonFrame, Operation, RequestOutcome},
        model::{
            AttachmentDescriptor, AttachmentId, BulkTransferId, RequestId, RoomKind, TrustState,
        },
    },
    ids::RoomId,
};

use crate::{
    composer::Composer,
    daemon::{
        client::{DaemonClient, DaemonEvent},
        reducer,
    },
    image_cache::TimelineImageLoader,
    media_cache::MediaCache,
    model::{ChatModel, ConnectionPhase, PendingRequest},
    mpv_player::MpvPlayer,
    scroll_capture::capture_scroll,
    timeline::{self, Attachment},
};

const SIDEBAR_WIDTH: f32 = 232.0;
const TOP_BAR_HEIGHT: f32 = 52.0;
const MIN_COMPOSER_HEIGHT: f32 = 82.0;
const MIN_COMPOSER_FRAME_HEIGHT: f32 = 54.0;
const VIDEO_WIDTH: usize = 704;
const VIDEO_HEIGHT: usize = 396;
const DECODED_IMAGE_CACHE_BYTES: usize = 64 * 1024 * 1024;
const SCROLL_RESPONSE_SECONDS: f32 = 0.006;
const SCROLL_SETTLE_THRESHOLD: f32 = 0.50;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct EagerImageKey {
    room_id: RoomId,
    attachment_id: AttachmentId,
    digest: [u8; 32],
}

#[derive(Clone, Debug)]
struct EagerImageFetch {
    key: EagerImageKey,
    descriptor: AttachmentDescriptor,
}

impl EagerImageFetch {
    fn new(room_id: RoomId, descriptor: AttachmentDescriptor) -> Self {
        Self {
            key: EagerImageKey {
                room_id,
                attachment_id: descriptor.id,
                digest: descriptor.digest,
            },
            descriptor,
        }
    }
}

#[derive(Default)]
struct EagerImageFetches {
    queued: VecDeque<EagerImageFetch>,
    queued_keys: HashSet<EagerImageKey>,
    active: HashMap<BulkTransferId, EagerImageFetch>,
    failures: HashMap<EagerImageKey, String>,
    pump_scheduled: bool,
}

impl EagerImageFetches {
    fn enqueue(&mut self, fetch: EagerImageFetch) -> bool {
        if self.failures.contains_key(&fetch.key)
            || self.queued_keys.contains(&fetch.key)
            || self.active.values().any(|active| active.key == fetch.key)
        {
            return false;
        }
        self.queued_keys.insert(fetch.key);
        self.queued.push_back(fetch);
        true
    }

    fn pop_front(&mut self) -> Option<EagerImageFetch> {
        let fetch = self.queued.pop_front()?;
        self.queued_keys.remove(&fetch.key);
        Some(fetch)
    }

    fn started(&mut self, transfer_id: BulkTransferId, fetch: EagerImageFetch) {
        self.active.insert(transfer_id, fetch);
    }

    fn fail_to_start(&mut self, fetch: EagerImageFetch, reason: String) {
        self.failures.insert(fetch.key, reason);
    }

    fn failed(&mut self, transfer_id: BulkTransferId, reason: String) {
        if let Some(fetch) = self.active.remove(&transfer_id) {
            self.failures.insert(fetch.key, reason);
        }
    }

    fn cached(&mut self, descriptor: &AttachmentDescriptor) {
        self.active.retain(|_, fetch| {
            fetch.descriptor.id != descriptor.id || fetch.descriptor.digest != descriptor.digest
        });
        self.failures.retain(|key, _| {
            key.attachment_id != descriptor.id || key.digest != descriptor.digest
        });
    }

    fn failure(&self, key: EagerImageKey) -> Option<&str> {
        self.failures.get(&key).map(String::as_str)
    }

    fn retry(&mut self, fetch: EagerImageFetch) -> bool {
        self.failures.remove(&fetch.key);
        self.enqueue(fetch)
    }

    fn reset_transient(&mut self) {
        self.queued.clear();
        self.queued_keys.clear();
        self.active.clear();
        self.pump_scheduled = false;
    }
}

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
        KeyBinding::new("cmd-c", markdown::Copy, Some("Markdown")),
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
    next_request_id: u64,
    next_transfer_id: u64,
    editing: Option<(RoomId, rpc::ids::MessageId, String)>,
    composer: gpui::Entity<Composer>,
    media_cache: Arc<Mutex<MediaCache>>,
    image_cache: gpui::Entity<LruImageCache<TimelineImageLoader>>,
    eager_image_fetches: EagerImageFetches,
    list_state: ListState,
    pending_scroll: gpui::Pixels,
    scroll_animation_active: bool,
    last_scroll_frame: Option<Instant>,
    message_markdown: HashMap<u64, gpui::Entity<Markdown>>,
    timeline_selection: MarkdownSelectionGroup,
    player: Option<MpvPlayer>,
    video_wakeup: async_channel::Sender<()>,
    active_video: Option<u64>,
    frame: Option<Arc<RenderImage>>,
    position: f64,
    duration: f64,
    paused: bool,
    media_volume: f64,
    status: SharedString,
    _daemon_task: Task<()>,
    _video_task: Task<()>,
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
        let timeline_selection = MarkdownSelectionGroup::new(cx.focus_handle());
        let image_cache =
            LruImageCache::<TimelineImageLoader>::new(DECODED_IMAGE_CACHE_BYTES, cx);
        window.focus(&composer.focus_handle(cx), cx);
        let daemon_task = cx.spawn_in(window, async move |this, cx| {
            while let Ok(first_event) = daemon_events.recv().await {
                let mut events = vec![first_event];
                while let Ok(event) = daemon_events.try_recv() {
                    events.push(event);
                }
                if this
                    .update_in(cx, |this, window, cx| {
                        for event in events {
                            this.apply_daemon_event(event, window, cx);
                        }
                        cx.notify();
                    })
                    .is_err()
                {
                    return;
                }
            }
        });
        let (video_wakeup, video_updates) = async_channel::bounded(1);
        let video_task = cx.spawn_in(window, async move |this, cx| {
            while video_updates.recv().await.is_ok() {
                while video_updates.try_recv().is_ok() {}
                if this
                    .update_in(cx, |_, window, _| window.refresh())
                    .is_err()
                {
                    return;
                }
            }
        });
        Self {
            model,
            daemon,
            next_request_id: 1,
            next_transfer_id: 1,
            editing: None,
            composer,
            media_cache,
            image_cache,
            eager_image_fetches: EagerImageFetches::default(),
            list_state,
            pending_scroll: px(0.),
            scroll_animation_active: false,
            last_scroll_frame: None,
            message_markdown: HashMap::new(),
            timeline_selection,
            player: None,
            video_wakeup,
            active_video: None,
            frame: None,
            position: 0.0,
            duration: 0.0,
            paused: true,
            media_volume: 100.0,
            status: "Discovering Chatt daemon…".into(),
            _daemon_task: daemon_task,
            _video_task: video_task,
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

    fn advance_video(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut terminal_video_status = None;
        if let Some(player) = self.player.as_mut()
            && self.active_video.is_some()
        {
            match player.drain_events() {
                Ok(playback) => {
                    self.position = playback.position;
                    self.duration = playback.duration;
                    self.paused = playback.paused;
                    if playback.finished {
                        terminal_video_status = Some("Playback finished".into());
                    }
                }
                Err(error) => {
                    terminal_video_status = Some(format!("Video event failed: {error}"));
                }
            }
            if terminal_video_status.is_none() {
                if let Some(frame) = player.render_frame()
                    && let Some(buffer) =
                        RgbaImage::from_raw(frame.width, frame.height, frame.pixels)
                {
                    let next_frame = Arc::new(RenderImage::new(vec![Frame::new(buffer)]));
                    if let Some(previous_frame) = self.frame.replace(next_frame) {
                        cx.drop_image(previous_frame, Some(window));
                    }
                }
            }
        }
        if let Some(status) = terminal_video_status {
            self.release_video(window, cx);
            self.status = status.into();
        }
    }

    fn apply_daemon_event(
        &mut self,
        event: DaemonEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            DaemonEvent::Discovering => self.model.phase = ConnectionPhase::Discovering,
            DaemonEvent::Connecting => self.model.phase = ConnectionPhase::Connecting,
            DaemonEvent::TransportConnected => self.model.phase = ConnectionPhase::Syncing,
            DaemonEvent::Disconnected(reason) => {
                self.media_cache
                    .lock()
                    .expect("media cache lock poisoned")
                    .cancel_all();
                self.eager_image_fetches.reset_transient();
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
                self.eager_image_fetches.cached(&descriptor);
                self.pump_eager_image_fetches(cx);
            }
            DaemonEvent::MediaTransferFailed {
                transfer_id,
                reason,
            } => {
                self.media_cache
                    .lock()
                    .expect("media cache lock poisoned")
                    .cancel(transfer_id);
                self.eager_image_fetches
                    .failed(transfer_id, reason.clone());
                self.status = reason.into();
                self.pump_eager_image_fetches(cx);
            }
            DaemonEvent::Frame(frame) => {
                self.apply_daemon_state_frame(frame, window, cx);
            }
        }
    }

    fn apply_daemon_state_frame(
        &mut self,
        frame: DaemonFrame,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let pending = match &frame {
            DaemonFrame::RequestResult(result) => {
                self.model.pending.get(&result.request_id).cloned()
            }
            _ => None,
        };
        let old_len = self.model.messages.len();
        let old_selected_room = self.model.selected_room;
        let effect = reducer::apply(&mut self.model, frame);
        if self.model.selected_room != old_selected_room || effect.replace_messages {
            self.timeline_selection.clear();
        } else if !effect.splices.is_empty() {
            self.timeline_selection.retain_items(
                self.model
                    .messages
                    .iter()
                    .map(|message| MarkdownSelectionKey(message.id)),
            );
        }
        if self.model.selected_room != old_selected_room {
            self.release_video(window, cx);
            self.message_markdown.clear();
            self.image_cache
                .update(cx, |cache, cx| cache.clear(window, cx));
        }
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
                        && pending.operation == Operation::EditMessage
                        && pending.draft.as_deref() == Some(self.composer.read(cx).text().as_str())
                    {
                        self.composer.update(cx, |composer, cx| composer.clear(cx));
                        self.editing = None;
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
                        self.eager_image_fetches
                            .failed(transfer_id, message.clone());
                        self.pump_eager_image_fetches(cx);
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
        let clears_composer_on_enqueue = operation == Operation::SendMessage;
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
            if clears_composer_on_enqueue {
                self.composer.update(cx, |composer, cx| composer.clear(cx));
            }
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
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(message) = self.model.messages.get(index) else {
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
        let message_id = message.id;
        let room_id = message.room_id;
        let message_markdown = match self.message_markdown.get(&message.id) {
            Some(markdown) if markdown.read(cx).source().as_ref() == message.body.as_str() => {
                markdown.clone()
            }
            _ => {
                let markdown = cx.new(|cx| {
                    Markdown::new(message.body.clone().into(), None, None, cx)
                });
                self.message_markdown.insert(message.id, markdown.clone());
                markdown
            }
        };
        let mut markdown_style = MarkdownStyle::themed(MarkdownFont::Agent, window, cx);
        markdown_style.base_text_style.color = rgb(0xd7d9dd).into();
        markdown_style.selection_background_color = rgba(0x5277a866).into();
        let sender = message.sender.clone();
        let edited = message.edited;
        let unverified = message.unverified;
        let timestamp_ms = message.timestamp_ms;
        let edit = (message.local && !message.notice).then(|| {
            (
                message.room_id,
                rpc::ids::MessageId(message.id),
                message.body.clone(),
            )
        });
        let attachment = message.attachment.clone();
        div()
            .id(("message", message_id as usize))
            .w_full()
            .pl(px(64.))
            .pr(px(28.))
            .py(px(if continuation { 3. } else { 10. }))
            .bg(rgb(background))
            .hover(|row| row.bg(rgb(0x1b1e24)))
            .child(
                div()
                    .relative()
                    .w_full()
                    .max_w(px(860.))
                    .pl(px(15.))
                    .child(
                        div()
                            .absolute()
                            .left_0()
                            .top_0()
                            .bottom_0()
                            .w(px(3.))
                            .bg(rgb(if continuation { background } else { accent })),
                    )
                    .child(
                        div()
                            .w_full()
                            .min_w_0()
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
                                                .child(sender),
                                        )
                                        .when(edited, |meta| {
                                            meta.child(
                                                div()
                                                    .text_xs()
                                                    .text_color(rgb(0x777d87))
                                                    .child("edited"),
                                            )
                                        })
                                        .when(unverified, |meta| {
                                            meta.child(
                                                div()
                                                    .text_xs()
                                                    .text_color(rgb(0xc49a74))
                                                    .child("unverified"),
                                            )
                                        })
                                        .child(div().flex_1())
                                        .when_some(edit, |header, (room_id, edit_id, edit_body)| {
                                            header
                                                .child(
                                                    mini_button(
                                                        ("edit", message_id as usize),
                                                        "Edit",
                                                    )
                                                    .on_click(cx.listener(move |this, _, _, cx| {
                                                        this.begin_edit(
                                                            room_id,
                                                            edit_id,
                                                            edit_body.clone(),
                                                            cx,
                                                        )
                                                    })),
                                                )
                                                .child(
                                                    mini_button(
                                                        ("delete", message_id as usize),
                                                        "Delete",
                                                    )
                                                    .on_click(cx.listener(move |this, _, _, cx| {
                                                        this.delete_message(room_id, edit_id, cx)
                                                    })),
                                                )
                                        })
                                        .child(div().text_xs().text_color(rgb(0x777d87)).child(
                                            timeline::format_age(
                                                timestamp_ms,
                                                timeline::now_ms(),
                                            ),
                                        )),
                                )
                            })
                            .child(
                                MarkdownElement::new(message_markdown, markdown_style)
                                    .selection_group(
                                        self.timeline_selection.clone(),
                                        MarkdownSelectionKey(message_id),
                                    ),
                            )
                            .when_some(attachment, |content, attachment| {
                                content.child(self.render_attachment(
                                    room_id,
                                    message_id,
                                    attachment,
                                    window,
                                    cx,
                                ))
                            }),
                    ),
            )
            .into_any_element()
    }

    fn render_attachment(
        &mut self,
        room_id: RoomId,
        message_id: u64,
        attachment: Attachment,
        window: &mut Window,
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
        if attachment.is_image()
            && let Some(path) = cache_path.clone()
        {
            let (width, height) = timeline::media_box_size(
                descriptor.width.unwrap_or(4),
                descriptor.height.unwrap_or(3),
            );
            return img(path)
                .image_cache(&self.image_cache)
                .id(("image", message_id as usize))
                .mt_2()
                .w(px(width))
                .h(px(height))
                .max_w_full()
                .object_fit(ObjectFit::Contain)
                .into_any_element();
        }
        if attachment.is_image() {
            let fetch = EagerImageFetch::new(room_id, descriptor.clone());
            if let Some(transfer_id) = active_transfer {
                let action = mini_button(
                    ("cancel-image-read", transfer_id.0 as usize),
                    "Cancel",
                )
                .on_click(
                    cx.listener(move |this, _, _, cx| this.cancel_bulk_read(transfer_id, cx)),
                )
                .into_any_element();
                return Self::render_image_status(
                    message_id,
                    &descriptor,
                    format!("Fetching {}…", descriptor.file_name),
                    Some(action),
                );
            }
            if let Some(reason) = self.eager_image_fetches.failure(fetch.key) {
                let retry = fetch.clone();
                let action = mini_button(("retry-image-read", message_id as usize), "Retry")
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.retry_eager_image(retry.clone(), window, cx)
                    }))
                    .into_any_element();
                return Self::render_image_status(
                    message_id,
                    &descriptor,
                    format!("Could not fetch {} · {reason}", descriptor.file_name),
                    Some(action),
                );
            }
            self.enqueue_eager_image(fetch, window, cx);
            return Self::render_image_status(
                message_id,
                &descriptor,
                format!("Loading {}…", descriptor.file_name),
                None,
            );
        }
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
                                move |this, _, window, cx| {
                                    if this.active_video == Some(message_id) {
                                        this.toggle_playback_inner(cx);
                                    } else {
                                        this.activate_video(
                                            message_id,
                                            play_path.clone(),
                                            window,
                                            cx,
                                        );
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

    fn render_image_status(
        message_id: u64,
        descriptor: &AttachmentDescriptor,
        label: String,
        action: Option<AnyElement>,
    ) -> AnyElement {
        div()
            .id(("image-status", message_id as usize))
            .mt_2()
            .max_w_full()
            .when_some(image_box_size(descriptor), |frame, (width, height)| {
                frame.w(px(width)).h(px(height))
            })
            .px_3()
            .py_2()
            .flex()
            .items_center()
            .justify_center()
            .gap_3()
            .border_1()
            .border_color(rgb(0x596a90))
            .bg(rgb(0x171a20))
            .child(
                div()
                    .min_w_0()
                    .text_sm()
                    .text_color(rgb(0x9aa1ac))
                    .child(label),
            )
            .when_some(action, |status, action| status.child(action))
            .into_any_element()
    }

    fn enqueue_eager_image(
        &mut self,
        fetch: EagerImageFetch,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        self.eager_image_fetches.enqueue(fetch);
        self.schedule_eager_image_fetches(window, cx);
    }

    fn retry_eager_image(
        &mut self,
        fetch: EagerImageFetch,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        self.eager_image_fetches.retry(fetch);
        self.schedule_eager_image_fetches(window, cx);
        cx.notify();
    }

    fn schedule_eager_image_fetches(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.model.is_ready()
            && !self.eager_image_fetches.queued.is_empty()
            && !self.eager_image_fetches.pump_scheduled
        {
            self.eager_image_fetches.pump_scheduled = true;
            cx.defer_in(window, |this, _, cx| this.pump_eager_image_fetches(cx));
        }
    }

    fn pump_eager_image_fetches(&mut self, cx: &mut Context<Self>) {
        self.eager_image_fetches.pump_scheduled = false;
        if !self.model.is_ready() {
            return;
        }
        loop {
            let has_capacity = self
                .media_cache
                .lock()
                .expect("media cache lock poisoned")
                .available_transfer_slots()
                > 0;
            if !has_capacity {
                break;
            }
            let Some(fetch) = self.eager_image_fetches.pop_front() else {
                break;
            };
            match self.begin_attachment_read(fetch.key.room_id, fetch.descriptor.clone(), cx) {
                Ok(Some(transfer_id)) => {
                    self.eager_image_fetches.started(transfer_id, fetch);
                }
                Ok(None) => {}
                Err(reason) => {
                    self.eager_image_fetches.fail_to_start(fetch, reason);
                }
            }
        }
        cx.notify();
    }

    fn fetch_attachment(
        &mut self,
        descriptor: AttachmentDescriptor,
        cx: &mut Context<Self>,
    ) {
        if !self.model.is_ready() {
            return;
        }
        let Some(room_id) = self.model.selected_room else {
            return;
        };
        if let Err(error) = self.begin_attachment_read(room_id, descriptor, cx) {
            self.status = error.into();
            self.pump_eager_image_fetches(cx);
        }
    }

    fn begin_attachment_read(
        &mut self,
        room_id: RoomId,
        descriptor: AttachmentDescriptor,
        cx: &mut Context<Self>,
    ) -> Result<Option<BulkTransferId>, String> {
        if !self.model.is_ready() {
            return Ok(None);
        }
        let request_id = self.request_id();
        let transfer_id = self.transfer_id();
        {
            let mut cache = self.media_cache.lock().expect("media cache lock poisoned");
            if cache.path_for(&descriptor).is_some()
                || cache.active_transfer(&descriptor).is_some()
            {
                return Ok(None);
            }
            cache.reserve(transfer_id, &descriptor)?;
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
            return Err(error);
        } else {
            self.status = format!("Fetching {}…", descriptor.file_name).into();
        }
        cx.notify();
        Ok(Some(transfer_id))
    }

    fn activate_video(
        &mut self,
        message_id: u64,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.player.is_none() {
            match MpvPlayer::new(self.video_wakeup.clone(), VIDEO_WIDTH, VIDEO_HEIGHT) {
                Ok(player) => self.player = Some(player),
                Err(error) => {
                    self.status = format!("Video unavailable: {error}").into();
                    cx.notify();
                    return;
                }
            }
        }
        if let Some(frame) = self.frame.take() {
            cx.drop_image(frame, Some(window));
        }
        let player = self
            .player
            .as_mut()
            .expect("player was initialized immediately above");
        let load_result = player
            .set_volume(self.media_volume)
            .and_then(|_| player.load(&path.to_string_lossy()));
        match load_result {
            Ok(()) => {
                self.active_video = Some(message_id);
                self.position = 0.;
                self.duration = 0.;
                self.paused = false;
                self.status = "Playing cached attachment".into();
            }
            Err(error) => {
                self.player = None;
                self.active_video = None;
                self.status = format!("Could not open video: {error}").into();
            }
        }
        cx.notify();
    }

    fn release_video(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(frame) = self.frame.take() {
            cx.drop_image(frame, Some(window));
        }
        self.player = None;
        self.active_video = None;
        self.position = 0.;
        self.duration = 0.;
        self.paused = true;
    }

    fn toggle_playback_inner(&mut self, cx: &mut Context<Self>) {
        if let Some(player) = self.player.as_mut() {
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

    fn scroll_timeline(
        &mut self,
        event: &ScrollWheelEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let distance = -event.delta.pixel_delta(px(20.)).y;
        if distance == px(0.) {
            return false;
        }

        crate::frame_stats::record_scroll_input();
        let input_delta = f32::from(event.delta.pixel_delta(px(20.)).y);
        let input_offset = self.list_state.logical_scroll_top();
        let input_following = self.list_state.is_following_tail();
        crate::frame_stats::trace_scroll(|| {
            format!(
                "input delta_y={input_delta:.2}px distance={:.2}px at item={} offset={:.2}px following={input_following}",
                f32::from(distance),
                input_offset.item_ix,
                f32::from(input_offset.offset_in_item),
            )
        });
        if input_following {
            crate::frame_stats::trace_scroll(|| {
                "passing input to GPUI list to leave follow-tail".to_string()
            });
            return false;
        }

        let old_pending = f32::from(self.pending_scroll);
        let new_distance = f32::from(distance);
        if old_pending != 0.0 && old_pending.signum() != new_distance.signum() {
            self.pending_scroll = distance;
        } else {
            self.pending_scroll += distance;
        }

        if !self.scroll_animation_active {
            self.scroll_animation_active = true;
            self.last_scroll_frame = Some(Instant::now());
        }
        cx.notify();
        true
    }

    fn autoscroll_timeline_selection(
        &mut self,
        distance: gpui::Pixels,
        cx: &mut Context<Self>,
    ) {
        self.pending_scroll = px(0.);
        self.scroll_animation_active = false;
        self.last_scroll_frame = None;
        self.list_state.scroll_by(distance);
        cx.notify();
    }

    fn advance_timeline_scroll(&mut self, window: &mut Window) {
        let now = Instant::now();
        let elapsed = self
            .last_scroll_frame
            .replace(now)
            .map_or_else(Default::default, |last| now.saturating_duration_since(last));
        let pending = f32::from(self.pending_scroll);
        let before = self.list_state.logical_scroll_top();
        let following_before = self.list_state.is_following_tail();

        let step = if pending.abs() <= SCROLL_SETTLE_THRESHOLD {
            let step = self.pending_scroll;
            self.pending_scroll = px(0.);
            self.scroll_animation_active = false;
            self.last_scroll_frame = None;
            step
        } else {
            let response = 1.0
                - (-elapsed.as_secs_f32() / SCROLL_RESPONSE_SECONDS)
                    .exp()
                    .clamp(0.0, 1.0);
            let step = self.pending_scroll * response.clamp(0.25, 0.85);
            self.pending_scroll -= step;
            step
        };
        self.list_state.scroll_by(step);

        let after = self.list_state.logical_scroll_top();
        let following_after = self.list_state.is_following_tail();
        crate::frame_stats::trace_scroll(|| {
            format!(
                "apply step={:.2}px item={}:{}({:.2}px->{:.2}px) following={following_before}->{following_after}",
                f32::from(step),
                before.item_ix,
                after.item_ix,
                f32::from(before.offset_in_item),
                f32::from(after.offset_in_item),
            )
        });

        crate::frame_stats::record_scroll_update();
        if self.scroll_animation_active {
            window.request_animation_frame();
        }
    }
}

impl Render for ChattView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.active_video.is_some() {
            self.advance_video(window, cx);
        }
        if self.scroll_animation_active {
            self.advance_timeline_scroll(window);
        }
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
        let timeline_view = cx.entity().downgrade();
        let selection_view = cx.entity().downgrade();
        let timeline = MarkdownSelectionArea::new(
            capture_scroll(
                list(self.list_state.clone(), cx.processor(Self::render_message))
                    .with_sizing_behavior(gpui::ListSizingBehavior::Auto)
                    .flex_grow_1(),
                move |event, window, cx| {
                    timeline_view
                        .update(cx, |view, cx| view.scroll_timeline(event, window, cx))
                        .unwrap_or(false)
                },
            ),
            self.timeline_selection.clone(),
        )
        .on_vertical_autoscroll(move |distance, _, cx| {
            let _ = selection_view.update(cx, |view, cx| {
                view.autoscroll_timeline_selection(distance, cx)
            });
        });
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

fn image_box_size(descriptor: &AttachmentDescriptor) -> Option<(f32, f32)> {
    let (Some(width), Some(height)) = (descriptor.width, descriptor.height) else {
        return None;
    };
    Some(timeline::media_box_size(width, height))
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

#[cfg(test)]
mod tests {
    use super::*;
    use rpc::daemon::model::MediaKind;

    fn image_fetch(room_id: RoomId, marker: u8) -> EagerImageFetch {
        EagerImageFetch::new(
            room_id,
            AttachmentDescriptor {
                id: AttachmentId([marker; 16]),
                file_name: format!("image-{marker}.png"),
                media_kind: MediaKind::Image,
                content_type: "image/png".into(),
                byte_len: 10,
                digest: [marker; 32],
                width: Some(400),
                height: Some(300),
            },
        )
    }

    #[test]
    fn eager_images_are_deduplicated_and_require_manual_retry_after_failure() {
        let fetch = image_fetch(RoomId(1), 7);
        let mut images = EagerImageFetches::default();

        assert!(images.enqueue(fetch.clone()));
        assert!(!images.enqueue(fetch.clone()));
        let queued = images.pop_front().unwrap();
        images.started(BulkTransferId(1), queued);
        assert!(!images.enqueue(fetch.clone()));

        images.failed(BulkTransferId(1), "network error".into());
        assert_eq!(images.failure(fetch.key), Some("network error"));
        assert!(!images.enqueue(fetch.clone()));

        assert!(images.retry(fetch.clone()));
        assert!(images.failure(fetch.key).is_none());
        assert_eq!(images.pop_front().unwrap().key, fetch.key);
    }

    #[test]
    fn cached_image_clears_active_and_failed_state() {
        let fetch = image_fetch(RoomId(1), 8);
        let mut images = EagerImageFetches::default();
        images.started(BulkTransferId(2), fetch.clone());
        images.failures.insert(fetch.key, "old failure".into());

        images.cached(&fetch.descriptor);

        assert!(images.active.is_empty());
        assert!(images.failure(fetch.key).is_none());
    }

    #[test]
    fn image_status_reserves_only_known_dimensions() {
        let known = image_fetch(RoomId(1), 9).descriptor;
        assert_eq!(image_box_size(&known), Some((400.0, 300.0)));

        let mut unknown = known;
        unknown.width = None;
        unknown.height = None;
        assert_eq!(image_box_size(&unknown), None);
    }
}
