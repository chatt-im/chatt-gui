use std::{
    cell::Cell,
    collections::{HashMap, HashSet, VecDeque},
    fs,
    path::PathBuf,
    rc::Rc,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use crate::{
    appearance::{AppearanceConfig, AppearanceSync},
    attachment_source::{
        AttachmentSource, AttachmentSourceKey, AttachmentSourceRegistry,
        RegisteredAttachmentSource, VideoSourceCache, VideoSourceCandidate, VideoSourcePin,
        VideoSourceView,
    },
    audio_manager::{AttachmentAudioManager, AudioDrain, AudioKey},
    audio_player::{AudioPlayerConfig, AudioPlayerEvent, AudioPlayerHandler, render_audio_player},
    code_viewer::{
        CodeDocument, CodeSearchResults, CodeSelection, CodeViewState, MAX_CODE_PREVIEW_BYTES,
        render_code_document,
    },
    composer::{
        Composer, ComposerChanged, ComposerImagePaste, ComposerStateChanged, PastedImage,
        completion::{
            self, ArgumentKind, AssistSession, CompletionContext, CompletionOption,
            CompletionValue, OptionKey,
        },
        uploads::{
            FileInspection, FileQueue, MAX_QUEUED_FILES, QueuedFile, QueuedFileSource,
            inspect_files, prepare_images,
        },
    },
    daemon::{
        client::{DaemonClient, DaemonEvent},
        reducer,
    },
    formatted_message::{
        FormattedMessage, FormattedMessageElement, MessageSelectionArea, MessageSelectionGroup,
        MessageSelectionKey, PreparedFormattedMessage,
    },
    icons::{IconName, icon},
    image_cache::{PreviewImageLoader, TimelineImageLoader},
    media_cache::{CachedAttachment, MediaCache},
    model::{ChatModel, ConnectionPhase, PendingRequest},
    mpv_player::{MpvPlayer, SeekMode},
    preview::{
        CodePreviewState, DIVIDER_WIDTH as PREVIEW_DIVIDER_WIDTH, ImageViewState, PreviewContent,
        PreviewHistory, PreviewItem, clamp_chat_width, default_chat_width,
        panel_width_for_chat_width,
    },
    scroll_capture::capture_scroll,
    settings::{ConfigurationState, SettingsView, SettingsViewEvent},
    theme::{AppliedSettings, ThemePalette, ThemeRole},
    timeline::{self, Attachment, AttachmentRenderKind},
    ui_controls::{
        compact_action_button, composer_add_button, icon_button, message_action_button,
        mini_button, preview_action_button, preview_control_button, preview_status, room_button,
        sidebar_footer_button, toolbar_button, PREVIEW_HEADER_ICON_SIZE,
    },
    ui_scale::rems_from_px,
    video_controls::{
        CONTROLS_ANIMATION_DURATION, CONTROLS_HIDE_DELAY, VideoControlsState, VideoScrub,
        VideoVolumeDrag, horizontal_fraction, vertical_fraction, volume_scroll_delta,
    },
    video_manager::{AttachmentVideoManager, VideoDrain, VideoKey},
    video_player::{
        INLINE_VIDEO_ASPECT_RATIO, VideoPlayerConfig, VideoPlayerEvent, VideoPlayerHandler,
        aspect_ratio, render_video_player,
    },
    video_thumbnail::{ThumbnailKey, VideoThumbnailCache},
};
use chatt_message_format::reference::{MessageRef, REF_PREFIX};
use dbus_message::{FileChooserResponse, OpenFileOptions, open_files};
use gpui::{
    Anchor, Animation, AnimationExt, AnyElement, App, Asset, Bounds, ClipboardItem, Context, Div,
    ExternalPaths, FocusHandle, Focusable, FollowMode, FontWeight, ImageCacheError, ImageSource,
    ListAlignment, ListState, LruImageCache, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, ObjectFit, PinchEvent, Pixels, Point, Render, RenderImage, ScrollDelta,
    ScrollHandle, ScrollStrategy, ScrollWheelEvent, SharedString, Stateful, Subscription, Task,
    UniformListScrollHandle, WeakFocusHandle, Window, actions, anchored, canvas, deferred, div,
    img, list, point, prelude::*, px, relative,
};
use local_rpc::{
    frame::{ClientFrame, DaemonFrame, Operation, RequestOutcome, StateDelta},
    ids::{RoomId, StreamId},
    model::{
        AttachmentDescriptor, AttachmentId, BulkTransferId, CommandCandidate, CommandCandidateKind,
        CommandOutputLine, MediaKind, RequestId, RoomKind, ServerAvailability,
        ServerSelectionPrompt, ServerSummary, TrustState, VoiceState,
    },
};

const SIDEBAR_WIDTH: f32 = 232.0;
const TOP_BAR_HEIGHT: f32 = 52.0;
const MIN_COMPOSER_HEIGHT: f32 = 64.0;
const TIMELINE_GROUP_GAP: f32 = 7.0;
const TIMELINE_MESSAGE_ROW_PADDING_TOP: f32 = 2.0;
const TIMELINE_CONTINUATION_ROW_PADDING_Y: f32 = 2.0;
const TIMELINE_ROW_HEADER_HEIGHT: f32 = 24.0;
const TIMELINE_MESSAGE_ACTIONS_TOP: f32 = 7.0;
const TIMELINE_CONTINUATION_ACTIONS_TOP: f32 = 1.0;
const DECODED_IMAGE_CACHE_BYTES: usize = 64 * 1024 * 1024;
const PREVIEW_IMAGE_CACHE_BYTES: usize = 256 * 1024 * 1024;
const VIDEO_THUMBNAIL_CACHE_BYTES: usize = 64 * 1024 * 1024;
const SCROLL_RESPONSE_SECONDS: f32 = 0.006;
const SCROLL_SETTLE_THRESHOLD: f32 = 0.50;
const MIN_LIVE_ZOOM: f32 = 1.0;
const MAX_LIVE_ZOOM: f32 = 20.0;
const MIN_LIVE_PANE_HEIGHT: f32 = 160.0;
const MIN_CONSTRAINED_LIVE_PANE_HEIGHT: f32 = 96.0;
const MIN_CHAT_PANE_HEIGHT: f32 = 140.0;
const LIVE_PANE_DIVIDER_SIZE: f32 = 9.0;
const PREVIEW_TAB_BAR_HEIGHT: f32 = TOP_BAR_HEIGHT;
const PREVIEW_SEARCH_BAR_HEIGHT: f32 = 39.0;

fn timeline_message_row_padding_top(continuation: bool) -> f32 {
    if continuation {
        TIMELINE_CONTINUATION_ROW_PADDING_Y
    } else {
        TIMELINE_MESSAGE_ROW_PADDING_TOP
    }
}

fn timeline_message_actions_top(continuation: bool) -> f32 {
    if continuation {
        TIMELINE_CONTINUATION_ACTIONS_TOP
    } else {
        TIMELINE_MESSAGE_ACTIONS_TOP
    }
}

fn cached_attachment_image_source<A>(
    attachment: CachedAttachment,
    image_cache: gpui::Entity<LruImageCache<A>>,
) -> ImageSource
where
    A: Asset<Source = CachedAttachment, Output = Result<Arc<RenderImage>, ImageCacheError>>,
{
    ImageSource::Custom(Arc::new(move |window, cx| {
        image_cache.update(cx, |image_cache, cx| {
            image_cache.load_source(&attachment, window, cx)
        })
    }))
}

fn image_dimensions_from_bytes(bytes: &[u8]) -> image::ImageResult<(u32, u32)> {
    image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()?
        .into_dimensions()
}

fn write_cached_attachment_to_user_selected_path(
    attachment: &CachedAttachment,
    destination: Option<PathBuf>,
) -> std::io::Result<Option<PathBuf>> {
    let Some(destination) = destination else {
        return Ok(None);
    };
    fs::write(&destination, attachment.bytes())?;
    Ok(Some(destination))
}

fn code_preview_size_error(byte_len: u64) -> Option<&'static str> {
    (byte_len > MAX_CODE_PREVIEW_BYTES).then_some("file too large to preview")
}

fn formatted_message_candidates(events: &[DaemonEvent]) -> Vec<(RoomId, u64, String)> {
    let mut candidates = HashMap::new();
    let mut add_message = |message: &local_rpc::model::Message| {
        candidates.insert(
            (message.room_id, message.message_id.0),
            message.body.clone(),
        );
    };

    for event in events {
        let DaemonEvent::Frame(frame) = event else {
            continue;
        };
        match frame {
            DaemonFrame::Snapshot { snapshot, .. } => {
                if let Some(room) = snapshot.room.as_ref() {
                    for message in &room.messages {
                        add_message(message);
                    }
                }
            }
            DaemonFrame::Event(event) => match &event.delta {
                StateDelta::RoomSnapshot(room) => {
                    for message in &room.messages {
                        add_message(message);
                    }
                }
                StateDelta::MessagesPrepended { messages, .. } => {
                    for message in messages {
                        add_message(message);
                    }
                }
                StateDelta::MessageUpserted { message } => add_message(message),
                _ => {}
            },
            _ => {}
        }
    }

    candidates
        .into_iter()
        .map(|((room_id, message_id), body)| (room_id, message_id, body))
        .collect()
}

fn timeline_selection_key(item: timeline::MessageListItem) -> MessageSelectionKey {
    match item.source {
        timeline::MessageListSource::Message { message_id, .. } => {
            MessageSelectionKey::Message(message_id)
        }
        timeline::MessageListSource::Command { local_id, .. } => {
            MessageSelectionKey::Command(local_id)
        }
    }
}

type EagerImageKey = AttachmentId;

#[derive(Clone, Debug)]
struct EagerImageFetch {
    key: EagerImageKey,
    room_id: RoomId,
    descriptor: AttachmentDescriptor,
}

struct LivePlayerView {
    player: MpvPlayer,
    zoom: f32,
    pan: Point<Pixels>,
    last_mouse_position: Option<Point<Pixels>>,
    coded_size: (u32, u32),
    viewport_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
}

#[derive(Clone)]
struct TheaterVideo {
    key: VideoKey,
    descriptor: AttachmentDescriptor,
    source: Option<RegisteredAttachmentSource>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum MediaPlaybackTarget {
    Audio(AudioKey),
    Video(VideoKey),
}

#[derive(Clone, Copy, Debug)]
struct AudioScrub {
    key: AudioKey,
    bounds: Bounds<Pixels>,
    duration: f64,
    last_fraction: f64,
    last_seek: Instant,
}

impl AudioScrub {
    fn position(self) -> f64 {
        self.duration * self.last_fraction
    }

    fn should_dispatch_seek(&mut self, now: Instant) -> bool {
        if now.saturating_duration_since(self.last_seek) < Duration::from_millis(16) {
            return false;
        }
        self.last_seek = now;
        true
    }
}

#[derive(Clone, Copy, Debug)]
struct AudioVolumeDrag {
    key: AudioKey,
    bounds: Bounds<Pixels>,
}

#[derive(Clone, Copy, Debug)]
struct LivePaneResize {
    start_y: Pixels,
    start_height: Pixels,
}

#[derive(Clone, Copy, Debug)]
struct PreviewPaneResize {
    start_x: Pixels,
    start_chat_width: Pixels,
}

#[derive(Clone)]
struct CompletionView {
    context: CompletionContext,
    context_key: String,
    options: Vec<CompletionOption>,
    hint: Option<SharedString>,
}

#[derive(Clone, Debug)]
struct PendingCommand {
    request_id: RequestId,
    draft: String,
}

#[derive(Clone, Debug)]
struct EditingMessage {
    room_id: RoomId,
    target: local_rpc::ids::MessageId,
}

#[derive(Clone, Debug)]
struct SubmittedUpload {
    file: QueuedFile,
    begin_request: RequestId,
    finish_request: RequestId,
}

#[derive(Clone, Debug)]
enum SubmissionPhase {
    AwaitingMessage { request_id: RequestId },
    ReadyForUpload,
    Uploading(SubmittedUpload),
}

#[derive(Clone, Debug)]
struct PendingSubmission {
    room_id: RoomId,
    draft: Option<String>,
    files: VecDeque<QueuedFile>,
    total_files: usize,
    completed_files: usize,
    phase: SubmissionPhase,
}

impl PendingSubmission {
    fn request_ids(&self) -> (Option<RequestId>, Option<RequestId>) {
        match &self.phase {
            SubmissionPhase::AwaitingMessage { request_id } => (Some(*request_id), None),
            SubmissionPhase::ReadyForUpload => (None, None),
            SubmissionPhase::Uploading(upload) => {
                (Some(upload.begin_request), Some(upload.finish_request))
            }
        }
    }

    fn into_failed_files(mut self) -> Vec<QueuedFile> {
        let mut files = Vec::with_capacity(self.files.len() + 1);
        if let SubmissionPhase::Uploading(upload) = self.phase {
            files.push(upload.file);
        }
        files.extend(self.files.drain(..));
        files
    }

