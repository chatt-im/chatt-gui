use std::borrow::Cow;

use gpui::App;

const IBM_PLEX_SANS_REGULAR: &[u8] =
    include_bytes!("../../ref/zed/assets/fonts/ibm-plex-sans/IBMPlexSans-Regular.ttf");
const IBM_PLEX_SANS_ITALIC: &[u8] =
    include_bytes!("../../ref/zed/assets/fonts/ibm-plex-sans/IBMPlexSans-Italic.ttf");
const IBM_PLEX_SANS_SEMIBOLD: &[u8] =
    include_bytes!("../../ref/zed/assets/fonts/ibm-plex-sans/IBMPlexSans-SemiBold.ttf");
const IBM_PLEX_SANS_SEMIBOLD_ITALIC: &[u8] =
    include_bytes!("../../ref/zed/assets/fonts/ibm-plex-sans/IBMPlexSans-SemiBoldItalic.ttf");
const LILEX_REGULAR: &[u8] =
    include_bytes!("../../ref/zed/assets/fonts/lilex/Lilex-Regular.ttf");
const LILEX_ITALIC: &[u8] = include_bytes!("../../ref/zed/assets/fonts/lilex/Lilex-Italic.ttf");
const LILEX_BOLD: &[u8] = include_bytes!("../../ref/zed/assets/fonts/lilex/Lilex-Bold.ttf");
const LILEX_BOLD_ITALIC: &[u8] =
    include_bytes!("../../ref/zed/assets/fonts/lilex/Lilex-BoldItalic.ttf");

pub fn load(cx: &App) {
    cx.text_system()
        .add_fonts(vec![
            Cow::Borrowed(IBM_PLEX_SANS_REGULAR),
            Cow::Borrowed(IBM_PLEX_SANS_ITALIC),
            Cow::Borrowed(IBM_PLEX_SANS_SEMIBOLD),
            Cow::Borrowed(IBM_PLEX_SANS_SEMIBOLD_ITALIC),
            Cow::Borrowed(LILEX_REGULAR),
            Cow::Borrowed(LILEX_ITALIC),
            Cow::Borrowed(LILEX_BOLD),
            Cow::Borrowed(LILEX_BOLD_ITALIC),
        ])
        .expect("failed to load Chatt's bundled UI fonts");
}
