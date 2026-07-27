use std::{collections::BTreeMap, fmt};

use toml_spanner::{Arena, Context, Failed, FromToml, Item, ToToml, ToTomlError, Toml};

pub(crate) const GUI_SCHEMA_VERSION: u16 = 1;

pub(crate) const DEFAULT_INTERFACE_FAMILY: &str = ".SystemUIFont";
pub(crate) const DEFAULT_MESSAGE_FAMILY: &str = "IBM Plex Sans";
pub(crate) const DEFAULT_CODE_FAMILY: &str = "Lilex";
pub(crate) const DEFAULT_INTERFACE_SIZE: f32 = 16.0;
pub(crate) const DEFAULT_MESSAGE_SIZE: f32 = 16.0;
pub(crate) const DEFAULT_CODE_SIZE: f32 = 14.0;

#[derive(Clone, Debug, PartialEq, Toml)]
#[toml(
    FromToml,
    ToToml,
    recoverable,
    warn_unknown_fields,
    rename_all = "kebab-case"
)]
pub(crate) struct GuiConfig {
    #[toml(default = GUI_SCHEMA_VERSION)]
    pub(crate) schema_version: u16,
    #[toml(default, style = Header)]
    pub(crate) theme: ThemeConfig,
    #[toml(default, style = Header)]
    pub(crate) fonts: FontConfig,
    #[toml(default, style = Header)]
    pub(crate) input: InputConfig,
    #[toml(default, style = Header)]
    pub(crate) bindings: BindingsConfig,
}