    fn outcome_is_ambiguous(&self) -> bool {
        matches!(
            &self.phase,
            SubmissionPhase::AwaitingMessage { .. } | SubmissionPhase::Uploading(_)
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct LiveVideoGeometry {
    bounds: Bounds<Pixels>,
    scale: f32,
}

impl LiveVideoGeometry {
    fn new(
        coded_size: (u32, u32),
        viewport: Bounds<Pixels>,
        zoom: f32,
        pan: Point<Pixels>,
    ) -> Option<Self> {
        let (coded_width, coded_height) = coded_size;
        let viewport_width = viewport.size.width.as_f32();
        let viewport_height = viewport.size.height.as_f32();
        if coded_width == 0 || coded_height == 0 || viewport_width <= 0.0 || viewport_height <= 0.0
        {
            return None;
        }

        let scale =
            (viewport_width / coded_width as f32).min(viewport_height / coded_height as f32) * zoom;
        let width = px(coded_width as f32 * scale);
        let height = px(coded_height as f32 * scale);
        let center = viewport.center() + pan;
        Some(Self {
            bounds: Bounds {
                origin: point(center.x - width / 2.0, center.y - height / 2.0),
                size: gpui::size(width, height),
            },
            scale,
        })
    }

    fn source_pixel_at(self, position: Point<Pixels>) -> Point<f32> {
        point(
            (position.x - self.bounds.origin.x).as_f32() / self.scale,
            (position.y - self.bounds.origin.y).as_f32() / self.scale,
        )
    }

    fn position_of_source_pixel(self, source: Point<f32>) -> Point<Pixels> {
        point(
            self.bounds.origin.x + px(source.x * self.scale),
            self.bounds.origin.y + px(source.y * self.scale),
        )
    }
}

fn live_pan_limits(coded_size: (u32, u32), viewport: Bounds<Pixels>, zoom: f32) -> Point<Pixels> {
    let (coded_width, coded_height) = coded_size;
    let viewport_width = viewport.size.width.as_f32();
    let viewport_height = viewport.size.height.as_f32();
    if coded_width == 0 || coded_height == 0 || viewport_width <= 0.0 || viewport_height <= 0.0 {
        return point(px(0.0), px(0.0));
    }

    let fit_scale =
        (viewport_width / coded_width as f32).min(viewport_height / coded_height as f32);
    let scaled_width = coded_width as f32 * fit_scale * zoom;
    let scaled_height = coded_height as f32 * fit_scale * zoom;
    point(
        px(((scaled_width - viewport_width) / 2.0).max(0.0)),
        px(((scaled_height - viewport_height) / 2.0).max(0.0)),
    )
}

fn clamp_live_pan(
    pan: Point<Pixels>,
    coded_size: (u32, u32),
    viewport: Bounds<Pixels>,
    zoom: f32,
) -> Point<Pixels> {
    let limits = live_pan_limits(coded_size, viewport, zoom);
    point(
        pan.x.clamp(-limits.x, limits.x),
        pan.y.clamp(-limits.y, limits.y),
    )
}

fn zoom_live_pan(
    pan: Point<Pixels>,
    coded_size: (u32, u32),
    old_zoom: f32,
    new_zoom: f32,
    viewport: Bounds<Pixels>,
    focal_point: Point<Pixels>,
) -> Point<Pixels> {
    let old_pan = clamp_live_pan(pan, coded_size, viewport, old_zoom);
    let Some(old_geometry) = LiveVideoGeometry::new(coded_size, viewport, old_zoom, old_pan) else {
        return point(px(0.0), px(0.0));
    };
    let source = old_geometry.source_pixel_at(focal_point);
    let Some(new_geometry) =
        LiveVideoGeometry::new(coded_size, viewport, new_zoom, point(px(0.0), px(0.0)))
    else {
        return point(px(0.0), px(0.0));
    };
    let source_without_pan = new_geometry.position_of_source_pixel(source);
    clamp_live_pan(
        focal_point - source_without_pan,
        coded_size,
        viewport,
        new_zoom,
    )
}

fn clamp_live_pane_height(height: Pixels, window_height: Pixels, rem_size: Pixels) -> Pixels {
    let available = window_height
        - crate::ui_scale::scaled_px(TOP_BAR_HEIGHT, rem_size)
        - crate::ui_scale::scaled_px(MIN_CHAT_PANE_HEIGHT, rem_size)
        - crate::ui_scale::scaled_px(LIVE_PANE_DIVIDER_SIZE, rem_size);
    let min_height = crate::ui_scale::scaled_px(MIN_LIVE_PANE_HEIGHT, rem_size).min(available.max(
        crate::ui_scale::scaled_px(MIN_CONSTRAINED_LIVE_PANE_HEIGHT, rem_size),
    ));
    height.clamp(min_height, available.max(min_height))
}

impl EagerImageFetch {
    fn new(room_id: RoomId, descriptor: AttachmentDescriptor) -> Self {
        Self {
            key: descriptor.id,
            room_id,
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
        self.active
            .retain(|_, fetch| fetch.descriptor.id != descriptor.id);
        self.failures.retain(|key, _| *key != descriptor.id);
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

    fn clear(&mut self) {
        self.reset_transient();
        self.failures.clear();
    }
}

actions!(
    chatt_gui,
    [
        OpenMedia,
        OpenSettings,
        SendMessage,
        TogglePlayback,
        SeekBack,
        SeekForward,
        LiveZoomIn,
        LiveZoomOut,
        LiveReset,
        LivePanUp,
        LivePanDown,
        ToggleMute,
        ToggleDeafen,
        ToggleVoice,
        ClosePreview,
        FindInCode,
        NextCodeMatch,
        PreviousCodeMatch,
        CloseCodeSearch,
        CompletionNext,
        CompletionPrevious,
        CompletionAccept,
        CompletionAcceptEngaged,
        CompletionDismiss,
        ServerNext,
        ServerPrevious,
        ServerActivate,
        CloseServerSelector
    ]
);

const REFERENCE_HOVER_DELAY: Duration = Duration::from_millis(200);
const REFERENCE_JUMP_PAGE_LIMIT: usize = 10;
const REFERENCE_JUMP_PAGE_SIZE: u16 = 200;

struct MessageReferenceHover {
    target: MessageRef,
    anchor: Bounds<Pixels>,
    visible: bool,
    message: Option<timeline::Message>,
    formatted: Option<Rc<FormattedMessage>>,
    missing: bool,
    request_id: Option<RequestId>,
}

struct PendingMessageJump {
    target: MessageRef,
    pages_requested: usize,
    page_request_id: Option<RequestId>,
    room_request_id: Option<RequestId>,
}

struct PendingMessageReferenceClick {
    target: MessageRef,
    request_id: RequestId,
}

struct PendingReferenceMediaPreview {
    target: MessageRef,
    attachment: Attachment,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MessageReferenceJumpDecision {
    Found,
    LoadOlder(local_rpc::ids::MessageId),
    Unavailable,
    SearchWindowExhausted,
}

fn message_reference_jump_decision(
    messages: &[timeline::Message],
    target: local_rpc::ids::MessageId,
    older_cursor: Option<local_rpc::ids::MessageId>,
    at_start: bool,
    pages_requested: usize,
) -> MessageReferenceJumpDecision {
    if messages
        .binary_search_by_key(&target.0, |message| message.id)
        .is_ok()
    {
        return MessageReferenceJumpDecision::Found;
    }
    if messages.first().is_some_and(|first| target.0 >= first.id) || at_start {
        return MessageReferenceJumpDecision::Unavailable;
    }
    let Some(before) = older_cursor else {
        return MessageReferenceJumpDecision::Unavailable;
    };
    if before.0 <= target.0 {
        return MessageReferenceJumpDecision::Unavailable;
    }
    if pages_requested >= REFERENCE_JUMP_PAGE_LIMIT {
        return MessageReferenceJumpDecision::SearchWindowExhausted;
    }
    MessageReferenceJumpDecision::LoadOlder(before)
}

pub struct ChattView {
    model: ChatModel,
    daemon: DaemonClient,
    next_request_id: u64,
    next_transfer_id: u64,
    editing: Option<EditingMessage>,
    composer: gpui::Entity<Composer>,
    queued_files: FileQueue,
    file_inspection_pending: bool,
    pending_submission: Option<PendingSubmission>,
    submission_outcome_unknown: bool,
    completion_session: Option<AssistSession>,
    command_candidates: HashMap<CommandCandidateKind, Vec<CommandCandidate>>,
    candidate_requests: HashMap<CommandCandidateKind, RequestId>,
    pending_command: Option<PendingCommand>,
    suppress_completion_refresh: bool,
    server_search_input: gpui::Entity<Composer>,
    server_selector_open: bool,
    composer_menu_open: bool,
    composer_menu_action_taken: bool,
    composer_menu_trigger_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    show_rooms_sidebar: bool,
    show_top_status_bar: bool,
    selected_server_label: Option<String>,
    server_list_scroll: ScrollHandle,
    pending_server_selection: Option<RequestId>,
    server_selection_target: Option<String>,
    pending_server_prompt: Option<(RequestId, bool)>,
    code_search_input: gpui::Entity<Composer>,
    code_viewer_focus: FocusHandle,
    code_selection: CodeSelection,
    code_search_open: bool,
    code_search_pending: bool,
    code_search_generation: u64,
    code_search_results: CodeSearchResults,
    code_search_result_index: usize,
    code_search_task: Option<Task<()>>,
    media_cache: Arc<Mutex<MediaCache>>,
    image_cache: gpui::Entity<LruImageCache<TimelineImageLoader>>,
    preview_image_cache: gpui::Entity<LruImageCache<PreviewImageLoader>>,
    eager_image_fetches: EagerImageFetches,
    preview_history: PreviewHistory,
    next_code_load_id: u64,
    code_load_tasks: HashMap<AttachmentId, (u64, Task<()>)>,
    preview_tabs_scroll: ScrollHandle,
    preview_return_focus: Option<WeakFocusHandle>,
    preview_image: ImageViewState,
    preview_image_viewport: Cell<Option<Bounds<Pixels>>>,
    preview_last_mouse_position: Option<Point<Pixels>>,
    preview_chat_width: Pixels,
    preview_pane_resize: Option<PreviewPaneResize>,
    list_state: ListState,
    collapsed_sections: timeline::CollapsedSections,
    command_rows: Vec<timeline::LocalCommandRow>,
    next_command_row_id: u64,
    message_list: Vec<timeline::MessageListItem>,
    pending_scroll: gpui::Pixels,
    scroll_animation_active: bool,
    last_scroll_frame: Option<Instant>,
    formatted_messages: HashMap<u64, Rc<FormattedMessage>>,
    formatted_command_messages: HashMap<u64, Rc<FormattedMessage>>,
    message_reference_hover: Option<MessageReferenceHover>,
    message_reference_hover_task: Option<Task<()>>,
    message_reference_cache: HashMap<MessageRef, Option<timeline::Message>>,
    pending_message_reference_click: Option<PendingMessageReferenceClick>,
    pending_reference_media_preview: Option<PendingReferenceMediaPreview>,
    pending_message_jump: Option<PendingMessageJump>,
    message_reference_flash: Option<(MessageRef, u64)>,
    next_message_reference_flash_id: u64,
    message_reference_flash_task: Option<Task<()>>,
    timeline_selection: MessageSelectionGroup,
    audios: AttachmentAudioManager,
    videos: AttachmentVideoManager,
    video_thumbnails: VideoThumbnailCache,
    attachment_source_registry: AttachmentSourceRegistry,
    video_sources: VideoSourceCache,
    media_namespace_generation: u64,
    pending_video_plays: HashSet<VideoKey>,
    pending_audio_plays: HashSet<AudioKey>,
    visible_video_keys: HashSet<VideoKey>,
    visible_audio_keys: HashSet<AudioKey>,
    media_interactions: VecDeque<MediaPlaybackTarget>,
    video_source_retry_task: Option<Task<()>>,
    audio_scrub: Option<AudioScrub>,
    audio_volume_drag: Option<AudioVolumeDrag>,
    video_scrub: Option<VideoScrub>,
    video_volume_drag: Option<VideoVolumeDrag>,
    video_controls: VideoControlsState,
    video_volume_popup_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    video_volume_button_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    theater_video: Option<TheaterVideo>,
    video_controls_animation_task: Option<Task<()>>,
    video_controls_hide_task: Option<Task<()>>,
    video_volume_hide_task: Option<Task<()>>,
    video_surface_click_task: Option<Task<()>>,
    video_wakeup: async_channel::Sender<()>,
    live_players: HashMap<StreamId, LivePlayerView>,
    fullscreen_share: Option<StreamId>,
    live_pane_height: Option<Pixels>,
    live_pane_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    live_pane_resize: Option<LivePaneResize>,
    composer_error: Option<SharedString>,
    status: SharedString,
    typography_revision: u64,
    ui_scale_revision: u64,
    settings: Option<gpui::Entity<SettingsView>>,
    settings_subscription: Option<Subscription>,
    settings_remote_session: Option<local_rpc::settings::SettingsSessionId>,
    settings_close_when_opened: bool,
    appearance_sync: AppearanceSync,
    next_appearance_session: u32,
    appearance_reload_task: Option<Task<()>>,
    _code_search_subscription: Subscription,
    _composer_image_paste_subscription: Subscription,
    _composer_state_subscription: Subscription,
    _composer_focus_subscription: Subscription,
    _composer_blur_subscription: Subscription,
    _server_search_subscription: Subscription,
    _daemon_task: Task<()>,
    _video_task: Task<()>,
}

impl ChattView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let model = ChatModel::default();
        let list_state = ListState::new(0, ListAlignment::Bottom, px(1_600.0));
        list_state.set_follow_mode(FollowMode::Tail);
        let media_cache = Arc::new(Mutex::new(MediaCache::new(512 * 1024 * 1024)));
        let (daemon, daemon_events) = DaemonClient::spawn(media_cache.clone());
        let binding_mode = AppliedSettings::get(cx).binding_mode;
        let composer = cx.new(|cx| Composer::with_binding_mode(binding_mode, cx));
        let server_search_input = cx.new(Composer::server_search);
        let server_search_subscription =
            cx.subscribe(&server_search_input, |this, _, _: &ComposerChanged, cx| {
                if let Some(index) = this.reconcile_server_selection(cx) {
                    this.server_list_scroll.scroll_to_item(index);
                }
                cx.notify();
            });
        let code_search_input = cx.new(Composer::search);
        let code_search_subscription =
            cx.subscribe(&code_search_input, |this, _, _: &ComposerChanged, cx| {
                this.update_code_search(cx);
            });
        let composer_image_paste_subscription =
            cx.subscribe(&composer, |this, _, event: &ComposerImagePaste, cx| {
                this.queue_clipboard_images(event.images.clone(), cx);
            });
        let composer_state_subscription =
            cx.subscribe(&composer, |this, _, _: &ComposerStateChanged, cx| {
                if !this.suppress_completion_refresh {
                    this.refresh_completion(cx);
                }
            });
        let composer_focus = composer.focus_handle(cx);
        let composer_focus_subscription = cx.on_focus(&composer_focus, window, |this, _, cx| {
            this.dismiss_composer_menu(cx);
        });
        let composer_blur_subscription = cx.on_blur(&composer_focus, window, |this, _, cx| {
            if this.completion_session.take().is_some() {
                this.composer
                    .update(cx, |composer, _| composer.set_completion_open(false));
                cx.notify();
            }
        });
        let code_viewer_focus = cx.focus_handle();
        let code_selection = CodeSelection::new(code_viewer_focus.clone());
        let timeline_selection = MessageSelectionGroup::new(cx.focus_handle());
        let image_cache = LruImageCache::<TimelineImageLoader>::new(DECODED_IMAGE_CACHE_BYTES, cx);
        let preview_image_cache =
            LruImageCache::<PreviewImageLoader>::new(PREVIEW_IMAGE_CACHE_BYTES, cx);
        window.focus(&composer.focus_handle(cx), cx);
        let formatting_executor = cx.background_executor().clone();
        let daemon_task = cx.spawn_in(window, async move |this, cx| {
            while let Ok(first_event) = daemon_events.recv().await {
                let mut events = vec![first_event];
                while let Ok(event) = daemon_events.try_recv() {
                    events.push(event);
                }
                let candidates = formatted_message_candidates(&events);
                let prepared = if candidates.is_empty() {
                    Vec::new()
                } else {
                    formatting_executor
                        .spawn(async move {
                            candidates
                                .into_iter()
                                .map(|(room_id, message_id, body)| {
                                    (room_id, message_id, FormattedMessage::prepare(body))
                                })
                                .collect::<Vec<_>>()
                        })
                        .await
                };
                if this
                    .update_in(cx, |this, window, cx| {
                        for event in events {
                            this.apply_daemon_event(event, window, cx);
                        }
                        this.install_prepared_messages(prepared);
                        cx.notify();
                        log::debug!("daemon event batch notified ChattView");
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
                if this.update_in(cx, |_, window, _| window.refresh()).is_err() {
                    return;
                }
            }
        });
        let timeline_view = cx.entity().downgrade();
        list_state.set_visible_range_handler(move |range, _, cx| {
            let timeline_view = timeline_view.clone();
            // List invokes this while its state is mutably borrowed.
            cx.defer(move |cx| {
                let _ = timeline_view.update(cx, |this, cx| {
                    this.update_video_visibility(range, cx);
                });
            });
        });
        let media_namespace_generation = 1;
        let attachment_source_registry = AttachmentSourceRegistry::new(media_namespace_generation);
        let video_sources = VideoSourceCache::new(
            media_namespace_generation,
            model.limits.concurrent_attachment_streams,
        );
        let videos =
            AttachmentVideoManager::new(video_wakeup.clone(), attachment_source_registry.clone());
        let audios =
            AttachmentAudioManager::new(video_wakeup.clone(), attachment_source_registry.clone());
        let video_thumbnails =
            VideoThumbnailCache::new(VIDEO_THUMBNAIL_CACHE_BYTES, video_wakeup.clone());
        Self {
            model,
            daemon,
            next_request_id: 1,
            next_transfer_id: 1,
            editing: None,
            composer,
            queued_files: FileQueue::default(),
            file_inspection_pending: false,
            pending_submission: None,
            submission_outcome_unknown: false,
            completion_session: None,
            command_candidates: HashMap::new(),
            candidate_requests: HashMap::new(),
            pending_command: None,
            suppress_completion_refresh: false,
            server_search_input,
            server_selector_open: false,
            composer_menu_open: false,
            composer_menu_action_taken: false,
            composer_menu_trigger_bounds: Rc::new(Cell::new(None)),
            show_rooms_sidebar: true,
            show_top_status_bar: true,
            selected_server_label: None,
            server_list_scroll: ScrollHandle::new(),
            pending_server_selection: None,
            server_selection_target: None,
            pending_server_prompt: None,
            code_search_input,
            code_viewer_focus,
            code_selection,
            code_search_open: false,
            code_search_pending: false,
            code_search_generation: 0,
            code_search_results: CodeSearchResults::default(),
            code_search_result_index: 0,
            code_search_task: None,
            media_cache,
            image_cache,
            preview_image_cache,
            eager_image_fetches: EagerImageFetches::default(),
            preview_history: PreviewHistory::default(),
            next_code_load_id: 1,
            code_load_tasks: HashMap::new(),
            preview_tabs_scroll: ScrollHandle::new(),
            preview_return_focus: None,
            preview_image: ImageViewState::default(),
            preview_image_viewport: Cell::new(None),
            preview_last_mouse_position: None,
            preview_chat_width: default_chat_width(
                window.viewport_size().width
                    - crate::ui_scale::scaled_px(SIDEBAR_WIDTH, crate::ui_scale::rem_size(cx)),
                crate::ui_scale::rem_size(cx),
            ),
            preview_pane_resize: None,
            list_state,
            collapsed_sections: timeline::CollapsedSections::new(),
            command_rows: Vec::new(),
            next_command_row_id: 1,
            message_list: Vec::new(),
            pending_scroll: px(0.),
            scroll_animation_active: false,
            last_scroll_frame: None,
            formatted_messages: HashMap::new(),
            formatted_command_messages: HashMap::new(),
            message_reference_hover: None,
            message_reference_hover_task: None,
            message_reference_cache: HashMap::new(),
            pending_message_reference_click: None,
            pending_reference_media_preview: None,
            pending_message_jump: None,
            message_reference_flash: None,
            next_message_reference_flash_id: 1,
            message_reference_flash_task: None,
            timeline_selection,
            audios,
            videos,
            video_thumbnails,
            attachment_source_registry,
            video_sources,
            media_namespace_generation,
            pending_video_plays: HashSet::new(),
            pending_audio_plays: HashSet::new(),
            visible_video_keys: HashSet::new(),
            visible_audio_keys: HashSet::new(),
            media_interactions: VecDeque::new(),
            video_source_retry_task: None,
            audio_scrub: None,
            audio_volume_drag: None,
            video_scrub: None,
            video_volume_drag: None,
            video_controls: VideoControlsState::default(),
            video_volume_popup_bounds: Rc::new(Cell::new(None)),
            video_volume_button_bounds: Rc::new(Cell::new(None)),
            theater_video: None,
            video_controls_animation_task: None,
            video_controls_hide_task: None,
            video_volume_hide_task: None,
            video_surface_click_task: None,
            video_wakeup,
            live_players: HashMap::new(),
            fullscreen_share: None,
            live_pane_height: None,
            live_pane_bounds: Rc::new(Cell::new(None)),
            live_pane_resize: None,
            composer_error: None,
            status: "Discovering Chatt daemon…".into(),
            typography_revision: AppliedSettings::get(cx).typography_revision,
            ui_scale_revision: crate::ui_scale::revision(cx),
            settings: None,
            settings_subscription: None,
            settings_remote_session: None,
            settings_close_when_opened: false,
            appearance_sync: AppearanceSync::new(),
            next_appearance_session: 1,
            appearance_reload_task: None,
            _code_search_subscription: code_search_subscription,
            _composer_image_paste_subscription: composer_image_paste_subscription,
            _composer_state_subscription: composer_state_subscription,
            _composer_focus_subscription: composer_focus_subscription,
            _composer_blur_subscription: composer_blur_subscription,
            _server_search_subscription: server_search_subscription,
            _daemon_task: daemon_task,
            _video_task: video_task,
        }
    }

    fn request_id(&mut self) -> RequestId {
        let id = self.next_request_id.clamp(1, (1u64 << 63) - 1);
        self.next_request_id = if id == (1u64 << 63) - 1 { 1 } else { id + 1 };
        RequestId(id)
    }

    fn appearance_session_id(&mut self) -> local_rpc::appearance::AppearanceSessionId {
        let serial = self.next_appearance_session.max(1);
        self.next_appearance_session = serial.wrapping_add(1).max(1);
        local_rpc::appearance::AppearanceSessionId(
            (u64::from(std::process::id()) << 32) | u64::from(serial),
        )
    }

    fn set_composer_error(&mut self, message: impl Into<SharedString>, cx: &mut Context<Self>) {
        let message = message.into();
        log::warn!("composer action failed: {message}");
        self.status = message.clone();
        self.composer_error = Some(message);
        cx.notify();
    }

    fn open_settings(&mut self, _: &OpenSettings, window: &mut Window, cx: &mut Context<Self>) {
        self.composer_menu_open = false;
        self.composer_menu_action_taken = false;
        if let Some(settings) = self.settings.as_ref() {
            window.focus(&settings.focus_handle(cx), cx);
            return;
        }
        self.settings_close_when_opened = false;
        let appearance_session = self.appearance_session_id();
        let settings = cx.new(move |cx| SettingsView::new(appearance_session, cx));
        let subscription =
            cx.subscribe(
                &settings,
                |this, _, event: &SettingsViewEvent, cx| match event {
                    SettingsViewEvent::Closed => {
                        this.settings = None;
                        this.settings_subscription = None;
                        if let Some(session_id) = this.settings_remote_session {
                            this.send_settings_command(
                                local_rpc::settings::SettingsCommand::Close { session_id },
                                cx,
                            );
                        } else {
                            this.settings_close_when_opened = true;
                        }
                        cx.notify();
                    }
                    SettingsViewEvent::Command(command) => {
                        this.send_settings_command(command.clone(), cx);
                    }
                    SettingsViewEvent::LocalAppearancePreview {
                        session_id,
                        appearance,
                    } => {
                        let loaded = cx.global::<ConfigurationState>().0.clone();
                        this.appearance_sync.local_preview(
                            *session_id,
                            appearance.clone(),
                            &loaded,
                            cx,
                        );
                    }
                    SettingsViewEvent::AppearanceCommand(command) => {
                        this.send_appearance_command(command.clone(), cx);
                    }
                },
            );
        window.focus(&settings.focus_handle(cx), cx);
        self.settings = Some(settings.clone());
        self.settings_subscription = Some(subscription);
        settings.update(cx, |settings, cx| settings.begin_remote(cx));
        cx.notify();
    }

    fn send_settings_command(
        &mut self,
        command: local_rpc::settings::SettingsCommand,
        cx: &mut Context<Self>,
    ) {
        let request_id = self.request_id();
        if let Err(error) = self.daemon.send(ClientFrame::Settings {
            request_id,
            command,
        }) && let Some(settings) = &self.settings
        {
            settings.update(cx, |settings, cx| {
                settings.remote_command_failed(&error, cx)
            });
        }
    }

    fn send_appearance_command(
        &mut self,
        command: local_rpc::appearance::AppearanceCommand,
        cx: &mut Context<Self>,
    ) {
        let loaded = cx.global::<ConfigurationState>().0.clone();
        match &command {
            local_rpc::appearance::AppearanceCommand::Commit {
                session_id,
                document,
                ..
            } => match AppearanceConfig::from_document(document) {
                Ok(appearance) => {
                    self.appearance_sync
                        .local_commit(*session_id, appearance, &loaded, cx);
                }
                Err(error) => {
                    self.status = format!("Could not apply saved appearance · {error}").into();
                    cx.notify();
                    return;
                }
            },
            local_rpc::appearance::AppearanceCommand::End { session_id } => {
                self.appearance_sync.end_local(*session_id, &loaded, cx);
            }
            local_rpc::appearance::AppearanceCommand::Preview { .. } => {}
        }
        let request_id = self.request_id();
        if let Err(error) = self.daemon.send(ClientFrame::Appearance {
            request_id,
            command,
        }) {
            self.status = format!("Appearance preview is local only · {error}").into();
            cx.notify();
        }
    }

    fn reconcile_committed_appearance(
        &mut self,
        appearance: AppearanceConfig,
        cx: &mut Context<Self>,
    ) {
        let Some(path) = cx.global::<ConfigurationState>().0.path.clone() else {
            return;
        };
        let executor = cx.background_executor().clone();
        self.appearance_reload_task = Some(cx.spawn(async move |this, cx| {
            let loaded = executor
                .spawn(async move { crate::config::io::load_path(path) })
                .await;
            if !matches!(
                loaded.status,
                crate::config::io::SourceStatus::Loaded | crate::config::io::SourceStatus::Missing
            ) || AppearanceConfig::from_gui(&loaded.config) != appearance
            {
                return;
            }
            let _ = this.update(cx, |this, cx| {
                this.appearance_reload_task.take();
                if let Some(settings) = &this.settings {
                    if let Err(error) = settings.update(cx, |settings, cx| {
                        settings.install_external_loaded_if_clean(loaded, cx)
                    }) {
                        log::warn!("could not reconcile shared gui.toml: {error}");
                    }
                } else if let Err(error) = crate::settings::install_external_loaded(loaded, cx) {
                    log::warn!("could not reconcile shared gui.toml: {error}");
                }
            });
        }));
    }

    fn completion_view(&self, cx: &Context<Self>) -> Option<CompletionView> {
        let snapshot = self.composer.read(cx).snapshot();
        let context = completion::completion_context(
            &snapshot.text,
            snapshot.selection,
            snapshot.accepts_completion,
            snapshot.composing,
            &self.model.commands,
        )?;
        if !matches!(context, CompletionContext::Emoji { .. })
            && (!self.model.is_ready()
                || self.editing.is_some()
                || self.pending_command.is_some()
                || self.file_inspection_pending
                || self.pending_submission.is_some())
        {
            return None;
        }
        let context_key = completion::context_key(&context);
        let (options, hint) = match &context {
            CompletionContext::Command { query, .. } => {
                let options = completion::command_options(&self.model.commands, query);
                let hint = options
                    .is_empty()
                    .then(|| "No matching commands · Enter runs as typed".into());
                (options, hint)
            }
            CompletionContext::Argument {
                command,
                kind: ArgumentKind::Free,
                query,
                ..
            } => {
                let hint = query
                    .is_empty()
                    .then(|| {
                        command
                            .placeholder
                            .clone()
                            .unwrap_or_else(|| command.usage.clone())
                    })
                    .map(SharedString::from);
                (Vec::new(), hint)
            }
            CompletionContext::Argument {
                kind: ArgumentKind::Candidates(kind),
                query,
                ..
            } => {
                let cached = self.command_candidates.get(kind);
                let options = cached
                    .map(|items| completion::candidate_options(*kind, items, query))
                    .unwrap_or_default();
                let hint = if cached.is_none() && self.candidate_requests.contains_key(kind) {
                    Some(format!("Loading {}…", candidate_kind_label(*kind)).into())
                } else if options.is_empty() {
                    Some(format!("No matching {}", candidate_kind_label(*kind)).into())
                } else {
                    None
                };
                (options, hint)
            }
            CompletionContext::Emoji { query, .. } => (completion::emoji_options(query), None),
        };
        Some(CompletionView {
            context,
            context_key,
            options,
            hint,
        })
    }

    fn refresh_completion(&mut self, cx: &mut Context<Self>) {
        let view = self.completion_view(cx);
        let previous_key = self
            .completion_session
            .as_ref()
            .map(|session| session.context_key.clone());
        let request_kind = view.as_ref().and_then(|view| match &view.context {
            CompletionContext::Argument {
                kind: ArgumentKind::Candidates(kind),
                ..
            } if previous_key.as_deref() != Some(view.context_key.as_str()) => Some(*kind),
            _ => None,
        });

        match &view {
            None => self.completion_session = None,
            Some(view) if previous_key.as_deref() != Some(view.context_key.as_str()) => {
                self.completion_session = Some(completion::open_session(&view.context));
            }
            Some(view) => completion::reconcile_session(
                &mut self.completion_session,
                Some(&view.context),
                &view.options,
            ),
        }
        if let Some(view) = &view
            && matches!(view.context, CompletionContext::Emoji { .. })
            && let Some(session) = &mut self.completion_session
        {
            completion::engage_first(session, &view.options);
        }
        if let Some(kind) = request_kind {
            self.request_command_candidates(kind);
        }
        let completion_open = view.as_ref().is_some_and(|view| {
            self.completion_session
                .as_ref()
                .is_some_and(|session| session.context_key == view.context_key)
                && (!view.options.is_empty() || view.hint.is_some())
        });
        let completion_engaged = completion_open
            && self
                .completion_session
                .as_ref()
                .is_some_and(|session| session.engaged);
        self.composer.update(cx, |composer, _| {
            composer.set_completion_state(completion_open, completion_engaged)
        });
        cx.notify();
    }

    fn request_command_candidates(&mut self, kind: CommandCandidateKind) {
        if self.command_candidates.contains_key(&kind)
            || self.candidate_requests.contains_key(&kind)
        {
            return;
        }
        let request_id = self.request_id();
        self.candidate_requests.insert(kind, request_id);
        if let Err(error) = self
            .daemon
            .send(ClientFrame::RequestCommandCandidates { request_id, kind })
        {
            self.candidate_requests.remove(&kind);
            self.status = error.into();
        }
    }

    fn clear_completion(&mut self, cx: &mut Context<Self>) {
        self.completion_session = None;
        self.command_candidates.clear();
        self.candidate_requests.clear();
        self.composer
            .update(cx, |composer, _| composer.set_completion_open(false));
    }

    fn clear_command_surface(&mut self, cx: &mut Context<Self>) {
        self.pending_command = None;
        self.clear_completion(cx);
        self.formatted_command_messages.clear();
        if !self.command_rows.is_empty() {
            self.command_rows.clear();
            self.rebuild_message_list();
        }
    }

    fn append_command_output(&mut self, lines: Vec<CommandOutputLine>) {
        if lines.is_empty() {
            return;
        }
        let anchor_message_id = self.model.messages.last().map(|message| message.id);
        let timestamp_ms = timeline::now_ms();
        for line in lines {
            let local_id = self.next_command_row_id.max(1);
            self.next_command_row_id = if local_id == u64::MAX {
                1
            } else {
                local_id + 1
            };
            let body = line.text;
            self.formatted_command_messages
                .insert(local_id, Rc::new(FormattedMessage::plain(body.clone())));
            self.command_rows.push(timeline::LocalCommandRow {
                local_id,
                anchor_message_id,
                body,
                error: line.error,
                timestamp_ms,
            });
        }
        self.rebuild_message_list();
        self.list_state.scroll_to_end();
    }

    fn move_completion(&mut self, delta: isize, _: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.completion_view(cx) else {
            self.completion_session = None;
            self.composer
                .update(cx, |composer, _| composer.set_completion_open(false));
            return;
        };
        let Some(session) = &mut self.completion_session else {
            return;
        };
        if completion::move_selection(session, &view.options, delta) {
            self.composer
                .update(cx, |composer, _| composer.set_completion_state(true, true));
            cx.notify();
        }
    }

    fn completion_next(&mut self, _: &CompletionNext, window: &mut Window, cx: &mut Context<Self>) {
        self.move_completion(1, window, cx);
    }

    fn completion_previous(
        &mut self,
        _: &CompletionPrevious,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_completion(-1, window, cx);
    }

    fn completion_accept(
        &mut self,
        _: &CompletionAccept,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.completion_view(cx) else {
            return;
        };
        let Some(session) = &self.completion_session else {
            return;
        };
        let Some(option) = completion::tab_option(session, &view.options).cloned() else {
            return;
        };
        self.accept_completion_option(&view.context, option, window, cx);
    }

    fn completion_accept_engaged(
        &mut self,
        _: &CompletionAcceptEngaged,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.completion_view(cx) else {
            return;
        };
        let Some(session) = &self.completion_session else {
            return;
        };
        let Some(option) = completion::enter_option(session, &view.options).cloned() else {
            return;
        };
        self.accept_completion_option(&view.context, option, window, cx);
    }

    fn completion_dismiss(
        &mut self,
        _: &CompletionDismiss,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.completion_session.take().is_some() {
            self.composer
                .update(cx, |composer, _| composer.set_completion_open(false));
            cx.notify();
        }
    }

    fn hover_completion(&mut self, key: OptionKey, cx: &mut Context<Self>) {
        let Some(view) = self.completion_view(cx) else {
            return;
        };
        if !view.options.iter().any(|option| option.key == key) {
            return;
        }
        let Some(session) = &mut self.completion_session else {
            return;
        };
        session.active = Some(key);
        session.engaged = true;
        self.composer
            .update(cx, |composer, _| composer.set_completion_state(true, true));
        cx.notify();
    }

    fn accept_completion_key(
        &mut self,
        key: OptionKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.completion_view(cx) else {
            return;
        };
        let Some(option) = view
            .options
            .iter()
            .find(|option| option.key == key)
            .cloned()
        else {
            return;
        };
        self.accept_completion_option(&view.context, option, window, cx);
    }

    fn accept_completion_option(
        &mut self,
        context: &CompletionContext,
        option: CompletionOption,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let reopen = matches!(
            &option.value,
            CompletionValue::Command(command)
                if command.arg != local_rpc::model::CommandArgKind::None
        );
        let replacement = completion::replacement(context, &option);
        self.suppress_completion_refresh = true;
        self.composer.update(cx, |composer, cx| {
            composer.replace_completion(replacement.span, &replacement.text, window, cx)
        });
        self.suppress_completion_refresh = false;
        self.completion_session = None;
        if reopen {
            self.refresh_completion(cx);
        } else {
            self.composer
                .update(cx, |composer, _| composer.set_completion_open(false));
            cx.notify();
        }
    }

    fn transfer_id(&mut self) -> BulkTransferId {
        let id = self.next_transfer_id.clamp(1, (1u64 << 63) - 1);
        self.next_transfer_id = if id == (1u64 << 63) - 1 { 1 } else { id + 1 };
        BulkTransferId(id)
    }

    fn begin_submission(&mut self, room_id: RoomId, draft: Option<String>, cx: &mut Context<Self>) {
        let files = VecDeque::from(self.queued_files.take_all());
        let total_files = files.len();
        let phase = if draft.is_some() {
            let request_id = self.request_id();
            SubmissionPhase::AwaitingMessage { request_id }
        } else {
            SubmissionPhase::ReadyForUpload
        };
        self.pending_submission = Some(PendingSubmission {
            room_id,
            draft: draft.clone(),
            files,
            total_files,
            completed_files: 0,
            phase,
        });

        let Some(draft) = draft else {
            self.start_next_submission_upload(cx);
            return;
        };
        let SubmissionPhase::AwaitingMessage { request_id } = self
            .pending_submission
            .as_ref()
            .expect("submission was just installed")
            .phase
            .clone()
        else {
            unreachable!("message submission starts by waiting for its request")
        };
        self.model.pending.insert(
            request_id,
            PendingRequest {
                operation: Operation::SendMessage,
                room_id: Some(room_id),
                draft: Some(draft.clone()),
                transfer_id: None,
            },
        );
        if let Err(error) = self.daemon.send(ClientFrame::SendMessage {
            request_id,
            room_id,
            body: draft,
        }) {
            self.model.pending.remove(&request_id);
            self.fail_pending_submission(error, cx);
        } else {
            self.status = if total_files == 0 {
                "Sending message…".into()
            } else {
                format!("Sending message before {total_files} queued files…").into()
            };
            cx.notify();
        }
    }

    fn start_next_submission_upload(&mut self, cx: &mut Context<Self>) {
        let Some(mut submission) = self.pending_submission.take() else {
            return;
        };
        let Some(file) = submission.files.pop_front() else {
            self.status = if submission.total_files == 0 {
                "Message accepted".into()
            } else {
                format!(
                    "{} {} submitted",
                    submission.total_files,
                    if submission.total_files == 1 {
                        "file"
                    } else {
                        "files"
                    }
                )
                .into()
            };
            cx.notify();
            return;
        };

        let begin_request = self.request_id();
        let finish_request = self.request_id();
        let transfer_id = self.transfer_id();
        let room_id = submission.room_id;
        let file_name = file.file_name.clone();
        let source = file.source.clone();
        submission.phase = SubmissionPhase::Uploading(SubmittedUpload {
            file,
            begin_request,
            finish_request,
        });
        let position = submission.completed_files + 1;
        let total = submission.total_files;
        self.pending_submission = Some(submission);
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
        let upload_queued = match source {
            QueuedFileSource::Path(path) => {
                self.daemon.upload_file(
                    path,
                    room_id,
                    transfer_id,
                    begin_request,
                    finish_request,
                    self.model.limits.upload_bytes,
                );
                Ok(())
            }
            QueuedFileSource::Memory(bytes) => self.daemon.upload_bytes(
                bytes,
                file_name.clone(),
                room_id,
                transfer_id,
                begin_request,
                finish_request,
                self.model.limits.upload_bytes,
            ),
        };
        if let Err(error) = upload_queued {
            self.fail_pending_submission(error, cx);
            return;
        }
        self.status = format!("Sending file {position} of {total} · {file_name}").into();
        cx.notify();
    }

    fn handle_submission_result(
        &mut self,
        result: &local_rpc::frame::RequestResult,
        cx: &mut Context<Self>,
    ) -> bool {
        let phase = self
            .pending_submission
            .as_ref()
            .map(|submission| submission.phase.clone());
        match phase {
            Some(SubmissionPhase::AwaitingMessage { request_id })
                if request_id == result.request_id =>
            {
                match &result.outcome {
                    RequestOutcome::Accepted => {
                        let draft = self
                            .pending_submission
                            .as_mut()
                            .and_then(|submission| submission.draft.take());
                        if let Some(draft) = draft
                            && self.composer.read(cx).text() == draft
                        {
                            self.composer.update(cx, |composer, cx| composer.clear(cx));
                        }
                        if let Some(submission) = self.pending_submission.as_mut() {
                            submission.phase = SubmissionPhase::ReadyForUpload;
                        }
                        self.start_next_submission_upload(cx);
                    }
                    RequestOutcome::Rejected { message, .. } => {
                        self.fail_pending_submission(message.clone(), cx);
                    }
                }
                true
            }
            Some(SubmissionPhase::Uploading(upload))
                if upload.begin_request == result.request_id =>
            {
                match &result.outcome {
                    RequestOutcome::Accepted => {
                        if let Some(submission) = self.pending_submission.as_ref() {
                            self.status = format!(
                                "Sending file {} of {} · {}",
                                submission.completed_files + 1,
                                submission.total_files,
                                upload.file.file_name
                            )
                            .into();
                        }
                    }
                    RequestOutcome::Rejected { message, .. } => {
                        self.fail_pending_submission(message.clone(), cx);
                    }
                }
                true
            }
            Some(SubmissionPhase::Uploading(upload))
                if upload.finish_request == result.request_id =>
            {
                match &result.outcome {
                    RequestOutcome::Accepted => {
                        if let Some(submission) = self.pending_submission.as_mut() {
                            submission.completed_files += 1;
                            submission.phase = SubmissionPhase::ReadyForUpload;
                        }
                        self.start_next_submission_upload(cx);
                    }
                    RequestOutcome::Rejected { message, .. } => {
                        self.fail_pending_submission(message.clone(), cx);
                    }
                }
                true
            }
            _ => false,
        }
    }

    fn pending_submission_matches_upload(
        &self,
        begin_request: RequestId,
        finish_request: RequestId,
    ) -> bool {
        matches!(
            self.pending_submission
                .as_ref()
                .map(|submission| &submission.phase),
            Some(SubmissionPhase::Uploading(upload))
                if upload.begin_request == begin_request
                    && upload.finish_request == finish_request
        )
    }

    fn fail_pending_submission(&mut self, reason: String, cx: &mut Context<Self>) {
        let Some(submission) = self.pending_submission.take() else {
            self.set_composer_error(format!("Upload failed · {reason}"), cx);
            return;
        };
        if let Some(draft) = submission.draft.as_ref()
            && self.composer.read(cx).text().is_empty()
        {
            self.composer
                .update(cx, |composer, cx| composer.restore(draft.clone(), cx));
        }
        let files = self.recover_failed_submission_files(submission);
        self.queued_files.restore(files);
        self.set_composer_error(format!("Could not submit · {reason}"), cx);
    }

    fn abandon_disconnected_submission(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(submission) = self.pending_submission.take() else {
            return self.submission_outcome_unknown;
        };
        let ambiguous = submission.outcome_is_ambiguous();
        if let Some(draft) = submission.draft.as_ref()
            && self.composer.read(cx).text().is_empty()
        {
            self.composer
                .update(cx, |composer, cx| composer.restore(draft.clone(), cx));
        }
        let files = self.recover_failed_submission_files(submission);
        self.queued_files.restore(files);
        self.submission_outcome_unknown |= ambiguous;
        self.submission_outcome_unknown
    }

    fn recover_failed_submission_files(
        &mut self,
        submission: PendingSubmission,
    ) -> Vec<QueuedFile> {
        let (first_request, second_request) = submission.request_ids();
        if let Some(request_id) = first_request {
            self.model.pending.remove(&request_id);
        }
        if let Some(request_id) = second_request {
            self.model.pending.remove(&request_id);
        }
        submission.into_failed_files()
    }

    fn advance_video(&mut self, cx: &mut Context<Self>) {
        let drain = self.videos.drain();
        let playback_changed = drain.changed || !drain.errors.is_empty();
        let audio_drain = self.audios.drain();
        let audio_view_changed = audio_drain.view_changed || !audio_drain.errors.is_empty();
        let audio_source_changed =
            audio_drain.source_changed || !audio_drain.transport_failures.is_empty();
        let thumbnails_changed = self.video_thumbnails.drain_results();
        let finished_sources = self.video_thumbnails.take_finished_sources();
        let transport_failures = self.video_thumbnails.take_transport_failures();
        let source_work_changed =
            thumbnails_changed || !finished_sources.is_empty() || !transport_failures.is_empty();
        let transport_failure_keys = transport_failures
            .iter()
            .map(|(key, _)| *key)
            .collect::<HashSet<_>>();
        for key in finished_sources {
            if transport_failure_keys.contains(&key) {
                self.video_sources
                    .set_pin(key, VideoSourcePin::Thumbnail, false);
            } else {
                self.video_sources.thumbnail_finished(key);
            }
        }
        for (key, error) in transport_failures {
            self.video_sources.source_failed(key, error, Instant::now());
        }
        self.apply_video_drain(drain);
        self.apply_audio_drain(audio_drain);
        let media_source_work_changed =
            source_work_changed || playback_changed || audio_source_changed;
        if media_source_work_changed {
            self.sync_video_source_pins();
            self.pump_video_sources(cx);
        }
        if thumbnails_changed || audio_view_changed {
            cx.notify();
        }
    }

    fn source_key(&self, room_id: RoomId, attachment_id: AttachmentId) -> AttachmentSourceKey {
        AttachmentSourceKey {
            namespace: self.media_namespace_generation,
            room_id,
            attachment_id,
        }
    }

    fn video_descriptor(&self, key: VideoKey) -> Option<AttachmentDescriptor> {
        self.theater_video
            .as_ref()
            .filter(|video| video.key == key)
            .map(|video| video.descriptor.clone())
            .or_else(|| {
                self.model
                    .messages
                    .iter()
                    .find(|message| message.room_id == key.room_id && message.id == key.message_id)
                    .and_then(|message| message.attachment.as_ref())
                    .filter(|attachment| attachment.is_video())
                    .map(|attachment| attachment.descriptor.clone())
            })
    }

    fn audio_descriptor(&self, key: AudioKey) -> Option<AttachmentDescriptor> {
        self.model
            .messages
            .iter()
            .find(|message| message.room_id == key.room_id && message.id == key.message_id)
            .and_then(|message| message.attachment.as_ref())
            .filter(|attachment| attachment.is_audio())
            .map(|attachment| attachment.descriptor.clone())
    }

    fn reset_attachment_source_state(&mut self) {
        self.audios.clear_sessions();
        self.videos.clear_sessions();
        self.video_thumbnails.clear();
        self.clear_video_interactions();
        self.pending_video_plays.clear();
        self.pending_audio_plays.clear();
        self.visible_video_keys.clear();
        self.visible_audio_keys.clear();
        self.media_interactions.clear();
        self.audio_scrub = None;
        self.audio_volume_drag = None;
        self.video_source_retry_task.take();
        self.media_namespace_generation = self.media_namespace_generation.wrapping_add(1).max(1);
        let canceled = self.video_sources.reset(
            self.media_namespace_generation,
            self.model.limits.concurrent_attachment_streams,
        );
        for request_id in canceled {
            self.model.pending.remove(&request_id);
        }
        self.attachment_source_registry
            .clear(self.media_namespace_generation);
    }

    fn attachment_source_protocol_error(&mut self, reason: &str, cx: &mut Context<Self>) {
        log::error!("daemon attachment source protocol error: {reason}");
        self.reset_attachment_source_state();
        self.daemon.disconnect_protocol(reason);
        self.status = format!("Attachment source protocol error · {reason}").into();
        cx.notify();
    }

    fn pump_video_sources(&mut self, cx: &mut Context<Self>) {
        if !self.model.is_ready() {
            return;
        }
        self.video_sources
            .update_limits(self.model.limits.concurrent_attachment_streams);
        loop {
            let request_id = self.request_id();
            let Some(open) = self.video_sources.start_next(request_id, Instant::now()) else {
                break;
            };
            self.model.pending.insert(
                request_id,
                PendingRequest {
                    operation: Operation::OpenAttachmentSource,
                    room_id: Some(open.key.room_id),
                    draft: None,
                    transfer_id: None,
                },
            );
            let frame = ClientFrame::OpenAttachmentSource {
                request_id: open.request_id,
                room_id: open.key.room_id,
                attachment_id: open.key.attachment_id,
            };
            if let Err(error) = self.daemon.send(frame) {
                self.model.pending.remove(&request_id);
                self.video_sources
                    .failed_to_send(request_id, error.clone(), Instant::now());
                self.status = format!("Could not request attachment source · {error}").into();
            }
        }
        self.schedule_video_source_retry(cx);
    }

    fn schedule_video_source_retry(&mut self, cx: &mut Context<Self>) {
        self.video_source_retry_task.take();
        let Some(retry_at) = self.video_sources.next_retry_at() else {
            return;
        };
        let delay = retry_at.saturating_duration_since(Instant::now());
        self.video_source_retry_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            let _ = this.update(cx, |this, cx| {
                this.video_source_retry_task.take();
                this.pump_video_sources(cx);
                cx.notify();
            });
        }));
    }

    fn sync_video_source_pins(&mut self) {
        let mut playing = self.videos.retained_source_keys();
        playing.extend(self.audios.retained_source_keys());
        self.video_sources
            .sync_pins(VideoSourcePin::Playing, &playing);
        let theater = self
            .theater_video
            .as_ref()
            .map(|video| self.source_key(video.key.room_id, video.key.attachment_id))
            .into_iter()
            .collect::<HashSet<_>>();
        self.video_sources
            .sync_pins(VideoSourcePin::Theater, &theater);
        let mut pending = self
            .pending_video_plays
            .iter()
            .map(|video| self.source_key(video.room_id, video.attachment_id))
            .collect::<HashSet<_>>();
        pending.extend(
            self.pending_audio_plays
                .iter()
                .map(|audio| self.source_key(audio.room_id, audio.attachment_id)),
        );
        self.video_sources
            .sync_pins(VideoSourcePin::PendingPlay, &pending);
    }

    fn apply_video_drain(&mut self, drain: VideoDrain) {
        for error in &drain.errors {
            log::error!("embedded video failed: {error}");
        }
        if let Some(error) = drain.errors.last() {
            self.status = error.clone().into();
        }
    }

    fn apply_audio_drain(&mut self, drain: AudioDrain) {
        for (key, error) in drain.transport_failures {
            self.video_sources.source_failed(key, error, Instant::now());
        }
        for error in &drain.errors {
            log::error!("embedded audio failed: {error}");
        }
        if let Some(error) = drain.errors.last() {
            self.status = error.clone().into();
        }
    }

    fn clear_video_interactions(&mut self) {
        self.video_scrub = None;
        self.video_volume_drag = None;
        self.video_controls.clear();
        self.theater_video = None;
        self.video_controls_animation_task.take();
        self.video_controls_hide_task.take();
        self.video_volume_hide_task.take();
        self.video_surface_click_task.take();
        self.video_volume_popup_bounds.set(None);
        self.video_volume_button_bounds.set(None);
    }

    fn update_video_visibility(&mut self, range: std::ops::Range<usize>, cx: &mut Context<Self>) {
        let list_len = self.message_list.len();
        let visible_start = range.start.min(list_len);
        let visible_end = range.end.min(list_len);
        let mut visible = (visible_start..visible_end)
            .filter_map(|index| self.message_list.get(index))
            .filter(|item| !item.is_collapsed())
            .filter_map(|item| item.message_index())
            .filter_map(|index| self.model.messages.get(index))
            .filter_map(message_video_key)
            .collect::<HashSet<_>>();
        let visible_audio = (visible_start..visible_end)
            .filter_map(|index| self.message_list.get(index))
            .filter(|item| !item.is_collapsed())
            .filter_map(|item| item.message_index())
            .filter_map(|index| self.model.messages.get(index))
            .filter_map(message_audio_key)
            .collect::<HashSet<_>>();
        if let Some(theater) = self.theater_video.as_ref() {
            visible.insert(theater.key);
        }
        self.visible_video_keys = visible.clone();
        self.visible_audio_keys = visible_audio.clone();

        let mut ordered_rows = (visible_start..visible_end)
            .rev()
            .map(|index| (index, true))
            .collect::<Vec<_>>();
        for distance in 0..4 {
            if let Some(index) = visible_end
                .checked_add(distance)
                .filter(|index| *index < list_len)
            {
                ordered_rows.push((index, false));
            }
            if let Some(index) = visible_start.checked_sub(distance + 1) {
                ordered_rows.push((index, false));
            }
        }
        let mut seen = HashSet::new();
        let theater_source_key = self
            .theater_video
            .as_ref()
            .map(|video| self.source_key(video.key.room_id, video.key.attachment_id));
        let mut candidates = ordered_rows
            .into_iter()
            .filter_map(|(index, visible)| {
                let item = self.message_list.get(index)?;
                if item.is_collapsed() {
                    return None;
                }
                let message = self.model.messages.get(item.message_index()?)?;
                let attachment = message
                    .attachment
                    .as_ref()?
                    .is_video()
                    .then_some(message.attachment.as_ref()?)?;
                let key = self.source_key(message.room_id, attachment.descriptor.id);
                seen.insert(key).then(|| VideoSourceCandidate {
                    key,
                    descriptor: attachment.descriptor.clone(),
                    visible,
                })
            })
            .collect::<Vec<_>>();
        candidates.retain(|candidate| {
            theater_source_key == Some(candidate.key)
                || self
                    .video_thumbnails
                    .view(ThumbnailKey {
                        source_key: candidate.key,
                    })
                    .image
                    .is_none()
        });
        if !candidates.is_empty() {
            self.video_thumbnails.warm();
        }
        let canceled = self.video_sources.update_visibility(candidates);
        for request_id in canceled {
            self.model.pending.remove(&request_id);
        }
        let drain = self.videos.update_visibility(&visible);
        let audio_changed = self.audios.update_visibility(&visible_audio);
        let changed = drain.changed || !drain.errors.is_empty() || audio_changed;
        self.apply_video_drain(drain);
        self.sync_video_source_pins();
        self.pump_video_sources(cx);
        if changed {
            cx.notify();
        }
    }

    fn advance_live_video(&mut self) {
        let mut ended = Vec::new();
        for (stream_id, view) in &mut self.live_players {
            match view.player.drain_events() {
                Ok(playback) if playback.finished => {
                    ended.push((*stream_id, "Screen share ended".to_string()));
                }
                Ok(_) => {}
                Err(error) => {
                    log::error!(
                        "screen-share playback failed stream_id={}: {error:#}",
                        stream_id.0
                    );
                    ended.push((*stream_id, format!("Screen share failed · {error:#}")));
                }
            }
        }
        for (stream_id, status) in ended {
            self.live_players.remove(&stream_id);
            self.send_stop_live_share(stream_id);
            self.status = status.into();
        }
        if self.live_players.is_empty() {
            self.live_pane_resize = None;
        }
    }

    fn start_live_share(&mut self, stream_id: StreamId, cx: &mut Context<Self>) {
        if self.live_players.contains_key(&stream_id) || !self.model.is_ready() {
            return;
        }
        let request_id = self.request_id();
        self.model.pending.insert(
            request_id,
            PendingRequest {
                operation: Operation::StartLiveShare,
                room_id: self
                    .model
                    .live_shares
                    .iter()
                    .find(|share| share.stream_id == stream_id)
                    .map(|share| share.room_id),
                draft: None,
                transfer_id: None,
            },
        );
        if let Err(error) = self.daemon.send(ClientFrame::StartLiveShare {
            request_id,
            stream_id,
        }) {
            self.model.pending.remove(&request_id);
            self.status = error.into();
        } else {
            self.status = "Starting screen share…".into();
        }
        cx.notify();
    }

    fn stop_live_share(&mut self, stream_id: StreamId, cx: &mut Context<Self>) {
        self.live_players.remove(&stream_id);
        if self.live_players.is_empty() {
            self.live_pane_resize = None;
        }
        if self.fullscreen_share == Some(stream_id) {
            self.fullscreen_share = None;
        }
        self.send_stop_live_share(stream_id);
        self.status = "Stopped screen share".into();
        cx.notify();
    }

    fn send_stop_live_share(&mut self, stream_id: StreamId) {
        let request_id = self.request_id();
        self.model.pending.insert(
            request_id,
            PendingRequest {
                operation: Operation::StopLiveShare,
                room_id: None,
                draft: None,
                transfer_id: None,
            },
        );
        if self
            .daemon
            .send(ClientFrame::StopLiveShare {
                request_id,
                stream_id,
            })
            .is_err()
        {
            self.model.pending.remove(&request_id);
        }
    }

    fn reset_live_view(&mut self, stream_id: StreamId, cx: &mut Context<Self>) {
        if let Some(view) = self.live_players.get_mut(&stream_id) {
            view.zoom = MIN_LIVE_ZOOM;
            view.pan = point(px(0.), px(0.));
            view.last_mouse_position = None;
            cx.notify();
        }
    }

    fn zoom_live_view_at(
        &mut self,
        stream_id: StreamId,
        factor: f32,
        focal_point: Option<Point<Pixels>>,
        cx: &mut Context<Self>,
    ) {
        if let Some(view) = self.live_players.get_mut(&stream_id) {
            let old_zoom = view.zoom;
            let new_zoom = (old_zoom * factor).clamp(MIN_LIVE_ZOOM, MAX_LIVE_ZOOM);
            if let Some(viewport) = view.viewport_bounds.get() {
                let focal_point = focal_point.unwrap_or_else(|| viewport.center());
                view.pan = zoom_live_pan(
                    view.pan,
                    view.coded_size,
                    old_zoom,
                    new_zoom,
                    viewport,
                    focal_point,
                );
            } else if new_zoom == MIN_LIVE_ZOOM {
                view.pan = point(px(0.), px(0.));
            }
            view.zoom = new_zoom;
            cx.notify();
        }
    }

    fn zoom_live_view(&mut self, stream_id: StreamId, factor: f32, cx: &mut Context<Self>) {
        self.zoom_live_view_at(stream_id, factor, None, cx);
    }

    fn pan_live_view(&mut self, stream_id: StreamId, x: f32, y: f32, cx: &mut Context<Self>) {
        if let Some(view) = self.live_players.get_mut(&stream_id) {
            view.pan += point(px(x), px(y));
            if let Some(viewport) = view.viewport_bounds.get() {
                view.pan = clamp_live_pan(view.pan, view.coded_size, viewport, view.zoom);
            }
            cx.notify();
        }
    }

    fn live_zoom_in_action(&mut self, _: &LiveZoomIn, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(stream_id) = self.fullscreen_share {
            self.zoom_live_view(stream_id, 1.25, cx);
        }
    }

    fn live_zoom_out_action(&mut self, _: &LiveZoomOut, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(stream_id) = self.fullscreen_share {
            self.zoom_live_view(stream_id, 1.0 / 1.25, cx);
        }
    }

    fn live_reset_action(&mut self, _: &LiveReset, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(stream_id) = self.fullscreen_share {
            self.reset_live_view(stream_id, cx);
        }
    }

    fn live_pan_up_action(&mut self, _: &LivePanUp, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(stream_id) = self.fullscreen_share {
            self.pan_live_view(stream_id, 0.0, 30.0, cx);
        }
    }

    fn live_pan_down_action(&mut self, _: &LivePanDown, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(stream_id) = self.fullscreen_share {
            self.pan_live_view(stream_id, 0.0, -30.0, cx);
        }
    }

    fn scroll_live_view(
        &mut self,
        stream_id: StreamId,
        event: &ScrollWheelEvent,
        cx: &mut Context<Self>,
    ) {
        let delta = match event.delta {
            ScrollDelta::Pixels(delta) => f32::from(delta.y),
            ScrollDelta::Lines(delta) => delta.y * 20.0,
        };
        if delta == 0.0 {
            return;
        }
        let factor = if delta > 0.0 {
            1.0 + delta.abs() * 0.01
        } else {
            1.0 / (1.0 + delta.abs() * 0.01)
        };
        self.zoom_live_view_at(stream_id, factor, Some(event.position), cx);
        cx.stop_propagation();
    }

    fn pinch_live_view(&mut self, stream_id: StreamId, event: &PinchEvent, cx: &mut Context<Self>) {
        self.zoom_live_view_at(
            stream_id,
            (1.0 + event.delta).max(0.01),
            Some(event.position),
            cx,
        );
        cx.stop_propagation();
    }

    fn live_mouse_down(
        &mut self,
        stream_id: StreamId,
        event: &MouseDownEvent,
        cx: &mut Context<Self>,
    ) {
        if event.button != MouseButton::Left {
            return;
        }
        if event.click_count == 2 {
            let factor = if event.modifiers.shift { 0.5 } else { 2.0 };
            self.zoom_live_view_at(stream_id, factor, Some(event.position), cx);
            return;
        }
        if let Some(view) = self.live_players.get_mut(&stream_id) {
            view.last_mouse_position = Some(event.position);
        }
    }

    fn live_mouse_move(
        &mut self,
        stream_id: StreamId,
        event: &MouseMoveEvent,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.live_players.get_mut(&stream_id) else {
            return;
        };
        if let Some(last) = view.last_mouse_position {
            view.pan += event.position - last;
            if let Some(viewport) = view.viewport_bounds.get() {
                view.pan = clamp_live_pan(view.pan, view.coded_size, viewport, view.zoom);
            }
            view.last_mouse_position = Some(event.position);
            cx.notify();
        }
    }

    fn live_mouse_up(&mut self, stream_id: StreamId, cx: &mut Context<Self>) {
        if let Some(view) = self.live_players.get_mut(&stream_id) {
            view.last_mouse_position = None;
            cx.notify();
        }
    }

    fn begin_live_pane_resize(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(bounds) = self.live_pane_bounds.get() else {
            return;
        };
        let start_height = clamp_live_pane_height(
            bounds.size.height,
            window.viewport_size().height,
            window.rem_size(),
        );
        self.live_pane_height = Some(start_height);
        self.live_pane_resize = Some(LivePaneResize {
            start_y: event.position.y,
            start_height,
        });
        cx.stop_propagation();
        cx.notify();
    }

    fn drag_live_pane(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(resize) = self.live_pane_resize else {
            return;
        };
        if !event.dragging() {
            self.finish_live_pane_resize(cx);
            return;
        }
        self.live_pane_height = Some(clamp_live_pane_height(
            resize.start_height + event.position.y - resize.start_y,
            window.viewport_size().height,
            window.rem_size(),
        ));
        cx.stop_propagation();
        cx.notify();
    }

    fn finish_live_pane_resize(&mut self, cx: &mut Context<Self>) {
        if self.live_pane_resize.take().is_none() {
            return;
        }
        for view in self.live_players.values_mut() {
            if let Some(viewport) = view.viewport_bounds.get() {
                view.pan = clamp_live_pan(view.pan, view.coded_size, viewport, view.zoom);
            }
        }
        cx.notify();
    }

    fn toggle_live_fullscreen(
        &mut self,
        stream_id: StreamId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.fullscreen_share = if self.fullscreen_share == Some(stream_id) {
            None
        } else {
            Some(stream_id)
        };
        window.toggle_fullscreen();
        cx.notify();
    }

    fn release_live_players(&mut self, window: &mut Window) {
        self.live_players.clear();
        self.live_pane_resize = None;
        if self.fullscreen_share.take().is_some() && window.is_fullscreen() {
            window.toggle_fullscreen();
        }
    }

    fn apply_daemon_event(
        &mut self,
        event: DaemonEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            DaemonEvent::Discovering => {
                self.clear_command_surface(cx);
                self.model.phase = ConnectionPhase::Discovering;
            }
            DaemonEvent::Connecting => self.model.phase = ConnectionPhase::Connecting,
            DaemonEvent::TransportConnected => {
                self.clear_completion(cx);
                self.model.phase = ConnectionPhase::Syncing;
            }
            DaemonEvent::Disconnected(reason) => {
                log::error!("daemon disconnected: {reason}");
                let loaded = cx.global::<ConfigurationState>().0.clone();
                self.appearance_sync.disconnected(&loaded, cx);
                self.clear_command_surface(cx);
                let submission_outcome_unknown = self.abandon_disconnected_submission(cx);
                self.media_cache
                    .lock()
                    .expect("media cache lock poisoned")
                    .cancel_all();
                self.eager_image_fetches.reset_transient();
                self.preview_history.fail_code_fetches(&reason);
                self.model.resync_requested = false;
                self.model.phase = ConnectionPhase::Disconnected {
                    reason: reason.clone(),
                };
                if !self.model.pending.is_empty() {
                    self.model.pending.clear();
                    self.model.last_error =
                        Some("Connection changed; pending operations were not replayed".into());
                }
                self.pending_server_selection = None;
                self.pending_server_prompt = None;
                self.server_selection_target = None;
                self.pending_message_reference_click = None;
                self.pending_reference_media_preview = None;
                self.pending_message_jump = None;
                self.message_reference_flash = None;
                self.message_reference_flash_task = None;
                self.reset_attachment_source_state();
                self.status = if submission_outcome_unknown {
                    format!(
                        "Offline · Submission outcome unknown; verify the timeline after reconnecting · {reason}"
                    )
                    .into()
                } else {
                    format!("Offline · {reason}").into()
                };
                self.release_live_players(window);
                if let Some(settings) = &self.settings {
                    settings.update(cx, |settings, cx| settings.remote_disconnected(&reason, cx));
                }
                self.settings_remote_session = None;
                self.settings_close_when_opened = false;
            }
            DaemonEvent::Incompatible(details) => {
                log::error!("daemon connection is incompatible: {details}");
                let loaded = cx.global::<ConfigurationState>().0.clone();
                self.appearance_sync.disconnected(&loaded, cx);
                self.clear_command_surface(cx);
                let submission_outcome_unknown = self.abandon_disconnected_submission(cx);
                self.model.phase = ConnectionPhase::Incompatible {
                    details: details.clone(),
                };
                self.pending_server_selection = None;
                self.pending_server_prompt = None;
                self.server_selection_target = None;
                self.pending_message_reference_click = None;
                self.pending_reference_media_preview = None;
                self.pending_message_jump = None;
                self.message_reference_flash = None;
                self.message_reference_flash_task = None;
                self.reset_attachment_source_state();
                self.status = if submission_outcome_unknown {
                    format!(
                        "Cannot connect · Submission outcome unknown; verify before retrying · {details}"
                    )
                    .into()
                } else {
                    format!("Cannot connect · {details}").into()
                };
                if let Some(settings) = &self.settings {
                    settings.update(cx, |settings, cx| {
                        settings.remote_disconnected(&details, cx)
                    });
                }
                self.settings_remote_session = None;
                self.settings_close_when_opened = false;
            }
            DaemonEvent::UploadFailed {
                begin_request,
                finish_request,
                reason,
            } => {
                log::error!(
                    "upload failed begin_request={} finish_request={}: {reason}",
                    begin_request.0,
                    finish_request.0,
                );
                self.model.pending.remove(&begin_request);
                self.model.pending.remove(&finish_request);
                if self.pending_submission_matches_upload(begin_request, finish_request) {
                    self.fail_pending_submission(reason, cx);
                } else {
                    self.status = format!("Upload failed · {reason}").into();
                }
            }
            DaemonEvent::MediaCached(descriptor) => {
                log::info!(
                    "attachment cached attachment_timestamp_ms={} attachment_transfer_id={} file={:?} media_kind={:?} content_type={:?} bytes={}",
                    descriptor.id.timestamp_ms,
                    descriptor.id.transfer_id.0,
                    descriptor.file_name,
                    descriptor.media_kind,
                    descriptor.content_type,
                    descriptor.byte_len,
                );
                self.status = format!("Cached {}", descriptor.file_name).into();
                self.eager_image_fetches.cached(&descriptor);
                self.resume_code_preview_load(&descriptor, cx);
                self.pump_eager_image_fetches(cx);
                let pending = self
                    .pending_reference_media_preview
                    .as_ref()
                    .is_some_and(|pending| pending.attachment.descriptor.id == descriptor.id)
                    .then(|| self.pending_reference_media_preview.take())
                    .flatten();
                if let Some(pending) = pending {
                    self.open_message_reference_attachment(
                        pending.target,
                        pending.attachment,
                        window,
                        cx,
                    );
                }
            }
            DaemonEvent::MediaTransferFailed {
                transfer_id,
                reason,
            } => {
                log::error!(
                    "attachment transfer failed transfer_id={}: {reason}",
                    transfer_id.0,
                );
                let pending_failed =
                    self.pending_reference_media_preview
                        .as_ref()
                        .is_some_and(|pending| {
                            self.media_cache
                                .lock()
                                .expect("media cache lock poisoned")
                                .active_transfer(&pending.attachment.descriptor)
                                == Some(transfer_id)
                        });
                self.media_cache
                    .lock()
                    .expect("media cache lock poisoned")
                    .cancel(transfer_id);
                if pending_failed {
                    self.pending_reference_media_preview = None;
                }
                self.eager_image_fetches.failed(transfer_id, reason.clone());
                self.preview_history
                    .fail_code_transfer(transfer_id, &reason);
                self.status = reason.into();
                self.pump_eager_image_fetches(cx);
            }
            DaemonEvent::Frame(frame) => {
                self.apply_daemon_state_frame(frame, window, cx);
            }
            DaemonEvent::AttachmentSourceOpened {
                request_id,
                room_id,
                attachment_id,
                byte_len,
                transport,
                fd,
            } => {
                self.model.pending.remove(&request_id);
                let Some(key) = self.video_sources.pending_key(request_id) else {
                    // The visibility/reset path canceled this open while the
                    // descriptor-bearing response was in flight. Dispose of
                    // the descriptor here, before any state-machine dispatch.
                    drop(fd);
                    self.pump_video_sources(cx);
                    cx.notify();
                    return;
                };
                let pending_descriptor = self.video_sources.pending_descriptor(request_id).cloned();
                let expected_len = pending_descriptor
                    .as_ref()
                    .map(|descriptor| descriptor.byte_len);
                if key.room_id != room_id
                    || key.attachment_id != attachment_id
                    || expected_len != Some(byte_len)
                {
                    drop(fd);
                    self.attachment_source_protocol_error(
                        "attachment source response identity does not match its request",
                        cx,
                    );
                    return;
                }
                let source = match AttachmentSource::from_descriptor(
                    key,
                    byte_len,
                    transport,
                    fd,
                    self.model.limits.attachment_read_bytes,
                ) {
                    Ok(source) => source,
                    Err(error) => {
                        self.attachment_source_protocol_error(
                            &format!("invalid attachment source descriptor: {error:#}"),
                            cx,
                        );
                        return;
                    }
                };
                let registered = match self.video_sources.opened(
                    request_id,
                    source,
                    &self.attachment_source_registry,
                ) {
                    Ok(source) => source,
                    Err(error) => {
                        self.attachment_source_protocol_error(
                            &format!("invalid attachment source completion: {error:#}"),
                            cx,
                        );
                        return;
                    }
                };
                if pending_descriptor.as_ref().is_some_and(|descriptor| {
                    Attachment {
                        descriptor: descriptor.clone(),
                    }
                    .is_video()
                }) {
                    self.videos.prepare();
                }
                let pending = self
                    .pending_video_plays
                    .iter()
                    .copied()
                    .filter(|video| self.source_key(video.room_id, video.attachment_id) == key)
                    .collect::<Vec<_>>();
                for video in pending {
                    self.videos.ensure_source(video, registered.clone());
                    if let Err(error) = self.videos.play(video) {
                        self.status = format!("Playback failed: {error}").into();
                    }
                    self.pending_video_plays.remove(&video);
                }
                let pending_audio = self
                    .pending_audio_plays
                    .iter()
                    .copied()
                    .filter(|audio| self.source_key(audio.room_id, audio.attachment_id) == key)
                    .collect::<Vec<_>>();
                for audio in pending_audio {
                    let drain = self.audios.provide_source(audio, registered.clone());
                    self.apply_audio_drain(drain);
                    self.pending_audio_plays.remove(&audio);
                }
                if !self
                    .pending_video_plays
                    .iter()
                    .any(|video| self.source_key(video.room_id, video.attachment_id) == key)
                    && !self
                        .pending_audio_plays
                        .iter()
                        .any(|audio| self.source_key(audio.room_id, audio.attachment_id) == key)
                {
                    self.video_sources
                        .set_pin(key, VideoSourcePin::PendingPlay, false);
                }
                self.sync_video_source_pins();
                self.pump_video_sources(cx);
                cx.notify();
            }
            DaemonEvent::LiveShareOpened {
                request_id,
                stream_id,
                stream,
            } => {
                self.model.pending.remove(&request_id);
                let Some(share) = self
                    .model
                    .live_shares
                    .iter()
                    .find(|share| share.stream_id == stream_id)
                    .cloned()
                else {
                    self.status = "Screen share ended before playback started".into();
                    return;
                };
                let coded_size = (share.coded_width, share.coded_height);
                match MpvPlayer::new_live(self.video_wakeup.clone(), share, stream) {
                    Ok(player) => {
                        self.live_players.insert(
                            stream_id,
                            LivePlayerView {
                                player,
                                zoom: 1.0,
                                pan: point(px(0.), px(0.)),
                                last_mouse_position: None,
                                coded_size,
                                viewport_bounds: Rc::new(Cell::new(None)),
                            },
                        );
                        self.status = "Playing live screen share".into();
                    }
                    Err(error) => {
                        self.status = format!("Could not play screen share · {error:#}").into();
                        self.send_stop_live_share(stream_id);
                    }
                }
            }
        }
    }

    fn install_prepared_messages(
        &mut self,
        prepared: Vec<(RoomId, u64, PreparedFormattedMessage)>,
    ) {
        for (room_id, message_id, prepared) in prepared {
            if self.model.selected_room != Some(room_id) {
                continue;
            }
            let Ok(index) = self
                .model
                .messages
                .binary_search_by_key(&message_id, |message| message.id)
            else {
                continue;
            };
            if self.model.messages[index].body.as_str() != prepared.source() {
                continue;
            }
            if self
                .formatted_messages
                .get(&message_id)
                .is_some_and(|formatted| formatted.source() == prepared.source())
            {
                continue;
            }
            self.formatted_messages.insert(
                message_id,
                Rc::new(FormattedMessage::from_prepared(prepared)),
            );
        }
    }

    fn rebuild_message_list(&mut self) {
        let next = timeline::build_timeline_list(
            &self.model.messages,
            &self.command_rows,
            &self.collapsed_sections,
        );
        let common_prefix = self
            .message_list
            .iter()
            .zip(&next)
            .take_while(|(old, new)| old.has_same_visible_state(**new))
            .count();
        let suffix_limit = self.message_list.len().min(next.len()) - common_prefix;
        let common_suffix = self
            .message_list
            .iter()
            .rev()
            .zip(next.iter().rev())
            .take(suffix_limit)
            .take_while(|(old, new)| old.has_same_visible_state(**new))
            .count();

        if common_prefix + common_suffix < self.message_list.len()
            || common_prefix + common_suffix < next.len()
        {
            self.list_state.splice(
                common_prefix..self.message_list.len() - common_suffix,
                next.len() - common_prefix - common_suffix,
            );
        }
        self.message_list = next;
        self.timeline_selection.retain_items(
            self.message_list
                .iter()
                .copied()
                .map(timeline_selection_key),
        );
        debug_assert_eq!(self.list_state.item_count(), self.message_list.len());
    }

    fn toggle_message_group(&mut self, message_id: u64, cx: &mut Context<Self>) {
        if !timeline::toggle_collapsed_section(
            &self.model.messages,
            &mut self.collapsed_sections,
            message_id,
        ) {
            return;
        }
        self.timeline_selection.clear();
        self.rebuild_message_list();
        cx.notify();
    }

    fn apply_daemon_state_frame(
        &mut self,
        frame: DaemonFrame,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match frame {
            DaemonFrame::Appearance(event) => {
                let loaded = cx.global::<ConfigurationState>().0.clone();
                let (settings_update, reconcile) = match &event {
                    local_rpc::appearance::AppearanceEvent::Preview { session_id, .. } => {
                        (Some((Some(*session_id), None)), None)
                    }
                    local_rpc::appearance::AppearanceEvent::Committed { document, .. } => {
                        match AppearanceConfig::from_document(document) {
                            Ok(appearance) => (
                                Some((None, Some(Some(appearance.clone())))),
                                Some(appearance),
                            ),
                            Err(error) => {
                                log::warn!("ignored invalid shared appearance: {error}");
                                self.status =
                                    format!("Ignored invalid shared appearance · {error}").into();
                                cx.notify();
                                return;
                            }
                        }
                    }
                    local_rpc::appearance::AppearanceEvent::Cleared { .. } => {
                        (Some((None, Some(None))), None)
                    }
                };
                if let Err(error) = self.appearance_sync.apply_event(event, &loaded, cx) {
                    log::warn!("ignored invalid shared appearance: {error}");
                    self.status = format!("Ignored invalid shared appearance · {error}").into();
                    cx.notify();
                } else if let (Some(settings), Some((preview, committed))) =
                    (&self.settings, settings_update)
                {
                    settings.update(cx, |settings, cx| {
                        if let Some(session_id) = preview {
                            settings.shared_preview_changed(session_id, cx);
                        }
                        if let Some(appearance) = committed {
                            settings.shared_committed_appearance_changed(appearance.as_ref(), cx);
                        }
                    });
                }
                if let Some(appearance) = reconcile {
                    self.reconcile_committed_appearance(appearance, cx);
                }
                return;
            }
            DaemonFrame::SettingsResult(result) => {
                let opened = match &result.payload {
                    local_rpc::settings::SettingsResultPayload::Document(document)
                        if result.result.operation == local_rpc::frame::Operation::OpenSettings =>
                    {
                        Some(document.session_id)
                    }
                    _ => None,
                };
                if let Some(session_id) = opened {
                    self.settings_remote_session = Some(session_id);
                }
                if let local_rpc::settings::SettingsResultPayload::Closed { session_id } =
                    &result.payload
                    && self.settings_remote_session == Some(*session_id)
                {
                    self.settings_remote_session = None;
                }
                if let Some(settings) = &self.settings {
                    if opened.is_some() {
                        self.settings_close_when_opened = false;
                    }
                    settings.update(cx, |settings, cx| settings.apply_remote_result(result, cx));
                } else if let Some(session_id) = opened
                    && self.settings_close_when_opened
                {
                    self.settings_close_when_opened = false;
                    self.send_settings_command(
                        local_rpc::settings::SettingsCommand::Close { session_id },
                        cx,
                    );
                }
                return;
            }
            DaemonFrame::SettingsEvent(event) => {
                if let Some(settings) = &self.settings {
                    settings.update(cx, |settings, cx| settings.apply_remote_event(event, cx));
                }
                return;
            }
            DaemonFrame::Welcome(_) => {
                self.appearance_sync.daemon_reconnected();
                if let Some(settings) = &self.settings {
                    settings.update(cx, |settings, cx| {
                        settings.remote_reconnected(cx);
                        settings.republish_appearance(cx);
                    });
                }
            }
            _ => {}
        }
        let pending = match &frame {
            DaemonFrame::RequestResult(result) => {
                self.model.pending.get(&result.request_id).cloned()
            }
            DaemonFrame::CommandResult { result, .. } => {
                self.model.pending.get(&result.request_id).cloned()
            }
            _ => None,
        };
        let old_selected_room = self.model.selected_room;
        let old_daemon_instance = self.model.daemon_instance;
        let old_active_server = self.model.active_server.clone();
        let old_server_selector_visible = self.server_selector_visible();
        let old_server_error = self.model.server_selection.error.clone();
        let old_server_prompt = self.model.server_selection.prompt.clone();
        let effect = reducer::apply(&mut self.model, frame);
        let reference_room_snapshot = effect.room_snapshot;
        let reference_history_changed = effect.history_changed;
        if let Some(result) = effect.request_result.as_ref() {
            if self.pending_server_selection == Some(result.request_id) {
                self.pending_server_selection = None;
                if matches!(result.outcome, RequestOutcome::Rejected { .. }) {
                    self.server_selection_target = None;
                }
            }
            if self
                .pending_server_prompt
                .is_some_and(|(request_id, _)| request_id == result.request_id)
            {
                let accepted_prompt = self
                    .pending_server_prompt
                    .take()
                    .is_some_and(|(_, accept)| accept);
                if matches!(result.outcome, RequestOutcome::Rejected { .. }) && accepted_prompt {
                    self.server_selection_target = None;
                }
            }
        }
        if self.model.server_selection.error != old_server_error {
            if let Some(error) = &self.model.server_selection.error {
                self.server_selection_target = None;
                self.status = error.message.clone().into();
            }
        }
        if self.model.server_selection.prompt != old_server_prompt
            && self.model.server_selection.prompt.is_some()
        {
            self.server_selector_open = true;
        }
        if self
            .server_selection_target
            .as_ref()
            .is_some_and(|target| self.model.active_server.as_ref() == Some(target))
            && self.model.server_selection.prompt.is_none()
        {
            self.server_selection_target = None;
            self.server_selector_open = false;
            self.server_search_input
                .update(cx, |input, cx| input.clear(cx));
            window.focus(&self.composer.focus_handle(cx), cx);
        }
        if !old_server_selector_visible && self.server_selector_visible() {
            window.focus(&self.server_search_input.focus_handle(cx), cx);
        }
        if self.model.selected_room != old_selected_room {
            self.collapsed_sections.clear();
        }
        let media_namespace_changed = self.model.daemon_instance != old_daemon_instance
            || self.model.active_server != old_active_server;
        if self.model.selected_room != old_selected_room
            || effect.replace_messages
            || media_namespace_changed
            || !self.model.is_ready()
        {
            self.clear_command_surface(cx);
        }
        if media_namespace_changed {
            let preview_was_open = self.preview_history.active().is_some();
            self.media_cache
                .lock()
                .expect("media cache lock poisoned")
                .clear();
            self.image_cache
                .update(cx, |cache, cx| cache.clear(window, cx));
            self.preview_image_cache
                .update(cx, |cache, cx| cache.clear(window, cx));
            self.eager_image_fetches.clear();
            self.code_load_tasks.clear();
            self.close_code_search(cx);
            self.code_selection.clear();
            self.preview_history.clear();
            if preview_was_open {
                self.restore_preview_focus(window, cx);
            } else {
                self.preview_return_focus = None;
            }
            self.preview_image_viewport.set(None);
            self.preview_last_mouse_position = None;
            self.preview_pane_resize = None;
            self.message_reference_hover = None;
            self.message_reference_hover_task = None;
            self.message_reference_cache.clear();
            self.pending_message_reference_click = None;
            self.pending_reference_media_preview = None;
            self.pending_message_jump = None;
            self.message_reference_flash = None;
            self.message_reference_flash_task = None;
        }
        if media_namespace_changed || self.model.selected_room != old_selected_room {
            self.reset_attachment_source_state();
        }
        let available = self
            .model
            .live_shares
            .iter()
            .map(|share| share.stream_id)
            .collect::<HashSet<_>>();
        self.live_players
            .retain(|stream_id, _| available.contains(stream_id));
        if self
            .fullscreen_share
            .is_some_and(|stream_id| !available.contains(&stream_id))
        {
            self.fullscreen_share = None;
            if window.is_fullscreen() {
                window.toggle_fullscreen();
            }
        }
        if self.model.selected_room != old_selected_room || effect.replace_messages {
            self.timeline_selection.clear();
        }
        if self.model.selected_room != old_selected_room {
            self.formatted_messages.clear();
            self.eager_image_fetches.reset_transient();
        } else if effect.messages_changed {
            if let Some(room_id) = self.model.selected_room {
                self.message_reference_cache
                    .retain(|target, _| target.room_id != room_id);
            }
            self.formatted_messages.retain(|message_id, _| {
                self.model
                    .messages
                    .binary_search_by_key(message_id, |message| message.id)
                    .is_ok()
            });
            let retained = self
                .model
                .messages
                .iter()
                .filter_map(message_video_key)
                .collect::<HashSet<_>>();
            let drain = self.videos.retain_sources(&retained);
            self.apply_video_drain(drain);
            let retained_audio = self
                .model
                .messages
                .iter()
                .filter_map(message_audio_key)
                .collect::<HashSet<_>>();
            let audio_drain = self.audios.retain_sources(&retained_audio);
            let audio_source_changed =
                audio_drain.source_changed || !audio_drain.transport_failures.is_empty();
            self.apply_audio_drain(audio_drain);
            if audio_source_changed {
                self.sync_video_source_pins();
                self.pump_video_sources(cx);
            }
            self.visible_video_keys.retain(|key| retained.contains(key));
            self.visible_audio_keys
                .retain(|key| retained_audio.contains(key));
            self.media_interactions.retain(|target| match target {
                MediaPlaybackTarget::Audio(key) => retained_audio.contains(key),
                MediaPlaybackTarget::Video(key) => retained.contains(key),
            });
            if self
                .theater_video
                .as_ref()
                .is_some_and(|theater| !retained.contains(&theater.key))
            {
                self.clear_video_interactions();
            }
        }
        if self.model.selected_room != old_selected_room || effect.messages_changed {
            self.rebuild_message_list();
        }
        if effect.messages_changed {
            self.enqueue_new_image_fetches(window, cx);
        }
        if effect.request_resync {
            self.clear_command_surface(cx);
            let request_id = self.request_id();
            if let Err(error) = self
                .daemon
                .send(ClientFrame::RequestSnapshot { request_id })
            {
                self.model.resync_requested = false;
                self.status = format!("Could not request daemon resync · {error}").into();
            }
        }
        if let Some((request_id, kind, items)) = effect.command_candidates
            && self.candidate_requests.get(&kind) == Some(&request_id)
        {
            self.candidate_requests.remove(&kind);
            self.command_candidates.insert(kind, items);
            self.refresh_completion(cx);
        }
        if let Some((request_id, room_id, message_id, message)) = effect.message_reference_resolved
        {
            let target = MessageRef {
                room_id,
                message_id,
            };
            let message = message.map(timeline::from_daemon);
            let activate = self
                .pending_message_reference_click
                .as_ref()
                .is_some_and(|pending| {
                    pending.request_id == request_id && pending.target == target
                });
            if activate {
                self.pending_message_reference_click = None;
            }
            self.message_reference_cache.insert(target, message.clone());
            self.install_message_reference_preview(target, message.clone(), Some(request_id), cx);
            if activate {
                if let Some(message) = message {
                    self.open_message_reference_target(target, message, window, cx);
                } else {
                    self.jump_to_message_reference(target, cx);
                }
            }
        }
        let mut command_result_applied = false;
        if let Some((result, lines)) = effect.command_result {
            command_result_applied = true;
            let matching = self
                .pending_command
                .as_ref()
                .is_some_and(|pending| pending.request_id == result.request_id);
            if matching {
                let submitted = self
                    .pending_command
                    .take()
                    .expect("matching command pending");
                match result.outcome {
                    RequestOutcome::Accepted => {
                        if self.composer.read(cx).text() == submitted.draft {
                            self.composer.update(cx, |composer, cx| composer.clear(cx));
                        }
                        self.append_command_output(lines);
                        self.status = "Command completed".into();
                    }
                    RequestOutcome::Rejected { code, message } => {
                        log::error!(
                            "daemon command rejected request_id={} code={code}: {message}",
                            result.request_id.0,
                        );
                        self.append_command_output(lines);
                        self.status = message.into();
                    }
                }
            }
        }
        if let Some(result) = effect.request_result {
            if result.operation == Operation::OpenAttachmentSource {
                match result.outcome {
                    RequestOutcome::Accepted => {
                        self.attachment_source_protocol_error(
                            "attachment source open returned an accepted result without a descriptor",
                            cx,
                        );
                    }
                    RequestOutcome::Rejected { code, message } => {
                        let matched = self.video_sources.rejected(
                            result.request_id,
                            code,
                            message.clone(),
                            Instant::now(),
                        );
                        if matched {
                            self.status = message.into();
                            self.pump_video_sources(cx);
                            cx.notify();
                        }
                    }
                }
                return;
            }
            let submission_result = self.handle_submission_result(&result, cx);
            let reference_jump_request = self.pending_message_jump.as_ref().is_some_and(|jump| {
                jump.room_request_id == Some(result.request_id)
                    || jump.page_request_id == Some(result.request_id)
            });
            if reference_jump_request
                && let RequestOutcome::Rejected { message, .. } = &result.outcome
            {
                self.pending_message_jump = None;
                self.status = message.clone().into();
            } else if reference_jump_request
                && matches!(&result.outcome, RequestOutcome::Accepted)
                && let Some(jump) = self.pending_message_jump.as_mut()
                && jump.room_request_id == Some(result.request_id)
            {
                jump.room_request_id = None;
            }
            if !submission_result && !reference_jump_request {
                match result.outcome {
                    RequestOutcome::Accepted => {
                        log::info!(
                            "daemon result applied request_id={} operation={:?} outcome=accepted",
                            result.request_id.0,
                            result.operation,
                        );
                        self.status =
                            format!("{} accepted", operation_label(&result.operation)).into();
                        if let Some(pending) = pending.as_ref()
                            && pending.operation == Operation::EditMessage
                            && pending.draft.as_deref()
                                == Some(self.composer.read(cx).text().as_str())
                        {
                            self.composer.update(cx, |composer, cx| composer.clear(cx));
                            self.editing = None;
                        }
                    }
                    RequestOutcome::Rejected { code, message } => {
                        log::error!(
                            "daemon result applied request_id={} operation={:?} outcome=rejected code={code}: {message}",
                            result.request_id.0,
                            result.operation,
                        );
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
                            self.preview_history
                                .fail_code_transfer(transfer_id, &message);
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
            }
        } else if !command_result_applied
            && self.model.is_ready()
            && self.model.last_error.is_none()
            && self.model.server_selection.error.is_none()
        {
            self.status = connection_label(&self.model).into();
        }
        if reference_room_snapshot {
            self.resume_message_reference_jump(cx);
        } else if reference_history_changed {
            if let Some(jump) = self.pending_message_jump.as_mut() {
                jump.page_request_id = None;
            }
            self.resume_message_reference_jump(cx);
        }
    }

    fn send_message(&mut self, _: &SendMessage, _: &mut Window, cx: &mut Context<Self>) {
        if !self.model.is_ready() {
            self.set_composer_error("Cannot send until the Chatt daemon is connected", cx);
            return;
        }
        if self.submission_outcome_unknown {
            self.submission_outcome_unknown = false;
            self.set_composer_error(
                "Previous submission outcome is unknown · Verify the timeline, then press Enter again to resend",
                cx,
            );
            return;
        }
        if self.file_inspection_pending {
            self.set_composer_error("Wait for attached files to finish checking", cx);
            return;
        }
        if self.pending_submission.is_some() {
            self.set_composer_error("Wait for the current submission to finish", cx);
            return;
        }
        let composer_empty = self.composer.read(cx).is_empty();
        if composer_empty && (self.editing.is_some() || self.queued_files.is_empty()) {
            return;
        }
        let draft = self.composer.read(cx).text();
        if self.editing.is_none() && draft.starts_with('/') {
            if self.pending_command.is_some() {
                self.set_composer_error("Wait for the current command to finish", cx);
                return;
            }
            if draft.contains(['\r', '\n']) {
                self.set_composer_error("Slash commands must fit on one line", cx);
                return;
            }
            self.composer_error = None;
            let request_id = self.request_id();
            self.model.pending.insert(
                request_id,
                PendingRequest {
                    operation: Operation::RunCommand,
                    room_id: self.model.selected_room,
                    draft: Some(draft.clone()),
                    transfer_id: None,
                },
            );
            self.pending_command = Some(PendingCommand {
                request_id,
                draft: draft.clone(),
            });
            self.completion_session = None;
            self.composer
                .update(cx, |composer, _| composer.set_completion_open(false));
            if let Err(error) = self.daemon.send(ClientFrame::RunCommand {
                request_id,
                body: draft,
            }) {
                self.model.pending.remove(&request_id);
                self.pending_command = None;
                self.status = error.into();
            } else {
                self.status = "Running command…".into();
            }
            cx.notify();
            return;
        }
        let Some(selected_room) = self.model.selected_room else {
            self.set_composer_error("Select a room before sending", cx);
            return;
        };
        if let Some((room_id, target)) = self
            .editing
            .as_ref()
            .map(|editing| (editing.room_id, editing.target))
        {
            let request_id = self.request_id();
            self.model.pending.insert(
                request_id,
                PendingRequest {
                    operation: Operation::EditMessage,
                    room_id: Some(room_id),
                    draft: Some(draft.clone()),
                    transfer_id: None,
                },
            );
            if let Err(error) = self.daemon.send(ClientFrame::EditMessage {
                request_id,
                room_id,
                target,
                body: draft,
            }) {
                self.model.pending.remove(&request_id);
                self.status = error.into();
            } else {
                self.status = "Saving edit…".into();
            }
            cx.notify();
            return;
        }

        self.composer_error = None;
        self.begin_submission(selected_room, (!composer_empty).then_some(draft), cx);
    }

    fn begin_edit(
        &mut self,
        room_id: RoomId,
        message_id: local_rpc::ids::MessageId,
        body: String,
        cx: &mut Context<Self>,
    ) {
        if !self.model.is_ready() {
            return;
        }
        if self.pending_submission.is_some() {
            self.status = "Wait for the current submission to finish".into();
            cx.notify();
            return;
        }
        if self.file_inspection_pending {
            self.status = "Wait for attached files to finish checking".into();
            cx.notify();
            return;
        }
        self.editing = Some(EditingMessage {
            room_id,
            target: message_id,
        });
        self.composer
            .update(cx, |composer, cx| composer.restore(body, cx));
        self.status = "Editing message · Enter saves".into();
        cx.notify();
    }

    fn delete_message(
        &mut self,
        room_id: RoomId,
        message_id: local_rpc::ids::MessageId,
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
        if self
            .pending_message_jump
            .as_ref()
            .is_some_and(|jump| jump.target.room_id != room_id)
        {
            self.pending_message_jump = None;
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

    fn server_selector_visible(&self) -> bool {
        self.model.is_ready()
            && (self.model.active_server.is_none()
                || self.server_selector_open
                || self.model.server_selection.error.is_some()
                || self.model.server_selection.prompt.is_some())
    }

    fn filtered_servers(&self, cx: &App) -> Vec<ServerSummary> {
        let query = self.server_search_input.read(cx).text();
        self.model
            .server_selection
            .servers
            .iter()
            .filter(|server| server_matches_query(server, &query))
            .cloned()
            .collect()
    }

    fn reconcile_server_selection(&mut self, cx: &App) -> Option<usize> {
        let servers = self.filtered_servers(cx);
        if servers.is_empty() {
            self.selected_server_label = None;
            return None;
        }
        if let Some(index) = server_index_for_label(&servers, self.selected_server_label.as_deref())
        {
            return Some(index);
        }
        self.selected_server_label = Some(servers[0].label.clone());
        Some(0)
    }

    fn selected_server_index(&self, servers: &[ServerSummary]) -> Option<usize> {
        server_index_for_label(servers, self.selected_server_label.as_deref())
    }

    fn server_switch_block_reason(&self) -> Option<&'static str> {
        let pending_activity = self.pending_submission.is_some()
            || self.pending_command.is_some()
            || self.model.pending.values().any(|pending| {
                matches!(
                    pending.operation,
                    Operation::SendMessage
                        | Operation::EditMessage
                        | Operation::DeleteMessage
                        | Operation::BeginUpload
                        | Operation::FinishUpload
                )
            });
        server_switch_guard_reason(pending_activity, !self.model.transfers.is_empty())
    }

    fn open_server_selector(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.model.is_ready() {
            return;
        }
        if let Some(reason) = self.server_switch_block_reason() {
            self.status = reason.into();
            cx.notify();
            return;
        }
        self.composer_menu_open = false;
        self.composer_menu_action_taken = false;
        self.server_selector_open = true;
        self.selected_server_label = self.model.active_server.clone();
        self.server_search_input
            .update(cx, |input, cx| input.clear(cx));
        if let Some(index) = self.reconcile_server_selection(cx) {
            self.server_list_scroll.scroll_to_item(index);
        }
        window.focus(&self.server_search_input.focus_handle(cx), cx);
        cx.notify();
    }

    fn toggle_composer_menu(&mut self, cx: &mut Context<Self>) {
        if self.composer_menu_open {
            self.dismiss_composer_menu(cx);
            return;
        }
        self.composer_menu_open = true;
        self.composer_menu_action_taken = false;
        cx.notify();
    }

    fn dismiss_composer_menu(&mut self, cx: &mut Context<Self>) {
        if !self.composer_menu_open {
            return;
        }
        self.composer_menu_open = false;
        self.composer_menu_action_taken = false;
        cx.notify();
    }

    fn toggle_rooms_sidebar(&mut self, cx: &mut Context<Self>) {
        self.show_rooms_sidebar = !self.show_rooms_sidebar;
        self.composer_menu_action_taken = true;
        cx.notify();
    }

    fn toggle_top_status_bar(&mut self, cx: &mut Context<Self>) {
        self.show_top_status_bar = !self.show_top_status_bar;
        self.composer_menu_action_taken = true;
        cx.notify();
    }

    fn close_server_selector(
        &mut self,
        _: &CloseServerSelector,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.model.active_server.is_none()
            || self.model.server_selection.error.is_some()
            || self.model.server_selection.prompt.is_some()
            || self.pending_server_selection.is_some()
            || self.pending_server_prompt.is_some()
        {
            return;
        }
        self.server_selector_open = false;
        self.server_search_input
            .update(cx, |input, cx| input.clear(cx));
        window.focus(&self.composer.focus_handle(cx), cx);
        cx.notify();
    }

    fn server_next(&mut self, _: &ServerNext, _: &mut Window, cx: &mut Context<Self>) {
        let servers = self.filtered_servers(cx);
        let count = servers.len();
        if count == 0 {
            return;
        }
        let selected = self
            .selected_server_index(&servers)
            .map_or(0, |index| (index + 1) % count);
        self.selected_server_label = Some(servers[selected].label.clone());
        self.server_list_scroll.scroll_to_item(selected);
        cx.notify();
    }

    fn server_previous(&mut self, _: &ServerPrevious, _: &mut Window, cx: &mut Context<Self>) {
        let servers = self.filtered_servers(cx);
        let count = servers.len();
        if count == 0 {
            return;
        }
        let selected = self
            .selected_server_index(&servers)
            .and_then(|index| index.checked_sub(1))
            .unwrap_or(count - 1);
        self.selected_server_label = Some(servers[selected].label.clone());
        self.server_list_scroll.scroll_to_item(selected);
        cx.notify();
    }

    fn server_activate(&mut self, _: &ServerActivate, window: &mut Window, cx: &mut Context<Self>) {
        let servers = self.filtered_servers(cx);
        let selected = self.selected_server_index(&servers).unwrap_or(0);
        if let Some(server) = servers.get(selected).cloned() {
            self.select_server(server, window, cx);
        }
    }

    fn select_server(
        &mut self,
        server: ServerSummary,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.model.is_ready()
            || self.pending_server_selection.is_some()
            || self.pending_server_prompt.is_some()
        {
            return;
        }
        if server.availability == ServerAvailability::PairingIncomplete {
            self.status =
                "Server pairing is incomplete; finish pairing in the terminal client".into();
            cx.notify();
            return;
        }
        if self.model.active_server.as_deref() == Some(server.label.as_str())
            && self.model.server_selection.error.is_none()
        {
            self.server_selector_open = false;
            self.server_search_input
                .update(cx, |input, cx| input.clear(cx));
            window.focus(&self.composer.focus_handle(cx), cx);
            cx.notify();
            return;
        }
        if self.model.active_server.is_some()
            && let Some(reason) = self.server_switch_block_reason()
        {
            self.status = reason.into();
            cx.notify();
            return;
        }

        let request_id = self.request_id();
        self.model.pending.insert(
            request_id,
            PendingRequest {
                operation: Operation::SelectServer,
                room_id: None,
                draft: Some(server.label.clone()),
                transfer_id: None,
            },
        );
        self.pending_server_selection = Some(request_id);
        self.server_selection_target = Some(server.label.clone());
        if let Err(error) = self.daemon.send(ClientFrame::SelectServer {
            request_id,
            label: server.label.clone(),
        }) {
            self.model.pending.remove(&request_id);
            self.pending_server_selection = None;
            self.server_selection_target = None;
            self.status = error.into();
        } else {
            self.status = format!("Connecting to {}…", server.label).into();
        }
        cx.notify();
    }

    fn resolve_server_prompt(&mut self, accept: bool, cx: &mut Context<Self>) {
        if self.pending_server_prompt.is_some() {
            return;
        }
        let Some(ServerSelectionPrompt::AllowUnencryptedTransport { label, attempt_id }) =
            self.model.server_selection.prompt.clone()
        else {
            return;
        };
        let request_id = self.request_id();
        self.model.pending.insert(
            request_id,
            PendingRequest {
                operation: Operation::ResolveServerPrompt,
                room_id: None,
                draft: Some(label.clone()),
                transfer_id: None,
            },
        );
        self.pending_server_prompt = Some((request_id, accept));
        if accept {
            self.server_selection_target = Some(label);
        } else {
            self.server_selection_target = None;
        }
        if let Err(error) = self.daemon.send(ClientFrame::ResolveServerPrompt {
            request_id,
            attempt_id,
            accept,
        }) {
            self.model.pending.remove(&request_id);
            self.pending_server_prompt = None;
            self.status = error.into();
        } else {
            self.status = if accept {
                "Saving security preference and reconnecting…".into()
            } else {
                "Canceling connection…".into()
            };
        }
        cx.notify();
    }

    fn load_older(&mut self, cx: &mut Context<Self>) {
        if !self.model.is_ready() || self.model.at_start || self.pending_message_jump.is_some() {
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

    fn message_reference_literal(target: MessageRef) -> String {
        format!("{REF_PREFIX}{}", target.encode())
    }

    fn quote_message_reference(
        &mut self,
        target: MessageRef,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let reference = Self::message_reference_literal(target);
        self.composer.update(cx, |composer, cx| {
            composer.insert_message_reference(&reference, window, cx)
        });
        window.focus(&self.composer.focus_handle(cx), cx);
        self.status = "Reference inserted".into();
        cx.notify();
    }

    fn copy_message_reference(&mut self, target: MessageRef, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(Self::message_reference_literal(
            target,
        )));
        self.status = "Message reference copied".into();
        cx.notify();
    }

    fn hover_message_reference(
        &mut self,
        hovered: Option<(MessageRef, Bounds<Pixels>)>,
        cx: &mut Context<Self>,
    ) {
        self.message_reference_hover_task = None;
        let Some((target, anchor)) = hovered else {
            self.message_reference_hover = None;
            cx.notify();
            return;
        };
        if let Some(state) = self.message_reference_hover.as_mut()
            && state.target == target
        {
            state.anchor = anchor;
            return;
        }
        self.message_reference_hover = Some(MessageReferenceHover {
            target,
            anchor,
            visible: false,
            message: None,
            formatted: None,
            missing: false,
            request_id: None,
        });
        let executor = cx.background_executor().clone();
        self.message_reference_hover_task = Some(cx.spawn(async move |this, cx| {
            executor.timer(REFERENCE_HOVER_DELAY).await;
            let _ = this.update(cx, |this, cx| {
                if this
                    .message_reference_hover
                    .as_ref()
                    .is_some_and(|hover| hover.target == target)
                {
                    this.begin_message_reference_preview(target, cx);
                }
            });
        }));
    }

    fn begin_message_reference_preview(&mut self, target: MessageRef, cx: &mut Context<Self>) {
        let loaded = (self.model.selected_room == Some(target.room_id))
            .then(|| {
                self.model
                    .messages
                    .binary_search_by_key(&target.message_id.0, |message| message.id)
                    .ok()
                    .map(|index| self.model.messages[index].clone())
            })
            .flatten();
        if let Some(message) = loaded {
            self.install_message_reference_preview(target, Some(message), None, cx);
            return;
        }
        if let Some(cached) = self.message_reference_cache.get(&target).cloned() {
            self.install_message_reference_preview(target, cached, None, cx);
            return;
        }
        let request_id = self.request_id();
        let Some(hover) = self
            .message_reference_hover
            .as_mut()
            .filter(|hover| hover.target == target)
        else {
            return;
        };
        hover.visible = true;
        hover.request_id = Some(request_id);
        if let Err(error) = self.daemon.send(ClientFrame::ResolveMessageReference {
            request_id,
            room_id: target.room_id,
            message_id: target.message_id,
        }) {
            hover.request_id = None;
            hover.missing = true;
            log::error!("could not resolve message reference: {error}");
        }
        cx.notify();
    }

    fn install_message_reference_preview(
        &mut self,
        target: MessageRef,
        message: Option<timeline::Message>,
        request_id: Option<RequestId>,
        cx: &mut Context<Self>,
    ) {
        let Some(hover) = self
            .message_reference_hover
            .as_mut()
            .filter(|hover| hover.target == target)
        else {
            return;
        };
        if request_id.is_some() && hover.request_id != request_id {
            return;
        }
        let formatted = message.as_ref().map(|message| {
            Rc::new(FormattedMessage::from_prepared(FormattedMessage::prepare(
                message.body.clone(),
            )))
        });
        hover.visible = true;
        hover.message = message;
        hover.formatted = formatted;
        hover.missing = hover.message.is_none();
        hover.request_id = None;
        cx.notify();
    }

    fn activate_message_reference(
        &mut self,
        target: MessageRef,
        shift: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let hover_request = self
            .message_reference_hover
            .as_ref()
            .filter(|hover| hover.target == target)
            .and_then(|hover| hover.request_id);
        self.message_reference_hover = None;
        self.message_reference_hover_task = None;
        self.pending_message_reference_click = None;
        self.pending_reference_media_preview = None;
        if shift {
            self.jump_to_message_reference(target, cx);
            return;
        }

        let loaded = (self.model.selected_room == Some(target.room_id))
            .then(|| {
                self.model
                    .messages
                    .binary_search_by_key(&target.message_id.0, |message| message.id)
                    .ok()
                    .map(|index| self.model.messages[index].clone())
            })
            .flatten();
        if let Some(message) = loaded {
            self.open_message_reference_target(target, message, window, cx);
            return;
        }
        if let Some(cached) = self.message_reference_cache.get(&target).cloned() {
            if let Some(message) = cached {
                self.open_message_reference_target(target, message, window, cx);
            } else {
                self.jump_to_message_reference(target, cx);
            }
            return;
        }

        let request_id = hover_request.unwrap_or_else(|| self.request_id());
        self.pending_message_reference_click =
            Some(PendingMessageReferenceClick { target, request_id });
        if hover_request.is_none()
            && let Err(error) = self.daemon.send(ClientFrame::ResolveMessageReference {
                request_id,
                room_id: target.room_id,
                message_id: target.message_id,
            })
        {
            self.pending_message_reference_click = None;
            self.status = error.into();
        } else {
            self.status = "Opening referenced attachment…".into();
        }
        cx.notify();
    }

    fn open_message_reference_target(
        &mut self,
        target: MessageRef,
        message: timeline::Message,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(attachment) = message.attachment else {
            self.jump_to_message_reference(target, cx);
            return;
        };
        self.open_message_reference_attachment(target, attachment, window, cx);
    }

    fn open_message_reference_attachment(
        &mut self,
        target: MessageRef,
        attachment: Attachment,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let descriptor = attachment.descriptor.clone();
        let render_kind = attachment.render_kind();
        if render_kind == AttachmentRenderKind::Image {
            let cached = self
                .media_cache
                .lock()
                .expect("media cache lock poisoned")
                .contains(descriptor.id);
            if cached {
                self.pending_reference_media_preview = None;
                self.open_image_preview(descriptor, window, cx);
            } else {
                self.pending_reference_media_preview = Some(PendingReferenceMediaPreview {
                    target,
                    attachment: attachment.clone(),
                });
                let fetch = EagerImageFetch::new(target.room_id, descriptor.clone());
                if self.eager_image_fetches.failure(fetch.key).is_some() {
                    self.retry_eager_image(fetch, window, cx);
                } else {
                    self.enqueue_eager_image(fetch, window, cx);
                }
                self.status = format!("Loading {}…", descriptor.file_name).into();
                cx.notify();
            }
            return;
        }
        if render_kind == AttachmentRenderKind::Audio {
            self.pending_reference_media_preview = None;
            self.jump_to_message_reference(target, cx);
            return;
        }
        if descriptor.media_kind == MediaKind::File && render_kind == AttachmentRenderKind::Other {
            self.pending_reference_media_preview = None;
            self.open_code_preview(target.room_id, descriptor, window, cx);
            return;
        }
        if render_kind == AttachmentRenderKind::Video {
            self.pending_reference_media_preview = None;
            let key = video_key(target.room_id, target.message_id.0, &descriptor);
            let source_key = self.source_key(target.room_id, descriptor.id);
            let source = match self.video_sources.view(source_key) {
                VideoSourceView::Ready(source) => Some(source),
                VideoSourceView::Absent
                | VideoSourceView::Loading
                | VideoSourceView::Failed { .. } => None,
            };
            self.video_sources.promote(source_key, descriptor.clone());
            self.toggle_video_theater(
                TheaterVideo {
                    key,
                    descriptor,
                    source,
                },
                cx,
            );
            self.pump_video_sources(cx);
            return;
        }

        self.pending_reference_media_preview = None;
        self.status = format!("{} cannot be previewed yet", descriptor.file_name).into();
        cx.notify();
    }

    fn jump_to_message_reference(&mut self, target: MessageRef, cx: &mut Context<Self>) {
        self.message_reference_hover = None;
        self.message_reference_hover_task = None;
        if !self.model.is_ready()
            || !self
                .model
                .rooms
                .iter()
                .any(|room| room.id == target.room_id)
        {
            self.status = "Referenced room is not available".into();
            cx.notify();
            return;
        }
        self.pending_message_jump = Some(PendingMessageJump {
            target,
            pages_requested: 0,
            page_request_id: None,
            room_request_id: None,
        });
        if self.model.selected_room != Some(target.room_id) {
            let request_id = self.request_id();
            self.model.pending.insert(
                request_id,
                PendingRequest {
                    operation: Operation::SelectRoom,
                    room_id: Some(target.room_id),
                    draft: None,
                    transfer_id: None,
                },
            );
            if let Some(jump) = self.pending_message_jump.as_mut() {
                jump.room_request_id = Some(request_id);
            }
            if let Err(error) = self.daemon.send(ClientFrame::SelectRoom {
                request_id,
                room_id: target.room_id,
            }) {
                self.model.pending.remove(&request_id);
                self.pending_message_jump = None;
                self.status = error.into();
            } else {
                self.status = "Opening referenced room…".into();
            }
            cx.notify();
            return;
        }
        self.resume_message_reference_jump(cx);
    }

    fn flash_message_reference(&mut self, target: MessageRef, cx: &mut Context<Self>) {
        let flash_id = self.next_message_reference_flash_id;
        self.next_message_reference_flash_id =
            self.next_message_reference_flash_id.wrapping_add(1).max(1);
        self.message_reference_flash = Some((target, flash_id));
        self.message_reference_flash_task = None;
        let executor = cx.background_executor().clone();
        self.message_reference_flash_task = Some(cx.spawn(async move |this, cx| {
            executor.timer(Duration::from_millis(1_600)).await;
            let _ = this.update(cx, |this, cx| {
                if this
                    .message_reference_flash
                    .is_some_and(|(_, active_id)| active_id == flash_id)
                {
                    this.message_reference_flash = None;
                    this.message_reference_flash_task = None;
                    cx.notify();
                }
            });
        }));
    }

    fn resume_message_reference_jump(&mut self, cx: &mut Context<Self>) {
        let Some(target) = self.pending_message_jump.as_ref().map(|jump| jump.target) else {
            return;
        };
        if self.model.selected_room != Some(target.room_id) {
            return;
        }
        if self
            .pending_message_jump
            .as_ref()
            .is_some_and(|jump| jump.page_request_id.is_some())
        {
            return;
        }
        let pages_requested = self
            .pending_message_jump
            .as_ref()
            .map_or(0, |jump| jump.pages_requested);
        let before = match message_reference_jump_decision(
            &self.model.messages,
            target.message_id,
            self.model.older_cursor,
            self.model.at_start,
            pages_requested,
        ) {
            MessageReferenceJumpDecision::Found => {
                if timeline::reveal_message(
                    &self.model.messages,
                    &mut self.collapsed_sections,
                    target.message_id.0,
                ) {
                    self.rebuild_message_list();
                }
                if let Some(index) = self
                    .message_list
                    .iter()
                    .position(|item| item.message_id() == Some(target.message_id.0))
                {
                    self.list_state.scroll_to_reveal_item(index);
                    self.flash_message_reference(target, cx);
                    self.pending_message_jump = None;
                    self.status = "Jumped to referenced message".into();
                    cx.notify();
                }
                return;
            }
            MessageReferenceJumpDecision::Unavailable => {
                self.pending_message_jump = None;
                self.status = "Referenced message is not available".into();
                cx.notify();
                return;
            }
            MessageReferenceJumpDecision::SearchWindowExhausted => {
                self.pending_message_jump = None;
                self.status = "Referenced message is outside the current jump search window".into();
                cx.notify();
                return;
            }
            MessageReferenceJumpDecision::LoadOlder(before) => before,
        };

        let request_id = self.request_id();
        self.model.pending.insert(
            request_id,
            PendingRequest {
                operation: Operation::LoadOlder,
                room_id: Some(target.room_id),
                draft: None,
                transfer_id: None,
            },
        );
        let jump = self
            .pending_message_jump
            .as_mut()
            .expect("reference jump remains pending");
        jump.pages_requested += 1;
        jump.page_request_id = Some(request_id);
        if let Err(error) = self.daemon.send(ClientFrame::LoadOlder {
            request_id,
            room_id: target.room_id,
            before: Some(before),
            limit: REFERENCE_JUMP_PAGE_SIZE,
        }) {
            self.model.pending.remove(&request_id);
            self.pending_message_jump = None;
            self.status = error.into();
        } else {
            self.status = format!(
                "Finding referenced message… ({}/{REFERENCE_JUMP_PAGE_LIMIT})",
                jump.pages_requested
            )
            .into();
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
        if self.editing.is_some() {
            self.set_composer_error("Finish editing before attaching files", cx);
            return;
        }
        if self.pending_submission.is_some() {
            self.set_composer_error("Wait for the current submission to finish", cx);
            return;
        }
        if self.file_inspection_pending {
            self.set_composer_error("Wait for attached files to finish checking", cx);
            return;
        }

        let chooser = cx.background_executor().spawn(async {
            open_files(OpenFileOptions {
                title: "Attach files to a Chatt message".into(),
                accept_label: Some("Attach".into()),
                multiple: true,
                directory: false,
                current_folder: None,
                parent_window: String::new(),
            })
        });
        cx.spawn_in(window, async move |this, cx| {
            let response = chooser.await;
            let _ = this.update_in(cx, |this, _, cx| match response {
                Ok(FileChooserResponse::Selected(paths)) => this.queue_files(paths, cx),
                Ok(FileChooserResponse::Cancelled) => {}
                Ok(FileChooserResponse::Other) => {
                    this.status = "File chooser closed without a selection".into();
                    cx.notify();
                }
                Err(error) => {
                    this.status = format!("Could not open file chooser · {error}").into();
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn queue_files(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        if self.editing.is_some() {
            self.set_composer_error("Finish editing before attaching files", cx);
            return;
        }
        if self.pending_submission.is_some() {
            self.set_composer_error("Wait for the current submission to finish", cx);
            return;
        }
        if self.file_inspection_pending {
            self.set_composer_error("Wait for attached files to finish checking", cx);
            return;
        }
        if paths.is_empty() {
            return;
        }
        let available_slots = MAX_QUEUED_FILES.saturating_sub(self.queued_files.len());
        if available_slots == 0 {
            self.set_composer_error(
                format!("At most {MAX_QUEUED_FILES} files can be queued"),
                cx,
            );
            return;
        }
        let max_upload_bytes = self.model.limits.upload_bytes;
        let executor = cx.background_executor().clone();
        self.file_inspection_pending = true;
        self.status = "Checking attached files…".into();
        cx.spawn(async move |this, cx| {
            let result = executor
                .spawn(async move { inspect_files(paths, max_upload_bytes, available_slots) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.file_inspection_pending = false;
                this.accept_file_inspection(result, cx);
            });
        })
        .detach();
        cx.notify();
    }

    fn queue_clipboard_images(&mut self, images: Arc<[PastedImage]>, cx: &mut Context<Self>) {
        if self.editing.is_some() {
            self.set_composer_error("Finish editing before attaching files", cx);
            return;
        }
        if self.pending_submission.is_some() {
            self.set_composer_error("Wait for the current submission to finish", cx);
            return;
        }
        if self.file_inspection_pending {
            self.set_composer_error("Wait for attached files to finish checking", cx);
            return;
        }
        if images.is_empty() {
            return;
        }
        let available_slots = MAX_QUEUED_FILES.saturating_sub(self.queued_files.len());
        if available_slots == 0 {
            self.set_composer_error(
                format!("At most {MAX_QUEUED_FILES} files can be queued"),
                cx,
            );
            return;
        }
        let timestamp = chrono::Utc::now().format("%Y-%m-%dT%H-%M-%SZ").to_string();
        let result = prepare_images(
            images,
            self.model.limits.upload_bytes,
            available_slots,
            &timestamp,
        );
        log::info!(
            "clipboard image paste prepared accepted={} rejected={}",
            result.accepted.len(),
            result.rejected.len(),
        );
        self.accept_file_inspection(result, cx);
    }

    fn accept_file_inspection(&mut self, result: FileInspection, cx: &mut Context<Self>) {
        let accepted = result.accepted.len();
        self.queued_files.extend(result.accepted);
        let first_error = result.rejected.first().cloned();
        for error in result.rejected {
            log::error!("file was not queued: {error}");
        }
        self.composer_error = first_error.clone().map(Into::into);
        self.status = match (accepted, first_error) {
            (0, Some(error)) => error.into(),
            (accepted, Some(error)) => format!("{accepted} queued · {error}").into(),
            (_, None) => {
                let count = self.queued_files.len();
                queued_files_status(
                    count,
                    self.model.is_ready(),
                    self.model.selected_room.is_some(),
                )
                .into()
            }
        };
        cx.notify();
    }

    fn remove_queued_file(&mut self, id: u64, cx: &mut Context<Self>) {
        if self.pending_submission.is_some() || !self.queued_files.remove(id) {
            return;
        }
        self.status = if self.queued_files.is_empty() {
            connection_label(&self.model).into()
        } else {
            let count = self.queued_files.len();
            queued_files_status(
                count,
                self.model.is_ready(),
                self.model.selected_room.is_some(),
            )
            .into()
        };
        cx.notify();
    }

    fn toggle_mute(&mut self, _: &ToggleMute, _: &mut Window, cx: &mut Context<Self>) {
        if !self.model.is_ready() {
            return;
        }
        let state = self.model.voice.state.toggle_mute();
        self.request_voice_state(state, cx);
    }

    fn toggle_deafen(&mut self, _: &ToggleDeafen, _: &mut Window, cx: &mut Context<Self>) {
        if !self.model.is_ready() {
            return;
        }
        let state = self.model.voice.state.toggle_deafen();
        self.request_voice_state(state, cx);
    }

    fn request_voice_state(&mut self, state: VoiceState, cx: &mut Context<Self>) {
        let request_id = self.request_id();
        self.model.pending.insert(
            request_id,
            PendingRequest {
                operation: Operation::SetVoiceState,
                room_id: self.model.selected_room,
                draft: None,
                transfer_id: None,
            },
        );
        if let Err(error) = self
            .daemon
            .send(ClientFrame::SetVoiceState { request_id, state })
        {
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
            .clamp(0., local_rpc::MAX_OUTPUT_VOLUME_PERCENT);
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
        transfer_id: local_rpc::ids::FileTransferId,
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

    fn begin_preview_session(&mut self, window: &Window, cx: &App) {
        if self.preview_history.active().is_none() {
            self.preview_return_focus = window.focused(cx).map(|focus| focus.downgrade());
            let body_width = self.chat_body_width(window);
            self.preview_chat_width = default_chat_width(body_width, window.rem_size());
        }
    }

    fn chat_body_width(&self, window: &Window) -> Pixels {
        window.viewport_size().width
            - if self.show_rooms_sidebar {
                crate::ui_scale::scaled_px(SIDEBAR_WIDTH, window.rem_size())
            } else {
                px(0.)
            }
    }

    fn restore_preview_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(focus) = self
            .preview_return_focus
            .take()
            .and_then(|focus| focus.upgrade())
        {
            window.focus(&focus, cx);
        } else {
            window.blur();
        }
    }

    fn reveal_active_preview_tab(&self) {
        let Some(active_key) = self.preview_history.active_key() else {
            return;
        };
        if let Some(index) = self
            .preview_history
            .items()
            .iter()
            .position(|item| item.key() == active_key)
        {
            self.preview_tabs_scroll.scroll_to_item(index);
        }
    }

    fn cancel_code_load(&mut self, key: AttachmentId) {
        self.code_load_tasks.remove(&key);
    }

    fn apply_preview_eviction(&mut self, evicted: Option<AttachmentId>) {
        if let Some(evicted) = evicted {
            self.cancel_code_load(evicted);
        }
    }

    fn open_image_preview(
        &mut self,
        descriptor: AttachmentDescriptor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(attachment) = self
            .media_cache
            .lock()
            .expect("media cache lock poisoned")
            .get(descriptor.id)
        else {
            self.status = format!("{} is not cached yet", descriptor.file_name).into();
            cx.notify();
            return;
        };
        let natural_size = match (descriptor.width, descriptor.height) {
            (Some(width), Some(height)) if width > 0 && height > 0 => (width, height),
            _ => image_dimensions_from_bytes(attachment.bytes()).unwrap_or((640, 480)),
        };
        self.begin_preview_session(window, cx);
        let item = PreviewItem::image(descriptor, natural_size);
        let opened = self.preview_history.open(item);
        self.apply_preview_eviction(opened.evicted);
        if opened.active_changed {
            self.code_selection.clear();
        }
        self.reveal_active_preview_tab();
        self.close_code_search(cx);
        window.focus(&self.code_viewer_focus, cx);
        if opened.active_changed {
            self.preview_image.reset(
                self.preview_history
                    .active()
                    .and_then(PreviewItem::image_size)
                    .expect("new image preview has image content"),
            );
            self.preview_image_viewport.set(None);
            self.preview_last_mouse_position = None;
        }
        cx.notify();
    }

    fn open_code_preview(
        &mut self,
        room_id: RoomId,
        descriptor: AttachmentDescriptor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (cached_attachment, active_transfer) = {
            let mut cache = self.media_cache.lock().expect("media cache lock poisoned");
            (cache.get(descriptor.id), cache.active_transfer(&descriptor))
        };
        self.begin_preview_session(window, cx);
        let opened = self
            .preview_history
            .open(PreviewItem::code(descriptor.clone(), active_transfer));
        self.apply_preview_eviction(opened.evicted);
        if opened.active_changed {
            self.code_selection.clear();
        }
        self.reveal_active_preview_tab();
        self.close_code_search(cx);
        self.preview_image_viewport.set(None);
        self.preview_last_mouse_position = None;
        window.focus(&self.code_viewer_focus, cx);

        if let Some(reason) = code_preview_size_error(descriptor.byte_len) {
            self.cancel_code_load(descriptor.id);
            if let Some(preview) = self
                .preview_history
                .item_mut(descriptor.id)
                .and_then(PreviewItem::code_preview_mut)
            {
                preview.state = CodePreviewState::Error(reason.into());
            }
            cx.notify();
            return;
        }

        let ready_or_preparing = self
            .preview_history
            .item(descriptor.id)
            .and_then(PreviewItem::code_preview)
            .is_some_and(|preview| {
                matches!(
                    preview.state,
                    CodePreviewState::Ready(_) | CodePreviewState::Preparing { .. }
                )
            });
        if ready_or_preparing {
            cx.notify();
            return;
        }

        if let Some(attachment) = cached_attachment {
            self.start_code_preview_load(descriptor.id, attachment, cx);
        } else if let Some(transfer_id) = active_transfer {
            if let Some(preview) = self
                .preview_history
                .item_mut(descriptor.id)
                .and_then(PreviewItem::code_preview_mut)
            {
                preview.state = CodePreviewState::Fetching {
                    transfer_id: Some(transfer_id),
                };
            }
        } else {
            match self.begin_attachment_read(room_id, descriptor.clone(), cx) {
                Ok(Some(transfer_id)) => {
                    if let Some(preview) = self
                        .preview_history
                        .item_mut(descriptor.id)
                        .and_then(PreviewItem::code_preview_mut)
                    {
                        preview.state = CodePreviewState::Fetching {
                            transfer_id: Some(transfer_id),
                        };
                    }
                }
                Ok(None) => {
                    let attachment = self
                        .media_cache
                        .lock()
                        .expect("media cache lock poisoned")
                        .get(descriptor.id);
                    if let Some(attachment) = attachment {
                        self.start_code_preview_load(descriptor.id, attachment, cx);
                    } else if let Some(preview) = self
                        .preview_history
                        .item_mut(descriptor.id)
                        .and_then(PreviewItem::code_preview_mut)
                    {
                        preview.state = CodePreviewState::Error("attachment was not cached".into());
                    }
                }
                Err(reason) => {
                    if let Some(preview) = self
                        .preview_history
                        .item_mut(descriptor.id)
                        .and_then(PreviewItem::code_preview_mut)
                    {
                        preview.state = CodePreviewState::Error(reason.clone());
                    }
                    self.status = reason.into();
                }
            }
        }
        cx.notify();
    }

    fn start_code_preview_load(
        &mut self,
        key: AttachmentId,
        attachment: CachedAttachment,
        cx: &mut Context<Self>,
    ) {
        let Some(item) = self.preview_history.item(key) else {
            return;
        };
        let file_name = item.descriptor.file_name.clone();
        self.cancel_code_load(key);
        let load_id = self.next_code_load_id;
        self.next_code_load_id = self.next_code_load_id.wrapping_add(1).max(1);
        let Some(preview) = self
            .preview_history
            .item_mut(key)
            .and_then(PreviewItem::code_preview_mut)
        else {
            return;
        };
        preview.view_state.reset();
        preview.scrollbar_state.reset();
        preview.state = CodePreviewState::Preparing { load_id };
        if self.preview_history.active_key() == Some(key) {
            self.code_selection.clear();
        }

        let executor = cx.background_executor().clone();
        let task = cx.spawn(async move |this, cx| {
            let result = executor
                .spawn(async move { CodeDocument::load(attachment.bytes(), &file_name, load_id) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if !this
                    .code_load_tasks
                    .get(&key)
                    .is_some_and(|(active_load, _)| *active_load == load_id)
                {
                    return;
                }
                this.code_load_tasks.remove(&key);
                let Some(preview) = this
                    .preview_history
                    .item_mut(key)
                    .and_then(PreviewItem::code_preview_mut)
                else {
                    return;
                };
                if !matches!(
                    preview.state,
                    CodePreviewState::Preparing {
                        load_id: active_load
                    } if active_load == load_id
                ) {
                    return;
                }
                preview.state = match result {
                    Ok(document) => CodePreviewState::Ready(document),
                    Err(reason) => CodePreviewState::Error(reason),
                };
                cx.notify();
            });
        });
        self.code_load_tasks.insert(key, (load_id, task));
    }

    fn resume_code_preview_load(
        &mut self,
        descriptor: &AttachmentDescriptor,
        cx: &mut Context<Self>,
    ) {
        let should_load = self
            .preview_history
            .item(descriptor.id)
            .and_then(PreviewItem::code_preview)
            .is_some_and(|preview| {
                matches!(
                    preview.state,
                    CodePreviewState::Fetching { .. } | CodePreviewState::Error(_)
                )
            });
        if !should_load {
            return;
        }
        let attachment = self
            .media_cache
            .lock()
            .expect("media cache lock poisoned")
            .get(descriptor.id);
        if let Some(attachment) = attachment {
            self.start_code_preview_load(descriptor.id, attachment, cx);
        }
    }

    fn active_code_document(
        &self,
    ) -> Option<(Arc<CodeDocument>, UniformListScrollHandle, CodeViewState)> {
        let preview = self.preview_history.active()?.code_preview()?;
        let CodePreviewState::Ready(document) = &preview.state else {
            return None;
        };
        Some((
            document.clone(),
            preview.scroll_handle.clone(),
            preview.view_state.clone(),
        ))
    }

    fn update_code_search(&mut self, cx: &mut Context<Self>) {
        if !self.code_search_open {
            return;
        }
        let Some((document, _, _)) = self.active_code_document() else {
            self.close_code_search(cx);
            return;
        };
        let query = {
            let input = self.code_search_input.read(cx);
            input.text()
        };
        self.code_search_generation = self.code_search_generation.wrapping_add(1);
        let generation = self.code_search_generation;
        self.code_search_task.take();
        self.code_search_results = CodeSearchResults::default();
        self.code_search_result_index = 0;
        self.code_search_pending = !query.is_empty();
        if query.is_empty() {
            cx.notify();
            return;
        }

        let executor = cx.background_executor().clone();
        self.code_search_task = Some(cx.spawn(async move |this, cx| {
            executor.timer(Duration::from_millis(75)).await;
            let results = executor.spawn(async move { document.search(&query) }).await;
            let _ = this.update(cx, |this, cx| {
                if !this.code_search_open || this.code_search_generation != generation {
                    return;
                }
                this.code_search_task.take();
                this.code_search_pending = false;
                this.code_search_results = results;
                this.code_search_result_index = 0;
                this.scroll_to_code_match(ScrollStrategy::Center);
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn scroll_to_code_match(&self, strategy: ScrollStrategy) {
        let Some(search_match) = self.code_search_results.get(self.code_search_result_index) else {
            return;
        };
        if let Some((document, scroll_handle, view_state)) = self.active_code_document() {
            let target = document.match_target(search_match);
            view_state.request_match_reveal(search_match);
            scroll_handle.scroll_to_item_strict(target.line, strategy);
        }
    }

    fn open_code_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.active_code_document().is_none() {
            return;
        }
        self.code_search_open = true;
        self.update_code_search(cx);
        window.focus(&self.code_search_input.focus_handle(cx), cx);
        cx.notify();
    }

    fn next_code_match(&mut self, cx: &mut Context<Self>) {
        if self.code_search_results.is_empty() {
            return;
        }
        self.code_search_result_index =
            (self.code_search_result_index + 1) % self.code_search_results.len();
        self.scroll_to_code_match(ScrollStrategy::Center);
        cx.notify();
    }

    fn previous_code_match(&mut self, cx: &mut Context<Self>) {
        if self.code_search_results.is_empty() {
            return;
        }
        self.code_search_result_index = self
            .code_search_result_index
            .checked_sub(1)
            .unwrap_or(self.code_search_results.len() - 1);
        self.scroll_to_code_match(ScrollStrategy::Center);
        cx.notify();
    }

    fn close_code_search(&mut self, cx: &mut Context<Self>) {
        if !self.code_search_open
            && !self.code_search_pending
            && self.code_search_results.is_empty()
        {
            return;
        }
        self.code_search_generation = self.code_search_generation.wrapping_add(1);
        self.code_search_task.take();
        self.code_search_open = false;
        self.code_search_pending = false;
        self.code_search_results = CodeSearchResults::default();
        self.code_search_result_index = 0;
        cx.notify();
    }

    fn find_in_code_action(&mut self, _: &FindInCode, window: &mut Window, cx: &mut Context<Self>) {
        cx.stop_propagation();
        self.open_code_search(window, cx);
    }

    fn next_code_match_action(
        &mut self,
        _: &NextCodeMatch,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        self.next_code_match(cx);
    }

    fn previous_code_match_action(
        &mut self,
        _: &PreviousCodeMatch,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        self.previous_code_match(cx);
    }

    fn close_code_search_action(
        &mut self,
        _: &CloseCodeSearch,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        self.close_code_search(cx);
        window.focus(&self.code_viewer_focus, cx);
    }

    fn select_preview(&mut self, key: AttachmentId, window: &mut Window, cx: &mut Context<Self>) {
        if self.preview_history.select(key) {
            self.code_selection.clear();
            self.close_code_search(cx);
            if let Some(natural_size) = self
                .preview_history
                .active()
                .and_then(PreviewItem::image_size)
            {
                self.preview_image.reset(natural_size);
            }
            window.focus(&self.code_viewer_focus, cx);
            self.reveal_active_preview_tab();
            self.preview_image_viewport.set(None);
            self.preview_last_mouse_position = None;
            cx.notify();
        }
    }

    fn close_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.preview_history.active().is_none() {
            return;
        }
        self.preview_history.close_panel();
        self.code_selection.clear();
        self.close_code_search(cx);
        self.preview_image_viewport.set(None);
        self.preview_last_mouse_position = None;
        self.preview_pane_resize = None;
        self.restore_preview_focus(window, cx);
        cx.notify();
    }

    fn close_preview_action(
        &mut self,
        _: &ClosePreview,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.exit_video_theater(cx) {
            return;
        }
        self.close_preview(window, cx);
    }

    fn close_preview_tab(
        &mut self,
        key: AttachmentId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cancel_code_load(key);
        if self.preview_history.close_tab(key) {
            self.code_selection.clear();
            self.close_code_search(cx);
            if let Some(natural_size) = self
                .preview_history
                .active()
                .and_then(PreviewItem::image_size)
            {
                self.preview_image.reset(natural_size);
            }
            if self.preview_history.active().is_some() {
                window.focus(&self.code_viewer_focus, cx);
                self.reveal_active_preview_tab();
            } else {
                self.restore_preview_focus(window, cx);
            }
            self.preview_image_viewport.set(None);
            self.preview_last_mouse_position = None;
        }
        cx.notify();
    }

    fn save_preview_attachment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = self.preview_history.active().cloned() else {
            return;
        };
        let Some(attachment) = self
            .media_cache
            .lock()
            .expect("media cache lock poisoned")
            .get(item.descriptor.id)
        else {
            self.status = format!("{} is no longer cached", item.descriptor.file_name).into();
            cx.notify();
            return;
        };
        let directory = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let receiver = cx.prompt_for_new_path(&directory, Some(&item.descriptor.file_name));
        let executor = cx.background_executor().clone();
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(destination)) = receiver.await else {
                return;
            };
            let result = executor
                .spawn(async move {
                    write_cached_attachment_to_user_selected_path(&attachment, destination)
                })
                .await;
            let destination = match result {
                Ok(Some(destination)) => destination,
                Ok(None) => return,
                Err(error) => {
                    let _ = this.update_in(cx, |this, _, cx| {
                        this.status = format!("Could not save file · {error}").into();
                        cx.notify();
                    });
                    return;
                }
            };
            let _ = this.update_in(cx, |this, _, cx| {
                this.status = format!("Saved to {}", destination.display()).into();
                cx.notify();
            });
        })
        .detach();
    }

    fn copy_preview_code(&mut self, cx: &mut Context<Self>) {
        let Some(document) = self
            .preview_history
            .active()
            .and_then(PreviewItem::code_preview)
            .and_then(|preview| match &preview.state {
                CodePreviewState::Ready(document) => Some(document),
                _ => None,
            })
        else {
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(document.source().to_string()));
        self.status = "Copied file".into();
        cx.notify();
    }

    fn preview_viewport(&self) -> Option<Bounds<Pixels>> {
        self.preview_image_viewport.get()
    }

    fn fit_preview_image(&mut self, cx: &mut Context<Self>) {
        if let Some(viewport) = self.preview_viewport() {
            self.preview_image.fit(viewport);
            cx.notify();
        }
    }

    fn actual_size_preview_image(&mut self, cx: &mut Context<Self>) {
        self.preview_image.actual_size();
        cx.notify();
    }

    fn zoom_preview_image(&mut self, delta: f32, cx: &mut Context<Self>) {
        if let Some(viewport) = self.preview_viewport() {
            self.preview_image.zoom_from_center(delta, viewport);
            cx.notify();
        }
    }

    fn scroll_preview_image(&mut self, event: &ScrollWheelEvent, cx: &mut Context<Self>) {
        let Some(viewport) = self.preview_viewport() else {
            return;
        };
        let delta = match event.delta {
            ScrollDelta::Pixels(delta) => f32::from(delta.y),
            ScrollDelta::Lines(delta) => delta.y * 16.0,
        };
        if delta == 0.0 {
            return;
        }
        self.preview_image
            .zoom_by_factor((delta * 0.002).exp(), viewport, event.position);
        cx.stop_propagation();
        cx.notify();
    }

    fn pinch_preview_image(&mut self, event: &PinchEvent, cx: &mut Context<Self>) {
        let Some(viewport) = self.preview_viewport() else {
            return;
        };
        self.preview_image
            .zoom_by_factor((1.0 + event.delta).max(0.01), viewport, event.position);
        cx.stop_propagation();
        cx.notify();
    }

    fn preview_image_mouse_down(&mut self, event: &MouseDownEvent, cx: &mut Context<Self>) {
        if event.button != MouseButton::Left {
            return;
        }
        self.preview_last_mouse_position = Some(event.position);
        cx.stop_propagation();
    }

    fn preview_image_mouse_move(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        let Some(previous) = self.preview_last_mouse_position else {
            return;
        };
        if !event.dragging() {
            self.finish_preview_image_pan(cx);
            return;
        }
        if let Some(viewport) = self.preview_viewport() {
            self.preview_image
                .pan_by(event.position - previous, viewport);
            self.preview_last_mouse_position = Some(event.position);
            cx.stop_propagation();
            cx.notify();
        }
    }

    fn finish_preview_image_pan(&mut self, cx: &mut Context<Self>) {
        if self.preview_last_mouse_position.take().is_some() {
            cx.notify();
        }
    }

    fn begin_preview_pane_resize(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let body_width = self.chat_body_width(window);
        self.preview_chat_width =
            clamp_chat_width(self.preview_chat_width, body_width, window.rem_size());
        self.preview_pane_resize = Some(PreviewPaneResize {
            start_x: event.position.x,
            start_chat_width: self.preview_chat_width,
        });
        cx.stop_propagation();
        cx.notify();
    }

    fn drag_preview_pane(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(resize) = self.preview_pane_resize else {
            return;
        };
        if !event.dragging() {
            self.finish_preview_pane_resize(cx);
            return;
        }
        let body_width = self.chat_body_width(window);
        self.preview_chat_width = clamp_chat_width(
            resize.start_chat_width + event.position.x - resize.start_x,
            body_width,
            window.rem_size(),
        );
        cx.stop_propagation();
        cx.notify();
    }

    fn finish_preview_pane_resize(&mut self, cx: &mut Context<Self>) {
        if self.preview_pane_resize.take().is_some() {
            cx.notify();
        }
    }

    fn render_command_row(
        &self,
        command_index: usize,
        local_id: u64,
        trailing_gap: bool,
        cx: &App,
    ) -> AnyElement {
        let Some(row) = self.command_rows.get(command_index) else {
            return div().into_any_element();
        };
        let settings = AppliedSettings::get(cx);
        let accent = settings.theme.color(if row.error {
            ThemeRole::StateDanger
        } else {
            ThemeRole::MediaProgressFill
        });
        let body_color = settings.theme.color(if row.error {
            ThemeRole::StateDanger
        } else {
            ThemeRole::TextBody
        });
        let row_hover = settings.theme.color(ThemeRole::StateRowHover);
        let timestamp = timeline::format_age(row.timestamp_ms, timeline::now_ms());
        let formatted = self
            .formatted_command_messages
            .get(&local_id)
            .cloned()
            .unwrap_or_else(|| Rc::new(FormattedMessage::plain(row.body.clone())));
        let command_row = div()
            .id(("command-output", local_id as usize))
            .relative()
            .w_full()
            .pl(rems_from_px(64.))
            .pr(rems_from_px(28.))
            .py(rems_from_px(9.))
            .bg(settings.theme.color(ThemeRole::Raised))
            .hover(move |row| row.bg(row_hover))
            .child(
                div()
                    .absolute()
                    .left(rems_from_px(64.))
                    .top_0()
                    .bottom_0()
                    .w(rems_from_px(3.))
                    .bg(accent),
            )
            .child(
                div()
                    .absolute()
                    .left_0()
                    .top(rems_from_px(9.))
                    .h(rems_from_px(TIMELINE_ROW_HEADER_HEIGHT))
                    .w(rems_from_px(64.))
                    .pr(rems_from_px(8.))
                    .flex()
                    .items_center()
                    .justify_end()
                    .text_xs()
                    .text_color(settings.theme.color(ThemeRole::TextDim))
                    .child(timestamp),
            )
            .child(
                div()
                    .w_full()
                    .max_w(rems_from_px(860.))
                    .min_w_0()
                    .pl(rems_from_px(15.))
                    .child(
                        div()
                            .min_h(rems_from_px(TIMELINE_ROW_HEADER_HEIGHT))
                            .line_height(rems_from_px(TIMELINE_ROW_HEADER_HEIGHT))
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(accent)
                                    .child("chatt"),
                            )
                            .child(
                                div()
                                    .px_1()
                                    .border_1()
                                    .border_color(settings.theme.color(ThemeRole::BorderStrong))
                                    .text_xs()
                                    .text_color(settings.theme.color(ThemeRole::TextMuted))
                                    .child(if row.error {
                                        "Command error"
                                    } else {
                                        "Command"
                                    }),
                            ),
                    )
                    .child(
                        FormattedMessageElement::new(formatted)
                            .body_color(body_color)
                            .selection_group(
                                self.timeline_selection.clone(),
                                MessageSelectionKey::Command(local_id),
                            ),
                    ),
            )
            .into_any_element();

        div()
            .w_full()
            .pb(rems_from_px(if trailing_gap {
                TIMELINE_GROUP_GAP
            } else {
                0.
            }))
            .child(command_row)
            .into_any_element()
    }

    fn render_message(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(item) = self.message_list.get(index).copied() else {
            return div().into_any_element();
        };
        if let timeline::MessageListSource::Command {
            command_index,
            local_id,
        } = item.source
        {
            return self.render_command_row(command_index, local_id, item.trailing_gap, cx);
        }
        let Some(message_index) = item.message_index() else {
            return div().into_any_element();
        };
        let Some(message) = self.model.messages.get(message_index) else {
            return div().into_any_element();
        };
        let continuation = item.continuation;
        let collapsed_count = item.collapsed_count;
        let collapsed = item.is_collapsed();
        let settings = AppliedSettings::get(cx);
        let reduce_motion = cx.reduce_motion();
        let accent = sender_color(&message.sender, message.local, &settings);
        let background = if collapsed {
            settings.theme.color(ThemeRole::ControlSurface)
        } else if message.notice {
            settings.theme.color(ThemeRole::Raised)
        } else {
            settings.theme.color(ThemeRole::Window)
        };
        let row_hover = settings.theme.color(if collapsed {
            ThemeRole::ControlButtonHover
        } else {
            ThemeRole::StateRowHover
        });
        let message_id = message.id;
        let room_id = message.room_id;
        let reference_flash_id = self.message_reference_flash.and_then(|(target, flash_id)| {
            (target.room_id == room_id && target.message_id.0 == message_id).then_some(flash_id)
        });
        let formatted_message =
            (!collapsed).then(|| match self.formatted_messages.get(&message.id) {
                Some(formatted) if formatted.source() == message.body.as_str() => formatted.clone(),
                _ => Rc::new(FormattedMessage::plain(message.body.clone())),
            });
        let sender = message.sender.clone();
        let edited = message.edited;
        let unverified = message.unverified;
        let timestamp_ms = message.timestamp_ms;
        let current_ms = timeline::now_ms();
        let timestamp = timeline::format_age(timestamp_ms, current_ms);
        let day_separator_label = item
            .day_separator
            .then(|| timeline::format_day_label(timestamp_ms, current_ms))
            .flatten();
        let hover_group: SharedString = format!("message-actions-{message_id}").into();
        let local_actions = (!collapsed && message.local && !message.notice).then(|| {
            (
                message.room_id,
                local_rpc::ids::MessageId(message.id),
                message.attachment.is_none().then(|| message.body.clone()),
            )
        });
        let reference_target = (!message.notice && message.id != 0).then_some(MessageRef {
            room_id,
            message_id: local_rpc::ids::MessageId(message_id),
        });
        let formatted_message = formatted_message.map(|formatted_message| {
            let click_view = cx.entity().downgrade();
            let hover_view = cx.entity().downgrade();
            FormattedMessageElement::new(formatted_message)
                .selection_group(
                    self.timeline_selection.clone(),
                    MessageSelectionKey::Message(message_id),
                )
                .on_reference_click(Rc::new(move |target, shift, window, cx| {
                    let _ = click_view.update(cx, |this, cx| {
                        this.activate_message_reference(target, shift, window, cx)
                    });
                }))
                .on_reference_hover(Rc::new(move |hovered, _, cx| {
                    let _ =
                        hover_view.update(cx, |this, cx| this.hover_message_reference(hovered, cx));
                }))
        });
        let attachment = (!collapsed).then(|| message.attachment.clone()).flatten();
        let row_padding_top = timeline_message_row_padding_top(continuation);
        let message_row = div()
            .id(("message", message_id as usize))
            .group(hover_group.clone())
            .relative()
            .w_full()
            .pl(rems_from_px(64.))
            .pr(rems_from_px(28.))
            .pt(rems_from_px(row_padding_top))
            .pb(rems_from_px(TIMELINE_CONTINUATION_ROW_PADDING_Y))
            .bg(background)
            .hover(move |row| row.bg(row_hover))
            .when_some(reference_flash_id, |row, flash_id| {
                let flash = div()
                    .absolute()
                    .inset_0()
                    .bg(settings.theme.color(ThemeRole::StateSelected));
                let flash = if reduce_motion {
                    flash.opacity(0.65).into_any_element()
                } else {
                    flash
                        .with_animation(
                            ("message-reference-flash", flash_id as usize),
                            Animation::new(Duration::from_millis(1_600))
                                .with_easing(gpui::ease_out_quint()),
                            |flash, delta| flash.opacity(1.0 - delta),
                        )
                        .into_any_element()
                };
                row.child(flash)
            })
            .child(
                div()
                    .absolute()
                    .left(rems_from_px(64.))
                    .top_0()
                    .bottom_0()
                    .w(rems_from_px(3.))
                    .bg(accent),
            )
            .child(
                div()
                    .absolute()
                    .left_0()
                    .top(rems_from_px(row_padding_top))
                    .h(rems_from_px(TIMELINE_ROW_HEADER_HEIGHT))
                    .w(rems_from_px(64.))
                    .pr(rems_from_px(15.))
                    .flex()
                    .items_end()
                    .justify_end()
                    .text_xs()
                    .text_color(settings.theme.color(ThemeRole::TextDim))
                    .when(continuation, |time| {
                        time.invisible()
                            .group_hover(hover_group.clone(), |time| time.visible())
                    })
                    .child(div().child(timestamp)),
            )
            .child(
                div()
                    .relative()
                    .w_full()
                    .max_w(rems_from_px(860.))
                    .pl(rems_from_px(15.))
                    .child(
                        div()
                            .w_full()
                            .min_w_0()
                            .when(!continuation, |content| {
                                content.child(
                                    div()
                                        .min_h(rems_from_px(TIMELINE_ROW_HEADER_HEIGHT))
                                        .line_height(rems_from_px(TIMELINE_ROW_HEADER_HEIGHT))
                                        .flex()
                                        .items_end()
                                        .gap_2()
                                        .pr(rems_from_px(180.))
                                        .child(
                                            div()
                                                .font_weight(FontWeight::SEMIBOLD)
                                                .text_color(accent)
                                                .child(sender),
                                        )
                                        .when(!collapsed && edited, |meta| {
                                            meta.child(
                                                div()
                                                    .text_xs()
                                                    .text_color(
                                                        settings.theme.color(ThemeRole::TextDim),
                                                    )
                                                    .child("edited"),
                                            )
                                        })
                                        .when(!collapsed && unverified, |meta| {
                                            meta.child(
                                                div()
                                                    .text_xs()
                                                    .text_color(
                                                        settings
                                                            .theme
                                                            .color(ThemeRole::StateWarning),
                                                    )
                                                    .child("unverified"),
                                            )
                                        })
                                        .when_some(collapsed_count, |meta, count| {
                                            meta.child(
                                                div()
                                                    .min_w_0()
                                                    .truncate()
                                                    .text_xs()
                                                    .text_color(
                                                        settings.theme.color(ThemeRole::TextMuted),
                                                    )
                                                    .child(format!(
                                                        "· {count} {} collapsed",
                                                        if count == 1 {
                                                            "message"
                                                        } else {
                                                            "messages"
                                                        }
                                                    )),
                                            )
                                        }),
                                )
                            })
                            .when_some(formatted_message, |content, formatted_message| {
                                content.child(formatted_message).when_some(
                                    attachment,
                                    |content, attachment| {
                                        content.child(self.render_attachment(
                                            room_id, message_id, attachment, window, cx,
                                        ))
                                    },
                                )
                            }),
                    ),
            )
            .child(
                div()
                    .absolute()
                    .top(rems_from_px(timeline_message_actions_top(continuation)))
                    .right(rems_from_px(28.))
                    .flex()
                    .gap_1()
                    .invisible()
                    .group_hover(hover_group, |actions| actions.visible())
                    .when(collapsed, |actions| actions.visible())
                    .when_some(
                        local_actions,
                        |actions, (room_id, message_id, edit_body)| {
                            actions
                                .when_some(edit_body, |actions, edit_body| {
                                    actions.child(
                                        message_action_button(
                                            ("edit", message_id.0 as usize),
                                            IconName::Pencil,
                                            false,
                                            &settings.theme,
                                        )
                                        .on_click(
                                            cx.listener(move |this, _, _, cx| {
                                                this.begin_edit(
                                                    room_id,
                                                    message_id,
                                                    edit_body.clone(),
                                                    cx,
                                                )
                                            }),
                                        ),
                                    )
                                })
                                .child(
                                    message_action_button(
                                        ("delete", message_id.0 as usize),
                                        IconName::Trash,
                                        true,
                                        &settings.theme,
                                    )
                                    .on_click(cx.listener(
                                        move |this, _, _, cx| {
                                            this.delete_message(room_id, message_id, cx)
                                        },
                                    )),
                                )
                        },
                    )
                    .when_some(reference_target, |actions, target| {
                        actions
                            .child(
                                message_action_button(
                                    ("quote-reference", message_id as usize),
                                    IconName::CornerUpLeft,
                                    false,
                                    &settings.theme,
                                )
                                .on_click(cx.listener(
                                    move |this, _, window, cx| {
                                        this.quote_message_reference(target, window, cx)
                                    },
                                )),
                            )
                            .child(
                                message_action_button(
                                    ("copy-reference", message_id as usize),
                                    IconName::AtSign,
                                    false,
                                    &settings.theme,
                                )
                                .on_click(cx.listener(
                                    move |this, _, _, cx| this.copy_message_reference(target, cx),
                                )),
                            )
                    })
                    .child(
                        message_action_button(
                            ("collapse", message_id as usize),
                            if collapsed {
                                IconName::ListChevronsUpDown
                            } else {
                                IconName::ListChevronsDownUp
                            },
                            false,
                            &settings.theme,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.toggle_message_group(message_id, cx)
                        })),
                    ),
            )
            .into_any_element();

        let line_color = settings.theme.color(ThemeRole::BorderSubtle);
        div()
            .w_full()
            .pb(rems_from_px(if item.trailing_gap {
                TIMELINE_GROUP_GAP
            } else {
                0.
            }))
            .when_some(day_separator_label, |item, label| {
                item.child(
                    div()
                        .w_full()
                        .px_4()
                        .pt_4()
                        .pb_2()
                        .flex()
                        .items_center()
                        .gap_2()
                        .text_xs()
                        .text_color(settings.theme.color(ThemeRole::TextDim))
                        .child(div().h(px(1.)).flex_1().bg(line_color))
                        .child(label)
                        .child(div().h(px(1.)).flex_1().bg(line_color)),
                )
            })
            .child(message_row)
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
        let settings = AppliedSettings::get(cx);
        let descriptor = attachment.descriptor.clone();
        let render_kind = attachment.render_kind();
        if render_kind == AttachmentRenderKind::Image {
            let (cached_attachment, active_transfer) = {
                let mut cache = self.media_cache.lock().expect("media cache lock poisoned");
                (cache.get(descriptor.id), cache.active_transfer(&descriptor))
            };
            if let Some(attachment) = cached_attachment {
                let preview = descriptor.clone();
                let hover_border = settings.theme.color(ThemeRole::BorderFocus);
                return image_frame(&descriptor, &settings.theme)
                    .id(("image-frame", message_id as usize))
                    .mt_2()
                    .overflow_hidden()
                    .cursor_pointer()
                    .hover(move |image| image.border_color(hover_border))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_image_preview(preview.clone(), window, cx)
                    }))
                    .child(
                        img(cached_attachment_image_source(
                            attachment,
                            self.image_cache.clone(),
                        ))
                        .id(("image", message_id as usize))
                        .absolute()
                        .inset_0()
                        .size_full()
                        .object_fit(ObjectFit::Contain),
                    )
                    .into_any_element();
            }
            let fetch = EagerImageFetch::new(room_id, descriptor.clone());
            if let Some(transfer_id) = active_transfer {
                let action = mini_button(
                    ("cancel-image-read", transfer_id.0 as usize),
                    "Cancel",
                    &settings.theme,
                )
                .on_click(cx.listener(move |this, _, _, cx| this.cancel_bulk_read(transfer_id, cx)))
                .into_any_element();
                return Self::render_image_status(
                    message_id,
                    &descriptor,
                    format!("Fetching {}…", descriptor.file_name),
                    Some(action),
                    &settings.theme,
                );
            }
            if let Some(reason) = self.eager_image_fetches.failure(fetch.key) {
                let retry = fetch.clone();
                let action = mini_button(
                    ("retry-image-read", message_id as usize),
                    "Retry",
                    &settings.theme,
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.retry_eager_image(retry.clone(), window, cx)
                }))
                .into_any_element();
                return Self::render_image_status(
                    message_id,
                    &descriptor,
                    format!("Could not fetch {} · {reason}", descriptor.file_name),
                    Some(action),
                    &settings.theme,
                );
            }
            self.enqueue_eager_image(fetch, window, cx);
            return Self::render_image_status(
                message_id,
                &descriptor,
                format!("Loading {}…", descriptor.file_name),
                None,
                &settings.theme,
            );
        }
        if render_kind == AttachmentRenderKind::Audio {
            return self.render_attachment_audio(room_id, message_id, descriptor, cx);
        }
        if render_kind == AttachmentRenderKind::Video {
            let key = video_key(room_id, message_id, &descriptor);
            let source_key = self.source_key(room_id, descriptor.id);
            let has_cached_poster = self
                .video_thumbnails
                .view(ThumbnailKey { source_key })
                .image
                .is_some();
            return match self.video_sources.view(source_key) {
                VideoSourceView::Ready(source) => {
                    self.render_attachment_video(key, descriptor, Some(source), false, cx)
                }
                VideoSourceView::Failed { reason, retryable } => {
                    let retry_key = source_key;
                    let action = mini_button(
                        ("retry-video-source", message_id as usize),
                        "Retry",
                        &settings.theme,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.video_sources.retry(retry_key);
                        this.pump_video_sources(cx);
                        cx.notify();
                    }))
                    .into_any_element();
                    let suffix = if retryable {
                        " · retrying automatically"
                    } else {
                        ""
                    };
                    Self::render_video_source_status(
                        message_id,
                        &descriptor,
                        format!("Could not load preview · {reason}{suffix}"),
                        Some(action),
                        &settings.theme,
                    )
                }
                VideoSourceView::Absent => {
                    self.video_sources.promote(source_key, descriptor.clone());
                    self.video_thumbnails.warm();
                    self.pump_video_sources(cx);
                    if has_cached_poster {
                        self.render_attachment_video(key, descriptor, None, false, cx)
                    } else {
                        Self::render_video_source_status(
                            message_id,
                            &descriptor,
                            "Loading preview…".into(),
                            None,
                            &settings.theme,
                        )
                    }
                }
                VideoSourceView::Loading => {
                    if has_cached_poster {
                        self.render_attachment_video(key, descriptor, None, false, cx)
                    } else {
                        Self::render_video_source_status(
                            message_id,
                            &descriptor,
                            "Loading preview…".into(),
                            None,
                            &settings.theme,
                        )
                    }
                }
            };
        }
        let (cached, active_transfer) = {
            let cache = self.media_cache.lock().expect("media cache lock poisoned");
            (
                cache.contains(descriptor.id),
                cache.active_transfer(&descriptor),
            )
        };
        if descriptor.media_kind == MediaKind::File {
            let open = descriptor.clone();
            let label = if cached {
                format!(
                    "{} · {} bytes · click to preview",
                    descriptor.file_name, descriptor.byte_len
                )
            } else if active_transfer.is_some() {
                format!("Fetching {}…", descriptor.file_name)
            } else {
                format!(
                    "{} · {} bytes · click to preview",
                    descriptor.file_name, descriptor.byte_len
                )
            };
            return div()
                .id(("file-attachment", message_id as usize))
                .mt_2()
                .px_3()
                .py_2()
                .flex()
                .items_center()
                .gap_3()
                .bg(settings.theme.color(ThemeRole::Panel))
                .cursor_pointer()
                .hover({
                    let hover = settings.theme.color(ThemeRole::StateHover);
                    move |item| item.bg(hover)
                })
                .text_sm()
                .text_color(settings.theme.color(ThemeRole::TextSecondary))
                .child(div().flex_1().min_w_0().truncate().child(label))
                .when_some(active_transfer, |card, transfer_id| {
                    card.child(
                        mini_button(
                            ("cancel-read", transfer_id.0 as usize),
                            "Cancel",
                            &settings.theme,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.cancel_bulk_read(transfer_id, cx)
                        })),
                    )
                })
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.open_code_preview(room_id, open.clone(), window, cx)
                }))
                .into_any_element();
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
                .bg(settings.theme.color(ThemeRole::Panel))
                .child(
                    div()
                        .flex_1()
                        .text_sm()
                        .text_color(settings.theme.color(ThemeRole::TextSecondary))
                        .child(format!("Fetching {}…", descriptor.file_name)),
                )
                .child(
                    mini_button(
                        ("cancel-read", transfer_id.0 as usize),
                        "Cancel",
                        &settings.theme,
                    )
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.cancel_bulk_read(transfer_id, cx)),
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
            .bg(settings.theme.color(ThemeRole::Panel))
            .cursor_pointer()
            .hover({
                let hover = settings.theme.color(ThemeRole::StateHover);
                move |item| item.bg(hover)
            })
            .text_sm()
            .text_color(settings.theme.color(ThemeRole::TextSecondary))
            .child(format!(
                "{} · {} bytes · click to fetch",
                descriptor.file_name, descriptor.byte_len
            ))
            .on_click(cx.listener(move |this, _, _, cx| this.fetch_attachment(fetch.clone(), cx)))
            .into_any_element()
    }

