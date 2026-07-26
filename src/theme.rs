use std::sync::Arc;

use chatt_message_format::highlight::PaletteRole;
use gpui::{App, Global, Rgba, SharedString, TextRenderingMode, rgba};

use crate::config::{
    io::SourceStatus,
    schema::{
        BindingMode, DEFAULT_CODE_FAMILY, DEFAULT_INTERFACE_FAMILY, DEFAULT_MESSAGE_FAMILY,
        FontConfig, FontRendering, GuiConfig, Rgba8, ThemeConfig,
    },
    validation::ConfigDiagnostic,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum ThemeGroup {
    Surfaces,
    Text,
    Borders,
    States,
    Controls,
    Scrollbar,
    Participants,
    Syntax,
    Media,
}

impl ThemeGroup {
    pub(crate) const fn table(self) -> &'static str {
        match self {
            Self::Surfaces => "surfaces",
            Self::Text => "text",
            Self::Borders => "borders",
            Self::States => "states",
            Self::Controls => "controls",
            Self::Scrollbar => "scrollbar",
            Self::Participants => "participants",
            Self::Syntax => "syntax",
            Self::Media => "media",
        }
    }

    pub(crate) const fn help(self) -> &'static str {
        match self {
            Self::Surfaces => "Background colors for application and media surfaces.",
            Self::Text => "Foreground colors used by messages and interface chrome.",
            Self::Borders => "Borders and focus outlines.",
            Self::States => "Hover, pressed, selection, search, and status colors.",
            Self::Controls => "Buttons and other interactive controls.",
            Self::Scrollbar => "Overlay scrollbar track and thumb colors.",
            Self::Participants => "Local and remote participant accents.",
            Self::Syntax => "Colors mapped from every message-format syntax role.",
            Self::Media => "Video surfaces, overlays, progress, and labels.",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u16)]
pub(crate) enum ThemeRole {
    Window,
    Sidebar,
    Raised,
    Media,
    Toolbar,
    Panel,
    Input,
    CodeSurface,
    Scrim,
    TextPrimary,
    TextMuted,
    TextLink,
    TextBody,
    TextDim,
    TextSubtle,
    TextSecondary,
    TextInverse,
    BorderSubtle,
    BorderStrong,
    BorderCode,
    BorderMedia,
    BorderFocus,
    BorderQuote,
    StateHover,
    StateRowHover,
    StatePressed,
    StateSelected,
    StateSelection,
    StateSearch,
    StateCurrentSearch,
    StateSuccess,
    StateWarning,
    StateDanger,
    StateDisabled,
    StateComposerSelection,
    StateCursorNormal,
    StateCursorInsert,
    StateInlineCode,
    ControlSurface,
    ControlSurfaceHover,
    ControlButton,
    ControlButtonHover,
    ControlActive,
    ControlActiveText,
    ScrollbarTrack,
    ScrollbarThumb,
    ScrollbarThumbHover,
    ParticipantLocal,
    ParticipantRemoteOne,
    ParticipantRemoteTwo,
    ParticipantRemoteThree,
    ParticipantRemoteFour,
    ParticipantIdentitySurface,
    ParticipantIdentityText,
    SyntaxForeground,
    SyntaxType,
    SyntaxFunction,
    SyntaxBinding,
    SyntaxNamespace,
    SyntaxKeyword,
    SyntaxString,
    SyntaxNumber,
    SyntaxComment,
    MediaViewport,
    MediaOverlay,
    MediaOverlayStrong,
    MediaBorder,
    MediaProgressTrack,
    MediaProgressFill,
    MediaProgressKnob,
    MediaGradientStart,
    MediaGradientEnd,
    MediaText,
    MediaMutedText,
}

pub(crate) const THEME_ROLE_COUNT: usize = 74;

#[derive(Clone, Copy, Debug)]
pub(crate) struct ThemeRoleSpec {
    pub(crate) role: ThemeRole,
    pub(crate) group: ThemeGroup,
    pub(crate) key: &'static str,
    pub(crate) label: &'static str,
}

