use std::{
    collections::{HashMap, VecDeque},
    ffi::{CStr, c_void},
    panic::{AssertUnwindSafe, catch_unwind},
    slice,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
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
const JOB_BYTE_BUDGET: u64 = 32 * 1024 * 1024;
const JOB_DEADLINE: Duration = Duration::from_secs(10);
const PROBE_BYTES: i64 = 4 * 1024 * 1024;
const MAX_ANALYZE_DURATION_US: i64 = 3_000_000;
const MAX_DECODE_PIXELS: i64 = 7680 * 4320 * 2;

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
}

/// Work handoff to the extraction thread. `closed` and `generation` live outside
/// the mutex so an in-flight job can be aborted, and the worker's exit can be
/// observed, even when the lock is poisoned.
#[derive(Default)]
struct ThumbnailQueue {
    state: Mutex<ThumbnailWorkQueue>,
    ready: Condvar,
    closed: AtomicBool,
    generation: AtomicU64,
}

impl ThumbnailQueue {
    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Release);
        if let Ok(mut state) = self.state.lock() {
            state.jobs.clear();
        }
        self.ready.notify_all();
    }
}

/// A bounded timeline thumbnail cache backed by direct FFmpeg first-frame
/// decoding. Extraction is serial so thumbnail work remains bounded and never
/// contends for the application's playback render device.
pub(crate) struct VideoThumbnailCache {
    entries: HashMap<ThumbnailKey, CacheEntry>,
    total_bytes: usize,
    budget_bytes: usize,
    clock: u64,
    generation: u64,
    jobs: Arc<ThumbnailQueue>,
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
            jobs: Arc::new(ThumbnailQueue::default()),
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
        self.jobs
            .generation
            .store(self.generation, Ordering::Release);
        self.entries.clear();
        self.total_bytes = 0;
        if let Ok(mut state) = self.jobs.state.lock() {
            state.jobs.clear();
        }
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
                    self.remove_entry(dropped);
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
            if !self.entries.contains_key(&result.key) {
                continue;
            }
            changed = true;
            match result.result {
                Ok(thumbnail) => {
                    let byte_len = thumbnail.byte_len;
                    self.set_state(
                        result.key,
                        CacheState::Ready {
                            image: thumbnail.image,
                            duration: thumbnail.duration,
                            byte_len,
                        },
                    );
                    self.total_bytes = self.total_bytes.saturating_add(byte_len);
                }
                Err(error) => {
                    if result.source_failed {
                        self.transport_failures
                            .push((result.key.source_key, error.clone()));
                    }
                    self.record_failure(result.key, error);
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
            kvlog::warn!("video thumbnail warmup failed", err = %error);
            return;
        }
        if let Ok(mut state) = self.jobs.state.lock() {
            state.warm = true;
            self.jobs.ready.notify_one();
        }
    }

    pub(crate) fn take_finished_sources(&mut self) -> Vec<AttachmentSourceKey> {
        std::mem::take(&mut self.finished_sources)
    }

    pub(crate) fn take_transport_failures(&mut self) -> Vec<(AttachmentSourceKey, String)> {
        std::mem::take(&mut self.transport_failures)
    }

    fn start_worker(&mut self) -> Result<(), String> {
        if self.jobs.is_closed() {
            return Err("thumbnail worker stopped".into());
        }
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
        if self.jobs.is_closed() {
            return Err("thumbnail worker stopped".into());
        }
        let mut state = self
            .jobs
            .state
            .lock()
            .map_err(|_| "thumbnail work queue lock poisoned".to_string())?;
        let dropped = (state.jobs.len() >= MAX_QUEUED_JOBS)
            .then(|| state.jobs.pop_front().map(|job| job.key))
            .flatten();
        state.jobs.push_back(job);
        self.jobs.ready.notify_one();
        Ok(dropped)
    }

    fn record_failure(&mut self, key: ThumbnailKey, error: String) {
        kvlog::warn!("video thumbnail extraction failed", err = %error);
        let Some(entry) = self.entries.get_mut(&key) else {
            return;
        };
        entry.failures = entry.failures.saturating_add(1);
        let failures = entry.failures;
        self.set_state(
            key,
            CacheState::Failed {
                error,
                retry_at: Instant::now() + retry_delay(failures),
            },
        );
    }