    fn render_video_source_status(
        message_id: u64,
        _descriptor: &AttachmentDescriptor,
        label: String,
        action: Option<AnyElement>,
        palette: &ThemePalette,
    ) -> AnyElement {
        video_frame(palette)
            .id(("video-source-status", message_id as usize))
            .mt_2()
            .px_3()
            .py_2()
            .flex()
            .items_center()
            .justify_center()
            .gap_3()
            .bg(palette.color(ThemeRole::Panel))
            .child(
                div()
                    .min_w_0()
                    .text_sm()
                    .text_color(palette.color(ThemeRole::TextSecondary))
                    .child(label),
            )
            .when_some(action, |status, action| status.child(action))
            .into_any_element()
    }

    fn render_image_status(
        message_id: u64,
        descriptor: &AttachmentDescriptor,
        label: String,
        action: Option<AnyElement>,
        palette: &ThemePalette,
    ) -> AnyElement {
        image_frame(descriptor, palette)
            .id(("image-status", message_id as usize))
            .mt_2()
            .px_3()
            .py_2()
            .flex()
            .items_center()
            .justify_center()
            .gap_3()
            .bg(palette.color(ThemeRole::Panel))
            .child(
                div()
                    .min_w_0()
                    .text_sm()
                    .text_color(palette.color(ThemeRole::TextSecondary))
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

    fn enqueue_new_image_fetches(&mut self, window: &Window, cx: &mut Context<Self>) {
        let descriptors = self
            .model
            .messages
            .iter()
            .rev()
            .filter_map(|message| {
                message
                    .attachment
                    .as_ref()
                    .filter(|attachment| attachment.is_image())
                    .map(|attachment| (message.room_id, attachment.descriptor.clone()))
            })
            .collect::<Vec<_>>();
        for (room_id, descriptor) in descriptors {
            let cached_or_active = {
                let cache = self.media_cache.lock().expect("media cache lock poisoned");
                cache.contains(descriptor.id) || cache.active_transfer(&descriptor).is_some()
            };
            if !cached_or_active {
                self.eager_image_fetches
                    .enqueue(EagerImageFetch::new(room_id, descriptor));
            }
        }
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
            match self.begin_attachment_read(fetch.room_id, fetch.descriptor.clone(), cx) {
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

    fn fetch_attachment(&mut self, descriptor: AttachmentDescriptor, cx: &mut Context<Self>) {
        if !self.model.is_ready() {
            return;
        }
        let Some(room_id) = self.model.selected_room else {
            return;
        };
        log::info!(
            "attachment fetch requested room={} attachment_timestamp_ms={} attachment_transfer_id={} file={:?} media_kind={:?} content_type={:?} bytes={}",
            room_id.0,
            descriptor.id.timestamp_ms,
            descriptor.id.transfer_id.0,
            descriptor.file_name,
            descriptor.media_kind,
            descriptor.content_type,
            descriptor.byte_len,
        );
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
            if cache.contains(descriptor.id) || cache.active_transfer(&descriptor).is_some() {
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
        let read = local_rpc::bulk::BeginAttachmentRead {
            transfer_id,
            room_id,
            attachment_id: descriptor.id,
        };
        if let Err(error) = self
            .daemon
            .send(ClientFrame::BeginAttachmentRead { request_id, read })
        {
            log::error!(
                "attachment request enqueue failed request_id={} transfer_id={}: {error}",
                request_id.0,
                transfer_id.0,
            );
            self.model.pending.remove(&request_id);
            self.media_cache
                .lock()
                .expect("media cache lock poisoned")
                .cancel(transfer_id);
            return Err(error);
        } else {
            log::info!(
                "attachment request queued request_id={} bulk_transfer_id={} room={} attachment_timestamp_ms={} attachment_transfer_id={} file={:?} bytes={}",
                request_id.0,
                transfer_id.0,
                room_id.0,
                descriptor.id.timestamp_ms,
                descriptor.id.transfer_id.0,
                descriptor.file_name,
                descriptor.byte_len,
            );
            self.status = format!("Fetching {}…", descriptor.file_name).into();
        }
        cx.notify();
        Ok(Some(transfer_id))
    }

    fn render_preview_tabs(
        &mut self,
        active_key: AttachmentId,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let settings = AppliedSettings::get(cx);
        let tab_hover = settings.theme.color(ThemeRole::Raised);
        let tab_hover_text = settings.theme.color(ThemeRole::TextPrimary);
        let close_hover = settings.theme.color(ThemeRole::StateHover);
        let history = self.preview_history.items().to_vec();
        let mut tabs = div()
            .id("preview-tabs")
            .h_full()
            .flex_1()
            .min_w_0()
            .flex()
            .overflow_x_scroll()
            .track_scroll(&self.preview_tabs_scroll);
        for item in history {
            let key = item.key();
            let selected = key == active_key;
            let tab_icon = match item.content {
                PreviewContent::Image { .. } => IconName::Image,
                PreviewContent::Code(_) => IconName::FileText,
            };
            let tab_icon_color = settings.theme.color(if selected {
                ThemeRole::MediaProgressKnob
            } else {
                ThemeRole::TextMuted
            });
            let select_key = key;
            let close_key = key;
            let select_id: SharedString = format!(
                "preview-tab-select-{}-{}",
                key.timestamp_ms, key.transfer_id.0
            )
            .into();
            let close_id: SharedString = format!(
                "preview-tab-close-{}-{}",
                key.timestamp_ms, key.transfer_id.0
            )
            .into();
            tabs = tabs.child(
                div()
                    .h_full()
                    .max_w(rems_from_px(210.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .border_r_1()
                    .border_color(settings.theme.color(ThemeRole::BorderSubtle))
                    .bg(settings.theme.color(if selected {
                        ThemeRole::Window
                    } else {
                        ThemeRole::Panel
                    }))
                    .text_color(settings.theme.color(if selected {
                        ThemeRole::MediaProgressKnob
                    } else {
                        ThemeRole::TextMuted
                    }))
                    .hover(move |tab| tab.bg(tab_hover).text_color(tab_hover_text))
                    .child(
                        div()
                            .id(select_id)
                            .h_full()
                            .min_w_0()
                            .flex_1()
                            .flex()
                            .items_center()
                            .gap_2()
                            .pl_3()
                            .pr_1()
                            .cursor_pointer()
                            .child(icon(
                                tab_icon,
                                PREVIEW_HEADER_ICON_SIZE,
                                tab_icon_color,
                            ))
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_xs()
                                    .child(item.descriptor.file_name),
                            )
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.select_preview(select_key, window, cx)
                            })),
                    )
                    .child(
                        div()
                            .id(close_id)
                            .w(rems_from_px(28.0))
                            .h_full()
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .cursor_pointer()
                            .hover(move |button| button.bg(close_hover).text_color(tab_hover_text))
                            .child(icon(
                                IconName::Close,
                                PREVIEW_HEADER_ICON_SIZE,
                                settings.theme.color(ThemeRole::TextMuted),
                            ))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.close_preview_tab(close_key, window, cx)
                            })),
                    ),
            );
        }
        tabs
    }

