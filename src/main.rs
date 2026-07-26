mod app;
mod appearance;
mod attachment_source;
mod audio_manager;
mod audio_player;
mod code_viewer;
mod composer;
mod config;
mod daemon;
mod emoji;
mod fonts;
mod formatted_message;
mod frame_stats;
mod icons;
mod image_cache;
mod key_bindings;
mod libplacebo_numeric;
mod live_stream;
mod logger;
mod media_cache;
mod media_controls;
mod model;
mod mpv_player;
mod naga_bridge;
mod preview;
mod scroll_capture;
mod scrollbar;
mod settings;
mod theme;
mod timeline;
mod ui_controls;
mod ui_scale;
mod video_controls;
mod video_manager;
mod video_player;
mod video_thumbnail;

use gpui::{App, AppContext, Bounds, WindowBounds, WindowOptions, px, size};
use gpui_platform::application;

use crate::app::ChattView;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() {
    let logger_guard = logger::init();
    ensure_graphical_backend();
    let mut loaded = config::io::load();
    let binding_diagnostics = key_bindings::validate(&loaded.config);
    if config::validation::has_errors(&binding_diagnostics) {
        loaded.diagnostics.extend(binding_diagnostics);
        loaded.config = config::schema::GuiConfig::default();
        loaded.status = config::io::SourceStatus::Invalid;
    }
    kvlog::info!("logging initialized");
    application()
        .with_assets(icons::IconAssets)
        .run(move |cx: &mut App| {
            fonts::init(cx);
            let available_families = cx.text_system().all_font_names();
            loaded
                .diagnostics
                .extend(theme::font_warnings(&loaded.config, &available_families));
            let source = loaded
                .source
                .as_deref()
                .and_then(|source| std::str::from_utf8(source).ok());
            for diagnostic in &loaded.diagnostics {
                let location = source
                    .and_then(|source| diagnostic.source_excerpt(source))
                    .map(|excerpt| format!(":{}:{}", excerpt.line, excerpt.column))
                    .unwrap_or_default();
                match diagnostic.severity {
                    config::validation::DiagnosticSeverity::Warning => {
                        kvlog::warn!(
                            "configuration warning",
                            path = %diagnostic.path,
                            location = %location,
                            detail = %diagnostic.message
                        )
                    }
                    config::validation::DiagnosticSeverity::Error => {
                        kvlog::error!(
                            "configuration error",
                            path = %diagnostic.path,
                            location = %location,
                            detail = %diagnostic.message
                        )
                    }
                }
            }
            theme::apply_appearance(
                &loaded.config,
                loaded.status,
                &loaded.diagnostics,
                &available_families,
                cx,
            );
            ui_scale::install(cx);
            key_bindings::install(&loaded.config, cx)
                .expect("built-in GUI key bindings must compile");
            settings::install_loaded(loaded.clone(), cx);
            appearance::install(cx);
            frame_stats::start(cx);

            let bounds = Bounds::centered(None, size(px(1240.0), px(820.0)), cx);
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                move |window, cx| {
                    frame_stats::start_window(window);
                    cx.new(|cx| ChattView::new(window, cx))
                },
            )
            .expect("failed to open Chatt window");

            cx.activate(true);
        });
    logger_guard.flush();
}

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
fn ensure_graphical_backend() {
    let wayland_display =
        std::env::var_os("WAYLAND_DISPLAY").is_some_and(|display| !display.is_empty());
    let x11_display = std::env::var_os("DISPLAY").is_some_and(|display| !display.is_empty());
    let headless_override = std::env::var_os("ZED_HEADLESS").is_some();
    let selected = gpui::guess_compositor();

    kvlog::info!(
        "graphical backend selected",
        selected = %selected,
        wayland_compiled = cfg!(feature = "wayland"),
        x11_compiled = cfg!(feature = "x11"),
        wayland_display,
        x11_display,
        headless_override
    );

    if selected == "Headless" {
        let error = headless_backend_error(
            cfg!(feature = "wayland"),
            cfg!(feature = "x11"),
            wayland_display,
            x11_display,
            headless_override,
        );
        kvlog::error!("graphical backend unavailable", err = %error);
        std::process::exit(1);
    }
}

#[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
fn ensure_graphical_backend() {}

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
fn headless_backend_error(
    wayland_compiled: bool,
    x11_compiled: bool,
    wayland_display: bool,
    x11_display: bool,
    headless_override: bool,
) -> String {
    if headless_override {
        return "refusing to start chatt-gui headlessly because ZED_HEADLESS is set; unset it to open a window"
            .to_owned();
    }

    let mut unavailable_backends = Vec::new();
    if wayland_display && !wayland_compiled {
        unavailable_backends.push("WAYLAND_DISPLAY is set but Wayland support was not compiled in");
    }
    if x11_display && !x11_compiled {
        unavailable_backends.push(
            "DISPLAY is set but X11 support was not compiled in; rebuild with `--features x11`",
        );
    }

    if !unavailable_backends.is_empty() {
        return format!(
            "refusing to start chatt-gui headlessly: {}",
            unavailable_backends.join("; ")
        );
    }

    "refusing to start chatt-gui headlessly: neither WAYLAND_DISPLAY nor DISPLAY identifies a supported graphical session"
        .to_owned()
}

#[cfg(all(test, any(target_os = "linux", target_os = "freebsd")))]
mod tests {
    use super::headless_backend_error;

    #[test]
    fn reports_x11_display_without_compiled_x11_support() {
        assert_eq!(
            headless_backend_error(true, false, false, true, false),
            "refusing to start chatt-gui headlessly: DISPLAY is set but X11 support was not compiled in; rebuild with `--features x11`"
        );
    }

    #[test]
    fn reports_explicit_headless_override() {
        assert_eq!(
            headless_backend_error(true, true, true, true, true),
            "refusing to start chatt-gui headlessly because ZED_HEADLESS is set; unset it to open a window"
        );
    }

    #[test]
    fn reports_missing_display_environment() {
        assert_eq!(
            headless_backend_error(true, true, false, false, false),
            "refusing to start chatt-gui headlessly: neither WAYLAND_DISPLAY nor DISPLAY identifies a supported graphical session"
        );
    }
}
