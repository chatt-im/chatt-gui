mod catalog;

use std::sync::Arc;

use gpui::{
    AnyElement, App, Context, Div, Entity, EventEmitter, FocusHandle, Focusable, FontWeight,
    Global, KeyBinding, KeyDownEvent, Render, SharedString, Subscription, Task,
    UniformListScrollHandle, WeakEntity, div, prelude::*, px, rgba, uniform_list,
};

use crate::{
    composer::{Composer, ComposerChanged},
    config::{
        io::{self, LoadedConfig, SaveError, SourceStatus},
        schema::{BindCommand, BindingMode, FontRendering, GuiConfig, Rgba8},
        validation::{ConfigDiagnostic, DiagnosticSeverity, has_errors, validate},
    },
    key_bindings::{self, BindingScope},
    theme::{self, AppliedSettings, FontRole, ThemePalette, ThemeRole},
};
use catalog::{
    RowRef, SETTINGS_SECTIONS, ScalarSetting, SettingsSection, help, label, matches_search, path,
    rows,
};

#[derive(Clone)]
pub(crate) struct ConfigurationState(pub(crate) LoadedConfig);

impl Global for ConfigurationState {}

pub(crate) fn install_loaded(loaded: LoadedConfig, cx: &mut App) {
    cx.set_global(ConfigurationState(loaded));
}

pub(crate) struct SettingsClosed;

struct PendingSave {
    config: GuiConfig,
    bindings: Vec<KeyBinding>,
}

pub(crate) struct SettingsView {
    focus: FocusHandle,
    search: Entity<Composer>,
    editor: Entity<Composer>,
    active_editor: Option<RowRef>,
    active_section: usize,
    query: String,
    draft: GuiConfig,
    baseline: GuiConfig,
    source: Option<Vec<u8>>,
    source_status: SourceStatus,
    diagnostics: Vec<ConfigDiagnostic>,
    path: Option<std::path::PathBuf>,
    committed: Arc<theme::ResolvedSettings>,
    available_families: Vec<String>,
    editor_error: Option<SharedString>,
    status_message: Option<SharedString>,
    saving: bool,
    confirm_reload: bool,
    confirm_replace: bool,
    pending_save: Option<PendingSave>,
    pending_reload_draft: Option<GuiConfig>,
    recording: Option<(BindingScope, BindCommand)>,
    key_interceptor: Option<Subscription>,
    scroll: UniformListScrollHandle,
    _save_task: Option<Task<()>>,
    _search_subscription: Subscription,
    _editor_subscription: Subscription,
}

impl EventEmitter<SettingsClosed> for SettingsView {}

impl SettingsView {
    pub(crate) fn new(cx: &mut Context<Self>) -> Self {
        let loaded = cx.global::<ConfigurationState>().0.clone();
        let committed = AppliedSettings::get(cx);
        let search = cx.new(|cx| Composer::settings_input("Search settings", cx));
        let editor = cx.new(|cx| Composer::settings_input("Edit value", cx));
        let search_subscription = cx.subscribe(&search, |this, search, _: &ComposerChanged, cx| {
            this.query = search.read(cx).text();
            this.scroll.scroll_to_item(0, gpui::ScrollStrategy::Top);
            cx.notify();
        });
        let editor_subscription = cx.subscribe(&editor, |this, editor, _: &ComposerChanged, cx| {
            let value = editor.read(cx).text();
            this.apply_editor_text(&value, cx);
        });
        let mut system_families = cx.text_system().all_font_names();
        system_families.sort_by_key(|name| name.to_ascii_lowercase());
        system_families.dedup();
        system_families
            .retain(|name| name != "IBM Plex Sans" && name != "Lilex" && name != ".SystemUIFont");
        let mut available_families = vec![
            "IBM Plex Sans".into(),
            "Lilex".into(),
            ".SystemUIFont".into(),
        ];
        available_families.extend(system_families);

        Self {
            focus: cx.focus_handle(),
            search,
            editor,
            active_editor: None,
            active_section: 0,
            query: String::new(),
            draft: loaded.config.clone(),
            baseline: loaded.config,
            source: loaded.source,
            source_status: loaded.status,
            diagnostics: loaded.diagnostics,
            path: loaded.path,
            committed,
            available_families,
            editor_error: None,
            status_message: None,
            saving: false,
            confirm_reload: false,
            confirm_replace: false,
            pending_save: None,
            pending_reload_draft: None,
            recording: None,
            key_interceptor: None,
            scroll: UniformListScrollHandle::new(),
            _save_task: None,
            _search_subscription: search_subscription,
            _editor_subscription: editor_subscription,
        }
    }

    fn dirty(&self) -> bool {
        self.draft != self.baseline || self.editor_error.is_some()
    }

