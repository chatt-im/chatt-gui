use std::{cell::Cell, f32::consts::TAU, rc::Rc};

use gpui::{Bounds, HSV_COLOR_WHEEL_GEOMETRY, Pixels, Point};

use crate::{config::schema::Rgba8, theme::ThemeRole};

const TRIANGLE_HALF_HEIGHT: f32 = 0.866_025_4;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Hsva {
    pub(super) hue: f32,
    pub(super) saturation: f32,
    pub(super) value: f32,
    pub(super) alpha: f32,
}

impl Hsva {
    pub(super) fn from_rgba8(color: Rgba8) -> Self {
        let [red, green, blue, alpha] = color.packed().to_be_bytes();
        let red = f32::from(red) / 255.0;
        let green = f32::from(green) / 255.0;
        let blue = f32::from(blue) / 255.0;
        let maximum = red.max(green).max(blue);
        let minimum = red.min(green).min(blue);
        let delta = maximum - minimum;
        let hue = if delta == 0.0 {
            0.0
        } else if maximum == red {
            ((green - blue) / delta).rem_euclid(6.0) / 6.0
        } else if maximum == green {
            ((blue - red) / delta + 2.0) / 6.0
        } else {
            ((red - green) / delta + 4.0) / 6.0
        };
        Self {
            hue,
            saturation: if maximum == 0.0 { 0.0 } else { delta / maximum },
            value: maximum,
            alpha: f32::from(alpha) / 255.0,
        }
    }

