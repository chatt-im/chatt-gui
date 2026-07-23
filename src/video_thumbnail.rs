use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    sync::{Arc, Condvar, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, anyhow, bail};
use async_channel::Sender as AsyncSender;
use gpui::RenderImage;
use image::{Frame, RgbaImage};
use libmpv2::{
    Format, Mpv,
    events::Event,
    render::{SoftwareRenderTarget, mpv_render_update},
};
use local_rpc::model::AttachmentId;

const FORMAT_RGBA: &std::ffi::CStr = c"rgba";
const MAX_WIDTH: u32 = 1_360;
const MAX_HEIGHT: u32 = 840;
const EXTRACT_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_QUEUED_JOBS: usize = 16;
const MAX_CACHE_ENTRIES: usize = 256;
const RETRY_BASE_DELAY: Duration = Duration::from_secs(2);
const MAX_RETRY_SHIFT: u32 = 6;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ThumbnailKey {
    pub attachment_id: AttachmentId,
}

#[derive(Clone, Default)]
pub(crate) struct ThumbnailView {
    pub image: Option<Arc<RenderImage>>,
    pub duration: Option<f64>,
    pub failed: bool,
}

enum CacheState {
    Pending,
    Ready {
        image: Arc<RenderImage>,
        duration: Option<f64>,
        byte_len: usize,
    },
    Failed {
        error: String,
        retry_at: Instant,
    },
}

struct CacheEntry {
    state: CacheState,
    touched: u64,
    failures: u32,
}

struct ThumbnailJob {
    key: ThumbnailKey,
    path: PathBuf,
    generation: u64,
}

struct ThumbnailResult {
    key: ThumbnailKey,
    generation: u64,
    result: Result<ExtractedThumbnail, String>,
}

struct ExtractedThumbnail {
    image: Arc<RenderImage>,
    duration: Option<f64>,
    byte_len: usize,
}

#[derive(Default)]
struct ThumbnailWorkQueue {
    jobs: VecDeque<ThumbnailJob>,
    closed: bool,
}

type SharedWorkQueue = Arc<(Mutex<ThumbnailWorkQueue>, Condvar)>;

/// A bounded timeline thumbnail cache backed by one lazily-created, persistent
/// software libmpv core. Extraction is serial so thumbnails never contend for
/// the application's playback render device.
pub(crate) struct VideoThumbnailCache {
    entries: HashMap<ThumbnailKey, CacheEntry>,
    total_bytes: usize,
    budget_bytes: usize,
    clock: u64,
    generation: u64,
    jobs: SharedWorkQueue,
    worker_started: bool,
    results: mpsc::Receiver<ThumbnailResult>,
    worker_results: mpsc::Sender<ThumbnailResult>,
    wakeup: AsyncSender<()>,
}

impl VideoThumbnailCache {
    pub(crate) fn new(budget_bytes: usize, wakeup: AsyncSender<()>) -> Self {
        let (worker_results, results) = mpsc::channel();
        Self {
            entries: HashMap::new(),
            total_bytes: 0,
            budget_bytes,
            clock: 0,
            generation: 0,
            jobs: Arc::new((Mutex::new(ThumbnailWorkQueue::default()), Condvar::new())),
            worker_started: false,
            results,
            worker_results,
            wakeup,
        }
    }

    pub(crate) fn clear(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.entries.clear();
        self.total_bytes = 0;
        self.jobs.0.lock().unwrap().jobs.clear();
        while self.results.try_recv().is_ok() {}
    }

    pub(crate) fn request(&mut self, key: ThumbnailKey, path: PathBuf) -> ThumbnailView {
        self.drain_results();
        self.clock = self.clock.wrapping_add(1);
        let now = Instant::now();
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.touched = self.clock;
            match &entry.state {
                CacheState::Failed { retry_at, .. } if now >= *retry_at => {
                    entry.state = CacheState::Pending;
                }
                _ => return view_for_entry(entry),
            }
        } else {
            self.entries.insert(
                key,
                CacheEntry {
                    state: CacheState::Pending,
                    touched: self.clock,
                    failures: 0,
                },
            );
        }