    fn section(&self) -> &'static SettingsSection {
        &SETTINGS_SECTIONS[self.active_section]
    }

    fn visible_rows(&self) -> Vec<RowRef> {
        let section = self.section();
        rows(section, self.diagnostics.len())
            .into_iter()
            .filter(|row| matches_search(section, *row, &self.query))
            .collect()
    }

    fn select_section(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        self.active_section = index;
        self.active_editor = None;
        self.editor_error = None;
        self.scroll.scroll_to_item(0, gpui::ScrollStrategy::Top);
        cx.notify();
    }

    fn select_row(&mut self, row: RowRef, cx: &mut Context<Self>) {
        if self.saving || matches!(row, RowRef::Diagnostic(_)) {
            return;
        }
        self.active_editor = Some(row);
        self.editor_error = None;
        if matches!(row, RowRef::Choice(_)) {
            cx.notify();
            return;
        }
        let value = self.edit_text(row);
        self.editor
            .update(cx, |editor, cx| editor.set_value(value, cx));
        cx.notify();
    }

    fn edit_text(&self, row: RowRef) -> String {
        match row {
            RowRef::Theme(role) => self.draft.theme.color(role).to_string(),
            RowRef::FontFamily(role) => self.draft.fonts.family(role).to_string(),
            RowRef::FontSize(role) => format!("{:.1}", self.draft.fonts.size(role)),
            RowRef::Binding(scope, command) => {
                key_bindings::effective_sequences(&self.draft, scope, command).join(", ")
            }
            RowRef::Choice(_) | RowRef::Diagnostic(_) => String::new(),
        }
    }

    fn apply_editor_text(&mut self, text: &str, cx: &mut Context<Self>) {
        let Some(row) = self.active_editor else {
            return;
        };
        let result = match row {
            RowRef::Theme(role) => Rgba8::parse(text.trim())
                .map(|color| {
                    let changed = self.draft.theme.color(role) != color;
                    self.draft.theme.set_color(role, color);
                    changed
                })
                .map_err(str::to_string),
            RowRef::FontFamily(role) => {
                if text.trim().is_empty() {
                    Err("font family must not be empty".into())
                } else {
                    let family = text.trim();
                    let changed = self.draft.fonts.family(role) != family;
                    self.draft.fonts.set_family(role, family.to_string());
                    Ok(changed)
                }
            }
            RowRef::FontSize(role) => match text.trim().parse::<f32>() {
                Ok(size) if size.is_finite() && (8.0..=48.0).contains(&size) => {
                    let changed = self.draft.fonts.size(role) != size;
                    self.draft.fonts.set_size(role, size);
                    Ok(changed)
                }
                _ => Err("enter a number from 8 through 48".into()),
            },
            RowRef::Binding(scope, command) => {
                let sequences = text
                    .split([',', '\n'])
                    .map(str::trim)
                    .filter(|sequence| !sequence.is_empty())
                    .map(str::to_string)
                    .collect::<Vec<_>>();
                let error = sequences.iter().find_map(|sequence| {
                    sequence
                        .split_whitespace()
                        .find_map(|chord| gpui::Keystroke::parse(chord).err())
                        .map(|error| format!("invalid binding `{sequence}`: {error}"))
                });
                if let Some(error) = error {
                    Err(error)
                } else {
                    let mut candidate = self.draft.clone();
                    key_bindings::set_sequences(&mut candidate, scope, command, &sequences);
                    let conflicts = key_bindings::validate(&candidate);
                    if let Some(error) = conflicts.first() {
                        Err(error.message.clone())
                    } else {
                        let changed = self.draft != candidate;
                        self.draft = candidate;
                        Ok(changed)
                    }
                }
            }
            RowRef::Choice(_) | RowRef::Diagnostic(_) => Ok(false),
        };
        match result {
            Ok(changed) => {
                self.editor_error = None;
                if changed
                    && matches!(
                        row,
                        RowRef::Theme(_) | RowRef::FontFamily(_) | RowRef::FontSize(_)
                    )
                {
                    self.preview(cx);
                }
            }
            Err(error) => self.editor_error = Some(error.into()),
        }
        cx.notify();
    }

    fn preview(&mut self, cx: &mut Context<Self>) {
        let diagnostics = validate(&self.draft);
        if has_errors(&diagnostics) {
            return;
        }
        let mut preview = self.draft.clone();
        preview.input.default_binding_mode = self.committed.binding_mode;
        let mut diagnostics = diagnostics;
        diagnostics.extend(theme::font_warnings(&preview, &self.available_families));
        theme::apply_appearance(
            &preview,
            self.source_status,
            &diagnostics,
            &self.available_families,
            cx,
        );
    }

    fn choose(&mut self, setting: ScalarSetting, value: usize, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        match setting {
            ScalarSetting::FontRendering => {
                self.draft.fonts.rendering = match value {
                    1 => FontRendering::Subpixel,
                    2 => FontRendering::Grayscale,
                    _ => FontRendering::PlatformDefault,
                };
                self.preview(cx);
            }
            ScalarSetting::BindingMode => {
                self.draft.input.default_binding_mode = if value == 1 {
                    BindingMode::Vim
                } else {
                    BindingMode::Standard
                };
            }
        }
        cx.notify();
    }

    fn choose_font(&mut self, role: FontRole, family: String, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        let changed = self.draft.fonts.family(role) != family;
        self.draft.fonts.set_family(role, family.clone());
        self.active_editor = Some(RowRef::FontFamily(role));
        self.editor
            .update(cx, |editor, cx| editor.set_value(family, cx));
        if changed {
            self.preview(cx);
        }
        cx.notify();
    }

    fn reset_row(&mut self, row: RowRef, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        let defaults = GuiConfig::default();
        let appearance_changed = self.reset_row_value(row, &defaults);
        self.editor_error = None;
        if self.active_editor == Some(row) {
            let value = self.edit_text(row);
            self.editor
                .update(cx, |editor, cx| editor.set_value(value, cx));
        }
        if appearance_changed {
            self.preview(cx);
        }
        cx.notify();
    }

    fn reset_row_value(&mut self, row: RowRef, defaults: &GuiConfig) -> bool {
        match row {
            RowRef::Theme(role) => {
                self.draft.theme.set_color(role, defaults.theme.color(role));
                true
            }
            RowRef::FontFamily(role) => {
                self.draft
                    .fonts
                    .set_family(role, defaults.fonts.family(role).to_string());
                true
            }
            RowRef::FontSize(role) => {
                self.draft.fonts.set_size(role, defaults.fonts.size(role));
                true
            }
            RowRef::Choice(ScalarSetting::FontRendering) => {
                self.draft.fonts.rendering = defaults.fonts.rendering;
                true
            }
            RowRef::Choice(ScalarSetting::BindingMode) => {
                self.draft.input.default_binding_mode = defaults.input.default_binding_mode;
                false
            }
            RowRef::Binding(scope, command) => {
                let defaults = key_bindings::effective_sequences(&defaults, scope, command);
                key_bindings::set_sequences(&mut self.draft, scope, command, &defaults);
                false
            }
            RowRef::Diagnostic(_) => false,
        }
    }

    fn reset_section(&mut self, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        let section_rows = rows(self.section(), self.diagnostics.len());
        let defaults = GuiConfig::default();
        let mut appearance_changed = false;
        for row in section_rows {
            appearance_changed |= self.reset_row_value(row, &defaults);
        }
        self.editor_error = None;
        if let Some(row) = self.active_editor {
            let value = self.edit_text(row);
            self.editor
                .update(cx, |editor, cx| editor.set_value(value, cx));
        }
        if appearance_changed {
            self.preview(cx);
        }
        cx.notify();
    }

    fn reset_all(&mut self, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        self.draft = GuiConfig::default();
        self.active_editor = None;
        self.editor_error = None;
        self.preview(cx);
        cx.notify();
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        cx.set_text_rendering_mode(theme::rendering_mode(self.committed.rendering));
        cx.set_global(AppliedSettings(self.committed.clone()));
        cx.refresh_windows();
        cx.emit(SettingsClosed);
    }

    fn start_recording(
        &mut self,
        scope: BindingScope,
        command: BindCommand,
        cx: &mut Context<Self>,
    ) {
        if self.saving {
            return;
        }
        self.key_interceptor = None;
        self.recording = Some((scope, command));
        self.status_message = Some("Press one key chord; Escape cancels recording.".into());
        let view = cx.entity().downgrade();
        self.key_interceptor = Some(cx.intercept_keystrokes(move |event, _, cx| {
            cx.stop_propagation();
            let sequence = event.keystroke.to_string();
            let cancelled = event.keystroke.key == "escape";
            let _ = view.update(cx, |this, cx| {
                this.finish_recording((!cancelled).then_some(sequence), cx)
            });
        }));
        cx.notify();
    }

    fn finish_recording(&mut self, sequence: Option<String>, cx: &mut Context<Self>) {
        self.key_interceptor = None;
        let Some((scope, command)) = self.recording.take() else {
            return;
        };
        if let Some(sequence) = sequence {
            let mut sequences = key_bindings::effective_sequences(&self.draft, scope, command);
            if !sequences.contains(&sequence) {
                sequences.push(sequence);
            }
            let mut candidate = self.draft.clone();
            key_bindings::set_sequences(&mut candidate, scope, command, &sequences);
            if let Some(error) = key_bindings::validate(&candidate).first() {
                self.editor_error = Some(error.message.clone().into());
                self.status_message = Some("Recorded chord conflicts with another action.".into());
                cx.notify();
                return;
            }
            self.draft = candidate;
            self.active_editor = Some(RowRef::Binding(scope, command));
            let value = sequences.join(", ");
            self.editor
                .update(cx, |editor, cx| editor.set_value(value, cx));
            self.status_message = Some("Recorded chord. Save to commit the keymap.".into());
        } else {
            self.status_message = Some("Key recording cancelled.".into());
        }
        cx.notify();
    }

    fn save(&mut self, replace: bool, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        if self.editor_error.is_some() {
            self.status_message =
                Some("Fix or discard the invalid editor value before saving.".into());
            cx.notify();
            return;
        }
        if replace && !self.confirm_replace {
            self.confirm_replace = true;
            self.status_message = Some(
                "The file cannot be preserved safely. Click Confirm replace to write canonical version-1 TOML."
                    .into(),
            );
            cx.notify();
            return;
        }
        let Some(path) = self.path.clone() else {
            self.status_message =
                Some("No configuration path is available; changes remain session-only.".into());
            cx.notify();
            return;
        };
        let mut diagnostics = validate(&self.draft);
        diagnostics.extend(key_bindings::validate(&self.draft));
        if has_errors(&diagnostics) {
            self.diagnostics = diagnostics;
            self.status_message = Some("Fix validation errors before saving.".into());
            cx.notify();
            return;
        }
        let bindings = match key_bindings::compile(&self.draft, cx) {
            Ok(bindings) => bindings,
            Err(error) => {
                self.status_message = Some(error.into());
                cx.notify();
                return;
            }
        };
        let config = self.draft.clone();
        let baseline = self.source.clone();
        let executor = cx.background_executor().clone();
        self.saving = true;
        self.confirm_replace = false;
        self.pending_save = Some(PendingSave {
            config: config.clone(),
            bindings,
        });
        self.status_message = Some("Saving…".into());
        self._save_task = Some(cx.spawn(async move |this, cx| {
            let result = executor
                .spawn(async move { io::save(&path, baseline.as_deref(), &config, replace) })
                .await;
            let _ = this.update(cx, |this, cx| this.finish_save(result, cx));
        }));
        cx.notify();
    }

    fn finish_save(&mut self, result: Result<Vec<u8>, SaveError>, cx: &mut Context<Self>) {
        self.saving = false;
        match result {
            Ok(source) => {
                let saved = self
                    .pending_save
                    .take()
                    .expect("successful save retains its exact configuration snapshot");
                self.confirm_replace = false;
                key_bindings::apply_compiled(saved.bindings, cx);
                self.source = Some(source.clone());
                self.source_status = SourceStatus::Loaded;
                self.baseline = saved.config.clone();
                self.diagnostics = validate(&saved.config);
                self.diagnostics
                    .extend(key_bindings::validate(&saved.config));
                self.diagnostics.extend(theme::font_warnings(
                    &saved.config,
                    &self.available_families,
                ));
                self.committed = theme::apply_appearance(
                    &saved.config,
                    SourceStatus::Loaded,
                    &self.diagnostics,
                    &self.available_families,
                    cx,
                );
                cx.set_global(ConfigurationState(LoadedConfig {
                    path: self.path.clone(),
                    config: saved.config,
                    source: Some(source),
                    status: SourceStatus::Loaded,
                    diagnostics: self.diagnostics.clone(),
                }));
                if self.dirty() {
                    self.preview(cx);
                    self.status_message =
                        Some("Saved. Newer editor changes remain unsaved.".into());
                } else {
                    self.status_message = Some("Saved.".into());
                }
            }
            Err(SaveError::Invalid(diagnostics)) => {
                self.pending_save = None;
                self.diagnostics = diagnostics;
                self.status_message = Some("The draft contains invalid values.".into());
            }
            Err(SaveError::Conflict) => {
                self.pending_save = None;
                self.confirm_replace = true;
                self.status_message = Some(
                    "gui.toml changed on disk. Click Confirm replace to keep this draft, or reload."
                        .into(),
                );
            }
            Err(error) => {
                self.pending_save = None;
                self.status_message = Some(error.to_string().into());
            }
        }
        cx.notify();
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        if self.dirty() && !self.confirm_reload {
            self.confirm_reload = true;
            self.status_message =
                Some("Reload discards the current draft. Click Confirm reload to continue.".into());
            cx.notify();
            return;
        }
        self.confirm_reload = false;
        let Some(path) = self.path.clone() else {
            self.status_message = Some("No configuration path is available.".into());
            cx.notify();
            return;
        };
        let executor = cx.background_executor().clone();
        self.saving = true;
        self.pending_reload_draft = Some(self.draft.clone());
        self.status_message = Some("Reloading…".into());
        self._save_task = Some(cx.spawn(async move |this, cx| {
            let loaded = executor.spawn(async move { io::load_path(path) }).await;
            let _ = this.update(cx, |this, cx| this.finish_reload(loaded, cx));
        }));
        cx.notify();
    }

    fn finish_reload(&mut self, loaded: LoadedConfig, cx: &mut Context<Self>) {
        self.saving = false;
        let draft_at_start = self
            .pending_reload_draft
            .take()
            .expect("reload retains the draft it was started from");
        if self.draft != draft_at_start || self.editor_error.is_some() {
            self.status_message =
                Some("Reload finished, but newer editor changes were kept. Reload again to discard them.".into());
            cx.notify();
            return;
        }
        if matches!(loaded.status, SourceStatus::Loaded | SourceStatus::Missing) {
            let mut diagnostics = loaded.diagnostics.clone();
            diagnostics.extend(key_bindings::validate(&loaded.config));
            diagnostics.extend(theme::font_warnings(
                &loaded.config,
                &self.available_families,
            ));
            match key_bindings::compile(&loaded.config, cx) {
                Ok(bindings) if !has_errors(&diagnostics) => {
                    key_bindings::apply_compiled(bindings, cx);
                    self.draft = loaded.config.clone();
                    self.baseline = loaded.config.clone();
                    self.source = loaded.source.clone();
                    self.source_status = loaded.status;
                    self.diagnostics = diagnostics;
                    self.active_editor = None;
                    self.committed = theme::apply_appearance(
                        &self.draft,
                        loaded.status,
                        &self.diagnostics,
                        &self.available_families,
                        cx,
                    );
                    cx.set_global(ConfigurationState(LoadedConfig {
                        diagnostics: self.diagnostics.clone(),
                        ..loaded
                    }));
                    self.status_message = Some("Reloaded from disk.".into());
                }
                Ok(_) => {
                    self.diagnostics = diagnostics;
                    self.status_message = Some(
                        "Reloaded file contains invalid settings; current values kept.".into(),
                    );
                }
                Err(error) => {
                    self.status_message = Some(error.into());
                }
            }
        } else {
            self.diagnostics = loaded.diagnostics;
            self.status_message =
                Some("The file is invalid; the current draft and preview were kept.".into());
        }
        cx.notify();
    }

    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        _: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        if event.keystroke.key == "escape" && !self.saving {
            self.cancel(cx);
        }
    }

    fn render_choice_controls(&self, setting: ScalarSetting, view: WeakEntity<Self>) -> Div {
        let palette = ThemePalette::from_config(&self.draft.theme);
        let choices: &[(&str, usize, bool)] = match setting {
            ScalarSetting::FontRendering => &[
                (
                    "Platform default",
                    0,
                    self.draft.fonts.rendering == FontRendering::PlatformDefault,
                ),
                (
                    "Subpixel",
                    1,
                    self.draft.fonts.rendering == FontRendering::Subpixel,
                ),
                (
                    "Grayscale",
                    2,
                    self.draft.fonts.rendering == FontRendering::Grayscale,
                ),
            ],
            ScalarSetting::BindingMode => &[
                (
                    "Standard",
                    0,
                    self.draft.input.default_binding_mode == BindingMode::Standard,
                ),
                (
                    "Vim",
                    1,
                    self.draft.input.default_binding_mode == BindingMode::Vim,
                ),
            ],
        };
        let mut controls = div().flex().gap_2();
        for (title, value, selected) in choices {
            let title = *title;
            let value = *value;
            let selected = *selected;
            let view = view.clone();
            controls = controls.child(
                setting_button(
                    ("choice", setting as usize * 10 + value),
                    title,
                    selected,
                    &palette,
                )
                .on_click(move |_, _, cx| {
                    let _ = view.update(cx, |this, cx| this.choose(setting, value, cx));
                }),
            );
        }
        controls
    }

    fn render_font_suggestions(&self, role: FontRole, query: &str, view: WeakEntity<Self>) -> Div {
        let palette = ThemePalette::from_config(&self.draft.theme);
        let query = query.trim().to_ascii_lowercase();
        let mut suggestions = div().mt_2().flex().flex_wrap().gap_2();
        for (index, family) in self
            .available_families
            .iter()
            .filter(|family| query.is_empty() || family.to_ascii_lowercase().contains(&query))
            .take(8)
            .cloned()
            .enumerate()
        {
            let selected = self.draft.fonts.family(role) == family;
            let title = family.clone();
            let view = view.clone();
            suggestions = suggestions.child(
                setting_button(("font-suggestion", index), title, selected, &palette).on_click(
                    move |_, _, cx| {
                        let family = family.clone();
                        let _ = view.update(cx, |this, cx| this.choose_font(role, family, cx));
                    },
                ),
            );
        }
        suggestions
    }
}

