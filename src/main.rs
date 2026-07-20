mod app;
mod composer;
mod daemon;
mod frame_stats;
mod fonts;
mod image_cache;
mod live_stream;
mod media_cache;
mod model;
mod mpv_player;
mod scroll_capture;
mod timeline;
mod video_manager;
mod video_thumbnail;

use gpui::{App, AppContext, Bounds, WindowBounds, WindowOptions, px, size};
use gpui_platform::application;

use crate::app::ChattView;

fn main() {
    env_logger::Builder::from_env(
        env_logger::Env::default()
            .default_filter_or("warn,chatt_gui=info,gpui_wgpu::video=info"),
    )
    .init();
    log::info!(
        "logging initialized (set RUST_LOG for Rust diagnostics and CHATT_MPV_LOG for native mpv diagnostics)"
    );
    application().run(move |cx: &mut App| {
        fonts::load(cx);
        settings::init(cx);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        app::bind_keys(cx);
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
