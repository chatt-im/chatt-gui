use std::borrow::Cow;

use gpui::App;
pub(crate) const UI_FONT_FAMILY: &str = "IBM Plex Sans";
pub(crate) const CODE_FONT_FAMILY: &str = "Lilex";

const IBM_PLEX_SANS_REGULAR: &[u8] =
    include_bytes!("../vendor/zed/assets/fonts/ibm-plex-sans/IBMPlexSans-Regular.ttf");
const IBM_PLEX_SANS_ITALIC: &[u8] =
    include_bytes!("../vendor/zed/assets/fonts/ibm-plex-sans/IBMPlexSans-Italic.ttf");
const IBM_PLEX_SANS_SEMIBOLD: &[u8] =
    include_bytes!("../vendor/zed/assets/fonts/ibm-plex-sans/IBMPlexSans-SemiBold.ttf");
const IBM_PLEX_SANS_SEMIBOLD_ITALIC: &[u8] =
    include_bytes!("../vendor/zed/assets/fonts/ibm-plex-sans/IBMPlexSans-SemiBoldItalic.ttf");
const LILEX_REGULAR: &[u8] = include_bytes!("../vendor/zed/assets/fonts/lilex/Lilex-Regular.ttf");
const LILEX_ITALIC: &[u8] = include_bytes!("../vendor/zed/assets/fonts/lilex/Lilex-Italic.ttf");
const LILEX_BOLD: &[u8] = include_bytes!("../vendor/zed/assets/fonts/lilex/Lilex-Bold.ttf");
const LILEX_BOLD_ITALIC: &[u8] =
    include_bytes!("../vendor/zed/assets/fonts/lilex/Lilex-BoldItalic.ttf");

fn embedded_fonts() -> Vec<Cow<'static, [u8]>> {
    vec![
        Cow::Borrowed(IBM_PLEX_SANS_REGULAR),
        Cow::Borrowed(IBM_PLEX_SANS_ITALIC),
        Cow::Borrowed(IBM_PLEX_SANS_SEMIBOLD),
        Cow::Borrowed(IBM_PLEX_SANS_SEMIBOLD_ITALIC),
        Cow::Borrowed(LILEX_REGULAR),
        Cow::Borrowed(LILEX_ITALIC),
        Cow::Borrowed(LILEX_BOLD),
        Cow::Borrowed(LILEX_BOLD_ITALIC),
    ]
}

pub fn init(cx: &mut App) {
    cx.text_system()
        .add_fonts(embedded_fonts())
        .expect("failed to load Chatt's bundled UI fonts");
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use gpui::{
        FontRun, FontStyle, FontWeight, LineFragment, PlatformTextSystem, TextSystem, font, px,
    };
    use gpui_wgpu::CosmicTextSystem;

    use super::embedded_fonts;

    #[test]
    fn selects_embedded_weight_and_style_faces_without_font_kit() {
        let text = CosmicTextSystem::new_without_system_fonts("IBM Plex Sans");
        text.add_fonts(embedded_fonts()).unwrap();

        let regular = text.font_id(&font("IBM Plex Sans")).unwrap();
        let italic = text.font_id(&font("IBM Plex Sans").italic()).unwrap();
        let semibold = text
            .font_id(&gpui::Font {
                weight: FontWeight::SEMIBOLD,
                ..font("IBM Plex Sans")
            })
            .unwrap();
        let bold = text.font_id(&font("IBM Plex Sans").bold()).unwrap();
        let semibold_italic = text
            .font_id(&gpui::Font {
                weight: FontWeight::SEMIBOLD,
                style: FontStyle::Italic,
                ..font("IBM Plex Sans")
            })
            .unwrap();
        let bold_italic = text
            .font_id(&font("IBM Plex Sans").bold().italic())
            .unwrap();

        assert_ne!(regular, italic);
        assert_ne!(regular, semibold);
        assert_ne!(italic, semibold_italic);
        assert_eq!(bold, semibold);
        assert_eq!(bold_italic, semibold_italic);
    }

    #[test]
    fn shapes_system_fallback_text_and_wraps_without_native_font_libraries() {
        let platform = Arc::new(CosmicTextSystem::new("IBM Plex Sans"));
        platform.add_fonts(embedded_fonts()).unwrap();
        let primary = platform.font_id(&font("IBM Plex Sans")).unwrap();
        let sample = "Cafe\u{301} · العربية · नमस्ते · 字 · ☺ · שלום";
        let layout = platform.layout_line(
            sample,
            px(16.),
            &[FontRun {
                len: sample.len(),
                font_id: primary,
            }],
        );

        assert!(layout.width > px(0.));
        assert!(!layout.runs.is_empty());
        assert!(
            layout
                .runs
                .iter()
                .flat_map(|run| &run.glyphs)
                .all(|glyph| sample.is_char_boundary(glyph.index))
        );
        assert!(layout.runs.iter().any(|run| run.font_id != primary));
        let emoji_index = sample.find('☺').unwrap();
        assert!(
            layout
                .runs
                .iter()
                .filter(|run| run.font_id != primary)
                .flat_map(|run| &run.glyphs)
                .any(|glyph| glyph.index == emoji_index && glyph.id.0 != 0),
            "emoji did not select a non-primary fallback font"
        );

        let text_system = Arc::new(TextSystem::new(platform));
        let mut wrapper = text_system.line_wrapper(font("IBM Plex Sans"), px(16.));
        let fragments = [LineFragment::text(sample)];
        assert!(wrapper.wrap_line(&fragments, px(120.)).count() > 1);

        let isolated = CosmicTextSystem::new_without_system_fonts("IBM Plex Sans");
        isolated.add_fonts(embedded_fonts()).unwrap();
        let isolated_primary = isolated.font_id(&font("IBM Plex Sans")).unwrap();
        let missing_sample = "before \u{10ffff} after";
        let missing_layout = isolated.layout_line(
            missing_sample,
            px(16.),
            &[FontRun {
                len: missing_sample.len(),
                font_id: isolated_primary,
            }],
        );
        let missing_index = missing_sample.find('\u{10ffff}').unwrap();
        assert!(missing_layout.width > px(0.));
        assert!(
            missing_layout
                .runs
                .iter()
                .flat_map(|run| &run.glyphs)
                .any(|glyph| glyph.index == missing_index && glyph.id.0 == 0)
        );
    }
}
