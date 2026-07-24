mod catalog;

use std::sync::Arc;

use gpui::{
    AnyElement, App, Context, Div, Entity, EventEmitter, FocusHandle, Focusable, FontWeight,
    Global, KeyBinding, KeyDownEvent, Render, SharedString, Subscription, Task,
    UniformListScrollHandle, WeakEntity, div, prelude::*, px, rgba, uniform_list,
};

use crate::{
    composer::{ComposerChanged, Mode, TextEditor},
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SettingsFocus {
    Search,
    Row(RowRef),
    ResetAll,
    ResetSection,
    Reload,
    Cancel,
    Save,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EditorTarget {
    Search,
    Row(RowRef),
}

struct ActiveEditor {
    target: EditorTarget,
    entity: Entity<TextEditor>,
    _subscription: Subscription,
}

#[derive(Clone, PartialEq, Eq)]
struct InvalidEdit {
    row: RowRef,
    text: String,
    error: SharedString,
}

#[derive(Clone)]
enum RowAction {
    Reset,
    Record(BindingScope, BindCommand),
    Font(FontRole, String),
}

pub(crate) struct SettingsView {
    focus: FocusHandle,
    focused: SettingsFocus,
    editor: Option<ActiveEditor>,
    invalid_edits: Vec<InvalidEdit>,
    action_menu: Option<(RowRef, usize)>,
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
    status_message: Option<SharedString>,
    saving: bool,
    confirm_reload: bool,
    confirm_replace: bool,
    pending_save: Option<PendingSave>,
    pending_reload_draft: Option<GuiConfig>,
    pending_reload_invalid_edits: Option<Vec<InvalidEdit>>,
    recording: Option<(BindingScope, BindCommand)>,
    key_interceptor: Option<Subscription>,
    scroll: UniformListScrollHandle,
    _save_task: Option<Task<()>>,
}

impl EventEmitter<SettingsClosed> for SettingsView {}

impl SettingsView {
    pub(crate) fn new(cx: &mut Context<Self>) -> Self {
        let loaded = cx.global::<ConfigurationState>().0.clone();
        let committed = AppliedSettings::get(cx);
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

        let first_row = rows(&SETTINGS_SECTIONS[0], loaded.diagnostics.len())
            .into_iter()
            .find(|row| !matches!(row, RowRef::Diagnostic(_)))
            .expect("appearance settings contain an editable row");
        let mut this = Self {
            focus: cx.focus_handle(),
            focused: SettingsFocus::Row(first_row),
            editor: None,
            invalid_edits: Vec::new(),
            action_menu: None,
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
            status_message: None,
            saving: false,
            confirm_reload: false,
            confirm_replace: false,
            pending_save: None,
            pending_reload_draft: None,
            pending_reload_invalid_edits: None,
            recording: None,
            key_interceptor: None,
            scroll: UniformListScrollHandle::new(),
            _save_task: None,
        };
        this.materialize_editor(EditorTarget::Row(first_row), cx);
        this
    }

    fn dirty(&self) -> bool {
        self.draft != self.baseline || !self.invalid_edits.is_empty()
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

    fn invalid_edit(&self, row: RowRef) -> Option<&InvalidEdit> {
        self.invalid_edits.iter().find(|edit| edit.row == row)
    }

    fn clear_invalid_edit(&mut self, row: RowRef) {
        self.invalid_edits.retain(|edit| edit.row != row);
    }

    fn set_invalid_edit(&mut self, row: RowRef, text: &str, error: String) {
        let error: SharedString = error.into();
        if let Some(edit) = self.invalid_edits.iter_mut().find(|edit| edit.row == row) {
            edit.text.clear();
            edit.text.push_str(text);
            edit.error = error.clone();
        } else {
            self.invalid_edits.push(InvalidEdit {
                row,
                text: text.to_string(),
                error: error.clone(),
            });
        }
    }

    fn row_has_editor(row: RowRef) -> bool {
        matches!(
            row,
            RowRef::Theme(_) | RowRef::FontFamily(_) | RowRef::FontSize(_) | RowRef::Binding(_, _)
        )
    }

    fn editor_value(&self, target: EditorTarget) -> String {
        match target {
            EditorTarget::Search => self.query.clone(),
            EditorTarget::Row(row) => self
                .invalid_edit(row)
                .map(|edit| edit.text.clone())
                .unwrap_or_else(|| self.edit_text(row)),
        }
    }

    fn materialize_editor(&mut self, target: EditorTarget, cx: &mut Context<Self>) {
        if self
            .editor
            .as_ref()
            .is_some_and(|editor| editor.target == target)
        {
            return;
        }
        self.editor = None;
        let value = self.editor_value(target);
        let binding_mode = self.draft.input.default_binding_mode;
        let placeholder = match target {
            EditorTarget::Search => "Search settings",
            EditorTarget::Row(_) => "Edit value",
        };
        let entity = cx.new(|cx| {
            let mut editor = TextEditor::settings_input(placeholder, binding_mode, cx);
            editor.set_value(value, cx);
            editor
        });
        let subscription = cx.subscribe(&entity, move |this, editor, _: &ComposerChanged, cx| {
            let value = editor.read(cx).text();
            match target {
                EditorTarget::Search => {
                    this.query = value;
                    this.scroll.scroll_to_item(0, gpui::ScrollStrategy::Top);
                    this.reconcile_focus_after_filter(cx);
                }
                EditorTarget::Row(row) if this.focused == SettingsFocus::Row(row) => {
                    this.apply_editor_text(&value, cx);
                }
                EditorTarget::Row(_) => {}
            }
        });
        self.editor = Some(ActiveEditor {
            target,
            entity,
            _subscription: subscription,
        });
    }

    fn focus_order(&self) -> Vec<SettingsFocus> {
        let mut order = Vec::with_capacity(self.visible_rows().len() + 7);
        order.push(SettingsFocus::Search);
        order.extend(
            self.visible_rows()
                .into_iter()
                .filter(|row| !matches!(row, RowRef::Diagnostic(_)))
                .map(SettingsFocus::Row),
        );
        order.extend([
            SettingsFocus::ResetAll,
            SettingsFocus::ResetSection,
            SettingsFocus::Reload,
            SettingsFocus::Cancel,
            SettingsFocus::Save,
        ]);
        order
    }

    fn focus_target(
        &mut self,
        target: SettingsFocus,
        enter_insert: bool,
        window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        self.action_menu = None;
        self.focused = target;
        match target {
            SettingsFocus::Search => {
                self.materialize_editor(EditorTarget::Search, cx);
                if enter_insert {
                    if let Some(editor) = &self.editor {
                        editor
                            .entity
                            .update(cx, |editor, cx| editor.enter_insert_mode(cx));
                    }
                }
                if let Some(editor) = &self.editor {
                    window.focus(&editor.entity.focus_handle(cx), cx);
                }
            }
            SettingsFocus::Row(row) if Self::row_has_editor(row) => {
                self.materialize_editor(EditorTarget::Row(row), cx);
                if enter_insert {
                    if let Some(editor) = &self.editor {
                        editor
                            .entity
                            .update(cx, |editor, cx| editor.enter_insert_mode(cx));
                    }
                }
                if let Some(index) = self
                    .visible_rows()
                    .iter()
                    .position(|candidate| *candidate == row)
                {
                    self.scroll
                        .scroll_to_item(index, gpui::ScrollStrategy::Center);
                }
                if let Some(editor) = &self.editor {
                    window.focus(&editor.entity.focus_handle(cx), cx);
                }
            }
            SettingsFocus::Row(row) => {
                self.editor = None;
                if let Some(index) = self
                    .visible_rows()
                    .iter()
                    .position(|candidate| *candidate == row)
                {
                    self.scroll
                        .scroll_to_item(index, gpui::ScrollStrategy::Center);
                }
                window.focus(&self.focus, cx);
            }
            _ => {
                self.editor = None;
                window.focus(&self.focus, cx);
            }
        }
        cx.notify();
    }

    fn move_focus(
        &mut self,
        delta: isize,
        enter_insert: bool,
        window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        let order = self.focus_order();
        if order.is_empty() {
            return;
        }
        let current = order
            .iter()
            .position(|target| *target == self.focused)
            .unwrap_or(0);
        let next = (current as isize + delta).rem_euclid(order.len() as isize) as usize;
        self.focus_target(order[next], enter_insert, window, cx);
    }

    fn reconcile_focus_after_filter(&mut self, cx: &mut Context<Self>) {
        let order = self.focus_order();
        if order.contains(&self.focused) {
            cx.notify();
            return;
        }
        self.focused = order
            .iter()
            .copied()
            .find(|target| matches!(target, SettingsFocus::Row(_)))
            .unwrap_or(SettingsFocus::Search);
        self.editor = None;
        match self.focused {
            SettingsFocus::Search => self.materialize_editor(EditorTarget::Search, cx),
            SettingsFocus::Row(row) if Self::row_has_editor(row) => {
                self.materialize_editor(EditorTarget::Row(row), cx)
            }
            _ => {}
        }
        cx.notify();
    }

    fn select_section(&mut self, index: usize, window: &mut gpui::Window, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        self.active_section = index;
        self.scroll.scroll_to_item(0, gpui::ScrollStrategy::Top);
        let target = if matches!(
            self.focused,
            SettingsFocus::Search
                | SettingsFocus::ResetAll
                | SettingsFocus::ResetSection
                | SettingsFocus::Reload
                | SettingsFocus::Cancel
                | SettingsFocus::Save
        ) {
            self.focused
        } else {
            self.visible_rows()
                .into_iter()
                .find(|row| !matches!(row, RowRef::Diagnostic(_)))
                .map(SettingsFocus::Row)
                .unwrap_or(SettingsFocus::Search)
        };
        self.focus_target(target, false, window, cx);
    }

    fn select_row(&mut self, row: RowRef, window: &mut gpui::Window, cx: &mut Context<Self>) {
        if self.saving || matches!(row, RowRef::Diagnostic(_)) {
            return;
        }
        self.focus_target(SettingsFocus::Row(row), false, window, cx);
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

    fn sync_row_editor(&mut self, row: RowRef, cx: &mut Context<Self>) {
        let entity = self
            .editor
            .as_ref()
            .filter(|editor| editor.target == EditorTarget::Row(row))
            .map(|editor| editor.entity.clone());
        if let Some(entity) = entity {
            let value = self.editor_value(EditorTarget::Row(row));
            entity.update(cx, |editor, cx| editor.set_value(value, cx));
        }
    }

    fn sync_editor_binding_mode(&mut self, cx: &mut Context<Self>) {
        let entity = self.editor.as_ref().map(|editor| editor.entity.clone());
        if let Some(entity) = entity {
            let binding_mode = self.draft.input.default_binding_mode;
            entity.update(cx, |editor, cx| editor.set_binding_mode(binding_mode, cx));
        }
    }

    fn apply_editor_text(&mut self, text: &str, cx: &mut Context<Self>) {
        let SettingsFocus::Row(row) = self.focused else {
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
                self.clear_invalid_edit(row);
                if changed
                    && matches!(
                        row,
                        RowRef::Theme(_) | RowRef::FontFamily(_) | RowRef::FontSize(_)
                    )
                {
                    self.preview(cx);
                }
            }
            Err(error) => self.set_invalid_edit(row, text, error),
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
                self.sync_editor_binding_mode(cx);
            }
        }
        cx.notify();
    }

    fn choose_font(&mut self, role: FontRole, family: String, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        let changed = self.draft.fonts.family(role) != family;
        let row = RowRef::FontFamily(role);
        self.draft.fonts.set_family(role, family);
        self.clear_invalid_edit(row);
        self.sync_row_editor(row, cx);
        self.action_menu = None;
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
        if row == RowRef::Choice(ScalarSetting::BindingMode) {
            self.sync_editor_binding_mode(cx);
        }
        self.clear_invalid_edit(row);
        self.sync_row_editor(row, cx);
        self.action_menu = None;
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
            self.clear_invalid_edit(row);
        }
        self.sync_editor_binding_mode(cx);
        if let SettingsFocus::Row(row) = self.focused {
            self.sync_row_editor(row, cx);
        }
        self.action_menu = None;
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
        self.invalid_edits.clear();
        self.sync_editor_binding_mode(cx);
        if let SettingsFocus::Row(row) = self.focused {
            self.sync_row_editor(row, cx);
        }
        self.action_menu = None;
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
                self.status_message = Some(
                    format!(
                        "Recorded chord conflicts with another action: {}",
                        error.message
                    )
                    .into(),
                );
                cx.notify();
                return;
            }
            self.draft = candidate;
            let row = RowRef::Binding(scope, command);
            self.clear_invalid_edit(row);
            self.sync_row_editor(row, cx);
            self.action_menu = None;
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
        if !self.invalid_edits.is_empty() {
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
        self.pending_reload_invalid_edits = Some(self.invalid_edits.clone());
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
        let invalid_edits_at_start = self
            .pending_reload_invalid_edits
            .take()
            .expect("reload retains invalid edits present when it started");
        if self.draft != draft_at_start || self.invalid_edits != invalid_edits_at_start {
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
                    self.invalid_edits.clear();
                    self.action_menu = None;
                    self.sync_editor_binding_mode(cx);
                    if let SettingsFocus::Row(row) = self.focused {
                        self.sync_row_editor(row, cx);
                    }
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

    fn choice_value(&self, setting: ScalarSetting) -> usize {
        match setting {
            ScalarSetting::FontRendering => match self.draft.fonts.rendering {
                FontRendering::PlatformDefault => 0,
                FontRendering::Subpixel => 1,
                FontRendering::Grayscale => 2,
            },
            ScalarSetting::BindingMode => {
                usize::from(self.draft.input.default_binding_mode == BindingMode::Vim)
            }
        }
    }

    fn cycle_choice(&mut self, setting: ScalarSetting, delta: isize, cx: &mut Context<Self>) {
        let count = match setting {
            ScalarSetting::FontRendering => 3,
            ScalarSetting::BindingMode => 2,
        };
        let next = (self.choice_value(setting) as isize + delta).rem_euclid(count) as usize;
        self.choose(setting, next, cx);
    }

    fn row_actions(&self, row: RowRef) -> Vec<(SharedString, RowAction)> {
        let mut actions = vec![("Reset".into(), RowAction::Reset)];
        match row {
            RowRef::Binding(scope, command) => actions.push((
                if self.recording == Some((scope, command)) {
                    "Recording…".into()
                } else {
                    "Record one chord".into()
                },
                RowAction::Record(scope, command),
            )),
            RowRef::FontFamily(role) => {
                let query = self
                    .editor_value(EditorTarget::Row(row))
                    .trim()
                    .to_ascii_lowercase();
                actions.extend(
                    self.available_families
                        .iter()
                        .filter(|family| {
                            query.is_empty() || family.to_ascii_lowercase().contains(&query)
                        })
                        .take(8)
                        .cloned()
                        .map(|family| {
                            (
                                SharedString::from(family.clone()),
                                RowAction::Font(role, family),
                            )
                        }),
                );
            }
            _ => {}
        }
        actions
    }

    fn run_row_action(&mut self, row: RowRef, index: usize, cx: &mut Context<Self>) {
        let Some((_, action)) = self.row_actions(row).get(index).cloned() else {
            return;
        };
        match action {
            RowAction::Reset => self.reset_row(row, cx),
            RowAction::Record(scope, command) => self.start_recording(scope, command, cx),
            RowAction::Font(role, family) => self.choose_font(role, family, cx),
        }
    }

    fn activate_focused(&mut self, window: &mut gpui::Window, cx: &mut Context<Self>) {
        match self.focused {
            SettingsFocus::Search => self.focus_target(SettingsFocus::Search, true, window, cx),
            SettingsFocus::Row(RowRef::Choice(setting)) => self.cycle_choice(setting, 1, cx),
            SettingsFocus::Row(row) if Self::row_has_editor(row) => {
                self.focus_target(SettingsFocus::Row(row), true, window, cx)
            }
            SettingsFocus::Row(_) => {}
            SettingsFocus::ResetAll => self.reset_all(cx),
            SettingsFocus::ResetSection => self.reset_section(cx),
            SettingsFocus::Reload => self.reload(cx),
            SettingsFocus::Cancel => self.cancel(cx),
            SettingsFocus::Save => {
                let replace = self.confirm_replace
                    || matches!(
                        self.source_status,
                        SourceStatus::Invalid | SourceStatus::ReadFailed
                    );
                self.save(replace, cx);
            }
        }
    }

    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        if self.saving || self.recording.is_some() {
            return;
        }
        let key = event.keystroke.key.as_str();
        let modifiers = event.keystroke.modifiers;

        if key == "tab" {
            let delta = if modifiers.shift { -1 } else { 1 };
            let next = (self.active_section as isize + delta)
                .rem_euclid(SETTINGS_SECTIONS.len() as isize) as usize;
            self.select_section(next, window, cx);
            window.prevent_default();
            cx.stop_propagation();
            return;
        }

        if let Some((row, selected)) = self.action_menu {
            match key {
                "escape" => {
                    self.action_menu = None;
                    cx.notify();
                }
                "j" | "down" => {
                    let count = self.row_actions(row).len();
                    self.action_menu = Some((row, (selected + 1) % count.max(1)));
                    cx.notify();
                }
                "k" | "up" => {
                    let count = self.row_actions(row).len().max(1);
                    self.action_menu = Some((row, (selected + count - 1) % count));
                    cx.notify();
                }
                "enter" => self.run_row_action(row, selected, cx),
                _ => return,
            }
            window.prevent_default();
            cx.stop_propagation();
            return;
        }

        if key == "enter" && modifiers.secondary() {
            if let SettingsFocus::Row(row) = self.focused {
                if !matches!(row, RowRef::Diagnostic(_)) {
                    self.action_menu = Some((row, 0));
                    cx.notify();
                    window.prevent_default();
                    cx.stop_propagation();
                }
            }
            return;
        }

        let binding_mode = self.draft.input.default_binding_mode;
        let editor_mode = self
            .editor
            .as_ref()
            .map(|editor| editor.entity.read(cx).mode());
        let editor_focused = matches!(
            (
                self.focused,
                self.editor.as_ref().map(|editor| editor.target)
            ),
            (SettingsFocus::Search, Some(EditorTarget::Search))
                | (SettingsFocus::Row(_), Some(EditorTarget::Row(_)))
        );

        if editor_focused && binding_mode == BindingMode::Vim {
            match editor_mode {
                Some(Mode::Normal) => match key {
                    "j" | "down" => self.move_focus(1, false, window, cx),
                    "k" | "up" => self.move_focus(-1, false, window, cx),
                    "enter" => self.activate_focused(window, cx),
                    "escape" => self.cancel(cx),
                    _ => return,
                },
                Some(Mode::Insert) if key == "enter" => self.move_focus(1, true, window, cx),
                Some(Mode::Insert | Mode::Visual | Mode::VisualLine | Mode::VisualBlock) => return,
                None => return,
            }
        } else if editor_focused {
            match key {
                "down" => self.move_focus(1, false, window, cx),
                "up" => self.move_focus(-1, false, window, cx),
                "enter" => self.move_focus(1, false, window, cx),
                "escape" => self.cancel(cx),
                _ => return,
            }
        } else {
            match key {
                "j" | "down" if binding_mode == BindingMode::Vim => {
                    self.move_focus(1, false, window, cx)
                }
                "k" | "up" if binding_mode == BindingMode::Vim => {
                    self.move_focus(-1, false, window, cx)
                }
                "down" => self.move_focus(1, false, window, cx),
                "up" => self.move_focus(-1, false, window, cx),
                "h" if binding_mode == BindingMode::Vim => {
                    if let SettingsFocus::Row(RowRef::Choice(setting)) = self.focused {
                        self.cycle_choice(setting, -1, cx);
                    } else {
                        return;
                    }
                }
                "left" => {
                    if let SettingsFocus::Row(RowRef::Choice(setting)) = self.focused {
                        self.cycle_choice(setting, -1, cx);
                    } else {
                        return;
                    }
                }
                "l" if binding_mode == BindingMode::Vim => {
                    if let SettingsFocus::Row(RowRef::Choice(setting)) = self.focused {
                        self.cycle_choice(setting, 1, cx);
                    } else {
                        return;
                    }
                }
                "right" => {
                    if let SettingsFocus::Row(RowRef::Choice(setting)) = self.focused {
                        self.cycle_choice(setting, 1, cx);
                    } else {
                        return;
                    }
                }
                "enter" => self.activate_focused(window, cx),
                "i" if binding_mode == BindingMode::Vim => self.activate_focused(window, cx),
                "escape" => self.cancel(cx),
                _ => return,
            }
        }
        window.prevent_default();
        cx.stop_propagation();
    }

    fn render_action_menu(&self, row: RowRef, selected: usize, view: WeakEntity<Self>) -> Div {
        let palette = ThemePalette::from_config(&self.draft.theme);
        let mut menu = div()
            .flex_none()
            .flex()
            .items_center()
            .gap_2()
            .px_5()
            .py_2()
            .border_b_1()
            .border_color(palette.color(ThemeRole::BorderSubtle))
            .bg(palette.color(ThemeRole::Window))
            .child(
                div()
                    .text_xs()
                    .text_color(palette.color(ThemeRole::TextMuted))
                    .child(format!("{} actions", label(row))),
            );
        for (index, (title, _)) in self.row_actions(row).into_iter().enumerate() {
            let action_view = view.clone();
            menu = menu.child(
                setting_button(
                    ("row-action", row_id(row) * 16 + index),
                    title,
                    index == selected,
                    &palette,
                )
                .on_click(move |_, _, cx| {
                    let _ = action_view.update(cx, |this, cx| this.run_row_action(row, index, cx));
                }),
            );
        }
        menu.child(
            div()
                .ml_auto()
                .text_xs()
                .text_color(palette.color(ThemeRole::TextDim))
                .child("⌘/Ctrl+Enter · j/k · Enter · Esc"),
        )
    }
}

impl Render for SettingsView {
    fn render(&mut self, window: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity().downgrade();
        let palette = ThemePalette::from_config(&self.draft.theme);
        let section = self.section();
        let visible_rows = self.visible_rows();
        let focused = self.focused;
        let draft = self.draft.clone();
        let diagnostics = self.diagnostics.clone();
        let invalid_edits = self.invalid_edits.clone();
        let active_editor = self
            .editor
            .as_ref()
            .map(|editor| (editor.target, editor.entity.clone()));
        let row_editor = active_editor.clone();
        let diagnostic_source = self
            .source
            .as_deref()
            .and_then(|source| std::str::from_utf8(source).ok())
            .map(ToOwned::to_owned);
        let row_view = view.clone();
        let row_palette = palette.clone();
        let list_rows = visible_rows.clone();
        let list = uniform_list(
            ("settings-rows", self.active_section),
            list_rows.len(),
            move |range, _, _| {
                range
                    .map(|index| {
                        let row = list_rows[index];
                        let editor = row_editor.as_ref().and_then(|(target, entity)| {
                            (*target == EditorTarget::Row(row)).then(|| entity.clone())
                        });
                        let invalid = invalid_edits.iter().find(|edit| edit.row == row);
                        render_row(
                            row,
                            &draft,
                            &diagnostics,
                            focused == SettingsFocus::Row(row),
                            editor,
                            invalid.map(|edit| edit.text.clone()),
                            invalid.map(|edit| edit.error.clone()),
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
            let section_view = view.clone();
            navigation = navigation.child(
                setting_button(candidate.id, candidate.title, selected, &palette)
                    .w_full()
                    .on_click(move |_, window, cx| {
                        let _ = section_view
                            .update(cx, |this, cx| this.select_section(index, window, cx));
                    }),
            );
        }
        let reset_all_view = view.clone();
        navigation = navigation.child(div().flex_1()).child(
            setting_button(
                "reset-all",
                "Reset all",
                focused == SettingsFocus::ResetAll,
                &palette,
            )
            .on_click(move |_, window, cx| {
                let _ = reset_all_view.update(cx, |this, cx| {
                    this.focus_target(SettingsFocus::ResetAll, false, window, cx);
                    this.reset_all(cx);
                });
            }),
        );

        let search_editor = active_editor
            .as_ref()
            .and_then(|(target, entity)| (*target == EditorTarget::Search).then(|| entity.clone()));
        let search_view = view.clone();
        let search = div()
            .id("settings-search")
            .w(px(360.))
            .min_h(px(38.))
            .flex()
            .items_center()
            .px_3()
            .py_2()
            .rounded_md()
            .border_1()
            .border_color(if focused == SettingsFocus::Search {
                palette.color(ThemeRole::BorderFocus)
            } else {
                palette.color(ThemeRole::BorderStrong)
            })
            .bg(palette.color(ThemeRole::Input))
            .cursor_text()
            .on_click(move |_, window, cx| {
                let _ = search_view.update(cx, |this, cx| {
                    this.focus_target(SettingsFocus::Search, false, window, cx)
                });
            })
            .when_some(search_editor, |search, editor| search.child(editor))
            .when(
                active_editor
                    .as_ref()
                    .is_none_or(|(target, _)| *target != EditorTarget::Search),
                |search| {
                    search.child(
                        div()
                            .text_sm()
                            .text_color(if self.query.is_empty() {
                                palette.color(ThemeRole::TextDim)
                            } else {
                                palette.color(ThemeRole::TextPrimary)
                            })
                            .child(if self.query.is_empty() {
                                "Search settings".to_string()
                            } else {
                                self.query.clone()
                            }),
                    )
                },
            );

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
            .capture_key_down(cx.listener(Self::handle_key_down))
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
                            .child(search)
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
                                .when_some(self.action_menu, |content, (row, selected)| {
                                    content.child(self.render_action_menu(
                                        row,
                                        selected,
                                        view.clone(),
                                    ))
                                })
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
                                setting_button(
                                    "reset-section",
                                    "Reset section",
                                    focused == SettingsFocus::ResetSection,
                                    &palette,
                                )
                                .on_click(move |_, window, cx| {
                                    let _ = reset_section_view.update(cx, |this, cx| {
                                        this.focus_target(
                                            SettingsFocus::ResetSection,
                                            false,
                                            window,
                                            cx,
                                        );
                                        this.reset_section(cx);
                                    });
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
                                    focused == SettingsFocus::Reload || self.confirm_reload,
                                    &palette,
                                )
                                .on_click(move |_, window, cx| {
                                    let _ = reload_view.update(cx, |this, cx| {
                                        this.focus_target(SettingsFocus::Reload, false, window, cx);
                                        this.reload(cx);
                                    });
                                }),
                            )
                            .child(
                                setting_button(
                                    "cancel-settings",
                                    "Cancel",
                                    focused == SettingsFocus::Cancel,
                                    &palette,
                                )
                                .on_click(move |_, window, cx| {
                                    let _ = cancel_view.update(cx, |this, cx| {
                                        this.focus_target(SettingsFocus::Cancel, false, window, cx);
                                        this.cancel(cx);
                                    });
                                }),
                            )
                            .child(
                                setting_button(
                                    "save-settings",
                                    save_label,
                                    focused == SettingsFocus::Save,
                                    &palette,
                                )
                                .on_click(move |_, window, cx| {
                                    let _ = save_view.update(cx, |this, cx| {
                                        this.focus_target(SettingsFocus::Save, false, window, cx);
                                        this.save(replace, cx);
                                    });
                                }),
                            ),
                    ),
            )
    }
}

impl Focusable for SettingsView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor
            .as_ref()
            .map(|editor| editor.entity.focus_handle(cx))
            .unwrap_or_else(|| self.focus.clone())
    }
}

fn render_row(
    row: RowRef,
    draft: &GuiConfig,
    diagnostics: &[ConfigDiagnostic],
    focused: bool,
    editor: Option<Entity<TextEditor>>,
    invalid_text: Option<String>,
    editor_error: Option<SharedString>,
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
    let row_value = invalid_text.unwrap_or_else(|| match row {
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
    });
    let path_text = path(row);
    let reset_view = view.clone();
    let select_view = view.clone();
    let choice_view = view.clone();
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
    let value = if let Some(editor) = editor {
        div()
            .w(px(330.))
            .min_h(px(36.))
            .flex()
            .items_center()
            .px_3()
            .py_2()
            .rounded_md()
            .border_1()
            .border_color(if editor_error.is_some() {
                palette.color(ThemeRole::StateDanger)
            } else {
                palette.color(ThemeRole::BorderFocus)
            })
            .bg(palette.color(ThemeRole::Input))
            .child(editor)
            .into_any_element()
    } else if let RowRef::Choice(setting) = row {
        div()
            .id(("settings-choice", setting as usize))
            .max_w(px(330.))
            .px_3()
            .py_2()
            .rounded_md()
            .cursor_pointer()
            .bg(palette.color(ThemeRole::ControlSurface))
            .text_sm()
            .text_color(palette.color(ThemeRole::TextSecondary))
            .child(format!("‹  {row_value}  ›"))
            .on_click(move |_, window, cx| {
                let _ = choice_view.update(cx, |this, cx| {
                    this.focus_target(SettingsFocus::Row(row), false, window, cx);
                    this.cycle_choice(setting, 1, cx);
                });
            })
            .into_any_element()
    } else {
        div()
            .max_w(px(330.))
            .truncate()
            .text_sm()
            .text_color(diagnostic_color)
            .child(row_value)
            .into_any_element()
    };
    div()
        .id(("settings-row", row_id(row)))
        .h(px(84.))
        .w_full()
        .flex()
        .items_center()
        .gap_3()
        .px_5()
        .border_b_1()
        .border_color(palette.color(ThemeRole::BorderSubtle))
        .bg(if focused {
            palette.color(ThemeRole::Panel)
        } else {
            palette.color(ThemeRole::Raised)
        })
        .hover({
            let hover = palette.color(ThemeRole::StateRowHover);
            move |row| row.bg(hover)
        })
        .on_click(move |_, window, cx| {
            let _ = select_view.update(cx, |this, cx| this.select_row(row, window, cx));
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
                .w(px(330.))
                .flex_none()
                .flex()
                .flex_col()
                .child(value)
                .when_some(editor_error, |column, error| {
                    column.child(
                        div()
                            .mt_1()
                            .truncate()
                            .text_xs()
                            .text_color(palette.color(ThemeRole::StateDanger))
                            .child(error),
                    )
                }),
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
                settings.apply_editor_text("invalid color", cx);
                assert_eq!(settings.invalid_edits.len(), 1);
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
    fn invalid_text_survives_editor_materialization_changes(cx: &mut gpui::TestAppContext) {
        let settings = create_settings(cx);

        cx.update(|cx| {
            settings.update(cx, |settings, cx| {
                let SettingsFocus::Row(row) = settings.focused else {
                    panic!("settings opens on the first editable row");
                };
                settings.apply_editor_text("not a valid value", cx);
                assert_eq!(
                    settings.invalid_edit(row).map(|edit| edit.text.as_str()),
                    Some("not a valid value")
                );

                settings.focused = SettingsFocus::Search;
                settings.materialize_editor(EditorTarget::Search, cx);
                assert_eq!(
                    settings.editor.as_ref().map(|editor| editor.target),
                    Some(EditorTarget::Search)
                );

                settings.focused = SettingsFocus::Row(row);
                settings.materialize_editor(EditorTarget::Row(row), cx);
                let editor = settings
                    .editor
                    .as_ref()
                    .expect("focused editable row materializes an editor")
                    .entity
                    .clone();
                assert_eq!(editor.read(cx).text(), "not a valid value");
                assert_eq!(settings.invalid_edits.len(), 1);
            });
        });
    }

    #[gpui::test]
    fn vim_settings_navigation_preserves_editor_modes_and_tabs(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::fonts::init(cx);
            let config = GuiConfig::default();
            let available_families = cx.text_system().all_font_names();
            theme::apply_appearance(&config, SourceStatus::Missing, &[], &available_families, cx);
            key_bindings::install(&config, cx).unwrap();
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
        });
        let (settings, cx) = cx.add_window_view(|window, cx| {
            let settings = SettingsView::new(cx);
            let focus = settings.focus_handle(cx);
            window.focus(&focus, cx);
            settings
        });

        let first = settings.read_with(cx, |settings, cx| {
            assert_eq!(settings.draft.input.default_binding_mode, BindingMode::Vim);
            assert_eq!(
                settings
                    .editor
                    .as_ref()
                    .expect("first row has an editor")
                    .entity
                    .read(cx)
                    .mode(),
                Mode::Normal
            );
            settings.focused
        });

        cx.simulate_keystrokes("enter");
        settings.read_with(cx, |settings, cx| {
            assert_eq!(settings.focused, first);
            assert_eq!(
                settings.editor.as_ref().unwrap().entity.read(cx).mode(),
                Mode::Insert
            );
        });
        cx.simulate_keystrokes("escape");
        settings.read_with(cx, |settings, cx| {
            assert_eq!(settings.focused, first);
            assert_eq!(
                settings.editor.as_ref().unwrap().entity.read(cx).mode(),
                Mode::Normal
            );
        });

        cx.simulate_keystrokes("j");
        let second = settings.read_with(cx, |settings, _| settings.focused);
        assert_ne!(second, first);
        cx.simulate_keystrokes("enter enter");
        settings.read_with(cx, |settings, cx| {
            assert_ne!(settings.focused, second);
            assert_eq!(
                settings.editor.as_ref().unwrap().entity.read(cx).mode(),
                Mode::Insert
            );
        });

        let section = settings.read_with(cx, |settings, _| settings.active_section);
        cx.simulate_keystrokes("tab");
        settings.read_with(cx, |settings, _| {
            assert_eq!(
                settings.active_section,
                (section + 1) % SETTINGS_SECTIONS.len()
            );
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
                settings.pending_reload_invalid_edits = Some(settings.invalid_edits.clone());
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

    #[gpui::test]
    fn confirmed_reload_discards_invalid_text_present_when_read_started(
        cx: &mut gpui::TestAppContext,
    ) {
        let settings = create_settings(cx);

        cx.update(|cx| {
            settings.update(cx, |settings, cx| {
                settings.apply_editor_text("invalid before reload", cx);
                assert!(!settings.invalid_edits.is_empty());
                settings.pending_reload_draft = Some(settings.draft.clone());
                settings.pending_reload_invalid_edits = Some(settings.invalid_edits.clone());
                settings.saving = true;
                settings.finish_reload(
                    LoadedConfig {
                        path: None,
                        config: GuiConfig::default(),
                        source: None,
                        status: SourceStatus::Missing,
                        diagnostics: Vec::new(),
                    },
                    cx,
                );
                assert!(settings.invalid_edits.is_empty());
                assert_eq!(
                    settings
                        .editor
                        .as_ref()
                        .expect("focused row editor remains materialized")
                        .entity
                        .read(cx)
                        .text(),
                    settings.edit_text(match settings.focused {
                        SettingsFocus::Row(row) => row,
                        _ => panic!("settings remains focused on its row"),
                    })
                );
            });
        });
    }
}
