use std::sync::Arc;

use gpui::{Bounds, Pixels, Point, UniformListScrollHandle, point, px, size};
use local_rpc::model::{AttachmentDescriptor, AttachmentId, BulkTransferId};

use crate::{
    code_viewer::{CodeDocument, CodeViewState},
    scrollbar::OverlayScrollbarState,
};

// TODO: Preview history is count-bounded rather than byte-bounded. Ready code
// previews can retain disproportionately large source and highlighting buffers;
// consider byte-accounted eviction if preview limits grow.
pub const HISTORY_LIMIT: usize = 16;
pub const MIN_PANEL_WIDTH: f32 = 320.0;
pub const MIN_CHAT_WIDTH: f32 = 360.0;
pub const DEFAULT_CHAT_WIDTH: f32 = 800.0;
pub const DIVIDER_WIDTH: f32 = 9.0;
pub const TABBED_LAYOUT_MAX_BODY_WIDTH: f32 = 1300.0;

const MANUAL_ZOOM_MIN: f32 = 0.1;
const MANUAL_ZOOM_MAX: f32 = 8.0;
const DEFAULT_AUTO_FIT_MAX: f32 = 3.0;

#[derive(Clone, Debug)]
pub struct PreviewItem {
    pub descriptor: AttachmentDescriptor,
    pub content: PreviewContent,
}

impl PreviewItem {
    pub fn image(descriptor: AttachmentDescriptor, natural_size: (u32, u32)) -> Self {
        Self {
            descriptor,
            content: PreviewContent::Image {
                natural_size: (natural_size.0.max(1), natural_size.1.max(1)),
            },
        }
    }

    pub fn code(descriptor: AttachmentDescriptor, transfer_id: Option<BulkTransferId>) -> Self {
        Self {
            descriptor,
            content: PreviewContent::Code(CodePreview {
                state: CodePreviewState::Fetching { transfer_id },
                scroll_handle: UniformListScrollHandle::new(),
                view_state: CodeViewState::default(),
                scrollbar_state: OverlayScrollbarState::default(),
            }),
        }
    }

    pub fn key(&self) -> AttachmentId {
        self.descriptor.id
    }

    pub fn image_size(&self) -> Option<(u32, u32)> {
        match self.content {
            PreviewContent::Image { natural_size } => Some(natural_size),
            PreviewContent::Code(_) => None,
        }
    }

    pub fn code_preview(&self) -> Option<&CodePreview> {
        match &self.content {
            PreviewContent::Code(preview) => Some(preview),
            PreviewContent::Image { .. } => None,
        }
    }

    pub fn code_preview_mut(&mut self) -> Option<&mut CodePreview> {
        match &mut self.content {
            PreviewContent::Code(preview) => Some(preview),
            PreviewContent::Image { .. } => None,
        }
    }
}

#[derive(Clone, Debug)]
pub enum PreviewContent {
    Image { natural_size: (u32, u32) },
    Code(CodePreview),
}

#[derive(Clone, Debug)]
pub struct CodePreview {
    pub state: CodePreviewState,
    pub scroll_handle: UniformListScrollHandle,
    pub view_state: CodeViewState,
    pub scrollbar_state: OverlayScrollbarState,
}

#[derive(Clone, Debug)]
pub enum CodePreviewState {
    Fetching { transfer_id: Option<BulkTransferId> },
    Preparing { load_id: u64 },
    Ready(Arc<CodeDocument>),
    Error(String),
}

#[derive(Default)]
pub struct PreviewHistory {
    items: Vec<PreviewItem>,
    active: Option<AttachmentId>,
    /// Set while the chat is showing as the pinned tab rather than the viewer.
    /// Only the tabbed layout can reach it; the split layout has no chat tab,
    /// so there `active` alone decides whether the panel is on screen.
    chat_selected: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PreviewOpenResult {
    pub active_changed: bool,
    pub evicted: Option<AttachmentId>,
}

impl PreviewHistory {
    pub fn reset_code_measurements(&mut self) {
        for item in &mut self.items {
            if let Some(preview) = item.code_preview_mut() {
                preview.view_state.reset();
            }
        }
    }