    fn render_preview_panel(
        &mut self,
        active: PreviewItem,
        width: Pixels,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        if matches!(active.content, PreviewContent::Code(_)) {
            return self.render_code_preview_panel(active, width, cx);
        }
        let settings = AppliedSettings::get(cx);
        let muted = settings.theme.color(ThemeRole::TextMuted);
        let tabs = self.render_preview_tabs(active.key(), cx);
        let cached_attachment = self
            .media_cache
            .lock()
            .expect("media cache lock poisoned")
            .get(active.descriptor.id);
        let cache_missing = cached_attachment.is_none();
        let viewport =
            preview_image_viewport_bounds(window.viewport_size(), width, window.rem_size());
        self.preview_image_viewport.set(Some(viewport));
        let geometry = self.preview_image.geometry(viewport);
        let zoom_percent = self.preview_image.zoom_percent(viewport);
        let can_pan = self.preview_image.can_pan(viewport);
        let panning = self.preview_last_mouse_position.is_some() && can_pan;

        let viewport_element = div()
            .id("preview-image-viewport")
            .relative()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .bg(settings.theme.color(ThemeRole::MediaViewport))
            .cursor(if panning {
                gpui::CursorStyle::ClosedHand
            } else if can_pan {
                gpui::CursorStyle::OpenHand
            } else {
                gpui::CursorStyle::Arrow
            })
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _, cx| {
                this.scroll_preview_image(event, cx)
            }))
            .on_pinch(
                cx.listener(|this, event: &PinchEvent, _, cx| this.pinch_preview_image(event, cx)),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, _, cx| {
                    this.preview_image_mouse_down(event, cx)
                }),
            )
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                this.preview_image_mouse_move(event, cx)
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, cx| this.finish_preview_image_pan(cx)),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, cx| this.finish_preview_image_pan(cx)),
            )
            .when_some(cached_attachment, |viewport_element, attachment| {
                viewport_element.child(
                    img(cached_attachment_image_source(
                        attachment,
                        self.preview_image_cache.clone(),
                    ))
                    .absolute()
                    .left(geometry.bounds.origin.x - viewport.origin.x)
                    .top(geometry.bounds.origin.y - viewport.origin.y)
                    .w(geometry.bounds.size.width)
                    .h(geometry.bounds.size.height)
                    .object_fit(ObjectFit::Contain)
                    .with_loading(move || {
                        div()
                            .size_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_sm()
                            .text_color(muted)
                            .child("loading…")
                            .into_any_element()
                    })
                    .with_fallback(move || {
                        div()
                            .size_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_sm()
                            .text_color(muted)
                            .child("failed to load image")
                            .into_any_element()
                    }),
                )
            })
            .when(cache_missing, |viewport_element| {
                viewport_element.child(
                    div()
                        .absolute()
                        .inset_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_sm()
                        .text_color(muted)
                        .child("image is no longer cached"),
                )
            });

        div()
            .id("preview-panel")
            .w(width)
            .h_full()
            .min_w_0()
            .flex_none()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(settings.theme.color(ThemeRole::Window))
            .track_focus(&self.code_viewer_focus)
            .child(
                div()
                    .min_h(rems_from_px(PREVIEW_TAB_BAR_HEIGHT))
                    .flex_none()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .min_w_0()
                    .border_b_1()
                    .border_color(settings.theme.color(ThemeRole::BorderSubtle))
                    .bg(settings.theme.color(ThemeRole::Panel))
                    .child(tabs)
                    .child(
                        div()
                            .h_full()
                            .flex_none()
                            .flex()
                            .items_center()
                            .gap_1()
                            .px_2()
                            .bg(settings.theme.color(ThemeRole::Panel))
                            .child(
                                preview_control_button("preview-fit", "Fit", &settings.theme)
                                    .min_h(rems_from_px(PREVIEW_TAB_BAR_HEIGHT))
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.fit_preview_image(cx)),
                                    ),
                            )
                            .child(
                                preview_control_button("preview-actual", "1:1", &settings.theme)
                                    .min_h(rems_from_px(PREVIEW_TAB_BAR_HEIGHT))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.actual_size_preview_image(cx)
                                    })),
                            )
                            .child(
                                preview_control_button("preview-zoom-out", "−", &settings.theme)
                                    .min_h(rems_from_px(PREVIEW_TAB_BAR_HEIGHT))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.zoom_preview_image(-0.25, cx)
                                    })),
                            )
                            .child(
                                div()
                                    .w(rems_from_px(44.0))
                                    .text_center()
                                    .text_xs()
                                    .text_color(muted)
                                    .child(format!("{zoom_percent}%")),
                            )
                            .child(
                                preview_control_button("preview-zoom-in", "+", &settings.theme)
                                    .min_h(rems_from_px(PREVIEW_TAB_BAR_HEIGHT))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.zoom_preview_image(0.25, cx)
                                    })),
                            )
                            .child(
                                preview_action_button(
                                    "preview-save",
                                    IconName::Download,
                                    &settings.theme,
                                )
                                .on_click(cx.listener(
                                    |this, _, window, cx| this.save_preview_attachment(window, cx),
                                )),
                            )
                            .child(
                                preview_action_button(
                                    "preview-close",
                                    IconName::Close,
                                    &settings.theme,
                                )
                                .on_click(
                                    cx.listener(|this, _, window, cx| {
                                        this.close_preview(window, cx)
                                    }),
                                ),
                            ),
                    ),
            )
            .child(viewport_element)
    }

    fn render_code_preview_panel(
        &mut self,
        active: PreviewItem,
        width: Pixels,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let settings = AppliedSettings::get(cx);
        let tabs = self.render_preview_tabs(active.key(), cx);
        let preview = active
            .code_preview()
            .expect("code preview panel requires code content")
            .clone();
        let ready = matches!(&preview.state, CodePreviewState::Ready(_));
        let compact_search =
            width < crate::ui_scale::scaled_px(360.0, crate::ui_scale::rem_size(cx));
        let active_match = self
            .code_search_open
            .then(|| {
                let CodePreviewState::Ready(document) = &preview.state else {
                    return None;
                };
                self.code_search_results
                    .get(self.code_search_result_index)
                    .map(|search_match| document.match_target(search_match))
            })
            .flatten();
        let active_match_hidden = active_match.is_some_and(|target| target.hidden);
        let search_status: SharedString = if self.code_search_input.read(cx).text().is_empty() {
            "".into()
        } else if self.code_search_pending {
            if compact_search {
                "…".into()
            } else {
                "Searching…".into()
            }
        } else if self.code_search_results.is_empty() {
            if compact_search {
                "0".into()
            } else {
                "No matches".into()
            }
        } else {
            if compact_search {
                format!(
                    "{}/{}{}",
                    self.code_search_result_index + 1,
                    self.code_search_results.len(),
                    if active_match_hidden { "*" } else { "" }
                )
                .into()
            } else {
                format!(
                    "{} / {}{}",
                    self.code_search_result_index + 1,
                    self.code_search_results.len(),
                    if active_match_hidden {
                        " · hidden"
                    } else {
                        ""
                    }
                )
                .into()
            }
        };
        let body = match preview.state {
            CodePreviewState::Fetching { .. } => preview_status("loading…", &settings.theme),
            CodePreviewState::Preparing { .. } => preview_status("highlighting…", &settings.theme),
            CodePreviewState::Error(reason) => preview_status(reason, &settings.theme),
            CodePreviewState::Ready(document) => div()
                .id("preview-code-viewport")
                .flex_1()
                .min_w_0()
                .min_h_0()
                .overflow_hidden()
                .bg(settings.theme.color(ThemeRole::MediaViewport))
                .child(render_code_document(
                    document,
                    preview.scroll_handle.clone(),
                    preview.view_state.clone(),
                    preview.scrollbar_state.clone(),
                    self.code_selection.clone(),
                    active_match,
                    Some(settings.clone()),
                ))
                .into_any_element(),
        };

        div()
            .id("preview-panel")
            .w(width)
            .h_full()
            .min_w_0()
            .flex_none()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(settings.theme.color(ThemeRole::Window))
            .key_context("ChattCodeViewer")
            .track_focus(&self.code_viewer_focus)
            .child(
                div()
                    .min_h(rems_from_px(PREVIEW_TAB_BAR_HEIGHT))
                    .flex_none()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .min_w_0()
                    .border_b_1()
                    .border_color(settings.theme.color(ThemeRole::BorderSubtle))
                    .bg(settings.theme.color(ThemeRole::Panel))
                    .child(tabs)
                    .child(
                        div()
                            .h_full()
                            .flex_none()
                            .flex()
                            .items_center()
                            .gap_1()
                            .px_2()
                            .bg(settings.theme.color(ThemeRole::Panel))
                            .when(ready, |actions| {
                                actions
                                    .child(
                                        preview_action_button(
                                            "preview-find-code",
                                            IconName::Search,
                                            &settings.theme,
                                        )
                                        .on_click(
                                            cx.listener(|this, _, window, cx| {
                                                this.open_code_search(window, cx)
                                            }),
                                        ),
                                    )
                                    .child(
                                        preview_action_button(
                                            "preview-copy-code",
                                            IconName::Copy,
                                            &settings.theme,
                                        )
                                        .on_click(
                                            cx.listener(|this, _, _, cx| {
                                                this.copy_preview_code(cx)
                                            }),
                                        ),
                                    )
                            })
                            .child(
                                preview_action_button(
                                    "preview-save",
                                    IconName::Download,
                                    &settings.theme,
                                )
                                .on_click(cx.listener(
                                    |this, _, window, cx| this.save_preview_attachment(window, cx),
                                )),
                            )
                            .child(
                                preview_action_button(
                                    "preview-close",
                                    IconName::Close,
                                    &settings.theme,
                                )
                                .on_click(
                                    cx.listener(|this, _, window, cx| {
                                        this.close_preview(window, cx)
                                    }),
                                ),
                            ),
                    ),
            )
            .when(self.code_search_open && ready, |panel| {
                panel.child(
                    div()
                        .min_h(rems_from_px(PREVIEW_SEARCH_BAR_HEIGHT))
                        .flex_none()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap(rems_from_px(if compact_search { 4.0 } else { 8.0 }))
                        .px(rems_from_px(if compact_search { 4.0 } else { 8.0 }))
                        .border_b_1()
                        .border_color(settings.theme.color(ThemeRole::BorderSubtle))
                        .bg(settings.theme.color(ThemeRole::Window))
                        .child(
                            div()
                                .min_h(rems_from_px(30.0))
                                .min_w_0()
                                .flex_1()
                                .flex()
                                .items_center()
                                .px_2()
                                .border_1()
                                .border_color(settings.theme.color(ThemeRole::BorderStrong))
                                .bg(settings.theme.color(ThemeRole::Input))
                                .text_sm()
                                .child(self.code_search_input.clone()),
                        )
                        .child(
                            div()
                                .w(rems_from_px(if compact_search {
                                    48.0
                                } else if active_match_hidden {
                                    112.0
                                } else {
                                    70.0
                                }))
                                .flex_none()
                                .text_right()
                                .text_xs()
                                .text_color(settings.theme.color(ThemeRole::TextMuted))
                                .child(search_status),
                        )
                        .child(
                            preview_control_button("preview-find-previous", "↑", &settings.theme)
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.previous_code_match(cx)),
                                ),
                        )
                        .child(
                            preview_control_button("preview-find-next", "↓", &settings.theme)
                                .on_click(cx.listener(|this, _, _, cx| this.next_code_match(cx))),
                        )
                        .child(
                            preview_control_button("preview-find-close", "×", &settings.theme)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.close_code_search(cx);
                                    window.focus(&this.code_viewer_focus, cx);
                                })),
                        ),
                )
            })
            .child(body)
    }

    fn render_attachment_audio(
        &mut self,
        room_id: RoomId,
        message_id: u64,
        descriptor: AttachmentDescriptor,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = audio_key(room_id, message_id, &descriptor);
        let source_key = self.source_key(room_id, descriptor.id);
        let mut audio = self.audios.view(key);
        match self.video_sources.view(source_key) {
            VideoSourceView::Loading if self.pending_audio_plays.contains(&key) => {
                audio.loading = true;
            }
            VideoSourceView::Failed { reason, .. } if audio.error.is_none() => {
                audio.loading = false;
                audio.error = Some(format!("Could not open audio · {reason}"));
            }
            _ => {}
        }
        let duration = audio.duration;
        let active_scrub = self.audio_scrub.filter(|scrub| scrub.key == key);
        let display_position = active_scrub.map_or(audio.position, AudioScrub::position);
        let view = cx.entity().downgrade();
        let handler: AudioPlayerHandler = Rc::new(move |event, _, cx| {
            let _ = view.update(cx, |this, cx| {
                this.handle_audio_player_event(key, duration, event, cx)
            });
        });
        render_audio_player(
            AudioPlayerConfig {
                key,
                audio,
                duration,
                display_position,
            },
            handler,
            AppliedSettings::get(cx),
        )
        .into_any_element()
    }

    fn handle_audio_player_event(
        &mut self,
        key: AudioKey,
        duration: f64,
        event: AudioPlayerEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            AudioPlayerEvent::Play => self.play_audio(key, cx),
            AudioPlayerEvent::ScrubPressed { bounds, event } => {
                self.begin_audio_scrub(key, duration, bounds, &event, cx)
            }
            AudioPlayerEvent::CycleSpeed => self.cycle_audio_speed(key, cx),
            AudioPlayerEvent::ToggleMute => self.toggle_audio_mute(key, cx),
            AudioPlayerEvent::VolumePressed { bounds, event } => {
                self.begin_audio_volume_drag(key, bounds, &event, cx)
            }
        }
    }

    fn play_audio(&mut self, key: AudioKey, cx: &mut Context<Self>) {
        self.note_media_interaction(MediaPlaybackTarget::Audio(key));
        self.pending_audio_plays.retain(|pending| *pending == key);
        let source_key = self.source_key(key.room_id, key.attachment_id);
        match self.video_sources.view(source_key) {
            VideoSourceView::Ready(source) => {
                self.pending_audio_plays.remove(&key);
                match self.audios.play(key, Some(source)) {
                    Ok(()) => self.status = "Starting audio playback…".into(),
                    Err(error) => {
                        log::error!("embedded audio play failed key={key:?}: {error:#}");
                        self.status = format!("Audio playback failed: {error}").into();
                    }
                }
            }
            VideoSourceView::Absent | VideoSourceView::Loading | VideoSourceView::Failed { .. } => {
                if let Err(error) = self.audios.play(key, None) {
                    log::error!("embedded audio preparation failed key={key:?}: {error:#}");
                    self.status = format!("Audio playback failed: {error}").into();
                } else if let Some(descriptor) = self.audio_descriptor(key) {
                    self.pending_audio_plays.insert(key);
                    self.video_sources.promote(source_key, descriptor);
                    self.video_sources
                        .set_pin(source_key, VideoSourcePin::PendingPlay, true);
                    self.video_sources.retry(source_key);
                    self.pump_video_sources(cx);
                    self.status = "Preparing audio playback…".into();
                } else {
                    self.status = "Audio source is no longer available".into();
                }
            }
        }
        self.sync_video_source_pins();
        cx.notify();
    }

    fn seek_audio(&mut self, key: AudioKey, seconds: f64, cx: &mut Context<Self>) {
        self.note_media_interaction(MediaPlaybackTarget::Audio(key));
        if let Err(error) = self.audios.seek(key, seconds) {
            log::error!("embedded audio seek failed key={key:?} seconds={seconds}: {error:#}");
            self.status = format!("Audio seek failed: {error}").into();
        }
        cx.notify();
    }

    fn begin_audio_scrub(
        &mut self,
        key: AudioKey,
        duration: f64,
        bounds: Bounds<Pixels>,
        event: &MouseDownEvent,
        cx: &mut Context<Self>,
    ) {
        let Some(fraction) = horizontal_fraction(bounds, event.position.x, duration) else {
            return;
        };
        self.note_media_interaction(MediaPlaybackTarget::Audio(key));
        self.audio_scrub = Some(AudioScrub {
            key,
            bounds,
            duration,
            last_fraction: fraction,
            last_seek: Instant::now(),
        });
        if let Err(error) = self.audios.scrub(key, fraction, duration) {
            log::error!("embedded audio initial scrub failed key={key:?}: {error:#}");
            self.status = format!("Audio seek failed: {error}").into();
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn drag_audio_scrub(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) -> bool {
        let Some(mut scrub) = self.audio_scrub else {
            return false;
        };
        if !event.dragging() {
            self.finish_audio_scrub(cx);
            return true;
        }
        let Some(fraction) = horizontal_fraction(scrub.bounds, event.position.x, scrub.duration)
        else {
            self.finish_audio_scrub(cx);
            return true;
        };
        if fraction != scrub.last_fraction {
            scrub.last_fraction = fraction;
            let dispatch = scrub.should_dispatch_seek(Instant::now());
            self.audio_scrub = Some(scrub);
            if dispatch && let Err(error) = self.audios.scrub(scrub.key, fraction, scrub.duration) {
                log::error!(
                    "embedded audio drag scrub failed key={:?}: {error:#}",
                    scrub.key
                );
                self.status = format!("Audio seek failed: {error}").into();
            }
            cx.notify();
        }
        cx.stop_propagation();
        true
    }

    fn finish_audio_scrub(&mut self, cx: &mut Context<Self>) {
        let Some(scrub) = self.audio_scrub.take() else {
            return;
        };
        if let Err(error) = self
            .audios
            .scrub(scrub.key, scrub.last_fraction, scrub.duration)
        {
            log::error!(
                "embedded audio final scrub failed key={:?}: {error:#}",
                scrub.key
            );
            self.status = format!("Audio seek failed: {error}").into();
        }
        cx.notify();
    }

    fn set_audio_volume(&mut self, key: AudioKey, volume: f64, cx: &mut Context<Self>) {
        self.note_media_interaction(MediaPlaybackTarget::Audio(key));
        if let Err(error) = self.audios.set_volume_for(key, volume) {
            log::error!("embedded audio volume failed key={key:?}: {error:#}");
            self.status = format!("Audio volume failed: {error}").into();
        }
        cx.notify();
    }

    fn toggle_audio_mute(&mut self, key: AudioKey, cx: &mut Context<Self>) {
        self.note_media_interaction(MediaPlaybackTarget::Audio(key));
        if let Err(error) = self.audios.toggle_mute(key) {
            log::error!("embedded audio mute failed key={key:?}: {error:#}");
            self.status = format!("Audio volume failed: {error}").into();
        }
        cx.notify();
    }

    fn cycle_audio_speed(&mut self, key: AudioKey, cx: &mut Context<Self>) {
        self.note_media_interaction(MediaPlaybackTarget::Audio(key));
        if let Err(error) = self.audios.cycle_playback_speed(key) {
            log::error!("embedded audio speed change failed key={key:?}: {error:#}");
            self.status = format!("Audio speed change failed: {error}").into();
        }
        cx.notify();
    }

    fn begin_audio_volume_drag(
        &mut self,
        key: AudioKey,
        bounds: Bounds<Pixels>,
        event: &MouseDownEvent,
        cx: &mut Context<Self>,
    ) {
        let Some(fraction) = horizontal_fraction(bounds, event.position.x, 1.0) else {
            return;
        };
        self.audio_volume_drag = Some(AudioVolumeDrag { key, bounds });
        self.set_audio_volume(key, fraction * 100.0, cx);
        cx.stop_propagation();
    }

    fn drag_audio_volume(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) -> bool {
        let Some(drag) = self.audio_volume_drag else {
            return false;
        };
        if !event.dragging() {
            self.finish_audio_volume_drag(cx);
            return true;
        }
        if let Some(fraction) = horizontal_fraction(drag.bounds, event.position.x, 1.0) {
            self.set_audio_volume(drag.key, fraction * 100.0, cx);
        }
        cx.stop_propagation();
        true
    }

    fn finish_audio_volume_drag(&mut self, cx: &mut Context<Self>) {
        if self.audio_volume_drag.take().is_some() {
            cx.notify();
        }
    }

    fn render_attachment_video(
        &mut self,
        key: VideoKey,
        descriptor: AttachmentDescriptor,
        registered_source: Option<RegisteredAttachmentSource>,
        theater: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let video = self.videos.view(key);
        let thumbnail_key = ThumbnailKey {
            source_key: self.source_key(key.room_id, descriptor.id),
        };
        let thumbnail = if let Some(source) = registered_source.as_ref() {
            let thumbnail = self.video_thumbnails.request(thumbnail_key, source.clone());
            if thumbnail.pending {
                self.video_sources
                    .set_pin(source.source().key(), VideoSourcePin::Thumbnail, true);
            }
            thumbnail
        } else {
            self.video_thumbnails.view(thumbnail_key)
        };
        let duration = if video.duration > 0.0 {
            video.duration
        } else {
            thumbnail.duration.unwrap_or(0.0)
        };
        let active_scrub = self.video_scrub.filter(|scrub| scrub.key == key);
        let display_position = active_scrub.map_or(video.position, VideoScrub::position);
        let active_controls = self.video_controls.active_key == Some(key);
        let controls_phase = active_controls
            .then_some(self.video_controls.phase)
            .unwrap_or_default();
        let controls_pinned = video.paused
            || video.finished
            || active_scrub.is_some()
            || self.video_volume_drag.is_some_and(|drag| drag.key == key)
            || (active_controls
                && (self.video_controls.bar_hovered || self.video_controls.volume_open));
        let scrub_hover_fraction = active_controls
            .then(|| {
                active_scrub
                    .map(|scrub| scrub.last_fraction)
                    .or(self.video_controls.scrub_hover_fraction)
            })
            .flatten();
        let source = TheaterVideo {
            key,
            descriptor: descriptor.clone(),
            source: registered_source,
        };
        let view = cx.entity().downgrade();
        let event_source = source.clone();
        let handler: VideoPlayerHandler = Rc::new(move |event, _, cx| {
            let source = event_source.clone();
            let _ = view.update(cx, |this, cx| {
                this.handle_video_player_event(source, duration, event, cx)
            });
        });
        let fallback_label = video.error.clone().unwrap_or_else(|| {
            if source.source.is_none() && thumbnail.image.is_none() {
                "Loading preview…".into()
            } else {
                descriptor.file_name.clone()
            }
        });
        let aspect_ratio = aspect_ratio(&video, (descriptor.width, descriptor.height));
        render_video_player(
            VideoPlayerConfig {
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
                volume_open: active_controls && self.video_controls.volume_open,
                measure_volume_bounds: active_controls,
            },
            handler,
            self.video_volume_popup_bounds.clone(),
            self.video_volume_button_bounds.clone(),
            AppliedSettings::get(cx),
        )
    }

    fn handle_video_player_event(
        &mut self,
        source: TheaterVideo,
        duration: f64,
        event: VideoPlayerEvent,
        cx: &mut Context<Self>,
    ) {
        let key = source.key;
        match event {
            VideoPlayerEvent::PlayerHovered(hovered) => self.hover_video_player(key, hovered, cx),
            VideoPlayerEvent::PointerMoved => self.video_pointer_moved(key, cx),
            VideoPlayerEvent::SurfaceClicked {
                click_count,
                unstarted,
            } => self.click_video_surface(source, click_count, unstarted, cx),
            VideoPlayerEvent::Play => {
                self.video_surface_click_task.take();
                self.play_video(key, cx);
            }
            VideoPlayerEvent::ScrubHovered(fraction) => self.hover_video_scrub(key, fraction, cx),
            VideoPlayerEvent::ScrubHoverCleared => self.clear_video_scrub_hover(key, cx),
            VideoPlayerEvent::ScrubPressed { bounds, event } => {
                self.begin_video_scrub(key, duration, bounds, &event, cx)
            }
            VideoPlayerEvent::ControlsHovered(hovered) => {
                self.hover_video_controls(key, hovered, cx)
            }
            VideoPlayerEvent::VolumeHovered(hovered) => self.hover_video_volume(key, hovered, cx),
            VideoPlayerEvent::VolumePopupHovered(hovered) => {
                self.hover_video_volume_popup(key, hovered, cx)
            }
            VideoPlayerEvent::ToggleMute => self.toggle_video_mute(key, cx),
            VideoPlayerEvent::VolumePressed { bounds, event } => {
                self.begin_video_volume_drag(key, bounds, &event, cx)
            }
            VideoPlayerEvent::ToggleTheater => self.toggle_video_theater(source, cx),
        }
    }

    fn play_video(&mut self, key: VideoKey, cx: &mut Context<Self>) {
        self.note_media_interaction(MediaPlaybackTarget::Video(key));
        self.show_video_controls(key, cx);
        let source_key = self.source_key(key.room_id, key.attachment_id);
        match self.video_sources.view(source_key) {
            VideoSourceView::Ready(source) => {
                self.pending_video_plays.remove(&key);
                self.videos.ensure_source(key, source);
                match self.videos.play(key) {
                    Ok(()) => self.status = "Starting attachment playback…".into(),
                    Err(error) => {
                        log::error!("embedded video play failed key={key:?}: {error:#}");
                        self.status = format!("Playback failed: {error}").into();
                    }
                }
            }
            VideoSourceView::Absent | VideoSourceView::Loading | VideoSourceView::Failed { .. } => {
                if let Some(descriptor) = self.video_descriptor(key) {
                    self.pending_video_plays.insert(key);
                    self.video_sources.promote(source_key, descriptor);
                    self.video_sources
                        .set_pin(source_key, VideoSourcePin::PendingPlay, true);
                    self.video_sources.retry(source_key);
                    self.pump_video_sources(cx);
                    self.status = "Preparing attachment playback…".into();
                } else {
                    self.status = "Video source is no longer available".into();
                }
            }
        }
        self.sync_video_source_pins();
        self.schedule_video_controls_hide(key, cx);
        cx.notify();
    }

    fn seek_video(&mut self, key: VideoKey, seconds: f64, cx: &mut Context<Self>) {
        self.note_media_interaction(MediaPlaybackTarget::Video(key));
        if let Err(error) = self.videos.seek(key, seconds) {
            log::error!("embedded video seek failed key={key:?} seconds={seconds}: {error:#}");
            self.status = format!("Seek failed: {error}").into();
        }
        self.show_video_controls(key, cx);
        self.schedule_video_controls_hide(key, cx);
        cx.notify();
    }

    fn show_video_controls(&mut self, key: VideoKey, cx: &mut Context<Self>) {
        if self.video_controls.active_key != Some(key) {
            self.video_controls_hide_task.take();
            self.video_volume_hide_task.take();
            self.video_volume_popup_bounds.set(None);
            self.video_volume_button_bounds.set(None);
        }
        let Some(serial) = self.video_controls.show(key) else {
            return;
        };
        self.video_controls_animation_task.take();
        self.video_controls_animation_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(CONTROLS_ANIMATION_DURATION)
                .await;
            let _ = this.update(cx, |this, cx| {
                this.video_controls_animation_task.take();
                if this.video_controls.finish_animation(serial) {
                    cx.notify();
                }
            });
        }));
        cx.notify();
    }

    fn hide_video_controls(&mut self, key: VideoKey, cx: &mut Context<Self>) {
        let Some(serial) = self.video_controls.hide(key) else {
            return;
        };
        self.video_controls_hide_task.take();
        self.video_volume_hide_task.take();
        self.video_volume_popup_bounds.set(None);
        self.video_controls_animation_task.take();
        self.video_controls_animation_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(CONTROLS_ANIMATION_DURATION)
                .await;
            let _ = this.update(cx, |this, cx| {
                this.video_controls_animation_task.take();
                if this.video_controls.finish_animation(serial) {
                    cx.notify();
                }
            });
        }));
        cx.notify();
    }

    fn video_controls_pinned(&self, key: VideoKey) -> bool {
        let video = self.videos.view(key);
        let dragging = self.video_scrub.is_some_and(|scrub| scrub.key == key)
            || self.video_volume_drag.is_some_and(|drag| drag.key == key);
        self.video_controls
            .pinned(key, video.paused, video.finished, dragging)
    }

    fn schedule_video_controls_hide(&mut self, key: VideoKey, cx: &mut Context<Self>) {
        self.video_controls_hide_task.take();
        if self.video_controls.active_key != Some(key) || self.video_controls_pinned(key) {
            return;
        }
        if !self.video_controls.player_hovered {
            self.hide_video_controls(key, cx);
            return;
        }
        self.video_controls_hide_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(CONTROLS_HIDE_DELAY).await;
            let _ = this.update(cx, |this, cx| {
                this.video_controls_hide_task.take();
                if this.video_controls.active_key == Some(key) && !this.video_controls_pinned(key) {
                    this.hide_video_controls(key, cx);
                }
            });
        }));
    }

    fn hover_video_player(&mut self, key: VideoKey, hovered: bool, cx: &mut Context<Self>) {
        self.show_video_controls(key, cx);
        self.video_controls.player_hovered = hovered;
        if hovered {
            self.schedule_video_controls_hide(key, cx);
        } else {
            self.video_controls.scrub_hover_fraction = None;
            self.schedule_video_controls_hide(key, cx);
        }
        cx.notify();
    }

    fn video_pointer_moved(&mut self, key: VideoKey, cx: &mut Context<Self>) {
        self.show_video_controls(key, cx);
        self.video_controls.player_hovered = true;
        self.schedule_video_controls_hide(key, cx);
    }

    fn hover_video_controls(&mut self, key: VideoKey, hovered: bool, cx: &mut Context<Self>) {
        self.show_video_controls(key, cx);
        self.video_controls.bar_hovered = hovered;
        if hovered {
            self.video_controls_hide_task.take();
        } else {
            self.schedule_video_controls_hide(key, cx);
        }
        cx.notify();
    }

    fn click_video_surface(
        &mut self,
        source: TheaterVideo,
        click_count: usize,
        unstarted: bool,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        if click_count >= 2 {
            self.video_surface_click_task.take();
            self.toggle_video_theater(source, cx);
            return;
        }
        if click_count != 1 {
            return;
        }
        if unstarted {
            self.video_surface_click_task.take();
            self.play_video(source.key, cx);
            return;
        }
        self.video_surface_click_task.take();
        let key = source.key;
        self.video_surface_click_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(220))
                .await;
            let _ = this.update(cx, |this, cx| {
                this.video_surface_click_task.take();
                this.play_video(key, cx);
            });
        }));
    }

    fn toggle_video_theater(&mut self, source: TheaterVideo, cx: &mut Context<Self>) {
        if self
            .theater_video
            .as_ref()
            .is_some_and(|active| active.key == source.key)
        {
            self.exit_video_theater(cx);
            return;
        }
        self.theater_video = Some(source.clone());
        let source_key = self.source_key(source.key.room_id, source.key.attachment_id);
        self.video_sources
            .promote(source_key, source.descriptor.clone());
        self.video_sources
            .set_pin(source_key, VideoSourcePin::Theater, true);
        self.pump_video_sources(cx);
        self.show_video_controls(source.key, cx);
        self.video_surface_click_task.take();
        cx.notify();
    }

    fn exit_video_theater(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(theater) = self.theater_video.take() else {
            return false;
        };
        self.video_surface_click_task.take();
        self.finish_video_scrub(cx);
        self.finish_video_volume_drag(cx);
        self.video_volume_popup_bounds.set(None);
        self.video_controls.player_hovered = false;
        self.schedule_video_controls_hide(theater.key, cx);
        self.sync_video_source_pins();
        self.pump_video_sources(cx);
        cx.notify();
        true
    }

    fn hover_video_scrub(&mut self, key: VideoKey, fraction: f64, cx: &mut Context<Self>) {
        self.show_video_controls(key, cx);
        if self.video_controls.scrub_hover_fraction != Some(fraction) {
            self.video_controls.scrub_hover_fraction = Some(fraction);
            cx.notify();
        }
    }

    fn clear_video_scrub_hover(&mut self, key: VideoKey, cx: &mut Context<Self>) {
        if self.video_controls.active_key == Some(key)
            && self.video_controls.scrub_hover_fraction.take().is_some()
        {
            cx.notify();
        }
    }

    fn begin_video_scrub(
        &mut self,
        key: VideoKey,
        duration: f64,
        bounds: Bounds<Pixels>,
        event: &MouseDownEvent,
        cx: &mut Context<Self>,
    ) {
        let Some(fraction) = horizontal_fraction(bounds, event.position.x, duration) else {
            return;
        };
        self.note_media_interaction(MediaPlaybackTarget::Video(key));
        self.show_video_controls(key, cx);
        self.video_controls_hide_task.take();
        self.video_scrub = Some(VideoScrub {
            key,
            bounds,
            duration,
            last_fraction: fraction,
            last_seek: Instant::now(),
        });
        if let Err(error) = self.videos.scrub(key, fraction, duration, SeekMode::Exact) {
            log::error!(
                "embedded video initial scrub failed key={key:?} fraction={fraction}: {error:#}"
            );
            self.status = format!("Seek failed: {error}").into();
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn drag_video_scrub(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) -> bool {
        let Some(mut scrub) = self.video_scrub else {
            return false;
        };
        if !event.dragging() {
            self.finish_video_scrub(cx);
            return true;
        }
        let Some(fraction) = horizontal_fraction(scrub.bounds, event.position.x, scrub.duration)
        else {
            self.finish_video_scrub(cx);
            return true;
        };
        if fraction != scrub.last_fraction {
            scrub.last_fraction = fraction;
            let dispatch_seek = scrub.should_dispatch_seek(Instant::now());
            self.video_scrub = Some(scrub);
            self.video_controls.scrub_hover_fraction = Some(fraction);
            if dispatch_seek
                && let Err(error) =
                    self.videos
                        .scrub(scrub.key, fraction, scrub.duration, SeekMode::Exact)
            {
                log::error!(
                    "embedded video drag scrub failed key={:?} fraction={fraction}: {error:#}",
                    scrub.key,
                );
                self.status = format!("Seek failed: {error}").into();
            }
            cx.notify();
        }
        cx.stop_propagation();
        true
    }

    fn finish_video_scrub(&mut self, cx: &mut Context<Self>) {
        let Some(scrub) = self.video_scrub.take() else {
            return;
        };
        self.schedule_video_controls_hide(scrub.key, cx);
        cx.notify();
    }

    fn hover_video_volume(&mut self, key: VideoKey, hovered: bool, cx: &mut Context<Self>) {
        self.show_video_controls(key, cx);
        self.video_controls.volume_button_hovered = hovered;
        if hovered {
            self.video_volume_hide_task.take();
            self.video_controls.volume_open = true;
            self.video_controls_hide_task.take();
        } else {
            self.schedule_video_volume_close(key, cx);
        }
        cx.notify();
    }

    fn hover_video_volume_popup(&mut self, key: VideoKey, hovered: bool, cx: &mut Context<Self>) {
        self.video_controls.volume_popup_hovered = hovered;
        if hovered {
            self.video_volume_hide_task.take();
            self.video_controls.volume_open = true;
        } else {
            self.schedule_video_volume_close(key, cx);
        }
        cx.notify();
    }

    fn schedule_video_volume_close(&mut self, key: VideoKey, cx: &mut Context<Self>) {
        self.video_volume_hide_task.take();
        self.video_volume_hide_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(160))
                .await;
            let _ = this.update(cx, |this, cx| {
                this.video_volume_hide_task.take();
                if this.video_controls.active_key == Some(key)
                    && !this.video_controls.volume_button_hovered
                    && !this.video_controls.volume_popup_hovered
                    && !this.video_volume_drag.is_some_and(|drag| drag.key == key)
                {
                    this.video_controls.volume_open = false;
                    this.video_volume_popup_bounds.set(None);
                    this.schedule_video_controls_hide(key, cx);
                    cx.notify();
                }
            });
        }));
    }

    fn set_video_volume(&mut self, key: VideoKey, volume: f64, cx: &mut Context<Self>) {
        self.note_media_interaction(MediaPlaybackTarget::Video(key));
        if let Err(error) = self.videos.set_volume_for(key, volume) {
            log::error!(
                "embedded video volume change failed key={key:?} volume={volume}: {error:#}"
            );
            self.status = format!("Volume failed: {error}").into();
        }
        self.show_video_controls(key, cx);
        cx.notify();
    }

    fn toggle_video_mute(&mut self, key: VideoKey, cx: &mut Context<Self>) {
        self.note_media_interaction(MediaPlaybackTarget::Video(key));
        if let Err(error) = self.videos.toggle_mute(key) {
            log::error!("embedded video mute toggle failed key={key:?}: {error:#}");
            self.status = format!("Volume failed: {error}").into();
        }
        self.show_video_controls(key, cx);
        self.video_controls.volume_open = true;
        cx.notify();
    }

    fn begin_video_volume_drag(
        &mut self,
        key: VideoKey,
        bounds: Bounds<Pixels>,
        event: &MouseDownEvent,
        cx: &mut Context<Self>,
    ) {
        let Some(fraction) = vertical_fraction(bounds, event.position.y) else {
            return;
        };
        self.video_volume_drag = Some(VideoVolumeDrag { key, bounds });
        self.video_controls.volume_open = true;
        self.video_controls_hide_task.take();
        self.set_video_volume(key, fraction * 100.0, cx);
        cx.stop_propagation();
    }

    fn drag_video_volume(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) -> bool {
        let Some(drag) = self.video_volume_drag else {
            return false;
        };
        if !event.dragging() {
            self.finish_video_volume_drag(cx);
            return true;
        }
        if let Some(fraction) = vertical_fraction(drag.bounds, event.position.y) {
            self.set_video_volume(drag.key, fraction * 100.0, cx);
        }
        cx.stop_propagation();
        true
    }

    fn finish_video_volume_drag(&mut self, cx: &mut Context<Self>) {
        let Some(drag) = self.video_volume_drag.take() else {
            return;
        };
        self.schedule_video_volume_close(drag.key, cx);
        cx.notify();
    }

    fn scroll_video_volume(&mut self, event: &ScrollWheelEvent, cx: &mut Context<Self>) -> bool {
        let Some(key) = self.video_controls.active_key else {
            return false;
        };
        if !self.video_controls.volume_open
            || !self
                .video_volume_popup_bounds
                .get()
                .is_some_and(|bounds| bounds.contains(&event.position))
                && !self
                    .video_volume_button_bounds
                    .get()
                    .is_some_and(|bounds| bounds.contains(&event.position))
        {
            return false;
        }
        let delta = volume_scroll_delta(event.delta);
        if delta == 0.0 {
            return false;
        }
        self.adjust_video_volume(key, delta, cx);
        self.video_controls.volume_open = true;
        self.video_controls_hide_task.take();
        true
    }

    fn adjust_video_volume(&mut self, key: VideoKey, delta: f64, cx: &mut Context<Self>) {
        self.note_media_interaction(MediaPlaybackTarget::Video(key));
        if let Err(error) = self.videos.adjust_volume(key, delta) {
            log::error!("embedded video volume change failed key={key:?} delta={delta}: {error:#}");
            self.status = format!("Volume failed: {error}").into();
        }
        self.show_video_controls(key, cx);
        cx.notify();
    }

    fn note_media_interaction(&mut self, target: MediaPlaybackTarget) {
        self.media_interactions
            .retain(|candidate| *candidate != target);
        self.media_interactions.push_front(target);
        self.media_interactions.truncate(256);
    }

    fn last_visible_media_interaction(&self) -> Option<MediaPlaybackTarget> {
        latest_visible_media(
            &self.media_interactions,
            &self.visible_audio_keys,
            &self.visible_video_keys,
        )
    }

    fn toggle_playback(&mut self, _: &TogglePlayback, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(key) = self.theater_video.as_ref().map(|theater| theater.key) {
            self.play_video(key, cx);
            return;
        }
        match self.last_visible_media_interaction() {
            Some(MediaPlaybackTarget::Audio(key)) => self.play_audio(key, cx),
            Some(MediaPlaybackTarget::Video(key)) => self.play_video(key, cx),
            None => {}
        }
    }
    fn seek_back(&mut self, _: &SeekBack, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(stream_id) = self.fullscreen_share {
            self.pan_live_view(stream_id, 30.0, 0.0, cx);
        } else if let Some(theater) = self.theater_video.as_ref() {
            self.seek_video(theater.key, -10.0, cx);
        } else {
            match self.last_visible_media_interaction() {
                Some(MediaPlaybackTarget::Audio(key)) => self.seek_audio(key, -10.0, cx),
                Some(MediaPlaybackTarget::Video(key)) => self.seek_video(key, -10.0, cx),
                None => {}
            }
        }
    }
    fn seek_forward(&mut self, _: &SeekForward, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(stream_id) = self.fullscreen_share {
            self.pan_live_view(stream_id, -30.0, 0.0, cx);
        } else if let Some(theater) = self.theater_video.as_ref() {
            self.seek_video(theater.key, 10.0, cx);
        } else {
            match self.last_visible_media_interaction() {
                Some(MediaPlaybackTarget::Audio(key)) => self.seek_audio(key, 10.0, cx),
                Some(MediaPlaybackTarget::Video(key)) => self.seek_video(key, 10.0, cx),
                None => {}
            }
        }
    }

    fn render_live_shares(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Div {
        let settings = AppliedSettings::get(cx);
        let shares = self.model.live_shares.clone();
        let resizable = !self.live_players.is_empty();
        let pane_height = resizable
            .then_some(self.live_pane_height)
            .flatten()
            .map(|height| {
                clamp_live_pane_height(height, window.viewport_size().height, window.rem_size())
            });
        if resizable {
            self.live_pane_height = pane_height;
        }
        let constrained = pane_height.is_some();
        let pane_bounds = self.live_pane_bounds.clone();
        let mut panel = div()
            .relative()
            .flex_none()
            .flex()
            .flex_col()
            .min_h_0()
            .overflow_hidden()
            .gap_0()
            .border_b_1()
            .border_color(settings.theme.color(ThemeRole::BorderSubtle))
            .bg(settings.theme.color(ThemeRole::Raised))
            .when_some(pane_height, |panel, height| panel.h(height))
            .child(
                canvas(
                    move |bounds, _, _| pane_bounds.set(Some(bounds)),
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            );
        for share in shares {
            panel = panel.child(self.render_live_share_card(share, false, constrained, cx));
        }
        if resizable {
            panel = panel.child(
                div()
                    .id("live-pane-resize")
                    .absolute()
                    .left_0()
                    .right_0()
                    .bottom_0()
                    .h(rems_from_px(LIVE_PANE_DIVIDER_SIZE))
                    .cursor_row_resize()
                    .hover({
                        let hover = settings.theme.color(ThemeRole::StateSelection);
                        move |handle| handle.bg(hover)
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, event: &MouseDownEvent, window, cx| {
                            this.begin_live_pane_resize(event, window, cx)
                        }),
                    )
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this, _: &MouseUpEvent, _, cx| {
                            this.finish_live_pane_resize(cx)
                        }),
                    )
                    .on_mouse_up_out(
                        MouseButton::Left,
                        cx.listener(|this, _: &MouseUpEvent, _, cx| {
                            this.finish_live_pane_resize(cx)
                        }),
                    ),
            );
        }
        panel
    }

    fn render_live_share_card(
        &mut self,
        share: local_rpc::model::LiveShare,
        fullscreen: bool,
        constrained: bool,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let settings = AppliedSettings::get(cx);
        let stream_id = share.stream_id;
        let active = self.live_players.get(&stream_id).map(|view| {
            let viewport_bounds = view.viewport_bounds.clone();
            (
                view.player.surface(),
                view.zoom,
                view.pan,
                view.last_mouse_position.is_some(),
                viewport_bounds,
                view.coded_size,
            )
        });
        let active_share = active.is_some();
        let mut card = div()
            .id(("live-share", stream_id.0 as usize))
            .flex()
            .flex_col()
            .bg(settings.theme.color(ThemeRole::MediaViewport))
            .when(fullscreen, |card| card.size_full())
            .when(constrained && active_share, |card| card.flex_1().min_h_0())
            .when(constrained && !active_share, |card| card.flex_none());
        if let Some((video_surface, zoom, pan, dragging, viewport_bounds, coded_size)) = active {
            let stop_id = stream_id;
            let reset_id = stream_id;
            let zoom_out_id = stream_id;
            let zoom_in_id = stream_id;
            let fullscreen_id = stream_id;
            card = card
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap_1()
                        .px_3()
                        .py_2()
                        .bg(settings.theme.color(ThemeRole::Window))
                        .child(live_share_title(&share, &settings.theme))
                        .child(
                            icon_button(
                                ("live-stop", stream_id.0 as usize),
                                IconName::Stop,
                                &settings.theme,
                            )
                            .on_click(cx.listener(
                                move |this, _, window, cx| {
                                    if this.fullscreen_share == Some(stop_id)
                                        && window.is_fullscreen()
                                    {
                                        window.toggle_fullscreen();
                                    }
                                    this.stop_live_share(stop_id, cx)
                                },
                            )),
                        )
                        .child(
                            icon_button(
                                ("live-reset", stream_id.0 as usize),
                                IconName::RotateCcw,
                                &settings.theme,
                            )
                            .on_click(
                                cx.listener(move |this, _, _, cx| {
                                    this.reset_live_view(reset_id, cx)
                                }),
                            ),
                        )
                        .child(
                            icon_button(
                                ("live-zoom-out", stream_id.0 as usize),
                                IconName::ZoomOut,
                                &settings.theme,
                            )
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.zoom_live_view(zoom_out_id, 1.0 / 1.25, cx)
                                },
                            )),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(settings.theme.color(ThemeRole::TextMuted))
                                .child(format!("{:.0}%", zoom * 100.0)),
                        )
                        .child(
                            icon_button(
                                ("live-zoom-in", stream_id.0 as usize),
                                IconName::ZoomIn,
                                &settings.theme,
                            )
                            .on_click(cx.listener(
                                move |this, _, _, cx| this.zoom_live_view(zoom_in_id, 1.25, cx),
                            )),
                        )
                        .child(
                            icon_button(
                                ("live-fullscreen", stream_id.0 as usize),
                                if fullscreen {
                                    IconName::Minimize
                                } else {
                                    IconName::Maximize
                                },
                                &settings.theme,
                            )
                            .on_click(cx.listener(
                                move |this, _, window, cx| {
                                    this.toggle_live_fullscreen(fullscreen_id, window, cx)
                                },
                            )),
                        ),
                )
                .child({
                    let scroll_id = stream_id;
                    let down_id = stream_id;
                    let move_id = stream_id;
                    let up_id = stream_id;
                    let pinch_id = stream_id;
                    div()
                        .relative()
                        .overflow_hidden()
                        .w_full()
                        .when(fullscreen || constrained, |viewport| {
                            viewport.flex_1().min_h_0()
                        })
                        .when(!fullscreen && !constrained, |viewport| {
                            viewport.h(rems_from_px(320.))
                        })
                        .bg(settings.theme.color(ThemeRole::MediaViewport))
                        .cursor(if dragging {
                            gpui::CursorStyle::ClosedHand
                        } else {
                            gpui::CursorStyle::OpenHand
                        })
                        .on_scroll_wheel(cx.listener(
                            move |this, event: &ScrollWheelEvent, _, cx| {
                                this.scroll_live_view(scroll_id, event, cx)
                            },
                        ))
                        .on_pinch(cx.listener(move |this, event: &PinchEvent, _, cx| {
                            this.pinch_live_view(pinch_id, event, cx)
                        }))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                                this.live_mouse_down(down_id, event, cx)
                            }),
                        )
                        .on_mouse_move(cx.listener(move |this, event: &MouseMoveEvent, _, cx| {
                            this.live_mouse_move(move_id, event, cx)
                        }))
                        .on_mouse_up(
                            MouseButton::Left,
                            cx.listener(move |this, _: &MouseUpEvent, _, cx| {
                                this.live_mouse_up(up_id, cx)
                            }),
                        )
                        .child(
                            canvas(
                                move |bounds, _, _| {
                                    viewport_bounds.set(Some(bounds));
                                    let pan = clamp_live_pan(pan, coded_size, bounds, zoom);
                                    LiveVideoGeometry::new(coded_size, bounds, zoom, pan)
                                },
                                move |_, geometry, window, _| {
                                    if let Some(geometry) = geometry {
                                        window
                                            .paint_platform_surface(geometry.bounds, video_surface);
                                    }
                                },
                            )
                            .absolute()
                            .size_full(),
                        )
                });
        } else {
            card = card.child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_1()
                    .px_3()
                    .py_2()
                    .bg(settings.theme.color(ThemeRole::Window))
                    .child(live_share_title(&share, &settings.theme))
                    .child(
                        icon_button(
                            ("live-play", stream_id.0 as usize),
                            IconName::Play,
                            &settings.theme,
                        )
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.start_live_share(stream_id, cx)),
                        ),
                    ),
            );
        }
        card
    }

    fn render_completion_popup(
        &mut self,
        view: CompletionView,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let settings = AppliedSettings::get(cx);
        let emoji_context = matches!(&view.context, CompletionContext::Emoji { .. });
        let active = self
            .completion_session
            .as_ref()
            .and_then(|session| session.active.clone());
        let mut rows = div().w_full().flex().flex_col();

        for (index, option) in view.options.into_iter().enumerate() {
            let selected = active.as_ref() == Some(&option.key);
            let hover_key = option.key.clone();
            let accept_key = option.key.clone();
            let content = match &option.value {
                CompletionValue::Command(command) => div()
                    .w_full()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        highlighted_completion_label(
                            command.name.clone(),
                            &option.match_ranges,
                            selected,
                            &settings.theme,
                        )
                        .max_w(rems_from_px(138.))
                        .min_w_0()
                        .flex_1(),
                    )
                    .child(
                        div()
                            .max_w(rems_from_px(190.))
                            .min_w_0()
                            .flex_1()
                            .truncate()
                            .text_xs()
                            .text_color(settings.theme.color(if selected {
                                ThemeRole::TextInverse
                            } else {
                                ThemeRole::TextSecondary
                            }))
                            .child(command.usage.clone()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_xs()
                            .text_color(settings.theme.color(if selected {
                                ThemeRole::TextInverse
                            } else {
                                ThemeRole::TextDim
                            }))
                            .child(command.description.clone()),
                    ),
                CompletionValue::Candidate { kind, item } => div()
                    .w_full()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .max_w(rems_from_px(72.))
                            .flex_none()
                            .text_xs()
                            .text_color(settings.theme.color(if selected {
                                ThemeRole::TextInverse
                            } else {
                                ThemeRole::TextSubtle
                            }))
                            .child(candidate_kind_label(*kind)),
                    )
                    .child(
                        highlighted_completion_label(
                            item.value.clone(),
                            &option.match_ranges,
                            selected,
                            &settings.theme,
                        )
                        .max_w(rems_from_px(220.))
                        .min_w_0()
                        .flex_1(),
                    )
                    .when_some(item.detail.clone(), |row, detail| {
                        row.child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_xs()
                                .text_color(settings.theme.color(if selected {
                                    ThemeRole::TextInverse
                                } else {
                                    ThemeRole::TextDim
                                }))
                                .child(detail),
                        )
                    }),
                CompletionValue::Emoji(record) => div()
                    .w_full()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .w(rems_from_px(48.))
                            .flex_none()
                            .text_xl()
                            .child(record.unicode.clone()),
                    )
                    .child(
                        div()
                            .max_w(rems_from_px(220.))
                            .min_w_0()
                            .flex_1()
                            .truncate()
                            .text_sm()
                            .text_color(settings.theme.color(if selected {
                                ThemeRole::TextInverse
                            } else {
                                ThemeRole::TextPrimary
                            }))
                            .child(record.label.clone()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_xs()
                            .text_color(settings.theme.color(if selected {
                                ThemeRole::TextInverse
                            } else {
                                ThemeRole::TextDim
                            }))
                            .child(format!(":{}:", record.shortcode)),
                    ),
            };
            rows = rows.child(
                div()
                    .id(("completion-option", index))
                    .min_h(rems_from_px(40.))
                    .w_full()
                    .flex_none()
                    .flex()
                    .items_center()
                    .px_3()
                    .border_b_1()
                    .border_color(settings.theme.color(if selected {
                        ThemeRole::BorderFocus
                    } else {
                        ThemeRole::BorderStrong
                    }))
                    .bg(settings.theme.color(if selected {
                        ThemeRole::ControlActive
                    } else {
                        ThemeRole::Raised
                    }))
                    .hover({
                        let hover = settings.theme.color(if selected {
                            ThemeRole::ControlActive
                        } else {
                            ThemeRole::StateHover
                        });
                        move |row| row.bg(hover)
                    })
                    .on_mouse_move(cx.listener(move |this, _: &MouseMoveEvent, _, cx| {
                        this.hover_completion(hover_key.clone(), cx)
                    }))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.accept_completion_key(accept_key.clone(), window, cx)
                    }))
                    .child(content),
            );
        }

        if let Some(hint) = view.hint {
            rows = rows.child(
                div()
                    .min_h(rems_from_px(40.))
                    .w_full()
                    .flex_none()
                    .flex()
                    .items_center()
                    .px_3()
                    .border_b_1()
                    .border_color(settings.theme.color(ThemeRole::BorderStrong))
                    .text_sm()
                    .text_color(settings.theme.color(ThemeRole::TextMuted))
                    .child(hint),
            );
        }

        div()
            .id("command-completion")
            .absolute()
            .left(rems_from_px(79.))
            .right(rems_from_px(79.))
            .bottom(relative(1.))
            .mb(rems_from_px(6.))
            .border_1()
            .border_color(settings.theme.color(ThemeRole::BorderStrong))
            .bg(settings.theme.color(ThemeRole::Raised))
            .child(rows)
            .child(
                div()
                    .min_h(rems_from_px(28.))
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_3()
                    .bg(settings.theme.color(ThemeRole::Sidebar))
                    .text_xs()
                    .text_color(settings.theme.color(ThemeRole::TextDim))
                    .child("↑↓ navigate  ·  Tab complete")
                    .child(if emoji_context {
                        "Enter insert  ·  Esc close"
                    } else {
                        "Enter run  ·  Esc close"
                    }),
            )
            .with_animation(
                ("command-completion-in", 0usize),
                Animation::new(Duration::from_millis(90)).with_easing(gpui::ease_out_quint()),
                |popup, delta| popup.opacity(delta).mb(rems_from_px(2. + 4. * delta)),
            )
            .into_any_element()
    }

    fn render_server_selector(&mut self, cx: &mut Context<Self>) -> Stateful<Div> {
        let applied = AppliedSettings::get(cx);
        let servers = self.filtered_servers(cx);
        if self.selected_server_index(&servers).is_none() {
            self.selected_server_label = servers.first().map(|server| server.label.clone());
        }
        let configured_empty = self.model.server_selection.servers.is_empty();
        let query_empty = self.server_search_input.read(cx).text().trim().is_empty();
        let active_server = self.model.active_server.clone();
        let pending_target = self.server_selection_target.clone();
        let prompt = self.model.server_selection.prompt.clone();
        let mut list = div()
            .id("server-list")
            .flex_1()
            .min_h_0()
            .w_full()
            .flex()
            .flex_col()
            .gap_2()
            .overflow_y_scroll()
            .track_scroll(&self.server_list_scroll);

        if configured_empty {
            list = list.child(
                div()
                    .flex_1()
                    .min_h(rems_from_px(220.))
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap_3()
                    .text_center()
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("No servers are configured yet"),
                    )
                    .child(
                        div()
                            .max_w(rems_from_px(520.))
                            .text_sm()
                            .text_color(applied.theme.color(ThemeRole::TextMuted))
                            .child(
                                "Pair with a server using `chatt pair JOIN_STRING` in a terminal. \
                                 Saved servers appear here through the running daemon.",
                            ),
                    ),
            );
        } else if servers.is_empty() && !query_empty {
            list = list.child(
                div()
                    .flex_1()
                    .min_h(rems_from_px(180.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_sm()
                    .text_color(applied.theme.color(ThemeRole::TextMuted))
                    .child("No saved servers match this search."),
            );
        } else {
            for (index, server) in servers.into_iter().enumerate() {
                let selected = self.selected_server_label.as_deref() == Some(server.label.as_str());
                let current = self.model.server_selection.error.is_none()
                    && active_server.as_deref() == Some(server.label.as_str());
                let pending = pending_target.as_deref() == Some(server.label.as_str());
                let connectable = server.availability == ServerAvailability::Ready;
                let row_server = server.clone();
                let hover = applied.theme.color(ThemeRole::ControlSurfaceHover);
                let mut row = div()
                    .id(("server-row", index))
                    .w_full()
                    .px_4()
                    .py_3()
                    .flex()
                    .items_center()
                    .gap_4()
                    .border_1()
                    .border_color(applied.theme.color(if selected {
                        ThemeRole::BorderFocus
                    } else if current {
                        ThemeRole::StateSuccess
                    } else {
                        ThemeRole::BorderSubtle
                    }))
                    .bg(applied.theme.color(ThemeRole::ControlSurface))
                    .when(connectable, |row| {
                        row.cursor_pointer()
                            .hover(move |row| row.bg(hover))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.selected_server_label = Some(row_server.label.clone());
                                this.select_server(row_server.clone(), window, cx)
                            }))
                    })
                    .child(
                        div()
                            .w(rems_from_px(28.))
                            .flex_none()
                            .text_center()
                            .text_color(applied.theme.color(if selected {
                                ThemeRole::TextPrimary
                            } else {
                                ThemeRole::TextMuted
                            }))
                            .child(if selected { "›" } else { " " }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        div()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child(server.label.clone()),
                                    )
                                    .when(current, |line| {
                                        line.child(
                                            div()
                                                .text_xs()
                                                .text_color(
                                                    applied.theme.color(ThemeRole::StateSuccess),
                                                )
                                                .child("Current"),
                                        )
                                    })
                                    .when(pending, |line| {
                                        line.child(
                                            div()
                                                .text_xs()
                                                .text_color(
                                                    applied.theme.color(ThemeRole::StateWarning),
                                                )
                                                .child("Connecting…"),
                                        )
                                    }),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(applied.theme.color(ThemeRole::TextSecondary))
                                    .child(server.username.clone()),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(applied.theme.color(ThemeRole::TextMuted))
                                    .child(server.tcp_addr.clone()),
                            ),
                    );
                row = if !connectable {
                    row.child(
                        div()
                            .max_w(rems_from_px(190.))
                            .text_right()
                            .text_xs()
                            .text_color(applied.theme.color(ThemeRole::StateWarning))
                            .child("Finish pairing in the terminal client"),
                    )
                } else if !server.require_transport_encryption {
                    row.child(
                        div()
                            .max_w(rems_from_px(190.))
                            .text_right()
                            .text_xs()
                            .text_color(applied.theme.color(ThemeRole::StateWarning))
                            .child("Transport encryption not required"),
                    )
                } else {
                    row
                };
                list = list.child(row);
            }
        }

        let can_cancel = active_server.is_some()
            && self.model.server_selection.error.is_none()
            && prompt.is_none()
            && self.pending_server_selection.is_none()
            && self.pending_server_prompt.is_none();
        let mut root = div()
            .id("chatt-server-selector")
            .key_context("Chatt ChattServerSelector")
            .on_action(cx.listener(Self::open_settings))
            .on_action(cx.listener(Self::server_next))
            .on_action(cx.listener(Self::server_previous))
            .on_action(cx.listener(Self::server_activate))
            .on_action(cx.listener(Self::close_server_selector))
            .size_full()
            .flex()
            .flex_col()
            .font_family(applied.fonts.interface_family.clone())
            .bg(applied.theme.color(ThemeRole::Window))
            .text_color(applied.theme.color(ThemeRole::TextPrimary))
            .child(
                div()
                    .min_h(rems_from_px(TOP_BAR_HEIGHT))
                    .flex_none()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_3()
                    .px_4()
                    .border_b_1()
                    .border_color(applied.theme.color(ThemeRole::BorderSubtle))
                    .bg(applied.theme.color(ThemeRole::Toolbar))
                    .child(div().font_weight(FontWeight::BOLD).child("Servers"))
                    .child(div().flex_1())
                    .child(
                        toolbar_button(
                            "server-selector-settings",
                            None,
                            "Settings",
                            &applied.theme,
                        )
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.open_settings(&OpenSettings, window, cx)
                        })),
                    )
                    .when(can_cancel, |bar| {
                        bar.child(
                            toolbar_button(
                                "server-selector-cancel",
                                Some(IconName::Close),
                                "Back to chat",
                                &applied.theme,
                            )
                            .on_click(cx.listener(
                                |this, _, window, cx| {
                                    this.close_server_selector(&CloseServerSelector, window, cx)
                                },
                            )),
                        )
                    }),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .flex()
                    .justify_center()
                    .p_6()
                    .child(
                        div()
                            .w_full()
                            .max_w(rems_from_px(760.))
                            .h_full()
                            .flex()
                            .flex_col()
                            .gap_4()
                            .child(
                                div()
                                    .child(div().text_2xl().font_weight(FontWeight::BOLD).child(
                                        if active_server.is_some()
                                            && self.model.server_selection.error.is_none()
                                        {
                                            "Switch server"
                                        } else {
                                            "Choose a server"
                                        },
                                    ))
                                    .child(
                                        div()
                                            .mt_1()
                                            .text_sm()
                                            .text_color(applied.theme.color(ThemeRole::TextMuted))
                                            .child(
                                                "Select a server saved in Chatt. The daemon owns \
                                                 the connection and shares it across clients.",
                                            ),
                                    ),
                            )
                            .child(
                                div()
                                    .min_h(rems_from_px(36.))
                                    .w_full()
                                    .flex_none()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .px_3()
                                    .border_1()
                                    .border_color(applied.theme.color(ThemeRole::BorderStrong))
                                    .bg(applied.theme.color(ThemeRole::Input))
                                    .child(icon(
                                        IconName::Search,
                                        15.0,
                                        applied.theme.color(ThemeRole::TextMuted),
                                    ))
                                    .child(
                                        div()
                                            .min_w_0()
                                            .flex_1()
                                            .text_sm()
                                            .child(self.server_search_input.clone()),
                                    ),
                            )
                            .when_some(self.model.server_selection.error.clone(), |panel, error| {
                                panel.child(
                                    div()
                                        .px_3()
                                        .py_2()
                                        .bg(applied.theme.color(ThemeRole::ControlSurface))
                                        .text_sm()
                                        .text_color(applied.theme.color(ThemeRole::StateDanger))
                                        .child(error.message),
                                )
                            })
                            .child(list)
                            .child(
                                div()
                                    .flex_none()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .text_xs()
                                    .text_color(applied.theme.color(ThemeRole::TextMuted))
                                    .child("↑/↓ select · Enter connect · type to search")
                                    .child(self.status.clone()),
                            ),
                    ),
            );

        if let Some(ServerSelectionPrompt::AllowUnencryptedTransport { label, .. }) = prompt {
            let resolving = self.pending_server_prompt.is_some();
            root = root.child(
                div()
                    .absolute()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(applied.theme.color(ThemeRole::MediaViewport))
                    .child(
                        div()
                            .w(rems_from_px(620.))
                            .max_w_full()
                            .mx_4()
                            .p_5()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .border_1()
                            .border_color(applied.theme.color(ThemeRole::StateDanger))
                            .bg(applied.theme.color(ThemeRole::Window))
                            .child(
                                div()
                                    .text_lg()
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(
                                        applied.theme.color(ThemeRole::StateDanger),
                                    )
                                    .child("Transport encryption disabled"),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .child(
                                        "The server disabled transport encryption. Control, \
                                         media, video, and file payloads will travel in \
                                         plaintext. Connect only if this is intentional and you \
                                         trust the network path.",
                                    ),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(
                                        applied.theme.color(ThemeRole::TextMuted),
                                    )
                                    .child(format!(
                                        "Accepting saves require-transport-encryption = false for {label}."
                                    )),
                            )
                            .child(
                                div()
                                    .mt_2()
                                    .flex()
                                    .justify_end()
                                    .gap_2()
                                    .child(
                                        compact_action_button(
                                            "server-prompt-cancel",
                                            None,
                                            "Cancel",
                                            &applied.theme,
                                        )
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.resolve_server_prompt(false, cx)
                                        })),
                                    )
                                    .child(
                                        compact_action_button(
                                            "server-prompt-accept",
                                            None,
                                            if resolving {
                                                "Working…"
                                            } else {
                                                "Connect anyway"
                                            },
                                            &applied.theme,
                                        )
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.resolve_server_prompt(true, cx)
                                        })),
                                    ),
                            ),
                    ),
            );
        }
        root
    }

    fn render_sidebar(&mut self, cx: &mut Context<Self>) -> Div {
        let applied = AppliedSettings::get(cx);
        let mut sidebar = div()
            .w(rems_from_px(SIDEBAR_WIDTH))
            .max_w(relative(0.5))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .border_r_1()
            .border_color(applied.theme.color(ThemeRole::BorderSubtle))
            .bg(applied.theme.color(ThemeRole::Sidebar))
            .child(
                div()
                    .min_h(rems_from_px(TOP_BAR_HEIGHT))
                    .flex_none()
                    .flex()
                    .items_center()
                    .px_4()
                    .border_b_1()
                    .border_color(applied.theme.color(ThemeRole::BorderSubtle))
                    .font_weight(FontWeight::BOLD)
                    .child("Rooms"),
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
                room_button(
                    ("room", room.id.0 as usize),
                    sigil,
                    label,
                    active,
                    unread,
                    &applied.theme,
                )
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
                    .text_color(applied.theme.color(ThemeRole::TextSubtle))
                    .child("Transfers"),
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
                        .text_color(applied.theme.color(ThemeRole::TextSecondary))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .child(format!("{} · {percent}%", transfer.file_name)),
                        )
                        .child(
                            mini_button(
                                ("cancel-transfer", transfer_id.0 as usize),
                                "×",
                                &applied.theme,
                            )
                            .on_click(cx.listener(
                                move |this, _, _, cx| this.cancel_file_transfer(transfer_id, cx),
                            )),
                        ),
                );
            }
        }
        let identity = self
            .model
            .local_identity
            .clone()
            .unwrap_or_else(|| "No identity".into());
        sidebar.child(div().flex_1()).child(
            div()
                .min_h(rems_from_px(MIN_COMPOSER_HEIGHT))
                .flex_none()
                .flex()
                .items_center()
                .pl_3()
                .border_t_1()
                .border_color(applied.theme.color(ThemeRole::BorderSubtle))
                .child(
                    div()
                        .size(rems_from_px(30.))
                        .mr_3()
                        .flex()
                        .items_center()
                        .justify_center()
                        .bg(applied.theme.color(ThemeRole::ParticipantIdentitySurface))
                        .text_color(applied.theme.color(ThemeRole::ParticipantIdentityText))
                        .child(identity.chars().next().unwrap_or('?').to_string()),
                )
                .child(div().flex_1().min_w_0().text_sm().child(identity))
                .child(
                    sidebar_footer_button("open-servers", "⇄", &applied.theme).on_click(
                        cx.listener(|this, _, window, cx| this.open_server_selector(window, cx)),
                    ),
                )
                .child(
                    sidebar_footer_button("open-settings", "⚙", &applied.theme).on_click(
                        cx.listener(|this, _, window, cx| {
                            this.open_settings(&OpenSettings, window, cx)
                        }),
                    ),
                ),
        )
    }

    fn render_composer_menu(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let applied = AppliedSettings::get(cx);
        let row_hover = applied.theme.color(ThemeRole::ControlSurfaceHover);
        let muted = applied.theme.color(ThemeRole::TextMuted);
        let menu_row = |id: &'static str, label: &'static str, value: &'static str| {
            div()
                .id(id)
                .min_h(rems_from_px(34.))
                .w_full()
                .px_3()
                .flex()
                .items_center()
                .gap_3()
                .cursor_pointer()
                .hover(move |row| row.bg(row_hover))
                .child(div().flex_1().child(label))
                .child(div().text_xs().text_color(muted).child(value))
        };

        deferred(
            div()
                .id("composer-menu-popup")
                .absolute()
                .right_0()
                .bottom(relative(1.))
                .mb(rems_from_px(8.))
                .w(rems_from_px(224.))
                .py_1()
                .border_1()
                .border_color(applied.theme.color(ThemeRole::BorderStrong))
                .bg(applied.theme.color(ThemeRole::Raised))
                .shadow_lg()
                .occlude()
                .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                    if !*hovered && this.composer_menu_action_taken {
                        this.dismiss_composer_menu(cx);
                    }
                }))
                .on_mouse_down_out(cx.listener(|this, event: &MouseDownEvent, _, cx| {
                    if this
                        .composer_menu_trigger_bounds
                        .get()
                        .is_some_and(|bounds| bounds.contains(&event.position))
                    {
                        return;
                    }
                    this.dismiss_composer_menu(cx);
                }))
                .child(
                    menu_row(
                        "composer-menu-rooms",
                        "Rooms sidebar",
                        if self.show_rooms_sidebar {
                            "Shown"
                        } else {
                            "Hidden"
                        },
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_rooms_sidebar(cx))),
                )
                .child(
                    menu_row(
                        "composer-menu-status",
                        "Top status bar",
                        if self.show_top_status_bar {
                            "Shown"
                        } else {
                            "Hidden"
                        },
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_top_status_bar(cx))),
                )
                .child(
                    div()
                        .mx_2()
                        .my_1()
                        .border_t_1()
                        .border_color(applied.theme.color(ThemeRole::BorderSubtle)),
                )
                .child(
                    menu_row("composer-menu-servers", "Switch server", "").on_click(
                        cx.listener(|this, _, window, cx| this.open_server_selector(window, cx)),
                    ),
                )
                .child(
                    menu_row("composer-menu-settings", "Settings", "").on_click(cx.listener(
                        |this, _, window, cx| this.open_settings(&OpenSettings, window, cx),
                    )),
                ),
        )
        .into_any_element()
    }

    fn scroll_timeline(
        &mut self,
        event: &ScrollWheelEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.scroll_video_volume(event, cx) {
            return true;
        }
        let distance = -event.delta.pixel_delta(px(20.)).y;
        if distance == px(0.) {
            return false;
        }
        self.message_reference_hover = None;
        self.message_reference_hover_task = None;

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

    fn autoscroll_timeline_selection(&mut self, distance: gpui::Pixels, cx: &mut Context<Self>) {
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

    fn render_queued_files(&self, cx: &mut Context<Self>) -> Div {
        let settings = AppliedSettings::get(cx);
        let mut row = div()
            .w_full()
            .flex()
            .flex_wrap()
            .gap_2()
            .pl(rems_from_px(51.))
            .pr_2()
            .pt_2()
            .pb_2();
        for file in self.queued_files.files() {
            let id = file.id;
            row = row.child(
                div()
                    .id(("queued-file", id as usize))
                    .max_w(rems_from_px(260.))
                    .min_h(rems_from_px(28.))
                    .px_2()
                    .flex()
                    .items_center()
                    .gap_2()
                    .border_1()
                    .border_color(settings.theme.color(ThemeRole::BorderStrong))
                    .bg(settings.theme.color(ThemeRole::Window))
                    .text_sm()
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .truncate()
                            .child(file.file_name.clone()),
                    )
                    .child(
                        div()
                            .id(("remove-queued-file", id as usize))
                            .size(rems_from_px(18.))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .cursor_pointer()
                            .text_color(settings.theme.color(ThemeRole::TextMuted))
                            .hover({
                                let hover = settings.theme.color(ThemeRole::ControlActive);
                                let text = settings.theme.color(ThemeRole::ControlActiveText);
                                move |button| button.bg(hover).text_color(text)
                            })
                            .child(icon(
                                IconName::Close,
                                13.0,
                                settings.theme.color(ThemeRole::TextSecondary),
                            ))
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.remove_queued_file(id, cx)),
                            ),
                    ),
            );
        }
        row
    }

    fn render_message_reference_preview(
        &mut self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let hover = self
            .message_reference_hover
            .as_ref()
            .filter(|hover| hover.visible)?;
        let target = hover.target;
        let anchor = hover.anchor;
        let message = hover.message.clone();
        let formatted = hover.formatted.clone();
        let missing = hover.missing;
        let applied = AppliedSettings::get(cx);
        let room_name = self
            .model
            .rooms
            .iter()
            .find(|room| room.id == target.room_id)
            .map(|room| room.name.clone())
            .unwrap_or_else(|| "Unknown room".into());
        let attachment = message
            .as_ref()
            .and_then(|message| message.attachment.as_ref())
            .map(|attachment| {
                self.render_message_reference_attachment(target, attachment.clone(), window, cx)
            });
        let card = div()
            .w(rems_from_px(400.))
            .max_w(rems_from_px(400.))
            .max_h(rems_from_px(480.))
            .overflow_hidden()
            .p_3()
            .border_1()
            .border_color(applied.theme.color(ThemeRole::BorderStrong))
            .bg(applied.theme.color(ThemeRole::Raised))
            .shadow_lg()
            .text_color(applied.theme.color(ThemeRole::TextBody))
            .child(
                div()
                    .mb_2()
                    .flex()
                    .items_center()
                    .gap_2()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .when_some(message.as_ref(), |header, message| {
                        header.child(message.sender.clone())
                    })
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::NORMAL)
                            .text_color(applied.theme.color(ThemeRole::TextDim))
                            .child(format!("in {room_name}")),
                    ),
            )
            .when_some(
                message.as_ref().zip(formatted.as_ref()),
                |card, (message, formatted)| {
                    let _ = message;
                    card.child(FormattedMessageElement::new(formatted.clone()))
                },
            )
            .when_some(attachment, |card, attachment| card.child(attachment))
            .when(message.is_none(), |card| {
                card.child(
                    div()
                        .text_sm()
                        .text_color(applied.theme.color(ThemeRole::TextMuted))
                        .child(if missing {
                            "Referenced message is not available"
                        } else {
                            "Loading referenced message…"
                        }),
                )
            });
        let position = point(
            anchor.origin.x,
            anchor.origin.y - crate::ui_scale::scaled_px(6.0, window.rem_size()),
        );
        Some(
            deferred(
                anchored()
                    .anchor(Anchor::BottomLeft)
                    .position(position)
                    .snap_to_window_with_margin(crate::ui_scale::scaled_px(8.0, window.rem_size()))
                    .child(card),
            )
            .into_any_element(),
        )
    }

    fn render_message_reference_attachment(
        &mut self,
        target: MessageRef,
        attachment: Attachment,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let is_image = attachment.is_image();
        let descriptor = attachment.descriptor;
        let settings = AppliedSettings::get(cx);
        if is_image {
            let (cached_attachment, active_transfer) = {
                let mut cache = self.media_cache.lock().expect("media cache lock poisoned");
                (cache.get(descriptor.id), cache.active_transfer(&descriptor))
            };
            if let Some(attachment) = cached_attachment {
                let source_width = descriptor.width.unwrap_or(4).max(1) as f32;
                let source_height = descriptor.height.unwrap_or(3).max(1) as f32;
                let aspect_ratio = source_width / source_height;
                let width = 368.0_f32.min(220.0 * aspect_ratio);
                let height = width / aspect_ratio;
                return div()
                    .id(("reference-preview-image", target.message_id.0 as usize))
                    .mt_2()
                    .w(rems_from_px(width))
                    .max_w_full()
                    .h(rems_from_px(height))
                    .overflow_hidden()
                    .bg(settings.theme.color(ThemeRole::Panel))
                    .child(
                        img(cached_attachment_image_source(
                            attachment,
                            self.image_cache.clone(),
                        ))
                        .absolute()
                        .inset_0()
                        .size_full()
                        .object_fit(ObjectFit::Contain),
                    )
                    .into_any_element();
            }
            let fetch = EagerImageFetch::new(target.room_id, descriptor.clone());
            let status = if active_transfer.is_some() {
                format!("Loading {}…", descriptor.file_name)
            } else if let Some(reason) = self.eager_image_fetches.failure(fetch.key) {
                format!("Could not load {} · {reason}", descriptor.file_name)
            } else {
                self.enqueue_eager_image(fetch, window, cx);
                format!("Loading {}…", descriptor.file_name)
            };
            return div()
                .mt_2()
                .min_h(rems_from_px(96.))
                .w_full()
                .px_2()
                .flex()
                .items_center()
                .justify_center()
                .bg(settings.theme.color(ThemeRole::Panel))
                .text_sm()
                .text_color(settings.theme.color(ThemeRole::TextMuted))
                .child(status)
                .into_any_element();
        }

        div()
            .mt_2()
            .px_2()
            .py_1()
            .bg(settings.theme.color(ThemeRole::ControlSurface))
            .text_sm()
            .text_color(settings.theme.color(ThemeRole::TextSecondary))
            .child(format!(
                "{} · {} bytes",
                descriptor.file_name, descriptor.byte_len
            ))
            .into_any_element()
    }
}

