use gpui::{Bounds, Pixels, ScrollDelta};

pub(crate) fn horizontal_fraction(
    bounds: Bounds<Pixels>,
    pointer_x: Pixels,
    duration: f64,
) -> Option<f64> {
    let width = bounds.size.width.as_f32();
    if width <= 0.0 || duration <= 0.0 || !duration.is_finite() {
        return None;
    }
    Some(((pointer_x - bounds.origin.x).as_f32() / width).clamp(0.0, 1.0) as f64)
}

pub(crate) fn volume_scroll_delta(delta: ScrollDelta) -> f64 {
    match delta {
        ScrollDelta::Lines(delta) => f64::from(delta.y) * 5.0,
        ScrollDelta::Pixels(delta) => f64::from(delta.y.as_f32()) * 0.12,
    }
    .clamp(-20.0, 20.0)
}

pub(crate) fn format_time(seconds: f64) -> String {
    let seconds = seconds.max(0.0).round() as u64;
    if seconds >= 3_600 {
        format!(
            "{}:{:02}:{:02}",
            seconds / 3_600,
            seconds / 60 % 60,
            seconds % 60
        )
    } else {
        format!("{}:{:02}", seconds / 60, seconds % 60)
    }
}

#[cfg(test)]
mod tests {
    use gpui::{point, px, size};

    use super::*;

    #[test]
    fn media_time_uses_hour_format_for_long_media() {
        assert_eq!(format_time(65.0), "1:05");
        assert_eq!(format_time(3_661.0), "1:01:01");
    }

    #[test]
    fn horizontal_scrub_fraction_maps_and_clamps_coordinates() {
        let bounds = Bounds::new(point(px(100.0), px(0.0)), size(px(400.0), px(20.0)));
        assert_eq!(horizontal_fraction(bounds, px(100.0), 60.0), Some(0.0));
        assert_eq!(horizontal_fraction(bounds, px(300.0), 60.0), Some(0.5));
        assert_eq!(horizontal_fraction(bounds, px(900.0), 60.0), Some(1.0));
        assert_eq!(horizontal_fraction(bounds, px(300.0), 0.0), None);
    }
}