impl Render for SettingsView {
    fn render(&mut self, window: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity().downgrade();
        let palette = ThemePalette::from_config(&self.draft.theme);
        let section = self.section();
        let visible_rows = self.visible_rows();
        let draft = self.draft.clone();
        let diagnostics = self.diagnostics.clone();
        let diagnostic_source = self
            .source
            .as_deref()
            .and_then(|source| std::str::from_utf8(source).ok())
            .map(ToOwned::to_owned);
        let active = self.active_editor;
        let row_view = view.clone();
        let row_palette = palette.clone();
        let list_rows = visible_rows.clone();
        let list = uniform_list(
            ("settings-rows", self.active_section),
            list_rows.len(),
            move |range, _, _| {
                range
                    .map(|index| {
                        render_row(
                            list_rows[index],
                            &draft,
                            &diagnostics,
                            active,
                            row_view.clone(),
                            &row_palette,
                            diagnostic_source.as_deref(),
                        )
                    })
                    .collect::<Vec<_>>()
            },
        )
        .track_scroll(&self.scroll)
        .w_full()
        .flex_1();

        let mut navigation = div()
            .w(px(190.))
            .flex_none()
            .flex()
            .flex_col()
            .gap_1()
            .p_3()
            .border_r_1()
            .border_color(palette.color(ThemeRole::BorderSubtle));
        for (index, candidate) in SETTINGS_SECTIONS.iter().enumerate() {
            let selected = index == self.active_section;
            let view = view.clone();
            navigation = navigation.child(
                setting_button(candidate.id, candidate.title, selected, &palette)
                    .w_full()
                    .on_click(move |_, _, cx| {
                        let _ = view.update(cx, |this, cx| this.select_section(index, cx));
                    }),
            );
        }
        let reset_all_view = view.clone();
        navigation = navigation.child(div().flex_1()).child(
            setting_button("reset-all", "Reset all", false, &palette).on_click(move |_, _, cx| {
                let _ = reset_all_view.update(cx, |this, cx| this.reset_all(cx));
            }),
        );

        let editor_query = self.editor.read(cx).text();
        let editor = self.active_editor.map(|row| {
            div()
                .flex_none()
                .p_3()
                .border_b_1()
                .border_color(palette.color(ThemeRole::BorderSubtle))
                .bg(palette.color(ThemeRole::Window))
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(format!("Edit {}", label(row))),
                )
                .child(
                    div()
                        .mt_2()
                        .px_3()
                        .py_2()
                        .rounded_md()
                        .border_1()
                        .border_color(if self.editor_error.is_some() {
                            palette.color(ThemeRole::StateDanger)
                        } else {
                            palette.color(ThemeRole::BorderStrong)
                        })
                        .bg(palette.color(ThemeRole::Input))
                        .child(self.editor.clone()),
                )
                .when_some(self.editor_error.clone(), |editor, error| {
                    editor.child(
                        div()
                            .mt_1()
                            .text_xs()
                            .text_color(palette.color(ThemeRole::StateDanger))
                            .child(error),
                    )
                })
                .when(matches!(row, RowRef::Binding(_, _)), |editor| {
                    let RowRef::Binding(scope, command) = row else {
                        unreachable!()
                    };
                    let record_view = view.clone();
                    editor.child(
                        div().mt_2().child(
                            setting_button(
                                ("record-binding", row_id(row)),
                                if self.recording == Some((scope, command)) {
                                    "Recording…"
                                } else {
                                    "Record one chord"
                                },
                                self.recording == Some((scope, command)),
                                &palette,
                            )
                            .on_click(move |_, _, cx| {
                                let _ = record_view.update(cx, |this, cx| {
                                    this.start_recording(scope, command, cx)
                                });
                            }),
                        ),
                    )
                })
                .when(matches!(row, RowRef::FontFamily(_)), |editor| {
                    let RowRef::FontFamily(role) = row else {
                        unreachable!()
                    };
                    editor.child(self.render_font_suggestions(role, &editor_query, view.clone()))
                })
        });

