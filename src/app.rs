use std::{
    cell::Cell,
    collections::{HashMap, HashSet, VecDeque},
    fs,
    path::PathBuf,
    rc::Rc,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use gpui::{
    AnyElement, App, Bounds, Context, Div, ExternalPaths, Focusable, FollowMode, FontWeight,
    KeyBinding, ListAlignment, ListState, LruImageCache, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, ObjectFit, PathPromptOptions, PinchEvent, Pixels, Point, Render,
    ScrollDelta, ScrollWheelEvent, SharedString, Stateful, Task, Window, actions, canvas, div, img,
    list, point, prelude::*, px, rgb, rgba,
};
use local_rpc::{
    frame::{ClientFrame, DaemonFrame, Operation, RequestOutcome},
    ids::{RoomId, StreamId},
    model::{AttachmentDescriptor, AttachmentId, BulkTransferId, RequestId, RoomKind, TrustState},
};
use markdown::{
    Markdown, MarkdownElement, MarkdownSelectionArea, MarkdownSelectionGroup, MarkdownSelectionKey,
    MarkdownStyle,
};

use crate::{
    composer::Composer,
    daemon::{
        client::{DaemonClient, DaemonEvent},
        reducer,
    },
    icons::{IconName, icon},
    image_cache::{PreviewImageLoader, TimelineImageLoader},
    media_cache::MediaCache,
    model::{ChatModel, ConnectionPhase, PendingRequest},
    mpv_player::{MpvPlayer, SeekMode},
    preview::{
        DEFAULT_PANEL_WIDTH, DIVIDER_WIDTH as PREVIEW_DIVIDER_WIDTH, ImageViewState,
        PreviewHistory, PreviewItem, clamp_panel_width,
    },
    scroll_capture::capture_scroll,
    timeline::{self, Attachment},
    video_controls::{
        CONTROLS_ANIMATION_DURATION, CONTROLS_HIDE_DELAY, VideoControlsState, VideoScrub,
        VideoVolumeDrag, horizontal_fraction, vertical_fraction, volume_scroll_delta,
    },
    video_manager::{AttachmentVideoManager, VideoDrain, VideoKey},
    video_player::{
        VideoPlayerConfig, VideoPlayerEvent, VideoPlayerHandler, aspect_ratio,
        render_video_player,
    },
    video_thumbnail::{ThumbnailKey, VideoThumbnailCache},
};

const SIDEBAR_WIDTH: f32 = 232.0;
const TOP_BAR_HEIGHT: f32 = 52.0;
const MIN_COMPOSER_HEIGHT: f32 = 64.0;
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

#[derive(Clone, Debug)]
struct TheaterVideo {
    key: VideoKey,
    descriptor: AttachmentDescriptor,
    path: PathBuf,
}

#[derive(Clone, Copy, Debug)]
struct LivePaneResize {
    start_y: Pixels,
    start_height: Pixels,
}

#[derive(Clone, Copy, Debug)]
struct PreviewPaneResize {
    start_x: Pixels,
    start_width: Pixels,
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

fn clamp_live_pane_height(height: Pixels, window_height: Pixels) -> Pixels {
    let available =
        window_height - px(TOP_BAR_HEIGHT) - px(MIN_CHAT_PANE_HEIGHT) - px(LIVE_PANE_DIVIDER_SIZE);
    let min_height =
        px(MIN_LIVE_PANE_HEIGHT).min(available.max(px(MIN_CONSTRAINED_LIVE_PANE_HEIGHT)));
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
        ClosePreview
    ]
);

pub fn bind_keys(cx: &mut App) {
    crate::composer::bind_keys(cx);
    cx.bind_keys([
        KeyBinding::new("cmd-c", markdown::Copy, Some("Markdown")),
        KeyBinding::new("cmd-o", OpenMedia, Some("Chatt")),
        KeyBinding::new("escape", ClosePreview, Some("Chatt && !ChattComposer")),
        KeyBinding::new("enter", SendMessage, Some("ChattComposer")),
        KeyBinding::new("space", TogglePlayback, Some("Chatt && !ChattComposer")),
        KeyBinding::new("left", SeekBack, Some("Chatt")),
        KeyBinding::new("right", SeekForward, Some("Chatt")),
        KeyBinding::new("=", LiveZoomIn, Some("Chatt")),
        KeyBinding::new("-", LiveZoomOut, Some("Chatt")),
        KeyBinding::new("home", LiveReset, Some("Chatt")),
        KeyBinding::new("up", LivePanUp, Some("Chatt")),
        KeyBinding::new("down", LivePanDown, Some("Chatt")),
    ]);
}

pub struct ChattView {
    model: ChatModel,
    daemon: DaemonClient,
    next_request_id: u64,
    next_transfer_id: u64,
    editing: Option<(RoomId, local_rpc::ids::MessageId, String)>,
    composer: gpui::Entity<Composer>,
    media_cache: Arc<Mutex<MediaCache>>,
    image_cache: gpui::Entity<LruImageCache<TimelineImageLoader>>,
    preview_image_cache: gpui::Entity<LruImageCache<PreviewImageLoader>>,
    eager_image_fetches: EagerImageFetches,
    preview_history: PreviewHistory,
    preview_image: ImageViewState,
    preview_image_viewport: Rc<Cell<Option<Bounds<Pixels>>>>,
    preview_last_mouse_position: Option<Point<Pixels>>,
    preview_panel_width: Pixels,
    preview_pane_resize: Option<PreviewPaneResize>,
    list_state: ListState,
    pending_scroll: gpui::Pixels,
    scroll_animation_active: bool,
    last_scroll_frame: Option<Instant>,
    message_markdown: HashMap<u64, gpui::Entity<Markdown>>,
    timeline_selection: MarkdownSelectionGroup,
    videos: AttachmentVideoManager,
    video_thumbnails: VideoThumbnailCache,
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
        let image_cache = LruImageCache::<TimelineImageLoader>::new(DECODED_IMAGE_CACHE_BYTES, cx);
        let preview_image_cache =
            LruImageCache::<PreviewImageLoader>::new(PREVIEW_IMAGE_CACHE_BYTES, cx);
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
        let videos = AttachmentVideoManager::new(video_wakeup.clone());
        let video_thumbnails =
            VideoThumbnailCache::new(VIDEO_THUMBNAIL_CACHE_BYTES, video_wakeup.clone());
        Self {
            model,
            daemon,
            next_request_id: 1,
            next_transfer_id: 1,
            editing: None,
            composer,
            media_cache,
            image_cache,
            preview_image_cache,
            eager_image_fetches: EagerImageFetches::default(),
            preview_history: PreviewHistory::default(),
            preview_image: ImageViewState::default(),
            preview_image_viewport: Rc::new(Cell::new(None)),
            preview_last_mouse_position: None,
            preview_panel_width: px(DEFAULT_PANEL_WIDTH),
            preview_pane_resize: None,
            list_state,
            pending_scroll: px(0.),
            scroll_animation_active: false,
            last_scroll_frame: None,
            message_markdown: HashMap::new(),
            timeline_selection,
            videos,
            video_thumbnails,
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

    fn advance_video(&mut self, cx: &mut Context<Self>) {
        let drain = self.videos.drain();
        let thumbnails_changed = self.video_thumbnails.drain_results();
        self.apply_video_drain(drain);
        if thumbnails_changed {
            cx.notify();
        }
    }

    fn apply_video_drain(&mut self, drain: VideoDrain) {
        for error in &drain.errors {
            log::error!("embedded video failed: {error}");
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
        let mut visible = self
            .model
            .messages
            .get(range)
            .unwrap_or_default()
            .iter()
            .filter_map(message_video_key)
            .collect::<HashSet<_>>();
        if let Some(theater) = self.theater_video.as_ref() {
            visible.insert(theater.key);
        }
        let drain = self.videos.update_visibility(&visible);
        let changed = drain.changed || !drain.errors.is_empty();
        self.apply_video_drain(drain);
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
        let start_height =
            clamp_live_pane_height(bounds.size.height, window.viewport_size().height);
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
            DaemonEvent::Discovering => self.model.phase = ConnectionPhase::Discovering,
            DaemonEvent::Connecting => self.model.phase = ConnectionPhase::Connecting,
            DaemonEvent::TransportConnected => self.model.phase = ConnectionPhase::Syncing,
            DaemonEvent::Disconnected(reason) => {
                log::error!("daemon disconnected: {reason}");
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
                self.release_live_players(window);
            }
            DaemonEvent::Incompatible(details) => {
                log::error!("daemon connection is incompatible: {details}");
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
                log::error!(
                    "upload preparation failed begin_request={} finish_request={}: {reason}",
                    begin_request.0,
                    finish_request.0,
                );
                self.model.pending.remove(&begin_request);
                self.model.pending.remove(&finish_request);
                self.status = format!("Could not prepare upload · {reason}").into();
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
                self.pump_eager_image_fetches(cx);
            }
            DaemonEvent::MediaTransferFailed {
                transfer_id,
                reason,
            } => {
                log::error!(
                    "attachment transfer failed transfer_id={}: {reason}",
                    transfer_id.0,
                );
                self.media_cache
                    .lock()
                    .expect("media cache lock poisoned")
                    .cancel(transfer_id);
                self.eager_image_fetches.failed(transfer_id, reason.clone());
                self.status = reason.into();
                self.pump_eager_image_fetches(cx);
            }
            DaemonEvent::Frame(frame) => {
                self.apply_daemon_state_frame(frame, window, cx);
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
        let old_daemon_instance = self.model.daemon_instance;
        let old_active_server = self.model.active_server.clone();
        let effect = reducer::apply(&mut self.model, frame);
        let media_namespace_changed = self.model.daemon_instance != old_daemon_instance
            || self.model.active_server != old_active_server;
        if media_namespace_changed {
            self.media_cache
                .lock()
                .expect("media cache lock poisoned")
                .clear();
            self.image_cache
                .update(cx, |cache, cx| cache.clear(window, cx));
            self.preview_image_cache
                .update(cx, |cache, cx| cache.clear(window, cx));
            self.eager_image_fetches.clear();
            self.preview_history.clear();
            self.preview_image_viewport.set(None);
            self.preview_last_mouse_position = None;
            self.preview_pane_resize = None;
            self.videos.clear_sessions();
            self.video_thumbnails.clear();
            self.clear_video_interactions();
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
        } else if !effect.splices.is_empty() {
            self.timeline_selection.retain_items(
                self.model
                    .messages
                    .iter()
                    .map(|message| MarkdownSelectionKey(message.id)),
            );
        }
        if self.model.selected_room != old_selected_room {
            self.videos.clear_sessions();
            self.clear_video_interactions();
            self.message_markdown.clear();
            self.eager_image_fetches.reset_transient();
        } else if effect.messages_changed {
            let retained = self
                .model
                .messages
                .iter()
                .filter_map(message_video_key)
                .collect::<HashSet<_>>();
            let drain = self.videos.retain_sources(&retained);
            self.apply_video_drain(drain);
            if self
                .theater_video
                .as_ref()
                .is_some_and(|theater| !retained.contains(&theater.key))
            {
                self.clear_video_interactions();
            }
        }
        if effect.replace_messages {
            self.list_state
                .splice(0..old_len, self.model.messages.len());
        }
        for (start, end, count) in effect.splices {
            self.list_state.splice(start..end, count);
        }
        if effect.messages_changed {
            self.enqueue_new_image_fetches(window, cx);
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
                    log::info!(
                        "daemon result applied request_id={} operation={:?} outcome=accepted",
                        result.request_id.0,
                        result.operation,
                    );
                    self.status = format!("{} accepted", operation_label(&result.operation)).into();
                    if let Some(pending) = pending.as_ref()
                        && pending.operation == Operation::EditMessage
                        && pending.draft.as_deref() == Some(self.composer.read(cx).text().as_str())
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
        message_id: local_rpc::ids::MessageId,
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

    fn open_image_preview(&mut self, descriptor: AttachmentDescriptor, cx: &mut Context<Self>) {
        let Some(path) = self
            .media_cache
            .lock()
            .expect("media cache lock poisoned")
            .path_for(&descriptor)
        else {
            self.status = format!("{} is not cached yet", descriptor.file_name).into();
            cx.notify();
            return;
        };
        let natural_size = match (descriptor.width, descriptor.height) {
            (Some(width), Some(height)) if width > 0 && height > 0 => (width, height),
            _ => image::image_dimensions(&path).unwrap_or((640, 480)),
        };
        let item = PreviewItem::new(descriptor, natural_size);
        if self.preview_history.open(item.clone()) {
            self.preview_image.reset(item.natural_size);
            self.preview_image_viewport.set(None);
            self.preview_last_mouse_position = None;
        }
        cx.notify();
    }

    fn select_preview(&mut self, key: AttachmentId, cx: &mut Context<Self>) {
        if self.preview_history.select(key) {
            if let Some(item) = self.preview_history.active() {
                self.preview_image.reset(item.natural_size);
            }
            self.preview_image_viewport.set(None);
            self.preview_last_mouse_position = None;
            cx.notify();
        }
    }

    fn close_preview(&mut self, cx: &mut Context<Self>) {
        if self.preview_history.active().is_none() {
            return;
        }
        self.preview_history.close_panel();
        self.preview_image_viewport.set(None);
        self.preview_last_mouse_position = None;
        self.preview_pane_resize = None;
        cx.notify();
    }

    fn close_preview_action(&mut self, _: &ClosePreview, _: &mut Window, cx: &mut Context<Self>) {
        if self.exit_video_theater(cx) {
            return;
        }
        self.close_preview(cx);
    }

    fn close_preview_tab(&mut self, key: AttachmentId, cx: &mut Context<Self>) {
        if self.preview_history.close_tab(key) {
            if let Some(item) = self.preview_history.active() {
                self.preview_image.reset(item.natural_size);
            }
            self.preview_image_viewport.set(None);
            self.preview_last_mouse_position = None;
        }
        cx.notify();
    }

    fn save_preview_image(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = self.preview_history.active().cloned() else {
            return;
        };
        let Some(source) = self
            .media_cache
            .lock()
            .expect("media cache lock poisoned")
            .path_for(&item.descriptor)
        else {
            self.status = format!("{} is no longer cached", item.descriptor.file_name).into();
            cx.notify();
            return;
        };
        let directory = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let receiver = cx.prompt_for_new_path(&directory, Some(&item.descriptor.file_name));
        let executor = cx.background_executor().clone();
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(destination))) = receiver.await else {
                return;
            };
            let result = executor
                .spawn(async move { fs::copy(source, &destination).map(|_| destination) })
                .await;
            let _ = this.update_in(cx, |this, _, cx| {
                match result {
                    Ok(destination) => {
                        this.status = format!("Saved image to {}", destination.display()).into()
                    }
                    Err(error) => this.status = format!("Could not save image · {error}").into(),
                }
                cx.notify();
            });
        })
        .detach();
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
        let body_width = window.viewport_size().width - px(SIDEBAR_WIDTH);
        self.preview_panel_width = clamp_panel_width(self.preview_panel_width, body_width);
        self.preview_pane_resize = Some(PreviewPaneResize {
            start_x: event.position.x,
            start_width: self.preview_panel_width,
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
        let body_width = window.viewport_size().width - px(SIDEBAR_WIDTH);
        self.preview_panel_width = clamp_panel_width(
            resize.start_width + resize.start_x - event.position.x,
            body_width,
        );
        cx.stop_propagation();
        cx.notify();
    }

    fn finish_preview_pane_resize(&mut self, cx: &mut Context<Self>) {
        if self.preview_pane_resize.take().is_some() {
            cx.notify();
        }
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
                let markdown =
                    cx.new(|cx| Markdown::new(message.body.clone().into(), None, None, cx));
                self.message_markdown.insert(message.id, markdown.clone());
                markdown
            }
        };
        let mut markdown_style = MarkdownStyle::simple(window, cx);
        markdown_style.base_text_style.color = rgb(0xd7d9dd).into();
        markdown_style.selection_background_color = rgba(0x5277a866).into();
        let sender = message.sender.clone();
        let edited = message.edited;
        let unverified = message.unverified;
        let timestamp_ms = message.timestamp_ms;
        let hover_group: SharedString = format!("message-actions-{message_id}").into();
        let edit = (message.local && !message.notice).then(|| {
            (
                message.room_id,
                local_rpc::ids::MessageId(message.id),
                message.body.clone(),
            )
        });
        let attachment = message.attachment.clone();
        div()
            .id(("message", message_id as usize))
            .group(hover_group.clone())
            .relative()
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
                                        .pr(px(66.))
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
                                        .child(div().text_xs().text_color(rgb(0x777d87)).child(
                                            timeline::format_age(timestamp_ms, timeline::now_ms()),
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
                                content.child(
                                    self.render_attachment(
                                        room_id, message_id, attachment, window, cx,
                                    ),
                                )
                            }),
                    ),
            )
            .when_some(edit, |row, (room_id, edit_id, edit_body)| {
                row.child(
                    div()
                        .absolute()
                        .top(px(if continuation { 1. } else { 7. }))
                        .right(px(28.))
                        .flex()
                        .gap_1()
                        .invisible()
                        .group_hover(hover_group, |actions| actions.visible())
                        .child(
                            message_action_button(
                                ("edit", message_id as usize),
                                IconName::Pencil,
                                false,
                            )
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.begin_edit(room_id, edit_id, edit_body.clone(), cx)
                                },
                            )),
                        )
                        .child(
                            message_action_button(
                                ("delete", message_id as usize),
                                IconName::Trash,
                                true,
                            )
                            .on_click(cx.listener(
                                move |this, _, _, cx| this.delete_message(room_id, edit_id, cx),
                            )),
                        ),
                )
            })
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
            let preview = descriptor.clone();
            return image_frame(&descriptor)
                .id(("image-frame", message_id as usize))
                .mt_2()
                .overflow_hidden()
                .cursor_pointer()
                .hover(|image| image.opacity(0.88))
                .on_click(
                    cx.listener(move |this, _, _, cx| this.open_image_preview(preview.clone(), cx)),
                )
                .child(
                    img(path)
                        .image_cache(&self.image_cache)
                        .id(("image", message_id as usize))
                        .absolute()
                        .inset_0()
                        .size_full()
                        .object_fit(ObjectFit::Contain),
                )
                .into_any_element();
        }
        if attachment.is_image() {
            let fetch = EagerImageFetch::new(room_id, descriptor.clone());
            if let Some(transfer_id) = active_transfer {
                let action = mini_button(("cancel-image-read", transfer_id.0 as usize), "Cancel")
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
            let key = video_key(room_id, message_id, &descriptor);
            self.videos.ensure_source(key, path.clone());
            return self.render_attachment_video(key, descriptor, path, false, cx);
        }
        let fetch = descriptor.clone();
        div()
            .id(("attachment", message_id as usize))
            .mt_2()
            .px_3()
            .py_2()
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
        image_frame(descriptor)
            .id(("image-status", message_id as usize))
            .mt_2()
            .px_3()
            .py_2()
            .flex()
            .items_center()
            .justify_center()
            .gap_3()
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
                let mut cache = self.media_cache.lock().expect("media cache lock poisoned");
                cache.path_for(&descriptor).is_some()
                    || cache.active_transfer(&descriptor).is_some()
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
            if cache.path_for(&descriptor).is_some() || cache.active_transfer(&descriptor).is_some()
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

    fn render_preview_panel(
        &mut self,
        active: PreviewItem,
        width: Pixels,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let active_key = active.key();
        let history = self.preview_history.items().to_vec();
        let mut tabs = div()
            .id("preview-tabs")
            .h_full()
            .flex_1()
            .min_w_0()
            .flex()
            .overflow_x_scroll();
        for item in history {
            let key = item.key();
            let selected = key == active_key;
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
                    .max_w(px(210.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .border_r_1()
                    .border_color(rgb(0x272a30))
                    .bg(rgb(if selected { 0x111317 } else { 0x191c21 }))
                    .text_color(rgb(if selected { 0xaebce0 } else { 0x8b929d }))
                    .hover(|tab| tab.bg(rgb(0x15181c)).text_color(rgb(0xd9dbe0)))
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
                            .child(div().flex_none().text_xs().child("▧"))
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_xs()
                                    .child(item.descriptor.file_name),
                            )
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.select_preview(select_key, cx)
                            })),
                    )
                    .child(
                        div()
                            .id(close_id)
                            .w(px(28.0))
                            .h_full()
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .cursor_pointer()
                            .text_sm()
                            .hover(|button| button.bg(rgb(0x20242a)).text_color(rgb(0xe4e6ea)))
                            .child("×")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.close_preview_tab(close_key, cx)
                            })),
                    ),
            );
        }

        let cache_path = self
            .media_cache
            .lock()
            .expect("media cache lock poisoned")
            .path_for(&active.descriptor);
        let cache_missing = cache_path.is_none();
        let viewport = self.preview_image_viewport.get().unwrap_or(Bounds {
            origin: point(
                window.viewport_size().width - width,
                px(TOP_BAR_HEIGHT + 80.0),
            ),
            size: gpui::size(
                width,
                (window.viewport_size().height - px(TOP_BAR_HEIGHT + 80.0)).max(px(1.0)),
            ),
        });
        let geometry = self.preview_image.geometry(viewport);
        let zoom_percent = self.preview_image.zoom_percent(viewport);
        let can_pan = self.preview_image.can_pan(viewport);
        let panning = self.preview_last_mouse_position.is_some() && can_pan;
        let measured_viewport = self.preview_image_viewport.clone();

        let viewport_element = div()
            .id("preview-image-viewport")
            .relative()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .bg(rgb(0x08090b))
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
            .child(
                canvas(
                    move |bounds, window, _| {
                        if measured_viewport.get() != Some(bounds) {
                            measured_viewport.set(Some(bounds));
                            window.request_animation_frame();
                        }
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .when_some(cache_path, |viewport_element, path| {
                viewport_element.child(
                    img(path)
                        .image_cache(&self.preview_image_cache)
                        .absolute()
                        .left(geometry.bounds.origin.x - viewport.origin.x)
                        .top(geometry.bounds.origin.y - viewport.origin.y)
                        .w(geometry.bounds.size.width)
                        .h(geometry.bounds.size.height)
                        .object_fit(ObjectFit::Contain)
                        .with_loading(|| {
                            div()
                                .size_full()
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_sm()
                                .text_color(rgb(0x8b929d))
                                .child("loading…")
                                .into_any_element()
                        })
                        .with_fallback(|| {
                            div()
                                .size_full()
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_sm()
                                .text_color(rgb(0x8b929d))
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
                        .text_color(rgb(0x8b929d))
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
            .bg(rgb(0x111317))
            .child(
                div()
                    .h(px(39.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .min_w_0()
                    .border_b_1()
                    .border_color(rgb(0x272a30))
                    .bg(rgb(0x191c21))
                    .child(tabs)
                    .child(
                        div()
                            .h_full()
                            .flex_none()
                            .flex()
                            .items_center()
                            .gap_1()
                            .px_2()
                            .bg(rgb(0x191c21))
                            .child(
                                preview_action_button("preview-save", IconName::Download).on_click(
                                    cx.listener(|this, _, window, cx| {
                                        this.save_preview_image(window, cx)
                                    }),
                                ),
                            )
                            .child(
                                preview_action_button("preview-close", IconName::Close)
                                    .on_click(cx.listener(|this, _, _, cx| this.close_preview(cx))),
                            ),
                    ),
            )
            .child(
                div()
                    .h(px(41.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .border_b_1()
                    .border_color(rgb(0x272a30))
                    .bg(rgb(0x111317))
                    .child(
                        preview_control_button("preview-fit", "Fit")
                            .on_click(cx.listener(|this, _, _, cx| this.fit_preview_image(cx))),
                    )
                    .child(
                        preview_control_button("preview-actual", "100%").on_click(
                            cx.listener(|this, _, _, cx| this.actual_size_preview_image(cx)),
                        ),
                    )
                    .child(
                        preview_control_button("preview-zoom-out", "−").on_click(
                            cx.listener(|this, _, _, cx| this.zoom_preview_image(-0.25, cx)),
                        ),
                    )
                    .child(
                        div()
                            .w(px(56.0))
                            .text_center()
                            .text_xs()
                            .text_color(rgb(0x8b929d))
                            .child(format!("{zoom_percent}%")),
                    )
                    .child(
                        preview_control_button("preview-zoom-in", "+").on_click(
                            cx.listener(|this, _, _, cx| this.zoom_preview_image(0.25, cx)),
                        ),
                    ),
            )
            .child(viewport_element)
    }

    fn render_attachment_video(
        &mut self,
        key: VideoKey,
        descriptor: AttachmentDescriptor,
        path: PathBuf,
        theater: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.videos.ensure_source(key, path.clone());
        let video = self.videos.view(key);
        let thumbnail = self.video_thumbnails.request(
            ThumbnailKey {
                attachment_id: descriptor.id,
            },
            path.clone(),
        );
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
        let scrub_hover_fraction = active_controls.then(|| {
            active_scrub
                .map(|scrub| scrub.last_fraction)
                .or(self.video_controls.scrub_hover_fraction)
        }).flatten();
        let source = TheaterVideo {
            key,
            descriptor: descriptor.clone(),
            path,
        };
        let view = cx.entity().downgrade();
        let event_source = source.clone();
        let handler: VideoPlayerHandler = Rc::new(move |event, _, cx| {
            let source = event_source.clone();
            let _ = view.update(cx, |this, cx| {
                this.handle_video_player_event(source, duration, event, cx)
            });
        });
        let fallback_label = video
            .error
            .clone()
            .unwrap_or_else(|| descriptor.file_name.clone());
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
            VideoPlayerEvent::PlayerHovered(hovered) => {
                self.hover_video_player(key, hovered, cx)
            }
            VideoPlayerEvent::PointerMoved => self.video_pointer_moved(key, cx),
            VideoPlayerEvent::SurfaceClicked(click_count) => {
                self.click_video_surface(source, click_count, cx)
            }
            VideoPlayerEvent::Play => self.play_video(key, cx),
            VideoPlayerEvent::ScrubHovered(fraction) => {
                self.hover_video_scrub(key, fraction, cx)
            }
            VideoPlayerEvent::ScrubHoverCleared => self.clear_video_scrub_hover(key, cx),
            VideoPlayerEvent::ScrubPressed { bounds, event } => {
                self.begin_video_scrub(key, duration, bounds, &event, cx)
            }
            VideoPlayerEvent::ControlsHovered(hovered) => {
                self.hover_video_controls(key, hovered, cx)
            }
            VideoPlayerEvent::VolumeHovered(hovered) => {
                self.hover_video_volume(key, hovered, cx)
            }
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
        self.show_video_controls(key, cx);
        match self.videos.play(key) {
            Ok(()) => self.status = "Starting cached attachment…".into(),
            Err(error) => {
                log::error!("embedded video play failed key={key:?}: {error:#}");
                self.status = format!("Playback failed: {error}").into();
            }
        }
        self.schedule_video_controls_hide(key, cx);
        cx.notify();
    }

    fn seek_video(&mut self, key: VideoKey, seconds: f64, cx: &mut Context<Self>) {
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
                if this.video_controls.active_key == Some(key)
                    && !this.video_controls_pinned(key)
                {
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
        if self.theater_video.as_ref().is_some_and(|active| active.key == source.key) {
            self.exit_video_theater(cx);
            return;
        }
        self.theater_video = Some(source.clone());
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
        cx.notify();
        true
    }

    fn hover_video_scrub(
        &mut self,
        key: VideoKey,
        fraction: f64,
        cx: &mut Context<Self>,
    ) {
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
            log::error!("embedded video scrub failed key={key:?} fraction={fraction}: {error:#}");
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
                && let Err(error) = self.videos.scrub(
                    scrub.key,
                    fraction,
                    scrub.duration,
                    SeekMode::Keyframes,
                )
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
        if let Err(error) = self.videos.scrub(
            scrub.key,
            scrub.last_fraction,
            scrub.duration,
            SeekMode::Exact,
        ) {
            log::error!(
                "embedded video final scrub failed key={:?} fraction={}: {error:#}",
                scrub.key,
                scrub.last_fraction,
            );
            self.status = format!("Seek failed: {error}").into();
        }
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
        if let Err(error) = self.videos.set_volume_for(key, volume) {
            log::error!("embedded video volume change failed key={key:?} volume={volume}: {error:#}");
            self.status = format!("Volume failed: {error}").into();
        }
        self.show_video_controls(key, cx);
        cx.notify();
    }

    fn toggle_video_mute(&mut self, key: VideoKey, cx: &mut Context<Self>) {
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
        if let Err(error) = self.videos.adjust_volume(key, delta) {
            log::error!("embedded video volume change failed key={key:?} delta={delta}: {error:#}");
            self.status = format!("Volume failed: {error}").into();
        }
        self.show_video_controls(key, cx);
        cx.notify();
    }

    fn toggle_playback(&mut self, _: &TogglePlayback, _: &mut Window, cx: &mut Context<Self>) {
        let key = self
            .theater_video
            .as_ref()
            .map(|theater| theater.key)
            .or_else(|| self.videos.last_visible_key());
        if let Some(key) = key {
            self.play_video(key, cx);
        }
    }
    fn seek_back(&mut self, _: &SeekBack, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(stream_id) = self.fullscreen_share {
            self.pan_live_view(stream_id, 30.0, 0.0, cx);
        } else if let Some(theater) = self.theater_video.as_ref() {
            self.seek_video(theater.key, -10.0, cx);
        } else if let Some(key) = self.videos.last_visible_key() {
            self.seek_video(key, -10.0, cx);
        }
    }
    fn seek_forward(&mut self, _: &SeekForward, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(stream_id) = self.fullscreen_share {
            self.pan_live_view(stream_id, -30.0, 0.0, cx);
        } else if let Some(theater) = self.theater_video.as_ref() {
            self.seek_video(theater.key, 10.0, cx);
        } else if let Some(key) = self.videos.last_visible_key() {
            self.seek_video(key, 10.0, cx);
        }
    }

    fn render_live_shares(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Div {
        let shares = self.model.live_shares.clone();
        let resizable = !self.live_players.is_empty();
        let pane_height = resizable
            .then_some(self.live_pane_height)
            .flatten()
            .map(|height| clamp_live_pane_height(height, window.viewport_size().height));
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
            .border_color(rgb(0x272a30))
            .bg(rgb(0x14161a))
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
                    .h(px(LIVE_PANE_DIVIDER_SIZE))
                    .cursor_row_resize()
                    .hover(|handle| handle.bg(rgba(0x53698766)))
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
            .bg(rgb(0x08090b))
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
                        .items_center()
                        .gap_1()
                        .px_3()
                        .py_2()
                        .bg(rgb(0x111317))
                        .child(live_share_title(&share))
                        .child(
                            icon_button(("live-stop", stream_id.0 as usize), IconName::Stop)
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    if this.fullscreen_share == Some(stop_id)
                                        && window.is_fullscreen()
                                    {
                                        window.toggle_fullscreen();
                                    }
                                    this.stop_live_share(stop_id, cx)
                                })),
                        )
                        .child(
                            icon_button(("live-reset", stream_id.0 as usize), IconName::RotateCcw)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.reset_live_view(reset_id, cx)
                                })),
                        )
                        .child(
                            icon_button(("live-zoom-out", stream_id.0 as usize), IconName::ZoomOut)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.zoom_live_view(zoom_out_id, 1.0 / 1.25, cx)
                                })),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(0x8b929d))
                                .child(format!("{:.0}%", zoom * 100.0)),
                        )
                        .child(
                            icon_button(("live-zoom-in", stream_id.0 as usize), IconName::ZoomIn)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.zoom_live_view(zoom_in_id, 1.25, cx)
                                })),
                        )
                        .child(
                            icon_button(
                                ("live-fullscreen", stream_id.0 as usize),
                                if fullscreen {
                                    IconName::Minimize
                                } else {
                                    IconName::Maximize
                                },
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
                        .when(!fullscreen && !constrained, |viewport| viewport.h(px(320.)))
                        .bg(rgb(0x08090b))
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
                    .items_center()
                    .gap_1()
                    .px_3()
                    .py_2()
                    .bg(rgb(0x111317))
                    .child(live_share_title(&share))
                    .child(
                        icon_button(("live-play", stream_id.0 as usize), IconName::Play).on_click(
                            cx.listener(move |this, _, _, cx| this.start_live_share(stream_id, cx)),
                        ),
                    ),
            );
        }
        card
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
        if self.scroll_video_volume(event, cx) {
            return true;
        }
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
}

