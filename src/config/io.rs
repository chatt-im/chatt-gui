use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

use tempfile::NamedTempFile;
use toml_spanner::{Arena, ErrorKind, Formatting, Table, ToToml};

use super::{
    paths,
    schema::GuiConfig,
    validation::{ConfigDiagnostic, has_errors, validate},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SourceStatus {
    Loaded,
    Missing,
    Invalid,
    ReadFailed,
    PathUnavailable,
}

#[derive(Clone, Debug)]
pub(crate) struct LoadedConfig {
    pub(crate) path: Option<PathBuf>,
    pub(crate) config: GuiConfig,
    pub(crate) source: Option<Vec<u8>>,
    pub(crate) status: SourceStatus,
    pub(crate) diagnostics: Vec<ConfigDiagnostic>,
}

pub(crate) fn load() -> LoadedConfig {
    let Some(path) = paths::config_path() else {
        return LoadedConfig {
            path: None,
            config: GuiConfig::default(),
            source: None,
            status: SourceStatus::PathUnavailable,
            diagnostics: vec![ConfigDiagnostic::warning(
                "gui.toml",
                "no platform configuration directory is available; changes are session-only",
            )],
        };
    };
    load_path(path)
}

pub(crate) fn load_path(path: PathBuf) -> LoadedConfig {
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return LoadedConfig {
                path: Some(path),
                config: GuiConfig::default(),
                source: None,
                status: SourceStatus::Missing,
                diagnostics: Vec::new(),
            };
        }
        Err(error) => {
            return LoadedConfig {
                path: Some(path),
                config: GuiConfig::default(),
                source: None,
                status: SourceStatus::ReadFailed,
                diagnostics: vec![ConfigDiagnostic::error(
                    "gui.toml",
                    format!("could not read configuration: {error}"),
                )],
            };
        }
    };

    let source = match std::str::from_utf8(&bytes) {
        Ok(source) => source,
        Err(error) => {
            return LoadedConfig {
                path: Some(path),
                config: GuiConfig::default(),
                source: Some(bytes),
                status: SourceStatus::Invalid,
                diagnostics: vec![ConfigDiagnostic::error(
                    "gui.toml",
                    format!("configuration is not valid UTF-8: {error}"),
                )],
            };
        }
    };
    let arena = Arena::new();
    let mut document = toml_spanner::parse_recoverable(source, &arena);
    let (config, conversion_errors) = match document.to_allowing_errors::<GuiConfig>() {
        Ok(result) => result,
        Err(error) => {
            let diagnostics = toml_diagnostics(source, error.errors);
            return LoadedConfig {
                path: Some(path),
                config: GuiConfig::default(),
                source: Some(bytes),
                status: SourceStatus::Invalid,
                diagnostics,
            };
        }
    };
    let mut diagnostics = toml_diagnostics(source, conversion_errors.errors);
    diagnostics.extend(validate(&config));
    if has_errors(&diagnostics) {
        LoadedConfig {
            path: Some(path),
            config: GuiConfig::default(),
            source: Some(bytes),
            status: SourceStatus::Invalid,
            diagnostics,
        }
    } else {
        LoadedConfig {
            path: Some(path),
            config,
            source: Some(bytes),
            status: SourceStatus::Loaded,
            diagnostics,
        }
    }
}

fn toml_diagnostics(source: &str, errors: Vec<toml_spanner::Error>) -> Vec<ConfigDiagnostic> {
    errors
        .into_iter()
        .map(|error| {
            let path = error
                .path()
                .map(ToString::to_string)
                .unwrap_or_else(|| "gui.toml".into());
            let span = error.span();
            let diagnostic = if matches!(error.kind(), ErrorKind::UnexpectedKey { .. }) {
                ConfigDiagnostic::warning(path, error.message(source))
            } else {
                ConfigDiagnostic::error(path, error.message(source))
            };
            if span.is_empty() {
                diagnostic
            } else {
                diagnostic.with_source_range(span.range())
            }
        })
        .collect()
}

#[derive(Debug)]
pub(crate) enum SaveError {
    Invalid(Vec<ConfigDiagnostic>),
    Conflict,
    Io(io::Error),
    Serialize(String),
}