impl Render for ChattView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let applied = AppliedSettings::get(cx);
        let show_config_diagnostic = !applied.diagnostics.is_empty()
            && !matches!(
                applied.source_status,
                crate::config::io::SourceStatus::Missing
            );
        window.set_rem_size(crate::ui_scale::rem_size(cx));
        let ui_scale_revision = crate::ui_scale::revision(cx);
        if self.typography_revision != applied.typography_revision
            || self.ui_scale_revision != ui_scale_revision
        {
            self.typography_revision = applied.typography_revision;
            self.ui_scale_revision = ui_scale_revision;
            self.preview_history.reset_code_measurements();
        }
        self.composer.update(cx, |composer, cx| {
            composer.set_binding_mode(applied.binding_mode, cx)
        });
        self.advance_video(cx);
        if self.server_selector_visible() {
            return self
                .render_server_selector(cx)
                .when_some(self.settings.clone(), |root, settings| root.child(settings));
        }
        if !self.live_players.is_empty() {
            self.advance_live_video();
        }
        if let Some(theater) = self.theater_video.clone() {
            let source_key = self.source_key(theater.key.room_id, theater.key.attachment_id);
            let player =
                match theater
                    .source
                    .or_else(|| match self.video_sources.view(source_key) {
                        VideoSourceView::Ready(source) => Some(source),
                        _ => None,
                    }) {
                    Some(source) => {
                        if let Some(active) = self.theater_video.as_mut() {
                            active.source = Some(source.clone());
                        }
                        self.render_attachment_video(
                            theater.key,
                            theater.descriptor,
                            Some(source),
                            true,
                            cx,
                        )
                    }
                    None => {
                        let state = self.video_sources.view(source_key);
                        let cached_poster = self
                            .video_thumbnails
                            .view(ThumbnailKey { source_key })
                            .image
                            .is_some();
                        if cached_poster && !matches!(state, VideoSourceView::Failed { .. }) {
                            self.render_attachment_video(
                                theater.key,
                                theater.descriptor,
                                None,
                                true,
                                cx,
                            )
                        } else {
                            let (label, retry) = match state {
                                VideoSourceView::Failed { reason, .. } => {
                                    (format!("Could not load video · {reason}"), true)
                                }
                                _ => ("Loading preview…".into(), false),
                            };
                            let retry_descriptor = theater.descriptor.clone();
                            div()
                                .size_full()
                                .flex()
                                .flex_col()
                                .items_center()
                                .justify_center()
                                .gap_3()
                                .text_color(applied.theme.color(ThemeRole::TextSecondary))
                                .child(label)
                                .when(retry, |view| {
                                    view.child(
                                        mini_button(
                                            "retry-theater-video-source",
                                            "Retry",
                                            &applied.theme,
                                        )
                                        .on_click(
                                            cx.listener(move |this, _, _, cx| {
                                                this.video_sources
                                                    .promote(source_key, retry_descriptor.clone());
                                                this.video_sources.retry(source_key);
                                                this.pump_video_sources(cx);
                                                cx.notify();
                                            }),
                                        ),
                                    )
                                })
                                .into_any_element()
                        }
                    }
                };
            return div()
                .id("chatt-video-theater")
                .key_context("Chatt")
                .on_action(cx.listener(Self::open_settings))
                .on_action(cx.listener(Self::toggle_playback))
                .on_action(cx.listener(Self::seek_back))
                .on_action(cx.listener(Self::seek_forward))
                .on_action(cx.listener(Self::close_preview_action))
                .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _, cx| {
                    if this.scroll_video_volume(event, cx) {
                        cx.stop_propagation();
                    }
                }))
                .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                    if !this.drag_video_volume(event, cx) {
                        this.drag_video_scrub(event, cx);
                    }
                }))
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(|this, _: &MouseUpEvent, _, cx| {
                        this.finish_video_scrub(cx);
                        this.finish_video_volume_drag(cx);
                    }),
                )
                .size_full()
                .font_family(applied.fonts.interface_family.clone())
                .bg(applied.theme.color(ThemeRole::MediaViewport))
                .text_color(applied.theme.color(ThemeRole::MediaText))
                .child(player)
                .when_some(self.settings.clone(), |root, settings| root.child(settings));
        }
        if let Some(stream_id) = self.fullscreen_share
            && let Some(share) = self
                .model
                .live_shares
                .iter()
                .find(|share| share.stream_id == stream_id)
                .cloned()
            && self.live_players.contains_key(&stream_id)
        {
            let card = self.render_live_share_card(share, true, false, cx);
            return div()
                .id("chatt-live-fullscreen")
                .key_context("Chatt")
                .on_action(cx.listener(Self::open_settings))
                .on_action(cx.listener(Self::seek_back))
                .on_action(cx.listener(Self::seek_forward))
                .on_action(cx.listener(Self::live_zoom_in_action))
                .on_action(cx.listener(Self::live_zoom_out_action))
                .on_action(cx.listener(Self::live_reset_action))
                .on_action(cx.listener(Self::live_pan_up_action))
                .on_action(cx.listener(Self::live_pan_down_action))
                .size_full()
                .font_family(applied.fonts.interface_family.clone())
                .bg(applied.theme.color(ThemeRole::MediaViewport))
                .child(card)
                .when_some(self.settings.clone(), |root, settings| root.child(settings));
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
        let can_attach = self.editing.is_none()
            && !self.file_inspection_pending
            && self.pending_submission.is_none();
        let timeline_view = cx.entity().downgrade();
        let selection_view = cx.entity().downgrade();
        let timeline = MessageSelectionArea::new(
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
        let live_panel =
            (!self.model.live_shares.is_empty()).then(|| self.render_live_shares(window, cx));
        let resizing_live_pane = self.live_pane_resize.is_some();
        let active_preview = self.preview_history.active().cloned();
        let preview_panel = active_preview.map(|active| {
            let body_width = self.chat_body_width(window);
            let panel_width =
                panel_width_for_chat_width(self.preview_chat_width, body_width, window.rem_size());
            self.render_preview_panel(active, panel_width, window, cx)
        });
        let resizing_preview_pane = self.preview_pane_resize.is_some();
        let completion_view = self.completion_view(cx).filter(|view| {
            self.completion_session
                .as_ref()
                .is_some_and(|session| session.context_key == view.context_key)
                && (!view.options.is_empty() || view.hint.is_some())
        });
        let completion_engaged = completion_view.is_some()
            && self
                .completion_session
                .as_ref()
                .is_some_and(|session| session.engaged);
        let completion_key_context = if completion_engaged {
            "CompletionOpen CompletionEngaged"
        } else if completion_view.is_some() {
            "CompletionOpen"
        } else {
            ""
        };
        let completion_popup = completion_view.map(|view| self.render_completion_popup(view, cx));
        let queued_file_row = (!self.queued_files.is_empty()).then(|| self.render_queued_files(cx));
        let message_reference_preview = self.render_message_reference_preview(window, cx);
        let sidebar = self.show_rooms_sidebar.then(|| self.render_sidebar(cx));
        let composer_menu = self
            .composer_menu_open
            .then(|| self.render_composer_menu(cx));
        let composer_menu_trigger_bounds = self.composer_menu_trigger_bounds.clone();
        div()
            .id("chatt")
            .key_context("Chatt")
            .on_action(cx.listener(Self::open_media))
            .on_action(cx.listener(Self::open_settings))
            .on_action(cx.listener(Self::send_message))
            .on_action(cx.listener(Self::completion_next))
            .on_action(cx.listener(Self::completion_previous))
            .on_action(cx.listener(Self::completion_accept))
            .on_action(cx.listener(Self::completion_accept_engaged))
            .on_action(cx.listener(Self::completion_dismiss))
            .on_action(cx.listener(Self::toggle_playback))
            .on_action(cx.listener(Self::seek_back))
            .on_action(cx.listener(Self::seek_forward))
            .on_action(cx.listener(Self::live_zoom_in_action))
            .on_action(cx.listener(Self::live_zoom_out_action))
            .on_action(cx.listener(Self::live_reset_action))
            .on_action(cx.listener(Self::live_pan_up_action))
            .on_action(cx.listener(Self::live_pan_down_action))
            .on_action(cx.listener(Self::toggle_mute))
            .on_action(cx.listener(Self::toggle_deafen))
            .on_action(cx.listener(Self::toggle_voice))
            .on_action(cx.listener(Self::close_preview_action))
            .on_action(cx.listener(Self::find_in_code_action))
            .on_action(cx.listener(Self::next_code_match_action))
            .on_action(cx.listener(Self::previous_code_match_action))
            .on_action(cx.listener(Self::close_code_search_action))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                this.queue_files(paths.0.to_vec(), cx)
            }))
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, window, cx| {
                if !this.drag_audio_volume(event, cx)
                    && !this.drag_audio_scrub(event, cx)
                    && !this.drag_video_volume(event, cx)
                    && !this.drag_video_scrub(event, cx)
                {
                    if this.preview_pane_resize.is_some() {
                        this.drag_preview_pane(event, window, cx)
                    } else if this.preview_last_mouse_position.is_some() {
                        this.preview_image_mouse_move(event, cx)
                    } else {
                        this.drag_live_pane(event, window, cx)
                    }
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, cx| {
                    this.finish_audio_scrub(cx);
                    this.finish_audio_volume_drag(cx);
                    this.finish_video_scrub(cx);
                    this.finish_video_volume_drag(cx);
                    this.finish_live_pane_resize(cx);
                    this.finish_preview_pane_resize(cx);
                    this.finish_preview_image_pan(cx)
                }),
            )
            .size_full()
            .flex()
            .font_family(applied.fonts.interface_family.clone())
            .bg(applied.theme.color(ThemeRole::Window))
            .text_color(applied.theme.color(ThemeRole::TextPrimary))
            .when(resizing_live_pane, |root| {
                root.child(
                    canvas(
                        |_, _, _| {},
                        |_, _, window, _| {
                            window.set_window_cursor_style(gpui::CursorStyle::ResizeRow)
                        },
                    )
                    .absolute()
                    .size_full(),
                )
            })
            .when(resizing_preview_pane, |root| {
                root.child(
                    canvas(
                        |_, _, _| {},
                        |_, _, window, _| {
                            window.set_window_cursor_style(gpui::CursorStyle::ResizeColumn)
                        },
                    )
                    .absolute()
                    .size_full(),
                )
            })
            .when_some(sidebar, |root, sidebar| root.child(sidebar))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex()
                    .flex_col()
                    .when_some(live_panel, |column, live_panel| column.child(live_panel))
                    .child(
                        div()
                            .id("room-view")
                            .flex_1()
                            .min_h_0()
                            .w_full()
                            .flex()
                            .flex_col()
                            .when(self.show_top_status_bar, |panel| {
                                panel.child(
                                div()
                            .min_h(rems_from_px(TOP_BAR_HEIGHT))
                            .flex_none()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap_3()
                            .px_4()
                            .border_b_1()
                            .border_color(applied.theme.color(ThemeRole::BorderSubtle))
                            .bg(applied.theme.color(ThemeRole::Toolbar))
                            .child(
                                div()
                                    .text_color(applied.theme.color(ThemeRole::TextDim))
                                    .child("#"),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(selected_name.clone()),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(applied.theme.color(ThemeRole::TextDim))
                                    .child(format!("{count} messages")),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(applied.theme.color(ThemeRole::TextDim))
                                    .child(format!("{online} online")),
                            )
                            .when(!security.is_empty(), |bar| {
                                bar.child(
                                    div()
                                        .text_xs()
                                        .text_color(applied.theme.color(ThemeRole::TextMuted))
                                        .child(security),
                                )
                            })
                            .child(div().flex_1())
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(if ready {
                                        applied.theme.color(ThemeRole::StateSuccess)
                                    } else {
                                        applied.theme.color(ThemeRole::StateWarning)
                                    })
                                    .child(self.status.clone()),
                            )
                            .when(!ready, |bar| {
                                bar.child(
                                    toolbar_button(
                                        "retry",
                                        Some(IconName::RotateCcw),
                                        "Retry",
                                        &applied.theme,
                                    )
                                        .on_click(cx.listener(|this, _, _, cx| this.retry(cx))),
                                )
                            })
                            .child(
                                toolbar_button(
                                    "mute",
                                    Some(if self.model.voice.state.is_muted() {
                                        IconName::MicOff
                                    } else {
                                        IconName::Mic
                                    }),
                                    if self.model.voice.state.is_muted() {
                                        "Unmute"
                                    } else {
                                        "Mute"
                                    },
                                    &applied.theme,
                                )
                                .on_click(cx.listener(
                                    |this, _, window, cx| this.toggle_mute(&ToggleMute, window, cx),
                                )),
                            )
                            .child(
                                toolbar_button(
                                    "deafen",
                                    Some(if self.model.voice.state.is_deafened() {
                                        IconName::AudioOff
                                    } else {
                                        IconName::AudioOn
                                    }),
                                    if self.model.voice.state.is_deafened() {
                                        "Undeafen"
                                    } else {
                                        "Deafen"
                                    },
                                    &applied.theme,
                                )
                                .on_click(cx.listener(
                                    |this, _, window, cx| {
                                        this.toggle_deafen(&ToggleDeafen, window, cx)
                                    },
                                )),
                            )
                            .child(toolbar_button(
                                "output-down",
                                None,
                                "Vol −",
                                &applied.theme,
                            ).on_click(
                                cx.listener(|this, _, _, cx| this.adjust_output_volume(-5., cx)),
                            ))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(applied.theme.color(ThemeRole::TextMuted))
                                    .child(format!("{}", self.model.voice.output_volume.round())),
                            )
                            .child(toolbar_button(
                                "output-up",
                                None,
                                "Vol +",
                                &applied.theme,
                            ).on_click(
                                cx.listener(|this, _, _, cx| this.adjust_output_volume(5., cx)),
                            ))
                            .child(
                                toolbar_button(
                                    "voice",
                                    Some(
                                        if self.model.voice.joined_room == self.model.selected_room
                                            && self.model.selected_room.is_some()
                                        {
                                            IconName::Stop
                                        } else {
                                            IconName::Play
                                        },
                                    ),
                                    if self.model.voice.joined_room == self.model.selected_room
                                        && self.model.selected_room.is_some()
                                    {
                                        "Leave voice"
                                    } else {
                                        "Join voice"
                                    },
                                    &applied.theme,
                                )
                                .on_click(cx.listener(
                                    |this, _, window, cx| {
                                        this.toggle_voice(&ToggleVoice, window, cx)
                                    },
                                )),
                            )
                            )
                            })
                    .when(show_config_diagnostic, |panel| {
                        let diagnostic = &applied.diagnostics[0];
                        panel.child(
                            div()
                                .flex_none()
                                .px_4()
                                .py_2()
                                .border_b_1()
                                .border_color(applied.theme.color(ThemeRole::BorderSubtle))
                                .bg(applied.theme.color(ThemeRole::Panel))
                                .text_sm()
                                .text_color(match diagnostic.severity {
                                    crate::config::validation::DiagnosticSeverity::Warning => {
                                        applied.theme.color(ThemeRole::StateWarning)
                                    }
                                    crate::config::validation::DiagnosticSeverity::Error => {
                                        applied.theme.color(ThemeRole::StateDanger)
                                    }
                                })
                                .child(format!(
                                    "{}: {} — open Settings for details",
                                    diagnostic.path, diagnostic.message
                                )),
                        )
                    })
                    .when(
                        !self.model.at_start && self.model.older_cursor.is_some(),
                        |panel| {
                            panel.child(
                                div()
                                    .min_h(rems_from_px(34.))
                                    .flex_none()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .border_b_1()
                                    .border_color(applied.theme.color(ThemeRole::BorderSubtle))
                                    .child(
                                        compact_action_button(
                                            "load-older",
                                            Some(IconName::Download),
                                            "Load older messages",
                                            &applied.theme,
                                        )
                                        .on_click(
                                            cx.listener(|this, _, _, cx| this.load_older(cx)),
                                        ),
                                    ),
                            )
                        },
                    )
                    .child(
                        div()
                            .relative()
                            .flex_1()
                            .min_h_0()
                            .flex()
                            .flex_col()
                            .child(timeline)
                            .when(count == 0 && self.command_rows.is_empty(), |area| {
                                area.child(
                                    div()
                                        .absolute()
                                        .top(rems_from_px(40.))
                                        .left_0()
                                        .right_0()
                                        .px_4()
                                        .text_center()
                                        .text_color(
                                            applied.theme.color(ThemeRole::TextDim),
                                        )
                                        .child(empty_state(&self.model)),
                                )
                            }),
                    )
                    .child(
                        div()
                            .relative()
                            .key_context(completion_key_context)
                            .min_h(rems_from_px(MIN_COMPOSER_HEIGHT))
                            .flex_none()
                            .flex()
                            .flex_col()
                            .items_stretch()
                            .pl(rems_from_px(28.))
                            .pr(rems_from_px(28.))
                            .border_t_1()
                            .border_color(applied.theme.color(ThemeRole::BorderSubtle))
                            .bg(applied.theme.color(ThemeRole::Input))
                            .when_some(composer_menu, |bar, menu| bar.child(menu))
                            .when_some(completion_popup, |bar, popup| bar.child(popup))
                            .when_some(queued_file_row, |bar, files| bar.child(files))
                            .when_some(self.composer_error.clone(), |bar, error| {
                                bar.child(
                                    div()
                                        .mb_1()
                                        .mt_2()
                                        .text_sm()
                                        .text_color(applied.theme.color(ThemeRole::StateWarning))
                                        .child(format!(
                                            "{error}. Your draft and queued files were retained."
                                        )),
                                )
                            })
                            .when(!ready, |bar| {
                                bar.child(
                                    div()
                                        .mb_1()
                                        .mt_2()
                                        .text_sm()
                                        .text_color(applied.theme.color(ThemeRole::TextMuted))
                                        .child(
                                            "Daemon offline — your draft and queued files are retained; sending is disabled.",
                                        ),
                                )
                            })
                            .child(
                                div()
                                    .min_h(rems_from_px(MIN_COMPOSER_HEIGHT))
                                    .flex()
                                    .items_stretch()
                                    .child(
                                        div()
                                            .id("add-media-region")
                                            .ml(rems_from_px(-28.))
                                            .w(rems_from_px(64.))
                                            .min_h(rems_from_px(MIN_COMPOSER_HEIGHT))
                                            .flex_none()
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .cursor_pointer()
                                            .mr(rems_from_px(15.))
                                            .child(composer_add_button(
                                                can_attach,
                                                &applied.theme,
                                            ))
                                            .on_click(cx.listener(
                                                |this, _, window, cx| {
                                                    this.composer_menu_open = false;
                                                    this.composer_menu_action_taken = false;
                                                    this.open_media(&OpenMedia, window, cx)
                                                },
                                            )),
                                    )
                                    .child(
                                        div()
                                            .min_w_0()
                                            .min_h(rems_from_px(40.))
                                            .flex_1()
                                            .flex()
                                            .items_center()
                                            .child(self.composer.clone()),
                                    )
                                    .child(
                                        div()
                                            .id("composer-menu")
                                            .relative()
                                            .ml(rems_from_px(15.))
                                            .mr(rems_from_px(-28.))
                                            .w(rems_from_px(64.))
                                            .min_h(rems_from_px(MIN_COMPOSER_HEIGHT))
                                            .flex_none()
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .cursor_pointer()
                                            .child(
                                                canvas(
                                                    move |bounds, _, _| {
                                                        composer_menu_trigger_bounds
                                                            .set(Some(bounds));
                                                    },
                                                    |_, _, _, _| {},
                                                )
                                                .absolute()
                                                .size_full(),
                                            )
                                            .child(icon(
                                                IconName::Menu,
                                                24.0,
                                                applied.theme.color(
                                                    if self.composer_menu_open {
                                                        ThemeRole::TextPrimary
                                                    } else {
                                                        ThemeRole::TextMuted
                                                    },
                                                ),
                                            ))
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.toggle_composer_menu(cx)
                                            })),
                                    ),
                            ),
                    ),
                    ),
            )
            .when_some(preview_panel, |root, preview_panel| {
                root.child(
                    div()
                        .id("preview-pane-resize")
                        .w(rems_from_px(PREVIEW_DIVIDER_WIDTH))
                        .h_full()
                        .flex_none()
                        .flex()
                        .justify_center()
                        .cursor_col_resize()
                        .hover({
                            let hover = applied.theme.color(ThemeRole::StateSelection);
                            move |divider| divider.bg(hover)
                        })
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, event: &MouseDownEvent, window, cx| {
                                this.begin_preview_pane_resize(event, window, cx)
                            }),
                        )
                        .on_mouse_up(
                            MouseButton::Left,
                            cx.listener(|this, _: &MouseUpEvent, _, cx| {
                                this.finish_preview_pane_resize(cx)
                            }),
                        )
                        .on_mouse_up_out(
                            MouseButton::Left,
                            cx.listener(|this, _: &MouseUpEvent, _, cx| {
                                this.finish_preview_pane_resize(cx)
                            }),
                        )
                        .child(div().w(rems_from_px(3.0)).h_full().bg(applied.theme.color(
                            if resizing_preview_pane {
                                ThemeRole::BorderFocus
                            } else {
                                ThemeRole::BorderSubtle
                            },
                        ))),
                )
                .child(preview_panel)
            })
            .when_some(message_reference_preview, |root, preview| root.child(preview))
            .when_some(self.settings.clone(), |root, settings| root.child(settings))
    }
}

