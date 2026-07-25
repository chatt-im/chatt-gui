use gpui::{App, Global};
use local_rpc::appearance::{
    APPEARANCE_FORMAT_TOML_V1, AppearanceDocument, AppearanceEvent, AppearanceSessionId,
};
use toml_spanner::Toml;

use crate::{
    config::{
        io::LoadedConfig,
        schema::{FontConfig, GUI_SCHEMA_VERSION, GuiConfig, ThemeConfig},
        validation::{has_errors, validate},
    },
    theme,
};

#[derive(Clone, Debug, PartialEq, Toml)]
#[toml(FromToml, ToToml, recoverable, rename_all = "kebab-case")]
pub(crate) struct AppearanceConfig {
    #[toml(default = GUI_SCHEMA_VERSION)]
    schema_version: u16,
    #[toml(default, style = Header)]
    theme: ThemeConfig,
    #[toml(default, style = Header)]
    fonts: FontConfig,
}

impl AppearanceConfig {
    pub(crate) fn from_gui(config: &GuiConfig) -> Self {
        Self {
            schema_version: GUI_SCHEMA_VERSION,
            theme: config.theme.clone(),
            fonts: config.fonts.clone(),
        }
    }

    pub(crate) fn merge_into(&self, config: &mut GuiConfig) {
        config.theme = self.theme.clone();
        config.fonts = self.fonts.clone();
    }

    pub(crate) fn document(&self) -> Result<AppearanceDocument, String> {
        let toml = toml_spanner::to_string(self)
            .map_err(|error| error.to_string())?
            .into_bytes();
        let document = AppearanceDocument {
            format_version: APPEARANCE_FORMAT_TOML_V1,
            toml,
        };
        document.validate()?;
        Ok(document)
    }

    pub(crate) fn from_document(document: &AppearanceDocument) -> Result<Self, String> {
        document.validate()?;
        let source = std::str::from_utf8(&document.toml)
            .map_err(|_| "appearance document is not valid UTF-8".to_string())?;
        let appearance: Self = toml_spanner::from_str(source).map_err(|error| error.to_string())?;
        if appearance.schema_version != GUI_SCHEMA_VERSION {
            return Err(format!(
                "unsupported GUI appearance schema version {}",
                appearance.schema_version
            ));
        }
        let mut candidate = GuiConfig::default();
        appearance.merge_into(&mut candidate);
        let diagnostics = validate(&candidate);
        if has_errors(&diagnostics) {
            return Err(diagnostics
                .into_iter()
                .map(|diagnostic| diagnostic.message)
                .collect::<Vec<_>>()
                .join("; "));
        }
        Ok(appearance)
    }
}

#[derive(Clone, Default)]
pub(crate) struct SharedCommittedAppearance(pub(crate) Option<AppearanceConfig>);

impl Global for SharedCommittedAppearance {}

pub(crate) fn install(cx: &mut App) {
    cx.set_global(SharedCommittedAppearance::default());
}

pub(crate) struct AppearanceSync {
    generation: u64,
    active: Option<(AppearanceSessionId, AppearanceConfig)>,
    committed: Option<AppearanceConfig>,
    local: Option<(AppearanceSessionId, AppearanceConfig)>,
}

impl AppearanceSync {
    pub(crate) fn new() -> Self {
        Self {
            generation: 0,
            active: None,
            committed: None,
            local: None,
        }
    }

    pub(crate) fn local_preview(
        &mut self,
        session_id: AppearanceSessionId,
        appearance: AppearanceConfig,
        loaded: &LoadedConfig,
        cx: &mut App,
    ) {
        self.local = Some((session_id, appearance.clone()));
        self.active = Some((session_id, appearance.clone()));
        apply(&appearance, loaded, cx);
    }

    pub(crate) fn daemon_reconnected(&mut self) {
        self.generation = 0;
    }

    pub(crate) fn local_commit(
        &mut self,
        session_id: AppearanceSessionId,
        appearance: AppearanceConfig,
        loaded: &LoadedConfig,
        cx: &mut App,
    ) {
        self.committed = Some(appearance.clone());
        self.local = None;
        if self
            .active
            .as_ref()
            .is_some_and(|(active, _)| *active == session_id)
        {
            self.active = None;
        }
        cx.set_global(SharedCommittedAppearance(Some(appearance.clone())));
        apply(&appearance, loaded, cx);
    }