    pub fn items(&self) -> &[PreviewItem] {
        &self.items
    }

    pub fn active(&self) -> Option<&PreviewItem> {
        let key = self.active?;
        self.items.iter().find(|item| item.key() == key)
    }

    pub fn active_key(&self) -> Option<AttachmentId> {
        self.active().map(PreviewItem::key)
    }

    pub fn item(&self, key: AttachmentId) -> Option<&PreviewItem> {
        self.items.iter().find(|item| item.key() == key)
    }

    pub fn item_mut(&mut self, key: AttachmentId) -> Option<&mut PreviewItem> {
        self.items.iter_mut().find(|item| item.key() == key)
    }

    pub fn fail_code_transfer(&mut self, transfer_id: BulkTransferId, reason: &str) -> bool {
        let mut changed = false;
        for item in &mut self.items {
            let Some(preview) = item.code_preview_mut() else {
                continue;
            };
            if matches!(
                preview.state,
                CodePreviewState::Fetching {
                    transfer_id: Some(active)
                } if active == transfer_id
            ) {
                preview.state = CodePreviewState::Error(reason.to_string());
                changed = true;
            }
        }
        changed
    }

    pub fn fail_code_fetches(&mut self, reason: &str) -> bool {
        let mut changed = false;
        for item in &mut self.items {
            let Some(preview) = item.code_preview_mut() else {
                continue;
            };
            if matches!(preview.state, CodePreviewState::Fetching { .. }) {
                preview.state = CodePreviewState::Error(reason.to_string());
                changed = true;
            }
        }
        changed
    }

    /// Whether the tabbed layout should show the tab bar. Closing the panel
    /// hides it; the tabs themselves are kept for the next preview.
    pub fn tab_bar_visible(&self) -> bool {
        !self.items.is_empty() && (self.active.is_some() || self.chat_selected)
    }

    pub fn open(&mut self, item: PreviewItem) -> PreviewOpenResult {
        let key = item.key();
        let active_changed = self.active_key() != Some(key);
        self.chat_selected = false;
        let promoted = self
            .items
            .iter()
            .find(|candidate| candidate.key() == key)
            .cloned()
            .unwrap_or(item);
        self.items.retain(|candidate| candidate.key() != key);
        self.items.insert(0, promoted);
        let evicted = (self.items.len() > HISTORY_LIMIT).then(|| {
            self.items
                .pop()
                .expect("preview history exceeded its limit")
                .key()
        });
        self.active = Some(key);
        PreviewOpenResult {
            active_changed,
            evicted,
        }
    }

    pub fn select(&mut self, key: AttachmentId) -> bool {
        if !self.items.iter().any(|item| item.key() == key) {
            return false;
        }
        let changed = self.active_key() != Some(key) || self.chat_selected;
        self.active = Some(key);
        self.chat_selected = false;
        changed
    }

    /// Shows the chat in place of the viewer while keeping the tab bar.
    pub fn select_chat(&mut self) -> bool {
        let changed = self.active.is_some() || !self.chat_selected;
        self.active = None;
        self.chat_selected = true;
        changed
    }

    pub fn close_panel(&mut self) -> bool {
        let changed = self.active.is_some() || self.chat_selected;
        self.active = None;
        self.chat_selected = false;
        changed
    }

    pub fn close_tab(&mut self, key: AttachmentId) -> bool {
        let Some(index) = self.items.iter().position(|item| item.key() == key) else {
            return false;
        };
        let was_active = self.active_key() == Some(key);
        self.items.remove(index);
        if was_active {
            self.active = self
                .items
                .get(index)
                .or_else(|| index.checked_sub(1).and_then(|index| self.items.get(index)))
                .map(PreviewItem::key);
        }
        was_active
    }