fn live_share_title(share: &local_rpc::model::LiveShare, palette: &ThemePalette) -> Div {
    div()
        .min_w_0()
        .flex_1()
        .flex()
        .items_center()
        .gap_2()
        .text_sm()
        .child(
            div()
                .min_w_0()
                .truncate()
                .font_weight(FontWeight::SEMIBOLD)
                .child(format!("{} is sharing", share.sender_name)),
        )
        .child(
            div()
                .flex_none()
                .text_xs()
                .text_color(palette.color(ThemeRole::TextDim))
                .child(format!(
                    "{}×{} · {}",
                    share.coded_width, share.coded_height, share.codec
                )),
        )
}

fn image_box_size(descriptor: &AttachmentDescriptor) -> (f32, f32) {
    timeline::media_box_size(
        descriptor.width.unwrap_or(4),
        descriptor.height.unwrap_or(3),
    )
}

fn preview_image_viewport_bounds(
    window_size: gpui::Size<Pixels>,
    panel_width: Pixels,
    rem_size: Pixels,
) -> Bounds<Pixels> {
    let chrome_height = crate::ui_scale::scaled_px(PREVIEW_TAB_BAR_HEIGHT, rem_size);
    Bounds {
        origin: point(window_size.width - panel_width, chrome_height),
        size: gpui::size(
            panel_width,
            (window_size.height - chrome_height).max(px(1.0)),
        ),
    }
}