        let reset_section_view = view.clone();
        let reload_view = view.clone();
        let cancel_view = view.clone();
        let save_view = view.clone();
        let replace = self.confirm_replace
            || matches!(
                self.source_status,
                SourceStatus::Invalid | SourceStatus::ReadFailed
            );
        let save_label = if self.saving {
            "Working…"
        } else if self.confirm_replace {
            "Confirm replace"
        } else if replace {
            "Replace invalid file"
        } else {
            "Save"
        };

        window.set_rem_size(px(AppliedSettings::get(cx).fonts.interface_size));
        div()
            .id("settings")
            .key_context("ChattSettings")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::handle_key_down))
            .absolute()
            .inset_0()
            .p_6()
            .bg(palette.color(ThemeRole::Scrim))
            .child(
                div()
                    .size_full()
                    .overflow_hidden()
                    .rounded_lg()
                    .border_1()
                    .border_color(palette.color(ThemeRole::BorderStrong))
                    .bg(palette.color(ThemeRole::Raised))
                    .shadow_lg()
                    .flex()
                    .flex_col()
                    .font_family(AppliedSettings::get(cx).fonts.interface_family.clone())
                    .child(
                        div()
                            .h(px(64.))
                            .flex_none()
                            .flex()
                            .items_center()
                            .gap_3()
                            .px_5()
                            .border_b_1()
                            .border_color(palette.color(ThemeRole::BorderSubtle))
                            .child(
                                div()
                                    .text_lg()
                                    .font_weight(FontWeight::BOLD)
                                    .child("Settings"),
                            )
                            .child(
                                div()
                                    .w(px(360.))
                                    .px_3()
                                    .py_2()
                                    .rounded_md()
                                    .border_1()
                                    .border_color(palette.color(ThemeRole::BorderStrong))
                                    .bg(palette.color(ThemeRole::Input))
                                    .child(self.search.clone()),
                            )
                            .child(div().flex_1())
                            .when(self.dirty(), |header| {
                                header.child(
                                    div()
                                        .text_xs()
                                        .text_color(palette.color(ThemeRole::StateWarning))
                                        .child("Unsaved preview"),
                                )
                            }),
                    )
                    .child(
                        div().flex_1().min_h_0().flex().child(navigation).child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .child(
                                    div()
                                        .flex_none()
                                        .px_5()
                                        .py_3()
                                        .border_b_1()
                                        .border_color(palette.color(ThemeRole::BorderSubtle))
                                        .child(
                                            div()
                                                .font_weight(FontWeight::SEMIBOLD)
                                                .child(section.title),
                                        )
                                        .child(
                                            div()
                                                .text_sm()
                                                .text_color(palette.color(ThemeRole::TextMuted))
                                                .child(section.help),
                                        ),
                                )
                                .when_some(editor, |content, editor| content.child(editor))
                                .when(
                                    matches!(
                                        active,
                                        Some(RowRef::Choice(ScalarSetting::FontRendering))
                                            | Some(RowRef::Choice(ScalarSetting::BindingMode))
                                    ),
                                    |content| {
                                        let RowRef::Choice(setting) = active.unwrap() else {
                                            unreachable!()
                                        };
                                        content.child(
                                            div()
                                                .p_3()
                                                .border_b_1()
                                                .border_color(
                                                    palette.color(ThemeRole::BorderSubtle),
                                                )
                                                .child(
                                                    self.render_choice_controls(
                                                        setting,
                                                        view.clone(),
                                                    ),
                                                ),
                                        )
                                    },
                                )
                                .child(list),
                        ),
                    )
                    .child(
                        div()
                            .h(px(58.))
                            .flex_none()
                            .flex()
                            .items_center()
                            .gap_2()
                            .px_4()
                            .border_t_1()
                            .border_color(palette.color(ThemeRole::BorderSubtle))
                            .when_some(self.status_message.clone(), |footer, message| {
                                footer.child(
                                    div()
                                        .flex_1()
                                        .text_sm()
                                        .text_color(palette.color(ThemeRole::TextMuted))
                                        .child(message),
                                )
                            })
                            .when(self.status_message.is_none(), |footer| {
                                footer.child(div().flex_1())
                            })
                            .child(
                                setting_button("reset-section", "Reset section", false, &palette)
                                    .on_click(move |_, _, cx| {
                                        let _ = reset_section_view
                                            .update(cx, |this, cx| this.reset_section(cx));
                                    }),
                            )
                            .child(
                                setting_button(
                                    "reload-settings",
                                    if self.confirm_reload {
                                        "Confirm reload"
                                    } else {
                                        "Reload from disk"
                                    },
                                    self.confirm_reload,
                                    &palette,
                                )
                                .on_click(move |_, _, cx| {
                                    let _ = reload_view.update(cx, |this, cx| this.reload(cx));
                                }),
                            )
                            .child(
                                setting_button("cancel-settings", "Cancel", false, &palette)
                                    .on_click(move |_, _, cx| {
                                        let _ = cancel_view.update(cx, |this, cx| this.cancel(cx));
                                    }),
                            )
                            .child(
                                setting_button("save-settings", save_label, true, &palette)
                                    .on_click(move |_, _, cx| {
                                        let _ =
                                            save_view.update(cx, |this, cx| this.save(replace, cx));
                                    }),
                            ),
                    ),
            )
    }
}

