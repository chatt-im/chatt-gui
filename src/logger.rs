use std::{env, sync::OnceLock};

use log::{LevelFilter, Log, Metadata, Record};

struct StderrLogger {
    default_level: LevelFilter,
    chatt_level: LevelFilter,
    video_level: LevelFilter,
    native_mpv_level: LevelFilter,
}

impl Log for StderrLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        let target = metadata.target();
        let level = if target == "chatt_mpv" {
            self.native_mpv_level
        } else if target.starts_with("chatt_gui") {
            self.chatt_level
        } else if target.starts_with("gpui_wgpu::video") {
            self.video_level
        } else {
            self.default_level
        };
        metadata.level() <= level
    }

    fn log(&self, record: &Record<'_>) {
        if self.enabled(record.metadata()) {
            eprintln!("{} {}: {}", record.level(), record.target(), record.args());
        }
    }

    fn flush(&self) {}
}

pub fn init() {
    static LOGGER: OnceLock<StderrLogger> = OnceLock::new();
    let requested = env::var("RUST_LOG").ok();
    let native_mpv_level = env::var("CHATT_MPV_LOG")
        .ok()
        .as_deref()
        .and_then(parse_simple_level)
        .unwrap_or(LevelFilter::Warn);
    let logger = LOGGER.get_or_init(|| StderrLogger {
        default_level: requested
            .as_deref()
            .and_then(parse_simple_level)
            .unwrap_or(LevelFilter::Warn),
        chatt_level: LevelFilter::Info,
        video_level: LevelFilter::Info,
        native_mpv_level,
    });
    let max_level = logger
        .default_level
        .max(logger.chatt_level)
        .max(logger.video_level)
        .max(logger.native_mpv_level);
    if log::set_logger(logger).is_ok() {
        log::set_max_level(max_level);
    }
}

fn parse_simple_level(value: &str) -> Option<LevelFilter> {
    match value.trim().to_ascii_lowercase().as_str() {
        "off" => Some(LevelFilter::Off),
        "error" => Some(LevelFilter::Error),
        "warn" => Some(LevelFilter::Warn),
        "info" => Some(LevelFilter::Info),
        "v" | "debug" => Some(LevelFilter::Debug),
        "trace" => Some(LevelFilter::Trace),
        _ => None,
    }
}
