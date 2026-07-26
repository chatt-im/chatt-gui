use std::{env, sync::OnceLock};

use kvlog::{
    Encode, LogLevel,
    encoding::{Encoder, StaticKey},
};
use log::{Level, LevelFilter, Log, Metadata, Record};

const RENDER_GROUP: &str = "render";
const NATIVE_MPV_GROUP: &str = "native-mpv";
const KVLOG_FILE_VARIABLE: &str = "CHATT_GUI_KVLOG_FILE";

#[derive(Clone, Copy)]
#[cfg_attr(not(any(feature = "diagnostic-logs", test)), allow(dead_code))]
struct FacadeConfig {
    default_level: LevelFilter,
    native_mpv_level: LevelFilter,
    media: bool,
    render: bool,
    rpc: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FacadeDecision {
    level: LogLevel,
    group: Option<&'static str>,
}

struct FacadeLogger {
    config: FacadeConfig,
}

static LOGGER: OnceLock<FacadeLogger> = OnceLock::new();

impl FacadeLogger {
    fn classify(&self, metadata: &Metadata<'_>) -> Option<FacadeDecision> {
        classify(metadata.target(), metadata.level(), self.config)
    }
}

impl Log for FacadeLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        self.classify(metadata).is_some()
    }

    fn log(&self, record: &Record<'_>) {
        let Some(decision) = self.classify(record.metadata()) else {
            return;
        };
        write_record(record, decision);
    }

    fn flush(&self) {}
}

pub fn init() -> kvlog::collector::LoggerGuard {
    let (config, invalid_values) = read_config();
    let logger_guard = match env::var(KVLOG_FILE_VARIABLE) {
        Ok(path) if !path.is_empty() => kvlog::collector::init_file_logger(&path),
        _ => kvlog::collector::init_stdout_logger(),
    };
    let logger = LOGGER
        .set(FacadeLogger { config })
        .map(|()| LOGGER.get().unwrap())
        .unwrap_or_else(|_| panic!("Chatt GUI logger initialized more than once"));
    log::set_logger(logger).expect("failed to install Chatt GUI log facade adapter");
    log::set_max_level(LevelFilter::Info);

    for (variable, value) in invalid_values {
        kvlog::warn!(
            "invalid logging environment value; using disabled fallback",
            variable,
            value = %value
        );
    }

    logger_guard
}

#[cfg(feature = "diagnostic-logs")]
pub fn media_logging_enabled() -> bool {
    LOGGER.get().is_some_and(|logger| logger.config.media)
}

#[cfg(feature = "diagnostic-logs")]
pub fn render_logging_enabled() -> bool {
    LOGGER.get().is_some_and(|logger| logger.config.render)
}

#[cfg(feature = "diagnostic-logs")]
pub fn rpc_logging_enabled() -> bool {
    LOGGER.get().is_some_and(|logger| logger.config.rpc)
}

pub fn native_mpv_log_level() -> &'static str {
    let Some(logger) = LOGGER.get() else {
        return "warn";
    };
    match logger.config.native_mpv_level {
        LevelFilter::Off => "off",
        LevelFilter::Error => "error",
        LevelFilter::Warn => "warn",
        LevelFilter::Info => "info",
        LevelFilter::Debug | LevelFilter::Trace => {
            unreachable!("native mpv logging is capped at Info")
        }
    }
}

fn read_config() -> (FacadeConfig, Vec<(&'static str, String)>) {
    let mut invalid_values = Vec::new();
    let media = read_boolean("CHATT_GUI_MEDIA_LOG", &mut invalid_values);
    let render = read_boolean("CHATT_GUI_RENDER_LOG", &mut invalid_values);
    let rpc = read_boolean("CHATT_GUI_RPC_LOG", &mut invalid_values);
    let default_level = env::var("RUST_LOG")
        .ok()
        .as_deref()
        .and_then(parse_simple_level)
        .unwrap_or(LevelFilter::Warn);
    let native_mpv_level = env::var("CHATT_MPV_LOG")
        .ok()
        .as_deref()
        .and_then(parse_mpv_level)
        .unwrap_or(LevelFilter::Warn);

    (
        FacadeConfig {
            default_level,
            native_mpv_level,
            media,
            render,
            rpc,
        },
        invalid_values,
    )
}

fn read_boolean(variable: &'static str, invalid_values: &mut Vec<(&'static str, String)>) -> bool {
    let Ok(value) = env::var(variable) else {
        return false;
    };
    match parse_boolean(&value) {
        Some(enabled) => enabled,
        None => {
            invalid_values.push((variable, value));
            false
        }
    }
}