impl Render for ChattView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.advance_video(cx);
        if !self.live_players.is_empty() {
            self.advance_live_video();
        }
        if let Some(theater) = self.theater_video.clone() {
            let player = self.render_attachment_video(
                theater.key,
                theater.descriptor,
                theater.path,
                true,
                cx,
            );
            return div()
                .id("chatt-video-theater")
                .key_context("Chatt")
                .on_action(cx.listener(Self::toggle_playback))
                .on_action(cx.listener(Self::seek_back))
                .on_action(cx.listener(Self::seek_forward))
                .on_action(cx.listener(Self::close_preview_action))
                .on_scroll_wheel(cx.listener(
                    |this, event: &ScrollWheelEvent, _, cx| {
                        if this.scroll_video_volume(event, cx) {
                            cx.stop_propagation();
                        }
                    },
                ))
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
                .bg(rgb(0x08090b))
                .text_color(rgb(0xd9dbe0))
                .child(player);
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
                .on_action(cx.listener(Self::seek_back))
                .on_action(cx.listener(Self::seek_forward))
                .on_action(cx.listener(Self::live_zoom_in_action))
                .on_action(cx.listener(Self::live_zoom_out_action))
                .on_action(cx.listener(Self::live_reset_action))
                .on_action(cx.listener(Self::live_pan_up_action))
                .on_action(cx.listener(Self::live_pan_down_action))
                .size_full()
                .bg(rgb(0x08090b))
                .child(card);
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
        let live_panel =
            (!self.model.live_shares.is_empty()).then(|| self.render_live_shares(window, cx));
        let resizing_live_pane = self.live_pane_resize.is_some();
        let active_preview = self.preview_history.active().cloned();
        let preview_panel = active_preview.map(|active| {
            let body_width = window.viewport_size().width - px(SIDEBAR_WIDTH);
            self.preview_panel_width = clamp_panel_width(self.preview_panel_width, body_width);
            self.render_preview_panel(active, self.preview_panel_width, window, cx)
        });
        let resizing_preview_pane = self.preview_pane_resize.is_some();
        div()
            .id("chatt")
            .key_context("Chatt")
            .on_action(cx.listener(Self::open_media))
            .on_action(cx.listener(Self::send_message))
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
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                this.queue_uploads(paths.0.to_vec(), cx)
            }))
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, window, cx| {
                if !this.drag_video_volume(event, cx) && !this.drag_video_scrub(event, cx) {
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
                    this.finish_video_scrub(cx);
                    this.finish_video_volume_drag(cx);
                    this.finish_live_pane_resize(cx);
                    this.finish_preview_pane_resize(cx);
                    this.finish_preview_image_pan(cx)
                }),
            )
            .size_full()
            .flex()
            .bg(rgb(0x111317))
            .text_color(rgb(0xd9dbe0))
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
                                    toolbar_button("retry", Some(IconName::RotateCcw), "Retry")
                                        .on_click(cx.listener(|this, _, _, cx| this.retry(cx))),
                                )
                            })
                            .child(
                                toolbar_button(
                                    "mute",
                                    Some(if self.model.voice.muted {
                                        IconName::MicOff
                                    } else {
                                        IconName::Mic
                                    }),
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
                                    Some(if self.model.voice.deafened {
                                        IconName::AudioOff
                                    } else {
                                        IconName::AudioOn
                                    }),
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
                            .child(toolbar_button("output-down", None, "Vol −").on_click(
                                cx.listener(|this, _, _, cx| this.adjust_output_volume(-5., cx)),
                            ))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(0x8b929d))
                                    .child(format!("{}", self.model.voice.output_volume.round())),
                            )
                            .child(toolbar_button("output-up", None, "Vol +").on_click(
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
                                )
                                .on_click(cx.listener(
                                    |this, _, window, cx| {
                                        this.toggle_voice(&ToggleVoice, window, cx)
                                    },
                                )),
                            ),
                    )
                    .when_some(live_panel, |panel, live_panel| panel.child(live_panel))
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
                                        toolbar_button(
                                            "load-older",
                                            Some(IconName::Download),
                                            "Load older messages",
                                        )
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
                            .flex()
                            .items_center()
                            .pl(px(28.))
                            .pr(px(28.))
                            .py_2()
                            .border_t_1()
                            .border_color(rgb(0x272a30))
                            .bg(rgb(0x14161a))
                            .child(
                                div()
                                    .w(px(36.))
                                    .h(px(40.))
                                    .flex_none()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .mr(px(15.))
                                    .child(composer_add_button(ready).on_click(cx.listener(
                                        |this, _, window, cx| {
                                            this.open_media(&OpenMedia, window, cx)
                                        },
                                    ))),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .min_h(px(40.))
                                    .flex_1()
                                    .flex()
                                    .items_center()
                                    .child(self.composer.clone()),
                            ),
                    ),
            )
            .when_some(preview_panel, |root, preview_panel| {
                root.child(
                    div()
                        .id("preview-pane-resize")
                        .w(px(PREVIEW_DIVIDER_WIDTH))
                        .h_full()
                        .flex_none()
                        .flex()
                        .justify_center()
                        .cursor_col_resize()
                        .hover(|divider| divider.bg(rgba(0x53698733)))
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
                        .child(div().w(px(3.0)).h_full().bg(rgb(if resizing_preview_pane {
                            0x536987
                        } else {
                            0x272a30
                        }))),
                )
                .child(preview_panel)
            })
    }
}