impl Focusable for SettingsView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

fn render_row(
    row: RowRef,
    draft: &GuiConfig,
    diagnostics: &[ConfigDiagnostic],
    active: Option<RowRef>,
    view: WeakEntity<SettingsView>,
    palette: &ThemePalette,
    source: Option<&str>,
) -> AnyElement {
    if let RowRef::Diagnostic(index) = row {
        return render_diagnostic_row(index, diagnostics, draft, palette, source);
    }
    let row_label = match row {
        RowRef::Diagnostic(index) => diagnostics
            .get(index)
            .map(|diagnostic| diagnostic.path.as_str())
            .unwrap_or("diagnostic"),
        _ => label(row),
    };
    let row_value = match row {
        RowRef::Theme(role) => draft.theme.color(role).to_string(),
        RowRef::FontFamily(role) => draft.fonts.family(role).to_string(),
        RowRef::FontSize(role) => format!("{:.1} px", draft.fonts.size(role)),
        RowRef::Choice(ScalarSetting::FontRendering) => format!("{:?}", draft.fonts.rendering),
        RowRef::Choice(ScalarSetting::BindingMode) => {
            format!("{:?}", draft.input.default_binding_mode)
        }
        RowRef::Binding(scope, command) => {
            let values = key_bindings::effective_sequences(draft, scope, command);
            if values.is_empty() {
                "Unbound".into()
            } else {
                values.join(", ")
            }
        }
        RowRef::Diagnostic(index) => diagnostics
            .get(index)
            .map(|diagnostic| diagnostic.message.clone())
            .unwrap_or_default(),
    };
    let path_text = path(row);
    let reset_view = view.clone();
    let select_view = view.clone();
    let diagnostic_color = match row {
        RowRef::Diagnostic(index)
            if diagnostics
                .get(index)
                .is_some_and(|diagnostic| diagnostic.severity == DiagnosticSeverity::Error) =>
        {
            palette.color(ThemeRole::StateDanger)
        }
        RowRef::Diagnostic(_) => palette.color(ThemeRole::StateWarning),
        _ => palette.color(ThemeRole::TextMuted),
    };
    div()
        .id(("settings-row", row_id(row)))
        .h(px(72.))
        .w_full()
        .flex()
        .items_center()
        .gap_3()
        .px_5()
        .border_b_1()
        .border_color(palette.color(ThemeRole::BorderSubtle))
        .bg(if active == Some(row) {
            palette.color(ThemeRole::Panel)
        } else {
            palette.color(ThemeRole::Raised)
        })
        .hover({
            let hover = palette.color(ThemeRole::StateRowHover);
            move |row| row.bg(hover)
        })
        .on_click(move |_, _, cx| {
            let _ = select_view.update(cx, |this, cx| this.select_row(row, cx));
        })
        .when_some(
            match row {
                RowRef::Theme(role) => Some(draft.theme.color(role).packed()),
                _ => None,
            },
            |row, color| {
                row.child(
                    div()
                        .size(px(30.))
                        .flex_none()
                        .rounded_md()
                        .border_1()
                        .border_color(palette.color(ThemeRole::BorderFocus))
                        .bg(rgba(color)),
                )
            },
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .child(
                    div()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(row_label.to_string()),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(palette.color(ThemeRole::TextDim))
                        .child(path_text),
                )
                .when_some(help(row), |column, help| {
                    column.child(
                        div()
                            .text_xs()
                            .truncate()
                            .text_color(palette.color(ThemeRole::TextSubtle))
                            .child(help),
                    )
                }),
        )
        .child(
            div()
                .max_w(px(330.))
                .truncate()
                .text_sm()
                .text_color(diagnostic_color)
                .child(row_value),
        )
        .when(!matches!(row, RowRef::Diagnostic(_)), |row_element| {
            row_element.child(
                setting_button(("reset-row", row_id(row)), "Reset", false, palette).on_click(
                    move |_, _, cx| {
                        let _ = reset_view.update(cx, |this, cx| this.reset_row(row, cx));
                    },
                ),
            )
        })
        .into_any_element()
}

