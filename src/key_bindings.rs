use std::{
    collections::{BTreeMap, HashMap},
    rc::Rc,
};

use gpui::{Action, App, KeyBinding, KeyBindingContextPredicate};

use crate::{
    app::{
        CloseCodeSearch, ClosePreview, CloseServerSelector, CompletionAccept,
        CompletionAcceptEngaged, CompletionDismiss, CompletionNext, CompletionPrevious, FindInCode,
        LivePanDown, LivePanUp, LiveReset, LiveZoomIn, LiveZoomOut, NextCodeMatch, OpenMedia,
        OpenSettings, PreviousCodeMatch, SeekBack, SeekForward, SendMessage, ServerActivate,
        ServerNext, ServerPrevious, ToggleDeafen, ToggleMute, TogglePlayback, ToggleVoice,
    },
    code_viewer, composer,
    config::{
        schema::{BindCommand, BindingsConfig, GuiConfig},
        validation::ConfigDiagnostic,
    },
    formatted_message,
    ui_scale::{DecreaseUiScale, IncreaseUiScale, ResetUiScale},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub(crate) enum BindingScope {
    Application,
    Composer,
    Completion,
    Vim,
    CodeSearch,
    CodeViewer,
    FormattedMessage,
    NonInput,
}

impl BindingScope {
    pub(crate) const ALL: [Self; 8] = [
        Self::Application,
        Self::Composer,
        Self::Completion,
        Self::Vim,
        Self::CodeSearch,
        Self::CodeViewer,
        Self::FormattedMessage,
        Self::NonInput,
    ];

    pub(crate) const fn key(self) -> &'static str {
        match self {
            Self::Application => "application",
            Self::Composer => "composer",
            Self::Completion => "completion",
            Self::Vim => "vim",
            Self::CodeSearch => "code-search",
            Self::CodeViewer => "code-viewer",
            Self::FormattedMessage => "formatted-message",
            Self::NonInput => "non-input",
        }
    }

    pub(crate) const fn title(self) -> &'static str {
        match self {
            Self::Application => "Application",
            Self::Composer => "Composer",
            Self::Completion => "Completion",
            Self::Vim => "Vim",
            Self::CodeSearch => "Code search",
            Self::CodeViewer => "Code viewer",
            Self::FormattedMessage => "Formatted message",
            Self::NonInput => "Non-input",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct BindingSpec {
    pub(crate) scope: BindingScope,
    pub(crate) command: BindCommand,
    pub(crate) label: &'static str,
    pub(crate) help: Option<&'static str>,
    pub(crate) contexts: &'static [&'static str],
    pub(crate) defaults: &'static [&'static str],
}

const CHAT: &str = "Chatt && !ChattSettings";
const NON_INPUT: &str = "Chatt && !ChattSettings && !ChattComposer && !ChattCodeSearch";
const CODE_VIEWER: &str = "ChattCodeViewer && !ChattCodeSearch && !ChattSettings";
const CODE_SEARCH: &str = "ChattCodeSearch && !ChattSettings";
const COMPOSER_INSERT: &str = "ComposerInsert && !ChattSettings";
const CHAT_COMPOSER_INSERT: &str =
    "ChattComposer && ComposerInsert && !CompletionEngaged && !ChattSettings";
const COMPLETION_OPEN: &str = "ChattComposer && ComposerInsert && CompletionOpen && !ChattSettings";
const COMPLETION_ENGAGED: &str =
    "ChattComposer && ComposerInsert && CompletionEngaged && !ChattSettings";
const UI_SCALE_CONTEXT: &str = "Chatt";

#[cfg(target_os = "windows")]
const UI_SCALE_IN_DEFAULTS: &[&str] = &["ctrl-=", "ctrl-shift-="];
#[cfg(not(target_os = "windows"))]
const UI_SCALE_IN_DEFAULTS: &[&str] = &["ctrl-=", "ctrl-+"];

