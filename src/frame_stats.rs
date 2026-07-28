use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
#[cfg(any(feature = "diagnostic-logs", test))]
use std::time::Duration;
#[cfg(feature = "diagnostic-logs")]
use std::time::Instant;

#[cfg(feature = "diagnostic-logs")]
use gpui::profiler;
use gpui::{App, Window};
#[cfg(feature = "input-latency")]
use gpui::{AnyWindowHandle, AppContext as _};

#[cfg(feature = "diagnostic-logs")]
const REPORT_INTERVAL: Duration = Duration::from_secs(2);
static ENABLED: AtomicBool = AtomicBool::new(false);
static DISPLAY_FRAMES: AtomicU64 = AtomicU64::new(0);
static SCROLL_INPUTS: AtomicU64 = AtomicU64::new(0);
static SCROLL_UPDATES: AtomicU64 = AtomicU64::new(0);
static SCROLL_TRACES: AtomicU64 = AtomicU64::new(0);

/// Window whose input-latency histograms the reporting loop samples. Set once,
/// when the only window opens.
#[cfg(feature = "input-latency")]
static LATENCY_WINDOW: std::sync::Mutex<Option<AnyWindowHandle>> = std::sync::Mutex::new(None);

pub fn start(cx: &mut App) {
    #[cfg(feature = "diagnostic-logs")]
    start_diagnostics(cx);
    #[cfg(not(feature = "diagnostic-logs"))]
    let _ = cx;
}