impl Default for GuiConfig {
    fn default() -> Self {
        Self {
            schema_version: GUI_SCHEMA_VERSION,
            theme: ThemeConfig::default(),
            fonts: FontConfig::default(),
            input: InputConfig::default(),
            bindings: BindingsConfig::default(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Toml)]
#[toml(FromToml, ToToml, recoverable, rename_all = "kebab-case")]
pub(crate) struct ThemeConfig {
    #[toml(default, style = Header)]
    pub(crate) surfaces: SurfaceColors,
    #[toml(default, style = Header)]
    pub(crate) text: TextColors,
    #[toml(default, style = Header)]
    pub(crate) borders: BorderColors,
    #[toml(default, style = Header)]
    pub(crate) states: StateColors,
    #[toml(default, style = Header)]
    pub(crate) controls: ControlColors,
    #[toml(default, style = Header)]
    pub(crate) scrollbar: ScrollbarColors,
    #[toml(default, style = Header)]
    pub(crate) participants: ParticipantColors,
    #[toml(default, style = Header)]
    pub(crate) syntax: SyntaxColors,
    #[toml(default, style = Header)]
    pub(crate) media: MediaColors,
}

impl Default for ThemeConfig {
    fn default() -> Self {
        Self {
            surfaces: SurfaceColors::default(),
            text: TextColors::default(),
            borders: BorderColors::default(),
            states: StateColors::default(),
            controls: ControlColors::default(),
            scrollbar: ScrollbarColors::default(),
            participants: ParticipantColors::default(),
            syntax: SyntaxColors::default(),
            media: MediaColors::default(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Toml)]
#[toml(FromToml, ToToml, recoverable, rename_all = "kebab-case")]
pub(crate) struct SurfaceColors {
    #[toml(default = Rgba8::rgb(0x16, 0x16, 0x16))]
    pub(crate) window: Rgba8,
    #[toml(default = Rgba8::rgb(0x1d, 0x1d, 0x1d))]
    pub(crate) sidebar: Rgba8,
    #[toml(default = Rgba8::rgb(0x1d, 0x1d, 0x1d))]
    pub(crate) raised: Rgba8,
    #[toml(default = Rgba8::rgb(0x16, 0x16, 0x16))]
    pub(crate) media: Rgba8,
    #[toml(default = Rgba8::rgb(0x1d, 0x1d, 0x1d))]
    pub(crate) toolbar: Rgba8,
    #[toml(default = Rgba8::rgb(0x1d, 0x1d, 0x1d))]
    pub(crate) panel: Rgba8,
    #[toml(default = Rgba8::rgb(0x1d, 0x1d, 0x1d))]
    pub(crate) input: Rgba8,
    #[toml(default = Rgba8::rgb(0x0e, 0x0e, 0x0e))]
    pub(crate) code: Rgba8,
    #[toml(default = Rgba8::rgba(0x00, 0x00, 0x00, 0xdd))]
    pub(crate) scrim: Rgba8,
}

impl Default for SurfaceColors {
    fn default() -> Self {
        Self {
            window: Rgba8::rgb(0x16, 0x16, 0x16),
            sidebar: Rgba8::rgb(0x1d, 0x1d, 0x1d),
            raised: Rgba8::rgb(0x1d, 0x1d, 0x1d),
            media: Rgba8::rgb(0x16, 0x16, 0x16),
            toolbar: Rgba8::rgb(0x1d, 0x1d, 0x1d),
            panel: Rgba8::rgb(0x1d, 0x1d, 0x1d),
            input: Rgba8::rgb(0x1d, 0x1d, 0x1d),
            code: Rgba8::rgb(0x0e, 0x0e, 0x0e),
            scrim: Rgba8::rgba(0x00, 0x00, 0x00, 0xdd),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Toml)]
#[toml(FromToml, ToToml, recoverable, rename_all = "kebab-case")]
pub(crate) struct TextColors {
    #[toml(default = Rgba8::rgb(0xe5, 0xe5, 0xe5))]
    pub(crate) primary: Rgba8,
    #[toml(default = Rgba8::rgb(0x8f, 0x8f, 0x8f))]
    pub(crate) muted: Rgba8,
    #[toml(default = Rgba8::rgb(0xf0, 0xf0, 0xf0))]
    pub(crate) link: Rgba8,
    #[toml(default = Rgba8::rgb(0xd8, 0xd8, 0xd8))]
    pub(crate) body: Rgba8,
    #[toml(default = Rgba8::rgb(0x73, 0x73, 0x73))]
    pub(crate) dim: Rgba8,
    #[toml(default = Rgba8::rgb(0x66, 0x66, 0x66))]
    pub(crate) subtle: Rgba8,
    #[toml(default = Rgba8::rgb(0xb8, 0xb8, 0xb8))]
    pub(crate) secondary: Rgba8,
    #[toml(default = Rgba8::rgb(0xf5, 0xf5, 0xf5))]
    pub(crate) inverse: Rgba8,
}

impl Default for TextColors {
    fn default() -> Self {
        Self {
            primary: Rgba8::rgb(0xe5, 0xe5, 0xe5),
            muted: Rgba8::rgb(0x8f, 0x8f, 0x8f),
            link: Rgba8::rgb(0xf0, 0xf0, 0xf0),
            body: Rgba8::rgb(0xd8, 0xd8, 0xd8),
            dim: Rgba8::rgb(0x73, 0x73, 0x73),
            subtle: Rgba8::rgb(0x66, 0x66, 0x66),
            secondary: Rgba8::rgb(0xb8, 0xb8, 0xb8),
            inverse: Rgba8::rgb(0xf5, 0xf5, 0xf5),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Toml)]
#[toml(FromToml, ToToml, recoverable, rename_all = "kebab-case")]
pub(crate) struct BorderColors {
    #[toml(default = Rgba8::rgb(0x2a, 0x2a, 0x2a))]
    pub(crate) subtle: Rgba8,
    #[toml(default = Rgba8::rgb(0x2a, 0x2a, 0x2a))]
    pub(crate) strong: Rgba8,
    #[toml(default = Rgba8::rgb(0x2a, 0x2a, 0x2a))]
    pub(crate) code: Rgba8,
    #[toml(default = Rgba8::rgb(0x2a, 0x2a, 0x2a))]
    pub(crate) media: Rgba8,
    #[toml(default = Rgba8::rgb(0x73, 0x73, 0x73))]
    pub(crate) focus: Rgba8,
    #[toml(default = Rgba8::rgb(0x4a, 0x4a, 0x4a))]
    pub(crate) quote: Rgba8,
}

impl Default for BorderColors {
    fn default() -> Self {
        Self {
            subtle: Rgba8::rgb(0x2a, 0x2a, 0x2a),
            strong: Rgba8::rgb(0x2a, 0x2a, 0x2a),
            code: Rgba8::rgb(0x2a, 0x2a, 0x2a),
            media: Rgba8::rgb(0x2a, 0x2a, 0x2a),
            focus: Rgba8::rgb(0x73, 0x73, 0x73),
            quote: Rgba8::rgb(0x4a, 0x4a, 0x4a),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Toml)]
#[toml(FromToml, ToToml, recoverable, rename_all = "kebab-case")]
pub(crate) struct StateColors {
    #[toml(default = Rgba8::rgb(0x18, 0x18, 0x18))]
    pub(crate) hover: Rgba8,
    #[toml(default = Rgba8::rgb(0x11, 0x11, 0x11))]
    pub(crate) row_hover: Rgba8,
    #[toml(default = Rgba8::rgb(0x24, 0x24, 0x24))]
    pub(crate) pressed: Rgba8,
    #[toml(default = Rgba8::rgb(0x26, 0x26, 0x26))]
    pub(crate) selected: Rgba8,
    #[toml(default = Rgba8::rgba(0xff, 0xff, 0xff, 0x1f))]
    pub(crate) selection: Rgba8,
    #[toml(default = Rgba8::rgba(0xd9, 0xa4, 0x41, 0x44))]
    pub(crate) search: Rgba8,
    #[toml(default = Rgba8::rgba(0xd9, 0xa4, 0x41, 0x66))]
    pub(crate) current_search: Rgba8,
    #[toml(default = Rgba8::rgb(0x7f, 0x9c, 0x70))]
    pub(crate) success: Rgba8,
    #[toml(default = Rgba8::rgb(0xd9, 0xa0, 0x66))]
    pub(crate) warning: Rgba8,
    #[toml(default = Rgba8::rgb(0xd9, 0x9a, 0x93))]
    pub(crate) danger: Rgba8,
    #[toml(default = Rgba8::rgb(0x55, 0x55, 0x55))]
    pub(crate) disabled: Rgba8,
    #[toml(default = Rgba8::rgba(0xff, 0xff, 0xff, 0x24))]
    pub(crate) composer_selection: Rgba8,
    #[toml(default = Rgba8::rgba(0xd0, 0xd0, 0xd0, 0x88))]
    pub(crate) cursor_normal: Rgba8,
    #[toml(default = Rgba8::rgba(0xe5, 0xe5, 0xe5, 0xff))]
    pub(crate) cursor_insert: Rgba8,
    #[toml(default = Rgba8::rgba(0xff, 0xff, 0xff, 0x14))]
    pub(crate) inline_code: Rgba8,
}

impl Default for StateColors {
    fn default() -> Self {
        Self {
            hover: Rgba8::rgb(0x18, 0x18, 0x18),
            row_hover: Rgba8::rgb(0x11, 0x11, 0x11),
            pressed: Rgba8::rgb(0x24, 0x24, 0x24),
            selected: Rgba8::rgb(0x26, 0x26, 0x26),
            selection: Rgba8::rgba(0xff, 0xff, 0xff, 0x1f),
            search: Rgba8::rgba(0xd9, 0xa4, 0x41, 0x44),
            current_search: Rgba8::rgba(0xd9, 0xa4, 0x41, 0x66),
            success: Rgba8::rgb(0x7f, 0x9c, 0x70),
            warning: Rgba8::rgb(0xd9, 0xa0, 0x66),
            danger: Rgba8::rgb(0xd9, 0x9a, 0x93),
            disabled: Rgba8::rgb(0x55, 0x55, 0x55),
            composer_selection: Rgba8::rgba(0xff, 0xff, 0xff, 0x24),
            cursor_normal: Rgba8::rgba(0xd0, 0xd0, 0xd0, 0x88),
            cursor_insert: Rgba8::rgba(0xe5, 0xe5, 0xe5, 0xff),
            inline_code: Rgba8::rgba(0xff, 0xff, 0xff, 0x14),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Toml)]
#[toml(FromToml, ToToml, recoverable, rename_all = "kebab-case")]
pub(crate) struct ControlColors {
    #[toml(default = Rgba8::rgb(0x18, 0x18, 0x18))]
    pub(crate) surface: Rgba8,
    #[toml(default = Rgba8::rgb(0x26, 0x26, 0x26))]
    pub(crate) surface_hover: Rgba8,
    #[toml(default = Rgba8::rgb(0x17, 0x17, 0x17))]
    pub(crate) button: Rgba8,
    #[toml(default = Rgba8::rgb(0x24, 0x24, 0x24))]
    pub(crate) button_hover: Rgba8,
    #[toml(default = Rgba8::rgb(0x30, 0x30, 0x30))]
    pub(crate) active: Rgba8,
    #[toml(default = Rgba8::rgb(0xf2, 0xf2, 0xf2))]
    pub(crate) active_text: Rgba8,
}

impl Default for ControlColors {
    fn default() -> Self {
        Self {
            surface: Rgba8::rgb(0x18, 0x18, 0x18),
            surface_hover: Rgba8::rgb(0x26, 0x26, 0x26),
            button: Rgba8::rgb(0x17, 0x17, 0x17),
            button_hover: Rgba8::rgb(0x24, 0x24, 0x24),
            active: Rgba8::rgb(0x30, 0x30, 0x30),
            active_text: Rgba8::rgb(0xf2, 0xf2, 0xf2),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Toml)]
#[toml(FromToml, ToToml, recoverable, rename_all = "kebab-case")]
pub(crate) struct ScrollbarColors {
    #[toml(default = Rgba8::rgba(0x00, 0x00, 0x00, 0xdd))]
    pub(crate) track: Rgba8,
    #[toml(default = Rgba8::rgba(0x50, 0x50, 0x50, 0xcc))]
    pub(crate) thumb: Rgba8,
    #[toml(default = Rgba8::rgba(0x78, 0x78, 0x78, 0xdd))]
    pub(crate) thumb_hover: Rgba8,
}

impl Default for ScrollbarColors {
    fn default() -> Self {
        Self {
            track: Rgba8::rgba(0x00, 0x00, 0x00, 0xdd),
            thumb: Rgba8::rgba(0x50, 0x50, 0x50, 0xcc),
            thumb_hover: Rgba8::rgba(0x78, 0x78, 0x78, 0xdd),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Toml)]
#[toml(FromToml, ToToml, recoverable, rename_all = "kebab-case")]
pub(crate) struct ParticipantColors {
    #[toml(default = Rgba8::rgb(0x9f, 0xbd, 0x89))]
    pub(crate) local: Rgba8,
    #[toml(default = Rgba8::rgb(0x8c, 0xa9, 0xd8))]
    pub(crate) remote_one: Rgba8,
    #[toml(default = Rgba8::rgb(0xc4, 0x9a, 0xcb))]
    pub(crate) remote_two: Rgba8,
    #[toml(default = Rgba8::rgb(0xd1, 0xa4, 0x77))]
    pub(crate) remote_three: Rgba8,
    #[toml(default = Rgba8::rgb(0x79, 0xb9, 0xb0))]
    pub(crate) remote_four: Rgba8,
    #[toml(default = Rgba8::rgb(0x33, 0x41, 0x2f))]
    pub(crate) identity_surface: Rgba8,
    #[toml(default = Rgba8::rgb(0xaa, 0xcb, 0x93))]
    pub(crate) identity_text: Rgba8,
}

impl Default for ParticipantColors {
    fn default() -> Self {
        Self {
            local: Rgba8::rgb(0x9f, 0xbd, 0x89),
            remote_one: Rgba8::rgb(0x8c, 0xa9, 0xd8),
            remote_two: Rgba8::rgb(0xc4, 0x9a, 0xcb),
            remote_three: Rgba8::rgb(0xd1, 0xa4, 0x77),
            remote_four: Rgba8::rgb(0x79, 0xb9, 0xb0),
            identity_surface: Rgba8::rgb(0x33, 0x41, 0x2f),
            identity_text: Rgba8::rgb(0xaa, 0xcb, 0x93),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Toml)]
#[toml(FromToml, ToToml, recoverable, rename_all = "kebab-case")]
pub(crate) struct SyntaxColors {
    #[toml(default = Rgba8::rgb(0xbd, 0xc0, 0xbe))]
    pub(crate) foreground: Rgba8,
    #[toml(default = Rgba8::rgb(0xeb, 0xc7, 0x82))]
    pub(crate) r#type: Rgba8,
    #[toml(default = Rgba8::rgb(0x8a, 0xa6, 0xbd))]
    pub(crate) function: Rgba8,
    #[toml(default = Rgba8::rgb(0xc8, 0x72, 0x70))]
    pub(crate) binding: Rgba8,
    #[toml(default = Rgba8::rgb(0xd9, 0x9a, 0x6d))]
    pub(crate) namespace: Rgba8,
    #[toml(default = Rgba8::rgb(0xb4, 0x9b, 0xbb))]
    pub(crate) keyword: Rgba8,
    #[toml(default = Rgba8::rgb(0xb8, 0xbe, 0x77))]
    pub(crate) string: Rgba8,
    #[toml(default = Rgba8::rgb(0xcc, 0xcc, 0xcc))]
    pub(crate) number: Rgba8,
    #[toml(default = Rgba8::rgb(0x8a, 0x8c, 0x8a))]
    pub(crate) comment: Rgba8,
}

impl Default for SyntaxColors {
    fn default() -> Self {
        Self {
            foreground: Rgba8::rgb(0xbd, 0xc0, 0xbe),
            r#type: Rgba8::rgb(0xeb, 0xc7, 0x82),
            function: Rgba8::rgb(0x8a, 0xa6, 0xbd),
            binding: Rgba8::rgb(0xc8, 0x72, 0x70),
            namespace: Rgba8::rgb(0xd9, 0x9a, 0x6d),
            keyword: Rgba8::rgb(0xb4, 0x9b, 0xbb),
            string: Rgba8::rgb(0xb8, 0xbe, 0x77),
            number: Rgba8::rgb(0xcc, 0xcc, 0xcc),
            comment: Rgba8::rgb(0x8a, 0x8c, 0x8a),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Toml)]
#[toml(FromToml, ToToml, recoverable, rename_all = "kebab-case")]
pub(crate) struct MediaColors {
    #[toml(default = Rgba8::rgb(0x00, 0x00, 0x00))]
    pub(crate) viewport: Rgba8,
    #[toml(default = Rgba8::rgba(0x00, 0x00, 0x00, 0xcc))]
    pub(crate) overlay: Rgba8,
    #[toml(default = Rgba8::rgba(0x00, 0x00, 0x00, 0xf2))]
    pub(crate) overlay_strong: Rgba8,
    #[toml(default = Rgba8::rgb(0x2a, 0x2a, 0x2a))]
    pub(crate) border: Rgba8,
    #[toml(default = Rgba8::rgba(0xc8, 0xc8, 0xc8, 0x33))]
    pub(crate) progress_track: Rgba8,
    #[toml(default = Rgba8::rgb(0xa3, 0xa3, 0xa3))]
    pub(crate) progress_fill: Rgba8,
    #[toml(default = Rgba8::rgb(0xe5, 0xe5, 0xe5))]
    pub(crate) progress_knob: Rgba8,
    #[toml(default = Rgba8::rgba(0x00, 0x00, 0x00, 0x00))]
    pub(crate) gradient_start: Rgba8,
    #[toml(default = Rgba8::rgba(0x00, 0x00, 0x00, 0xe8))]
    pub(crate) gradient_end: Rgba8,
    #[toml(default = Rgba8::rgb(0xe5, 0xe5, 0xe5))]
    pub(crate) text: Rgba8,
    #[toml(default = Rgba8::rgb(0x8f, 0x8f, 0x8f))]
    pub(crate) muted_text: Rgba8,
}

impl Default for MediaColors {
    fn default() -> Self {
        Self {
            viewport: Rgba8::rgb(0x00, 0x00, 0x00),
            overlay: Rgba8::rgba(0x00, 0x00, 0x00, 0xcc),
            overlay_strong: Rgba8::rgba(0x00, 0x00, 0x00, 0xf2),
            border: Rgba8::rgb(0x2a, 0x2a, 0x2a),
            progress_track: Rgba8::rgba(0xc8, 0xc8, 0xc8, 0x33),
            progress_fill: Rgba8::rgb(0xa3, 0xa3, 0xa3),
            progress_knob: Rgba8::rgb(0xe5, 0xe5, 0xe5),
            gradient_start: Rgba8::rgba(0x00, 0x00, 0x00, 0x00),
            gradient_end: Rgba8::rgba(0x00, 0x00, 0x00, 0xe8),
            text: Rgba8::rgb(0xe5, 0xe5, 0xe5),
            muted_text: Rgba8::rgb(0x8f, 0x8f, 0x8f),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Toml)]
#[toml(FromToml, ToToml, recoverable, rename_all = "kebab-case")]
pub(crate) struct FontConfig {
    #[toml(default = DEFAULT_INTERFACE_FAMILY.to_string())]
    pub(crate) interface_family: String,
    #[toml(default = DEFAULT_MESSAGE_FAMILY.to_string())]
    pub(crate) message_family: String,
    #[toml(default = DEFAULT_CODE_FAMILY.to_string())]
    pub(crate) code_family: String,
    #[toml(default = DEFAULT_INTERFACE_SIZE)]
    pub(crate) interface_size: f32,
    #[toml(default = DEFAULT_MESSAGE_SIZE)]
    pub(crate) message_size: f32,
    #[toml(default = DEFAULT_CODE_SIZE)]
    pub(crate) code_size: f32,
    #[toml(default)]
    pub(crate) rendering: FontRendering,
}

impl Default for FontConfig {
    fn default() -> Self {
        Self {
            interface_family: DEFAULT_INTERFACE_FAMILY.to_string(),
            message_family: DEFAULT_MESSAGE_FAMILY.to_string(),
            code_family: DEFAULT_CODE_FAMILY.to_string(),
            interface_size: DEFAULT_INTERFACE_SIZE,
            message_size: DEFAULT_MESSAGE_SIZE,
            code_size: DEFAULT_CODE_SIZE,
            rendering: FontRendering::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Toml)]
#[toml(FromToml, ToToml, rename_all = "kebab-case")]
pub(crate) enum FontRendering {
    #[default]
    PlatformDefault,
    Subpixel,
    Grayscale,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Toml)]
#[toml(FromToml, ToToml, rename_all = "kebab-case")]
pub(crate) enum BindingMode {
    Standard,
    #[default]
    Vim,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Toml)]
#[toml(FromToml, ToToml, recoverable, rename_all = "kebab-case")]
pub(crate) struct InputConfig {
    #[toml(default)]
    pub(crate) default_binding_mode: BindingMode,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Toml)]
#[toml(FromToml, ToToml, recoverable, rename_all = "kebab-case")]
pub(crate) struct BindingsConfig {
    #[toml(default, style = Header)]
    pub(crate) application: BindingTable,
    #[toml(default, style = Header)]
    pub(crate) composer: BindingTable,
    #[toml(default, style = Header)]
    pub(crate) completion: BindingTable,
    #[toml(default, style = Header)]
    pub(crate) vim: BindingTable,
    #[toml(default, style = Header)]
    pub(crate) code_search: BindingTable,
    #[toml(default, style = Header)]
    pub(crate) code_viewer: BindingTable,
    #[toml(default, style = Header)]
    pub(crate) formatted_message: BindingTable,
    #[toml(default, style = Header)]
    pub(crate) non_input: BindingTable,
}

pub(crate) type BindingTable = BTreeMap<String, BindCommand>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Toml)]
#[toml(FromToml, ToToml)]
pub(crate) enum BindCommand {
    OpenMedia,
    OpenSettings,
    IncreaseUiScale,
    DecreaseUiScale,
    ResetUiScale,
    ToggleMute,
    ToggleDeafen,
    ToggleVoice,
    SendMessage,
    Newline,
    Backspace,
    Delete,
    Left,
    Right,
    SelectLeft,
    SelectRight,
    SelectAll,
    Paste,
    Copy,
    Cut,
    InsertTab,
    CompletionNext,
    CompletionPrevious,
    CompletionAccept,
    CompletionAcceptEngaged,
    CompletionDismiss,
    FindInCode,
    NextCodeMatch,
    PreviousCodeMatch,
    CloseCodeSearch,
    ClosePreview,
    TogglePlayback,
    SeekBack,
    SeekForward,
    DecreaseContrast,
    IncreaseContrast,
    DecreaseBrightness,
    IncreaseBrightness,
    DecreaseGamma,
    IncreaseGamma,
    DecreaseSaturation,
    IncreaseSaturation,
    DecreaseVolume,
    IncreaseVolume,
    DecreasePlaybackSpeed,
    IncreasePlaybackSpeed,
    PreviousFrame,
    NextFrame,
    LiveZoomIn,
    LiveZoomOut,
    LiveReset,
    LivePanUp,
    LivePanDown,
    Unbind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Rgba8(u32);

impl Rgba8 {
    pub(crate) const fn rgb(red: u8, green: u8, blue: u8) -> Self {
        Self::rgba(red, green, blue, u8::MAX)
    }

    pub(crate) const fn rgba(red: u8, green: u8, blue: u8, alpha: u8) -> Self {
        Self(u32::from_be_bytes([red, green, blue, alpha]))
    }

    pub(crate) const fn packed(self) -> u32 {
        self.0
    }

    pub(crate) fn parse(value: &str) -> Result<Self, &'static str> {
        let Some(hex) = value.strip_prefix('#') else {
            return Err("expected a color beginning with '#'");
        };
        let mut channels = [0_u8; 4];
        match hex.len() {
            3 | 4 => {
                for (index, digit) in hex.bytes().enumerate() {
                    let nibble = hex_nibble(digit).ok_or("color contains a non-hex digit")?;
                    channels[index] = nibble * 0x11;
                }
                if hex.len() == 3 {
                    channels[3] = u8::MAX;
                }
            }
            6 | 8 => {
                for (index, pair) in hex.as_bytes().chunks_exact(2).enumerate() {
                    let high = hex_nibble(pair[0]).ok_or("color contains a non-hex digit")?;
                    let low = hex_nibble(pair[1]).ok_or("color contains a non-hex digit")?;
                    channels[index] = (high << 4) | low;
                }
                if hex.len() == 6 {
                    channels[3] = u8::MAX;
                }
            }
            _ => return Err("expected #rgb, #rgba, #rrggbb, or #rrggbbaa"),
        }
        Ok(Self::rgba(
            channels[0],
            channels[1],
            channels[2],
            channels[3],
        ))
    }
}

impl fmt::Display for Rgba8 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [red, green, blue, alpha] = self.0.to_be_bytes();
        if alpha == u8::MAX {
            write!(formatter, "#{red:02x}{green:02x}{blue:02x}")
        } else {
            write!(formatter, "#{red:02x}{green:02x}{blue:02x}{alpha:02x}")
        }
    }
}

impl<'de> FromToml<'de> for Rgba8 {
    fn from_toml(ctx: &mut Context<'de>, item: &Item<'de>) -> Result<Self, Failed> {
        let value = <&str>::from_toml(ctx, item)?;
        Self::parse(value).map_err(|error| ctx.report_custom_error(error, item))
    }
}

impl ToToml for Rgba8 {
    fn to_toml<'a>(&'a self, arena: &'a Arena) -> Result<Item<'a>, ToTomlError> {
        let value = self.to_string();
        Ok(Item::string(arena.alloc_str(&value)))
    }
}

const fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_file_uses_concrete_defaults() {
        let config: GuiConfig = toml_spanner::from_str(
            r##"
[theme.text]
link = "#abc"

[fonts]
code-size = 15.5
"##,
        )
        .unwrap();

        assert_eq!(config.schema_version, GUI_SCHEMA_VERSION);
        assert_eq!(config.theme.text.link, Rgba8::rgb(0xaa, 0xbb, 0xcc));
        assert_eq!(
            config.theme.text.primary,
            ThemeConfig::default().text.primary
        );
        assert_eq!(config.fonts.code_size, 15.5);
        assert_eq!(
            config.fonts.message_family,
            DEFAULT_MESSAGE_FAMILY.to_string()
        );
    }

    #[test]
    fn default_theme_uses_neutral_core_surfaces() {
        let theme = ThemeConfig::default();

        assert_eq!(theme.surfaces.window, Rgba8::rgb(0x16, 0x16, 0x16));
        assert_eq!(theme.surfaces.raised, Rgba8::rgb(0x1d, 0x1d, 0x1d));
        assert_eq!(theme.surfaces.input, Rgba8::rgb(0x1d, 0x1d, 0x1d));
        assert_eq!(theme.surfaces.code, Rgba8::rgb(0x0e, 0x0e, 0x0e));
        assert_eq!(theme.states.selected, Rgba8::rgb(0x26, 0x26, 0x26));
        assert_eq!(theme.borders.subtle, Rgba8::rgb(0x2a, 0x2a, 0x2a));
        assert_eq!(theme.borders.media, Rgba8::rgb(0x2a, 0x2a, 0x2a));
        assert_eq!(theme.controls.active, Rgba8::rgb(0x30, 0x30, 0x30));
    }

    #[test]
    fn bindings_map_sequences_to_commands() {
        let config: GuiConfig = toml_spanner::from_str(
            r#"
[bindings.application]
"cmd-o" = "OpenMedia"
"secondary-," = "OpenSettings"
"cmd-shift-m" = "Unbind"

[bindings.formatted-message]
"secondary-c" = "Copy"
y = "Copy"

[bindings.completion]
enter = "CompletionAcceptEngaged"

[bindings.non-input]
space = "TogglePlayback"
"[" = "DecreasePlaybackSpeed"
"." = "NextFrame"
"#,
        )
        .unwrap();

        assert_eq!(
            config.bindings.application.get("cmd-o"),
            Some(&BindCommand::OpenMedia)
        );
        assert_eq!(
            config.bindings.application.get("secondary-,"),
            Some(&BindCommand::OpenSettings)
        );
        assert_eq!(
            config.bindings.application.get("cmd-shift-m"),
            Some(&BindCommand::Unbind)
        );
        assert_eq!(
            config.bindings.formatted_message.get("secondary-c"),
            Some(&BindCommand::Copy)
        );
        assert_eq!(
            config.bindings.formatted_message.get("y"),
            Some(&BindCommand::Copy)
        );
        assert_eq!(
            config.bindings.completion.get("enter"),
            Some(&BindCommand::CompletionAcceptEngaged)
        );
        assert_eq!(
            config.bindings.non_input.get("space"),
            Some(&BindCommand::TogglePlayback)
        );
        assert_eq!(
            config.bindings.non_input.get("["),
            Some(&BindCommand::DecreasePlaybackSpeed)
        );
        assert_eq!(
            config.bindings.non_input.get("."),
            Some(&BindCommand::NextFrame)
        );
        assert!(config.bindings.composer.is_empty());
    }

    #[test]
    fn serialization_emits_the_derived_configuration() {
        let rendered = toml_spanner::to_string(&GuiConfig::default()).unwrap();

        assert!(rendered.contains("schema-version = 1"));
        assert!(rendered.contains("[theme.surfaces]"));
        assert!(rendered.contains("window = "));
        assert!(rendered.contains("[fonts]"));
        assert!(rendered.contains("message-family = \"IBM Plex Sans\""));
        assert!(rendered.contains("[input]"));
        assert!(rendered.contains("default-binding-mode = \"vim\""));
    }

    #[test]
    fn binding_tables_round_trip_binding_then_command() {
        let config: GuiConfig = toml_spanner::from_str(
            r#"
[bindings.composer]
enter = "SendMessage"
shift-enter = "Newline"
"secondary-x" = "Unbind"
"#,
        )
        .unwrap();
        let rendered = toml_spanner::to_string(&config).unwrap();
        let reparsed: GuiConfig = toml_spanner::from_str(&rendered).unwrap();

        assert_eq!(reparsed, config);
        assert!(rendered.contains("enter = \"SendMessage\""));
        assert!(rendered.contains("shift-enter = \"Newline\""));
        assert!(rendered.contains("secondary-x = \"Unbind\""));
    }

    #[test]
    fn rgba8_accepts_and_normalizes_supported_forms() {
        let cases = [
            ("#abc", "#aabbcc", 0xaabb_ccff),
            ("#abcd", "#aabbccdd", 0xaabb_ccdd),
            ("#A1b2C3", "#a1b2c3", 0xa1b2_c3ff),
            ("#A1b2C3d4", "#a1b2c3d4", 0xa1b2_c3d4),
        ];

        for (source, rendered, packed) in cases {
            let color = Rgba8::parse(source).unwrap();
            assert_eq!(color.to_string(), rendered);
            assert_eq!(color.packed(), packed);
        }
    }

    #[test]
    fn rgba8_rejects_invalid_forms() {
        for source in ["abc", "#ab", "#abcde", "#gggggg", "#123456789"] {
            assert!(Rgba8::parse(source).is_err(), "{source}");
        }
    }
}