pub(crate) static BINDINGS: &[BindingSpec] = &[
    BindingSpec {
        scope: BindingScope::Application,
        command: BindCommand::OpenMedia,
        label: "Open media",
        help: None,
        contexts: &[CHAT],
        defaults: &["cmd-o"],
    },
    BindingSpec {
        scope: BindingScope::Application,
        command: BindCommand::OpenSettings,
        label: "Open Settings",
        help: Some("The sidebar gear remains available if this is unbound."),
        contexts: &[CHAT],
        defaults: &["secondary-,"],
    },
    BindingSpec {
        scope: BindingScope::Application,
        command: BindCommand::IncreaseUiScale,
        label: "Increase UI scale",
        help: Some("Temporarily scales the entire interface for this session."),
        contexts: &[UI_SCALE_CONTEXT],
        defaults: UI_SCALE_IN_DEFAULTS,
    },
    BindingSpec {
        scope: BindingScope::Application,
        command: BindCommand::DecreaseUiScale,
        label: "Decrease UI scale",
        help: Some("Temporarily scales the entire interface for this session."),
        contexts: &[UI_SCALE_CONTEXT],
        defaults: &["ctrl--"],
    },
    BindingSpec {
        scope: BindingScope::Application,
        command: BindCommand::ResetUiScale,
        label: "Reset UI scale",
        help: Some("Restores the interface scale configured in GUI settings."),
        contexts: &[UI_SCALE_CONTEXT],
        defaults: &["ctrl-0"],
    },
    BindingSpec {
        scope: BindingScope::Composer,
        command: BindCommand::SendMessage,
        label: "Send message",
        help: None,
        contexts: &[CHAT_COMPOSER_INSERT],
        defaults: &["enter"],
    },
    BindingSpec {
        scope: BindingScope::Composer,
        command: BindCommand::Newline,
        label: "Insert newline",
        help: None,
        contexts: &[COMPOSER_INSERT],
        defaults: &["shift-enter"],
    },
    BindingSpec {
        scope: BindingScope::Composer,
        command: BindCommand::Backspace,
        label: "Backspace",
        help: None,
        contexts: &[COMPOSER_INSERT],
        defaults: &["backspace"],
    },
    BindingSpec {
        scope: BindingScope::Composer,
        command: BindCommand::Delete,
        label: "Delete",
        help: None,
        contexts: &[COMPOSER_INSERT],
        defaults: &["delete"],
    },
    BindingSpec {
        scope: BindingScope::Composer,
        command: BindCommand::Left,
        label: "Move left",
        help: None,
        contexts: &[COMPOSER_INSERT],
        defaults: &["left"],
    },
    BindingSpec {
        scope: BindingScope::Composer,
        command: BindCommand::Right,
        label: "Move right",
        help: None,
        contexts: &[COMPOSER_INSERT],
        defaults: &["right"],
    },
    BindingSpec {
        scope: BindingScope::Composer,
        command: BindCommand::SelectLeft,
        label: "Select left",
        help: None,
        contexts: &[COMPOSER_INSERT],
        defaults: &["shift-left"],
    },
    BindingSpec {
        scope: BindingScope::Composer,
        command: BindCommand::SelectRight,
        label: "Select right",
        help: None,
        contexts: &[COMPOSER_INSERT],
        defaults: &["shift-right"],
    },
    BindingSpec {
        scope: BindingScope::Composer,
        command: BindCommand::SelectAll,
        label: "Select all",
        help: None,
        contexts: &[COMPOSER_INSERT],
        defaults: &["cmd-a"],
    },
    BindingSpec {
        scope: BindingScope::Composer,
        command: BindCommand::Paste,
        label: "Paste",
        help: None,
        contexts: &[COMPOSER_INSERT],
        defaults: &["secondary-v"],
    },
    BindingSpec {
        scope: BindingScope::Composer,
        command: BindCommand::Copy,
        label: "Copy",
        help: None,
        contexts: &[COMPOSER_INSERT],
        defaults: &["cmd-c"],
    },
    BindingSpec {
        scope: BindingScope::Composer,
        command: BindCommand::Cut,
        label: "Cut",
        help: None,
        contexts: &[COMPOSER_INSERT],
        defaults: &["cmd-x"],
    },
    BindingSpec {
        scope: BindingScope::Composer,
        command: BindCommand::InsertTab,
        label: "Insert tab",
        help: None,
        contexts: &["ComposerInsert && !CompletionOpen && !ChattSettings"],
        defaults: &["tab"],
    },
    BindingSpec {
        scope: BindingScope::Completion,
        command: BindCommand::CompletionNext,
        label: "Next completion",
        help: None,
        contexts: &[COMPLETION_OPEN],
        defaults: &["down"],
    },
    BindingSpec {
        scope: BindingScope::Completion,
        command: BindCommand::CompletionPrevious,
        label: "Previous completion",
        help: None,
        contexts: &[COMPLETION_OPEN],
        defaults: &["up"],
    },
    BindingSpec {
        scope: BindingScope::Completion,
        command: BindCommand::CompletionAccept,
        label: "Accept completion",
        help: None,
        contexts: &[COMPLETION_OPEN],
        defaults: &["tab"],
    },
    BindingSpec {
        scope: BindingScope::Completion,
        command: BindCommand::CompletionAcceptEngaged,
        label: "Accept engaged completion",
        help: None,
        contexts: &[COMPLETION_ENGAGED],
        defaults: &["enter"],
    },
    BindingSpec {
        scope: BindingScope::Completion,
        command: BindCommand::CompletionDismiss,
        label: "Dismiss completion",
        help: None,
        contexts: &[COMPLETION_OPEN],
        defaults: &["escape"],
    },
    BindingSpec {
        scope: BindingScope::Vim,
        command: BindCommand::Paste,
        label: "Paste in Vim mode",
        help: None,
        contexts: &["VimMode && !ChattSettings"],
        defaults: &["secondary-v"],
    },
    BindingSpec {
        scope: BindingScope::CodeSearch,
        command: BindCommand::Backspace,
        label: "Search backspace",
        help: None,
        contexts: &[CODE_SEARCH],
        defaults: &["backspace"],
    },
    BindingSpec {
        scope: BindingScope::CodeSearch,
        command: BindCommand::Delete,
        label: "Search delete",
        help: None,
        contexts: &[CODE_SEARCH],
        defaults: &["delete"],
    },
    BindingSpec {
        scope: BindingScope::CodeSearch,
        command: BindCommand::Left,
        label: "Search move left",
        help: None,
        contexts: &[CODE_SEARCH],
        defaults: &["left"],
    },
    BindingSpec {
        scope: BindingScope::CodeSearch,
        command: BindCommand::Right,
        label: "Search move right",
        help: None,
        contexts: &[CODE_SEARCH],
        defaults: &["right"],
    },
    BindingSpec {
        scope: BindingScope::CodeSearch,
        command: BindCommand::SelectLeft,
        label: "Search select left",
        help: None,
        contexts: &[CODE_SEARCH],
        defaults: &["shift-left"],
    },
    BindingSpec {
        scope: BindingScope::CodeSearch,
        command: BindCommand::SelectRight,
        label: "Search select right",
        help: None,
        contexts: &[CODE_SEARCH],
        defaults: &["shift-right"],
    },
    BindingSpec {
        scope: BindingScope::CodeSearch,
        command: BindCommand::SelectAll,
        label: "Select all search text",
        help: None,
        contexts: &[CODE_SEARCH],
        defaults: &["cmd-a"],
    },
    BindingSpec {
        scope: BindingScope::CodeSearch,
        command: BindCommand::Paste,
        label: "Paste search text",
        help: None,
        contexts: &[CODE_SEARCH],
        defaults: &["secondary-v"],
    },
    BindingSpec {
        scope: BindingScope::CodeSearch,
        command: BindCommand::Copy,
        label: "Copy search text",
        help: None,
        contexts: &[CODE_SEARCH],
        defaults: &["cmd-c"],
    },
    BindingSpec {
        scope: BindingScope::CodeSearch,
        command: BindCommand::Cut,
        label: "Cut search text",
        help: None,
        contexts: &[CODE_SEARCH],
        defaults: &["cmd-x"],
    },
    BindingSpec {
        scope: BindingScope::CodeSearch,
        command: BindCommand::NextCodeMatch,
        label: "Next match",
        help: None,
        contexts: &[CODE_SEARCH],
        defaults: &["enter"],
    },
    BindingSpec {
        scope: BindingScope::CodeSearch,
        command: BindCommand::PreviousCodeMatch,
        label: "Previous match",
        help: None,
        contexts: &[CODE_SEARCH],
        defaults: &["shift-enter"],
    },
    BindingSpec {
        scope: BindingScope::CodeSearch,
        command: BindCommand::CloseCodeSearch,
        label: "Close search",
        help: None,
        contexts: &[CODE_SEARCH],
        defaults: &["escape"],
    },
    BindingSpec {
        scope: BindingScope::CodeViewer,
        command: BindCommand::Copy,
        label: "Copy selection",
        help: None,
        contexts: &[CODE_VIEWER],
        defaults: &["cmd-c"],
    },
    BindingSpec {
        scope: BindingScope::CodeViewer,
        command: BindCommand::SelectAll,
        label: "Select all",
        help: None,
        contexts: &[CODE_VIEWER],
        defaults: &["cmd-a"],
    },
    BindingSpec {
        scope: BindingScope::CodeViewer,
        command: BindCommand::FindInCode,
        label: "Find in code",
        help: None,
        contexts: &[CODE_VIEWER],
        defaults: &["cmd-f"],
    },
    BindingSpec {
        scope: BindingScope::FormattedMessage,
        command: BindCommand::Copy,
        label: "Copy message text",
        help: None,
        contexts: &["ChattFormattedText && !ChattSettings"],
        defaults: &["secondary-c", "y"],
    },
    BindingSpec {
        scope: BindingScope::NonInput,
        command: BindCommand::ToggleMute,
        label: "Toggle mute",
        help: None,
        contexts: &[NON_INPUT],
        defaults: &[],
    },
    BindingSpec {
        scope: BindingScope::NonInput,
        command: BindCommand::ToggleDeafen,
        label: "Toggle deafen",
        help: None,
        contexts: &[NON_INPUT],
        defaults: &[],
    },
    BindingSpec {
        scope: BindingScope::NonInput,
        command: BindCommand::ToggleVoice,
        label: "Join or leave voice",
        help: None,
        contexts: &[NON_INPUT],
        defaults: &[],
    },
    BindingSpec {
        scope: BindingScope::NonInput,
        command: BindCommand::ClosePreview,
        label: "Close preview",
        help: None,
        contexts: &[NON_INPUT],
        defaults: &["escape"],
    },
    BindingSpec {
        scope: BindingScope::NonInput,
        command: BindCommand::TogglePlayback,
        label: "Play or pause",
        help: None,
        contexts: &[NON_INPUT],
        defaults: &["space"],
    },
    BindingSpec {
        scope: BindingScope::NonInput,
        command: BindCommand::SeekBack,
        label: "Seek backward",
        help: None,
        contexts: &[NON_INPUT],
        defaults: &["left"],
    },
    BindingSpec {
        scope: BindingScope::NonInput,
        command: BindCommand::SeekForward,
        label: "Seek forward",
        help: None,
        contexts: &[NON_INPUT],
        defaults: &["right"],
    },
    BindingSpec {
        scope: BindingScope::NonInput,
        command: BindCommand::LiveZoomIn,
        label: "Zoom in",
        help: None,
        contexts: &[NON_INPUT],
        defaults: &["="],
    },
    BindingSpec {
        scope: BindingScope::NonInput,
        command: BindCommand::LiveZoomOut,
        label: "Zoom out",
        help: None,
        contexts: &[NON_INPUT],
        defaults: &["-"],
    },
    BindingSpec {
        scope: BindingScope::NonInput,
        command: BindCommand::LiveReset,
        label: "Reset view",
        help: None,
        contexts: &[NON_INPUT],
        defaults: &["home"],
    },
    BindingSpec {
        scope: BindingScope::NonInput,
        command: BindCommand::LivePanUp,
        label: "Pan up",
        help: None,
        contexts: &[NON_INPUT],
        defaults: &["up"],
    },
    BindingSpec {
        scope: BindingScope::NonInput,
        command: BindCommand::LivePanDown,
        label: "Pan down",
        help: None,
        contexts: &[NON_INPUT],
        defaults: &["down"],
    },
];

