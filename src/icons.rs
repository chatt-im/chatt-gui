use std::borrow::Cow;

use gpui::{AssetSource, Result, Rgba, SharedString, Svg, prelude::*, svg};

use crate::ui_scale::rems_from_px;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum IconName {
    ArrowLeftRight,
    AudioOff,
    AudioOn,
    AtSign,
    Chatt,
    Close,
    CornerUpLeft,
    Copy,
    Download,
    FileText,
    Image,
    ListChevronsDownUp,
    ListChevronsUpDown,
    Maximize,
    Menu,
    Mic,
    MicOff,
    Minimize,
    Pause,
    Pencil,
    Play,
    Plus,
    Repeat1,
    RepeatOff,
    RotateCcw,
    Search,
    Settings,
    Shield,
    ShieldAlert,
    ShieldCheck,
    Stop,
    Trash,
    VolumeHigh,
    VolumeLow,
    VolumeMuted,
    ZoomIn,
    ZoomOut,
}

impl IconName {
    fn path(self) -> &'static str {
        match self {
            Self::ArrowLeftRight => "icons/arrow-left-right.svg",
            Self::AudioOff => "icons/audio-off.svg",
            Self::AudioOn => "icons/audio-on.svg",
            Self::AtSign => "icons/at-sign.svg",
            Self::Chatt => "icons/chatt.svg",
            Self::Close => "icons/close.svg",
            Self::CornerUpLeft => "icons/corner-up-left.svg",
            Self::Copy => "icons/copy.svg",
            Self::Download => "icons/download.svg",
            Self::FileText => "icons/file-text.svg",
            Self::Image => "icons/image.svg",
            Self::ListChevronsDownUp => "icons/list-chevrons-down-up.svg",
            Self::ListChevronsUpDown => "icons/list-chevrons-up-down.svg",
            Self::Maximize => "icons/maximize.svg",
            Self::Menu => "icons/menu.svg",
            Self::Mic => "icons/mic.svg",
            Self::MicOff => "icons/mic-off.svg",
            Self::Minimize => "icons/minimize.svg",
            Self::Pause => "icons/pause.svg",
            Self::Pencil => "icons/pencil.svg",
            Self::Play => "icons/play.svg",
            Self::Plus => "icons/plus.svg",
            Self::Repeat1 => "icons/repeat-1.svg",
            Self::RepeatOff => "icons/repeat-off.svg",
            Self::RotateCcw => "icons/rotate-ccw.svg",
            Self::Search => "icons/search.svg",
            Self::Settings => "icons/settings.svg",
            Self::Shield => "icons/shield.svg",
            Self::ShieldAlert => "icons/shield-alert.svg",
            Self::ShieldCheck => "icons/shield-check.svg",
            Self::Stop => "icons/stop.svg",
            Self::Trash => "icons/trash.svg",
            Self::VolumeHigh => "icons/volume-high.svg",
            Self::VolumeLow => "icons/volume-low.svg",
            Self::VolumeMuted => "icons/volume-muted.svg",
            Self::ZoomIn => "icons/zoom-in.svg",
            Self::ZoomOut => "icons/zoom-out.svg",
        }
    }
}

pub(crate) fn icon(name: IconName, size: f32, color: Rgba) -> Svg {
    // Unlike text, GPUI SVG elements need their monochrome tint on the SVG's
    // own style at paint time. Relying on the button's inherited text color
    // leaves `Svg::paint` without a color and it skips drawing altogether.
    svg()
        .path(name.path())
        .size(rems_from_px(size))
        .flex_none()
        .text_color(color)
}

/// The client previously had no asset source. Keep this deliberately small and
/// embed just the monochrome controls used by the UI so packaged binaries do
/// not depend on a working directory at runtime.
pub(crate) struct IconAssets;

impl AssetSource for IconAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        Ok(icon_svg(path).map(|source| Cow::Owned(source.into_bytes())))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        if path == "icons" || path == "icons/" {
            Ok(ICON_PATHS.iter().copied().map(Into::into).collect())
        } else {
            Ok(Vec::new())
        }
    }
}