        if let Err(error) = self.start_worker() {
            self.record_failure(key, error);
            return self
                .entries
                .get(&key)
                .map(view_for_entry)
                .unwrap_or_default();
        }
        match self.enqueue(ThumbnailJob {
            key,
            path,
            generation: self.generation,
        }) {
            Ok(dropped) => {
                if let Some(dropped) = dropped {
                    self.entries.remove(&dropped);
                }
            }
            Err(error) => self.record_failure(key, error),
        }
        self.evict();
        self.entries
            .get(&key)
            .map(view_for_entry)
            .unwrap_or_default()
    }

    pub(crate) fn drain_results(&mut self) -> bool {
        let mut changed = false;
        while let Ok(result) = self.results.try_recv() {
            if result.generation != self.generation {
                continue;
            }
            let Some(entry) = self.entries.get_mut(&result.key) else {
                continue;
            };
            changed = true;
            match result.result {
                Ok(thumbnail) => {
                    self.total_bytes = self.total_bytes.saturating_add(thumbnail.byte_len);
                    entry.state = CacheState::Ready {
                        image: thumbnail.image,
                        duration: thumbnail.duration,
                        byte_len: thumbnail.byte_len,
                    };
                }
                Err(error) => {
                    log::warn!("video thumbnail extraction failed: {error}");
                    entry.failures = entry.failures.saturating_add(1);
                    entry.state = CacheState::Failed {
                        error,
                        retry_at: Instant::now() + retry_delay(entry.failures),
                    };
                }
            }
        }
        if changed {
            self.evict();
        }
        changed
    }

    fn start_worker(&mut self) -> Result<(), String> {
        if self.worker_started {
            return Ok(());
        }
        let jobs = self.jobs.clone();
        let results = self.worker_results.clone();
        let wakeup = self.wakeup.clone();
        thread::Builder::new()
            .name("mpv-thumbnail".into())
            .spawn(move || thumbnail_worker(jobs, results, wakeup))
            .map_err(|error| format!("could not start video thumbnail worker: {error}"))?;
        self.worker_started = true;
        Ok(())
    }

    fn enqueue(&self, job: ThumbnailJob) -> Result<Option<ThumbnailKey>, String> {
        let (jobs, ready) = &*self.jobs;
        let mut jobs = jobs
            .lock()
            .map_err(|_| "thumbnail work queue lock poisoned".to_string())?;
        if jobs.closed {
            return Err("thumbnail worker stopped".into());
        }
        let dropped = (jobs.jobs.len() >= MAX_QUEUED_JOBS)
            .then(|| jobs.jobs.pop_front().map(|job| job.key))
            .flatten();
        jobs.jobs.push_back(job);
        ready.notify_one();
        Ok(dropped)
    }

    fn record_failure(&mut self, key: ThumbnailKey, error: String) {
        log::warn!("video thumbnail extraction failed: {error}");
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.failures = entry.failures.saturating_add(1);
            entry.state = CacheState::Failed {
                error,
                retry_at: Instant::now() + retry_delay(entry.failures),
            };
        }
    }

    fn evict(&mut self) {
        while self.total_bytes > self.budget_bytes {
            let Some(key) = self
                .entries
                .iter()
                .filter(|(_, entry)| matches!(entry.state, CacheState::Ready { .. }))
                .min_by_key(|(_, entry)| entry.touched)
                .map(|(key, _)| *key)
            else {
                break;
            };
            self.remove_entry(key);
        }
        while self.entries.len() > MAX_CACHE_ENTRIES {
            let Some(key) = self
                .entries
                .iter()
                .filter(|(_, entry)| !matches!(entry.state, CacheState::Pending))
                .min_by_key(|(_, entry)| entry.touched)
                .map(|(key, _)| *key)
            else {
                break;
            };
            self.remove_entry(key);
        }
    }

    fn remove_entry(&mut self, key: ThumbnailKey) {
        if let Some(CacheEntry {
            state: CacheState::Ready { byte_len, .. },
            ..
        }) = self.entries.remove(&key)
        {
            self.total_bytes = self.total_bytes.saturating_sub(byte_len);
        }
    }
}