fn table(config: &BindingsConfig, scope: BindingScope) -> &BTreeMap<String, BindCommand> {
    match scope {
        BindingScope::Application => &config.application,
        BindingScope::Composer => &config.composer,
        BindingScope::Completion => &config.completion,
        BindingScope::Vim => &config.vim,
        BindingScope::CodeSearch => &config.code_search,
        BindingScope::CodeViewer => &config.code_viewer,
        BindingScope::FormattedMessage => &config.formatted_message,
        BindingScope::NonInput => &config.non_input,
    }
}

pub(crate) fn table_mut(
    config: &mut BindingsConfig,
    scope: BindingScope,
) -> &mut BTreeMap<String, BindCommand> {
    match scope {
        BindingScope::Application => &mut config.application,
        BindingScope::Composer => &mut config.composer,
        BindingScope::Completion => &mut config.completion,
        BindingScope::Vim => &mut config.vim,
        BindingScope::CodeSearch => &mut config.code_search,
        BindingScope::CodeViewer => &mut config.code_viewer,
        BindingScope::FormattedMessage => &mut config.formatted_message,
        BindingScope::NonInput => &mut config.non_input,
    }
}

pub(crate) fn spec(scope: BindingScope, command: BindCommand) -> Option<&'static BindingSpec> {
    BINDINGS
        .iter()
        .find(|candidate| candidate.scope == scope && candidate.command == command)
}

