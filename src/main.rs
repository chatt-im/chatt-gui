mod app;
mod composer;
mod daemon;
mod image_cache;
mod media_cache;
mod model;
mod mpv_player;
mod timeline;

use gpui::{App, AppContext, Bounds, WindowBounds, WindowOptions, px, size};
use gpui_platform::application;

use crate::app::ChattView;

fn main() {
    env_logger::init();
    application().run(move |cx: &mut App| {
        settings::init(cx);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        app::bind_keys(cx);

        let bounds = Bounds::centered(None, size(px(1240.0), px(820.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            move |window, cx| cx.new(|cx| ChattView::new(window, cx)),
        )
        .expect("failed to open Chatt window");

        cx.activate(true);
    });
}