    fn set_state(&mut self, key: ThumbnailKey, state: CacheState) {
        let Some(entry) = self.entries.get_mut(&key) else {
            return;
        };
        if let CacheState::Ready { byte_len, .. } = entry.state {
            self.total_bytes = self.total_bytes.saturating_sub(byte_len);
        }
        entry.state = state;
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
        self.jobs.close();
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

/// Marks the queue closed on every worker exit path, and fails the job that was
/// in flight so its cache entry and attachment source pin are released instead
/// of leaking as a permanently pending request.
struct WorkerExit {
    queue: Arc<ThumbnailQueue>,
    results: mpsc::Sender<ThumbnailResult>,
    wakeup: AsyncSender<()>,
    in_flight: Option<(ThumbnailKey, u64)>,
}

impl Drop for WorkerExit {
    fn drop(&mut self) {
        self.queue.close();
        let Some((key, generation)) = self.in_flight.take() else {
            return;
        };
        let sent = self.results.send(ThumbnailResult {
            key,
            generation,
            result: Err("thumbnail worker stopped before finishing".to_string()),
            source_failed: false,
        });
        if sent.is_ok() {
            let _ = self.wakeup.try_send(());
        }
    }
}

fn thumbnail_worker(
    queue: Arc<ThumbnailQueue>,
    results: mpsc::Sender<ThumbnailResult>,
    wakeup: AsyncSender<()>,
) {
    kvlog::info!("video thumbnail worker started");
    let mut exit = WorkerExit {
        queue: queue.clone(),
        results: results.clone(),
        wakeup: wakeup.clone(),
        in_flight: None,
    };
    let mut extractor = ThumbnailExtractor::new();
    'jobs: loop {
        let job = {
            let mut state = match queue.state.lock() {
                Ok(state) => state,
                Err(_) => break 'jobs,
            };
            loop {
                if queue.is_closed() {
                    break 'jobs;
                }
                if let Some(job) = state.jobs.pop_back() {
                    break Some(job);
                }
                if state.warm {
                    state.warm = false;
                    break None;
                }
                state = match queue.ready.wait(state) {
                    Ok(state) => state,
                    Err(_) => break 'jobs,
                };
            }
        };
        let Some(job) = job else {
            continue;
        };
        exit.in_flight = Some((job.key, job.generation));
        let _started_at = Instant::now();
        #[cfg(feature = "diagnostic-logs")]
        if crate::logger::media_logging_enabled() {
            let key = job.key.source_key;
            kvlog::info!(
                "video thumbnail extraction started",
                group = "media",
                namespace = key.namespace,
                room_id = key.room_id,
                attachment_timestamp_ms = key.attachment_id.timestamp_ms,
                attachment_transfer_id = key.attachment_id.transfer_id,
                backend = if job.source.source().is_remote() {
                    "remote"
                } else {
                    "direct"
                },
                size = job.source.source().byte_len()
            );
        }
        let result = catch_unwind(AssertUnwindSafe(|| {
            extractor.extract(job.source.source().clone(), queue.clone(), job.generation)
        }))
        .unwrap_or_else(|_| Err(anyhow!("thumbnail extraction panicked")))
        .map_err(|error| format!("{error:#}"));
        #[cfg(feature = "diagnostic-logs")]
        if crate::logger::media_logging_enabled() {
            let key = job.key.source_key;
            kvlog::info!(
                "video thumbnail extraction completed",
                group = "media",
                namespace = key.namespace,
                room_id = key.room_id,
                attachment_timestamp_ms = key.attachment_id.timestamp_ms,
                attachment_transfer_id = key.attachment_id.transfer_id,
                success = result.is_ok(),
                source_failed = job.source.source().has_failed(),
                elapsed_ms = _started_at.elapsed().as_secs_f64() * 1_000.0
            );
        }
        exit.in_flight = None;
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
    kvlog::info!("video thumbnail worker stopped");
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Rotation {
    None,
    Quarter,
    Half,
    ThreeQuarter,
}

impl Rotation {
    fn from_degrees(degrees: i32) -> Result<Self> {
        match degrees {
            0 => Ok(Self::None),
            90 => Ok(Self::Quarter),
            180 => Ok(Self::Half),
            270 => Ok(Self::ThreeQuarter),
            _ => bail!("thumbnail decoder returned an unsupported rotation of {degrees} degrees"),
        }
    }

    fn apply_to_size(self, width: u32, height: u32) -> (u32, u32) {
        match self {
            Self::Quarter | Self::ThreeQuarter => (height, width),
            Self::None | Self::Half => (width, height),
        }
    }
}

/// Copies `source` into a freshly allocated buffer, applying a clockwise
/// rotation. `width` and `height` describe `source`; the result holds the
/// rotated image, so its dimensions are swapped for quarter turns.
///
/// The output owns exactly as many bytes as it holds, which is what keeps a
/// cached thumbnail's allocation equal to the byte length the cache budget
/// accounts for.
fn rotate_pixels(source: &[u8], width: usize, height: usize, rotation: Rotation) -> Vec<u8> {
    let mut destination = vec![0u8; source.len()];
    match rotation {
        Rotation::None => destination.copy_from_slice(source),
        Rotation::Quarter => {
            for y in 0..height {
                for x in 0..width {
                    let from = (y * width + x) * 4;
                    let to = (x * height + (height - 1 - y)) * 4;
                    destination[to..to + 4].copy_from_slice(&source[from..from + 4]);
                }
            }
        }
        Rotation::Half => {
            for y in 0..height {
                for x in 0..width {
                    let from = (y * width + x) * 4;
                    let to = ((height - 1 - y) * width + (width - 1 - x)) * 4;
                    destination[to..to + 4].copy_from_slice(&source[from..from + 4]);
                }
            }
        }
        Rotation::ThreeQuarter => {
            for y in 0..height {
                for x in 0..width {
                    let from = (y * width + x) * 4;
                    let to = ((width - 1 - x) * height + y) * 4;
                    destination[to..to + 4].copy_from_slice(&source[from..from + 4]);
                }
            }
        }
    }
    destination
}

struct ThumbnailExtractor {
    scratch: Vec<u8>,
}

struct ThumbnailIo {
    cursor: AttachmentCursor,
    error: Option<String>,
    queue: Arc<ThumbnailQueue>,
    generation: u64,
    deadline: Instant,
    budget: u64,
}

impl ThumbnailIo {
    /// Custom IO means every demuxed byte passes through the read callback, so
    /// this is the one place that reliably bounds and cancels an extraction —
    /// FFmpeg consults `interrupt_callback` only inside
    /// `avformat_find_stream_info`.
    fn aborted(&self) -> Option<&'static str> {
        if self.queue.is_closed() {
            return Some("thumbnail worker stopped");
        }
        if self.queue.generation.load(Ordering::Acquire) != self.generation {
            return Some("thumbnail request was superseded");
        }
        if self.budget == 0 {
            return Some("thumbnail extraction exceeded its read budget");
        }
        if Instant::now() > self.deadline {
            return Some("thumbnail extraction exceeded its time budget");
        }
        None
    }
}

impl ThumbnailExtractor {
    fn new() -> Self {
        Self {
            scratch: Vec::new(),
        }
    }

    fn extract(
        &mut self,
        source: Arc<AttachmentSource>,
        queue: Arc<ThumbnailQueue>,
        generation: u64,
    ) -> Result<ExtractedThumbnail> {
        let capacity = usize::try_from(MAX_WIDTH)?
            .checked_mul(usize::try_from(MAX_HEIGHT)?)
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or_else(|| anyhow!("thumbnail output size overflow"))?;
        self.scratch.resize(capacity, 0);
        let mut thumbnail = libmpv2_sys::ChattFfmpegThumbnail::default();
        let mut error = [0i8; 512];
        let byte_len = i64::try_from(source.byte_len()).context("thumbnail source is too large")?;
        let mut io = ThumbnailIo {
            cursor: AttachmentCursor::new(Some(source)),
            error: None,
            queue,
            generation,
            deadline: Instant::now() + JOB_DEADLINE,
            budget: JOB_BYTE_BUDGET,
        };
        let status = unsafe {
            libmpv2_sys::chatt_ffmpeg_extract_first_frame(
                (&mut io as *mut ThumbnailIo).cast(),
                byte_len,
                thumbnail_read,
                thumbnail_seek,
                thumbnail_interrupt,
                i32::try_from(MAX_WIDTH)?,
                i32::try_from(MAX_HEIGHT)?,
                MAX_DECODE_PIXELS,
                PROBE_BYTES,
                MAX_ANALYZE_DURATION_US,
                self.scratch.as_mut_ptr(),
                self.scratch.len(),
                &mut thumbnail,
                error.as_mut_ptr(),
                error.len(),
            )
        };
        if status != 0 {
            let error = io.error.take().unwrap_or_else(|| {
                unsafe { CStr::from_ptr(error.as_ptr()) }
                    .to_string_lossy()
                    .into_owned()
            });
            bail!(
                "{}",
                if error.is_empty() {
                    "FFmpeg thumbnail extraction failed"
                } else {
                    &error
                }
            );
        }

        let scaled_width = u32::try_from(thumbnail.width)
            .context("thumbnail decoder returned a negative width")?;
        let scaled_height = u32::try_from(thumbnail.height)
            .context("thumbnail decoder returned a negative height")?;
        let rotation = Rotation::from_degrees(thumbnail.rotate)?;
        let (width, height) = rotation.apply_to_size(scaled_width, scaled_height);
        if width == 0 || height == 0 || width > MAX_WIDTH || height > MAX_HEIGHT {
            bail!("thumbnail decoder returned invalid dimensions {width}x{height}");
        }
        let byte_len = usize::try_from(width)?
            .checked_mul(usize::try_from(height)?)
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or_else(|| anyhow!("thumbnail image size overflow"))?;
        if byte_len > self.scratch.len() {
            bail!("thumbnail decoder returned more pixels than it was given room for");
        }

        let pixels = rotate_pixels(
            &self.scratch[..byte_len],
            scaled_width as usize,
            scaled_height as usize,
            rotation,
        );
        // GPUI's RenderImage stores its upload bytes in image::RgbaImage but
        // deliberately interprets them as BGRA.
        let image = RgbaImage::from_raw(width, height, pixels)
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
        if let Some(reason) = io.aborted() {
            return Err(reason.to_string());
        }
        let output = unsafe { slice::from_raw_parts_mut(buffer, length as usize) };
        let read = io
            .cursor
            .read(output)
            .and_then(|read| i32::try_from(read).context("thumbnail read is too large"))
            .map_err(|error| format!("{error:#}"))?;
        io.budget = io.budget.saturating_sub(read as u64);
        Ok(read)
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

unsafe extern "C" fn thumbnail_interrupt(opaque: *mut c_void) -> i32 {
    let aborted = catch_unwind(AssertUnwindSafe(|| {
        if opaque.is_null() {
            return true;
        }
        let io = unsafe { &mut *opaque.cast::<ThumbnailIo>() };
        let Some(reason) = io.aborted() else {
            return false;
        };
        if io.error.is_none() {
            io.error = Some(reason.to_string());
        }
        true
    }));
    i32::from(aborted.unwrap_or(true))
}

fn retry_delay(failures: u32) -> Duration {
    RETRY_BASE_DELAY * (1 << failures.saturating_sub(1).min(MAX_RETRY_SHIFT))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attachment_source::AttachmentSourceRegistry;
    use local_rpc::model::AttachmentId;
    use std::{fs::File, io::Write};

    const RED_MJPEG_MKV: &[u8] = include_bytes!("../tests/media/thumbnail.mkv");
    const ROTATED_MP4: &[u8] = include_bytes!("../tests/media/thumbnail-rotated.mp4");

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

    fn fixture(bytes: &[u8]) -> File {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(bytes).unwrap();
        file.flush().unwrap();
        file
    }

    fn extract(source: &RegisteredAttachmentSource) -> Result<ExtractedThumbnail> {
        ThumbnailExtractor::new().extract(
            source.source().clone(),
            Arc::new(ThumbnailQueue::default()),
            0,
        )
    }

    #[test]
    fn extracts_first_frame_through_attachment_source() {
        let registry = AttachmentSourceRegistry::new(1);
        let source = registered(&registry, key(1), fixture(RED_MJPEG_MKV));
        let mut extractor = ThumbnailExtractor::new();
        for _ in 0..8 {
            let thumbnail = extractor
                .extract(
                    source.source().clone(),
                    Arc::new(ThumbnailQueue::default()),
                    0,
                )
                .unwrap();
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
    fn extracts_a_thumbnail_through_a_remote_attachment_source() {
        use local_rpc::attachment_stream::{ResponseStatus, read_request, write_response};
        use std::os::unix::net::UnixStream;

        let bytes = RED_MJPEG_MKV.to_vec();
        let byte_len = bytes.len() as u64;
        let (frontend, mut daemon) = UnixStream::pair().unwrap();
        let daemon = thread::spawn(move || {
            while let Ok(Some(request)) = read_request(&mut daemon) {
                let start = request.offset as usize;
                let end = start
                    .saturating_add(request.length as usize)
                    .min(bytes.len());
                let payload = if start >= bytes.len() {
                    &[][..]
                } else {
                    &bytes[start..end]
                };
                if write_response(&mut daemon, ResponseStatus::Data, payload).is_err() {
                    break;
                }
            }
        });

        let registry = AttachmentSourceRegistry::new(1);
        let source = registry.register(crate::attachment_source::AttachmentSource::remote(
            key(5).source_key,
            frontend,
            byte_len,
            local_rpc::MAX_ATTACHMENT_READ_BYTES,
        ));
        let thumbnail = extract(&source).unwrap();

        assert_eq!(thumbnail.image.size(0).width.0, 320);
        assert_eq!(thumbnail.image.size(0).height.0, 180);
        drop(source);
        daemon.join().unwrap();
    }

    #[test]
    fn cached_thumbnail_allocation_matches_the_accounted_byte_length() {
        let scratch = vec![7u8; MAX_WIDTH as usize * MAX_HEIGHT as usize * 4];
        let byte_len = 320 * 180 * 4;

        let pixels = rotate_pixels(&scratch[..byte_len], 320, 180, Rotation::None);

        assert_eq!(pixels.len(), byte_len);
        assert_eq!(
            pixels.capacity(),
            byte_len,
            "a cached thumbnail must own only its own pixels, not the shared scratch allocation",
        );
    }

    /// The fixture is a 320x180 red frame with a green block in its top-left
    /// corner, tagged with a display matrix that mpv reports as a 90 degree
    /// counter-clockwise turn. Applying that turn moves the green block to the
    /// bottom-left of the 180x320 result; turning the wrong way would put it in
    /// the top-right, which is the failure this guards.
    #[test]
    fn rotated_video_thumbnail_is_upright() {
        let registry = AttachmentSourceRegistry::new(1);
        let source = registered(&registry, key(2), fixture(ROTATED_MP4));
        let thumbnail = extract(&source).unwrap();

        assert_eq!(
            (
                thumbnail.image.size(0).width.0,
                thumbnail.image.size(0).height.0
            ),
            (180, 320),
            "a quarter-turn display matrix must swap the thumbnail's dimensions",
        );
        assert_eq!(thumbnail.byte_len, 180 * 320 * 4);

        let pixels = thumbnail.image.as_bytes(0).unwrap();
        let pixel = |x: usize, y: usize| {
            let offset = (y * 180 + x) * 4;
            &pixels[offset..offset + 4]
        };
        let bottom_left = pixel(20, 300);
        assert!(
            bottom_left[0] < 64 && bottom_left[1] > 128 && bottom_left[2] < 64,
            "the green corner must land bottom-left after the turn: {bottom_left:?}",
        );
        let top_left = pixel(20, 20);
        assert!(
            top_left[0] < 64 && top_left[1] < 64 && top_left[2] > 128,
            "the rest of the frame must stay red: {top_left:?}",
        );
    }

    #[test]
    fn read_callback_stops_once_the_byte_budget_is_spent() {
        let registry = AttachmentSourceRegistry::new(1);
        let source = registered(&registry, key(3), fixture(RED_MJPEG_MKV));
        let mut io = ThumbnailIo {
            cursor: AttachmentCursor::new(Some(source.source().clone())),
            error: None,
            queue: Arc::new(ThumbnailQueue::default()),
            generation: 0,
            deadline: Instant::now() + JOB_DEADLINE,
            budget: 16,
        };
        let mut buffer = [0u8; 16];

        let read = unsafe {
            thumbnail_read(
                (&mut io as *mut ThumbnailIo).cast(),
                buffer.as_mut_ptr(),
                buffer.len() as i32,
            )
        };
        assert_eq!(read, 16);
        assert_eq!(io.budget, 0);

        let read = unsafe {
            thumbnail_read(
                (&mut io as *mut ThumbnailIo).cast(),
                buffer.as_mut_ptr(),
                buffer.len() as i32,
            )
        };
        assert_eq!(read, -1);
        assert_eq!(
            io.error.as_deref(),
            Some("thumbnail extraction exceeded its read budget")
        );
    }

    #[test]
    fn undecodable_source_fails_instead_of_scanning_forever() {
        let registry = AttachmentSourceRegistry::new(1);
        let mut file = tempfile::tempfile().unwrap();
        let noise = (0..64 * 1024)
            .map(|value| (value % 251) as u8)
            .collect::<Vec<_>>();
        for _ in 0..16 {
            file.write_all(&noise).unwrap();
        }
        file.flush().unwrap();
        let source = registered(&registry, key(4), file);

        let Err(error) = extract(&source) else {
            panic!("a source with no decodable video must not produce a thumbnail");
        };
        assert!(!format!("{error:#}").is_empty());
    }

    #[test]
    fn aborts_when_the_request_generation_changes() {
        let queue = Arc::new(ThumbnailQueue::default());
        let io = ThumbnailIo {
            cursor: AttachmentCursor::new(None),
            error: None,
            queue: queue.clone(),
            generation: 0,
            deadline: Instant::now() + JOB_DEADLINE,
            budget: JOB_BYTE_BUDGET,
        };
        assert_eq!(io.aborted(), None);
        queue.generation.store(1, Ordering::Release);
        assert_eq!(io.aborted(), Some("thumbnail request was superseded"));
    }

    #[test]
    fn aborts_when_the_queue_closes_or_budgets_run_out() {
        let queue = Arc::new(ThumbnailQueue::default());
        let mut io = ThumbnailIo {
            cursor: AttachmentCursor::new(None),
            error: None,
            queue: queue.clone(),
            generation: 0,
            deadline: Instant::now() + JOB_DEADLINE,
            budget: 0,
        };
        assert_eq!(
            io.aborted(),
            Some("thumbnail extraction exceeded its read budget")
        );

        io.budget = JOB_BYTE_BUDGET;
        io.deadline = Instant::now() - Duration::from_secs(1);
        assert_eq!(
            io.aborted(),
            Some("thumbnail extraction exceeded its time budget")
        );

        queue.close();
        assert_eq!(io.aborted(), Some("thumbnail worker stopped"));
    }

    #[test]
    fn rotate_pixels_turns_the_image_clockwise() {
        let source = [
            1, 1, 1, 1, 2, 2, 2, 2, //
            3, 3, 3, 3, 4, 4, 4, 4, //
            5, 5, 5, 5, 6, 6, 6, 6, //
        ];

        assert_eq!(
            rotate_pixels(&source, 2, 3, Rotation::Quarter),
            [
                5, 5, 5, 5, 3, 3, 3, 3, 1, 1, 1, 1, //
                6, 6, 6, 6, 4, 4, 4, 4, 2, 2, 2, 2, //
            ]
        );
        assert_eq!(
            rotate_pixels(&source, 2, 3, Rotation::ThreeQuarter),
            [
                2, 2, 2, 2, 4, 4, 4, 4, 6, 6, 6, 6, //
                1, 1, 1, 1, 3, 3, 3, 3, 5, 5, 5, 5, //
            ]
        );
        assert_eq!(
            rotate_pixels(&source, 2, 3, Rotation::Half),
            [
                6, 6, 6, 6, 5, 5, 5, 5, //
                4, 4, 4, 4, 3, 3, 3, 3, //
                2, 2, 2, 2, 1, 1, 1, 1, //
            ]
        );
        assert_eq!(rotate_pixels(&source, 2, 3, Rotation::None), source);
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
        let state = cache.jobs.state.lock().unwrap();
        assert_eq!(state.jobs.len(), MAX_QUEUED_JOBS);
        assert_eq!(state.jobs.back().map(|job| job.key), Some(key(255)));
    }

    #[test]
    fn requests_fail_fast_once_the_worker_queue_is_closed() {
        let (wakeup, _) = async_channel::bounded(1);
        let registry = AttachmentSourceRegistry::new(1);
        let file = tempfile::tempfile().unwrap();
        file.set_len(1).unwrap();
        let source = registered(&registry, key(1), file);
        let mut cache = VideoThumbnailCache::new(1024, wakeup);
        cache.jobs.close();

        let view = cache.request(key(1), source);
        assert!(
            view.failed,
            "a stopped worker must not leave a pending view"
        );
        assert!(!view.pending);
    }

    #[test]
    fn replacing_a_ready_entry_subtracts_its_accounted_bytes() {
        let (wakeup, _) = async_channel::bounded(1);
        let mut cache = VideoThumbnailCache::new(1024, wakeup);
        cache.entries.insert(
            key(1),
            CacheEntry {
                state: CacheState::Ready {
                    image: Arc::new(RenderImage::new(vec![Frame::new(
                        RgbaImage::from_raw(1, 1, vec![0; 4]).unwrap(),
                    )])),
                    duration: None,
                    byte_len: 512,
                },
                touched: 0,
                failures: 0,
            },
        );
        cache.total_bytes = 512;

        cache.record_failure(key(1), "invalid video".into());

        assert_eq!(cache.total_bytes, 0);
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
