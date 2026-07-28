use crate::{
    config::schema::BindCommand,
    key_bindings::{BINDINGS, BindingScope},
    theme::{FONT_ROLES, FontRole, THEME_ROLES, ThemeGroup, ThemeRole},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ScalarSetting {
    FontRendering,
    BindingMode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ToggleSetting {
    StatusBarVisible,
    RoomMenuVisible,
    NativeFullscreen,
    VideoLoopByDefault,
    LiveLowDelayDecode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RowRef {
    Theme(ThemeRole),
    FontFamily(FontRole),
    FontSize(FontRole),
    Choice(ScalarSetting),
    Toggle(ToggleSetting),
    Binding(BindingScope, BindCommand),
    Diagnostic(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CatalogItem {
    ThemeGroup(ThemeGroup),
    Font(FontRole),
    Choice(ScalarSetting),
    Toggle(ToggleSetting),
    Keymap,
    Diagnostics,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct SettingsSection {
    pub(crate) id: &'static str,
    pub(crate) title: &'static str,
    pub(crate) help: &'static str,
    pub(crate) items: &'static [CatalogItem],
}

const APPEARANCE: &[CatalogItem] = &[
    CatalogItem::ThemeGroup(ThemeGroup::Surfaces),
    CatalogItem::ThemeGroup(ThemeGroup::Text),
    CatalogItem::ThemeGroup(ThemeGroup::Borders),
    CatalogItem::ThemeGroup(ThemeGroup::States),
    CatalogItem::ThemeGroup(ThemeGroup::Controls),
    CatalogItem::ThemeGroup(ThemeGroup::Scrollbar),
    CatalogItem::ThemeGroup(ThemeGroup::Participants),
];
const TYPOGRAPHY: &[CatalogItem] = &[
    CatalogItem::Font(FontRole::Interface),
    CatalogItem::Font(FontRole::Message),
    CatalogItem::Font(FontRole::Code),
    CatalogItem::Choice(ScalarSetting::FontRendering),
];
const SYNTAX: &[CatalogItem] = &[CatalogItem::ThemeGroup(ThemeGroup::Syntax)];
const MEDIA: &[CatalogItem] = &[
    CatalogItem::ThemeGroup(ThemeGroup::Media),
    CatalogItem::Toggle(ToggleSetting::NativeFullscreen),
    CatalogItem::Toggle(ToggleSetting::VideoLoopByDefault),
    CatalogItem::Toggle(ToggleSetting::LiveLowDelayDecode),
];
const LAYOUT: &[CatalogItem] = &[
    CatalogItem::Toggle(ToggleSetting::StatusBarVisible),
    CatalogItem::Toggle(ToggleSetting::RoomMenuVisible),
];
const INPUT: &[CatalogItem] = &[
    CatalogItem::Choice(ScalarSetting::BindingMode),
    CatalogItem::Keymap,
];
const DIAGNOSTICS: &[CatalogItem] = &[CatalogItem::Diagnostics];

pub(crate) static SETTINGS_SECTIONS: &[SettingsSection] = &[
    SettingsSection {
        id: "appearance",
        title: "Appearance",
        help: "Application backgrounds and general text colors.",
        items: APPEARANCE,
    },
    SettingsSection {
        id: "typography",
        title: "Typography",
        help: "Font families, sizes, and text rendering.",
        items: TYPOGRAPHY,
    },
    SettingsSection {
        id: "syntax",
        title: "Syntax",
        help: "Colors used by formatted messages and code.",
        items: SYNTAX,
    },
    SettingsSection {
        id: "media",
        title: "Media",
        help: "Video surfaces, overlays, progress, and labels.",
        items: MEDIA,
    },
    SettingsSection {
        id: "layout",
        title: "Layout",
        help: "Default visibility of the main room interface.",
        items: LAYOUT,
    },
    SettingsSection {
        id: "input",
        title: "Input & keymap",
        help: "Composer behavior and renderer-owned action bindings.",
        items: INPUT,
    },
    SettingsSection {
        id: "diagnostics",
        title: "Diagnostics",
        help: "Configuration warnings and errors.",
        items: DIAGNOSTICS,
    },
];

pub(crate) fn rows(section: &SettingsSection, diagnostic_count: usize) -> Vec<RowRef> {
    let mut rows = Vec::new();
    for item in section.items {
        match *item {
            CatalogItem::ThemeGroup(group) => rows.extend(
                THEME_ROLES
                    .iter()
                    .filter(move |spec| spec.group == group)
                    .map(|spec| RowRef::Theme(spec.role)),
            ),
            CatalogItem::Font(role) => {
                rows.push(RowRef::FontFamily(role));
                rows.push(RowRef::FontSize(role));
            }
            CatalogItem::Choice(choice) => rows.push(RowRef::Choice(choice)),
            CatalogItem::Toggle(toggle) => rows.push(RowRef::Toggle(toggle)),
            CatalogItem::Keymap => rows.extend(
                BINDINGS
                    .iter()
                    .map(|binding| RowRef::Binding(binding.scope, binding.command)),
            ),
            CatalogItem::Diagnostics => {
                rows.extend((0..diagnostic_count).map(RowRef::Diagnostic));
            }
        }
    }
    rows
}

pub(crate) fn path(row: RowRef) -> String {
    match row {
        RowRef::Theme(role) => {
            let spec = THEME_ROLES
                .iter()
                .find(|candidate| candidate.role == role)
                .expect("theme row belongs to registry");
            format!("theme.{}.{}", spec.group.table(), spec.key)
        }
        RowRef::FontFamily(role) => {
            format!("fonts.{}-family", font_spec(role).key_stem)
        }
        RowRef::FontSize(role) => format!("fonts.{}-size", font_spec(role).key_stem),
        RowRef::Choice(ScalarSetting::FontRendering) => "fonts.rendering".into(),
        RowRef::Choice(ScalarSetting::BindingMode) => "input.default-binding-mode".into(),
        RowRef::Toggle(ToggleSetting::StatusBarVisible) => "layout.status-bar-visible".into(),
        RowRef::Toggle(ToggleSetting::RoomMenuVisible) => "layout.room-menu-visible".into(),
        RowRef::Toggle(ToggleSetting::NativeFullscreen) => "native-fullscreen".into(),
        RowRef::Toggle(ToggleSetting::VideoLoopByDefault) => "video-loop-by-default".into(),
        RowRef::Toggle(ToggleSetting::LiveLowDelayDecode) => "live-low-delay-decode".into(),
        RowRef::Binding(scope, command) => {
            format!("bindings.{}.{}", scope.key(), format!("{command:?}"))
        }
        RowRef::Diagnostic(index) => format!("diagnostic.{index}"),
    }
}

pub(crate) fn label(row: RowRef) -> &'static str {
    match row {
        RowRef::Theme(role) => {
            THEME_ROLES
                .iter()
                .find(|candidate| candidate.role == role)
                .expect("theme row belongs to registry")
                .label
        }
        RowRef::FontFamily(role) => font_spec(role).label,
        RowRef::FontSize(role) => match role {
            FontRole::Interface => "Interface font size",
            FontRole::Message => "Message font size",
            FontRole::Code => "Code font size",
        },
        RowRef::Choice(ScalarSetting::FontRendering) => "Text rendering",
        RowRef::Choice(ScalarSetting::BindingMode) => "Default composer mode",
        RowRef::Toggle(ToggleSetting::StatusBarVisible) => "Show status bar by default",
        RowRef::Toggle(ToggleSetting::RoomMenuVisible) => "Show room menu by default",
        RowRef::Toggle(ToggleSetting::NativeFullscreen) => "Use native fullscreen for media",
        RowRef::Toggle(ToggleSetting::VideoLoopByDefault) => "Loop videos by default",
        RowRef::Toggle(ToggleSetting::LiveLowDelayDecode) => "Low-delay decoding for live shares",
        RowRef::Binding(scope, command) => {
            crate::key_bindings::spec(scope, command)
                .expect("binding row belongs to registry")
                .label
        }
        RowRef::Diagnostic(_) => "Configuration diagnostic",
    }
}

pub(crate) fn help(row: RowRef) -> Option<&'static str> {
    match row {
        RowRef::Theme(role) => {
            let group = THEME_ROLES
                .iter()
                .find(|candidate| candidate.role == role)
                .expect("theme row belongs to registry")
                .group;
            Some(group.help())
        }
        RowRef::FontFamily(role) => Some(font_spec(role).sample),
        RowRef::FontSize(FontRole::Interface) => {
            Some("Rem-based interface spacing scales with this size.")
        }
        RowRef::FontSize(_) => Some("Accepted range: 8 through 48 px."),
        RowRef::Choice(ScalarSetting::FontRendering) => Some(
            "Subpixel rendering is a request; GPUI falls back when the platform, window, or renderer cannot support it.",
        ),
        RowRef::Choice(ScalarSetting::BindingMode) => {
            Some("Standard starts in Insert; Vim starts in Normal mode.")
        }
        RowRef::Toggle(ToggleSetting::StatusBarVisible) => {
            Some("Controls whether the status bar above a room is shown when the GUI starts.")
        }
        RowRef::Toggle(ToggleSetting::RoomMenuVisible) => {
            Some("Controls whether the room menu sidebar is shown when the GUI starts.")
        }
        RowRef::Toggle(ToggleSetting::NativeFullscreen) => Some(
            "When enabled, fullscreen videos and live streams also fullscreen the application window.",
        ),
        RowRef::Toggle(ToggleSetting::VideoLoopByDefault) => Some(
            "Sets the initial loop state for each video. The playback control can override it per video.",
        ),
        RowRef::Toggle(ToggleSetting::LiveLowDelayDecode) => Some(
            "Shows every live share frame the moment it is decoded, assuming the stream \
             carries no B-frames. Disable if a share encoded with B-frames plays back \
             with dropped or reordered frames. Applies to shares opened after saving.",
        ),
        RowRef::Binding(scope, command) => {
            crate::key_bindings::spec(scope, command)
                .expect("binding row belongs to registry")
                .help
        }
        RowRef::Diagnostic(_) => None,
    }
}

fn font_spec(role: FontRole) -> &'static crate::theme::FontRoleSpec {
    FONT_ROLES
        .iter()
        .find(|candidate| candidate.role == role)
        .expect("font row belongs to registry")
}

pub(crate) fn matches_search(section: &SettingsSection, row: RowRef, query: &str) -> bool {
    let query = query.trim().to_ascii_lowercase();
    if query.is_empty() {
        return true;
    }
    [
        section.title,
        label(row),
        help(row).unwrap_or_default(),
        &path(row),
    ]
    .iter()
    .any(|candidate| candidate.to_ascii_lowercase().contains(&query))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_contains_each_persisted_appearance_row_once() {
        let all = SETTINGS_SECTIONS
            .iter()
            .flat_map(|section| rows(section, 0))
            .collect::<Vec<_>>();
        for role in THEME_ROLES.iter().map(|spec| spec.role) {
            assert_eq!(
                all.iter()
                    .filter(|row| **row == RowRef::Theme(role))
                    .count(),
                1
            );
        }
        for role in FONT_ROLES.iter().map(|spec| spec.role) {
            assert_eq!(
                all.iter()
                    .filter(|row| **row == RowRef::FontFamily(role))
                    .count(),
                1
            );
            assert_eq!(
                all.iter()
                    .filter(|row| **row == RowRef::FontSize(role))
                    .count(),
                1
            );
        }
    }

    #[test]
    fn layout_section_contains_both_visibility_toggles() {
        let layout = SETTINGS_SECTIONS
            .iter()
            .find(|section| section.id == "layout")
            .expect("layout section is registered");

        assert_eq!(
            rows(layout, 0),
            vec![
                RowRef::Toggle(ToggleSetting::StatusBarVisible),
                RowRef::Toggle(ToggleSetting::RoomMenuVisible),
            ]
        );
    }

    #[test]
    fn media_section_contains_native_fullscreen_toggle() {
        let media = SETTINGS_SECTIONS
            .iter()
            .find(|section| section.id == "media")
            .expect("media section is registered");

        assert!(rows(media, 0).contains(&RowRef::Toggle(ToggleSetting::NativeFullscreen)));
        assert_eq!(
            path(RowRef::Toggle(ToggleSetting::NativeFullscreen)),
            "native-fullscreen"
        );
    }

    #[test]
    fn media_section_contains_video_loop_default_toggle() {
        let media = SETTINGS_SECTIONS
            .iter()
            .find(|section| section.id == "media")
            .expect("media section is registered");

        assert!(rows(media, 0).contains(&RowRef::Toggle(ToggleSetting::VideoLoopByDefault)));
        assert_eq!(
            path(RowRef::Toggle(ToggleSetting::VideoLoopByDefault)),
            "video-loop-by-default"
        );
    }

    #[test]
    fn media_section_contains_live_low_delay_decode_toggle() {
        let media = SETTINGS_SECTIONS
            .iter()
            .find(|section| section.id == "media")
            .expect("media section is registered");

        assert!(rows(media, 0).contains(&RowRef::Toggle(ToggleSetting::LiveLowDelayDecode)));
        assert_eq!(
            path(RowRef::Toggle(ToggleSetting::LiveLowDelayDecode)),
            "live-low-delay-decode"
        );
    }
}
