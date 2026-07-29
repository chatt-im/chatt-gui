mod catalog;
mod color_picker;
mod remote;

use std::{cell::Cell, rc::Rc, sync::Arc};

use gpui::{
    AnyElement, App, Bounds, Context, Div, Entity, EventEmitter, FocusHandle, Focusable,
    FontWeight, Global, KeyBinding, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, Pixels, Render, SharedString, Subscription, Task, UniformListScrollHandle,
    WeakEntity, canvas, checkerboard, deferred, div, linear_color_stop, linear_gradient,
    prelude::*, relative, rgba, uniform_list,
};

use crate::{
    appearance::{AppearanceConfig, SharedCommittedAppearance},
    composer::{ComposerChanged, Mode, TextEditor},
    config::{
        io::{self, LoadedConfig, SaveError, SourceStatus},
        schema::{BindCommand, BindingMode, FontRendering, GuiConfig, LayoutConfig, Rgba8},
        validation::{ConfigDiagnostic, DiagnosticSeverity, has_errors, validate},
    },
    icons::{IconName, icon},
    key_bindings::{self, BindingScope},
    theme::{self, AppliedSettings, FontRole, ThemePalette, ThemeRole},
    ui_scale::rems_from_px,
};
use catalog::{
    RowRef, SETTINGS_SECTIONS, ScalarSetting, SettingsSection, ToggleSetting, help, label,
    matches_search, path, rows,
};
use color_picker::{ColorPicker, DragTarget, Hsva};
use local_rpc::settings as wire_settings;
use remote::{RemoteField, RemoteSection, RemoteValues};

#[derive(Clone)]
pub(crate) struct ConfigurationState(pub(crate) LoadedConfig);

impl Global for ConfigurationState {}

pub(crate) fn install_loaded(loaded: LoadedConfig, cx: &mut App) {
    cx.set_global(ConfigurationState(loaded));
}

pub(crate) fn install_external_loaded(
    mut loaded: LoadedConfig,
    cx: &mut App,
) -> Result<LoadedConfig, String> {
    if !matches!(loaded.status, SourceStatus::Loaded | SourceStatus::Missing) {
        return Err("external gui.toml is not valid".into());
    }
    let available_families = cx.text_system().all_font_names();
    let mut diagnostics = loaded.diagnostics.clone();
    diagnostics.extend(key_bindings::validate(&loaded.config));
    diagnostics.extend(theme::font_warnings(&loaded.config, &available_families));
    if has_errors(&diagnostics) {
        return Err("external gui.toml contains invalid settings".into());
    }
    let bindings = key_bindings::compile(&loaded.config, cx)?;
    key_bindings::apply_compiled(bindings, cx);
    theme::apply_appearance(
        &loaded.config,
        loaded.status,
        &diagnostics,
        &available_families,
        cx,
    );
    loaded.diagnostics = diagnostics;
    cx.set_global(ConfigurationState(loaded.clone()));
    Ok(loaded)
}

pub(crate) enum SettingsViewEvent {
    Closed,
    Command(wire_settings::SettingsCommand),
    LocalAppearancePreview {
        session_id: local_rpc::appearance::AppearanceSessionId,
        appearance: AppearanceConfig,
    },
    LocalLayoutPreview {
        status_bar_visible: Option<bool>,
        room_menu_visible: Option<bool>,
    },
    AppearanceCommand(local_rpc::appearance::AppearanceCommand),
}

struct PendingSave {
    config: GuiConfig,
    bindings: Vec<KeyBinding>,
}

#[derive(Clone, Copy, Default)]
struct LayoutPreviewed {
    status_bar_visible: bool,
    room_menu_visible: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SettingsFocus {
    Search,
    Row(RowRef),
    RemoteRow(RemoteField),
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
    RemoteRow(RemoteField),
}

struct ActiveEditor {
    target: EditorTarget,
    entity: Entity<TextEditor>,
    _subscription: Subscription,
}

struct ChoicePicker {
    field: RemoteField,
    query: String,
    selected: usize,
    search: Entity<TextEditor>,
    _subscription: Subscription,
    scroll: UniformListScrollHandle,
}

#[derive(Clone, PartialEq, Eq)]
struct InvalidEdit {
    row: RowRef,
    text: String,
    error: SharedString,
}

#[derive(Clone, PartialEq, Eq)]
struct InvalidRemoteEdit {
    field: RemoteField,
    text: String,
    error: SharedString,
}

#[derive(Clone)]
enum RowAction {
    Reset,
    PickColor(ThemeRole),
    Record(BindingScope, BindCommand),
    Font(FontRole, String),
}

pub(crate) struct SettingsView {
    focus: FocusHandle,
    focused: SettingsFocus,
    editor: Option<ActiveEditor>,
    choice_picker: Option<ChoicePicker>,
    color_picker: Option<ColorPicker>,
    syncing_color_picker_editor: bool,
    invalid_edits: Vec<InvalidEdit>,
    invalid_remote_edits: Vec<InvalidRemoteEdit>,
    action_menu: Option<(RowRef, usize)>,
    active_section: usize,
    query: String,
    draft: GuiConfig,
    baseline: GuiConfig,
    layout_preview_baseline: LayoutConfig,
    layout_previewed: LayoutPreviewed,
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
    remote_session: Option<wire_settings::SettingsSessionId>,
    remote_revision: u64,
    remote_sections: Vec<wire_settings::SettingsSection>,
    remote_actions: Option<wire_settings::SettingsActions>,
    remote_draft: Option<RemoteValues>,
    remote_baseline: Option<RemoteValues>,
    remote_defaults: Option<RemoteValues>,
    remote_runtime: Option<wire_settings::AudioRuntimeState>,
    remote_diagnostics: Vec<wire_settings::SettingsDiagnostic>,
    remote_status: SharedString,
    remote_open_pending: bool,
    remote_saving: bool,
    remote_pending_save: Option<RemoteValues>,
    remote_confirm_replace: bool,
    remote_loopback: bool,
    remote_advanced: bool,
    remote_meter_rms: f32,
    remote_meter_peak: f32,
    recording: Option<(BindingScope, BindCommand)>,
    key_interceptor: Option<Subscription>,
    section_menu_open: bool,
    section_menu_trigger_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    scroll: UniformListScrollHandle,
    _save_task: Option<Task<()>>,
    appearance_session: local_rpc::appearance::AppearanceSessionId,
    appearance_mutation_seq: u64,
}

impl EventEmitter<SettingsViewEvent> for SettingsView {}

impl SettingsView {
    pub(crate) fn new(
        appearance_session: local_rpc::appearance::AppearanceSessionId,
        live_layout: LayoutConfig,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut loaded = cx.global::<ConfigurationState>().0.clone();
        if let Some(appearance) = cx
            .try_global::<SharedCommittedAppearance>()
            .and_then(|shared| shared.0.as_ref())
        {
            appearance.merge_into(&mut loaded.config);
        }
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
        let initial_color_picker = match first_row {
            RowRef::Theme(role) => Some(ColorPicker::new(role, loaded.config.theme.color(role))),
            _ => None,
        };
        let mut this = Self {
            focus: cx.focus_handle(),
            focused: SettingsFocus::Row(first_row),
            editor: None,
            choice_picker: None,
            color_picker: initial_color_picker,
            syncing_color_picker_editor: false,
            invalid_edits: Vec::new(),
            invalid_remote_edits: Vec::new(),
            action_menu: None,
            active_section: 0,
            query: String::new(),
            draft: loaded.config.clone(),
            baseline: loaded.config,
            layout_preview_baseline: live_layout,
            layout_previewed: LayoutPreviewed::default(),
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
            remote_session: None,
            remote_revision: 0,
            remote_sections: Vec::new(),
            remote_actions: None,
            remote_draft: None,
            remote_baseline: None,
            remote_defaults: None,
            remote_runtime: None,
            remote_diagnostics: Vec::new(),
            remote_status: "Connecting to Chatt daemon…".into(),
            remote_open_pending: false,
            remote_saving: false,
            remote_pending_save: None,
            remote_confirm_replace: false,
            remote_loopback: false,
            remote_advanced: false,
            remote_meter_rms: 0.0,
            remote_meter_peak: 0.0,
            recording: None,
            key_interceptor: None,
            section_menu_open: false,
            section_menu_trigger_bounds: Rc::new(Cell::new(None)),
            scroll: UniformListScrollHandle::new(),
            _save_task: None,
            appearance_session,
            appearance_mutation_seq: 0,
        };
        this.materialize_editor(EditorTarget::Row(first_row), cx);
        this
    }

    fn dirty(&self) -> bool {
        self.local_dirty() || self.remote_dirty()
    }

    fn local_dirty(&self) -> bool {
        self.draft != self.baseline || !self.invalid_edits.is_empty()
    }

    fn remote_dirty(&self) -> bool {
        self.remote_draft != self.remote_baseline || !self.invalid_remote_edits.is_empty()
    }

    fn local_working(&self) -> bool {
        self.pending_save.is_some() || self.pending_reload_draft.is_some()
    }

    fn section(&self) -> &'static SettingsSection {
        &SETTINGS_SECTIONS[self.active_section]
    }

    fn remote_section(&self) -> Option<RemoteSection> {
        self.active_section
            .checked_sub(SETTINGS_SECTIONS.len())
            .filter(|index| *index < self.remote_sections.len())
    }

    fn remote_field(&self, field: RemoteField) -> Option<&wire_settings::SettingsField> {
        remote::field(&self.remote_sections, field)
    }

    fn remote_field_is_text(&self, field: RemoteField) -> bool {
        self.remote_field(field).is_some_and(remote::is_text)
    }

    fn remote_field_is_searchable_choice(&self, field: RemoteField) -> bool {
        self.remote_field(field)
            .is_some_and(|field| field.control.kind == wire_settings::CONTROL_SEARCHABLE_CHOICE)
    }

    fn remote_section_is_audio(&self) -> bool {
        self.remote_section()
            .and_then(|section| self.remote_sections.get(section))
            .is_some_and(|section| section.fields.iter().any(remote::is_audio))
    }

    fn visible_rows(&self) -> Vec<RowRef> {
        if self.remote_section().is_some() {
            return Vec::new();
        }
        let section = self.section();
        rows(section, self.diagnostics.len())
            .into_iter()
            .filter(|row| matches_search(section, *row, &self.query))
            .collect()
    }

    fn visible_remote_fields(&self) -> Vec<RemoteField> {
        let Some(section) = self.remote_section() else {
            return Vec::new();
        };
        remote::fields(
            &self.remote_sections,
            section,
            self.remote_advanced || !self.query.trim().is_empty(),
        )
        .into_iter()
        .filter(|field| remote::matches_search(&self.remote_sections, section, *field, &self.query))
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

    fn invalid_remote_edit(&self, field: RemoteField) -> Option<&InvalidRemoteEdit> {
        self.invalid_remote_edits
            .iter()
            .find(|edit| edit.field == field)
    }

    fn clear_invalid_remote_edit(&mut self, field: RemoteField) {
        self.invalid_remote_edits.retain(|edit| edit.field != field);
    }

    fn set_invalid_remote_edit(&mut self, field: RemoteField, text: &str, error: String) {
        let error: SharedString = error.into();
        if let Some(edit) = self
            .invalid_remote_edits
            .iter_mut()
            .find(|edit| edit.field == field)
        {
            edit.text.clear();
            edit.text.push_str(text);
            edit.error = error;
        } else {
            self.invalid_remote_edits.push(InvalidRemoteEdit {
                field,
                text: text.to_string(),
                error,
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
            EditorTarget::RemoteRow(field) => self
                .invalid_remote_edit(field)
                .map(|edit| edit.text.clone())
                .or_else(|| {
                    self.remote_draft.as_ref().and_then(|draft| {
                        self.remote_field(field)
                            .map(|descriptor| remote::editor_value(draft, descriptor))
                    })
                })
                .unwrap_or_default(),
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
            EditorTarget::RemoteRow(_) => "Edit value",
        };
        let entity = cx.new(|cx| {
            let mut editor = TextEditor::settings_input(placeholder, binding_mode, cx);
            editor.set_value(value, cx);
            editor
        });
        let subscription = cx.subscribe(&entity, move |this, editor, _: &ComposerChanged, cx| {
            if this.syncing_color_picker_editor {
                return;
            }
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
                EditorTarget::RemoteRow(field)
                    if this.focused == SettingsFocus::RemoteRow(field) =>
                {
                    this.apply_remote_editor_text(field, &value, cx);
                }
                EditorTarget::RemoteRow(_) => {}
            }
        });
        self.editor = Some(ActiveEditor {
            target,
            entity,
            _subscription: subscription,
        });
    }

    fn focus_order(&self) -> Vec<SettingsFocus> {
        let mut order =
            Vec::with_capacity(self.visible_rows().len() + self.visible_remote_fields().len() + 7);
        order.push(SettingsFocus::Search);
        if self.remote_section().is_some() {
            order.extend(
                self.visible_remote_fields()
                    .into_iter()
                    .map(SettingsFocus::RemoteRow),
            );
        } else {
            order.extend(
                self.visible_rows()
                    .into_iter()
                    .filter(|row| !matches!(row, RowRef::Diagnostic(_)))
                    .map(SettingsFocus::Row),
            );
        }
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
        let color_panel_was_hidden = self.color_picker.is_none();
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
            SettingsFocus::RemoteRow(field)
                if self.remote_field(field).is_some_and(remote::is_text)
                    && self.remote_draft.is_some() =>
            {
                self.materialize_editor(EditorTarget::RemoteRow(field), cx);
                if enter_insert {
                    if let Some(editor) = &self.editor {
                        editor
                            .entity
                            .update(cx, |editor, cx| editor.enter_insert_mode(cx));
                    }
                }
                if let Some(index) = self
                    .visible_remote_fields()
                    .iter()
                    .position(|candidate| *candidate == field)
                {
                    self.scroll
                        .scroll_to_item(index, gpui::ScrollStrategy::Center);
                }
                if let Some(editor) = &self.editor {
                    window.focus(&editor.entity.focus_handle(cx), cx);
                }
            }
            SettingsFocus::RemoteRow(field) => {
                self.editor = None;
                if let Some(index) = self
                    .visible_remote_fields()
                    .iter()
                    .position(|candidate| *candidate == field)
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
        self.reconcile_color_picker_with_focus();
        if color_panel_was_hidden
            && let SettingsFocus::Row(row @ RowRef::Theme(_)) = target
            && let Some(index) = self
                .visible_rows()
                .iter()
                .position(|candidate| *candidate == row)
        {
            self.scroll
                .scroll_to_item(index, gpui::ScrollStrategy::Nearest);
        }
        cx.notify();
    }

    fn reconcile_color_picker_with_focus(&mut self) {
        let selected_role = match self.focused {
            SettingsFocus::Row(RowRef::Theme(role)) => Some(role),
            _ => None,
        };
        if self.color_picker.as_ref().map(|picker| picker.role) == selected_role {
            return;
        }
        self.color_picker =
            selected_role.map(|role| ColorPicker::new(role, self.draft.theme.color(role)));
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
        if !self.query.trim().is_empty() && !self.active_section_has_matches() {
            if let Some(index) = (0..SETTINGS_SECTIONS.len() + self.remote_sections.len())
                .find(|index| self.section_has_matches(*index))
            {
                let was_audio = self.remote_section_is_audio();
                self.active_section = index;
                let is_audio = self.remote_section_is_audio();
                if was_audio != is_audio {
                    self.set_remote_audio_active(is_audio, cx);
                }
                self.scroll.scroll_to_item(0, gpui::ScrollStrategy::Top);
            }
        }
        let order = self.focus_order();
        if order.contains(&self.focused) {
            cx.notify();
            return;
        }
        self.focused = order
            .iter()
            .copied()
            .find(|target| matches!(target, SettingsFocus::Row(_) | SettingsFocus::RemoteRow(_)))
            .unwrap_or(SettingsFocus::Search);
        self.editor = None;
        match self.focused {
            SettingsFocus::Search => self.materialize_editor(EditorTarget::Search, cx),
            SettingsFocus::Row(row) if Self::row_has_editor(row) => {
                self.materialize_editor(EditorTarget::Row(row), cx)
            }
            SettingsFocus::RemoteRow(field)
                if self.remote_field(field).is_some_and(remote::is_text) =>
            {
                self.materialize_editor(EditorTarget::RemoteRow(field), cx)
            }
            _ => {}
        }
        self.reconcile_color_picker_with_focus();
        cx.notify();
    }

    fn active_section_has_matches(&self) -> bool {
        self.section_has_matches(self.active_section)
    }

    fn section_has_matches(&self, index: usize) -> bool {
        if let Some(section) = index
            .checked_sub(SETTINGS_SECTIONS.len())
            .filter(|index| *index < self.remote_sections.len())
        {
            return remote::fields(&self.remote_sections, section, true)
                .into_iter()
                .any(|field| {
                    remote::matches_search(&self.remote_sections, section, field, &self.query)
                });
        }
        let Some(section) = SETTINGS_SECTIONS.get(index) else {
            return false;
        };
        rows(section, self.diagnostics.len())
            .into_iter()
            .any(|row| matches_search(section, row, &self.query))
    }

    fn select_section(&mut self, index: usize, window: &mut gpui::Window, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        self.section_menu_open = false;
        self.choice_picker = None;
        let was_audio = self.remote_section_is_audio();
        self.active_section = index;
        let is_audio = self.remote_section_is_audio();
        if was_audio != is_audio {
            self.set_remote_audio_active(is_audio, cx);
        }
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
        } else if self.remote_section().is_some() {
            self.visible_remote_fields()
                .into_iter()
                .next()
                .map(SettingsFocus::RemoteRow)
                .unwrap_or(SettingsFocus::Search)
        } else {
            self.visible_rows()
                .into_iter()
                .find(|row| !matches!(row, RowRef::Diagnostic(_)))
                .map(SettingsFocus::Row)
                .unwrap_or(SettingsFocus::Search)
        };
        self.focus_target(target, false, window, cx);
    }

    fn toggle_section_menu(&mut self, cx: &mut Context<Self>) {
        self.section_menu_open = !self.section_menu_open;
        cx.notify();
    }

    fn dismiss_section_menu(&mut self, cx: &mut Context<Self>) {
        if !self.section_menu_open {
            return;
        }
        self.section_menu_open = false;
        cx.notify();
    }

    fn select_remote_row(
        &mut self,
        field: RemoteField,
        window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        if self.saving || self.remote_draft.is_none() {
            return;
        }
        self.focus_target(SettingsFocus::RemoteRow(field), false, window, cx);
    }

    fn choice_picker_items<'a>(
        &'a self,
        field: RemoteField,
        query: &str,
    ) -> impl Iterator<Item = &'a wire_settings::SettingsChoice> {
        let query = query.trim().to_ascii_lowercase();
        self.remote_field(field)
            .into_iter()
            .flat_map(|field| &field.control.choices)
            .filter(move |choice| {
                query.is_empty()
                    || choice.label.to_ascii_lowercase().contains(&query)
                    || choice.detail.to_ascii_lowercase().contains(&query)
                    || choice.value.to_ascii_lowercase().contains(&query)
                    || choice.search.to_ascii_lowercase().contains(&query)
            })
    }