fn render_diagnostic_row(
    index: usize,
    diagnostics: &[ConfigDiagnostic],
    draft: &GuiConfig,
    palette: &ThemePalette,
    source: Option<&str>,
) -> AnyElement {
    let Some(diagnostic) = diagnostics.get(index) else {
        return div().into_any_element();
    };
    let excerpt = source.and_then(|source| diagnostic.source_excerpt(source));
    let color = match diagnostic.severity {
        DiagnosticSeverity::Warning => palette.color(ThemeRole::StateWarning),
        DiagnosticSeverity::Error => palette.color(ThemeRole::StateDanger),
    };
    let location = excerpt
        .as_ref()
        .map(|excerpt| format!("line {}, column {}", excerpt.line, excerpt.column));
    let marker = excerpt.as_ref().map(|excerpt| {
        format!(
            "{}{}",
            " ".repeat(excerpt.marker_start),
            "^".repeat(excerpt.marker_len)
        )
    });
    div()
        .id(("settings-diagnostic", index))
        .h(px(104.))
        .w_full()
        .flex()
        .items_center()
        .gap_4()
        .px_5()
        .border_b_1()
        .border_color(palette.color(ThemeRole::BorderSubtle))
        .bg(palette.color(ThemeRole::Raised))
        .child(
            div()
                .w(px(220.))
                .flex_none()
                .min_w_0()
                .child(
                    div()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(color)
                        .child(diagnostic.path.clone()),
                )
                .when_some(location, |column, location| {
                    column.child(
                        div()
                            .text_xs()
                            .text_color(palette.color(ThemeRole::TextDim))
                            .child(location),
                    )
                }),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .child(
                    div()
                        .text_sm()
                        .text_color(color)
                        .child(diagnostic.message.clone()),
                )
                .when_some(excerpt, |column, excerpt| {
                    column.child(
                        div()
                            .mt_1()
                            .min_w_0()
                            .truncate()
                            .font_family(draft.fonts.code_family.clone())
                            .text_xs()
                            .text_color(palette.color(ThemeRole::TextSecondary))
                            .child(excerpt.line_text.to_owned()),
                    )
                })
                .when_some(marker, |column, marker| {
                    column.child(
                        div()
                            .min_w_0()
                            .truncate()
                            .font_family(draft.fonts.code_family.clone())
                            .text_xs()
                            .text_color(color)
                            .child(marker),
                    )
                }),
        )
        .into_any_element()
}