#[cfg(feature = "diagnostic-logs")]
fn start_diagnostics(cx: &mut App) {
    if !crate::logger::render_logging_enabled() {
        ENABLED.store(false, Ordering::Relaxed);
        return;
    }

    ENABLED.store(true, Ordering::Relaxed);
    profiler::set_frame_trace_enabled(true);
    let mut collector = profiler::FrameTimingCollector::new();

    kvlog::info!(
        "frame timing diagnostics started",
        group = "render",
        interval_seconds = REPORT_INTERVAL.as_secs_f64()
    );

    cx.spawn(async move |cx| {
        let mut interval_start = Instant::now();
        #[cfg(feature = "input-latency")]
        let mut previous_latency = None;
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

            #[cfg(feature = "input-latency")]
            report_input_latency(cx, &mut previous_latency, elapsed_seconds);

            if frames.is_empty() {
                kvlog::info!(
                    "frame timing summary",
                    group = "render",
                    display_hz,
                    draw_count = 0u64,
                    elapsed_seconds,
                    scroll_input_hz,
                    scroll_update_hz
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
            let per_frame = |total: u64| total as f64 / frame_count as f64;
            let layout_nodes = per_frame(frames.iter().map(|frame| frame.layout.nodes).sum());
            let measure_calls =
                per_frame(frames.iter().map(|frame| frame.layout.measure_calls).sum());
            let views_reused = per_frame(frames.iter().map(|frame| frame.views_reused).sum());
            let views_rendered = per_frame(frames.iter().map(|frame| frame.views_rendered).sum());
            let sum32 = |total: u32| total as u64;
            let scene_primitives = per_frame(
                frames
                    .iter()
                    .map(|frame| sum32(frame.scene.primitives))
                    .sum(),
            );
            let scene_sprites =
                per_frame(frames.iter().map(|frame| sum32(frame.scene.sprites)).sum());
            let scene_quads = per_frame(frames.iter().map(|frame| sum32(frame.scene.quads)).sum());
            let scene_kib = per_frame(
                frames
                    .iter()
                    .map(|frame| sum32(frame.scene.instance_bytes))
                    .sum(),
            ) / 1024.0;
            let draw_p50 = percentile(&draw_times, 50);
            let draw_p95 = percentile(&draw_times, 95);
            let draw_max = draw_times.last().copied().unwrap_or_default();

            if let Some(dirty_p95) = percentile(&dirty_to_draw_times, 95) {
                kvlog::info!(
                    "frame timing summary",
                    group = "render",
                    display_hz,
                    fps,
                    frame_count,
                    elapsed_seconds,
                    draw_p50_ms = draw_p50.unwrap_or_default().as_secs_f64() * 1_000.0,
                    draw_p95_ms = draw_p95.unwrap_or_default().as_secs_f64() * 1_000.0,
                    draw_max_ms = draw_max.as_secs_f64() * 1_000.0,
                    dirty_to_draw_p95_ms = dirty_p95.as_secs_f64() * 1_000.0,
                    invalidations_per_frame,
                    layout_nodes,
                    measure_calls,
                    views_reused,
                    views_rendered,
                    scene_primitives,
                    scene_sprites,
                    scene_quads,
                    scene_kib,
                    scroll_input_hz,
                    scroll_update_hz
                );
            } else {
                kvlog::info!(
                    "frame timing summary",
                    group = "render",
                    display_hz,
                    fps,
                    frame_count,
                    elapsed_seconds,
                    draw_p50_ms = draw_p50.unwrap_or_default().as_secs_f64() * 1_000.0,
                    draw_p95_ms = draw_p95.unwrap_or_default().as_secs_f64() * 1_000.0,
                    draw_max_ms = draw_max.as_secs_f64() * 1_000.0,
                    invalidations_per_frame,
                    layout_nodes,
                    measure_calls,
                    views_reused,
                    views_rendered,
                    scene_primitives,
                    scene_sprites,
                    scene_quads,
                    scene_kib,
                    scroll_input_hz,
                    scroll_update_hz
                );
            }
        }
    })
    .detach();
}

pub fn start_window(window: &Window) {
    if ENABLED.load(Ordering::Relaxed) {
        #[cfg(feature = "input-latency")]
        LATENCY_WINDOW
            .lock()
            .unwrap()
            .replace(window.window_handle());
        window.on_next_frame(record_display_frame);
    }
}

fn record_display_frame(window: &mut Window, _: &mut App) {
    DISPLAY_FRAMES.fetch_add(1, Ordering::Relaxed);
    window.on_next_frame(record_display_frame);
}

/// Logs the input-to-present latency accumulated since the previous report.
///
/// GPUI's tracker is cumulative for the life of the window, so each interval
/// subtracts the previous cumulative snapshot to recover that interval's
/// samples. `first_input_at` is stamped when GPUI begins dispatching the event
/// and flushed when the frame it caused is committed, so this spans the part of
/// input latency the client controls — it excludes compositor-to-client
/// delivery and everything after the commit.
#[cfg(feature = "input-latency")]
fn report_input_latency(
    cx: &mut gpui::AsyncApp,
    previous: &mut Option<gpui::InputLatencySnapshot>,
    elapsed_seconds: f64,
) {
    let Some(handle) = *LATENCY_WINDOW.lock().unwrap() else {
        return;
    };
    let Ok(snapshot) = cx.update_window(handle, |_, window, _| window.input_latency_snapshot())
    else {
        return;
    };

    let mut latency = snapshot.latency_histogram.clone();
    let mut events = snapshot.events_per_frame_histogram.clone();
    let mut dropped = snapshot.mid_draw_events_dropped;
    if let Some(previous) = previous.as_ref() {
        // A failed subtraction leaves the interval histogram holding cumulative
        // counts. Reporting slightly stale percentiles beats dropping the line.
        latency.subtract(&previous.latency_histogram).ok();
        events.subtract(&previous.events_per_frame_histogram).ok();
        dropped = dropped.saturating_sub(previous.mid_draw_events_dropped);
    }
    *previous = Some(snapshot);

    if latency.is_empty() {
        return;
    }

    let ms = |nanos: u64| nanos as f64 / 1_000_000.0;
    kvlog::info!(
        "input latency summary",
        group = "render",
        elapsed_seconds,
        input_samples = latency.len(),
        input_hz = latency.len() as f64 / elapsed_seconds,
        input_to_present_p50_ms = ms(latency.value_at_quantile(0.50)),
        input_to_present_p95_ms = ms(latency.value_at_quantile(0.95)),
        input_to_present_p99_ms = ms(latency.value_at_quantile(0.99)),
        input_to_present_max_ms = ms(latency.max()),
        events_per_frame_p50 = events.value_at_quantile(0.50),
        events_per_frame_max = events.max(),
        mid_draw_dropped = dropped
    );
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
        kvlog::info!("scroll trace", group = "render", detail = %message());
    }
}

#[cfg(any(feature = "diagnostic-logs", test))]
fn percentile(samples: &[Duration], percentile: usize) -> Option<Duration> {
    if samples.is_empty() {
        return None;
    }

    let index = (samples.len() * percentile).div_ceil(100).saturating_sub(1);
    samples.get(index).copied()
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