const ICON_PATHS: &[&str] = &[
    "icons/arrow-left-right.svg",
    "icons/audio-off.svg",
    "icons/audio-on.svg",
    "icons/at-sign.svg",
    "icons/chatt.svg",
    "icons/close.svg",
    "icons/corner-up-left.svg",
    "icons/copy.svg",
    "icons/download.svg",
    "icons/file-text.svg",
    "icons/image.svg",
    "icons/list-chevrons-down-up.svg",
    "icons/list-chevrons-up-down.svg",
    "icons/maximize.svg",
    "icons/menu.svg",
    "icons/mic.svg",
    "icons/mic-off.svg",
    "icons/minimize.svg",
    "icons/pause.svg",
    "icons/pencil.svg",
    "icons/play.svg",
    "icons/plus.svg",
    "icons/repeat-1.svg",
    "icons/repeat-off.svg",
    "icons/rotate-ccw.svg",
    "icons/search.svg",
    "icons/settings.svg",
    "icons/shield.svg",
    "icons/shield-alert.svg",
    "icons/shield-check.svg",
    "icons/stop.svg",
    "icons/trash.svg",
    "icons/volume-high.svg",
    "icons/volume-low.svg",
    "icons/volume-muted.svg",
    "icons/zoom-in.svg",
    "icons/zoom-out.svg",
];