impl Drop for VideoThumbnailCache {
    fn drop(&mut self) {
        let (jobs, ready) = &*self.jobs;
        if let Ok(mut jobs) = jobs.lock() {
            jobs.closed = true;
            jobs.jobs.clear();
            ready.notify_one();
        }
    }
}

fn view_for_entry(entry: &CacheEntry) -> ThumbnailView {
    match &entry.state {
        CacheState::Ready {
            image, duration, ..
        } => ThumbnailView {
            image: Some(image.clone()),
            duration: *duration,
            failed: false,
        },
        CacheState::Failed { error, .. } => {
            let _ = error;
            ThumbnailView {
                failed: true,
                ..ThumbnailView::default()
            }
        }
        CacheState::Pending => ThumbnailView::default(),
    }
}

fn thumbnail_worker(
    jobs: SharedWorkQueue,
    results: mpsc::Sender<ThumbnailResult>,
    wakeup: AsyncSender<()>,
) {
    log::info!("lazy video thumbnail worker started");
    let mut extractor = None;
    loop {
        let job = {
            let (jobs, ready) = &*jobs;
            let mut jobs = match jobs.lock() {
                Ok(jobs) => jobs,
                Err(_) => break,
            };
            while jobs.jobs.is_empty() && !jobs.closed {
                jobs = match ready.wait(jobs) {
                    Ok(jobs) => jobs,
                    Err(_) => return,
                };
            }
            if jobs.closed {
                break;
            }
            jobs.jobs.pop_back().expect("non-empty thumbnail queue")
        };
        if extractor.is_none() {
            extractor = match ThumbnailExtractor::new() {
                Ok(created) => Some(created),
                Err(error) => {
                    log::error!("video thumbnail decoder initialization failed: {error:#}");
                    None
                }
            };
        }
        let result = match extractor.as_mut() {
            Some(extractor) => extractor
                .extract(&job.path)
                .map_err(|error| format!("{error:#}")),
            None => Err("initialize thumbnail libmpv core".into()),
        };
        if result.is_err() {
            extractor = None;
        }
        if results
            .send(ThumbnailResult {
                key: job.key,
                generation: job.generation,
                result,
            })
            .is_err()
        {
            break;
        }
        let _ = wakeup.try_send(());
    }
    log::info!("video thumbnail worker stopped");
}

struct ThumbnailExtractor {
    mpv: Arc<Mpv>,
    render: libmpv2::render::RenderContext,
    aligned: Vec<u8>,
}

impl ThumbnailExtractor {
    fn new() -> Result<Self> {
        let mpv = Arc::new(
            Mpv::with_initializer(|initializer| {
                initializer.set_option("vo", "libmpv")?;
                initializer.set_option("idle", "yes")?;
                initializer.set_option("keep-open", "no")?;
                initializer.set_option("pause", "no")?;
                initializer.set_option("audio", "no")?;
                initializer.set_option("sub", "no")?;
                initializer.set_option("hwdec", "no")?;
                initializer.set_option("profile", "fast")?;
                initializer.set_option("untimed", "yes")?;
                initializer.set_option("video-sync", "display-desync")?;
                initializer.set_option("cache", "no")?;
                initializer.set_option("sws-allow-zimg", "no")?;
                initializer.set_option("sws-scaler", "bilinear")?;
                initializer.set_option("sws-fast", "yes")?;
                Ok(())
            })
            .context("initialize thumbnail libmpv core")?,
        );
        mpv.observe_property("duration", Format::Double, 1)?;
        let render = mpv
            .create_software_render_context(false)
            .context("create thumbnail software render context")?;
        Ok(Self {
            mpv,
            render,
            aligned: Vec::new(),
        })
    }

    fn extract(&mut self, path: &PathBuf) -> Result<ExtractedThumbnail> {
        self.reset_source()?;
        let result = self.extract_frame(path);
        let reset = self.reset_source();
        match (result, reset) {
            (Ok(thumbnail), Ok(())) => Ok(thumbnail),
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(error).context("reset thumbnail source after extraction"),
        }
    }