fn parse_boolean(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "" | "0" | "false" | "no" | "off" => Some(false),
        "1" | "true" | "yes" | "on" => Some(true),
        _ => None,
    }
}

fn parse_simple_level(value: &str) -> Option<LevelFilter> {
    match value.trim().to_ascii_lowercase().as_str() {
        "off" => Some(LevelFilter::Off),
        "error" => Some(LevelFilter::Error),
        "warn" => Some(LevelFilter::Warn),
        "info" | "v" | "debug" | "trace" => Some(LevelFilter::Info),
        _ => None,
    }
}

fn parse_mpv_level(value: &str) -> Option<LevelFilter> {
    match value.trim().to_ascii_lowercase().as_str() {
        "off" => Some(LevelFilter::Off),
        "error" => Some(LevelFilter::Error),
        "warn" => Some(LevelFilter::Warn),
        "info" => Some(LevelFilter::Info),
        _ => None,
    }
}

fn classify(target: &str, level: Level, config: FacadeConfig) -> Option<FacadeDecision> {
    let (threshold, group) = if target == "chatt_mpv" {
        (config.native_mpv_level, Some(NATIVE_MPV_GROUP))
    } else if target.starts_with("gpui_wgpu::video") {
        let threshold = if cfg!(feature = "diagnostic-logs") && config.render {
            LevelFilter::Info
        } else {
            LevelFilter::Warn
        };
        (threshold, Some(RENDER_GROUP))
    } else {
        (config.default_level, None)
    };
    (level <= threshold).then(|| FacadeDecision {
        level: facade_level(level),
        group,
    })
}

fn facade_level(level: Level) -> LogLevel {
    match level {
        Level::Error => LogLevel::Error,
        Level::Warn => LogLevel::Warn,
        Level::Info => LogLevel::Info,
        Level::Debug | Level::Trace => {
            unreachable!("the log facade is compiled with a fixed Info maximum")
        }
    }
}

fn encode_record(encoder: &mut Encoder, record: &Record<'_>, decision: FacadeDecision) {
    let mut fields = encoder.append_now(decision.level);
    record
        .target()
        .encode_log_value_into(fields.static_key(StaticKey::target));
    fields
        .static_key(StaticKey::msg)
        .value_via_display(record.args());
    if let Some(group) = decision.group {
        group.encode_log_value_into(fields.dynamic_key("group"));
    }
    fields.apply_current_span();
}

fn write_record(record: &Record<'_>, decision: FacadeDecision) {
    let mut queue = kvlog::global_logger();
    encode_record(&mut queue.encoder, record, decision);
    queue.poke();
}

