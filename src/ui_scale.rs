use gpui::{App, Global, Pixels, Rems, actions, px, rems};

use crate::theme::AppliedSettings;

pub(crate) const BASE_REM_SIZE: f32 = 16.0;
pub(crate) const MIN_UI_SIZE: f32 = 8.0;
pub(crate) const MAX_UI_SIZE: f32 = 48.0;

actions!(chatt_gui, [IncreaseUiScale, DecreaseUiScale, ResetUiScale]);

#[derive(Clone, Copy, Debug)]
struct UiScaleState {
    configured_interface_size: f32,
    adjusted_interface_size: Option<f32>,
    revision: u64,
}

impl Global for UiScaleState {}

pub(crate) fn install(cx: &mut App) {
    let configured_interface_size = configured_interface_size(cx);
    cx.set_global(UiScaleState {
        configured_interface_size,
        adjusted_interface_size: None,
        revision: 1,
    });
    cx.on_action(|_: &IncreaseUiScale, cx| adjust(cx, 1.0));
    cx.on_action(|_: &DecreaseUiScale, cx| adjust(cx, -1.0));
    cx.on_action(|_: &ResetUiScale, cx| reset(cx));
}

pub(crate) fn configured_interface_size_changed(size: f32, cx: &mut App) {
    let Some(state) = cx.try_global::<UiScaleState>().copied() else {
        return;
    };
    if state.configured_interface_size == size {
        return;
    }
    cx.set_global(UiScaleState {
        configured_interface_size: size,
        adjusted_interface_size: None,
        revision: state.revision.saturating_add(1),
    });
}

pub(crate) fn rem_size(cx: &App) -> Pixels {
    px(effective_interface_size(cx))
}

pub(crate) fn revision(cx: &App) -> u64 {
    cx.try_global::<UiScaleState>()
        .map_or(0, |state| state.revision)
}

pub(crate) fn rems_from_px(value: impl Into<f32>) -> Rems {
    rems(value.into() / BASE_REM_SIZE)
}

pub(crate) fn font_rems(font_size: f32, interface_size: f32) -> Rems {
    rems(font_size / interface_size.max(f32::EPSILON))
}

pub(crate) fn scaled_px(value: impl Into<f32>, rem_size: Pixels) -> Pixels {
    rem_size * (value.into() / BASE_REM_SIZE)
}

fn configured_interface_size(cx: &App) -> f32 {
    cx.try_global::<AppliedSettings>()
        .map_or(BASE_REM_SIZE, |settings| settings.0.fonts.interface_size)
}

fn effective_interface_size(cx: &App) -> f32 {
    let configured = configured_interface_size(cx);
    cx.try_global::<UiScaleState>()
        .filter(|state| state.configured_interface_size == configured)
        .and_then(|state| state.adjusted_interface_size)
        .unwrap_or(configured)
}

fn adjust(cx: &mut App, delta: f32) {
    let configured = configured_interface_size(cx);
    let current = effective_interface_size(cx);
    let next = (current + delta).clamp(MIN_UI_SIZE, MAX_UI_SIZE);
    if next == current {
        return;
    }
    let revision = cx
        .try_global::<UiScaleState>()
        .map_or(1, |state| state.revision.saturating_add(1));
    cx.set_global(UiScaleState {
        configured_interface_size: configured,
        adjusted_interface_size: (next != configured).then_some(next),
        revision,
    });
    cx.refresh_windows();
}

fn reset(cx: &mut App) {
    let configured = configured_interface_size(cx);
    let Some(state) = cx.try_global::<UiScaleState>().copied() else {
        return;
    };
    if state.adjusted_interface_size.is_none() && state.configured_interface_size == configured {
        return;
    }
    cx.set_global(UiScaleState {
        configured_interface_size: configured,
        adjusted_interface_size: None,
        revision: state.revision.saturating_add(1),
    });
    cx.refresh_windows();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn design_pixels_convert_to_rems_at_the_sixteen_pixel_baseline() {
        assert_eq!(rems_from_px(16.0), rems(1.0));
        assert_eq!(rems_from_px(40.0), rems(2.5));
        assert_eq!(scaled_px(40.0, px(32.0)), px(80.0));
    }

    #[test]
    fn configured_font_sizes_convert_to_root_relative_rems() {
        assert_eq!(font_rems(16.0, 16.0), rems(1.0));
        assert_eq!(font_rems(14.0, 28.0), rems(0.5));
    }

    #[gpui::test]
    fn session_scale_clamps_and_resets_to_the_configured_size(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            install(cx);
            assert_eq!(rem_size(cx), px(BASE_REM_SIZE));

            adjust(cx, 1.0);
            assert_eq!(rem_size(cx), px(BASE_REM_SIZE + 1.0));
            let adjusted_revision = revision(cx);

            adjust(cx, 100.0);
            assert_eq!(rem_size(cx), px(MAX_UI_SIZE));
            assert!(revision(cx) > adjusted_revision);

            adjust(cx, -100.0);
            assert_eq!(rem_size(cx), px(MIN_UI_SIZE));

            reset(cx);
            assert_eq!(rem_size(cx), px(BASE_REM_SIZE));
        });
    }
}
