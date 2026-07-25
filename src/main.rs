mod app;
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
mod video_controls;
mod video_manager;
mod video_player;
mod video_thumbnail;

use gpui::{App, AppContext, Bounds, WindowBounds, WindowOptions, px, size};
use gpui_platform::application;

use crate::app::ChattView;

fn main() {
    logger::init();
    let mut loaded = config::io::load();
    let binding_diagnostics = key_bindings::validate(&loaded.config);
    if config::validation::has_errors(&binding_diagnostics) {
        loaded.diagnostics.extend(binding_diagnostics);
        loaded.config = config::schema::GuiConfig::default();
        loaded.status = config::io::SourceStatus::Invalid;
    }
    log::info!(
        "logging initialized (set RUST_LOG for Rust diagnostics and CHATT_MPV_LOG for native mpv diagnostics)"
    );
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
                        log::warn!("{}{}: {}", diagnostic.path, location, diagnostic.message)
                    }
                    config::validation::DiagnosticSeverity::Error => {
                        log::error!("{}{}: {}", diagnostic.path, location, diagnostic.message)
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
            key_bindings::install(&loaded.config, cx)
                .expect("built-in GUI key bindings must compile");
            settings::install_loaded(loaded.clone(), cx);
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
}