    fn current_choice_selection(&self, field: RemoteField) -> Option<&str> {
        match self.remote_draft.as_ref()?.get(field)? {
            wire_settings::SettingsValue::Text(value) => Some(value),
            _ => None,
        }
    }

    fn open_choice_picker(
        &mut self,
        field: RemoteField,
        window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        let Some(descriptor) = self.remote_field(field) else {
            return;
        };
        if descriptor.control.kind != wire_settings::CONTROL_SEARCHABLE_CHOICE
            || self.saving
            || self.remote_draft.is_none()
        {
            return;
        }
        let placeholder = if descriptor.control.placeholder.is_empty() {
            "Search choices".to_string()
        } else {
            descriptor.control.placeholder.clone()
        };
        let current = self.current_choice_selection(field);
        let selected = self
            .choice_picker_items(field, "")
            .position(|choice| Some(choice.value.as_str()) == current)
            .unwrap_or(0);
        let binding_mode = self.draft.input.default_binding_mode;
        let search = cx.new(|cx| {
            let mut editor = TextEditor::settings_input(placeholder, binding_mode, cx);
            editor.enter_insert_mode(cx);
            editor
        });
        let subscription = cx.subscribe(&search, move |this, editor, _: &ComposerChanged, cx| {
            let query = editor.read(cx).text();
            let current = this.current_choice_selection(field);
            let selected = this
                .choice_picker_items(field, &query)
                .position(|choice| Some(choice.value.as_str()) == current)
                .unwrap_or(0);
            let has_items = this.choice_picker_items(field, &query).next().is_some();
            if let Some(picker) = &mut this.choice_picker
                && picker.field == field
            {
                picker.query = query;
                picker.selected = selected;
                if has_items {
                    picker
                        .scroll
                        .scroll_to_item(selected, gpui::ScrollStrategy::Center);
                }
                cx.notify();
            }
        });
        let focus = search.focus_handle(cx);
        self.focused = SettingsFocus::RemoteRow(field);
        self.editor = None;
        self.color_picker = None;
        self.choice_picker = Some(ChoicePicker {
            field,
            query: String::new(),
            selected,
            search,
            _subscription: subscription,
            scroll: UniformListScrollHandle::new(),
        });
        self.refresh_remote_choices(field, cx);
        window.focus(&focus, cx);
        cx.notify();
    }

    fn close_choice_picker(&mut self, window: &mut gpui::Window, cx: &mut Context<Self>) {
        self.choice_picker = None;
        window.focus(&self.focus, cx);
        cx.notify();
    }

