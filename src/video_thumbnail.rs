use std::{
    collections::{HashMap, VecDeque},
    ffi::{CStr, c_void},
    panic::{AssertUnwindSafe, catch_unwind},
    slice,
    sync::{Arc, Condvar, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

use crate::attachment_source::{
    AttachmentCursor, AttachmentSeekMode, AttachmentSource, AttachmentSourceKey,
    RegisteredAttachmentSource,
};
use anyhow::{Context as _, Result, anyhow, bail};
use async_channel::Sender as AsyncSender;
use gpui::RenderImage;
use image::{Frame, RgbaImage};

const MAX_WIDTH: u32 = 1_360;
const MAX_HEIGHT: u32 = 840;
const MAX_QUEUED_JOBS: usize = 16;
const MAX_CACHE_ENTRIES: usize = 256;
const RETRY_BASE_DELAY: Duration = Duration::from_secs(2);
const MAX_RETRY_SHIFT: u32 = 6;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ThumbnailKey {
    pub source_key: AttachmentSourceKey,
}

#[derive(Clone, Default)]
pub(crate) struct ThumbnailView {
    pub image: Option<Arc<RenderImage>>,
    pub duration: Option<f64>,
    pub failed: bool,
    pub pending: bool,
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
    source: RegisteredAttachmentSource,
    generation: u64,
}

struct ThumbnailResult {
    key: ThumbnailKey,
    generation: u64,
    result: Result<ExtractedThumbnail, String>,
    source_failed: bool,
}

struct ExtractedThumbnail {
    image: Arc<RenderImage>,
    duration: Option<f64>,
    byte_len: usize,
}

#[derive(Default)]
struct ThumbnailWorkQueue {
    jobs: VecDeque<ThumbnailJob>,
    warm: bool,
    closed: bool,
}

type SharedWorkQueue = Arc<(Mutex<ThumbnailWorkQueue>, Condvar)>;

/// A bounded timeline thumbnail cache backed by direct FFmpeg first-frame
/// decoding. Extraction is serial so thumbnail work remains bounded and never
/// contends for the application's playback render device.
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
    finished_sources: Vec<AttachmentSourceKey>,
    transport_failures: Vec<(AttachmentSourceKey, String)>,
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
            finished_sources: Vec::new(),
            transport_failures: Vec::new(),
        }
    }

    pub(crate) fn clear(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.entries.clear();
        self.total_bytes = 0;
        self.jobs.0.lock().unwrap().jobs.clear();
        self.finished_sources.clear();
        self.transport_failures.clear();
        while self.results.try_recv().is_ok() {}
    }

    pub(crate) fn request(
        &mut self,
        key: ThumbnailKey,
        source: RegisteredAttachmentSource,
    ) -> ThumbnailView {
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
            source,
            generation: self.generation,
        }) {
            Ok(dropped) => {
                if let Some(dropped) = dropped {
                    self.entries.remove(&dropped);
                    self.finished_sources.push(dropped.source_key);
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

    pub(crate) fn view(&mut self, key: ThumbnailKey) -> ThumbnailView {
        self.drain_results();
        self.clock = self.clock.wrapping_add(1);
        let Some(entry) = self.entries.get_mut(&key) else {
            return ThumbnailView::default();
        };
        entry.touched = self.clock;
        view_for_entry(entry)
    }

    pub(crate) fn drain_results(&mut self) -> bool {
        let mut changed = false;
        while let Ok(result) = self.results.try_recv() {
            if result.generation != self.generation {
                continue;
            }
            self.finished_sources.push(result.key.source_key);
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
                    if result.source_failed {
                        self.transport_failures
                            .push((result.key.source_key, error.clone()));
                    }
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

    pub(crate) fn warm(&mut self) {
        if let Err(error) = self.start_worker() {
            log::warn!("video thumbnail warmup failed: {error}");
            return;
        }
        let (jobs, ready) = &*self.jobs;
        if let Ok(mut jobs) = jobs.lock() {
            jobs.warm = true;
            ready.notify_one();
        }
    }

    pub(crate) fn take_finished_sources(&mut self) -> Vec<AttachmentSourceKey> {
        std::mem::take(&mut self.finished_sources)
    }

    pub(crate) fn take_transport_failures(&mut self) -> Vec<(AttachmentSourceKey, String)> {
        std::mem::take(&mut self.transport_failures)
    }

    fn start_worker(&mut self) -> Result<(), String> {
        if self.worker_started {
            return Ok(());
        }
        let jobs = self.jobs.clone();
        let results = self.worker_results.clone();
        let wakeup = self.wakeup.clone();
        thread::Builder::new()
            .name("video-thumbnail".into())
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
            pending: false,
        },
        CacheState::Failed { error, .. } => {
            let _ = error;
            ThumbnailView {
                failed: true,
                pending: false,
                ..ThumbnailView::default()
            }
        }
        CacheState::Pending => ThumbnailView {
            pending: true,
            ..ThumbnailView::default()
        },
    }
}

fn thumbnail_worker(
    jobs: SharedWorkQueue,
    results: mpsc::Sender<ThumbnailResult>,
    wakeup: AsyncSender<()>,
) {
    log::info!("lazy video thumbnail worker started");
    let mut extractor = ThumbnailExtractor;
    loop {
        let job = {
            let (jobs, ready) = &*jobs;
            let mut jobs = match jobs.lock() {
                Ok(jobs) => jobs,
                Err(_) => break,
            };
            while jobs.jobs.is_empty() && !jobs.warm && !jobs.closed {
                jobs = match ready.wait(jobs) {
                    Ok(jobs) => jobs,
                    Err(_) => return,
                };
            }
            if jobs.closed {
                break;
            }
            if jobs.jobs.is_empty() {
                jobs.warm = false;
                None
            } else {
                Some(jobs.jobs.pop_back().expect("non-empty thumbnail queue"))
            }
        };
        let Some(job) = job else {
            continue;
        };
        let started_at = Instant::now();
        log::info!(
            "video thumbnail extraction started key={:?} backend={} byte_len={}",
            job.key.source_key,
            if job.source.source().is_remote() {
                "remote"
            } else {
                "direct"
            },
            job.source.source().byte_len(),
        );
        let result = extractor
            .extract(job.source.source().clone())
            .map_err(|error| format!("{error:#}"));
        log::info!(
            "video thumbnail extraction completed key={:?} success={} source_failed={} elapsed_ms={:.3}",
            job.key.source_key,
            result.is_ok(),
            job.source.source().has_failed(),
            started_at.elapsed().as_secs_f64() * 1_000.0,
        );
        if results
            .send(ThumbnailResult {
                key: job.key,
                generation: job.generation,
                result,
                source_failed: job.source.source().has_failed(),
            })
            .is_err()
        {
            break;
        }
        let _ = wakeup.try_send(());
    }
    log::info!("video thumbnail worker stopped");
}

struct ThumbnailExtractor;

struct ThumbnailIo {
    cursor: AttachmentCursor,
    error: Option<String>,
}

impl ThumbnailExtractor {
    fn extract(&mut self, source: Arc<AttachmentSource>) -> Result<ExtractedThumbnail> {
        let maximum_bytes = usize::try_from(MAX_WIDTH)?
            .checked_mul(usize::try_from(MAX_HEIGHT)?)
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or_else(|| anyhow!("thumbnail output size overflow"))?;
        let mut bgra = vec![0u8; maximum_bytes];
        let mut thumbnail = libmpv2_sys::ChattFfmpegThumbnail::default();
        let mut error = [0i8; 512];
        let byte_len = i64::try_from(source.byte_len()).context("thumbnail source is too large")?;
        let mut io = ThumbnailIo {
            cursor: AttachmentCursor::new(Some(source)),
            error: None,
        };
        let status = unsafe {
            libmpv2_sys::chatt_ffmpeg_extract_first_frame(
                (&mut io as *mut ThumbnailIo).cast(),
                byte_len,
                thumbnail_read,
                thumbnail_seek,
                i32::try_from(MAX_WIDTH)?,
                i32::try_from(MAX_HEIGHT)?,
                bgra.as_mut_ptr(),
                bgra.len(),
                &mut thumbnail,
                error.as_mut_ptr(),
                error.len(),
            )
        };
        if let Some(error) = io.error {
            bail!("{error}");
        }
        if status != 0 {
            let error = unsafe { CStr::from_ptr(error.as_ptr()) }
                .to_string_lossy()
                .into_owned();
            bail!(
                "{}",
                if error.is_empty() {
                    "FFmpeg thumbnail extraction failed"
                } else {
                    &error
                }
            );
        }

        let width = u32::try_from(thumbnail.width)
            .context("thumbnail decoder returned a negative width")?;
        let height = u32::try_from(thumbnail.height)
            .context("thumbnail decoder returned a negative height")?;
        if width == 0 || height == 0 || width > MAX_WIDTH || height > MAX_HEIGHT {
            bail!("thumbnail decoder returned invalid dimensions {width}x{height}");
        }
        let byte_len = usize::try_from(width)?
            .checked_mul(usize::try_from(height)?)
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or_else(|| anyhow!("thumbnail image size overflow"))?;
        bgra.truncate(byte_len);
        // GPUI's RenderImage stores its upload bytes in image::RgbaImage but
        // deliberately interprets them as BGRA.
        let image = RgbaImage::from_raw(width, height, bgra)
            .ok_or_else(|| anyhow!("thumbnail decoder returned an invalid buffer"))?;
        let duration = (thumbnail.duration.is_finite() && thumbnail.duration > 0.0)
            .then_some(thumbnail.duration);
        Ok(ExtractedThumbnail {
            image: Arc::new(RenderImage::new(vec![Frame::new(image)])),
            duration,
            byte_len,
        })
    }
}

unsafe extern "C" fn thumbnail_read(opaque: *mut c_void, buffer: *mut u8, length: i32) -> i32 {
    let result = catch_unwind(AssertUnwindSafe(|| {
        if opaque.is_null() || buffer.is_null() || length <= 0 {
            return Err("FFmpeg issued an invalid thumbnail read".to_string());
        }
        let io = unsafe { &mut *opaque.cast::<ThumbnailIo>() };
        let output = unsafe { slice::from_raw_parts_mut(buffer, length as usize) };
        io.cursor
            .read(output)
            .and_then(|read| i32::try_from(read).context("thumbnail read is too large"))
            .map_err(|error| format!("{error:#}"))
    }));
    match result {
        Ok(Ok(read)) => read,
        Ok(Err(error)) => {
            if !opaque.is_null() {
                unsafe { &mut *opaque.cast::<ThumbnailIo>() }.error = Some(error);
            }
            -1
        }
        Err(_) => {
            if !opaque.is_null() {
                unsafe { &mut *opaque.cast::<ThumbnailIo>() }.error =
                    Some("thumbnail read callback panicked".into());
            }
            -1
        }
    }
}

unsafe extern "C" fn thumbnail_seek(opaque: *mut c_void, offset: i64, whence: i32) -> i64 {
    let result = catch_unwind(AssertUnwindSafe(|| {
        if opaque.is_null() {
            return Err("FFmpeg issued a thumbnail seek without a source".to_string());
        }
        let mode = match whence {
            libc::SEEK_SET => AttachmentSeekMode::Set,
            libc::SEEK_CUR => AttachmentSeekMode::Current,
            libc::SEEK_END => AttachmentSeekMode::End,
            _ => {
                return Err(format!(
                    "FFmpeg issued invalid thumbnail seek mode {whence}"
                ));
            }
        };
        unsafe { &mut *opaque.cast::<ThumbnailIo>() }
            .cursor
            .seek(offset, mode)
            .and_then(|position| {
                i64::try_from(position).context("thumbnail seek position is too large")
            })
            .map_err(|error| format!("{error:#}"))
    }));
    match result {
        Ok(Ok(position)) => position,
        Ok(Err(error)) => {
            if !opaque.is_null() {
                unsafe { &mut *opaque.cast::<ThumbnailIo>() }.error = Some(error);
            }
            -1
        }
        Err(_) => {
            if !opaque.is_null() {
                unsafe { &mut *opaque.cast::<ThumbnailIo>() }.error =
                    Some("thumbnail seek callback panicked".into());
            }
            -1
        }
    }
}

fn retry_delay(failures: u32) -> Duration {
    RETRY_BASE_DELAY * (1 << failures.saturating_sub(1).min(MAX_RETRY_SHIFT))
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
    use crate::attachment_source::AttachmentSourceRegistry;
    use local_rpc::model::AttachmentId;
    use std::fs::File;

    fn key(value: u8) -> ThumbnailKey {
        ThumbnailKey {
            source_key: AttachmentSourceKey {
                namespace: 1,
                room_id: local_rpc::ids::RoomId(1),
                attachment_id: AttachmentId {
                    timestamp_ms: value as u64,
                    transfer_id: local_rpc::ids::FileTransferId(value as u64),
                },
            },
        }
    }

    fn registered(
        registry: &AttachmentSourceRegistry,
        key: ThumbnailKey,
        file: File,
    ) -> RegisteredAttachmentSource {
        let byte_len = file.metadata().unwrap().len();
        registry.register(crate::attachment_source::AttachmentSource::direct(
            key.source_key,
            file,
            byte_len,
        ))
    }

    #[test]
    fn thumbnail_dimensions_preserve_aspect_ratio_within_bounds() {
        assert_eq!(bounded_size(640, 360), (640, 360));
        assert_eq!(bounded_size(3_840, 2_160), (1_360, 765));
        assert_eq!(bounded_size(1_000, 2_000), (420, 840));
    }

    #[test]
    fn extracts_first_frame_through_attachment_source() {
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

        let registry = AttachmentSourceRegistry::new(1);
        let source = registered(&registry, key(1), File::open(&path).unwrap());
        let mut extractor = ThumbnailExtractor;
        for _ in 0..8 {
            let thumbnail = extractor.extract(source.source().clone()).unwrap();
            assert_eq!(thumbnail.image.size(0).width.0, 320);
            assert_eq!(thumbnail.image.size(0).height.0, 180);
            assert_eq!(thumbnail.byte_len, 320 * 180 * 4);
            let pixel = &thumbnail.image.as_bytes(0).unwrap()[..4];
            assert!(
                pixel[0] < 32 && pixel[1] < 32 && pixel[2] > 224 && pixel[3] == 255,
                "GPUI thumbnail bytes should contain opaque red in BGRA order: {pixel:?}",
            );
        }
    }

    #[test]
    fn queued_thumbnail_work_is_bounded_and_prefers_new_requests() {
        let (wakeup, _) = async_channel::bounded(1);
        let registry = AttachmentSourceRegistry::new(1);
        let file = tempfile::tempfile().unwrap();
        file.set_len(1).unwrap();
        let source = registered(&registry, key(1), file);
        let cache = VideoThumbnailCache::new(1024, wakeup);

        for value in 0..MAX_QUEUED_JOBS as u8 {
            assert!(
                cache
                    .enqueue(ThumbnailJob {
                        key: key(value),
                        source: source.clone(),
                        generation: 0,
                    })
                    .unwrap()
                    .is_none()
            );
        }
        let dropped = cache
            .enqueue(ThumbnailJob {
                key: key(255),
                source,
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
                    source_key: AttachmentSourceKey {
                        namespace: 1,
                        room_id: local_rpc::ids::RoomId(1),
                        attachment_id: AttachmentId {
                            timestamp_ms: value as u64,
                            transfer_id: local_rpc::ids::FileTransferId(value as u64),
                        },
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