#[cfg(test)]
mod tests {
    use std::{
        fmt,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use kvlog::encoding::{Key, Value};

    use super::*;

    fn config() -> FacadeConfig {
        FacadeConfig {
            default_level: LevelFilter::Warn,
            native_mpv_level: LevelFilter::Warn,
            media: false,
            render: false,
            rpc: false,
        }
    }

    #[test]
    fn parses_boolean_group_controls() {
        for value in ["", "0", "false", "FALSE", " no ", "off"] {
            assert_eq!(parse_boolean(value), Some(false));
        }
        for value in ["1", "true", "TRUE", " yes ", "on"] {
            assert_eq!(parse_boolean(value), Some(true));
        }
        assert_eq!(parse_boolean("sometimes"), None);
    }

    #[test]
    fn caps_simple_rust_log_at_info() {
        assert_eq!(parse_simple_level("off"), Some(LevelFilter::Off));
        assert_eq!(parse_simple_level("error"), Some(LevelFilter::Error));
        assert_eq!(parse_simple_level("warn"), Some(LevelFilter::Warn));
        assert_eq!(parse_simple_level("info"), Some(LevelFilter::Info));
        assert_eq!(parse_simple_level("debug"), Some(LevelFilter::Info));
        assert_eq!(parse_simple_level("trace"), Some(LevelFilter::Info));
        assert_eq!(parse_simple_level("crate=debug"), None);
    }

    #[test]
    fn native_mpv_threshold_and_group_are_preserved() {
        let mut config = config();
        assert!(classify("chatt_mpv", Level::Info, config).is_none());
        config.native_mpv_level = LevelFilter::Info;
        assert_eq!(
            classify("chatt_mpv", Level::Info, config),
            Some(FacadeDecision {
                level: LogLevel::Info,
                group: Some(NATIVE_MPV_GROUP),
            })
        );
    }

    #[test]
    fn wgpu_video_info_requires_both_diagnostic_gates() {
        let mut config = config();
        config.render = true;
        assert_eq!(
            classify("gpui_wgpu::video_surface", Level::Info, config).is_some(),
            cfg!(feature = "diagnostic-logs")
        );
        assert!(classify("gpui_wgpu::video_surface", Level::Warn, config).is_some());
    }

    #[test]
    fn default_external_filter_uses_global_ceiling() {
        let mut config = config();
        assert!(classify("calloop", Level::Info, config).is_none());
        assert!(classify("calloop", Level::Warn, config).is_some());
        config.default_level = LevelFilter::Error;
        assert!(classify("calloop", Level::Warn, config).is_none());
    }

    #[test]
    fn maps_facade_levels() {
        assert_eq!(facade_level(Level::Error), LogLevel::Error);
        assert_eq!(facade_level(Level::Warn), LogLevel::Warn);
        assert_eq!(facade_level(Level::Info), LogLevel::Info);
    }

    #[test]
    fn encodes_target_message_and_semantic_group_directly() {
        let record = Record::builder()
            .target("chatt_mpv")
            .level(Level::Warn)
            .args(format_args!("mpv[core] failed"))
            .build();
        let mut encoder = Encoder::new();
        encode_record(
            &mut encoder,
            &record,
            FacadeDecision {
                level: LogLevel::Warn,
                group: Some(NATIVE_MPV_GROUP),
            },
        );
        let (_, level, _, fields) = kvlog::encoding::decode(encoder.bytes())
            .next()
            .unwrap()
            .unwrap();
        assert_eq!(level, LogLevel::Warn);
        let fields = fields
            .map(Result::unwrap)
            .map(|(key, value)| {
                let key = match key {
                    Key::Static(key) => key.as_str().to_owned(),
                    Key::Dynamic(key) => String::from_utf8_lossy(key).into_owned(),
                };
                let value = match value {
                    Value::String(value) => String::from_utf8_lossy(value).into_owned(),
                    _ => panic!("expected string field"),
                };
                (key, value)
            })
            .collect::<Vec<_>>();
        assert!(fields.contains(&("target".into(), "chatt_mpv".into())));
        assert!(fields.contains(&("msg".into(), "mpv[core] failed".into())));
        assert!(fields.contains(&("group".into(), NATIVE_MPV_GROUP.into())));
    }

    struct CountedDisplay<'a>(&'a AtomicUsize);

    impl fmt::Display for CountedDisplay<'_> {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            self.0.fetch_add(1, Ordering::SeqCst);
            formatter.write_str("formatted")
        }
    }

    #[test]
    fn rejected_records_are_not_formatted() {
        let formats = AtomicUsize::new(0);
        let arguments = format_args!("{}", CountedDisplay(&formats));
        let record = Record::builder()
            .target("dependency")
            .level(Level::Info)
            .args(arguments)
            .build();
        FacadeLogger { config: config() }.log(&record);
        assert_eq!(formats.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn one_enabled_group_leaves_other_groups_disabled() {
        let config = FacadeConfig {
            media: true,
            ..config()
        };
        assert!(config.media);
        assert!(!config.render);
        assert!(!config.rpc);
    }

    #[test]
    fn native_records_preserve_target_and_error_fields_then_flush_on_shutdown() {
        kvlog::collector::set_uninitialized_log_policy(
            kvlog::collector::UninitializedLogPolicy::Buffer {
                max_bytes: 16 * 1024,
            },
        );
        let error = "representative failure";
        kvlog::warn!("representative native failure", err = %error);
        let bytes = {
            let mut queue = kvlog::global_logger();
            let bytes = queue.encoder.bytes().to_vec();
            queue.encoder.clear();
            bytes
        };
        let (_, level, _, fields) = kvlog::encoding::decode(&bytes).next().unwrap().unwrap();
        assert_eq!(level, LogLevel::Warn);
        let fields = fields.map(Result::unwrap).collect::<Vec<_>>();
        assert!(fields.iter().any(|(key, value)| {
            *key == StaticKey::target
                && matches!(value, Value::String(value) if *value == b"chatt_gui::logger::tests")
        }));
        assert!(fields.iter().any(|(key, value)| {
            *key == StaticKey::err
                && matches!(value, Value::String(value) if *value == error.as_bytes())
        }));

        let guard = kvlog::collector::init_stdout_logger();
        kvlog::info!("collector guard lifetime test");
        guard.flush();
        drop(guard);
    }
}