    fn move_choice_picker_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(picker) = self.choice_picker.as_ref() else {
            return;
        };
        let count = self
            .choice_picker_items(picker.field, &picker.query)
            .count();
        if count == 0 {
            return;
        }
        let selected =
            (picker.selected.min(count - 1) as isize + delta).rem_euclid(count as isize) as usize;
        if let Some(picker) = &mut self.choice_picker {
            picker.selected = selected;
            picker
                .scroll
                .scroll_to_item(selected, gpui::ScrollStrategy::Center);
        }
        cx.notify();
    }

    fn choose_selected_choice(&mut self, window: &mut gpui::Window, cx: &mut Context<Self>) {
        let Some(picker) = self.choice_picker.as_ref() else {
            return;
        };
        let field = picker.field;
        let Some(choice) = self
            .choice_picker_items(field, &picker.query)
            .nth(picker.selected)
            .cloned()
        else {
            return;
        };
        self.choose_choice(field, choice, window, cx);
    }

    fn choose_choice(
        &mut self,
        field: RemoteField,
        choice: wire_settings::SettingsChoice,
        window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        if !choice.enabled {
            return;
        }
        let Some(draft) = &mut self.remote_draft else {
            return;
        };
        draft.set(field, wire_settings::SettingsValue::Text(choice.value));
        self.clear_invalid_remote_edit(field);
        self.choice_picker = None;
        window.focus(&self.focus, cx);
        if self.remote_field(field).is_some_and(remote::is_audio) {
            self.preview_remote_audio(cx);
        }
        cx.notify();
    }

    fn reconcile_choice_picker(&mut self) {
        let Some(picker) = self.choice_picker.as_ref() else {
            return;
        };
        let field = picker.field;
        let query = picker.query.clone();
        let previous = picker.selected;
        if !self.remote_field_is_searchable_choice(field) {
            self.choice_picker = None;
            return;
        }
        let current = self.current_choice_selection(field);
        let selected = self
            .choice_picker_items(field, &query)
            .position(|choice| Some(choice.value.as_str()) == current)
            .unwrap_or_else(|| {
                previous.min(
                    self.choice_picker_items(field, &query)
                        .count()
                        .saturating_sub(1),
                )
            });
        if let Some(picker) = &mut self.choice_picker {
            picker.selected = selected;
        }
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
            RowRef::Choice(_) | RowRef::Toggle(_) | RowRef::Diagnostic(_) => String::new(),
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
            RowRef::Choice(_) | RowRef::Toggle(_) | RowRef::Diagnostic(_) => Ok(false),
        };
        match result {
            Ok(changed) => {
                self.clear_invalid_edit(row);
                if let RowRef::Theme(role) = row
                    && let Some(picker) = &mut self.color_picker
                    && picker.role == role
                {
                    picker.hsva = Hsva::from_rgba8(self.draft.theme.color(role));
                }
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

    fn open_color_picker(
        &mut self,
        role: ThemeRole,
        window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        if self.saving {
            return;
        }
        self.choice_picker = None;
        self.focus_target(SettingsFocus::Row(RowRef::Theme(role)), false, window, cx);
    }

    fn sync_color_picker_editor(&mut self, role: ThemeRole, color: Rgba8, cx: &mut Context<Self>) {
        let entity = self
            .editor
            .as_ref()
            .filter(|editor| editor.target == EditorTarget::Row(RowRef::Theme(role)))
            .map(|editor| editor.entity.clone());
        if let Some(entity) = entity {
            self.syncing_color_picker_editor = true;
            entity.update(cx, |editor, cx| editor.set_value(color.to_string(), cx));
            self.syncing_color_picker_editor = false;
        }
    }

    fn update_color_picker_pointer(
        &mut self,
        target: DragTarget,
        position: gpui::Point<gpui::Pixels>,
        force: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(picker) = &mut self.color_picker else {
            return;
        };
        if !picker.update_from_pointer(target, position) && !force {
            cx.notify();
            return;
        }
        let role = picker.role;
        let color = picker.hsva.to_rgba8();
        let changed = self.draft.theme.color(role) != color;
        self.draft.theme.set_color(role, color);
        self.clear_invalid_edit(RowRef::Theme(role));
        self.sync_color_picker_editor(role, color, cx);
        if changed {
            self.preview(cx);
        }
        cx.notify();
    }

    fn begin_color_picker_drag(&mut self, event: &MouseDownEvent, cx: &mut Context<Self>) {
        let Some(target) = self
            .color_picker
            .as_ref()
            .and_then(|picker| picker.target_at(event.position))
        else {
            return;
        };
        let mut promoted = false;
        if let Some(picker) = &mut self.color_picker {
            picker.drag_target = Some(target);
            if target == DragTarget::Hue {
                if picker.hsva.saturation <= 0.001 {
                    picker.hsva.saturation = 1.0;
                    promoted = true;
                }
                if picker.hsva.value <= 0.001 {
                    picker.hsva.value = 1.0;
                    promoted = true;
                }
            }
        }
        self.update_color_picker_pointer(target, event.position, promoted, cx);
    }

    fn drag_color_picker(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        let Some(target) = self
            .color_picker
            .as_ref()
            .and_then(|picker| picker.drag_target)
        else {
            return;
        };
        if !event.dragging() {
            if let Some(picker) = &mut self.color_picker {
                picker.drag_target = None;
            }
            return;
        }
        self.update_color_picker_pointer(target, event.position, false, cx);
    }

    fn finish_color_picker_drag(&mut self) {
        if let Some(picker) = &mut self.color_picker {
            picker.drag_target = None;
        }
    }

    fn apply_color_picker(&mut self, window: &mut gpui::Window, cx: &mut Context<Self>) {
        let Some(role) = self.color_picker.as_ref().map(|picker| picker.role) else {
            return;
        };
        if self.invalid_edit(RowRef::Theme(role)).is_some() {
            self.status_message = Some("Enter a valid hex color before applying.".into());
            cx.notify();
            return;
        }
        if let Some(picker) = &mut self.color_picker {
            picker.original = self.draft.theme.color(role);
            picker.hsva = Hsva::from_rgba8(picker.original);
            picker.drag_target = None;
        }
        if let Some(editor) = &self.editor {
            window.focus(&editor.entity.focus_handle(cx), cx);
        }
        cx.notify();
    }

    fn cancel_color_picker(&mut self, window: &mut gpui::Window, cx: &mut Context<Self>) {
        let Some((role, original)) = self
            .color_picker
            .as_ref()
            .map(|picker| (picker.role, picker.original))
        else {
            return;
        };
        let changed = self.draft.theme.color(role) != original;
        self.draft.theme.set_color(role, original);
        self.clear_invalid_edit(RowRef::Theme(role));
        if let Some(picker) = &mut self.color_picker {
            picker.hsva = Hsva::from_rgba8(original);
            picker.drag_target = None;
        }
        self.sync_color_picker_editor(role, original, cx);
        if changed {
            self.preview(cx);
        }
        if let Some(editor) = &self.editor {
            window.focus(&editor.entity.focus_handle(cx), cx);
        }
        cx.notify();
    }

    fn apply_remote_editor_text(&mut self, field: RemoteField, text: &str, cx: &mut Context<Self>) {
        let Some(descriptor) = self.remote_field(field).cloned() else {
            return;
        };
        let Some(mut candidate) = self.remote_draft.clone() else {
            return;
        };
        match remote::apply_text(&mut candidate, &descriptor, text) {
            Ok(()) => {
                let changed = self.remote_draft.as_ref() != Some(&candidate);
                self.remote_draft = Some(candidate);
                self.clear_invalid_remote_edit(field);
                if changed && remote::is_audio(&descriptor) {
                    self.preview_remote_audio(cx);
                }
            }
            Err(error) => self.set_invalid_remote_edit(field, text, error),
        }
        cx.notify();
    }

    fn cycle_remote_field(&mut self, field: RemoteField, delta: isize, cx: &mut Context<Self>) {
        let Some(descriptor) = self.remote_field(field).cloned() else {
            return;
        };
        let Some(draft) = &mut self.remote_draft else {
            return;
        };
        if !remote::cycle(draft, &descriptor, delta) {
            return;
        }
        self.clear_invalid_remote_edit(field);
        self.sync_remote_editor(field, cx);
        if remote::is_audio(&descriptor) {
            self.preview_remote_audio(cx);
        }
        cx.notify();
    }

    fn change_remote_field(
        &mut self,
        field: RemoteField,
        delta: isize,
        window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        if self.remote_field_is_searchable_choice(field) {
            self.open_choice_picker(field, window, cx);
        } else {
            self.cycle_remote_field(field, delta, cx);
        }
    }

    fn sync_remote_editor(&mut self, field: RemoteField, cx: &mut Context<Self>) {
        let entity = self
            .editor
            .as_ref()
            .filter(|editor| editor.target == EditorTarget::RemoteRow(field))
            .map(|editor| editor.entity.clone());
        if let Some(entity) = entity {
            let value = self.editor_value(EditorTarget::RemoteRow(field));
            entity.update(cx, |editor, cx| editor.set_value(value, cx));
        }
    }

    fn reset_remote_field(&mut self, field: RemoteField, cx: &mut Context<Self>) {
        let is_audio = self.remote_field(field).is_some_and(remote::is_audio);
        let (Some(draft), Some(defaults)) = (&mut self.remote_draft, self.remote_defaults.as_ref())
        else {
            return;
        };
        draft.copy_from(defaults, field);
        self.clear_invalid_remote_edit(field);
        self.sync_remote_editor(field, cx);
        if is_audio {
            self.preview_remote_audio(cx);
        }
        cx.notify();
    }

    fn set_remote_audio_active(&mut self, active: bool, cx: &mut Context<Self>) {
        let Some(session_id) = self.remote_session else {
            return;
        };
        if active
            && !self
                .remote_actions
                .as_ref()
                .is_some_and(|actions| actions.audio_preview)
        {
            return;
        }
        cx.emit(SettingsViewEvent::Command(
            wire_settings::SettingsCommand::SetAudioPreviewActive { session_id, active },
        ));
        if active {
            self.preview_remote_audio(cx);
        } else {
            self.remote_loopback = false;
            self.remote_meter_rms = 0.0;
            self.remote_meter_peak = 0.0;
        }
    }

    fn preview_remote_audio(&mut self, cx: &mut Context<Self>) {
        if !self.remote_section_is_audio() {
            return;
        }
        let (Some(session_id), Some(draft), Some(baseline)) = (
            self.remote_session,
            self.remote_draft.as_ref(),
            self.remote_baseline.as_ref(),
        ) else {
            return;
        };
        let changes = draft.changes(baseline, |field| {
            self.remote_field(field).is_some_and(remote::is_audio)
        });
        let preview_seq = self
            .remote_runtime
            .as_ref()
            .map_or(1, |runtime| runtime.preview_seq.saturating_add(1))
            .max(1);
        if let Some(runtime) = &mut self.remote_runtime {
            runtime.preview_seq = preview_seq;
        }
        cx.emit(SettingsViewEvent::Command(
            wire_settings::SettingsCommand::PreviewAudio {
                session_id,
                preview_seq,
                changes,
                loopback: self.remote_loopback,
            },
        ));
    }

    fn toggle_remote_loopback(&mut self, cx: &mut Context<Self>) {
        if !self
            .remote_actions
            .as_ref()
            .is_some_and(|actions| actions.audio_loopback)
        {
            self.remote_status = "Microphone loopback is unavailable in this daemon mode.".into();
            cx.notify();
            return;
        }
        self.remote_loopback = !self.remote_loopback;
        self.preview_remote_audio(cx);
        cx.notify();
    }

    fn refresh_remote_choices(&mut self, field: RemoteField, cx: &mut Context<Self>) {
        let (Some(session_id), Some(draft), Some(baseline)) = (
            self.remote_session,
            self.remote_draft.as_ref(),
            self.remote_baseline.as_ref(),
        ) else {
            return;
        };
        let changes = draft.changes(baseline, |_| true);
        self.remote_status = "Refreshing choices…".into();
        cx.emit(SettingsViewEvent::Command(
            wire_settings::SettingsCommand::RefreshChoices {
                session_id,
                field,
                changes,
            },
        ));
        cx.notify();
    }

    pub(crate) fn begin_remote(&mut self, cx: &mut Context<Self>) {
        if self.remote_open_pending || self.remote_session.is_some() {
            return;
        }
        self.remote_open_pending = true;
        self.remote_status = "Loading daemon settings…".into();
        cx.emit(SettingsViewEvent::Command(
            wire_settings::SettingsCommand::Open,
        ));
        cx.notify();
    }

    pub(crate) fn remote_disconnected(&mut self, reason: &str, cx: &mut Context<Self>) {
        self.remote_session = None;
        self.remote_open_pending = false;
        self.remote_saving = false;
        self.choice_picker = None;
        self.remote_status = format!("Daemon unavailable · {reason}").into();
        self.remote_runtime = None;
        self.remote_meter_rms = 0.0;
        self.remote_meter_peak = 0.0;
        self.saving = self.local_working();
        cx.notify();
    }

    pub(crate) fn remote_command_failed(&mut self, reason: &str, cx: &mut Context<Self>) {
        self.remote_open_pending = false;
        self.remote_saving = false;
        self.remote_pending_save = None;
        self.remote_status = format!("Could not send daemon setting change · {reason}").into();
        self.saving = self.local_working();
        cx.notify();
    }

    pub(crate) fn remote_reconnected(&mut self, cx: &mut Context<Self>) {
        if self.remote_session.is_none() {
            self.begin_remote(cx);
        }
    }

    pub(crate) fn apply_remote_result(
        &mut self,
        result: wire_settings::SettingsResult,
        cx: &mut Context<Self>,
    ) {
        let operation = result.result.operation;
        let document = match &result.payload {
            wire_settings::SettingsResultPayload::Document(document)
            | wire_settings::SettingsResultPayload::Conflict { latest: document } => Some(document),
            _ => None,
        };
        if let Some(document) = document {
            let same_session = self.remote_session == Some(document.session_id);
            if operation != local_rpc::frame::Operation::OpenSettings && !same_session
                || operation == local_rpc::frame::Operation::OpenSettings
                    && self.remote_session.is_some()
                    && !same_session
                    && !self.remote_open_pending
                || same_session && document.revision < self.remote_revision
            {
                return;
            }
        }
        if matches!(
            &result.payload,
            wire_settings::SettingsResultPayload::PreviewApplied { session_id, .. }
                | wire_settings::SettingsResultPayload::Closed { session_id }
                if self.remote_session != Some(*session_id)
        ) {
            return;
        }
        let rejected = match &result.result.outcome {
            local_rpc::frame::RequestOutcome::Accepted => None,
            local_rpc::frame::RequestOutcome::Rejected { message, .. } => Some(message.clone()),
        };
        if operation == local_rpc::frame::Operation::OpenSettings {
            self.remote_open_pending = false;
        }
        let save_result = operation == local_rpc::frame::Operation::SaveSettings;
        match result.payload {
            wire_settings::SettingsResultPayload::Document(document) => match operation {
                local_rpc::frame::Operation::OpenSettings => {
                    let source = document.source;
                    let preserve = self.remote_dirty();
                    self.install_remote_document(document, preserve);
                    self.remote_pending_save = None;
                    self.remote_saving = false;
                    self.remote_confirm_replace = false;
                    self.remote_status = match source {
                        wire_settings::SettingsSourceStatus::Defaults => {
                            "Daemon settings loaded from embedded defaults.".into()
                        }
                        wire_settings::SettingsSourceStatus::File => {
                            "Daemon settings loaded from chatt.toml.".into()
                        }
                    };
                    if self.remote_section_is_audio() {
                        self.set_remote_audio_active(true, cx);
                    }
                }
                local_rpc::frame::Operation::SaveSettings => {
                    let pending = self.remote_pending_save.take();
                    let newer_edits = pending
                        .as_ref()
                        .is_some_and(|pending| self.remote_draft.as_ref() != Some(pending));
                    self.install_remote_document(document, newer_edits);
                    self.remote_confirm_replace = false;
                    self.remote_status = if newer_edits {
                        "Daemon settings saved; newer edits remain unsaved.".into()
                    } else {
                        "Daemon settings saved.".into()
                    };
                }
                local_rpc::frame::Operation::ReloadSettings => {
                    self.install_remote_document(document, false);
                    self.remote_pending_save = None;
                    self.remote_confirm_replace = false;
                    self.remote_status = "Daemon settings reloaded from disk.".into();
                }
                local_rpc::frame::Operation::RefreshSettingsChoices => {
                    self.install_remote_document(document, true);
                    self.remote_status = "Choice refresh started.".into();
                }
                _ => self.install_remote_document(document, true),
            },
            wire_settings::SettingsResultPayload::PreviewApplied { runtime, .. } => {
                self.remote_diagnostics = runtime.diagnostics.clone();
                if let Some(diagnostic) = self.remote_diagnostics.first() {
                    self.remote_status =
                        format!("{} · {}", diagnostic.field, diagnostic.message).into();
                }
                self.remote_loopback = runtime.loopback;
                self.remote_runtime = Some(runtime);
            }
            wire_settings::SettingsResultPayload::Conflict { latest } => {
                self.install_remote_document(latest, true);
                self.remote_confirm_replace = true;
                self.remote_status =
                    "chatt.toml changed on disk. Reload or Confirm replace.".into();
            }
            wire_settings::SettingsResultPayload::Closed { .. } => {
                self.remote_session = None;
                self.remote_runtime = None;
                self.choice_picker = None;
                self.remote_status = "Daemon settings session closed.".into();
            }
            wire_settings::SettingsResultPayload::None => {}
        }
        let reload_result = operation == local_rpc::frame::Operation::ReloadSettings;
        if save_result || reload_result {
            self.remote_saving = false;
            self.saving = self.local_working();
        }
        if let Some(message) = rejected {
            if save_result {
                self.remote_pending_save = None;
            }
            if result.diagnostics.is_empty() {
                self.remote_status = message.into();
            } else {
                self.remote_status = format!(
                    "{message} · {}",
                    result
                        .diagnostics
                        .iter()
                        .map(|diagnostic| diagnostic.message.as_str())
                        .collect::<Vec<_>>()
                        .join("; ")
                )
                .into();
            }
            self.remote_diagnostics = result.diagnostics;
        }
        cx.notify();
    }

    pub(crate) fn apply_remote_event(
        &mut self,
        event: wire_settings::SettingsEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            wire_settings::SettingsEvent::AudioMeter {
                session_id,
                rms,
                peak,
                ..
            } if Some(session_id) == self.remote_session => {
                self.remote_meter_rms = self.remote_meter_rms * 0.62 + rms * 0.38;
                self.remote_meter_peak = (self.remote_meter_peak * 0.9).max(peak);
            }
            wire_settings::SettingsEvent::AudioRuntime {
                session_id,
                runtime,
            } if Some(session_id) == self.remote_session => {
                self.remote_diagnostics = runtime.diagnostics.clone();
                if let Some(diagnostic) = self.remote_diagnostics.first() {
                    self.remote_status =
                        format!("{} · {}", diagnostic.field, diagnostic.message).into();
                }
                self.remote_runtime = Some(runtime);
            }
            wire_settings::SettingsEvent::Document(document)
                if Some(document.session_id) == self.remote_session =>
            {
                if document.revision < self.remote_revision {
                    return;
                }
                self.install_remote_document(document, true);
                self.remote_status = "Choices refreshed.".into();
            }
            _ => return,
        }
        cx.notify();
    }

    fn install_remote_document(
        &mut self,
        document: wire_settings::SettingsDocument,
        preserve_draft: bool,
    ) {
        let retained_changes = if preserve_draft {
            self.remote_draft
                .as_ref()
                .zip(self.remote_baseline.as_ref())
                .map_or_else(Vec::new, |(draft, baseline)| {
                    draft.changes(baseline, |_| true)
                })
        } else {
            Vec::new()
        };
        let retained_invalid = preserve_draft.then(|| self.invalid_remote_edits.clone());
        let mut current = RemoteValues::current(&document);
        let known_fields = document
            .sections
            .iter()
            .flat_map(|section| &section.fields)
            .map(|field| field.id)
            .collect::<std::collections::HashSet<_>>();
        for change in retained_changes {
            if known_fields.contains(&change.field) {
                current.set(change.field, change.value);
            }
        }
        self.remote_session = Some(document.session_id);
        self.remote_revision = document.revision;
        self.remote_baseline = Some(RemoteValues::current(&document));
        self.remote_defaults = Some(RemoteValues::defaults(&document));
        self.remote_sections = document.sections;
        self.remote_actions = Some(document.actions);
        self.remote_draft = Some(current);
        self.reconcile_choice_picker();
        self.remote_runtime = Some(document.audio_runtime);
        self.remote_diagnostics = document.diagnostics;
        self.invalid_remote_edits = retained_invalid.unwrap_or_default();
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
        self.publish_appearance_preview(cx);
    }

    fn publish_appearance_preview(&mut self, cx: &mut Context<Self>) {
        let appearance = AppearanceConfig::from_gui(&self.draft);
        cx.emit(SettingsViewEvent::LocalAppearancePreview {
            session_id: self.appearance_session,
            appearance: appearance.clone(),
        });
        self.appearance_mutation_seq = self.appearance_mutation_seq.wrapping_add(1).max(1);
        let mutation_seq = self.appearance_mutation_seq;
        let document = match appearance.document() {
            Ok(document) => document,
            Err(error) => {
                self.status_message =
                    Some(format!("Could not share appearance preview · {error}").into());
                return;
            }
        };
        cx.emit(SettingsViewEvent::AppearanceCommand(
            local_rpc::appearance::AppearanceCommand::Preview {
                session_id: self.appearance_session,
                mutation_seq,
                document,
            },
        ));
    }

    pub(crate) fn republish_appearance(&mut self, cx: &mut Context<Self>) {
        if self.local_dirty() {
            self.publish_appearance_preview(cx);
        }
    }

    pub(crate) fn shared_preview_changed(
        &mut self,
        session_id: local_rpc::appearance::AppearanceSessionId,
        cx: &mut Context<Self>,
    ) {
        self.status_message = Some(if session_id == self.appearance_session {
            "Sharing live appearance preview.".into()
        } else {
            "Live appearance preview is controlled by another GUI.".into()
        });
        cx.notify();
    }

    pub(crate) fn shared_committed_appearance_changed(
        &mut self,
        appearance: Option<&AppearanceConfig>,
        cx: &mut Context<Self>,
    ) {
        if self.local_dirty() {
            self.status_message =
                Some("Another GUI committed appearance changes; this draft was preserved.".into());
            cx.notify();
            return;
        }
        let loaded = cx.global::<ConfigurationState>().0.clone();
        let mut config = loaded.config.clone();
        if let Some(appearance) = appearance {
            appearance.merge_into(&mut config);
        }
        self.draft = config.clone();
        self.baseline = config;
        self.source = loaded.source;
        self.source_status = loaded.status;
        self.diagnostics = loaded.diagnostics;
        self.committed = AppliedSettings::get(cx);
        self.invalid_edits.clear();
        if let SettingsFocus::Row(row) = self.focused {
            self.sync_row_editor(row, cx);
        }
        self.status_message = Some(if appearance.is_some() {
            "Appearance committed by a GUI connected to this daemon.".into()
        } else {
            "Shared appearance cleared; using gui.toml.".into()
        });
        cx.notify();
    }

    pub(crate) fn install_external_loaded_if_clean(
        &mut self,
        loaded: LoadedConfig,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        if self.local_dirty() {
            self.status_message =
                Some("gui.toml changed in another GUI; reload or replace this draft.".into());
            cx.notify();
            return Ok(false);
        }
        let loaded = install_external_loaded(loaded, cx)?;
        let layout = loaded.config.layout;
        self.draft = loaded.config.clone();
        self.baseline = loaded.config.clone();
        self.source = loaded.source;
        self.source_status = loaded.status;
        self.diagnostics = loaded.diagnostics;
        self.path = loaded.path;
        self.committed = AppliedSettings::get(cx);
        self.invalid_edits.clear();
        if let SettingsFocus::Row(row) = self.focused {
            self.sync_row_editor(row, cx);
        }
        self.install_live_layout(layout, cx);
        self.status_message = Some("Loaded appearance saved by another GUI.".into());
        cx.notify();
        Ok(true)
    }

    fn commit_shared_appearance(&mut self, config: &GuiConfig, cx: &mut Context<Self>) {
        self.appearance_mutation_seq = self.appearance_mutation_seq.wrapping_add(1).max(1);
        let document = match AppearanceConfig::from_gui(config).document() {
            Ok(document) => document,
            Err(error) => {
                self.status_message =
                    Some(format!("Saved, but could not share appearance · {error}").into());
                return;
            }
        };
        cx.emit(SettingsViewEvent::AppearanceCommand(
            local_rpc::appearance::AppearanceCommand::Commit {
                session_id: self.appearance_session,
                mutation_seq: self.appearance_mutation_seq,
                document,
            },
        ));
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
        let previous_layout = self.draft.layout;
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
        self.preview_layout_changes(previous_layout, cx);
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
            RowRef::Toggle(ToggleSetting::StatusBarVisible) => {
                self.draft.layout.status_bar_visible = defaults.layout.status_bar_visible;
                false
            }
            RowRef::Toggle(ToggleSetting::RoomMenuVisible) => {
                self.draft.layout.room_menu_visible = defaults.layout.room_menu_visible;
                false
            }
            RowRef::Toggle(ToggleSetting::NativeFullscreen) => {
                self.draft.native_fullscreen = defaults.native_fullscreen;
                false
            }
            RowRef::Toggle(ToggleSetting::VideoLoopByDefault) => {
                self.draft.video_loop_by_default = defaults.video_loop_by_default;
                false
            }
            RowRef::Toggle(ToggleSetting::LiveLowDelayDecode) => {
                self.draft.live_low_delay_decode = defaults.live_low_delay_decode;
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
        if let Some(section) = self.remote_section() {
            let section_fields = remote::fields(&self.remote_sections, section, true);
            let is_audio = self
                .remote_sections
                .get(section)
                .is_some_and(|section| section.fields.iter().any(remote::is_audio));
            let (Some(defaults), Some(draft)) =
                (self.remote_defaults.clone(), self.remote_draft.as_mut())
            else {
                return;
            };
            for field in &section_fields {
                draft.copy_from(&defaults, *field);
            }
            self.invalid_remote_edits
                .retain(|edit| !section_fields.contains(&edit.field));
            if is_audio {
                self.preview_remote_audio(cx);
            }
            if let SettingsFocus::RemoteRow(field) = self.focused {
                self.sync_remote_editor(field, cx);
            }
            cx.notify();
            return;
        }
        let section_rows = rows(self.section(), self.diagnostics.len());
        let defaults = GuiConfig::default();
        let previous_layout = self.draft.layout;
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
        self.preview_layout_changes(previous_layout, cx);
        cx.notify();
    }

    fn reset_all(&mut self, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        let previous_layout = self.draft.layout;
        self.draft = GuiConfig::default();
        self.invalid_edits.clear();
        if let Some(defaults) = self.remote_defaults.clone() {
            self.remote_draft = Some(defaults);
            self.invalid_remote_edits.clear();
            self.preview_remote_audio(cx);
        }
        self.sync_editor_binding_mode(cx);
        if let SettingsFocus::Row(row) = self.focused {
            self.sync_row_editor(row, cx);
        }
        self.action_menu = None;
        self.preview(cx);
        self.preview_layout_changes(previous_layout, cx);
        cx.notify();
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        self.restore_layout_preview(cx);
        cx.emit(SettingsViewEvent::AppearanceCommand(
            local_rpc::appearance::AppearanceCommand::End {
                session_id: self.appearance_session,
            },
        ));
        cx.emit(SettingsViewEvent::Closed);
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
        if !self.invalid_edits.is_empty() || !self.invalid_remote_edits.is_empty() {
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
        let mut diagnostics = validate(&self.draft);
        diagnostics.extend(key_bindings::validate(&self.draft));
        if has_errors(&diagnostics) {
            self.diagnostics = diagnostics;
            self.status_message = Some("Fix validation errors before saving.".into());
            cx.notify();
            return;
        }
        if self.remote_dirty() {
            if let (Some(session_id), Some(settings), Some(baseline)) = (
                self.remote_session,
                self.remote_draft.clone(),
                self.remote_baseline.as_ref(),
            ) {
                let changes =
                    settings.changes(baseline, |field| self.remote_field(field).is_some());
                self.remote_saving = true;
                self.remote_pending_save = Some(settings.clone());
                cx.emit(SettingsViewEvent::Command(
                    wire_settings::SettingsCommand::Save {
                        session_id,
                        expected_revision: self.remote_revision,
                        changes,
                        force: self.remote_confirm_replace,
                    },
                ));
                self.remote_status = "Saving daemon settings…".into();
            } else {
                self.remote_status = "Daemon unavailable; daemon changes remain unsaved.".into();
            }
        }

        if !self.local_dirty() {
            self.commit_layout_preview(self.draft.layout);
            self.saving = self.remote_saving;
            self.status_message = Some(if self.remote_saving {
                "Saving daemon settings…".into()
            } else {
                "Nothing to save.".into()
            });
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
        let Some(path) = self.path.clone() else {
            self.status_message =
                Some("GUI config path unavailable; daemon save continues independently.".into());
            self.saving = self.remote_saving;
            cx.notify();
            return;
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
        self.saving = self.remote_saving;
        match result {
            Ok(source) => {
                let saved = self
                    .pending_save
                    .take()
                    .expect("successful save retains its exact configuration snapshot");
                let saved_config = saved.config.clone();
                self.confirm_replace = false;
                self.commit_layout_preview(saved_config.layout);
                key_bindings::apply_compiled(saved.bindings, cx);
                self.source = Some(source.clone());
                self.source_status = SourceStatus::Loaded;
                self.baseline = saved_config.clone();
                self.diagnostics = validate(&saved_config);
                self.diagnostics
                    .extend(key_bindings::validate(&saved_config));
                self.diagnostics.extend(theme::font_warnings(
                    &saved_config,
                    &self.available_families,
                ));
                self.committed = theme::apply_appearance(
                    &saved_config,
                    SourceStatus::Loaded,
                    &self.diagnostics,
                    &self.available_families,
                    cx,
                );
                cx.set_global(ConfigurationState(LoadedConfig {
                    path: self.path.clone(),
                    config: saved_config.clone(),
                    source: Some(source),
                    status: SourceStatus::Loaded,
                    diagnostics: self.diagnostics.clone(),
                }));
                self.commit_shared_appearance(&saved_config, cx);
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
        if let Some(session_id) = self.remote_session {
            self.remote_saving = true;
            self.remote_status = "Reloading daemon settings…".into();
            cx.emit(SettingsViewEvent::Command(
                wire_settings::SettingsCommand::Reload { session_id },
            ));
        }
        let Some(path) = self.path.clone() else {
            self.saving = self.remote_saving;
            self.status_message =
                Some("GUI config path unavailable; daemon reload continues independently.".into());
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
        self.saving = self.remote_saving;
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
                    let layout = self.draft.layout;
                    cx.set_global(ConfigurationState(LoadedConfig {
                        diagnostics: self.diagnostics.clone(),
                        ..loaded
                    }));
                    self.install_live_layout(layout, cx);
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

    fn toggle_value(&self, setting: ToggleSetting) -> bool {
        match setting {
            ToggleSetting::StatusBarVisible => self.draft.layout.status_bar_visible,
            ToggleSetting::RoomMenuVisible => self.draft.layout.room_menu_visible,
            ToggleSetting::NativeFullscreen => self.draft.native_fullscreen,
            ToggleSetting::VideoLoopByDefault => self.draft.video_loop_by_default,
            ToggleSetting::LiveLowDelayDecode => self.draft.live_low_delay_decode,
        }
    }

    fn set_toggle(&mut self, setting: ToggleSetting, value: bool, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        let before = self.draft.layout;
        match setting {
            ToggleSetting::StatusBarVisible => self.draft.layout.status_bar_visible = value,
            ToggleSetting::RoomMenuVisible => self.draft.layout.room_menu_visible = value,
            ToggleSetting::NativeFullscreen => self.draft.native_fullscreen = value,
            ToggleSetting::VideoLoopByDefault => self.draft.video_loop_by_default = value,
            ToggleSetting::LiveLowDelayDecode => self.draft.live_low_delay_decode = value,
        }
        self.preview_layout_changes(before, cx);
        cx.notify();
    }

    fn toggle(&mut self, setting: ToggleSetting, cx: &mut Context<Self>) {
        self.set_toggle(setting, !self.toggle_value(setting), cx);
    }

    fn preview_layout_changes(&mut self, before: LayoutConfig, cx: &mut Context<Self>) {
        let status_bar_visible =
            (before.status_bar_visible != self.draft.layout.status_bar_visible).then(|| {
                self.layout_previewed.status_bar_visible = true;
                self.draft.layout.status_bar_visible
            });
        let room_menu_visible = (before.room_menu_visible != self.draft.layout.room_menu_visible)
            .then(|| {
                self.layout_previewed.room_menu_visible = true;
                self.draft.layout.room_menu_visible
            });
        if status_bar_visible.is_some() || room_menu_visible.is_some() {
            cx.emit(SettingsViewEvent::LocalLayoutPreview {
                status_bar_visible,
                room_menu_visible,
            });
        }
    }

    fn restore_layout_preview(&mut self, cx: &mut Context<Self>) {
        let status_bar_visible = self
            .layout_previewed
            .status_bar_visible
            .then_some(self.layout_preview_baseline.status_bar_visible);
        let room_menu_visible = self
            .layout_previewed
            .room_menu_visible
            .then_some(self.layout_preview_baseline.room_menu_visible);
        if status_bar_visible.is_some() || room_menu_visible.is_some() {
            cx.emit(SettingsViewEvent::LocalLayoutPreview {
                status_bar_visible,
                room_menu_visible,
            });
        }
        self.layout_previewed = LayoutPreviewed::default();
    }

    fn commit_layout_preview(&mut self, layout: LayoutConfig) {
        if self.layout_previewed.status_bar_visible {
            self.layout_preview_baseline.status_bar_visible = layout.status_bar_visible;
        }
        if self.layout_previewed.room_menu_visible {
            self.layout_preview_baseline.room_menu_visible = layout.room_menu_visible;
        }
        self.layout_previewed = LayoutPreviewed::default();
    }

    fn install_live_layout(&mut self, layout: LayoutConfig, cx: &mut Context<Self>) {
        self.layout_preview_baseline = layout;
        self.layout_previewed = LayoutPreviewed::default();
        cx.emit(SettingsViewEvent::LocalLayoutPreview {
            status_bar_visible: Some(layout.status_bar_visible),
            room_menu_visible: Some(layout.room_menu_visible),
        });
    }

    fn row_actions(&self, row: RowRef) -> Vec<(SharedString, RowAction)> {
        let mut actions = match row {
            RowRef::Theme(role) => vec![
                ("Pick color".into(), RowAction::PickColor(role)),
                ("Reset".into(), RowAction::Reset),
            ],
            _ => vec![("Reset".into(), RowAction::Reset)],
        };
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

    fn run_row_action(
        &mut self,
        row: RowRef,
        index: usize,
        window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        let Some((_, action)) = self.row_actions(row).get(index).cloned() else {
            return;
        };
        match action {
            RowAction::Reset => self.reset_row(row, cx),
            RowAction::PickColor(role) => self.open_color_picker(role, window, cx),
            RowAction::Record(scope, command) => self.start_recording(scope, command, cx),
            RowAction::Font(role, family) => self.choose_font(role, family, cx),
        }
    }

    fn activate_focused(&mut self, window: &mut gpui::Window, cx: &mut Context<Self>) {
        match self.focused {
            SettingsFocus::Search => self.focus_target(SettingsFocus::Search, true, window, cx),
            SettingsFocus::Row(RowRef::Choice(setting)) => self.cycle_choice(setting, 1, cx),
            SettingsFocus::Row(RowRef::Toggle(setting)) => self.toggle(setting, cx),
            SettingsFocus::Row(row) if Self::row_has_editor(row) => {
                self.focus_target(SettingsFocus::Row(row), true, window, cx)
            }
            SettingsFocus::Row(_) => {}
            SettingsFocus::RemoteRow(field) if self.remote_field_is_text(field) => {
                self.focus_target(SettingsFocus::RemoteRow(field), true, window, cx)
            }
            SettingsFocus::RemoteRow(field) => self.change_remote_field(field, 1, window, cx),
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

        if let Some(picker) = self.choice_picker.as_ref() {
            let mode = picker.search.read(cx).mode();
            match key {
                "escape" => self.close_choice_picker(window, cx),
                "down" | "tab" if !modifiers.shift => self.move_choice_picker_selection(1, cx),
                "up" | "tab" => self.move_choice_picker_selection(-1, cx),
                "j" if mode == Mode::Normal => self.move_choice_picker_selection(1, cx),
                "k" if mode == Mode::Normal => self.move_choice_picker_selection(-1, cx),
                "enter" => self.choose_selected_choice(window, cx),
                _ => return,
            }
            window.prevent_default();
            cx.stop_propagation();
            return;
        }

        if self.section_menu_open && key == "escape" {
            self.dismiss_section_menu(cx);
            window.prevent_default();
            cx.stop_propagation();
            return;
        }

        if key == "tab" {
            let delta = if modifiers.shift { -1 } else { 1 };
            let next = (self.active_section as isize + delta)
                .rem_euclid((SETTINGS_SECTIONS.len() + self.remote_sections.len()) as isize)
                as usize;
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
                "enter" => self.run_row_action(row, selected, window, cx),
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
                | (
                    SettingsFocus::RemoteRow(_),
                    Some(EditorTarget::RemoteRow(_))
                )
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
                    } else if let SettingsFocus::Row(RowRef::Toggle(setting)) = self.focused {
                        self.set_toggle(setting, false, cx);
                    } else if let SettingsFocus::RemoteRow(field) = self.focused
                        && !self.remote_field_is_text(field)
                    {
                        self.change_remote_field(field, -1, window, cx);
                    } else {
                        return;
                    }
                }
                "left" => {
                    if let SettingsFocus::Row(RowRef::Choice(setting)) = self.focused {
                        self.cycle_choice(setting, -1, cx);
                    } else if let SettingsFocus::Row(RowRef::Toggle(setting)) = self.focused {
                        self.set_toggle(setting, false, cx);
                    } else if let SettingsFocus::RemoteRow(field) = self.focused
                        && !self.remote_field_is_text(field)
                    {
                        self.change_remote_field(field, -1, window, cx);
                    } else {
                        return;
                    }
                }
                "l" if binding_mode == BindingMode::Vim => {
                    if let SettingsFocus::Row(RowRef::Choice(setting)) = self.focused {
                        self.cycle_choice(setting, 1, cx);
                    } else if let SettingsFocus::Row(RowRef::Toggle(setting)) = self.focused {
                        self.set_toggle(setting, true, cx);
                    } else if let SettingsFocus::RemoteRow(field) = self.focused
                        && !self.remote_field_is_text(field)
                    {
                        self.change_remote_field(field, 1, window, cx);
                    } else {
                        return;
                    }
                }
                "right" => {
                    if let SettingsFocus::Row(RowRef::Choice(setting)) = self.focused {
                        self.cycle_choice(setting, 1, cx);
                    } else if let SettingsFocus::Row(RowRef::Toggle(setting)) = self.focused {
                        self.set_toggle(setting, true, cx);
                    } else if let SettingsFocus::RemoteRow(field) = self.focused
                        && !self.remote_field_is_text(field)
                    {
                        self.change_remote_field(field, 1, window, cx);
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
                .on_click(move |_, window, cx| {
                    let _ = action_view
                        .update(cx, |this, cx| this.run_row_action(row, index, window, cx));
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
        let logical_width = f32::from(window.viewport_size().width)
            / f32::from(crate::ui_scale::rem_size(cx))
            * crate::ui_scale::BASE_REM_SIZE;
        let logical_height = f32::from(window.viewport_size().height)
            / f32::from(crate::ui_scale::rem_size(cx))
            * crate::ui_scale::BASE_REM_SIZE;
        let compact = logical_width < 900.0;
        let compact_menu_max_height = (logical_height - 240.0).clamp(120.0, 420.0);
        let remote_section = self.remote_section();
        let (section_title, section_help) = match remote_section {
            Some(section) => self
                .remote_sections
                .get(section)
                .map(|section| (section.title.clone(), section.help.clone()))
                .unwrap_or_default(),
            None => {
                let section = self.section();
                (section.title.to_string(), section.help.to_string())
            }
        };
        let visible_rows = self.visible_rows();
        let visible_remote_fields = self.visible_remote_fields();
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
        let list: AnyElement = if remote_section.is_some() {
            let fields = visible_remote_fields.clone();
            let remote_sections = self.remote_sections.clone();
            let remote_draft = self.remote_draft.clone();
            let invalid = self.invalid_remote_edits.clone();
            let remote_editor = row_editor.clone();
            uniform_list(
                ("settings-remote-rows", self.active_section),
                fields.len(),
                move |range, _, _| {
                    range
                        .map(|index| {
                            let field = fields[index];
                            let editor = remote_editor.as_ref().and_then(|(target, entity)| {
                                (*target == EditorTarget::RemoteRow(field)).then(|| entity.clone())
                            });
                            let invalid = invalid.iter().find(|edit| edit.field == field);
                            let descriptor = remote::field(&remote_sections, field)
                                .expect("visible remote field has a descriptor");
                            render_remote_row(
                                descriptor,
                                remote_draft.as_ref(),
                                focused == SettingsFocus::RemoteRow(field),
                                editor,
                                invalid.map(|edit| edit.error.clone()),
                                row_view.clone(),
                                &row_palette,
                                compact,
                            )
                        })
                        .collect::<Vec<_>>()
                },
            )
            .track_scroll(&self.scroll)
            .w_full()
            .flex_1()
            .into_any_element()
        } else {
            let list_rows = visible_rows.clone();
            uniform_list(
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
                                compact,
                            )
                        })
                        .collect::<Vec<_>>()
                },
            )
            .track_scroll(&self.scroll)
            .w_full()
            .flex_1()
            .into_any_element()
        };

        let navigation = if compact {
            let trigger_view = view.clone();
            let reset_all_view = view.clone();
            let section_menu = self.section_menu_open.then(|| {
                let mut menu = div()
                    .id("settings-section-menu-popup")
                    .absolute()
                    .top(relative(1.))
                    .left(rems_from_px(12.))
                    .right(rems_from_px(12.))
                    .mt(rems_from_px(4.))
                    .max_h(rems_from_px(compact_menu_max_height))
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .p_2()
                    .border_1()
                    .border_color(palette.color(ThemeRole::BorderStrong))
                    .bg(palette.color(ThemeRole::Raised))
                    .shadow_lg()
                    .occlude()
                    .on_mouse_down_out(cx.listener(|this, event: &MouseDownEvent, _, cx| {
                        if this
                            .section_menu_trigger_bounds
                            .get()
                            .is_some_and(|bounds| bounds.contains(&event.position))
                        {
                            return;
                        }
                        this.dismiss_section_menu(cx);
                    }))
                    .child(
                        div()
                            .px_2()
                            .pb_1()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(palette.color(ThemeRole::TextDim))
                            .child("Renderer"),
                    );
                for (index, candidate) in SETTINGS_SECTIONS.iter().enumerate() {
                    let selected = index == self.active_section;
                    let section_view = view.clone();
                    menu = menu.child(
                        setting_button(candidate.id, candidate.title, selected, &palette)
                            .w_full()
                            .on_click(move |_, window, cx| {
                                let _ = section_view
                                    .update(cx, |this, cx| this.select_section(index, window, cx));
                            }),
                    );
                }
                if !self.remote_sections.is_empty() {
                    menu = menu.child(
                        div()
                            .px_2()
                            .pt_3()
                            .pb_1()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(palette.color(ThemeRole::TextDim))
                            .child("Chatt daemon"),
                    );
                }
                for (remote_index, candidate) in self.remote_sections.iter().enumerate() {
                    let index = SETTINGS_SECTIONS.len() + remote_index;
                    let selected = index == self.active_section;
                    let section_view = view.clone();
                    menu = menu.child(
                        setting_button(
                            candidate.id.clone(),
                            candidate.title.clone(),
                            selected,
                            &palette,
                        )
                        .w_full()
                        .on_click(move |_, window, cx| {
                            let _ = section_view
                                .update(cx, |this, cx| this.select_section(index, window, cx));
                        }),
                    );
                }
                menu.child(
                    div()
                        .mt_2()
                        .pt_2()
                        .border_t_1()
                        .border_color(palette.color(ThemeRole::BorderSubtle))
                        .child(
                            setting_button(
                                "reset-all",
                                "Reset all",
                                focused == SettingsFocus::ResetAll,
                                &palette,
                            )
                            .w_full()
                            .on_click(move |_, window, cx| {
                                let _ = reset_all_view.update(cx, |this, cx| {
                                    this.section_menu_open = false;
                                    this.focus_target(SettingsFocus::ResetAll, false, window, cx);
                                    this.reset_all(cx);
                                });
                            }),
                        ),
                )
            });
            let trigger_bounds = self.section_menu_trigger_bounds.clone();
            div()
                .relative()
                .w_full()
                .flex_none()
                .p_3()
                .border_b_1()
                .border_color(palette.color(ThemeRole::BorderSubtle))
                .child(
                    div()
                        .id("settings-section-menu")
                        .relative()
                        .w_full()
                        .min_h(rems_from_px(40.))
                        .flex()
                        .items_center()
                        .gap_3()
                        .px_3()
                        .py_2()
                        .cursor_pointer()
                        .bg(if self.section_menu_open {
                            palette.color(ThemeRole::ControlActive)
                        } else {
                            palette.color(ThemeRole::ControlSurface)
                        })
                        .text_color(if self.section_menu_open {
                            palette.color(ThemeRole::ControlActiveText)
                        } else {
                            palette.color(ThemeRole::TextSecondary)
                        })
                        .hover({
                            let hover = palette.color(ThemeRole::ControlSurfaceHover);
                            move |button| button.bg(hover)
                        })
                        .child(
                            canvas(
                                move |bounds, _, _| trigger_bounds.set(Some(bounds)),
                                |_, _, _, _| {},
                            )
                            .absolute()
                            .size_full(),
                        )
                        .child(icon(
                            IconName::Menu,
                            18.0,
                            if self.section_menu_open {
                                palette.color(ThemeRole::ControlActiveText)
                            } else {
                                palette.color(ThemeRole::TextSecondary)
                            },
                        ))
                        .child(
                            div()
                                .min_w_0()
                                .flex_1()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(section_title.clone()),
                        )
                        .child(
                            div()
                                .flex_none()
                                .text_xs()
                                .text_color(if self.section_menu_open {
                                    palette.color(ThemeRole::ControlActiveText)
                                } else {
                                    palette.color(ThemeRole::TextDim)
                                })
                                .child(if self.section_menu_open {
                                    "Close"
                                } else {
                                    "Sections"
                                }),
                        )
                        .on_click(move |_, _, cx| {
                            let _ =
                                trigger_view.update(cx, |this, cx| this.toggle_section_menu(cx));
                        }),
                )
                .when_some(section_menu, |navigation, menu| {
                    navigation.child(deferred(menu))
                })
        } else {
            let reset_all_view = view.clone();
            let mut navigation = div()
                .w(rems_from_px(190.))
                .flex_none()
                .flex()
                .flex_col()
                .gap_1()
                .p_3()
                .border_r_1()
                .border_color(palette.color(ThemeRole::BorderSubtle))
                .child(
                    div()
                        .px_2()
                        .pb_1()
                        .text_xs()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(palette.color(ThemeRole::TextDim))
                        .child("Renderer"),
                );
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
            navigation = navigation.child(
                div()
                    .px_2()
                    .pt_3()
                    .pb_1()
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(palette.color(ThemeRole::TextDim))
                    .child("Chatt daemon"),
            );
            for (remote_index, candidate) in self.remote_sections.iter().enumerate() {
                let index = SETTINGS_SECTIONS.len() + remote_index;
                let selected = index == self.active_section;
                let section_view = view.clone();
                navigation = navigation.child(
                    setting_button(
                        candidate.id.clone(),
                        candidate.title.clone(),
                        selected,
                        &palette,
                    )
                    .w_full()
                    .on_click(move |_, window, cx| {
                        let _ = section_view
                            .update(cx, |this, cx| this.select_section(index, window, cx));
                    }),
                );
            }
            navigation.child(div().flex_1()).child(
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
            )
        };

        let search_editor = active_editor
            .as_ref()
            .and_then(|(target, entity)| (*target == EditorTarget::Search).then(|| entity.clone()));
        let search_view = view.clone();
        let search = div()
            .id("settings-search")
            .w(rems_from_px(360.))
            .max_w_full()
            .min_h(rems_from_px(38.))
            .flex()
            .items_center()
            .px_3()
            .py_2()
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
        } else if self.confirm_replace || self.remote_confirm_replace {
            "Confirm replace"
        } else if replace {
            "Replace invalid file"
        } else {
            "Save"
        };
        let footer_status = if remote_section.is_some() {
            Some(self.remote_status.clone())
        } else {
            self.status_message.clone()
        };
        let audio_toolbar = self.remote_section_is_audio().then(|| {
            let loopback_view = view.clone();
            let advanced_view = view.clone();
            let meter_width = 220.0 * (self.remote_meter_rms * 7.0).clamp(0.0, 1.0);
            let peak_width = 220.0 * (self.remote_meter_peak * 5.0).clamp(0.0, 1.0);
            div()
                .min_h(rems_from_px(54.))
                .flex_none()
                .flex()
                .items_center()
                .gap_2()
                .px_5()
                .border_b_1()
                .border_color(palette.color(ThemeRole::BorderSubtle))
                .child(
                    div()
                        .w(rems_from_px(220.))
                        .h(rems_from_px(10.))
                        .relative()
                        .overflow_hidden()
                        .bg(palette.color(ThemeRole::ControlSurface))
                        .child(
                            div()
                                .absolute()
                                .top_0()
                                .bottom_0()
                                .left_0()
                                .w(rems_from_px(peak_width))
                                .bg(palette.color(ThemeRole::StateWarning)),
                        )
                        .child(
                            div()
                                .absolute()
                                .top_0()
                                .bottom_0()
                                .left_0()
                                .w(rems_from_px(meter_width))
                                .bg(palette.color(ThemeRole::StateSuccess)),
                        ),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(palette.color(ThemeRole::TextDim))
                        .child(
                            if self
                                .remote_runtime
                                .as_ref()
                                .is_some_and(|runtime| runtime.applying)
                            {
                                "Applying audio…"
                            } else {
                                "Microphone level"
                            },
                        ),
                )
                .child(div().flex_1())
                .child(
                    setting_button(
                        "audio-loopback",
                        if !self
                            .remote_actions
                            .as_ref()
                            .is_some_and(|actions| actions.audio_loopback)
                        {
                            "Loopback unavailable"
                        } else if self.remote_loopback {
                            "Loopback on"
                        } else {
                            "Test loopback"
                        },
                        self.remote_loopback,
                        &palette,
                    )
                    .on_click(move |_, _, cx| {
                        let _ =
                            loopback_view.update(cx, |this, cx| this.toggle_remote_loopback(cx));
                    }),
                )
                .child(
                    setting_button(
                        "audio-advanced",
                        if self.remote_advanced {
                            "Hide advanced"
                        } else {
                            "Advanced"
                        },
                        self.remote_advanced,
                        &palette,
                    )
                    .on_click(move |_, _, cx| {
                        let _ = advanced_view.update(cx, |this, cx| {
                            this.remote_advanced = !this.remote_advanced;
                            this.scroll.scroll_to_item(0, gpui::ScrollStrategy::Top);
                            cx.notify();
                        });
                    }),
                )
                .into_any_element()
        });
        let choice_picker = self.choice_picker.as_ref().and_then(|picker| {
            let descriptor = self.remote_field(picker.field)?;
            Some(render_choice_picker(
                picker.field,
                descriptor.label.clone(),
                self.choice_picker_items(picker.field, &picker.query)
                    .cloned()
                    .collect(),
                self.current_choice_selection(picker.field)
                    .map(str::to_owned),
                picker.selected,
                picker.search.clone(),
                picker.scroll.clone(),
                view.clone(),
                &palette,
            ))
        });
        let color_picker = self.color_picker.as_ref().and_then(|picker| {
            if self.focused != SettingsFocus::Row(RowRef::Theme(picker.role)) {
                return None;
            }
            Some(render_color_picker(picker, view.clone(), &palette))
        });
        window.set_rem_size(crate::ui_scale::rem_size(cx));
        div()
            .id("settings")
            .key_context("ChattSettings ChattModal")
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
                    .border_1()
                    .border_color(palette.color(ThemeRole::BorderStrong))
                    .bg(palette.color(ThemeRole::Raised))
                    .shadow_lg()
                    .flex()
                    .flex_col()
                    .font_family(AppliedSettings::get(cx).fonts.interface_family.clone())
                    .child(
                        div()
                            .min_h(rems_from_px(64.))
                            .flex_none()
                            .flex()
                            .flex_wrap()
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
                        div()
                            .flex_1()
                            .min_h_0()
                            .flex()
                            .when(compact, |body| body.flex_col())
                            .child(navigation)
                            .child(
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
                                                    .child(section_title),
                                            )
                                            .child(
                                                div()
                                                    .text_sm()
                                                    .text_color(palette.color(ThemeRole::TextMuted))
                                                    .child(section_help),
                                            ),
                                    )
                                    .when_some(audio_toolbar, |content, toolbar| {
                                        content.child(toolbar)
                                    })
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
                    .when_some(color_picker, |modal, picker| modal.child(picker))
                    .child(
                        div()
                            .min_h(rems_from_px(58.))
                            .flex_none()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap_2()
                            .px_4()
                            .border_t_1()
                            .border_color(palette.color(ThemeRole::BorderSubtle))
                            .when_some(footer_status.clone(), |footer, message| {
                                footer.child(
                                    div()
                                        .flex_1()
                                        .text_sm()
                                        .text_color(palette.color(ThemeRole::TextMuted))
                                        .child(message),
                                )
                            })
                            .when(footer_status.is_none(), |footer| {
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
            .when_some(choice_picker, |root, picker| root.child(picker))
    }
}

impl Focusable for SettingsView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.choice_picker
            .as_ref()
            .map(|picker| picker.search.focus_handle(cx))
            .or_else(|| {
                self.editor
                    .as_ref()
                    .map(|editor| editor.entity.focus_handle(cx))
            })
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
    compact: bool,
) -> AnyElement {
    if let RowRef::Diagnostic(index) = row {
        return render_diagnostic_row(index, diagnostics, draft, palette, source, compact);
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
        RowRef::Toggle(ToggleSetting::StatusBarVisible) => {
            if draft.layout.status_bar_visible {
                "On".into()
            } else {
                "Off".into()
            }
        }
        RowRef::Toggle(ToggleSetting::RoomMenuVisible) => {
            if draft.layout.room_menu_visible {
                "On".into()
            } else {
                "Off".into()
            }
        }
        RowRef::Toggle(ToggleSetting::NativeFullscreen) => {
            if draft.native_fullscreen {
                "On".into()
            } else {
                "Off".into()
            }
        }
        RowRef::Toggle(ToggleSetting::VideoLoopByDefault) => {
            if draft.video_loop_by_default {
                "On".into()
            } else {
                "Off".into()
            }
        }
        RowRef::Toggle(ToggleSetting::LiveLowDelayDecode) => {
            if draft.live_low_delay_decode {
                "On".into()
            } else {
                "Off".into()
            }
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
    let color_view = view.clone();
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
            .w(rems_from_px(330.))
            .max_w_full()
            .min_h(rems_from_px(36.))
            .flex()
            .items_center()
            .px_3()
            .py_2()
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
            .max_w(rems_from_px(330.))
            .px_3()
            .py_2()
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
    } else if let RowRef::Toggle(setting) = row {
        let enabled = match setting {
            ToggleSetting::StatusBarVisible => draft.layout.status_bar_visible,
            ToggleSetting::RoomMenuVisible => draft.layout.room_menu_visible,
            ToggleSetting::NativeFullscreen => draft.native_fullscreen,
            ToggleSetting::VideoLoopByDefault => draft.video_loop_by_default,
            ToggleSetting::LiveLowDelayDecode => draft.live_low_delay_decode,
        };
        div()
            .id(("settings-toggle", setting as usize))
            .max_w(rems_from_px(330.))
            .flex()
            .items_center()
            .gap_2()
            .cursor_pointer()
            .child(
                div()
                    .w(rems_from_px(42.))
                    .h(rems_from_px(24.))
                    .p(rems_from_px(3.))
                    .flex()
                    .items_center()
                    .when(enabled, |track| track.justify_end())
                    .rounded_full()
                    .bg(if enabled {
                        palette.color(ThemeRole::ControlActive)
                    } else {
                        palette.color(ThemeRole::ControlSurface)
                    })
                    .child(div().size(rems_from_px(18.)).rounded_full().bg(if enabled {
                        palette.color(ThemeRole::ControlActiveText)
                    } else {
                        palette.color(ThemeRole::TextMuted)
                    })),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(palette.color(ThemeRole::TextSecondary))
                    .child(row_value),
            )
            .on_click(move |_, window, cx| {
                let _ = choice_view.update(cx, |this, cx| {
                    this.focus_target(SettingsFocus::Row(row), false, window, cx);
                    this.toggle(setting, cx);
                });
            })
            .into_any_element()
    } else {
        div()
            .max_w(rems_from_px(330.))
            .truncate()
            .text_sm()
            .text_color(diagnostic_color)
            .child(row_value)
            .into_any_element()
    };
    div()
        .id(("settings-row", row_id(row)))
        .h(rems_from_px(if compact { 132.0 } else { 84.0 }))
        .w_full()
        .flex()
        .when(compact, |row| row.flex_wrap())
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
                RowRef::Theme(role) => Some((role, draft.theme.color(role).packed())),
                _ => None,
            },
            |row, (role, color)| {
                row.child(
                    div()
                        .id(("settings-color-swatch", role as usize))
                        .size(rems_from_px(30.))
                        .flex_none()
                        .border_1()
                        .border_color(palette.color(ThemeRole::BorderFocus))
                        .bg(rgba(color))
                        .cursor_pointer()
                        .on_click(move |_, window, cx| {
                            let _ = color_view
                                .update(cx, |this, cx| this.open_color_picker(role, window, cx));
                            cx.stop_propagation();
                        }),
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
                .when(compact, |column| column.w_full().flex_auto())
                .when(!compact, |column| {
                    column.w(rems_from_px(330.)).max_w_full().flex_none()
                })
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

fn render_remote_row(
    descriptor: &wire_settings::SettingsField,
    draft: Option<&RemoteValues>,
    focused: bool,
    editor: Option<Entity<TextEditor>>,
    editor_error: Option<SharedString>,
    view: WeakEntity<SettingsView>,
    palette: &ThemePalette,
    compact: bool,
) -> AnyElement {
    let field = descriptor.id;
    let enabled = draft.is_some();
    let display = draft
        .map(|draft| remote::value(draft, descriptor))
        .unwrap_or_else(|| "Unavailable".into());
    let select_view = view.clone();
    let choice_view = view.clone();
    let reset_view = view.clone();
    let value = if remote::is_text(descriptor) {
        if let Some(editor) = editor {
            div()
                .id(("settings-remote-editor", remote_field_id(field)))
                .max_w(rems_from_px(330.))
                .min_h(rems_from_px(38.))
                .px_3()
                .py_2()
                .border_1()
                .border_color(if focused {
                    palette.color(ThemeRole::BorderFocus)
                } else {
                    palette.color(ThemeRole::BorderStrong)
                })
                .bg(palette.color(ThemeRole::Input))
                .child(editor)
                .into_any_element()
        } else {
            div()
                .id(("settings-remote-text", remote_field_id(field)))
                .max_w(rems_from_px(330.))
                .px_3()
                .py_2()
                .cursor_text()
                .bg(palette.color(ThemeRole::Input))
                .text_sm()
                .text_color(palette.color(ThemeRole::TextSecondary))
                .child(if display.is_empty() {
                    "Use platform default".to_string()
                } else {
                    display
                })
                .on_click(move |_, window, cx| {
                    let _ = choice_view.update(cx, |this, cx| {
                        this.focus_target(SettingsFocus::RemoteRow(field), true, window, cx);
                    });
                })
                .into_any_element()
        }
    } else if descriptor.control.kind == wire_settings::CONTROL_SEARCHABLE_CHOICE {
        div()
            .id(("settings-remote-picker", remote_field_id(field)))
            .max_w(rems_from_px(330.))
            .px_3()
            .py_2()
            .cursor_pointer()
            .bg(palette.color(ThemeRole::ControlSurface))
            .text_sm()
            .text_color(if enabled {
                palette.color(ThemeRole::TextSecondary)
            } else {
                palette.color(ThemeRole::TextDim)
            })
            .flex()
            .items_center()
            .gap_2()
            .child(div().flex_1().min_w_0().truncate().child(display))
            .child(
                div()
                    .text_xs()
                    .text_color(palette.color(ThemeRole::TextDim))
                    .child("⌄"),
            )
            .on_click(move |_, window, cx| {
                let _ = choice_view.update(cx, |this, cx| {
                    this.open_choice_picker(field, window, cx);
                });
                cx.stop_propagation();
            })
            .into_any_element()
    } else {
        div()
            .id(("settings-remote-choice", remote_field_id(field)))
            .max_w(rems_from_px(330.))
            .px_3()
            .py_2()
            .cursor_pointer()
            .bg(palette.color(ThemeRole::ControlSurface))
            .text_sm()
            .text_color(if enabled {
                palette.color(ThemeRole::TextSecondary)
            } else {
                palette.color(ThemeRole::TextDim)
            })
            .child(format!("‹  {display}  ›"))
            .on_click(move |_, window, cx| {
                let _ = choice_view.update(cx, |this, cx| {
                    this.focus_target(SettingsFocus::RemoteRow(field), false, window, cx);
                    this.change_remote_field(field, 1, window, cx);
                });
            })
            .into_any_element()
    };
    div()
        .id(("settings-remote-row", remote_field_id(field)))
        .h(rems_from_px(if compact { 132.0 } else { 84.0 }))
        .w_full()
        .flex()
        .when(compact, |row| row.flex_wrap())
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
            let _ = select_view.update(cx, |this, cx| this.select_remote_row(field, window, cx));
        })
        .child(
            div()
                .flex_1()
                .min_w_0()
                .child(
                    div()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(descriptor.label.clone()),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(palette.color(ThemeRole::TextDim))
                        .child(descriptor.key.clone()),
                )
                .child(
                    div()
                        .text_xs()
                        .truncate()
                        .text_color(palette.color(ThemeRole::TextSubtle))
                        .child(descriptor.help.clone()),
                ),
        )
        .child(
            div()
                .when(compact, |column| column.w_full().flex_auto())
                .when(!compact, |column| {
                    column.w(rems_from_px(330.)).max_w_full().flex_none()
                })
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
        .child(
            setting_button(
                ("reset-remote-row", remote_field_id(field)),
                "Reset",
                false,
                palette,
            )
            .on_click(move |_, _, cx| {
                let _ = reset_view.update(cx, |this, cx| this.reset_remote_field(field, cx));
            }),
        )
        .into_any_element()
}

fn render_color_picker(
    picker: &ColorPicker,
    view: WeakEntity<SettingsView>,
    palette: &ThemePalette,
) -> AnyElement {
    const WHEEL_SIZE: f32 = 210.0;

    let role = picker.role;
    let hsva = picker.hsva;
    let current = hsva.to_rgba8();
    let original = picker.original;
    let opaque_packed = (current.packed() & 0xffff_ff00) | 0xff;
    let transparent_packed = current.packed() & 0xffff_ff00;
    let wheel_bounds = picker.wheel_bounds.clone();
    let alpha_bounds = picker.alpha_bounds.clone();

    let wheel_view = view.clone();
    let alpha_view = view.clone();
    let move_view = view.clone();
    let up_view = view.clone();
    let out_view = view.clone();
    let cancel_view = view.clone();
    let apply_view = view.clone();

    let wheel = div()
        .id(("settings-hsv-color-wheel", role as usize))
        .relative()
        .size(rems_from_px(WHEEL_SIZE))
        .flex_none()
        .cursor(gpui::CursorStyle::Crosshair)
        .on_mouse_down(MouseButton::Left, move |event: &MouseDownEvent, _, cx| {
            let _ = wheel_view.update(cx, |this, cx| {
                this.begin_color_picker_drag(event, cx);
            });
            cx.stop_propagation();
        })
        .child(
            canvas(
                move |bounds, _, _| wheel_bounds.set(Some(bounds)),
                move |bounds, _, window, _| {
                    window.paint_hsv_color_wheel(bounds, hsva.hue, hsva.saturation, hsva.value);
                },
            )
            .absolute()
            .size_full(),
        );

    let alpha = div()
        .id(("settings-color-alpha", role as usize))
        .relative()
        .w_full()
        .h(rems_from_px(24.0))
        .flex_none()
        .overflow_hidden()
        .cursor(gpui::CursorStyle::ResizeLeftRight)
        .bg(palette.color(ThemeRole::ControlSurface))
        .on_mouse_down(MouseButton::Left, move |event: &MouseDownEvent, _, cx| {
            let _ = alpha_view.update(cx, |this, cx| {
                this.begin_color_picker_drag(event, cx);
            });
            cx.stop_propagation();
        })
        .child(
            div()
                .absolute()
                .inset_0()
                .bg(checkerboard(rgba(0xffffff30), 8.0)),
        )
        .child(div().absolute().inset_0().bg(linear_gradient(
            90.0,
            linear_color_stop(rgba(transparent_packed), 0.0),
            linear_color_stop(rgba(opaque_packed), 1.0),
        )))
        .child(
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .left(gpui::relative(hsva.alpha))
                .ml(rems_from_px(-3.0))
                .w(rems_from_px(6.0))
                .border_1()
                .border_color(rgba(0xffffffff))
                .bg(rgba(0x11111180)),
        )
        .child(
            canvas(
                move |bounds, _, _| alpha_bounds.set(Some(bounds)),
                |_, _, _, _| {},
            )
            .absolute()
            .size_full(),
        );

    let swatch = |id: &'static str, color: Rgba8| {
        div()
            .id((id, role as usize))
            .size(rems_from_px(42.0))
            .relative()
            .overflow_hidden()
            .border_1()
            .border_color(palette.color(ThemeRole::BorderStrong))
            .bg(palette.color(ThemeRole::ControlSurface))
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .bg(checkerboard(rgba(0xffffff30), 7.0)),
            )
            .child(div().absolute().inset_0().bg(rgba(color.packed())))
    };

    div()
        .id("settings-color-picker-panel")
        .h(rems_from_px(258.0))
        .flex_none()
        .flex()
        .items_center()
        .gap_5()
        .px_5()
        .py_4()
        .overflow_hidden()
        .border_t_1()
        .border_color(palette.color(ThemeRole::BorderStrong))
        .bg(palette.color(ThemeRole::Toolbar))
        .shadow_lg()
        .on_mouse_move(move |event: &MouseMoveEvent, _, cx| {
            let _ = move_view.update(cx, |this, cx| this.drag_color_picker(event, cx));
        })
        .on_mouse_up(MouseButton::Left, move |_: &MouseUpEvent, _, cx| {
            let _ = up_view.update(cx, |this, _| this.finish_color_picker_drag());
        })
        .on_mouse_up_out(MouseButton::Left, move |_: &MouseUpEvent, _, cx| {
            let _ = out_view.update(cx, |this, _| this.finish_color_picker_drag());
        })
        .child(
            div()
                .w(rems_from_px(180.0))
                .h_full()
                .flex_none()
                .flex()
                .flex_col()
                .child(
                    div()
                        .text_xs()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(palette.color(ThemeRole::TextDim))
                        .child("COLOR EDITOR"),
                )
                .child(
                    div()
                        .mt_2()
                        .text_lg()
                        .font_weight(FontWeight::BOLD)
                        .child(label(RowRef::Theme(role))),
                )
                .child(
                    div()
                        .text_xs()
                        .truncate()
                        .text_color(palette.color(ThemeRole::TextMuted))
                        .child(path(RowRef::Theme(role))),
                )
                .child(div().flex_1())
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_3()
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(swatch("settings-color-original", original))
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(palette.color(ThemeRole::TextDim))
                                        .child("Original"),
                                ),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(swatch("settings-color-current", current))
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(palette.color(ThemeRole::TextDim))
                                        .child("Current"),
                                ),
                        ),
                ),
        )
        .child(wheel)
        .child(
            div()
                .h_full()
                .w(rems_from_px(1.0))
                .flex_none()
                .bg(palette.color(ThemeRole::BorderStrong)),
        )
        .child(
            div()
                .h_full()
                .min_w_0()
                .flex_1()
                .flex()
                .flex_col()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .child(
                            div()
                                .text_xs()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child("Opacity"),
                        )
                        .child(div().flex_1())
                        .child(
                            div()
                                .text_xs()
                                .text_color(palette.color(ThemeRole::TextMuted))
                                .child(format!("{:.0}%", hsva.alpha * 100.0)),
                        ),
                )
                .child(alpha)
                .child(div().flex_1())
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_xs()
                                .truncate()
                                .text_color(palette.color(ThemeRole::TextDim))
                                .child("Drag to preview · Apply keeps · Cancel restores"),
                        )
                        .child(
                            setting_button("cancel-color-picker", "Cancel", false, palette)
                                .on_click(move |_, window, cx| {
                                    let _ = cancel_view.update(cx, |this, cx| {
                                        this.cancel_color_picker(window, cx);
                                    });
                                }),
                        )
                        .child(
                            setting_button("apply-color-picker", "Apply", true, palette).on_click(
                                move |_, window, cx| {
                                    let _ = apply_view.update(cx, |this, cx| {
                                        this.apply_color_picker(window, cx);
                                    });
                                },
                            ),
                        ),
                ),
        )
        .into_any_element()
}

fn render_choice_picker(
    field: RemoteField,
    title: String,
    items: Vec<wire_settings::SettingsChoice>,
    current: Option<String>,
    selected: usize,
    search: Entity<TextEditor>,
    scroll: UniformListScrollHandle,
    view: WeakEntity<SettingsView>,
    palette: &ThemePalette,
) -> AnyElement {
    let selectable_count = items.iter().filter(|choice| choice.enabled).count();
    let item_count = items.len();
    let list: AnyElement = if items.is_empty() {
        div()
            .h(rems_from_px(330.))
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_2()
            .text_color(palette.color(ThemeRole::TextMuted))
            .child("No choices match this search.")
            .child(
                div()
                    .text_xs()
                    .text_color(palette.color(ThemeRole::TextDim))
                    .child("Try another name, identifier, or alias."),
            )
            .into_any_element()
    } else {
        let items = Arc::new(items);
        let list_items = items.clone();
        let list_view = view.clone();
        let list_palette = palette.clone();
        uniform_list(
            ("settings-choice-picker-options", usize::from(field.0)),
            item_count,
            move |range, _, _| {
                range
                    .map(|index| {
                        let choice = list_items[index].clone();
                        let is_selected = index == selected.min(item_count - 1);
                        let is_current = Some(&choice.value) == current.as_ref();
                        let choice_view = list_view.clone();
                        let enabled = choice.enabled;
                        let detail = if choice.detail.is_empty() {
                            choice.value.clone()
                        } else {
                            choice.detail.clone()
                        };
                        div()
                            .id((
                                "settings-choice-picker-option",
                                usize::from(field.0) * 10_000 + index,
                            ))
                            .h(rems_from_px(78.))
                            .w_full()
                            .flex()
                            .items_center()
                            .gap_3()
                            .px_4()
                            .border_b_1()
                            .border_color(list_palette.color(ThemeRole::BorderSubtle))
                            .bg(if is_selected {
                                list_palette.color(ThemeRole::ControlActive)
                            } else {
                                list_palette.color(ThemeRole::Raised)
                            })
                            .hover({
                                let hover = list_palette.color(ThemeRole::ControlSurfaceHover);
                                move |row| row.bg(hover)
                            })
                            .when(enabled, |row| {
                                let choice = choice.clone();
                                row.cursor_pointer().on_click(move |_, window, cx| {
                                    let _ = choice_view.update(cx, |this, cx| {
                                        this.choose_choice(field, choice.clone(), window, cx);
                                    });
                                    cx.stop_propagation();
                                })
                            })
                            .when(!enabled, |row| row.cursor_default())
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .child(
                                        div()
                                            .truncate()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_color(if enabled {
                                                list_palette.color(ThemeRole::TextPrimary)
                                            } else {
                                                list_palette.color(ThemeRole::TextMuted)
                                            })
                                            .child(choice.label.clone()),
                                    )
                                    .child(
                                        div()
                                            .truncate()
                                            .text_xs()
                                            .text_color(list_palette.color(ThemeRole::TextDim))
                                            .child(detail),
                                    ),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .flex()
                                    .flex_col()
                                    .items_end()
                                    .gap_1()
                                    .when(is_current, |status| {
                                        status.child(
                                            div()
                                                .text_xs()
                                                .font_weight(FontWeight::SEMIBOLD)
                                                .text_color(
                                                    list_palette.color(ThemeRole::StateSuccess),
                                                )
                                                .child("Current"),
                                        )
                                    })
                                    .when(!enabled, |status| {
                                        status.child(
                                            div()
                                                .text_xs()
                                                .font_weight(FontWeight::SEMIBOLD)
                                                .text_color(
                                                    list_palette.color(ThemeRole::StateWarning),
                                                )
                                                .child("Unavailable"),
                                        )
                                    }),
                            )
                            .into_any_element()
                    })
                    .collect::<Vec<_>>()
            },
        )
        .track_scroll(&scroll)
        .h(rems_from_px(330.))
        .w_full()
        .into_any_element()
    };

    let dismiss_view = view.clone();
    let close_view = view.clone();
    let refresh_view = view.clone();
    div()
        .id("settings-choice-picker-scrim")
        .absolute()
        .inset_0()
        .flex()
        .items_center()
        .justify_center()
        .bg(rgba(0x00000070))
        .on_click(move |_, window, cx| {
            let _ = dismiss_view.update(cx, |this, cx| {
                this.close_choice_picker(window, cx);
            });
        })
        .child(
            div()
                .id("settings-choice-picker")
                .w(rems_from_px(600.))
                .max_h(rems_from_px(540.))
                .overflow_hidden()
                .border_1()
                .border_color(palette.color(ThemeRole::BorderStrong))
                .bg(palette.color(ThemeRole::Raised))
                .shadow_lg()
                .flex()
                .flex_col()
                .on_click(|_, _, cx| cx.stop_propagation())
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_3()
                        .px_5()
                        .py_4()
                        .border_b_1()
                        .border_color(palette.color(ThemeRole::BorderSubtle))
                        .child(
                            div()
                                .flex_1()
                                .child(div().text_lg().font_weight(FontWeight::BOLD).child(title))
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(palette.color(ThemeRole::TextDim))
                                        .child("Choices refresh automatically when opened."),
                                ),
                        )
                        .child(
                            setting_button("close-choice-picker", "Esc", false, palette).on_click(
                                move |_, window, cx| {
                                    let _ = close_view.update(cx, |this, cx| {
                                        this.close_choice_picker(window, cx);
                                    });
                                    cx.stop_propagation();
                                },
                            ),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_3()
                        .px_5()
                        .py_3()
                        .border_b_1()
                        .border_color(palette.color(ThemeRole::BorderSubtle))
                        .child(
                            div()
                                .flex_1()
                                .min_h(rems_from_px(40.))
                                .px_3()
                                .py_2()
                                .border_1()
                                .border_color(palette.color(ThemeRole::BorderFocus))
                                .bg(palette.color(ThemeRole::Input))
                                .child(search),
                        )
                        .child(
                            setting_button("refresh-choice-picker", "Refresh", false, palette)
                                .on_click(move |_, _, cx| {
                                    let _ = refresh_view.update(cx, |this, cx| {
                                        this.refresh_remote_choices(field, cx);
                                    });
                                    cx.stop_propagation();
                                }),
                        ),
                )
                .child(list)
                .child(
                    div()
                        .flex()
                        .items_center()
                        .px_5()
                        .py_3()
                        .border_t_1()
                        .border_color(palette.color(ThemeRole::BorderSubtle))
                        .text_xs()
                        .text_color(palette.color(ThemeRole::TextDim))
                        .child(format!(
                            "{selectable_count} selectable · ↑/↓ choose · Enter apply · Esc close"
                        )),
                ),
        )
        .into_any_element()
}

fn render_diagnostic_row(
    index: usize,
    diagnostics: &[ConfigDiagnostic],
    draft: &GuiConfig,
    palette: &ThemePalette,
    source: Option<&str>,
    compact: bool,
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
        .h(rems_from_px(if compact { 144.0 } else { 104.0 }))
        .w_full()
        .flex()
        .when(compact, |row| row.flex_wrap())
        .items_center()
        .gap_4()
        .px_5()
        .border_b_1()
        .border_color(palette.color(ThemeRole::BorderSubtle))
        .bg(palette.color(ThemeRole::Raised))
        .child(
            div()
                .when(compact, |column| column.w_full())
                .when(!compact, |column| column.w(rems_from_px(220.)).flex_none())
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
        RowRef::Toggle(setting) => 130 + setting as usize,
        RowRef::Binding(scope, command) => {
            200 + key_bindings::BINDINGS
                .iter()
                .position(|binding| binding.scope == scope && binding.command == command)
                .unwrap_or_default()
        }
        RowRef::Diagnostic(index) => 1000 + index,
    }
}

fn remote_field_id(field: RemoteField) -> usize {
    2_000 + usize::from(field.0)
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
    use std::{cell::RefCell, rc::Rc};

    use super::*;

    fn create_settings(cx: &gpui::TestAppContext) -> Entity<SettingsView> {
        create_settings_with_live_layout(cx, LayoutConfig::default())
    }

    fn create_settings_with_live_layout(
        cx: &gpui::TestAppContext,
        live_layout: LayoutConfig,
    ) -> Entity<SettingsView> {
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
            cx.new(move |cx| {
                SettingsView::new(
                    local_rpc::appearance::AppearanceSessionId(1),
                    live_layout,
                    cx,
                )
            })
        })
    }

    #[gpui::test]
    fn layout_toggles_update_the_draft_and_reset_to_visible(cx: &mut gpui::TestAppContext) {
        let settings = create_settings(cx);

        cx.update(|cx| {
            settings.update(cx, |settings, cx| {
                assert!(settings.draft.layout.status_bar_visible);
                assert!(settings.draft.layout.room_menu_visible);

                settings.toggle(ToggleSetting::StatusBarVisible, cx);
                settings.toggle(ToggleSetting::RoomMenuVisible, cx);
                assert!(!settings.draft.layout.status_bar_visible);
                assert!(!settings.draft.layout.room_menu_visible);
                assert!(settings.local_dirty());

                settings.reset_row(RowRef::Toggle(ToggleSetting::StatusBarVisible), cx);
                settings.reset_row(RowRef::Toggle(ToggleSetting::RoomMenuVisible), cx);
                assert!(settings.draft.layout.status_bar_visible);
                assert!(settings.draft.layout.room_menu_visible);
                assert!(!settings.local_dirty());
            });
        });
    }

    #[gpui::test]
    fn narrow_settings_collapses_sections_into_a_menu(cx: &mut gpui::TestAppContext) {
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
        });
        let (settings, cx) = cx.add_window_view(|window, cx| {
            let settings = SettingsView::new(
                local_rpc::appearance::AppearanceSessionId(1),
                LayoutConfig::default(),
                cx,
            );
            window.focus(&settings.focus, cx);
            settings
        });

        cx.simulate_resize(gpui::size(gpui::px(700.0), gpui::px(600.0)));
        cx.run_until_parked();
        let trigger = settings
            .read_with(cx, |settings, _| settings.section_menu_trigger_bounds.get())
            .expect("narrow settings renders one section-menu trigger");
        assert!(f32::from(trigger.size.height) <= 48.0);
        assert!(!settings.read_with(cx, |settings, _| settings.section_menu_open));

        cx.simulate_click(trigger.center(), gpui::Modifiers::none());
        assert!(settings.read_with(cx, |settings, _| settings.section_menu_open));

        cx.simulate_click(
            gpui::point(gpui::px(650.0), gpui::px(50.0)),
            gpui::Modifiers::none(),
        );
        assert!(!settings.read_with(cx, |settings, _| settings.section_menu_open));

        cx.simulate_click(trigger.center(), gpui::Modifiers::none());
        cx.simulate_keystrokes("escape");
        assert!(!settings.read_with(cx, |settings, _| settings.section_menu_open));
    }

    #[gpui::test]
    fn native_fullscreen_toggle_updates_the_draft_and_resets_to_disabled(
        cx: &mut gpui::TestAppContext,
    ) {
        let settings = create_settings(cx);

        cx.update(|cx| {
            settings.update(cx, |settings, cx| {
                assert!(!settings.draft.native_fullscreen);

                settings.toggle(ToggleSetting::NativeFullscreen, cx);
                assert!(settings.draft.native_fullscreen);
                assert!(settings.local_dirty());

                settings.reset_row(RowRef::Toggle(ToggleSetting::NativeFullscreen), cx);
                assert!(!settings.draft.native_fullscreen);
                assert!(!settings.local_dirty());
            });
        });
    }

    #[gpui::test]
    fn video_loop_default_toggle_updates_the_draft_and_resets_to_disabled(
        cx: &mut gpui::TestAppContext,
    ) {
        let settings = create_settings(cx);

        cx.update(|cx| {
            settings.update(cx, |settings, cx| {
                assert!(!settings.draft.video_loop_by_default);

                settings.toggle(ToggleSetting::VideoLoopByDefault, cx);
                assert!(settings.draft.video_loop_by_default);
                assert!(settings.local_dirty());

                settings.reset_row(RowRef::Toggle(ToggleSetting::VideoLoopByDefault), cx);
                assert!(!settings.draft.video_loop_by_default);
                assert!(!settings.local_dirty());
            });
        });
    }

    #[gpui::test]
    fn layout_changes_emit_live_deltas_and_cancel_restores_the_open_state(
        cx: &mut gpui::TestAppContext,
    ) {
        let settings = create_settings_with_live_layout(
            cx,
            LayoutConfig {
                status_bar_visible: false,
                room_menu_visible: false,
            },
        );
        let previews = Rc::new(RefCell::new(Vec::new()));
        let _subscription = cx.update({
            let settings = settings.clone();
            let previews = previews.clone();
            move |cx| {
                cx.subscribe(&settings, move |_, event: &SettingsViewEvent, _| {
                    if let SettingsViewEvent::LocalLayoutPreview {
                        status_bar_visible,
                        room_menu_visible,
                    } = event
                    {
                        previews
                            .borrow_mut()
                            .push((*status_bar_visible, *room_menu_visible));
                    }
                })
            }
        });

        settings.update(cx, |settings, cx| {
            settings.toggle(ToggleSetting::StatusBarVisible, cx);
            settings.toggle(ToggleSetting::RoomMenuVisible, cx);
            settings.cancel(cx);
        });

        assert_eq!(
            previews.borrow().as_slice(),
            &[
                (Some(false), None),
                (None, Some(false)),
                (Some(false), Some(false)),
            ]
        );
    }

    #[gpui::test]
    fn save_commits_a_live_layout_preview_even_when_the_draft_matches_disk(
        cx: &mut gpui::TestAppContext,
    ) {
        let settings = create_settings_with_live_layout(
            cx,
            LayoutConfig {
                status_bar_visible: false,
                room_menu_visible: true,
            },
        );

        settings.update(cx, |settings, cx| {
            settings.toggle(ToggleSetting::StatusBarVisible, cx);
            settings.toggle(ToggleSetting::StatusBarVisible, cx);
            assert!(!settings.local_dirty());

            settings.save(false, cx);
            assert!(settings.layout_preview_baseline.status_bar_visible);
            assert!(!settings.layout_previewed.status_bar_visible);
        });
    }

    const OUTPUT_VOLUME: RemoteField = wire_settings::SettingsFieldId(1);
    const MICROPHONE: RemoteField = wire_settings::SettingsFieldId(2);

    fn remote_document(value: f32, revision: u64) -> wire_settings::SettingsDocument {
        wire_settings::SettingsDocument {
            session_id: wire_settings::SettingsSessionId(9),
            revision,
            source: wire_settings::SettingsSourceStatus::File,
            sections: vec![wire_settings::SettingsSection {
                id: "audio".into(),
                title: "Audio".into(),
                help: String::new(),
                fields: vec![wire_settings::SettingsField {
                    id: OUTPUT_VOLUME,
                    key: "audio.output-volume".into(),
                    label: "Output volume".into(),
                    help: String::new(),
                    flags: wire_settings::FIELD_AUDIO,
                    value: wire_settings::SettingsValue::Float(value),
                    default: wire_settings::SettingsValue::Float(100.0),
                    control: wire_settings::SettingsControl {
                        kind: wire_settings::CONTROL_NUMBER,
                        choices: Vec::new(),
                        min: Some(0.0),
                        max: Some(130.0),
                        step: Some(1.0),
                        unit: "%".into(),
                        placeholder: String::new(),
                    },
                }],
            }],
            actions: wire_settings::SettingsActions {
                audio_preview: true,
                audio_loopback: true,
            },
            audio_runtime: wire_settings::AudioRuntimeState {
                preview_active: false,
                loopback: false,
                applying: false,
                preview_seq: 0,
                diagnostics: Vec::new(),
            },
            diagnostics: Vec::new(),
        }
    }

    fn searchable_choice_document() -> wire_settings::SettingsDocument {
        let mut document = remote_document(100.0, 4);
        document.sections[0].fields = vec![wire_settings::SettingsField {
            id: MICROPHONE,
            key: "audio.input-device-id".into(),
            label: "Microphone".into(),
            help: "Select an input device.".into(),
            flags: wire_settings::FIELD_AUDIO,
            value: wire_settings::SettingsValue::Text("alsa:usb-studio".into()),
            default: wire_settings::SettingsValue::Text(String::new()),
            control: wire_settings::SettingsControl {
                kind: wire_settings::CONTROL_SEARCHABLE_CHOICE,
                choices: vec![
                    wire_settings::SettingsChoice {
                        value: String::new(),
                        label: "System default".into(),
                        detail: "OS default input".into(),
                        search: "system default input".into(),
                        enabled: true,
                    },
                    wire_settings::SettingsChoice {
                        value: "alsa:usb-studio".into(),
                        label: "Studio microphone".into(),
                        detail: "48 kHz · stereo · USB Audio".into(),
                        search: "hw:Studio legacy-usb".into(),
                        enabled: true,
                    },
                    wire_settings::SettingsChoice {
                        value: "alsa:offline".into(),
                        label: "Disconnected microphone".into(),
                        detail: "Device is unavailable".into(),
                        search: "offline".into(),
                        enabled: false,
                    },
                ],
                min: None,
                max: None,
                step: None,
                unit: String::new(),
                placeholder: "Search microphones".into(),
            },
        }];
        document
    }

    #[gpui::test]
    fn searchable_choice_opens_with_metadata_search_and_refreshes_automatically(
        cx: &mut gpui::TestAppContext,
    ) {
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
        });
        let (settings, cx) = cx.add_window_view(|window, cx| {
            let mut settings = SettingsView::new(
                local_rpc::appearance::AppearanceSessionId(1),
                LayoutConfig::default(),
                cx,
            );
            settings.install_remote_document(searchable_choice_document(), false);
            window.focus(&settings.focus, cx);
            settings
        });
        let commands = Rc::new(RefCell::new(Vec::new()));
        let _subscription = cx.update({
            let settings = settings.clone();
            let commands = commands.clone();
            move |_, cx| {
                cx.subscribe(&settings, move |_, event: &SettingsViewEvent, _| {
                    if let SettingsViewEvent::Command(command) = event {
                        commands.borrow_mut().push(command.clone());
                    }
                })
            }
        });

        settings.update_in(cx, |settings, window, cx| {
            settings.open_choice_picker(MICROPHONE, window, cx);
        });

        settings.read_with(cx, |settings, _| {
            let picker = settings
                .choice_picker
                .as_ref()
                .expect("searchable choice opens a picker");
            assert_eq!(picker.selected, 1);
            assert_eq!(
                settings
                    .choice_picker_items(MICROPHONE, "48 khz")
                    .next()
                    .unwrap()
                    .label,
                "Studio microphone"
            );
            assert_eq!(
                settings
                    .choice_picker_items(MICROPHONE, "legacy-usb")
                    .next()
                    .unwrap()
                    .label,
                "Studio microphone"
            );
            assert!(
                !settings
                    .choice_picker_items(MICROPHONE, "offline")
                    .next()
                    .unwrap()
                    .enabled
            );
        });
        assert!(matches!(
            commands.borrow().as_slice(),
            [wire_settings::SettingsCommand::RefreshChoices {
                field: MICROPHONE,
                changes,
                ..
            }] if changes.is_empty()
        ));
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
            let settings = SettingsView::new(
                local_rpc::appearance::AppearanceSessionId(1),
                LayoutConfig::default(),
                cx,
            );
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
    fn appearance_preview_broadcasts_every_valid_draft_immediately(cx: &mut gpui::TestAppContext) {
        let settings = create_settings(cx);
        let commands = Rc::new(RefCell::new(Vec::new()));
        let _subscription = cx.update({
            let settings = settings.clone();
            let commands = commands.clone();
            move |cx| {
                cx.subscribe(&settings, move |_, event: &SettingsViewEvent, _| {
                    if let SettingsViewEvent::AppearanceCommand(command) = event {
                        commands.borrow_mut().push(command.clone());
                    }
                })
            }
        });

        settings.update(cx, |settings, cx| {
            settings.apply_editor_text("#010203", cx);
            settings.apply_editor_text("#040506", cx);
        });
        cx.run_until_parked();
        let commands = commands.borrow();
        assert_eq!(commands.len(), 2);
        let local_rpc::appearance::AppearanceCommand::Preview {
            mutation_seq,
            document,
            ..
        } = &commands[1]
        else {
            panic!("appearance edit emits a preview");
        };
        assert_eq!(*mutation_seq, 2);
        let appearance = AppearanceConfig::from_document(document).unwrap();
        let mut config = GuiConfig::default();
        appearance.merge_into(&mut config);
        assert_eq!(config.theme.color(ThemeRole::Window), Rgba8::rgb(4, 5, 6));
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

    #[gpui::test]
    fn daemon_save_commits_sent_snapshot_and_keeps_newer_edits_dirty(
        cx: &mut gpui::TestAppContext,
    ) {
        let settings = create_settings(cx);
        cx.update(|cx| {
            settings.update(cx, |settings, cx| {
                settings.install_remote_document(remote_document(80.0, 4), false);
                let sent = settings.remote_draft.clone().unwrap();
                settings.remote_pending_save = Some(sent.clone());
                settings.remote_saving = true;
                settings
                    .remote_draft
                    .as_mut()
                    .unwrap()
                    .set(OUTPUT_VOLUME, wire_settings::SettingsValue::Float(60.0));

                settings.apply_remote_result(
                    wire_settings::SettingsResult::accepted(
                        local_rpc::model::RequestId(3),
                        local_rpc::frame::Operation::SaveSettings,
                        wire_settings::SettingsResultPayload::Document(remote_document(80.0, 5)),
                    ),
                    cx,
                );

                assert_eq!(
                    settings
                        .remote_baseline
                        .as_ref()
                        .unwrap()
                        .get(OUTPUT_VOLUME),
                    Some(&wire_settings::SettingsValue::Float(80.0))
                );
                assert_eq!(
                    settings.remote_draft.as_ref().unwrap().get(OUTPUT_VOLUME),
                    Some(&wire_settings::SettingsValue::Float(60.0))
                );
                assert!(settings.remote_dirty());
                assert!(!settings.remote_saving);
            });
        });
    }

    #[gpui::test]
    fn audio_events_from_an_old_settings_session_are_ignored(cx: &mut gpui::TestAppContext) {
        let settings = create_settings(cx);
        cx.update(|cx| {
            settings.update(cx, |settings, cx| {
                settings.install_remote_document(remote_document(100.0, 4), false);
                settings.apply_remote_event(
                    wire_settings::SettingsEvent::AudioMeter {
                        session_id: wire_settings::SettingsSessionId(8),
                        rms: 1.0,
                        peak: 1.0,
                        voice_active: true,
                    },
                    cx,
                );
                assert_eq!(settings.remote_meter_rms, 0.0);
                assert_eq!(settings.remote_meter_peak, 0.0);
            });
        });
    }

    #[gpui::test]
    fn stale_documents_and_old_session_results_do_not_replace_the_active_session(
        cx: &mut gpui::TestAppContext,
    ) {
        let settings = create_settings(cx);
        cx.update(|cx| {
            settings.update(cx, |settings, cx| {
                settings.install_remote_document(remote_document(80.0, 5), false);
                settings.apply_remote_result(
                    wire_settings::SettingsResult::accepted(
                        local_rpc::model::RequestId(7),
                        local_rpc::frame::Operation::RefreshSettingsChoices,
                        wire_settings::SettingsResultPayload::Document(remote_document(100.0, 4)),
                    ),
                    cx,
                );
                settings.apply_remote_result(
                    wire_settings::SettingsResult::accepted(
                        local_rpc::model::RequestId(8),
                        local_rpc::frame::Operation::CloseSettings,
                        wire_settings::SettingsResultPayload::Closed {
                            session_id: wire_settings::SettingsSessionId(8),
                        },
                    ),
                    cx,
                );

                assert_eq!(
                    settings.remote_session,
                    Some(wire_settings::SettingsSessionId(9))
                );
                assert_eq!(settings.remote_revision, 5);
                assert_eq!(
                    settings
                        .remote_baseline
                        .as_ref()
                        .unwrap()
                        .get(OUTPUT_VOLUME),
                    Some(&wire_settings::SettingsValue::Float(80.0))
                );
            });
        });
    }

    #[gpui::test]
    fn color_picker_cancel_restores_and_apply_keeps_the_draft(cx: &mut gpui::TestAppContext) {
        let _ = create_settings(cx);
        let (settings, cx) = cx.add_window_view(|window, cx| {
            let settings = SettingsView::new(
                local_rpc::appearance::AppearanceSessionId(2),
                LayoutConfig::default(),
                cx,
            );
            window.focus(&settings.focus, cx);
            settings
        });
        let role = ThemeRole::Window;

        settings.update_in(cx, |settings, window, cx| {
            let original = settings.draft.theme.color(role);
            assert_eq!(
                settings.color_picker.as_ref().map(|picker| picker.role),
                Some(role)
            );
            settings.focus_target(SettingsFocus::Search, false, window, cx);
            assert!(settings.color_picker.is_none());
            settings.open_color_picker(role, window, cx);
            assert_eq!(settings.focused, SettingsFocus::Row(RowRef::Theme(role)));
            let selected_index = settings
                .visible_rows()
                .iter()
                .position(|row| *row == RowRef::Theme(role))
                .unwrap();
            let deferred_scroll = settings.scroll.0.borrow().deferred_scroll_to_item.unwrap();
            assert_eq!(deferred_scroll.item_index, selected_index);
            assert_eq!(deferred_scroll.strategy, gpui::ScrollStrategy::Nearest);

            let changed = Rgba8::rgba(0x12, 0x34, 0x56, 0x80);
            settings.draft.theme.set_color(role, changed);
            settings.color_picker.as_mut().unwrap().hsva = Hsva::from_rgba8(changed);
            settings.cancel_color_picker(window, cx);
            assert_eq!(settings.draft.theme.color(role), original);
            assert!(settings.color_picker.is_some());

            settings.apply_editor_text("#abcdef80", cx);
            let picker = settings.color_picker.as_ref().unwrap();
            assert_eq!(picker.hsva.to_rgba8(), Rgba8::rgba(0xab, 0xcd, 0xef, 0x80));
            settings.apply_color_picker(window, cx);
            assert_eq!(
                settings.draft.theme.color(role),
                Rgba8::rgba(0xab, 0xcd, 0xef, 0x80)
            );
            assert_eq!(
                settings.color_picker.as_ref().unwrap().original,
                Rgba8::rgba(0xab, 0xcd, 0xef, 0x80)
            );

            settings.apply_editor_text("not-a-color", cx);
            settings.apply_color_picker(window, cx);
            assert!(settings.color_picker.is_some());
            assert!(settings.invalid_edit(RowRef::Theme(role)).is_some());
            settings.cancel_color_picker(window, cx);
            settings.focus_target(SettingsFocus::Search, false, window, cx);
            assert!(settings.color_picker.is_none());
        });
    }
}