fn image_frame(descriptor: &AttachmentDescriptor, palette: &ThemePalette) -> Div {
    let (width, height) = image_box_size(descriptor);
    div()
        .relative()
        .w(rems_from_px(width))
        .max_w_full()
        .aspect_ratio(width / height)
        .border_6()
        .border_color(palette.color(ThemeRole::BorderMedia))
        .rounded_xs()
}

fn video_frame(palette: &ThemePalette) -> Div {
    div()
        .relative()
        .w_full()
        .aspect_ratio(INLINE_VIDEO_ASPECT_RATIO)
        .border_6()
        .border_color(palette.color(ThemeRole::BorderMedia))
        .rounded_xs()
}

fn connection_label(model: &ChatModel) -> String {
    match &model.phase {
        ConnectionPhase::Discovering => "Discovering daemon…".into(),
        ConnectionPhase::Connecting => "Connecting…".into(),
        ConnectionPhase::Syncing => "Syncing…".into(),
        ConnectionPhase::Ready => match model.server_connection {
            local_rpc::model::ConnectionState::Online => model.active_server.as_ref().map_or_else(
                || "Connected".into(),
                |server| format!("Connected · {server}"),
            ),
            local_rpc::model::ConnectionState::Connecting => {
                "Daemon ready · server connecting…".into()
            }
            local_rpc::model::ConnectionState::Offline => "Daemon ready · server offline".into(),
        },
        ConnectionPhase::Disconnected { .. } => "Daemon offline".into(),
        ConnectionPhase::Incompatible { .. } => "Daemon incompatible".into(),
    }
}