    pub(crate) fn end_local(
        &mut self,
        session_id: AppearanceSessionId,
        loaded: &LoadedConfig,
        cx: &mut App,
    ) {
        if self
            .local
            .as_ref()
            .is_some_and(|(local, _)| *local == session_id)
        {
            self.local = None;
        }
        if self
            .active
            .as_ref()
            .is_some_and(|(active, _)| *active == session_id)
        {
            self.active = None;
            self.apply_fallback(loaded, cx);
        }
    }

    pub(crate) fn disconnected(&mut self, loaded: &LoadedConfig, cx: &mut App) {
        if let Some((session_id, appearance)) = self.local.clone() {
            self.active = Some((session_id, appearance.clone()));
            apply(&appearance, loaded, cx);
        } else if self.active.take().is_some() {
            self.apply_fallback(loaded, cx);
        }
    }

    pub(crate) fn apply_event(
        &mut self,
        event: AppearanceEvent,
        loaded: &LoadedConfig,
        cx: &mut App,
    ) -> Result<(), String> {
        let generation = event.generation();
        if generation < self.generation {
            return Ok(());
        }
        self.generation = generation;
        match event {
            AppearanceEvent::Preview {
                session_id,
                document,
                ..
            } => {
                let appearance = AppearanceConfig::from_document(&document)?;
                self.active = Some((session_id, appearance.clone()));
                apply(&appearance, loaded, cx);
            }
            AppearanceEvent::Committed { document, .. } => {
                let appearance = AppearanceConfig::from_document(&document)?;
                self.committed = Some(appearance.clone());
                self.active = None;
                cx.set_global(SharedCommittedAppearance(Some(appearance.clone())));
                apply(&appearance, loaded, cx);
            }
            AppearanceEvent::Cleared { .. } => {
                self.committed = None;
                cx.set_global(SharedCommittedAppearance(None));
                if let Some((session_id, appearance)) = self.local.clone() {
                    self.active = Some((session_id, appearance.clone()));
                    apply(&appearance, loaded, cx);
                } else {
                    self.active = None;
                    apply_loaded(loaded, cx);
                }
            }
        }
        Ok(())
    }

    fn apply_fallback(&self, loaded: &LoadedConfig, cx: &mut App) {
        if let Some(committed) = &self.committed {
            apply(committed, loaded, cx);
        } else {
            apply_loaded(loaded, cx);
        }
    }
}

fn apply(appearance: &AppearanceConfig, loaded: &LoadedConfig, cx: &mut App) {
    let mut config = loaded.config.clone();
    appearance.merge_into(&mut config);
    let available_families = cx.text_system().all_font_names();
    let mut diagnostics = validate(&config);
    diagnostics.extend(theme::font_warnings(&config, &available_families));
    theme::apply_appearance(
        &config,
        loaded.status,
        &diagnostics,
        &available_families,
        cx,
    );
}

fn apply_loaded(loaded: &LoadedConfig, cx: &mut App) {
    let available_families = cx.text_system().all_font_names();
    theme::apply_appearance(
        &loaded.config,
        loaded.status,
        &loaded.diagnostics,
        &available_families,
        cx,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::{DEFAULT_CODE_FAMILY, Rgba8};
    use crate::theme::ThemeRole;

    #[test]
    fn document_round_trip_only_carries_appearance() {
        let mut config = GuiConfig::default();
        config
            .theme
            .set_color(ThemeRole::Window, Rgba8::rgb(1, 2, 3));
        config.fonts.code_family = "Preview Mono".into();
        let appearance = AppearanceConfig::from_gui(&config);
        let decoded = AppearanceConfig::from_document(&appearance.document().unwrap()).unwrap();

        let mut target = GuiConfig::default();
        target.fonts.code_family = DEFAULT_CODE_FAMILY.into();
        decoded.merge_into(&mut target);
        assert_eq!(target.theme, config.theme);
        assert_eq!(target.fonts, config.fonts);
        assert_eq!(
            target.input.default_binding_mode,
            GuiConfig::default().input.default_binding_mode
        );
    }

    #[test]
    fn rejects_wrong_document_version() {
        let mut document = AppearanceConfig::from_gui(&GuiConfig::default())
            .document()
            .unwrap();
        document.format_version += 1;
        assert!(AppearanceConfig::from_document(&document).is_err());
    }
}