    fn extract_frame(&mut self, path: &PathBuf) -> Result<ExtractedThumbnail> {
        self.mpv.set_property("pause", false)?;
        self.mpv
            .command("loadfile", &[&path.to_string_lossy(), "replace"])
            .with_context(|| format!("open video thumbnail source {path:?}"))?;

        let deadline = Instant::now() + EXTRACT_TIMEOUT;
        let mut source_size = None;
        let mut duration = None;
        let mut loaded = false;
        let mut ended = None;
        while Instant::now() < deadline {
            if let Some(event) = self.mpv.wait_event(0.01) {
                match event? {
                    Event::FileLoaded => {
                        loaded = true;
                        source_size = video_size(&self.mpv).ok().or(source_size);
                    }
                    Event::VideoReconfig => {
                        source_size = video_size(&self.mpv).ok().or(source_size);
                    }
                    Event::PropertyChange {
                        change: libmpv2::events::PropertyData::Double(value),
                        ..
                    } => duration = (value.is_finite() && value > 0.0).then_some(value),
                    Event::EndFile(reason) => ended = Some(format!("{reason:?}")),
                    _ => {}
                }
            }
            source_size = video_size(&self.mpv).ok().or(source_size);
            let updates = self.render.update();
            let has_frame_update = updates & u64::from(mpv_render_update::Frame) != 0;
            // Very short videos may report EndFile after making their only
            // frame current but without another edge-triggered update. Render
            // that retained frame rather than treating normal EOF as failure.
            if !has_frame_update && ended.is_none() {
                continue;
            }
            if !loaded {
                if has_frame_update {
                    self.render.skip_rendering()?;
                }
                continue;
            }
            let Some((source_width, source_height)) = source_size else {
                if has_frame_update {
                    self.render.skip_rendering()?;
                }
                continue;
            };
            let (width, height) = bounded_size(source_width, source_height);
            let row_bytes = usize::try_from(width)?
                .checked_mul(4)
                .ok_or_else(|| anyhow!("thumbnail row is too large"))?;
            let stride = row_bytes.next_multiple_of(64);
            self.aligned.resize(stride * height as usize, 0);
            self.render.render_software(SoftwareRenderTarget {
                width,
                height,
                format: FORMAT_RGBA,
                stride,
                pixels: &mut self.aligned,
            })?;
            let mut tight = vec![0; row_bytes * height as usize];
            for row in 0..height as usize {
                tight[row * row_bytes..(row + 1) * row_bytes]
                    .copy_from_slice(&self.aligned[row * stride..row * stride + row_bytes]);
            }
            for pixel in tight.chunks_exact_mut(4) {
                pixel.swap(0, 2);
            }
            let image = RgbaImage::from_raw(width, height, tight)
                .ok_or_else(|| anyhow!("thumbnail renderer returned an invalid buffer"))?;
            let byte_len = row_bytes * height as usize;
            let image = Arc::new(RenderImage::new(vec![Frame::new(image)]));
            let duration = duration.or_else(|| {
                self.mpv
                    .get_property::<f64>("duration")
                    .ok()
                    .filter(|value| value.is_finite() && *value > 0.0)
            });
            return Ok(ExtractedThumbnail {
                image,
                duration,
                byte_len,
            });
        }
        if let Some(reason) = ended {
            bail!("video ended before thumbnail frame was available: {reason}");
        }
        bail!("timed out waiting for the first decoded video frame")
    }

    fn reset_source(&mut self) -> Result<()> {
        self.mpv.command("stop", &[])?;
        while let Some(event) = self.mpv.wait_event(0.0) {
            event?;
        }
        let updates = self.render.update();
        if updates & u64::from(mpv_render_update::Frame) != 0 {
            self.render.skip_rendering()?;
        }
        Ok(())
    }
}

fn retry_delay(failures: u32) -> Duration {
    RETRY_BASE_DELAY * (1 << failures.saturating_sub(1).min(MAX_RETRY_SHIFT))
}