pub(crate) fn effective_scope(
    bindings: &BindingsConfig,
    scope: BindingScope,
) -> Vec<(String, BindCommand)> {
    let overrides = table(bindings, scope);
    let mut effective = Vec::new();
    for binding in BINDINGS.iter().filter(|binding| binding.scope == scope) {
        for sequence in binding.defaults {
            if !overrides.contains_key(*sequence) {
                effective.push(((*sequence).to_string(), binding.command));
            }
        }
    }
    for (sequence, command) in overrides {
        if *command != BindCommand::Unbind {
            effective.push((sequence.clone(), *command));
        }
    }
    effective.sort_by(|left, right| {
        left.0
            .cmp(&right.0)
            .then_with(|| format!("{:?}", left.1).cmp(&format!("{:?}", right.1)))
    });
    effective
}

pub(crate) fn effective_sequences(
    config: &GuiConfig,
    scope: BindingScope,
    command: BindCommand,
) -> Vec<String> {
    effective_scope(&config.bindings, scope)
        .into_iter()
        .filter_map(|(sequence, candidate)| (candidate == command).then_some(sequence))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect()
}

pub(crate) fn set_sequences(
    config: &mut GuiConfig,
    scope: BindingScope,
    command: BindCommand,
    sequences: &[String],
) {
    let binding = spec(scope, command).expect("catalog binding has a registry entry");
    let table = table_mut(&mut config.bindings, scope);
    table.retain(|_, candidate| *candidate != command);
    for inherited in binding.defaults {
        if !sequences.iter().any(|sequence| sequence == inherited) {
            table.insert((*inherited).to_string(), BindCommand::Unbind);
        } else {
            table.remove(*inherited);
        }
    }
    for sequence in sequences {
        if !binding.defaults.contains(&sequence.as_str()) {
            table.insert(sequence.clone(), command);
        }
    }
}