fn row_id(row: RowRef) -> usize {
    match row {
        RowRef::Theme(role) => role as usize,
        RowRef::FontFamily(role) => 100 + role as usize,
        RowRef::FontSize(role) => 110 + role as usize,
        RowRef::Choice(setting) => 120 + setting as usize,
        RowRef::Binding(scope, command) => {
            200 + key_bindings::BINDINGS
                .iter()
                .position(|binding| binding.scope == scope && binding.command == command)
                .unwrap_or_default()
        }
        RowRef::Diagnostic(index) => 1000 + index,
    }
}

fn setting_button(
    id: impl Into<gpui::ElementId>,
    label: impl Into<SharedString>,
    selected: bool,
    palette: &ThemePalette,
) -> gpui::Stateful<Div> {
    div()
        .id(id)
        .px_3()
        .py_2()
        .rounded_md()
        .cursor_pointer()
        .bg(if selected {
            palette.color(ThemeRole::ControlActive)
        } else {
            palette.color(ThemeRole::ControlSurface)
        })
        .text_color(if selected {
            palette.color(ThemeRole::ControlActiveText)
        } else {
            palette.color(ThemeRole::TextSecondary)
        })
        .hover({
            let hover = palette.color(ThemeRole::ControlSurfaceHover);
            move |button| button.bg(hover)
        })
        .child(label.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_settings(cx: &gpui::TestAppContext) -> Entity<SettingsView> {
        cx.update(|cx| {
            crate::fonts::init(cx);
            let config = GuiConfig::default();
            let available_families = cx.text_system().all_font_names();
            theme::apply_appearance(&config, SourceStatus::Missing, &[], &available_families, cx);
            install_loaded(
                LoadedConfig {
                    path: None,
                    config,
                    source: None,
                    status: SourceStatus::Missing,
                    diagnostics: Vec::new(),
                },
                cx,
            );
            cx.new(SettingsView::new)
        })
    }

    #[gpui::test]
    fn successful_save_commits_the_written_snapshot_and_keeps_newer_edits_dirty(
        cx: &mut gpui::TestAppContext,
    ) {
        let settings = create_settings(cx);
        let mut saved = GuiConfig::default();
        saved.fonts.interface_size = 18.0;
        let source = toml_spanner::to_string(&saved).unwrap().into_bytes();

        cx.update(|cx| {
            let bindings = key_bindings::compile(&saved, cx).unwrap();
            settings.update(cx, |settings, cx| {
                settings.pending_save = Some(PendingSave {
                    config: saved.clone(),
                    bindings,
                });
                settings.saving = true;
                settings.draft.fonts.interface_size = 20.0;
                settings.finish_save(Ok(source), cx);
            });
        });

        cx.read(|cx| {
            let settings = settings.read(cx);
            assert_eq!(settings.baseline.fonts.interface_size, 18.0);
            assert_eq!(settings.draft.fonts.interface_size, 20.0);
            assert_eq!(settings.committed.fonts.interface_size, 18.0);
            assert!(settings.dirty());
            assert_eq!(
                cx.global::<ConfigurationState>()
                    .0
                    .config
                    .fonts
                    .interface_size,
                18.0
            );
            assert_eq!(AppliedSettings::get(cx).fonts.interface_size, 20.0);
        });
    }

    #[gpui::test]
    fn invalid_active_editor_value_blocks_save(cx: &mut gpui::TestAppContext) {
        let settings = create_settings(cx);

        cx.update(|cx| {
            settings.update(cx, |settings, cx| {
                settings.editor_error = Some("invalid color".into());
                settings.save(false, cx);
                assert!(settings.pending_save.is_none());
                assert!(!settings.saving);
                assert!(
                    settings
                        .status_message
                        .as_ref()
                        .is_some_and(|message| message.contains("invalid editor value"))
                );
            });
        });
    }

    #[gpui::test]
    fn appearance_preview_preserves_the_committed_binding_mode(cx: &mut gpui::TestAppContext) {
        let settings = create_settings(cx);

        cx.update(|cx| {
            settings.update(cx, |settings, cx| {
                assert_eq!(settings.committed.binding_mode, BindingMode::Vim);
                settings.draft.input.default_binding_mode = BindingMode::Standard;
                settings
                    .draft
                    .theme
                    .set_color(ThemeRole::Window, Rgba8::rgb(1, 2, 3));
                settings.preview(cx);
            });
            assert_eq!(AppliedSettings::get(cx).binding_mode, BindingMode::Vim);
        });
    }

    #[gpui::test]
    fn save_conflict_enables_confirmed_replacement(cx: &mut gpui::TestAppContext) {
        let settings = create_settings(cx);

        cx.update(|cx| {
            settings.update(cx, |settings, cx| {
                settings.saving = true;
                settings.finish_save(Err(SaveError::Conflict), cx);
                assert!(settings.confirm_replace);
                assert!(settings.pending_save.is_none());
            });
        });
    }

    #[gpui::test]
    fn reload_does_not_discard_edits_made_while_reading(cx: &mut gpui::TestAppContext) {
        let settings = create_settings(cx);
        let mut loaded_config = GuiConfig::default();
        loaded_config.fonts.interface_size = 18.0;

        cx.update(|cx| {
            settings.update(cx, |settings, cx| {
                settings.pending_reload_draft = Some(settings.draft.clone());
                settings.saving = true;
                settings.draft.fonts.interface_size = 20.0;
                settings.finish_reload(
                    LoadedConfig {
                        path: None,
                        config: loaded_config,
                        source: None,
                        status: SourceStatus::Loaded,
                        diagnostics: Vec::new(),
                    },
                    cx,
                );
                assert_eq!(settings.draft.fonts.interface_size, 20.0);
                assert_ne!(settings.baseline.fonts.interface_size, 18.0);
            });
        });
    }
}