    pub fn clear(&mut self) {
        self.items.clear();
        self.active = None;
        self.chat_selected = false;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ImageScaleMode {
    Fit,
    Manual,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ImageGeometry {
    pub bounds: Bounds<Pixels>,
    pub scale: f32,
}

#[derive(Clone, Copy, Debug)]
pub struct ImageViewState {
    natural_size: (u32, u32),
    mode: ImageScaleMode,
    manual_scale: f32,
    auto_fit_max_scale: f32,
    pan: Point<Pixels>,
}

impl Default for ImageViewState {
    fn default() -> Self {
        Self {
            natural_size: (1, 1),
            mode: ImageScaleMode::Fit,
            manual_scale: 1.0,
            auto_fit_max_scale: DEFAULT_AUTO_FIT_MAX,
            pan: point(px(0.0), px(0.0)),
        }
    }
}

impl ImageViewState {
    pub fn reset(&mut self, natural_size: (u32, u32)) {
        self.natural_size = (natural_size.0.max(1), natural_size.1.max(1));
        self.mode = ImageScaleMode::Fit;
        self.manual_scale = 1.0;
        self.auto_fit_max_scale = DEFAULT_AUTO_FIT_MAX;
        self.pan = point(px(0.0), px(0.0));
    }

    pub fn scale(&self, viewport: Bounds<Pixels>) -> f32 {
        match self.mode {
            ImageScaleMode::Fit => self.raw_fit_scale(viewport).min(self.auto_fit_max_scale),
            ImageScaleMode::Manual => self.manual_scale,
        }
    }

    pub fn zoom_percent(&self, viewport: Bounds<Pixels>) -> u32 {
        (self.scale(viewport) * 100.0).round().max(1.0) as u32
    }

    pub fn geometry(&self, viewport: Bounds<Pixels>) -> ImageGeometry {
        let scale = self.scale(viewport);
        let pan = self.clamp_pan(self.pan, viewport, scale);
        let image_size = size(
            px(self.natural_size.0 as f32 * scale),
            px(self.natural_size.1 as f32 * scale),
        );
        let center = viewport.center() + pan;
        ImageGeometry {
            bounds: Bounds {
                origin: point(
                    center.x - image_size.width / 2.0,
                    center.y - image_size.height / 2.0,
                ),
                size: image_size,
            },
            scale,
        }
    }

    pub fn fit(&mut self, viewport: Bounds<Pixels>) {
        self.auto_fit_max_scale = self.auto_fit_max_scale.max(self.raw_fit_scale(viewport));
        self.mode = ImageScaleMode::Fit;
        self.pan = point(px(0.0), px(0.0));
    }

    pub fn actual_size(&mut self) {
        self.mode = ImageScaleMode::Manual;
        self.manual_scale = 1.0;
        self.pan = point(px(0.0), px(0.0));
    }

    pub fn zoom_from_center(&mut self, delta: f32, viewport: Bounds<Pixels>) {
        self.zoom_at(self.scale(viewport) + delta, viewport, viewport.center());
    }

    pub fn zoom_by_factor(
        &mut self,
        factor: f32,
        viewport: Bounds<Pixels>,
        focal_point: Point<Pixels>,
    ) {
        self.zoom_at(self.scale(viewport) * factor, viewport, focal_point);
    }

    pub fn pan_by(&mut self, delta: Point<Pixels>, viewport: Bounds<Pixels>) {
        let scale = self.scale(viewport);
        self.pan = self.clamp_pan(self.pan + delta, viewport, scale);
    }

    pub fn can_pan(&self, viewport: Bounds<Pixels>) -> bool {
        let scale = self.scale(viewport);
        self.natural_size.0 as f32 * scale > viewport.size.width.as_f32() + 0.5
            || self.natural_size.1 as f32 * scale > viewport.size.height.as_f32() + 0.5
    }

    fn raw_fit_scale(&self, viewport: Bounds<Pixels>) -> f32 {
        let width = viewport.size.width.as_f32();
        let height = viewport.size.height.as_f32();
        if width <= 0.0 || height <= 0.0 {
            return 1.0;
        }
        (width / self.natural_size.0 as f32).min(height / self.natural_size.1 as f32)
    }

    fn scale_limits(&self, viewport: Bounds<Pixels>) -> (f32, f32) {
        let fit = self.raw_fit_scale(viewport);
        (MANUAL_ZOOM_MIN.min(fit), MANUAL_ZOOM_MAX.max(fit))
    }

    fn pan_limits(&self, viewport: Bounds<Pixels>, scale: f32) -> Point<Pixels> {
        point(
            px(
                ((self.natural_size.0 as f32 * scale - viewport.size.width.as_f32()) / 2.0)
                    .max(0.0),
            ),
            px(
                ((self.natural_size.1 as f32 * scale - viewport.size.height.as_f32()) / 2.0)
                    .max(0.0),
            ),
        )
    }

    fn clamp_pan(&self, pan: Point<Pixels>, viewport: Bounds<Pixels>, scale: f32) -> Point<Pixels> {
        let limits = self.pan_limits(viewport, scale);
        point(
            pan.x.clamp(-limits.x, limits.x),
            pan.y.clamp(-limits.y, limits.y),
        )
    }

    fn zoom_at(&mut self, target_scale: f32, viewport: Bounds<Pixels>, focal_point: Point<Pixels>) {
        let old_scale = self.scale(viewport);
        if !target_scale.is_finite() || old_scale <= 0.0 {
            return;
        }
        let (min_scale, max_scale) = self.scale_limits(viewport);
        let new_scale = target_scale.clamp(min_scale, max_scale);
        let old_pan = self.clamp_pan(self.pan, viewport, old_scale);
        let source_offset = point(
            (focal_point.x - viewport.center().x - old_pan.x) / old_scale,
            (focal_point.y - viewport.center().y - old_pan.y) / old_scale,
        );
        let new_pan = point(
            focal_point.x - viewport.center().x - source_offset.x * new_scale,
            focal_point.y - viewport.center().y - source_offset.y * new_scale,
        );
        self.mode = ImageScaleMode::Manual;
        self.manual_scale = new_scale;
        self.pan = self.clamp_pan(new_pan, viewport, new_scale);
    }
}

/// Whether the body is too narrow to split, so the preview replaces the chat
/// and the chat becomes a pinned tab instead.
pub fn tabbed_preview_layout(body_width: Pixels, rem_size: Pixels) -> bool {
    body_width < crate::ui_scale::scaled_px(TABBED_LAYOUT_MAX_BODY_WIDTH, rem_size)
}

pub fn clamp_panel_width(width: Pixels, body_width: Pixels, rem_size: Pixels) -> Pixels {
    let (min_width, max_width) = panel_width_bounds(body_width, rem_size);
    width.clamp(min_width, max_width)
}

pub fn clamp_chat_width(width: Pixels, body_width: Pixels, rem_size: Pixels) -> Pixels {
    let usable = (body_width - crate::ui_scale::scaled_px(DIVIDER_WIDTH, rem_size)).max(px(0.0));
    usable - clamp_panel_width(usable - width, body_width, rem_size)
}

pub fn default_chat_width(body_width: Pixels, rem_size: Pixels) -> Pixels {
    clamp_chat_width(
        crate::ui_scale::scaled_px(DEFAULT_CHAT_WIDTH, rem_size),
        body_width,
        rem_size,
    )
}

pub fn panel_width_for_chat_width(
    chat_width: Pixels,
    body_width: Pixels,
    rem_size: Pixels,
) -> Pixels {
    let usable = (body_width - crate::ui_scale::scaled_px(DIVIDER_WIDTH, rem_size)).max(px(0.0));
    clamp_panel_width(usable - chat_width, body_width, rem_size)
}

fn panel_width_bounds(body_width: Pixels, rem_size: Pixels) -> (Pixels, Pixels) {
    let body_width = body_width.max(px(0.0));
    let divider = crate::ui_scale::scaled_px(DIVIDER_WIDTH, rem_size);
    let usable = (body_width - divider).max(px(0.0));
    let available = usable - crate::ui_scale::scaled_px(MIN_CHAT_WIDTH, rem_size);
    let constrained_floor = (usable / 2.0).min(crate::ui_scale::scaled_px(240.0, rem_size));
    let min_width =
        crate::ui_scale::scaled_px(MIN_PANEL_WIDTH, rem_size).min(available.max(constrained_floor));
    (min_width, available.max(min_width).min(usable))
}

#[cfg(test)]
mod tests {
    use local_rpc::{ids::FileTransferId, model::MediaKind};

    use super::*;

    fn item(message_id: u64) -> PreviewItem {
        PreviewItem::image(
            AttachmentDescriptor {
                id: AttachmentId {
                    timestamp_ms: message_id,
                    transfer_id: FileTransferId(message_id),
                },
                file_name: format!("image-{message_id}.png"),
                media_kind: MediaKind::Image,
                content_type: "image/png".into(),
                byte_len: 10,
                width: Some(400),
                height: Some(300),
            },
            (400, 300),
        )
    }

    fn code_item(message_id: u64, transfer_id: BulkTransferId) -> PreviewItem {
        PreviewItem::code(
            AttachmentDescriptor {
                id: AttachmentId {
                    timestamp_ms: message_id,
                    transfer_id: FileTransferId(message_id),
                },
                file_name: format!("source-{message_id}.rs"),
                media_kind: MediaKind::File,
                content_type: "text/rust".into(),
                byte_len: 10,
                width: None,
                height: None,
            },
            Some(transfer_id),
        )
    }

    fn viewport(width: f32, height: f32) -> Bounds<Pixels> {
        Bounds {
            origin: point(px(0.0), px(0.0)),
            size: size(px(width), px(height)),
        }
    }

    #[test]
    fn panel_width_clamp_preserves_chat_space() {
        assert_eq!(
            clamp_panel_width(px(1_200.0), px(1_400.0), px(16.0)),
            px(1_031.0)
        );
        assert_eq!(
            clamp_panel_width(px(500.0), px(1_400.0), px(16.0)),
            px(500.0)
        );
    }

    #[test]
    fn fixed_chat_width_gives_window_growth_to_the_preview_panel() {
        let chat_width = default_chat_width(px(1_400.0), px(16.0));
        assert_eq!(chat_width, px(800.0));
        assert_eq!(
            panel_width_for_chat_width(chat_width, px(1_400.0), px(16.0)),
            px(591.0)
        );
        assert_eq!(
            panel_width_for_chat_width(chat_width, px(1_800.0), px(16.0)),
            px(991.0)
        );
    }

    #[test]
    fn the_tabbed_layout_threshold_tracks_the_rem_size() {
        assert!(!tabbed_preview_layout(px(1_300.0), px(16.0)));
        assert!(tabbed_preview_layout(px(1_299.0), px(16.0)));
        assert!(!tabbed_preview_layout(px(2_600.0), px(32.0)));
        assert!(tabbed_preview_layout(px(2_599.0), px(32.0)));
    }

    #[test]
    fn panel_constraints_scale_with_the_window_rem_size() {
        assert_eq!(
            panel_width_for_chat_width(
                default_chat_width(px(2_800.0), px(32.0)),
                px(2_800.0),
                px(32.0),
            ),
            px(1_182.0)
        );
        assert_eq!(
            clamp_panel_width(px(2_400.0), px(2_800.0), px(32.0)),
            px(2_062.0)
        );
    }

    #[test]
    fn direct_open_promotes_existing_tab_and_bounds_history() {
        let mut history = PreviewHistory::default();
        let mut evicted = None;
        for id in 0..HISTORY_LIMIT as u64 + 2 {
            evicted = history.open(item(id)).evicted;
        }
        assert_eq!(history.items().len(), HISTORY_LIMIT);
        assert_eq!(history.active_key(), Some(item(17).key()));
        assert_eq!(evicted, Some(item(1).key()));

        assert_eq!(history.open(item(5)).evicted, None);
        assert_eq!(history.items()[0].key(), item(5).key());
        assert_eq!(history.items().len(), HISTORY_LIMIT);
    }

    #[test]
    fn closing_active_tab_selects_its_neighbor_and_panel_close_keeps_tabs() {
        let mut history = PreviewHistory::default();
        history.open(item(1));
        history.open(item(2));
        history.open(item(3));
        history.select(item(2).key());

        assert!(history.close_tab(item(2).key()));
        assert_eq!(history.active_key(), Some(item(1).key()));
        history.close_panel();
        assert!(history.active().is_none());
        assert_eq!(history.items().len(), 2);
    }

    #[test]
    fn the_chat_tab_keeps_the_bar_while_closing_the_panel_hides_it() {
        let mut history = PreviewHistory::default();
        history.open(item(1));
        history.open(item(2));

        assert!(history.select_chat());
        assert!(history.active().is_none());
        assert!(history.tab_bar_visible());
        assert!(!history.select_chat());

        assert!(history.close_panel());
        assert!(!history.tab_bar_visible());
        assert_eq!(history.items().len(), 2);
        assert!(!history.close_panel());

        assert!(history.select(item(1).key()));
        assert!(history.tab_bar_visible());

        // The last tab closing leaves nothing to pin the chat tab to.
        history.select_chat();
        history.close_tab(item(1).key());
        history.close_tab(item(2).key());
        assert!(!history.tab_bar_visible());
    }

    #[test]
    fn code_transfer_failure_updates_only_the_matching_fetch() {
        let mut history = PreviewHistory::default();
        history.open(code_item(1, BulkTransferId(11)));
        history.open(code_item(2, BulkTransferId(22)));

        assert!(!history.fail_code_transfer(BulkTransferId(33), "wrong transfer"));
        assert!(history.fail_code_transfer(BulkTransferId(11), "network lost"));

        assert!(matches!(
            &history
                .item(code_item(1, BulkTransferId(11)).key())
                .unwrap()
                .code_preview()
                .unwrap()
                .state,
            CodePreviewState::Error(reason) if reason == "network lost"
        ));
        assert!(matches!(
            history
                .item(code_item(2, BulkTransferId(22)).key())
                .unwrap()
                .code_preview()
                .unwrap()
                .state,
            CodePreviewState::Fetching {
                transfer_id: Some(BulkTransferId(22))
            }
        ));
    }

    #[test]
    fn image_fit_actual_size_zoom_and_pan_match_viewport_geometry() {
        let view_bounds = viewport(1000.0, 600.0);
        let mut view = ImageViewState::default();
        view.reset((1600, 900));
        assert_eq!(view.geometry(view_bounds).scale, 0.625);

        view.actual_size();
        assert_eq!(view.geometry(view_bounds).scale, 1.0);
        view.pan_by(point(px(999.0), px(-999.0)), view_bounds);
        assert_eq!(
            view.geometry(view_bounds).bounds.origin,
            point(px(0.0), px(-300.0))
        );

        view.fit(view_bounds);
        assert_eq!(
            view.geometry(view_bounds).bounds.origin,
            point(px(0.0), px(18.75))
        );
    }

    #[test]
    fn focal_zoom_keeps_the_point_over_the_same_source_pixel() {
        let view_bounds = viewport(1000.0, 600.0);
        let focal = point(px(700.0), px(250.0));
        let mut view = ImageViewState::default();
        view.reset((1600, 900));
        let old = view.geometry(view_bounds);
        let source = point(
            (focal.x - old.bounds.origin.x) / old.scale,
            (focal.y - old.bounds.origin.y) / old.scale,
        );

        view.zoom_by_factor(2.0, view_bounds, focal);
        let new = view.geometry(view_bounds);
        let mapped = point(
            new.bounds.origin.x + source.x * new.scale,
            new.bounds.origin.y + source.y * new.scale,
        );
        assert!((mapped.x - focal.x).as_f32().abs() < 0.01);
        assert!((mapped.y - focal.y).as_f32().abs() < 0.01);
    }
}