fn live_share_title(share: &local_rpc::model::LiveShare) -> Div {
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
                .text_color(rgb(0x747a84))
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

fn image_frame(descriptor: &AttachmentDescriptor) -> Div {
    let (width, height) = image_box_size(descriptor);
    div()
        .relative()
        .w(px(width))
        .max_w_full()
        .aspect_ratio(width / height)
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
        Operation::StartLiveShare => "Screen share playback",
        Operation::StopLiveShare => "Screen share stop",
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

fn toolbar_button(
    id: &'static str,
    icon_name: Option<IconName>,
    label: &'static str,
) -> Stateful<Div> {
    div()
        .id(id)
        .h(px(30.))
        .px_2()
        .flex()
        .items_center()
        .justify_center()
        .gap_1()
        .cursor_pointer()
        .bg(rgb(0x1b1e23))
        .hover(|button| button.bg(rgb(0x292d34)))
        .text_xs()
        .when_some(icon_name, |button, icon_name| {
            button.child(icon(icon_name, 15.0, 0xaeb4bf))
        })
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
        .bg(rgb(0x22262c))
        .hover(|button| button.bg(rgb(0x30353d)))
        .text_xs()
        .child(label)
}

fn icon_button(id: impl Into<gpui::ElementId>, icon_name: IconName) -> Stateful<Div> {
    div()
        .id(id)
        .size(px(28.))
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .bg(rgb(0x202329))
        .text_color(rgb(0xb4b9c2))
        .hover(|button| button.bg(rgb(0x536987)).text_color(rgb(0xf0f2f5)))
        .child(icon(icon_name, 17.0, 0xd0d4dc))
}

fn message_action_button(
    id: impl Into<gpui::ElementId>,
    icon_name: IconName,
    destructive: bool,
) -> Stateful<Div> {
    div()
        .id(id)
        .size(px(28.))
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .bg(rgb(0x202329))
        .text_color(rgb(0x8b929d))
        .hover(move |button| {
            button
                .bg(rgb(0x30353d))
                .text_color(rgb(if destructive { 0xd99a93 } else { 0xe4e6ea }))
        })
        .child(icon(
            icon_name,
            16.0,
            if destructive { 0xb9827d } else { 0xadb3bd },
        ))
}

fn composer_add_button(ready: bool) -> Stateful<Div> {
    div()
        .id("add-media")
        .size(px(36.))
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .text_color(rgb(if ready { 0x8b929d } else { 0x555a63 }))
        .hover(|button| button.text_color(rgb(0xd9dbe0)))
        .child(icon(
            IconName::Plus,
            24.0,
            if ready { 0x8b929d } else { 0x555a63 },
        ))
}

fn preview_action_button(id: &'static str, icon_name: IconName) -> Stateful<Div> {
    div()
        .id(id)
        .w(px(28.0))
        .h(px(28.0))
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .text_color(rgb(0x8b929d))
        .hover(|button| button.bg(rgb(0x111317)).text_color(rgb(0xd9dbe0)))
        .child(icon(icon_name, 17.0, 0x9ba1ab))
}

fn preview_control_button(id: &'static str, label: &'static str) -> Stateful<Div> {
    div()
        .id(id)
        .min_w(px(32.0))
        .h(px(28.0))
        .px_2()
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .bg(rgb(0x202329))
        .hover(|button| button.bg(rgb(0x536987)))
        .text_xs()
        .child(label)
}

fn video_key(room_id: RoomId, message_id: u64, descriptor: &AttachmentDescriptor) -> VideoKey {
    VideoKey {
        room_id,
        message_id,
        attachment_id: descriptor.id,
    }
}

fn message_video_key(message: &timeline::Message) -> Option<VideoKey> {
    let attachment = message.attachment.as_ref()?;
    attachment
        .is_video()
        .then(|| video_key(message.room_id, message.id, &attachment.descriptor))
}

#[cfg(test)]
mod tests {
    use super::*;
    use local_rpc::model::MediaKind;

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
            clamp_live_pane_height(px(100.0), px(900.0)),
            px(MIN_LIVE_PANE_HEIGHT),
        );
        assert_eq!(clamp_live_pane_height(px(900.0), px(900.0)), px(699.0),);
        assert_eq!(clamp_live_pane_height(px(900.0), px(300.0)), px(99.0),);
    }
}