pub(crate) static THEME_ROLES: &[ThemeRoleSpec] = &[
    ThemeRoleSpec {
        role: ThemeRole::Window,
        group: ThemeGroup::Surfaces,
        key: "window",
        label: "Window",
    },
    ThemeRoleSpec {
        role: ThemeRole::Sidebar,
        group: ThemeGroup::Surfaces,
        key: "sidebar",
        label: "Sidebar",
    },
    ThemeRoleSpec {
        role: ThemeRole::Raised,
        group: ThemeGroup::Surfaces,
        key: "raised",
        label: "Raised surface",
    },
    ThemeRoleSpec {
        role: ThemeRole::Media,
        group: ThemeGroup::Surfaces,
        key: "media",
        label: "Media surface",
    },
    ThemeRoleSpec {
        role: ThemeRole::Toolbar,
        group: ThemeGroup::Surfaces,
        key: "toolbar",
        label: "Toolbar",
    },
    ThemeRoleSpec {
        role: ThemeRole::Panel,
        group: ThemeGroup::Surfaces,
        key: "panel",
        label: "Panel",
    },
    ThemeRoleSpec {
        role: ThemeRole::Input,
        group: ThemeGroup::Surfaces,
        key: "input",
        label: "Input",
    },
    ThemeRoleSpec {
        role: ThemeRole::CodeSurface,
        group: ThemeGroup::Surfaces,
        key: "code",
        label: "Code surface",
    },
    ThemeRoleSpec {
        role: ThemeRole::Scrim,
        group: ThemeGroup::Surfaces,
        key: "scrim",
        label: "Modal scrim",
    },
    ThemeRoleSpec {
        role: ThemeRole::TextPrimary,
        group: ThemeGroup::Text,
        key: "primary",
        label: "Primary text",
    },
    ThemeRoleSpec {
        role: ThemeRole::TextMuted,
        group: ThemeGroup::Text,
        key: "muted",
        label: "Muted text",
    },
    ThemeRoleSpec {
        role: ThemeRole::TextLink,
        group: ThemeGroup::Text,
        key: "link",
        label: "Link text",
    },
    ThemeRoleSpec {
        role: ThemeRole::TextBody,
        group: ThemeGroup::Text,
        key: "body",
        label: "Message body",
    },
    ThemeRoleSpec {
        role: ThemeRole::TextDim,
        group: ThemeGroup::Text,
        key: "dim",
        label: "Dim text",
    },
    ThemeRoleSpec {
        role: ThemeRole::TextSubtle,
        group: ThemeGroup::Text,
        key: "subtle",
        label: "Subtle text",
    },
    ThemeRoleSpec {
        role: ThemeRole::TextSecondary,
        group: ThemeGroup::Text,
        key: "secondary",
        label: "Secondary text",
    },
    ThemeRoleSpec {
        role: ThemeRole::TextInverse,
        group: ThemeGroup::Text,
        key: "inverse",
        label: "Text on accent",
    },
    ThemeRoleSpec {
        role: ThemeRole::BorderSubtle,
        group: ThemeGroup::Borders,
        key: "subtle",
        label: "Subtle border",
    },
    ThemeRoleSpec {
        role: ThemeRole::BorderStrong,
        group: ThemeGroup::Borders,
        key: "strong",
        label: "Strong border",
    },
    ThemeRoleSpec {
        role: ThemeRole::BorderCode,
        group: ThemeGroup::Borders,
        key: "code",
        label: "Code border",
    },
    ThemeRoleSpec {
        role: ThemeRole::BorderMedia,
        group: ThemeGroup::Borders,
        key: "media",
        label: "Media border",
    },
    ThemeRoleSpec {
        role: ThemeRole::BorderFocus,
        group: ThemeGroup::Borders,
        key: "focus",
        label: "Focus border",
    },
    ThemeRoleSpec {
        role: ThemeRole::BorderQuote,
        group: ThemeGroup::Borders,
        key: "quote",
        label: "Quote rail",
    },
    ThemeRoleSpec {
        role: ThemeRole::StateHover,
        group: ThemeGroup::States,
        key: "hover",
        label: "Hover",
    },
    ThemeRoleSpec {
        role: ThemeRole::StateRowHover,
        group: ThemeGroup::States,
        key: "row-hover",
        label: "Row hover",
    },
    ThemeRoleSpec {
        role: ThemeRole::StatePressed,
        group: ThemeGroup::States,
        key: "pressed",
        label: "Pressed",
    },
    ThemeRoleSpec {
        role: ThemeRole::StateSelected,
        group: ThemeGroup::States,
        key: "selected",
        label: "Selected",
    },
    ThemeRoleSpec {
        role: ThemeRole::StateSelection,
        group: ThemeGroup::States,
        key: "selection",
        label: "Text selection",
    },
    ThemeRoleSpec {
        role: ThemeRole::StateSearch,
        group: ThemeGroup::States,
        key: "search",
        label: "Search match",
    },
    ThemeRoleSpec {
        role: ThemeRole::StateCurrentSearch,
        group: ThemeGroup::States,
        key: "current-search",
        label: "Current search match",
    },
    ThemeRoleSpec {
        role: ThemeRole::StateSuccess,
        group: ThemeGroup::States,
        key: "success",
        label: "Success",
    },
    ThemeRoleSpec {
        role: ThemeRole::StateWarning,
        group: ThemeGroup::States,
        key: "warning",
        label: "Warning",
    },
    ThemeRoleSpec {
        role: ThemeRole::StateDanger,
        group: ThemeGroup::States,
        key: "danger",
        label: "Danger",
    },
    ThemeRoleSpec {
        role: ThemeRole::StateDisabled,
        group: ThemeGroup::States,
        key: "disabled",
        label: "Disabled",
    },
    ThemeRoleSpec {
        role: ThemeRole::StateComposerSelection,
        group: ThemeGroup::States,
        key: "composer-selection",
        label: "Composer selection",
    },
    ThemeRoleSpec {
        role: ThemeRole::StateCursorNormal,
        group: ThemeGroup::States,
        key: "cursor-normal",
        label: "Normal-mode cursor",
    },
    ThemeRoleSpec {
        role: ThemeRole::StateCursorInsert,
        group: ThemeGroup::States,
        key: "cursor-insert",
        label: "Insert-mode cursor",
    },
    ThemeRoleSpec {
        role: ThemeRole::StateInlineCode,
        group: ThemeGroup::States,
        key: "inline-code",
        label: "Inline code background",
    },
    ThemeRoleSpec {
        role: ThemeRole::ControlSurface,
        group: ThemeGroup::Controls,
        key: "surface",
        label: "Control surface",
    },
    ThemeRoleSpec {
        role: ThemeRole::ControlSurfaceHover,
        group: ThemeGroup::Controls,
        key: "surface-hover",
        label: "Control hover",
    },
    ThemeRoleSpec {
        role: ThemeRole::ControlButton,
        group: ThemeGroup::Controls,
        key: "button",
        label: "Button",
    },
    ThemeRoleSpec {
        role: ThemeRole::ControlButtonHover,
        group: ThemeGroup::Controls,
        key: "button-hover",
        label: "Button hover",
    },
    ThemeRoleSpec {
        role: ThemeRole::ControlActive,
        group: ThemeGroup::Controls,
        key: "active",
        label: "Active control",
    },
    ThemeRoleSpec {
        role: ThemeRole::ControlActiveText,
        group: ThemeGroup::Controls,
        key: "active-text",
        label: "Active control text",
    },
    ThemeRoleSpec {
        role: ThemeRole::ScrollbarTrack,
        group: ThemeGroup::Scrollbar,
        key: "track",
        label: "Track",
    },
    ThemeRoleSpec {
        role: ThemeRole::ScrollbarThumb,
        group: ThemeGroup::Scrollbar,
        key: "thumb",
        label: "Thumb",
    },
    ThemeRoleSpec {
        role: ThemeRole::ScrollbarThumbHover,
        group: ThemeGroup::Scrollbar,
        key: "thumb-hover",
        label: "Hovered thumb",
    },
    ThemeRoleSpec {
        role: ThemeRole::ParticipantLocal,
        group: ThemeGroup::Participants,
        key: "local",
        label: "Local participant",
    },
    ThemeRoleSpec {
        role: ThemeRole::ParticipantRemoteOne,
        group: ThemeGroup::Participants,
        key: "remote-one",
        label: "Remote accent 1",
    },
    ThemeRoleSpec {
        role: ThemeRole::ParticipantRemoteTwo,
        group: ThemeGroup::Participants,
        key: "remote-two",
        label: "Remote accent 2",
    },
    ThemeRoleSpec {
        role: ThemeRole::ParticipantRemoteThree,
        group: ThemeGroup::Participants,
        key: "remote-three",
        label: "Remote accent 3",
    },
    ThemeRoleSpec {
        role: ThemeRole::ParticipantRemoteFour,
        group: ThemeGroup::Participants,
        key: "remote-four",
        label: "Remote accent 4",
    },
    ThemeRoleSpec {
        role: ThemeRole::ParticipantIdentitySurface,
        group: ThemeGroup::Participants,
        key: "identity-surface",
        label: "Identity surface",
    },
    ThemeRoleSpec {
        role: ThemeRole::ParticipantIdentityText,
        group: ThemeGroup::Participants,
        key: "identity-text",
        label: "Identity text",
    },
    ThemeRoleSpec {
        role: ThemeRole::SyntaxForeground,
        group: ThemeGroup::Syntax,
        key: "foreground",
        label: "Foreground",
    },
    ThemeRoleSpec {
        role: ThemeRole::SyntaxType,
        group: ThemeGroup::Syntax,
        key: "type",
        label: "Type",
    },
    ThemeRoleSpec {
        role: ThemeRole::SyntaxFunction,
        group: ThemeGroup::Syntax,
        key: "function",
        label: "Function",
    },
    ThemeRoleSpec {
        role: ThemeRole::SyntaxBinding,
        group: ThemeGroup::Syntax,
        key: "binding",
        label: "Binding",
    },
    ThemeRoleSpec {
        role: ThemeRole::SyntaxNamespace,
        group: ThemeGroup::Syntax,
        key: "namespace",
        label: "Namespace",
    },
    ThemeRoleSpec {
        role: ThemeRole::SyntaxKeyword,
        group: ThemeGroup::Syntax,
        key: "keyword",
        label: "Keyword",
    },
    ThemeRoleSpec {
        role: ThemeRole::SyntaxString,
        group: ThemeGroup::Syntax,
        key: "string",
        label: "String",
    },
    ThemeRoleSpec {
        role: ThemeRole::SyntaxNumber,
        group: ThemeGroup::Syntax,
        key: "number",
        label: "Number",
    },
    ThemeRoleSpec {
        role: ThemeRole::SyntaxComment,
        group: ThemeGroup::Syntax,
        key: "comment",
        label: "Comment",
    },
    ThemeRoleSpec {
        role: ThemeRole::MediaViewport,
        group: ThemeGroup::Media,
        key: "viewport",
        label: "Viewport",
    },
    ThemeRoleSpec {
        role: ThemeRole::MediaOverlay,
        group: ThemeGroup::Media,
        key: "overlay",
        label: "Overlay",
    },
    ThemeRoleSpec {
        role: ThemeRole::MediaOverlayStrong,
        group: ThemeGroup::Media,
        key: "overlay-strong",
        label: "Strong overlay",
    },
    ThemeRoleSpec {
        role: ThemeRole::MediaBorder,
        group: ThemeGroup::Media,
        key: "border",
        label: "Media border",
    },
    ThemeRoleSpec {
        role: ThemeRole::MediaProgressTrack,
        group: ThemeGroup::Media,
        key: "progress-track",
        label: "Progress track",
    },
    ThemeRoleSpec {
        role: ThemeRole::MediaProgressFill,
        group: ThemeGroup::Media,
        key: "progress-fill",
        label: "Progress fill",
    },
    ThemeRoleSpec {
        role: ThemeRole::MediaProgressKnob,
        group: ThemeGroup::Media,
        key: "progress-knob",
        label: "Progress knob",
    },
    ThemeRoleSpec {
        role: ThemeRole::MediaGradientStart,
        group: ThemeGroup::Media,
        key: "gradient-start",
        label: "Gradient start",
    },
    ThemeRoleSpec {
        role: ThemeRole::MediaGradientEnd,
        group: ThemeGroup::Media,
        key: "gradient-end",
        label: "Gradient end",
    },
    ThemeRoleSpec {
        role: ThemeRole::MediaText,
        group: ThemeGroup::Media,
        key: "text",
        label: "Media text",
    },
    ThemeRoleSpec {
        role: ThemeRole::MediaMutedText,
        group: ThemeGroup::Media,
        key: "muted-text",
        label: "Muted media text",
    },
];