pub(crate) fn validate(config: &GuiConfig) -> Vec<ConfigDiagnostic> {
    let mut diagnostics = Vec::new();
    for scope in BindingScope::ALL {
        for (sequence, command) in table(&config.bindings, scope) {
            if *command != BindCommand::Unbind && spec(scope, *command).is_none() {
                diagnostics.push(ConfigDiagnostic::error(
                    format!("bindings.{}.{}", scope.key(), sequence),
                    format!(
                        "command {command:?} is not valid in the {} scope",
                        scope.title()
                    ),
                ));
            }
        }
        let mut canonical_sequences = HashMap::<String, String>::new();
        for (sequence, _) in effective_scope(&config.bindings, scope) {
            if let Ok(canonical) = canonical_sequence(&sequence) {
                if let Some(previous) =
                    canonical_sequences.insert(canonical.clone(), sequence.clone())
                    && previous != sequence
                {
                    diagnostics.push(ConfigDiagnostic::error(
                        format!("bindings.{}.{}", scope.key(), sequence),
                        format!(
                            "`{sequence}` is the same canonical binding as `{previous}` ({canonical})"
                        ),
                    ));
                }
            }
        }
    }
    diagnostics
}

fn canonical_sequence(sequence: &str) -> Result<String, String> {
    sequence
        .split_whitespace()
        .map(|chord| {
            gpui::Keystroke::parse(chord)
                .map(|keystroke| keystroke.unparse())
                .map_err(|error| error.to_string())
        })
        .collect::<Result<Vec<_>, _>>()
        .map(|chords| chords.join(" "))
}