impl std::fmt::Display for SaveError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(_) => write!(formatter, "the draft contains invalid values"),
            Self::Conflict => write!(formatter, "gui.toml changed on disk; reload or replace it"),
            Self::Io(error) => write!(formatter, "{error}"),
            Self::Serialize(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for SaveError {}

impl From<io::Error> for SaveError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

pub(crate) fn save(
    path: &Path,
    baseline: Option<&[u8]>,
    config: &GuiConfig,
    replace: bool,
) -> Result<Vec<u8>, SaveError> {
    let diagnostics = validate(config);
    if has_errors(&diagnostics) {
        return Err(SaveError::Invalid(diagnostics));
    }

    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let _lock = FileLock::acquire(path)?;

    let current = match fs::read(path) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    if !replace && current.as_deref() != baseline {
        return Err(SaveError::Conflict);
    }

    let bytes = if !replace {
        match current
            .as_deref()
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
        {
            Some(source) => serialize_preserving_unknown(source, config)?,
            None => toml_spanner::to_string(config)
                .map_err(|error| SaveError::Serialize(error.to_string()))?
                .into_bytes(),
        }
    } else {
        toml_spanner::to_string(config)
            .map_err(|error| SaveError::Serialize(error.to_string()))?
            .into_bytes()
    };
    let mut temporary = NamedTempFile::new_in(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    temporary.write_all(&bytes)?;
    temporary.flush()?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(path)
        .map_err(|error| SaveError::Io(error.error))?;
    #[cfg(unix)]
    fs::File::open(parent)?.sync_all()?;

    Ok(bytes)
}

fn serialize_preserving_unknown(source: &str, config: &GuiConfig) -> Result<Vec<u8>, SaveError> {
    let arena = Arena::new();
    let formatting_document = toml_spanner::parse(source, &arena)
        .map_err(|error| SaveError::Serialize(error.to_string()))?;
    let unknown_document = toml_spanner::parse(source, &arena)
        .map_err(|error| SaveError::Serialize(error.to_string()))?;
    let mut known = config
        .to_toml(&arena)
        .map_err(|error| SaveError::Serialize(error.to_string()))?
        .into_table()
        .ok_or_else(|| {
            SaveError::Serialize("GUI configuration must serialize as a table".into())
        })?;
    merge_unknown(&mut known, unknown_document.into_table(), &arena);
    Ok(Formatting::preserved_from(&formatting_document).format_table_to_bytes(known, &arena))
}

fn merge_unknown<'a>(known: &mut Table<'a>, mut source: Table<'a>, arena: &'a Arena) {
    while let Some(name) = source.entries().first().map(|(key, _)| key.name) {
        let (key, value) = source
            .remove_entry(name)
            .expect("entry name came from the source table");
        let Some(known_value) = known.get_mut(name) else {
            known.insert_unique(key, value, arena);
            continue;
        };
        if known_value.as_table().is_some() && value.as_table().is_some() {
            let source_table = value
                .into_table()
                .expect("table kind was checked before conversion");
            merge_unknown(
                known_value
                    .as_table_mut()
                    .expect("table kind was checked before conversion"),
                source_table,
                arena,
            );
        }
    }
}

struct FileLock {
    file: fs::File,
}

impl FileLock {
    fn acquire(path: &Path) -> io::Result<Self> {
        let mut lock_name = path.as_os_str().to_os_string();
        lock_name.push(".lock");
        let lock_path = PathBuf::from(lock_name);
        let file = {
            let mut options = fs::OpenOptions::new();
            options.create(true).read(true).write(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            options.open(lock_path)?
        };
        file.lock()?;
        Ok(Self { file })
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_loads_defaults_without_creating_it() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("nested/gui.toml");
        let loaded = load_path(path.clone());
        assert_eq!(loaded.status, SourceStatus::Missing);
        assert_eq!(loaded.config, GuiConfig::default());
        assert!(!path.exists());
    }

    #[test]
    fn save_detects_exact_byte_conflicts() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("gui.toml");
        fs::write(&path, b"# changed\n").unwrap();
        let error = save(&path, Some(b"# original\n"), &GuiConfig::default(), false).unwrap_err();
        assert!(matches!(error, SaveError::Conflict));
        assert_eq!(fs::read(&path).unwrap(), b"# changed\n");
    }

    #[test]
    fn save_creates_parent_and_round_trips() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("nested/gui.toml");
        let bytes = save(&path, None, &GuiConfig::default(), false).unwrap();
        let reparsed: GuiConfig =
            toml_spanner::from_str(std::str::from_utf8(&bytes).unwrap()).unwrap();
        assert_eq!(reparsed, GuiConfig::default());
    }

    #[test]
    fn save_preserves_unknown_root_and_nested_entries_and_comments() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("gui.toml");
        let source = br##"# keep this comment
schema-version = 1
future-root = "kept"

[theme.surfaces]
window = '#111317'
future-surface = "#abcdef"

[bindings.future-device]
"secondary-k" = "FutureCommand"
"##;
        fs::write(&path, source).unwrap();
        let mut config = GuiConfig::default();
        config.fonts.code_size = 15.5;

        let bytes = save(&path, Some(source), &config, false).unwrap();
        let rendered = std::str::from_utf8(&bytes).unwrap();
        assert!(rendered.contains("# keep this comment"));
        assert!(rendered.contains("future-root = \"kept\""));
        assert!(rendered.contains("future-surface = \"#abcdef\""));
        assert!(rendered.contains("[bindings.future-device]"));
        assert!(rendered.contains("\"secondary-k\" = \"FutureCommand\""));
        assert!(rendered.contains("window = '#111317'"));
        assert!(rendered.contains("code-size = 15.5"));
    }

    #[test]
    fn unknown_keys_warn_and_do_not_invalidate_the_candidate() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("gui.toml");
        fs::write(
            &path,
            b"schema-version = 1\nfuture-root = true\n[fonts]\ncode-size = 15.5\n",
        )
        .unwrap();

        let loaded = load_path(path);
        assert_eq!(loaded.status, SourceStatus::Loaded);
        assert_eq!(loaded.config.fonts.code_size, 15.5);
        assert_eq!(loaded.diagnostics.len(), 1);
        assert_eq!(
            loaded.diagnostics[0].severity,
            super::super::validation::DiagnosticSeverity::Warning
        );
        assert_eq!(loaded.diagnostics[0].path, "future-root");
        assert!(loaded.diagnostics[0].source_range.is_some());
    }
}