fn video_size(mpv: &Mpv) -> Result<(u32, u32)> {
    let width = u32::try_from(mpv.get_property::<i64>("dwidth")?)?;
    let height = u32::try_from(mpv.get_property::<i64>("dheight")?)?;
    if width == 0 || height == 0 {
        bail!("video dimensions are not ready");
    }
    Ok((width, height))
}

fn bounded_size(width: u32, height: u32) -> (u32, u32) {
    let scale = (MAX_WIDTH as f64 / width.max(1) as f64)
        .min(MAX_HEIGHT as f64 / height.max(1) as f64)
        .min(1.0);
    (
        (width as f64 * scale).round().max(1.0) as u32,
        (height as f64 * scale).round().max(1.0) as u32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(value: u8) -> ThumbnailKey {
        ThumbnailKey {
            attachment_id: AttachmentId {
                timestamp_ms: value as u64,
                transfer_id: local_rpc::ids::FileTransferId(value as u64),
            },
        }
    }

    #[test]
    fn thumbnail_dimensions_preserve_aspect_ratio_within_bounds() {
        assert_eq!(bounded_size(640, 360), (640, 360));
        assert_eq!(bounded_size(3_840, 2_160), (1_360, 765));
        assert_eq!(bounded_size(1_000, 2_000), (420, 840));
    }

    #[test]
    fn extracts_first_frame_with_persistent_software_context() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("thumbnail.mkv");
        let output = std::process::Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=red:s=320x180:d=0.04",
                "-frames:v",
                "1",
                "-c:v",
                "mjpeg",
                "-y",
            ])
            .arg(&path)
            .output()
            .expect("ffmpeg is available with the required libmpv dependency");
        assert!(
            output.status.success(),
            "ffmpeg could not create the thumbnail fixture: {}",
            String::from_utf8_lossy(&output.stderr),
        );

        let mut extractor = ThumbnailExtractor::new().unwrap();
        for _ in 0..8 {
            let thumbnail = extractor.extract(&path).unwrap();
            assert_eq!(thumbnail.image.size(0).width.0, 320);
            assert_eq!(thumbnail.image.size(0).height.0, 180);
            assert_eq!(thumbnail.byte_len, 320 * 180 * 4);
        }
    }

    #[test]
    fn queued_thumbnail_work_is_bounded_and_prefers_new_requests() {
        let (wakeup, _) = async_channel::bounded(1);
        let cache = VideoThumbnailCache::new(1024, wakeup);

        for value in 0..MAX_QUEUED_JOBS as u8 {
            assert!(
                cache
                    .enqueue(ThumbnailJob {
                        key: key(value),
                        path: PathBuf::from("video.mp4"),
                        generation: 0,
                    })
                    .unwrap()
                    .is_none()
            );
        }
        let dropped = cache
            .enqueue(ThumbnailJob {
                key: key(255),
                path: PathBuf::from("latest.mp4"),
                generation: 0,
            })
            .unwrap();

        assert_eq!(dropped, Some(key(0)));
        let jobs = cache.jobs.0.lock().unwrap();
        assert_eq!(jobs.jobs.len(), MAX_QUEUED_JOBS);
        assert_eq!(jobs.jobs.back().map(|job| job.key), Some(key(255)));
    }

    #[test]
    fn failed_thumbnail_metadata_is_bounded() {
        let (wakeup, _) = async_channel::bounded(1);
        let mut cache = VideoThumbnailCache::new(1024, wakeup);
        for value in 0..(MAX_CACHE_ENTRIES + 20) {
            cache.entries.insert(
                ThumbnailKey {
                    attachment_id: AttachmentId {
                        timestamp_ms: value as u64,
                        transfer_id: local_rpc::ids::FileTransferId(value as u64),
                    },
                },
                CacheEntry {
                    state: CacheState::Failed {
                        error: "invalid video".into(),
                        retry_at: Instant::now() + Duration::from_secs(60),
                    },
                    touched: value as u64,
                    failures: 1,
                },
            );
        }

        cache.evict();

        assert_eq!(cache.entries.len(), MAX_CACHE_ENTRIES);
    }
}