impl ThemeConfig {
    pub(crate) fn color(&self, role: ThemeRole) -> Rgba8 {
        match role {
            ThemeRole::Window => self.surfaces.window,
            ThemeRole::Sidebar => self.surfaces.sidebar,
            ThemeRole::Raised => self.surfaces.raised,
            ThemeRole::Media => self.surfaces.media,
            ThemeRole::Toolbar => self.surfaces.toolbar,
            ThemeRole::Panel => self.surfaces.panel,
            ThemeRole::Input => self.surfaces.input,
            ThemeRole::CodeSurface => self.surfaces.code,
            ThemeRole::Scrim => self.surfaces.scrim,
            ThemeRole::TextPrimary => self.text.primary,
            ThemeRole::TextMuted => self.text.muted,
            ThemeRole::TextLink => self.text.link,
            ThemeRole::TextBody => self.text.body,
            ThemeRole::TextDim => self.text.dim,
            ThemeRole::TextSubtle => self.text.subtle,
            ThemeRole::TextSecondary => self.text.secondary,
            ThemeRole::TextInverse => self.text.inverse,
            ThemeRole::BorderSubtle => self.borders.subtle,
            ThemeRole::BorderStrong => self.borders.strong,
            ThemeRole::BorderCode => self.borders.code,
            ThemeRole::BorderMedia => self.borders.media,
            ThemeRole::BorderFocus => self.borders.focus,
            ThemeRole::BorderQuote => self.borders.quote,
            ThemeRole::StateHover => self.states.hover,
            ThemeRole::StateRowHover => self.states.row_hover,
            ThemeRole::StatePressed => self.states.pressed,
            ThemeRole::StateSelected => self.states.selected,
            ThemeRole::StateSelection => self.states.selection,
            ThemeRole::StateSearch => self.states.search,
            ThemeRole::StateCurrentSearch => self.states.current_search,
            ThemeRole::StateSuccess => self.states.success,
            ThemeRole::StateWarning => self.states.warning,
            ThemeRole::StateDanger => self.states.danger,
            ThemeRole::StateDisabled => self.states.disabled,
            ThemeRole::StateComposerSelection => self.states.composer_selection,
            ThemeRole::StateCursorNormal => self.states.cursor_normal,
            ThemeRole::StateCursorInsert => self.states.cursor_insert,
            ThemeRole::StateInlineCode => self.states.inline_code,
            ThemeRole::ControlSurface => self.controls.surface,
            ThemeRole::ControlSurfaceHover => self.controls.surface_hover,
            ThemeRole::ControlButton => self.controls.button,
            ThemeRole::ControlButtonHover => self.controls.button_hover,
            ThemeRole::ControlActive => self.controls.active,
            ThemeRole::ControlActiveText => self.controls.active_text,
            ThemeRole::ScrollbarTrack => self.scrollbar.track,
            ThemeRole::ScrollbarThumb => self.scrollbar.thumb,
            ThemeRole::ScrollbarThumbHover => self.scrollbar.thumb_hover,
            ThemeRole::ParticipantLocal => self.participants.local,
            ThemeRole::ParticipantRemoteOne => self.participants.remote_one,
            ThemeRole::ParticipantRemoteTwo => self.participants.remote_two,
            ThemeRole::ParticipantRemoteThree => self.participants.remote_three,
            ThemeRole::ParticipantRemoteFour => self.participants.remote_four,
            ThemeRole::ParticipantIdentitySurface => self.participants.identity_surface,
            ThemeRole::ParticipantIdentityText => self.participants.identity_text,
            ThemeRole::SyntaxForeground => self.syntax.foreground,
            ThemeRole::SyntaxType => self.syntax.r#type,
            ThemeRole::SyntaxFunction => self.syntax.function,
            ThemeRole::SyntaxBinding => self.syntax.binding,
            ThemeRole::SyntaxNamespace => self.syntax.namespace,
            ThemeRole::SyntaxKeyword => self.syntax.keyword,
            ThemeRole::SyntaxString => self.syntax.string,
            ThemeRole::SyntaxNumber => self.syntax.number,
            ThemeRole::SyntaxComment => self.syntax.comment,
            ThemeRole::MediaViewport => self.media.viewport,
            ThemeRole::MediaOverlay => self.media.overlay,
            ThemeRole::MediaOverlayStrong => self.media.overlay_strong,
            ThemeRole::MediaBorder => self.media.border,
            ThemeRole::MediaProgressTrack => self.media.progress_track,
            ThemeRole::MediaProgressFill => self.media.progress_fill,
            ThemeRole::MediaProgressKnob => self.media.progress_knob,
            ThemeRole::MediaGradientStart => self.media.gradient_start,
            ThemeRole::MediaGradientEnd => self.media.gradient_end,
            ThemeRole::MediaText => self.media.text,
            ThemeRole::MediaMutedText => self.media.muted_text,
        }
    }

