use gpui::{Bounds, Pixels, Point, point, px, size};
use local_rpc::model::{AttachmentDescriptor, AttachmentId};

pub const HISTORY_LIMIT: usize = 16;
pub const DEFAULT_PANEL_WIDTH: f32 = 560.0;
pub const MIN_PANEL_WIDTH: f32 = 320.0;
pub const MIN_CHAT_WIDTH: f32 = 360.0;
pub const DIVIDER_WIDTH: f32 = 9.0;

const MANUAL_ZOOM_MIN: f32 = 0.1;
const MANUAL_ZOOM_MAX: f32 = 8.0;
const DEFAULT_AUTO_FIT_MAX: f32 = 3.0;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreviewItem {
    pub descriptor: AttachmentDescriptor,
    pub natural_size: (u32, u32),
}

impl PreviewItem {
    pub fn new(descriptor: AttachmentDescriptor, natural_size: (u32, u32)) -> Self {
        Self {
            descriptor,
            natural_size: (natural_size.0.max(1), natural_size.1.max(1)),
        }
    }

    pub fn key(&self) -> AttachmentId {
        self.descriptor.id
    }
}

#[derive(Default)]
pub struct PreviewHistory {
    items: Vec<PreviewItem>,
    active: Option<AttachmentId>,
}

impl PreviewHistory {
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

    pub fn open(&mut self, item: PreviewItem) -> bool {
        let key = item.key();
        let active_changed = self.active_key() != Some(key);
        let promoted = self
            .items
            .iter()
            .find(|candidate| candidate.key() == key)
            .cloned()
            .unwrap_or(item);
        self.items.retain(|candidate| candidate.key() != key);
        self.items.insert(0, promoted);
        self.items.truncate(HISTORY_LIMIT);
        self.active = Some(key);
        active_changed
    }

    pub fn select(&mut self, key: AttachmentId) -> bool {
        if !self.items.iter().any(|item| item.key() == key) {
            return false;
        }
        let changed = self.active_key() != Some(key);
        self.active = Some(key);
        changed
    }

    pub fn close_panel(&mut self) {
        self.active = None;
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

pub fn clamp_panel_width(width: Pixels, body_width: Pixels) -> Pixels {
    let body_width = body_width.max(px(0.0));
    let available = body_width - px(MIN_CHAT_WIDTH) - px(DIVIDER_WIDTH);
    let min_width = px(MIN_PANEL_WIDTH).min(available.max(px(240.0)));
    width.clamp(min_width, available.max(min_width))
}

#[cfg(test)]
mod tests {
    use local_rpc::{
        ids::{MessageId, RoomId},
        model::MediaKind,
    };

    use super::*;

    fn item(message_id: u64) -> PreviewItem {
        PreviewItem::new(
            AttachmentDescriptor {
                id: AttachmentId {
                    room_id: RoomId(1),
                    message_id: MessageId(message_id),
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

    fn viewport(width: f32, height: f32) -> Bounds<Pixels> {
        Bounds {
            origin: point(px(0.0), px(0.0)),
            size: size(px(width), px(height)),
        }
    }

    #[test]
    fn direct_open_promotes_existing_tab_and_bounds_history() {
        let mut history = PreviewHistory::default();
        for id in 0..HISTORY_LIMIT as u64 + 2 {
            history.open(item(id));
        }
        assert_eq!(history.items().len(), HISTORY_LIMIT);
        assert_eq!(history.active_key(), Some(item(17).key()));

        history.open(item(5));
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