pub(crate) fn compile(config: &GuiConfig, cx: &App) -> Result<Vec<KeyBinding>, String> {
    let diagnostics = validate(config);
    if let Some(error) = diagnostics.first() {
        return Err(format!("{}: {}", error.path, error.message));
    }
    let mut compiled = Vec::new();
    for scope in BindingScope::ALL {
        for (sequence, command) in effective_scope(&config.bindings, scope) {
            let binding = spec(scope, command)
                .ok_or_else(|| format!("{command:?} is not valid in {}", scope.title()))?;
            for context in binding.contexts {
                let predicate = KeyBindingContextPredicate::parse(context)
                    .map_err(|error| format!("invalid built-in context `{context}`: {error}"))?;
                compiled.push(
                    KeyBinding::load(
                        &sequence,
                        make_action(scope, command),
                        Some(Rc::new(predicate)),
                        false,
                        None,
                        cx.keyboard_mapper().as_ref(),
                    )
                    .map_err(|error| format!("invalid binding `{sequence}`: {error}"))?,
                );
            }
        }
    }
    Ok(compiled)
}

pub(crate) fn install(config: &GuiConfig, cx: &mut App) -> Result<(), String> {
    let bindings = compile(config, cx)?;
    apply_compiled(bindings, cx);
    Ok(())
}

pub(crate) fn apply_compiled(bindings: Vec<KeyBinding>, cx: &mut App) {
    cx.clear_key_bindings();
    cx.bind_keys(bindings);
    install_fixed_settings_bindings(cx);
    install_fixed_server_search_bindings(cx);
}

fn install_fixed_settings_bindings(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("backspace", composer::Backspace, Some("ChattSettingsInput")),
        KeyBinding::new("delete", composer::Delete, Some("ChattSettingsInput")),
        KeyBinding::new("left", composer::Left, Some("ChattSettingsInput")),
        KeyBinding::new("right", composer::Right, Some("ChattSettingsInput")),
        KeyBinding::new(
            "shift-left",
            composer::SelectLeft,
            Some("ChattSettingsInput"),
        ),
        KeyBinding::new(
            "shift-right",
            composer::SelectRight,
            Some("ChattSettingsInput"),
        ),
        KeyBinding::new(
            "secondary-a",
            composer::SelectAll,
            Some("ChattSettingsInput"),
        ),
        KeyBinding::new("secondary-v", composer::Paste, Some("ChattSettingsInput")),
        KeyBinding::new("secondary-c", composer::Copy, Some("ChattSettingsInput")),
        KeyBinding::new("secondary-x", composer::Cut, Some("ChattSettingsInput")),
    ]);
}

fn install_fixed_server_search_bindings(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("backspace", composer::Backspace, Some("ChattServerSearch")),
        KeyBinding::new("delete", composer::Delete, Some("ChattServerSearch")),
        KeyBinding::new("left", composer::Left, Some("ChattServerSearch")),
        KeyBinding::new("right", composer::Right, Some("ChattServerSearch")),
        KeyBinding::new(
            "shift-left",
            composer::SelectLeft,
            Some("ChattServerSearch"),
        ),
        KeyBinding::new(
            "shift-right",
            composer::SelectRight,
            Some("ChattServerSearch"),
        ),
        KeyBinding::new(
            "secondary-a",
            composer::SelectAll,
            Some("ChattServerSearch"),
        ),
        KeyBinding::new("secondary-v", composer::Paste, Some("ChattServerSearch")),
        KeyBinding::new("secondary-c", composer::Copy, Some("ChattServerSearch")),
        KeyBinding::new("secondary-x", composer::Cut, Some("ChattServerSearch")),
        KeyBinding::new("down", ServerNext, Some("ChattServerSearch")),
        KeyBinding::new("up", ServerPrevious, Some("ChattServerSearch")),
        KeyBinding::new("enter", ServerActivate, Some("ChattServerSearch")),
        KeyBinding::new("escape", CloseServerSelector, Some("ChattServerSearch")),
    ]);
}