fn server_matches_query(server: &ServerSummary, query: &str) -> bool {
    let query = query.trim().to_lowercase();
    query.is_empty()
        || server.label.to_lowercase().contains(&query)
        || server.username.to_lowercase().contains(&query)
        || server.tcp_addr.to_lowercase().contains(&query)
}

fn server_index_for_label(servers: &[ServerSummary], selected: Option<&str>) -> Option<usize> {
    let selected = selected?;
    servers.iter().position(|server| server.label == selected)
}

fn server_switch_guard_reason(
    pending_activity: bool,
    active_transfers: bool,
) -> Option<&'static str> {
    if pending_activity {
        Some("Finish the pending message or upload before switching servers")
    } else if active_transfers {
        Some("Wait for or cancel active file transfers before switching servers")
    } else {
        None
    }
}

fn queued_files_status(count: usize, daemon_ready: bool, room_selected: bool) -> String {
    let noun = if count == 1 { "file" } else { "files" };
    let next_step = if !daemon_ready {
        "Waiting for daemon"
    } else if !room_selected {
        "Select a room to send"
    } else {
        "Enter to send"
    };
    format!("{count} {noun} queued · {next_step}")
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
        Operation::SelectServer => "Server selection",
        Operation::ResolveServerPrompt => "Server security decision",
        Operation::SelectRoom => "Room selection",
        Operation::SendMessage => "Message",
        Operation::RunCommand => "Command",
        Operation::EditMessage => "Edit",
        Operation::DeleteMessage => "Delete",
        Operation::SetVoiceState => "Voice state change",
        Operation::JoinVoice => "Voice join",
        Operation::LeaveVoice => "Voice leave",
        Operation::SetOutputVolume => "Volume change",
        Operation::StartLiveShare => "Screen share playback",
        Operation::StopLiveShare => "Screen share stop",
        Operation::BeginUpload => "Upload",
        Operation::CancelBulkTransfer => "Attachment cancellation",
        Operation::CancelFileTransfer => "File transfer cancellation",
        _ => "Request",
    }
}

fn candidate_kind_label(kind: CommandCandidateKind) -> &'static str {
    match kind {
        CommandCandidateKind::User => "users",
        CommandCandidateKind::Room => "rooms",
        CommandCandidateKind::Sound => "sounds",
    }
}

fn highlighted_completion_label(
    label: String,
    ranges: &[std::ops::Range<usize>],
    selected: bool,
    palette: &ThemePalette,
) -> Div {
    let mut element = div()
        .min_w_0()
        .overflow_hidden()
        .flex()
        .items_center()
        .text_sm()
        .font_weight(FontWeight::SEMIBOLD);
    for (byte_index, character) in label.char_indices() {
        let matched = ranges
            .iter()
            .any(|range| range.start <= byte_index && byte_index < range.end);
        element = element.child(
            div()
                .flex_none()
                .text_color(if matched {
                    if selected {
                        palette.color(ThemeRole::TextInverse)
                    } else {
                        palette.color(ThemeRole::ParticipantRemoteOne)
                    }
                } else if selected {
                    palette.color(ThemeRole::ControlActiveText)
                } else {
                    palette.color(ThemeRole::TextSecondary)
                })
                .child(character.to_string()),
        );
    }
    element
}

fn security_label(trust: TrustState) -> &'static str {
    match trust {
        TrustState::NotApplicable => "",
        TrustState::Unverified => "E2E unverified",
        TrustState::Verified => "E2E verified",
        TrustState::Changed => "E2E identity changed",
    }
}

fn sender_color(
    sender: &str,
    local: bool,
    settings: &crate::theme::ResolvedSettings,
) -> gpui::Rgba {
    if local {
        settings.theme.color(ThemeRole::ParticipantLocal)
    } else {
        match sender.as_bytes().first().copied().unwrap_or_default() % 4 {
            0 => settings.theme.color(ThemeRole::ParticipantRemoteOne),
            1 => settings.theme.color(ThemeRole::ParticipantRemoteTwo),
            2 => settings.theme.color(ThemeRole::ParticipantRemoteThree),
            _ => settings.theme.color(ThemeRole::ParticipantRemoteFour),
        }
    }
}

fn video_key(room_id: RoomId, message_id: u64, descriptor: &AttachmentDescriptor) -> VideoKey {
    VideoKey {
        room_id,
        message_id,
        attachment_id: descriptor.id,
    }
}

fn audio_key(room_id: RoomId, message_id: u64, descriptor: &AttachmentDescriptor) -> AudioKey {
    AudioKey {
        room_id,
        message_id,
        attachment_id: descriptor.id,
    }
}

fn message_audio_key(message: &timeline::Message) -> Option<AudioKey> {
    let attachment = message.attachment.as_ref()?;
    attachment
        .is_audio()
        .then(|| audio_key(message.room_id, message.id, &attachment.descriptor))
}

fn message_video_key(message: &timeline::Message) -> Option<VideoKey> {
    let attachment = message.attachment.as_ref()?;
    attachment
        .is_video()
        .then(|| video_key(message.room_id, message.id, &attachment.descriptor))
}

fn latest_visible_media(
    interactions: &VecDeque<MediaPlaybackTarget>,
    visible_audio: &HashSet<AudioKey>,
    visible_video: &HashSet<VideoKey>,
) -> Option<MediaPlaybackTarget> {
    interactions.iter().copied().find(|target| match target {
        MediaPlaybackTarget::Audio(key) => visible_audio.contains(key),
        MediaPlaybackTarget::Video(key) => visible_video.contains(key),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use local_rpc::bulk::BulkFinished;
    use local_rpc::model::MediaKind;

    #[test]
    fn voice_buttons_have_total_three_state_transitions() {
        let cases = [
            (VoiceState::Live, VoiceState::Muted, VoiceState::Deafened),
            (VoiceState::Muted, VoiceState::Live, VoiceState::Deafened),
            (VoiceState::Deafened, VoiceState::Live, VoiceState::Live),
        ];
        for (state, mute_target, deafen_target) in cases {
            assert_eq!(state.toggle_mute(), mute_target, "mute from {state:?}");
            assert_eq!(
                state.toggle_deafen(),
                deafen_target,
                "deafen from {state:?}"
            );
        }
    }

    fn queued_file(id: u64) -> QueuedFile {
        QueuedFile {
            id,
            file_name: format!("file-{id}.bin"),
            source: QueuedFileSource::Path(PathBuf::from(format!("/tmp/file-{id}.bin"))),
        }
    }

    fn image_fetch(room_id: RoomId, marker: u8) -> EagerImageFetch {
        EagerImageFetch::new(
            room_id,
            AttachmentDescriptor {
                id: AttachmentId {
                    timestamp_ms: marker as u64,
                    transfer_id: local_rpc::ids::FileTransferId(marker as u64),
                },
                file_name: format!("image-{marker}.png"),
                media_kind: MediaKind::Image,
                content_type: "image/png".into(),
                byte_len: 10,
                width: Some(400),
                height: Some(300),
            },
        )
    }

    fn complete_cached_attachment(
        cache: &mut MediaCache,
        descriptor: &AttachmentDescriptor,
        transfer_id: BulkTransferId,
        bytes: &[u8],
    ) {
        cache.reserve(transfer_id, descriptor).unwrap();
        cache.chunk(transfer_id, bytes).unwrap();
        cache.finish(BulkFinished { transfer_id }).unwrap();
    }

    fn jump_message(id: u64) -> timeline::Message {
        timeline::Message {
            room_id: RoomId(1),
            id,
            sender: "sender".into(),
            body: String::new(),
            timestamp_ms: id,
            local: false,
            edited: false,
            unverified: false,
            notice: false,
            attachment: None,
        }
    }

    #[test]
    fn preview_image_viewport_uses_fixed_panel_chrome_without_measurement() {
        let bounds = preview_image_viewport_bounds(
            gpui::size(px(1_920.0), px(1_080.0)),
            px(900.0),
            px(16.0),
        );

        assert_eq!(PREVIEW_TAB_BAR_HEIGHT, TOP_BAR_HEIGHT);
        assert_eq!(bounds.origin, point(px(1_020.0), px(52.0)));
        assert_eq!(bounds.size, gpui::size(px(900.0), px(1_028.0)));
    }

    #[test]
    fn reference_jump_uses_ordering_to_stop_after_crossing_a_target() {
        let messages = vec![jump_message(300), jump_message(400), jump_message(500)];

        assert_eq!(
            message_reference_jump_decision(
                &messages,
                local_rpc::ids::MessageId(350),
                Some(local_rpc::ids::MessageId(300)),
                false,
                1,
            ),
            MessageReferenceJumpDecision::Unavailable
        );
        assert_eq!(
            message_reference_jump_decision(
                &messages,
                local_rpc::ids::MessageId(200),
                Some(local_rpc::ids::MessageId(300)),
                false,
                1,
            ),
            MessageReferenceJumpDecision::LoadOlder(local_rpc::ids::MessageId(300))
        );
    }

    #[test]
    fn reference_jump_budget_is_a_distinct_search_window_outcome() {
        assert_eq!(
            message_reference_jump_decision(
                &[jump_message(300)],
                local_rpc::ids::MessageId(200),
                Some(local_rpc::ids::MessageId(300)),
                false,
                REFERENCE_JUMP_PAGE_LIMIT,
            ),
            MessageReferenceJumpDecision::SearchWindowExhausted
        );
    }

    #[test]
    fn queued_file_status_does_not_invite_sending_before_daemon_sync() {
        assert_eq!(
            queued_files_status(1, false, false),
            "1 file queued · Waiting for daemon",
        );
        assert_eq!(
            queued_files_status(2, true, false),
            "2 files queued · Select a room to send",
        );
        assert_eq!(
            queued_files_status(2, true, true),
            "2 files queued · Enter to send",
        );
    }

    #[test]
    fn code_preview_rejects_oversized_descriptors_before_transport() {
        assert_eq!(code_preview_size_error(MAX_CODE_PREVIEW_BYTES), None);
        assert_eq!(
            code_preview_size_error(MAX_CODE_PREVIEW_BYTES + 1),
            Some("file too large to preview")
        );
    }

    #[test]
    fn rejected_message_submission_recovers_the_complete_file_batch() {
        let submission = PendingSubmission {
            room_id: RoomId(7),
            draft: Some("message".into()),
            files: VecDeque::from([queued_file(1), queued_file(2)]),
            total_files: 2,
            completed_files: 0,
            phase: SubmissionPhase::AwaitingMessage {
                request_id: RequestId(11),
            },
        };

        assert_eq!(
            submission
                .into_failed_files()
                .into_iter()
                .map(|file| file.id)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
    }

    #[test]
    fn failed_upload_recovers_current_and_unstarted_files_but_not_completed_files() {
        let current = SubmittedUpload {
            file: queued_file(2),
            begin_request: RequestId(21),
            finish_request: RequestId(22),
        };
        let submission = PendingSubmission {
            room_id: RoomId(7),
            draft: None,
            files: VecDeque::from([queued_file(3), queued_file(4)]),
            total_files: 4,
            completed_files: 1,
            phase: SubmissionPhase::Uploading(current),
        };

        assert_eq!(
            submission
                .into_failed_files()
                .into_iter()
                .map(|file| file.id)
                .collect::<Vec<_>>(),
            vec![2, 3, 4]
        );
    }

    #[test]
    fn disconnect_marks_the_current_upload_ambiguous_and_preserves_the_retry_batch() {
        let current = SubmittedUpload {
            file: queued_file(2),
            begin_request: RequestId(21),
            finish_request: RequestId(22),
        };
        let submission = PendingSubmission {
            room_id: RoomId(7),
            draft: None,
            files: VecDeque::from([queued_file(3), queued_file(4)]),
            total_files: 4,
            completed_files: 1,
            phase: SubmissionPhase::Uploading(current),
        };

        assert!(submission.outcome_is_ambiguous());
        assert_eq!(
            submission
                .into_failed_files()
                .into_iter()
                .map(|file| file.id)
                .collect::<Vec<_>>(),
            vec![2, 3, 4]
        );
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
    fn server_search_matches_label_username_and_address_case_insensitively() {
        let server = ServerSummary {
            label: "Work Chat".into(),
            username: "Alice".into(),
            tcp_addr: "chat.example.test:443".into(),
            require_transport_encryption: true,
            availability: ServerAvailability::Ready,
        };

        assert!(server_matches_query(&server, ""));
        assert!(server_matches_query(&server, " work "));
        assert!(server_matches_query(&server, "ALICE"));
        assert!(server_matches_query(&server, "EXAMPLE.TEST"));
        assert!(!server_matches_query(&server, "personal"));
    }

    #[test]
    fn server_selection_uses_stable_labels_across_reordering_and_filtering() {
        let work = ServerSummary {
            label: "Work Chat".into(),
            username: "Alice".into(),
            tcp_addr: "work.example.test:443".into(),
            require_transport_encryption: true,
            availability: ServerAvailability::Ready,
        };
        let personal = ServerSummary {
            label: "Personal".into(),
            username: "alice".into(),
            tcp_addr: "home.example.test:443".into(),
            require_transport_encryption: true,
            availability: ServerAvailability::Ready,
        };

        assert_eq!(
            server_index_for_label(&[personal.clone(), work.clone()], Some("Work Chat")),
            Some(1)
        );
        assert_eq!(
            server_index_for_label(&[work, personal], Some("Work Chat")),
            Some(0)
        );
        assert_eq!(server_index_for_label(&[], Some("Work Chat")), None);
    }

    #[test]
    fn server_switch_guard_blocks_pending_work_and_active_transfers() {
        assert_eq!(
            server_switch_guard_reason(true, false),
            Some("Finish the pending message or upload before switching servers")
        );
        assert_eq!(
            server_switch_guard_reason(false, true),
            Some("Wait for or cancel active file transfers before switching servers")
        );
        assert_eq!(server_switch_guard_reason(false, false), None);
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
    fn completed_image_is_cached_via_contains_and_consumed_via_get() {
        let mut descriptor = image_fetch(RoomId(1), 11).descriptor;
        let bytes = b"immutable image bytes";
        descriptor.byte_len = bytes.len() as u64;
        let mut cache = MediaCache::new(1024);
        complete_cached_attachment(&mut cache, &descriptor, BulkTransferId(11), bytes);

        assert!(cache.contains(descriptor.id));
        let attachment = cache.get(descriptor.id).expect("cached attachment bytes");
        assert_eq!(attachment.id(), descriptor.id);
        assert_eq!(attachment.bytes(), bytes);
    }

    #[test]
    fn duplicate_attachment_reads_are_not_started() {
        let descriptor = image_fetch(RoomId(1), 12).descriptor;
        let mut cache = MediaCache::new(1024);
        cache.reserve(BulkTransferId(12), &descriptor).unwrap();

        assert!(cache.reserve(BulkTransferId(13), &descriptor).is_err());
        assert_eq!(cache.active_transfer(&descriptor), Some(BulkTransferId(12)));
        assert_eq!(
            cache.available_transfer_slots(),
            local_rpc::MAX_CONCURRENT_TRANSFERS - 1
        );
    }

    #[test]
    fn explicit_save_writes_exactly_the_selected_attachment_bytes() {
        let mut descriptor = image_fetch(RoomId(1), 13).descriptor;
        let bytes = b"bytes selected by the user";
        descriptor.byte_len = bytes.len() as u64;
        let mut cache = MediaCache::new(1024);
        complete_cached_attachment(&mut cache, &descriptor, BulkTransferId(13), bytes);
        let attachment = cache.get(descriptor.id).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("saved.bin");

        let saved =
            write_cached_attachment_to_user_selected_path(&attachment, Some(destination.clone()))
                .unwrap();

        assert_eq!(saved, Some(destination.clone()));
        assert_eq!(fs::read(destination).unwrap(), bytes);
    }

    #[test]
    fn canceling_the_save_picker_performs_no_write() {
        let mut descriptor = image_fetch(RoomId(1), 14).descriptor;
        let bytes = b"do not save";
        descriptor.byte_len = bytes.len() as u64;
        let mut cache = MediaCache::new(1024);
        complete_cached_attachment(&mut cache, &descriptor, BulkTransferId(14), bytes);
        let attachment = cache.get(descriptor.id).unwrap();
        let directory = tempfile::tempdir().unwrap();

        assert_eq!(
            write_cached_attachment_to_user_selected_path(&attachment, None).unwrap(),
            None
        );
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
    }

    #[test]
    fn video_rendering_route_does_not_consult_or_populate_media_cache() {
        let descriptor = AttachmentDescriptor {
            id: AttachmentId {
                timestamp_ms: 15,
                transfer_id: local_rpc::ids::FileTransferId(15),
            },
            file_name: "video.mp4".into(),
            media_kind: MediaKind::Video,
            content_type: "video/mp4".into(),
            byte_len: 10,
            width: Some(1920),
            height: Some(1080),
        };
        let attachment = Attachment {
            descriptor: descriptor.clone(),
        };
        let cache = MediaCache::new(1024);

        assert_eq!(attachment.render_kind(), AttachmentRenderKind::Video);
        assert!(!cache.contains(descriptor.id));
        assert_eq!(
            cache.available_transfer_slots(),
            local_rpc::MAX_CONCURRENT_TRANSFERS
        );
    }

    #[test]
    fn audio_rendering_route_does_not_consult_or_populate_media_cache() {
        let descriptor = AttachmentDescriptor {
            id: AttachmentId {
                timestamp_ms: 16,
                transfer_id: local_rpc::ids::FileTransferId(16),
            },
            file_name: "voice.opus".into(),
            media_kind: MediaKind::Audio,
            content_type: "audio/ogg".into(),
            byte_len: 10,
            width: None,
            height: None,
        };
        let attachment = Attachment {
            descriptor: descriptor.clone(),
        };
        let cache = MediaCache::new(1024);

        assert_eq!(attachment.render_kind(), AttachmentRenderKind::Audio);
        assert!(!cache.contains(descriptor.id));
        assert_eq!(
            cache.available_transfer_slots(),
            local_rpc::MAX_CONCURRENT_TRANSFERS
        );
    }

    #[test]
    fn media_shortcuts_skip_newer_interactions_that_are_not_visible() {
        let attachment_id = AttachmentId {
            timestamp_ms: 17,
            transfer_id: local_rpc::ids::FileTransferId(17),
        };
        let audio = AudioKey {
            room_id: RoomId(1),
            message_id: 17,
            attachment_id,
        };
        let video = VideoKey {
            room_id: RoomId(1),
            message_id: 18,
            attachment_id,
        };
        let interactions = VecDeque::from([
            MediaPlaybackTarget::Audio(audio),
            MediaPlaybackTarget::Video(video),
        ]);

        assert_eq!(
            latest_visible_media(&interactions, &HashSet::new(), &HashSet::from([video])),
            Some(MediaPlaybackTarget::Video(video))
        );
        assert_eq!(
            latest_visible_media(
                &interactions,
                &HashSet::from([audio]),
                &HashSet::from([video])
            ),
            Some(MediaPlaybackTarget::Audio(audio))
        );
    }

    #[test]
    fn image_box_size_uses_the_same_fallback_as_loaded_images() {
        let known = image_fetch(RoomId(1), 9).descriptor;
        assert_eq!(image_box_size(&known), (400.0, 300.0));

        let mut unknown = known;
        unknown.width = None;
        unknown.height = None;
        assert_eq!(image_box_size(&unknown), (128.0, 96.0));
    }

    #[test]
    fn live_pan_is_clamped_to_the_zoomed_video_edges() {
        let viewport = Bounds {
            origin: point(px(0.0), px(0.0)),
            size: gpui::size(px(1000.0), px(1000.0)),
        };

        assert_eq!(
            live_pan_limits((1600, 900), viewport, 2.0),
            point(px(500.0), px(62.5)),
        );
        assert_eq!(
            clamp_live_pan(point(px(900.0), px(-100.0)), (1600, 900), viewport, 2.0),
            point(px(500.0), px(-62.5)),
        );
        assert_eq!(
            clamp_live_pan(point(px(100.0), px(100.0)), (1600, 900), viewport, 1.0),
            point(px(0.0), px(0.0)),
        );
    }

    #[test]
    fn live_zoom_keeps_the_focal_pixel_stationary() {
        let viewport = Bounds {
            origin: point(px(100.0), px(50.0)),
            size: gpui::size(px(1000.0), px(600.0)),
        };
        let focal = viewport.center() + point(px(100.0), px(-50.0));
        let coded_size = (1600, 900);
        let old_pan = point(px(0.0), px(0.0));
        let source = LiveVideoGeometry::new(coded_size, viewport, 1.0, old_pan)
            .unwrap()
            .source_pixel_at(focal);
        let new_pan = zoom_live_pan(old_pan, coded_size, 1.0, 2.0, viewport, focal);

        assert_eq!(new_pan, point(px(-100.0), px(50.0)));
        let mapped = LiveVideoGeometry::new(coded_size, viewport, 2.0, new_pan)
            .unwrap()
            .position_of_source_pixel(source);
        assert!((mapped.x - focal.x).as_f32().abs() < 0.01);
        assert!((mapped.y - focal.y).as_f32().abs() < 0.01);
    }

    #[test]
    fn live_zoom_keeps_the_focal_pixel_stationary_after_panning() {
        let viewport = Bounds {
            origin: point(px(75.0), px(125.0)),
            size: gpui::size(px(1100.0), px(700.0)),
        };
        let coded_size = (2560, 1440);
        let focal = viewport.center() + point(px(-170.0), px(90.0));
        let old_pan = point(px(130.0), px(-45.0));
        let old_zoom = 2.0;
        let new_zoom = 3.25;
        let source = LiveVideoGeometry::new(coded_size, viewport, old_zoom, old_pan)
            .unwrap()
            .source_pixel_at(focal);

        let new_pan = zoom_live_pan(old_pan, coded_size, old_zoom, new_zoom, viewport, focal);
        let mapped = LiveVideoGeometry::new(coded_size, viewport, new_zoom, new_pan)
            .unwrap()
            .position_of_source_pixel(source);

        assert!((mapped.x - focal.x).as_f32().abs() < 0.01);
        assert!((mapped.y - focal.y).as_f32().abs() < 0.01);
    }

    #[test]
    fn live_video_geometry_fits_the_coded_resolution_into_the_viewport() {
        let viewport = Bounds {
            origin: point(px(100.0), px(50.0)),
            size: gpui::size(px(1000.0), px(1000.0)),
        };

        let geometry =
            LiveVideoGeometry::new((1600, 900), viewport, 1.0, point(px(0.0), px(0.0))).unwrap();

        assert_eq!(geometry.scale, 0.625);
        assert_eq!(geometry.bounds.size, gpui::size(px(1000.0), px(562.5)));
        assert_eq!(geometry.bounds.origin, point(px(100.0), px(268.75)));
    }

    #[test]
    fn video_scrub_fraction_maps_and_clamps_timeline_coordinates() {
        let bounds = Bounds {
            origin: point(px(100.0), px(20.0)),
            size: gpui::size(px(400.0), px(16.0)),
        };

        assert_eq!(horizontal_fraction(bounds, px(100.0), 60.0), Some(0.0));
        assert_eq!(horizontal_fraction(bounds, px(300.0), 60.0), Some(0.5));
        assert_eq!(horizontal_fraction(bounds, px(500.0), 60.0), Some(1.0));
        assert_eq!(horizontal_fraction(bounds, px(20.0), 60.0), Some(0.0));
        assert_eq!(horizontal_fraction(bounds, px(900.0), 60.0), Some(1.0));
    }

    #[test]
    fn video_scrub_fraction_rejects_unavailable_timeline() {
        let zero_width = Bounds {
            origin: point(px(100.0), px(20.0)),
            size: gpui::size(px(0.0), px(16.0)),
        };
        let valid = Bounds {
            origin: point(px(100.0), px(20.0)),
            size: gpui::size(px(400.0), px(16.0)),
        };

        assert_eq!(horizontal_fraction(zero_width, px(100.0), 60.0), None);
        assert_eq!(horizontal_fraction(valid, px(100.0), 0.0), None);
    }

    #[test]
    fn live_pane_height_preserves_both_video_and_chat() {
        assert_eq!(
            clamp_live_pane_height(px(100.0), px(900.0), px(16.0)),
            px(MIN_LIVE_PANE_HEIGHT),
        );
        assert_eq!(
            clamp_live_pane_height(px(900.0), px(900.0), px(16.0)),
            px(699.0),
        );
        assert_eq!(
            clamp_live_pane_height(px(900.0), px(300.0), px(16.0)),
            px(99.0),
        );
    }
}