fn icon_svg(path: &str) -> Option<String> {
    if path == "icons/chatt.svg" {
        return Some(
            r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64"><path d="M0 0v48h8v16l24-16h32V0Zm12 4 20 20-20 20Zm40 0v40L32 24Z" fill="black"/></svg>"#
                .to_owned(),
        );
    }

    let body = match path {
        "icons/arrow-left-right.svg" => {
            r#"<path d="M8 3 4 7l4 4"/><path d="M4 7h16"/><path d="m16 21 4-4-4-4"/><path d="M20 17H4"/>"#
        }
        "icons/audio-off.svg" => {
            r#"<path d="M11 5 6 9H2v6h4l5 4z"/><path d="m22 9-6 6"/><path d="m16 9 6 6"/>"#
        }
        "icons/audio-on.svg" => {
            r#"<path d="M11 5 6 9H2v6h4l5 4z"/><path d="M15.5 8.5a5 5 0 0 1 0 7"/><path d="M18.5 5.5a9 9 0 0 1 0 13"/>"#
        }
        "icons/at-sign.svg" => {
            r#"<circle cx="12" cy="12" r="4"/><path d="M16 8v5a3 3 0 0 0 6 0v-1a10 10 0 1 0-4 8"/>"#
        }
        "icons/close.svg" => r#"<path d="M18 6 6 18"/><path d="m6 6 12 12"/>"#,
        "icons/menu.svg" => r#"<path d="M4 6h16"/><path d="M4 12h16"/><path d="M4 18h16"/>"#,
        "icons/corner-up-left.svg" => {
            r#"<polyline points="9 14 4 9 9 4"/><path d="M20 20v-7a4 4 0 0 0-4-4H4"/>"#
        }
        "icons/copy.svg" => {
            r#"<rect x="9" y="9" width="13" height="13" rx="2"/><path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1"/>"#
        }
        "icons/download.svg" => {
            r#"<path d="M12 3v12"/><path d="m7 10 5 5 5-5"/><path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"/>"#
        }
        "icons/file-text.svg" => {
            r#"<path d="M6 22a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h8a2.4 2.4 0 0 1 1.704.706l3.588 3.588A2.4 2.4 0 0 1 20 8v12a2 2 0 0 1-2 2z"/><path d="M14 2v5a1 1 0 0 0 1 1h5"/><path d="M10 9H8"/><path d="M16 13H8"/><path d="M16 17H8"/>"#
        }
        "icons/image.svg" => {
            r#"<rect width="18" height="18" x="3" y="3" rx="2" ry="2"/><circle cx="9" cy="9" r="2"/><path d="m21 15-3.086-3.086a2 2 0 0 0-2.828 0L6 21"/>"#
        }
        "icons/list-chevrons-down-up.svg" => {
            r#"<path d="M3 5h8"/><path d="M3 12h8"/><path d="M3 19h8"/><path d="m15 5 3 3 3-3"/><path d="m15 19 3-3 3 3"/>"#
        }
        "icons/list-chevrons-up-down.svg" => {
            r#"<path d="M3 5h8"/><path d="M3 12h8"/><path d="M3 19h8"/><path d="m15 8 3-3 3 3"/><path d="m15 16 3 3 3-3"/>"#
        }
        "icons/maximize.svg" => {
            r#"<path d="M15 3h6v6"/><path d="m21 3-7 7"/><path d="m3 21 7-7"/><path d="M9 21H3v-6"/>"#
        }
        "icons/mic.svg" => {
            r#"<rect x="9" y="2" width="6" height="12" rx="3"/><path d="M5 10v2a7 7 0 0 0 14 0v-2"/><path d="M12 19v3"/>"#
        }
        "icons/mic-off.svg" => {
            r#"<path d="m2 2 20 20"/><path d="M9 9v3a3 3 0 0 0 5.12 2.12"/><path d="M15 9.34V5a3 3 0 0 0-5.68-1.33"/><path d="M5 10v2a7 7 0 0 0 12 4.9"/><path d="M19 10v2c0 .43-.04.84-.11 1.23"/><path d="M12 19v3"/>"#
        }
        "icons/minimize.svg" => {
            r#"<path d="m14 10 7-7"/><path d="M20 10h-6V4"/><path d="m3 21 7-7"/><path d="M4 14h6v6"/>"#
        }
        "icons/pause.svg" => {
            r#"<rect x="14" y="3" width="5" height="18" rx="1"/><rect x="5" y="3" width="5" height="18" rx="1"/>"#
        }
        "icons/pencil.svg" => {
            r#"<path d="M21.17 6.81a1 1 0 0 0-3.98-3.98L3.84 16.17a2 2 0 0 0-.5.83l-1.32 4.36a.5.5 0 0 0 .62.62L7 20.66a2 2 0 0 0 .83-.5z"/><path d="m15 5 4 4"/>"#
        }
        "icons/play.svg" => {
            r#"<path d="M5 5a2 2 0 0 1 3.01-1.73l12 7a2 2 0 0 1 0 3.46l-12 7A2 2 0 0 1 5 19z"/>"#
        }
        "icons/plus.svg" => r#"<path d="M5 12h14"/><path d="M12 5v14"/>"#,
        "icons/repeat-1.svg" => {
            r#"<path d="m17 2 4 4-4 4"/><path d="M3 11v-1a4 4 0 0 1 4-4h14"/><path d="m7 22-4-4 4-4"/><path d="M21 13v1a4 4 0 0 1-4 4H3"/><path d="M11 10h1v4"/>"#
        }
        "icons/repeat-off.svg" => {
            r#"<path d="M11.656 6H21l-4-4"/><path d="M17.898 17.898A4 4 0 0 1 17 18H3l4-4"/><path d="m2 2 20 20"/><path d="M21 13v1a4 4 0 0 1-.171 1.159"/><path d="m21 6-4 4"/><path d="M3 11v-1a4 4 0 0 1 3.102-3.898"/><path d="m7 22-4-4"/>"#
        }
        "icons/rotate-ccw.svg" => r#"<path d="M3 12a9 9 0 1 0 3-6.7L3 8"/><path d="M3 3v5h5"/>"#,
        "icons/search.svg" => r#"<circle cx="11" cy="11" r="8"/><path d="m21 21-4.35-4.35"/>"#,
        "icons/settings.svg" => {
            r#"<path d="M9.671 4.136a2.34 2.34 0 0 1 4.659 0 2.34 2.34 0 0 0 3.319 1.915 2.34 2.34 0 0 1 2.33 4.033 2.34 2.34 0 0 0 0 3.831 2.34 2.34 0 0 1-2.33 4.033 2.34 2.34 0 0 0-3.319 1.915 2.34 2.34 0 0 1-4.659 0 2.34 2.34 0 0 0-3.32-1.915 2.34 2.34 0 0 1-2.33-4.033 2.34 2.34 0 0 0 0-3.831A2.34 2.34 0 0 1 6.35 6.051a2.34 2.34 0 0 0 3.319-1.915"/><circle cx="12" cy="12" r="3"/>"#
        }
        "icons/shield.svg" => {
            r#"<path d="M20 13c0 5-3.5 7.5-7.66 8.95a1 1 0 0 1-.67-.01C7.5 20.5 4 18 4 13V6a1 1 0 0 1 1-1c2 0 4.5-1.2 6.24-2.72a1.17 1.17 0 0 1 1.52 0C14.51 3.81 17 5 19 5a1 1 0 0 1 1 1z"/>"#
        }
        "icons/shield-alert.svg" => {
            r#"<path d="M20 13c0 5-3.5 7.5-7.66 8.95a1 1 0 0 1-.67-.01C7.5 20.5 4 18 4 13V6a1 1 0 0 1 1-1c2 0 4.5-1.2 6.24-2.72a1.17 1.17 0 0 1 1.52 0C14.51 3.81 17 5 19 5a1 1 0 0 1 1 1z"/><path d="M12 8v4"/><path d="M12 16h.01"/>"#
        }
        "icons/shield-check.svg" => {
            r#"<path d="M20 13c0 5-3.5 7.5-7.66 8.95a1 1 0 0 1-.67-.01C7.5 20.5 4 18 4 13V6a1 1 0 0 1 1-1c2 0 4.5-1.2 6.24-2.72a1.17 1.17 0 0 1 1.52 0C14.51 3.81 17 5 19 5a1 1 0 0 1 1 1z"/><path d="m9 12 2 2 4-4"/>"#
        }
        "icons/stop.svg" => r#"<rect x="4" y="4" width="16" height="16" rx="2"/>"#,
        "icons/trash.svg" => {
            r#"<path d="M10 11v6"/><path d="M14 11v6"/><path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6"/><path d="M3 6h18"/><path d="M8 6V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2"/>"#
        }
        "icons/volume-high.svg" => {
            r#"<path d="M11 4.7a.7.7 0 0 0-1.2-.5L6.4 7.6a1.4 1.4 0 0 1-1 .4H3a1 1 0 0 0-1 1v6a1 1 0 0 0 1 1h2.4a1.4 1.4 0 0 1 1 .4l3.4 3.4a.7.7 0 0 0 1.2-.5z"/><path d="M16 9a5 5 0 0 1 0 6"/><path d="M19.4 18.4a9 9 0 0 0 0-12.8"/>"#
        }
        "icons/volume-low.svg" => {
            r#"<path d="M11 4.7a.7.7 0 0 0-1.2-.5L6.4 7.6a1.4 1.4 0 0 1-1 .4H3a1 1 0 0 0-1 1v6a1 1 0 0 0 1 1h2.4a1.4 1.4 0 0 1 1 .4l3.4 3.4a.7.7 0 0 0 1.2-.5z"/><path d="M16 9a5 5 0 0 1 0 6"/>"#
        }
        "icons/volume-muted.svg" => {
            r#"<path d="M11 4.7a.7.7 0 0 0-1.2-.5L6.4 7.6a1.4 1.4 0 0 1-1 .4H3a1 1 0 0 0-1 1v6a1 1 0 0 0 1 1h2.4a1.4 1.4 0 0 1 1 .4l3.4 3.4a.7.7 0 0 0 1.2-.5z"/><path d="m22 9-6 6"/><path d="m16 9 6 6"/>"#
        }
        "icons/zoom-in.svg" => {
            r#"<circle cx="11" cy="11" r="8"/><path d="m21 21-4.35-4.35"/><path d="M11 8v6"/><path d="M8 11h6"/>"#
        }
        "icons/zoom-out.svg" => {
            r#"<circle cx="11" cy="11" r="8"/><path d="m21 21-4.35-4.35"/><path d="M8 11h6"/>"#
        }
        _ => return None,
    };
    Some(format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="black" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">{body}</svg>"#
    ))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    #[test]
    fn every_icon_path_rasterizes_visible_pixels() {
        let renderer = gpui::SvgRenderer::new(Arc::new(IconAssets));
        for path in ICON_PATHS {
            let source = icon_svg(path).unwrap_or_else(|| panic!("missing icon asset for {path}"));
            let image = renderer
                .render_single_frame(source.as_bytes(), 1.0)
                .unwrap_or_else(|error| panic!("could not rasterize {path}: {error}"));
            assert!(
                image
                    .as_bytes(0)
                    .expect("rendered icon has a frame")
                    .chunks_exact(4)
                    .any(|pixel| pixel[3] != 0),
                "icon asset {path} rasterized completely transparent"
            );
        }
    }
}