fn make_action(scope: BindingScope, command: BindCommand) -> Box<dyn Action> {
    match (scope, command) {
        (BindingScope::Application, BindCommand::OpenMedia) => Box::new(OpenMedia),
        (BindingScope::Application, BindCommand::OpenSettings) => Box::new(OpenSettings),
        (BindingScope::Application, BindCommand::IncreaseUiScale) => Box::new(IncreaseUiScale),
        (BindingScope::Application, BindCommand::DecreaseUiScale) => Box::new(DecreaseUiScale),
        (BindingScope::Application, BindCommand::ResetUiScale) => Box::new(ResetUiScale),
        (BindingScope::NonInput, BindCommand::ToggleMute) => Box::new(ToggleMute),
        (BindingScope::NonInput, BindCommand::ToggleDeafen) => Box::new(ToggleDeafen),
        (BindingScope::NonInput, BindCommand::ToggleVoice) => Box::new(ToggleVoice),
        (BindingScope::Composer, BindCommand::SendMessage) => Box::new(SendMessage),
        (BindingScope::Composer, BindCommand::Newline) => Box::new(composer::Newline),
        (BindingScope::Composer | BindingScope::CodeSearch, BindCommand::Backspace) => {
            Box::new(composer::Backspace)
        }
        (BindingScope::Composer | BindingScope::CodeSearch, BindCommand::Delete) => {
            Box::new(composer::Delete)
        }
        (BindingScope::Composer | BindingScope::CodeSearch, BindCommand::Left) => {
            Box::new(composer::Left)
        }
        (BindingScope::Composer | BindingScope::CodeSearch, BindCommand::Right) => {
            Box::new(composer::Right)
        }
        (BindingScope::Composer | BindingScope::CodeSearch, BindCommand::SelectLeft) => {
            Box::new(composer::SelectLeft)
        }
        (BindingScope::Composer | BindingScope::CodeSearch, BindCommand::SelectRight) => {
            Box::new(composer::SelectRight)
        }
        (BindingScope::Composer | BindingScope::CodeSearch, BindCommand::SelectAll) => {
            Box::new(composer::SelectAll)
        }
        (
            BindingScope::Composer | BindingScope::CodeSearch | BindingScope::Vim,
            BindCommand::Paste,
        ) => Box::new(composer::Paste),
        (BindingScope::Composer | BindingScope::CodeSearch, BindCommand::Copy) => {
            Box::new(composer::Copy)
        }
        (BindingScope::Composer | BindingScope::CodeSearch, BindCommand::Cut) => {
            Box::new(composer::Cut)
        }
        (BindingScope::Composer, BindCommand::InsertTab) => Box::new(composer::InsertTab),
        (BindingScope::Completion, BindCommand::CompletionNext) => Box::new(CompletionNext),
        (BindingScope::Completion, BindCommand::CompletionPrevious) => Box::new(CompletionPrevious),
        (BindingScope::Completion, BindCommand::CompletionAccept) => Box::new(CompletionAccept),
        (BindingScope::Completion, BindCommand::CompletionAcceptEngaged) => {
            Box::new(CompletionAcceptEngaged)
        }
        (BindingScope::Completion, BindCommand::CompletionDismiss) => Box::new(CompletionDismiss),
        (BindingScope::CodeViewer, BindCommand::Copy) => Box::new(code_viewer::Copy),
        (BindingScope::CodeViewer, BindCommand::SelectAll) => Box::new(code_viewer::SelectAll),
        (BindingScope::CodeViewer, BindCommand::FindInCode) => Box::new(FindInCode),
        (BindingScope::CodeSearch, BindCommand::NextCodeMatch) => Box::new(NextCodeMatch),
        (BindingScope::CodeSearch, BindCommand::PreviousCodeMatch) => Box::new(PreviousCodeMatch),
        (BindingScope::CodeSearch, BindCommand::CloseCodeSearch) => Box::new(CloseCodeSearch),
        (BindingScope::FormattedMessage, BindCommand::Copy) => Box::new(formatted_message::Copy),
        (BindingScope::NonInput, BindCommand::ClosePreview) => Box::new(ClosePreview),
        (BindingScope::NonInput, BindCommand::TogglePlayback) => Box::new(TogglePlayback),
        (BindingScope::NonInput, BindCommand::SeekBack) => Box::new(SeekBack),
        (BindingScope::NonInput, BindCommand::SeekForward) => Box::new(SeekForward),
        (BindingScope::NonInput, BindCommand::LiveZoomIn) => Box::new(LiveZoomIn),
        (BindingScope::NonInput, BindCommand::LiveZoomOut) => Box::new(LiveZoomOut),
        (BindingScope::NonInput, BindCommand::LiveReset) => Box::new(LiveReset),
        (BindingScope::NonInput, BindCommand::LivePanUp) => Box::new(LivePanUp),
        (BindingScope::NonInput, BindCommand::LivePanDown) => Box::new(LivePanDown),
        _ => unreachable!("validated scope/command pair"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_identities_are_unique_and_defaults_are_conflict_free() {
        let mut identities = std::collections::HashSet::new();
        for binding in BINDINGS {
            assert!(identities.insert((binding.scope, binding.command)));
            for sequence in binding.defaults {
                assert!(canonical_sequence(sequence).is_ok());
            }
        }
        assert!(validate(&GuiConfig::default()).is_empty());
        let expanded_default_count = BINDINGS
            .iter()
            .map(|binding| binding.defaults.len() * binding.contexts.len())
            .sum::<usize>();
        assert_eq!(expanded_default_count, 52);
        assert_eq!(
            effective_scope(&GuiConfig::default().bindings, BindingScope::Composer)
                .iter()
                .filter(|(sequence, _)| sequence == "enter")
                .count(),
            1
        );
        assert_eq!(
            effective_scope(&GuiConfig::default().bindings, BindingScope::Completion)
                .iter()
                .filter(|(sequence, _)| sequence == "enter")
                .count(),
            1
        );
    }

    #[test]
    fn rebinding_composer_enter_does_not_change_completion_enter() {
        let mut config = GuiConfig::default();
        set_sequences(
            &mut config,
            BindingScope::Composer,
            BindCommand::SendMessage,
            &["cmd-enter".into()],
        );

        assert_eq!(
            effective_sequences(&config, BindingScope::Composer, BindCommand::SendMessage),
            vec!["cmd-enter"]
        );
        assert_eq!(
            effective_sequences(
                &config,
                BindingScope::Completion,
                BindCommand::CompletionAcceptEngaged
            ),
            vec!["enter"]
        );
    }

    #[test]
    fn rejects_duplicate_canonical_bindings_within_one_dispatch_scope() {
        let mut config = GuiConfig::default();
        config
            .bindings
            .application
            .insert(" cmd-o ".into(), BindCommand::OpenSettings);

        let diagnostics = validate(&config);
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("same canonical binding"))
        );
    }

    #[test]
    fn unbind_and_override_apply_to_one_sequence() {
        let mut config = GuiConfig::default();
        config
            .bindings
            .application
            .insert("cmd-o".into(), BindCommand::Unbind);
        config
            .bindings
            .application
            .insert("secondary-o".into(), BindCommand::OpenMedia);
        assert_eq!(
            effective_sequences(&config, BindingScope::Application, BindCommand::OpenMedia),
            vec!["secondary-o"]
        );
    }

    #[test]
    fn multiple_sequences_can_target_one_command() {
        let mut config = GuiConfig::default();
        set_sequences(
            &mut config,
            BindingScope::Application,
            BindCommand::OpenSettings,
            &["cmd-,".into(), "secondary-,".into()],
        );
        assert_eq!(
            effective_sequences(
                &config,
                BindingScope::Application,
                BindCommand::OpenSettings
            ),
            vec!["cmd-,", "secondary-,"]
        );
    }

    #[gpui::test]
    fn default_keymap_compiles_with_the_platform_mapper(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let compiled = compile(&GuiConfig::default(), cx).unwrap();
            assert!(!compiled.is_empty());
        });
    }
}
