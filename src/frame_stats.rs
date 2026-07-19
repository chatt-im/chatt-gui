use std::{
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::{Duration, Instant},
};

use gpui::{App, Window, profiler};

const REPORT_INTERVAL: Duration = Duration::from_secs(2);
static ENABLED: AtomicBool = AtomicBool::new(true);
static DISPLAY_FRAMES: AtomicU64 = AtomicU64::new(0);
static SCROLL_INPUTS: AtomicU64 = AtomicU64::new(0);
static SCROLL_UPDATES: AtomicU64 = AtomicU64::new(0);
static SCROLL_TRACES: AtomicU64 = AtomicU64::new(0);

pub fn start(cx: &mut App) {
    if std::env::var_os("CHATT_FRAME_STATS").is_some_and(|value| value == "0") {
        ENABLED.store(false, Ordering::Relaxed);
        return;
    }

    profiler::set_frame_trace_enabled(true);
    let mut collector = profiler::FrameTimingCollector::new();

    eprintln!(
        "[chatt frame] reporting GPUI draw statistics every {:.0}s (set CHATT_FRAME_STATS=0 to disable)",
        REPORT_INTERVAL.as_secs_f64()
    );

    cx.spawn(async move |cx| {
        let mut interval_start = Instant::now();
        loop {
            cx.background_executor().timer(REPORT_INTERVAL).await;

            let interval_end = Instant::now();
            let frames = collector.collect_unseen();
            let elapsed = interval_end.duration_since(interval_start);
            interval_start = interval_end;
            let elapsed_seconds = elapsed.as_secs_f64();
            let display_frames = DISPLAY_FRAMES.swap(0, Ordering::Relaxed);
            let scroll_inputs = SCROLL_INPUTS.swap(0, Ordering::Relaxed);
            let scroll_updates = SCROLL_UPDATES.swap(0, Ordering::Relaxed);
            let display_hz = display_frames as f64 / elapsed_seconds;
            let scroll_input_hz = scroll_inputs as f64 / elapsed_seconds;
            let scroll_update_hz = scroll_updates as f64 / elapsed_seconds;

            if frames.is_empty() {
                eprintln!(
                    "[chatt frame] {display_hz:.1} display hz | idle: 0 draws in {elapsed_seconds:.2}s | scroll {scroll_input_hz:.1} input/s {scroll_update_hz:.1} update/s",
                );
                continue;
            }

            let mut draw_times: Vec<_> = frames
                .iter()
                .map(profiler::FrameTiming::draw_duration)
                .collect();
            let mut dirty_to_draw_times: Vec<_> = frames
                .iter()
                .filter_map(profiler::FrameTiming::dirty_to_draw_duration)
                .collect();
            draw_times.sort_unstable();
            dirty_to_draw_times.sort_unstable();

            let frame_count = frames.len();
            let fps = frame_count as f64 / elapsed_seconds;
            let invalidations: u64 = frames.iter().map(|frame| frame.invalidations).sum();
            let invalidations_per_frame = invalidations as f64 / frame_count as f64;
            let draw_p50 = percentile(&draw_times, 50);
            let draw_p95 = percentile(&draw_times, 95);
            let draw_max = draw_times.last().copied().unwrap_or_default();

            if let Some(dirty_p95) = percentile(&dirty_to_draw_times, 95) {
                eprintln!(
                    "[chatt frame] {display_hz:.1} display hz | {fps:.1} draw fps ({frame_count} in {elapsed_seconds:.2}s) | draw p50 {} p95 {} max {} | dirty-to-draw p95 {} | invalidations/frame {invalidations_per_frame:.1} | scroll {scroll_input_hz:.1} input/s {scroll_update_hz:.1} update/s",
                    format_duration(draw_p50.unwrap_or_default()),
                    format_duration(draw_p95.unwrap_or_default()),
                    format_duration(draw_max),
                    format_duration(dirty_p95),
                );
            } else {
                eprintln!(
                    "[chatt frame] {display_hz:.1} display hz | {fps:.1} draw fps ({frame_count} in {elapsed_seconds:.2}s) | draw p50 {} p95 {} max {} | invalidations/frame {invalidations_per_frame:.1} | scroll {scroll_input_hz:.1} input/s {scroll_update_hz:.1} update/s",
                    format_duration(draw_p50.unwrap_or_default()),
                    format_duration(draw_p95.unwrap_or_default()),
                    format_duration(draw_max),
                );
            }
        }
    })
    .detach();
}

pub fn start_window(window: &Window) {
    if ENABLED.load(Ordering::Relaxed) {
        window.on_next_frame(record_display_frame);
    }
}

fn record_display_frame(window: &mut Window, _: &mut App) {
    DISPLAY_FRAMES.fetch_add(1, Ordering::Relaxed);
    window.on_next_frame(record_display_frame);
}

pub fn record_scroll_input() {
    if ENABLED.load(Ordering::Relaxed) {
        SCROLL_INPUTS.fetch_add(1, Ordering::Relaxed);
    }
}

pub fn record_scroll_update() {
    if ENABLED.load(Ordering::Relaxed) {
        SCROLL_UPDATES.fetch_add(1, Ordering::Relaxed);
    }
}

pub fn trace_scroll(message: impl FnOnce() -> String) {
    if ENABLED.load(Ordering::Relaxed) && SCROLL_TRACES.fetch_add(1, Ordering::Relaxed) < 24 {
        eprintln!("[chatt scroll] {}", message());
    }
}

fn percentile(samples: &[Duration], percentile: usize) -> Option<Duration> {
    if samples.is_empty() {
        return None;
    }

    let index = (samples.len() * percentile).div_ceil(100).saturating_sub(1);
    samples.get(index).copied()
}

fn format_duration(duration: Duration) -> String {
    format!("{:.2}ms", duration.as_secs_f64() * 1_000.)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_uses_nearest_rank() {
        let samples: Vec<_> = (1..=100).map(Duration::from_millis).collect();

        assert_eq!(percentile(&samples, 50), Some(Duration::from_millis(50)));
        assert_eq!(percentile(&samples, 95), Some(Duration::from_millis(95)));
    }

    #[test]
    fn percentile_rejects_empty_samples() {
        assert_eq!(percentile(&[], 95), None);
    }
}