    pub(crate) fn set_color(&mut self, role: ThemeRole, value: Rgba8) {
        match role {
            ThemeRole::Window => self.surfaces.window = value,
            ThemeRole::Sidebar => self.surfaces.sidebar = value,
            ThemeRole::Raised => self.surfaces.raised = value,
            ThemeRole::Media => self.surfaces.media = value,
            ThemeRole::Toolbar => self.surfaces.toolbar = value,
            ThemeRole::Panel => self.surfaces.panel = value,
            ThemeRole::Input => self.surfaces.input = value,
            ThemeRole::CodeSurface => self.surfaces.code = value,
            ThemeRole::Scrim => self.surfaces.scrim = value,
            ThemeRole::TextPrimary => self.text.primary = value,
            ThemeRole::TextMuted => self.text.muted = value,
            ThemeRole::TextLink => self.text.link = value,
            ThemeRole::TextBody => self.text.body = value,
            ThemeRole::TextDim => self.text.dim = value,
            ThemeRole::TextSubtle => self.text.subtle = value,
            ThemeRole::TextSecondary => self.text.secondary = value,
            ThemeRole::TextInverse => self.text.inverse = value,
            ThemeRole::BorderSubtle => self.borders.subtle = value,
            ThemeRole::BorderStrong => self.borders.strong = value,
            ThemeRole::BorderCode => self.borders.code = value,
            ThemeRole::BorderMedia => self.borders.media = value,
            ThemeRole::BorderFocus => self.borders.focus = value,
            ThemeRole::BorderQuote => self.borders.quote = value,
            ThemeRole::StateHover => self.states.hover = value,
            ThemeRole::StateRowHover => self.states.row_hover = value,
            ThemeRole::StatePressed => self.states.pressed = value,
            ThemeRole::StateSelected => self.states.selected = value,
            ThemeRole::StateSelection => self.states.selection = value,
            ThemeRole::StateSearch => self.states.search = value,
            ThemeRole::StateCurrentSearch => self.states.current_search = value,
            ThemeRole::StateSuccess => self.states.success = value,
            ThemeRole::StateWarning => self.states.warning = value,
            ThemeRole::StateDanger => self.states.danger = value,
            ThemeRole::StateDisabled => self.states.disabled = value,
            ThemeRole::StateComposerSelection => self.states.composer_selection = value,
            ThemeRole::StateCursorNormal => self.states.cursor_normal = value,
            ThemeRole::StateCursorInsert => self.states.cursor_insert = value,
            ThemeRole::StateInlineCode => self.states.inline_code = value,
            ThemeRole::ControlSurface => self.controls.surface = value,
            ThemeRole::ControlSurfaceHover => self.controls.surface_hover = value,
            ThemeRole::ControlButton => self.controls.button = value,
            ThemeRole::ControlButtonHover => self.controls.button_hover = value,
            ThemeRole::ControlActive => self.controls.active = value,
            ThemeRole::ControlActiveText => self.controls.active_text = value,
            ThemeRole::ScrollbarTrack => self.scrollbar.track = value,
            ThemeRole::ScrollbarThumb => self.scrollbar.thumb = value,
            ThemeRole::ScrollbarThumbHover => self.scrollbar.thumb_hover = value,
            ThemeRole::ParticipantLocal => self.participants.local = value,
            ThemeRole::ParticipantRemoteOne => self.participants.remote_one = value,
            ThemeRole::ParticipantRemoteTwo => self.participants.remote_two = value,
            ThemeRole::ParticipantRemoteThree => self.participants.remote_three = value,
            ThemeRole::ParticipantRemoteFour => self.participants.remote_four = value,
            ThemeRole::ParticipantIdentitySurface => self.participants.identity_surface = value,
            ThemeRole::ParticipantIdentityText => self.participants.identity_text = value,
            ThemeRole::SyntaxForeground => self.syntax.foreground = value,
            ThemeRole::SyntaxType => self.syntax.r#type = value,
            ThemeRole::SyntaxFunction => self.syntax.function = value,
            ThemeRole::SyntaxBinding => self.syntax.binding = value,
            ThemeRole::SyntaxNamespace => self.syntax.namespace = value,
            ThemeRole::SyntaxKeyword => self.syntax.keyword = value,
            ThemeRole::SyntaxString => self.syntax.string = value,
            ThemeRole::SyntaxNumber => self.syntax.number = value,
            ThemeRole::SyntaxComment => self.syntax.comment = value,
            ThemeRole::MediaViewport => self.media.viewport = value,
            ThemeRole::MediaOverlay => self.media.overlay = value,
            ThemeRole::MediaOverlayStrong => self.media.overlay_strong = value,
            ThemeRole::MediaBorder => self.media.border = value,
            ThemeRole::MediaProgressTrack => self.media.progress_track = value,
            ThemeRole::MediaProgressFill => self.media.progress_fill = value,
            ThemeRole::MediaProgressKnob => self.media.progress_knob = value,
            ThemeRole::MediaGradientStart => self.media.gradient_start = value,
            ThemeRole::MediaGradientEnd => self.media.gradient_end = value,
            ThemeRole::MediaText => self.media.text = value,
            ThemeRole::MediaMutedText => self.media.muted_text = value,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum FontRole {
    Interface,
    Message,
    Code,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct FontRoleSpec {
    pub(crate) role: FontRole,
    pub(crate) key_stem: &'static str,
    pub(crate) label: &'static str,
    pub(crate) sample: &'static str,
}

pub(crate) static FONT_ROLES: &[FontRoleSpec] = &[
    FontRoleSpec {
        role: FontRole::Interface,
        key_stem: "interface",
        label: "Interface font family",
        sample: "Rooms, buttons, and labels",
    },
    FontRoleSpec {
        role: FontRole::Message,
        key_stem: "message",
        label: "Message font family",
        sample: "The quick brown fox jumps over the lazy dog.",
    },
    FontRoleSpec {
        role: FontRole::Code,
        key_stem: "code",
        label: "Code font family",
        sample: "fn main() { println!(\"hello\"); }",
    },
];

impl FontConfig {
    pub(crate) fn family(&self, role: FontRole) -> &str {
        match role {
            FontRole::Interface => &self.interface_family,
            FontRole::Message => &self.message_family,
            FontRole::Code => &self.code_family,
        }
    }

    pub(crate) fn set_family(&mut self, role: FontRole, family: String) {
        match role {
            FontRole::Interface => self.interface_family = family,
            FontRole::Message => self.message_family = family,
            FontRole::Code => self.code_family = family,
        }
    }

    pub(crate) fn size(&self, role: FontRole) -> f32 {
        match role {
            FontRole::Interface => self.interface_size,
            FontRole::Message => self.message_size,
            FontRole::Code => self.code_size,
        }
    }

    pub(crate) fn set_size(&mut self, role: FontRole, size: f32) {
        match role {
            FontRole::Interface => self.interface_size = size,
            FontRole::Message => self.message_size = size,
            FontRole::Code => self.code_size = size,
        }
    }
}

#[derive(Clone)]
pub(crate) struct ThemePalette([Rgba; THEME_ROLE_COUNT]);

impl ThemePalette {
    pub(crate) fn from_config(config: &ThemeConfig) -> Self {
        let mut colors = [Rgba::default(); THEME_ROLE_COUNT];
        for spec in THEME_ROLES {
            colors[spec.role as usize] = rgba(config.color(spec.role).packed());
        }
        Self(colors)
    }

    #[inline]
    pub(crate) fn color(&self, role: ThemeRole) -> Rgba {
        self.0[role as usize]
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ResolvedFonts {
    pub(crate) interface_family: SharedString,
    pub(crate) message_family: SharedString,
    pub(crate) code_family: SharedString,
    pub(crate) interface_size: f32,
    pub(crate) message_size: f32,
    pub(crate) code_size: f32,
}

#[derive(Clone)]
pub(crate) struct ResolvedSettings {
    pub(crate) revision: u64,
    pub(crate) typography_revision: u64,
    pub(crate) source_status: SourceStatus,
    pub(crate) theme: ThemePalette,
    pub(crate) fonts: ResolvedFonts,
    pub(crate) binding_mode: BindingMode,
    pub(crate) rendering: FontRendering,
    pub(crate) diagnostics: Arc<[ConfigDiagnostic]>,
}

#[derive(Clone)]
pub(crate) struct AppliedSettings(pub(crate) Arc<ResolvedSettings>);

impl Global for AppliedSettings {}

impl AppliedSettings {
    pub(crate) fn get(cx: &App) -> Arc<ResolvedSettings> {
        cx.global::<Self>().0.clone()
    }
}

pub(crate) fn resolve(
    config: &GuiConfig,
    status: SourceStatus,
    diagnostics: &[ConfigDiagnostic],
    available_families: &[String],
    previous: Option<&ResolvedSettings>,
) -> ResolvedSettings {
    let contains = |family: &str| {
        family == DEFAULT_INTERFACE_FAMILY
            || available_families
                .iter()
                .any(|candidate| candidate == family)
    };
    let family = |configured: &str, fallback: &'static str| -> SharedString {
        if contains(configured) {
            configured.to_owned().into()
        } else {
            fallback.into()
        }
    };
    let fonts = ResolvedFonts {
        interface_family: family(&config.fonts.interface_family, DEFAULT_INTERFACE_FAMILY),
        message_family: family(&config.fonts.message_family, DEFAULT_MESSAGE_FAMILY),
        code_family: family(&config.fonts.code_family, DEFAULT_CODE_FAMILY),
        interface_size: config.fonts.interface_size,
        message_size: config.fonts.message_size,
        code_size: config.fonts.code_size,
    };
    let typography_changed = previous.is_none_or(|previous| previous.fonts != fonts);
    ResolvedSettings {
        revision: previous.map_or(1, |previous| previous.revision.saturating_add(1)),
        typography_revision: previous.map_or(1, |previous| {
            previous
                .typography_revision
                .saturating_add(u64::from(typography_changed))
        }),
        source_status: status,
        theme: ThemePalette::from_config(&config.theme),
        fonts,
        binding_mode: config.input.default_binding_mode,
        rendering: config.fonts.rendering,
        diagnostics: diagnostics.to_vec().into(),
    }
}

pub(crate) fn font_warnings(
    config: &GuiConfig,
    available_families: &[String],
) -> Vec<ConfigDiagnostic> {
    let mut warnings = Vec::new();
    for (path, configured, fallback) in [
        (
            "fonts.interface-family",
            config.fonts.interface_family.as_str(),
            DEFAULT_INTERFACE_FAMILY,
        ),
        (
            "fonts.message-family",
            config.fonts.message_family.as_str(),
            DEFAULT_MESSAGE_FAMILY,
        ),
        (
            "fonts.code-family",
            config.fonts.code_family.as_str(),
            DEFAULT_CODE_FAMILY,
        ),
    ] {
        if configured != DEFAULT_INTERFACE_FAMILY
            && !available_families
                .iter()
                .any(|candidate| candidate == configured)
        {
            warnings.push(ConfigDiagnostic::warning(
                path,
                format!(
                    "font family `{configured}` is unavailable; using `{fallback}` for this session"
                ),
            ));
        }
    }
    warnings
}

pub(crate) fn rendering_mode(mode: FontRendering) -> TextRenderingMode {
    match mode {
        FontRendering::PlatformDefault => TextRenderingMode::PlatformDefault,
        FontRendering::Subpixel => TextRenderingMode::Subpixel,
        FontRendering::Grayscale => TextRenderingMode::Grayscale,
    }
}

pub(crate) fn apply_appearance(
    config: &GuiConfig,
    status: SourceStatus,
    diagnostics: &[ConfigDiagnostic],
    available_families: &[String],
    cx: &mut App,
) -> Arc<ResolvedSettings> {
    let previous = cx
        .try_global::<AppliedSettings>()
        .map(|settings| settings.0.clone());
    let resolved = Arc::new(resolve(
        config,
        status,
        diagnostics,
        available_families,
        previous.as_deref(),
    ));
    cx.set_text_rendering_mode(rendering_mode(resolved.rendering));
    cx.set_global(AppliedSettings(resolved.clone()));
    crate::ui_scale::configured_interface_size_changed(resolved.fonts.interface_size, cx);
    cx.refresh_windows();
    resolved
}

pub(crate) fn syntax_role(role: PaletteRole) -> ThemeRole {
    match role {
        PaletteRole::Foreground => ThemeRole::SyntaxForeground,
        PaletteRole::Type => ThemeRole::SyntaxType,
        PaletteRole::Function => ThemeRole::SyntaxFunction,
        PaletteRole::Binding => ThemeRole::SyntaxBinding,
        PaletteRole::Namespace => ThemeRole::SyntaxNamespace,
        PaletteRole::Keyword => ThemeRole::SyntaxKeyword,
        PaletteRole::String => ThemeRole::SyntaxString,
        PaletteRole::Number => ThemeRole::SyntaxNumber,
        PaletteRole::Comment => ThemeRole::SyntaxComment,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_indexes_match_role_discriminants() {
        assert_eq!(THEME_ROLES.len(), THEME_ROLE_COUNT);
        for (index, spec) in THEME_ROLES.iter().enumerate() {
            assert_eq!(spec.role as usize, index);
        }
    }

    #[test]
    fn every_theme_role_reads_and_resets_through_typed_accessors() {
        let defaults = ThemeConfig::default();
        let mut edited = defaults.clone();
        for spec in THEME_ROLES {
            edited.set_color(spec.role, Rgba8::rgb(1, 2, 3));
            assert_eq!(edited.color(spec.role), Rgba8::rgb(1, 2, 3));
            edited.set_color(spec.role, defaults.color(spec.role));
        }
        assert_eq!(edited, defaults);
    }

    #[test]
    fn rendering_modes_map_exactly() {
        assert_eq!(
            rendering_mode(FontRendering::PlatformDefault),
            TextRenderingMode::PlatformDefault
        );
        assert_eq!(
            rendering_mode(FontRendering::Subpixel),
            TextRenderingMode::Subpixel
        );
        assert_eq!(
            rendering_mode(FontRendering::Grayscale),
            TextRenderingMode::Grayscale
        );
    }
}
