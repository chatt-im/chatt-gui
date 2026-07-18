mod app;
mod mpv_player;
mod timeline;

use std::path::PathBuf;

use gpui::{App, AppContext, Bounds, WindowBounds, WindowOptions, px, size};
use gpui_platform::application;

use crate::app::ChattView;

fn main() {
    env_logger::init();
    let media_paths: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();

    application().run(move |cx: &mut App| {
        app::bind_keys(cx);

        let bounds = Bounds::centered(None, size(px(1240.0), px(820.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            move |window, cx| {
                cx.new(|cx| ChattView::new(media_paths.clone(), window, cx))
            },
        )
        .expect("failed to open Chatt window");

        cx.activate(true);
    });
}