    pub(super) fn to_rgba8(self) -> Rgba8 {
        let hue = self.hue.rem_euclid(1.0) * 6.0;
        let chroma = self.value * self.saturation;
        let secondary = chroma * (1.0 - (hue.rem_euclid(2.0) - 1.0).abs());
        let (red, green, blue) = match hue.floor() as u8 {
            0 => (chroma, secondary, 0.0),
            1 => (secondary, chroma, 0.0),
            2 => (0.0, chroma, secondary),
            3 => (0.0, secondary, chroma),
            4 => (secondary, 0.0, chroma),
            _ => (chroma, 0.0, secondary),
        };
        let minimum = self.value - chroma;
        let channel = |value: f32| ((value + minimum).clamp(0.0, 1.0) * 255.0).round() as u8;
        Rgba8::rgba(
            channel(red),
            channel(green),
            channel(blue),
            (self.alpha.clamp(0.0, 1.0) * 255.0).round() as u8,
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DragTarget {
    Hue,
    SaturationValue,
    Alpha,
}

pub(super) struct ColorPicker {
    pub(super) role: ThemeRole,
    pub(super) original: Rgba8,
    pub(super) hsva: Hsva,
    pub(super) drag_target: Option<DragTarget>,
    pub(super) wheel_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    pub(super) alpha_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
}

impl ColorPicker {
    pub(super) fn new(role: ThemeRole, color: Rgba8) -> Self {
        Self {
            role,
            original: color,
            hsva: Hsva::from_rgba8(color),
            drag_target: None,
            wheel_bounds: Rc::new(Cell::new(None)),
            alpha_bounds: Rc::new(Cell::new(None)),
        }
    }

    pub(super) fn target_at(&self, position: Point<Pixels>) -> Option<DragTarget> {
        if self
            .alpha_bounds
            .get()
            .is_some_and(|bounds| bounds.contains(&position))
        {
            return Some(DragTarget::Alpha);
        }
        let point = normalized_wheel_point(self.wheel_bounds.get()?, position)?;
        let distance = point.0.hypot(point.1);
        if (HSV_COLOR_WHEEL_GEOMETRY.ring_inner_radius..=HSV_COLOR_WHEEL_GEOMETRY.ring_outer_radius)
            .contains(&distance)
        {
            return Some(DragTarget::Hue);
        }
        triangle_weights(point, self.hsva.hue)
            .iter()
            .all(|weight| *weight >= -0.0001)
            .then_some(DragTarget::SaturationValue)
    }

    pub(super) fn update_from_pointer(
        &mut self,
        target: DragTarget,
        position: Point<Pixels>,
    ) -> bool {
        if target == DragTarget::Hue && self.target_at(position) != Some(DragTarget::Hue) {
            return false;
        }
        let previous = self.hsva;
        match target {
            DragTarget::Hue => {
                let Some(point) = self
                    .wheel_bounds
                    .get()
                    .and_then(|bounds| normalized_wheel_point(bounds, position))
                else {
                    return false;
                };
                self.hsva.hue = (0.5 + point.1.atan2(point.0) / TAU).rem_euclid(1.0);
            }
            DragTarget::SaturationValue => {
                let Some(point) = self
                    .wheel_bounds
                    .get()
                    .and_then(|bounds| normalized_wheel_point(bounds, position))
                else {
                    return false;
                };
                let weights = closest_triangle_weights(point, self.hsva.hue);
                self.hsva.value = (1.0 - weights[0]).clamp(0.0, 1.0);
                self.hsva.saturation = if self.hsva.value <= f32::EPSILON {
                    0.0
                } else {
                    (weights[2] / self.hsva.value).clamp(0.0, 1.0)
                };
            }
            DragTarget::Alpha => {
                let Some(bounds) = self.alpha_bounds.get() else {
                    return false;
                };
                self.hsva.alpha =
                    ((position.x - bounds.origin.x) / bounds.size.width).clamp(0.0, 1.0);
            }
        }
        self.hsva != previous
    }
}

fn normalized_wheel_point(bounds: Bounds<Pixels>, position: Point<Pixels>) -> Option<(f32, f32)> {
    let radius = bounds.size.width.min(bounds.size.height) * 0.5;
    if radius <= Pixels::ZERO {
        return None;
    }
    let center = bounds.center();
    Some((
        (position.x - center.x) / radius,
        (position.y - center.y) / radius,
    ))
}

fn triangle_vertices(hue: f32) -> [(f32, f32); 3] {
    let radius = HSV_COLOR_WHEEL_GEOMETRY.triangle_radius;
    let local = [
        (-radius * 0.5, -radius * TRIANGLE_HALF_HEIGHT),
        (-radius * 0.5, radius * TRIANGLE_HALF_HEIGHT),
        (radius, 0.0),
    ];
    let angle = (hue - 0.5) * TAU;
    let (sin, cos) = angle.sin_cos();
    local.map(|(x, y)| (cos * x - sin * y, sin * x + cos * y))
}

fn triangle_weights(point: (f32, f32), hue: f32) -> [f32; 3] {
    let angle = (hue - 0.5) * TAU;
    let (sin, cos) = angle.sin_cos();
    let local_x = cos * point.0 + sin * point.1;
    let local_y = -sin * point.0 + cos * point.1;
    let radius = HSV_COLOR_WHEEL_GEOMETRY.triangle_radius;
    let hue_weight = (2.0 * local_x / radius + 1.0) / 3.0;
    let non_hue_weight = 1.0 - hue_weight;
    let white_minus_black = local_y / (radius * TRIANGLE_HALF_HEIGHT);
    let white_weight = (non_hue_weight + white_minus_black) * 0.5;
    [non_hue_weight - white_weight, white_weight, hue_weight]
}

fn closest_triangle_weights(point: (f32, f32), hue: f32) -> [f32; 3] {
    let weights = triangle_weights(point, hue);
    if weights.iter().all(|weight| *weight >= 0.0) {
        return weights;
    }

    let vertices = triangle_vertices(hue);
    let edges = [(0, 1), (1, 2), (2, 0)];
    let mut closest = vertices[0];
    let mut closest_distance = f32::INFINITY;
    for (start, end) in edges {
        let start = vertices[start];
        let end = vertices[end];
        let edge = (end.0 - start.0, end.1 - start.1);
        let length_squared = edge.0 * edge.0 + edge.1 * edge.1;
        let t = (((point.0 - start.0) * edge.0 + (point.1 - start.1) * edge.1) / length_squared)
            .clamp(0.0, 1.0);
        let candidate = (start.0 + edge.0 * t, start.1 + edge.1 * t);
        let distance = (candidate.0 - point.0).powi(2) + (candidate.1 - point.1).powi(2);
        if distance < closest_distance {
            closest = candidate;
            closest_distance = distance;
        }
    }
    triangle_weights(closest, hue).map(|weight| weight.clamp(0.0, 1.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{bounds, point, px, size};

    fn assert_channel_close(left: u8, right: u8) {
        assert!(left.abs_diff(right) <= 1, "{left} differs from {right}");
    }

    #[test]
    fn rgba_hsva_round_trips_representative_colors() {
        let cases: [u32; 7] = [
            0x0000_00ff,
            0xffff_ffff,
            0xff00_00ff,
            0x00ff_0080,
            0x0000_ff40,
            0x7a31_c9d2,
            0x7777_77ff,
        ];
        for packed in cases {
            let source = Rgba8::rgba(
                (packed >> 24) as u8,
                (packed >> 16) as u8,
                (packed >> 8) as u8,
                packed as u8,
            );
            let rendered = Hsva::from_rgba8(source).to_rgba8();
            let source = source.packed().to_be_bytes();
            let rendered = rendered.packed().to_be_bytes();
            for (left, right) in source.into_iter().zip(rendered) {
                assert_channel_close(left, right);
            }
        }
    }

    #[test]
    fn triangle_hue_vertex_rotates_to_selected_ring_angle() {
        for hue in [0.0, 0.125, 0.25, 0.5, 0.875] {
            let vertices = triangle_vertices(hue);
            let angle = vertices[2].1.atan2(vertices[2].0);
            assert!((angle - (hue - 0.5) * TAU).sin().abs() < 0.0001);
            let weights = triangle_weights(vertices[2], hue);
            assert!(weights[0].abs() < 0.0001);
            assert!(weights[1].abs() < 0.0001);
            assert!((weights[2] - 1.0).abs() < 0.0001);
        }
    }

    #[test]
    fn pointer_targets_ring_triangle_and_alpha() {
        let picker = ColorPicker::new(ThemeRole::Window, Rgba8::rgb(255, 0, 0));
        picker.wheel_bounds.set(Some(bounds(
            point(px(10.0), px(20.0)),
            size(px(200.0), px(200.0)),
        )));
        picker.alpha_bounds.set(Some(bounds(
            point(px(10.0), px(230.0)),
            size(px(200.0), px(20.0)),
        )));

        assert_eq!(
            picker.target_at(point(px(20.0), px(120.0))),
            Some(DragTarget::Hue)
        );
        let hue_tip = triangle_vertices(picker.hsva.hue)[2];
        assert_eq!(
            picker.target_at(point(
                px(110.0 + hue_tip.0 * 100.0),
                px(120.0 + hue_tip.1 * 100.0),
            )),
            Some(DragTarget::SaturationValue)
        );
        assert_eq!(
            picker.target_at(point(px(110.0), px(240.0))),
            Some(DragTarget::Alpha)
        );
    }

    #[test]
    fn hue_drag_does_not_follow_the_pointer_into_the_triangle() {
        let mut picker = ColorPicker::new(ThemeRole::Window, Rgba8::rgb(255, 0, 0));
        picker.wheel_bounds.set(Some(bounds(
            point(px(10.0), px(20.0)),
            size(px(200.0), px(200.0)),
        )));

        assert!(picker.update_from_pointer(DragTarget::Hue, point(px(110.0), px(40.0)),));
        let ring_hue = picker.hsva.hue;
        assert!(!picker.update_from_pointer(DragTarget::Hue, point(px(110.0), px(120.0)),));
        assert_eq!(picker.hsva.hue, ring_hue);
    }

    #[test]
    fn dedicated_wgsl_shader_parses_and_validates() {
        let source = include_str!("../../vendor/zed/crates/gpui_wgpu/src/hsv_color_wheel.wgsl");
        let module = naga::front::wgsl::parse_str(source).expect("color-wheel WGSL parses");
        naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::all(),
        )
        .validate(&module)
        .expect("color-wheel WGSL validates");
    }
}
