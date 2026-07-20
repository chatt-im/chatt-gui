use std::{
    collections::VecDeque,
    fs::{File, OpenOptions},
    io::{self, BufWriter, Read, Write},
    net::Shutdown,
    os::fd::AsRawFd,
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, Result, anyhow, bail};
use libmpv2::{Mpv, protocol};
use rpc::{
    bitstream::{self, Codec},
    daemon::model::LiveShare,
};

use crate::mpv_player::ControlCommand;

const NUT_MAGIC: &[u8] = b"nut/multimedia container\0";
const MAIN_STARTCODE: u64 = 0x4E4D7A561F5F04AD;
const STREAM_STARTCODE: u64 = 0x4E5311405BF2F9DB;
const SYNCPOINT_STARTCODE: u64 = 0x4E4BE4ADEECA4569;
const NUT_VERSION: u64 = 4;
const NUT_MINOR_VERSION: u64 = 1;
const NUT_FLAG_PIPE: u64 = 2;
const NUT_MAX_DISTANCE: u64 = 32 * 1024 - 1;
const FLAG_KEY: u64 = 1;
const FLAG_CODED_PTS: u64 = 8;
const FLAG_SIZE_MSB: u64 = 32;
const FLAG_CHECKSUM: u64 = 64;
const FLAG_CODED: u64 = 4096;
const PTS_SHIFT: u64 = 14;
const MAX_PENDING_FRAMES: usize = 2;
const NUT_FRAME_OVERHEAD_BUDGET: usize = 4096;
const MAX_PENDING_BYTES: usize = rpc::video::MAX_VIDEO_FRAME_LEN + NUT_FRAME_OVERHEAD_BUDGET;
const MAX_RECYCLED_BYTES: usize = MAX_PENDING_BYTES;
// A late viewer may receive the web-equivalent cached GOP as a burst. Let mpv
// consume that burst untimed, then return to the two-frame live queue as soon
// as it drains once.
const MAX_BOOTSTRAP_FRAMES: usize = 90;
const LIVE_RECORDING_MAGIC: &[u8] = b"chatt-live-rpc\0";
const LIVE_RECORDING_VERSION: u32 = 1;
const LIVE_DIAGNOSTIC_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);
const LIVE_DIAGNOSTIC_HISTORY: usize = 512;

pub(crate) struct LiveDiagnostics {
    state: Mutex<LiveDiagnosticState>,
}

#[derive(Default)]
struct LiveDiagnosticState {
    inputs: VecDeque<(u64, u64, Instant)>,
    latest_input: Option<(u64, u64)>,
    latest_render: Option<(u64, u64, u64)>,
    rendered_outputs: u64,
    input_queue_depth: usize,
    last_report: Option<Instant>,
    latency_window_us: Vec<u64>,
    total_latency_us: u128,
    max_latency_us: u64,
}

impl LiveDiagnostics {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(LiveDiagnosticState::default()),
        }
    }

    fn record_input(
        &self,
        sequence: u64,
        pts_ms: u64,
        received_at: Instant,
        input_queue_depth: usize,
    ) {
        let mut state = self.state.lock().unwrap();
        state.inputs.push_back((sequence, pts_ms, received_at));
        if state.inputs.len() > LIVE_DIAGNOSTIC_HISTORY {
            state.inputs.pop_front();
        }
        state.latest_input = Some((sequence, pts_ms));
        state.input_queue_depth = input_queue_depth;
    }

    pub(crate) fn record_render(&self, pts_seconds: f64) {
        if !pts_seconds.is_finite() || pts_seconds < 0.0 {
            return;
        }
        let pts_ms = (pts_seconds * 1_000.0).round() as u64;
        let rendered_at = Instant::now();
        let mut state = self.state.lock().unwrap();
        let (sequence, received_at) = state
            .inputs
            .iter()
            .min_by_key(|(_, input_pts, _)| input_pts.abs_diff(pts_ms))
            .map(|(sequence, _, received_at)| (*sequence, *received_at))
            .unwrap_or((0, rendered_at));
        let latency_us = rendered_at
            .duration_since(received_at)
            .as_micros()
            .min(u64::MAX as u128) as u64;
        state.latest_render = Some((sequence, pts_ms, latency_us));
        state.rendered_outputs += 1;
        state.latency_window_us.push(latency_us);
        state.total_latency_us += u128::from(latency_us);
        state.max_latency_us = state.max_latency_us.max(latency_us);
        let first_render = state.rendered_outputs == 1;
        Self::maybe_report(&mut state, first_render);
    }

    fn maybe_report(state: &mut LiveDiagnosticState, force: bool) {
        let (Some((input_sequence, input_pts)), Some((render_sequence, render_pts, latency_us))) =
            (state.latest_input, state.latest_render)
        else {
            return;
        };
        let now = Instant::now();
        if !force
            && state
                .last_report
                .is_some_and(|last| now.duration_since(last) < LIVE_DIAGNOSTIC_INTERVAL)
        {
            return;
        }
        state.last_report = Some(now);
        state.latency_window_us.sort_unstable();
        let latency_p50_us = percentile(&state.latency_window_us, 50);
        let latency_p95_us = percentile(&state.latency_window_us, 95);
        let latency_max_us = state.latency_window_us.last().copied().unwrap_or(0);
        log::info!(
            "live latency input_frame={} input_pts_ms={} rendered_input_frame={} rendered_pts_ms={} pending_frames={} pending_pts_delta_ms={} receive_to_render_ms={:.3} receive_to_render_p50_ms={:.3} receive_to_render_p95_ms={:.3} receive_to_render_max_ms={:.3} render_outputs={} input_queue={}",
            input_sequence,
            input_pts,
            render_sequence,
            render_pts,
            input_sequence.saturating_sub(render_sequence),
            input_pts.saturating_sub(render_pts),
            latency_us as f64 / 1_000.0,
            latency_p50_us as f64 / 1_000.0,
            latency_p95_us as f64 / 1_000.0,
            latency_max_us as f64 / 1_000.0,
            state.rendered_outputs,
            state.input_queue_depth,
        );
        state.latency_window_us.clear();
    }
}

impl Drop for LiveDiagnostics {
    fn drop(&mut self) {
        let state = self.state.lock().unwrap();
        let average_ms = if state.rendered_outputs == 0 {
            0.0
        } else {
            state.total_latency_us as f64 / state.rendered_outputs as f64 / 1_000.0
        };
        log::info!(
            "live latency summary input_frames={} render_outputs={} receive_to_render_avg_ms={average_ms:.3} receive_to_render_max_ms={:.3}",
            state.latest_input.map_or(0, |(sequence, _)| sequence),
            state.rendered_outputs,
            state.max_latency_us as f64 / 1_000.0,
        );
    }
}

fn percentile(sorted: &[u64], percentile: usize) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let index = (sorted.len() - 1) * percentile / 100;
    sorted[index]
}

/// A self-contained capture of the decrypted video RPC boundary. The header
/// retains the decoder description, dimensions, and wall-clock start. Every
/// following record contains a monotonic receive offset followed by an
/// untouched video RPC frame, including its source timestamp and key flag. This
/// deliberately records before the NUT bridge so playback experiments can
/// distinguish transport, demuxer, decoder, and renderer behavior.
struct LiveRecordingWriter {
    path: PathBuf,
    output: BufWriter<File>,
    started: Instant,
    frames: u64,
    bytes: u64,
}

impl LiveRecordingWriter {
    fn from_env(share: &LiveShare) -> Option<Self> {
        let path = std::env::var_os("CHATT_LIVE_RECORD").map(PathBuf::from)?;
        match Self::create(&path, share) {
            Ok(recorder) => {
                log::info!(
                    "recording decrypted live video RPC stream path={:?} stream_id={} format_version={}",
                    path,
                    share.stream_id.0,
                    LIVE_RECORDING_VERSION
                );
                Some(recorder)
            }
            Err(error) => {
                log::error!(
                    "could not start live video recording path={:?}: {error:#}",
                    path
                );
                None
            }
        }
    }

    fn create(path: &Path, share: &LiveShare) -> Result<Self> {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .with_context(|| format!("create live video recording at {}", path.display()))?;
        let mut output = BufWriter::new(file);
        let started_unix_ns = u64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .context("system time precedes Unix epoch")?
                .as_nanos(),
        )
        .context("system time does not fit recording header")?;
        output.write_all(LIVE_RECORDING_MAGIC)?;
        output.write_all(&LIVE_RECORDING_VERSION.to_le_bytes())?;
        output.write_all(&started_unix_ns.to_le_bytes())?;
        output.write_all(&share.stream_id.0.to_le_bytes())?;
        output.write_all(&share.coded_width.to_le_bytes())?;
        output.write_all(&share.coded_height.to_le_bytes())?;
        write_recording_bytes(&mut output, share.codec.as_bytes())?;
        write_recording_bytes(&mut output, &share.extradata)?;
        output.flush()?;
        Ok(Self {
            path: path.to_path_buf(),
            output,
            started: Instant::now(),
            frames: 0,
            bytes: 0,
        })
    }

    fn write_frame_parts(
        &mut self,
        received_at: Instant,
        header: &[u8],
        payload: &[u8],
    ) -> Result<()> {
        let received_ns = u64::try_from(received_at.duration_since(self.started).as_nanos())
            .context("live recording receive timestamp exceeds u64")?;
        self.output.write_all(&received_ns.to_le_bytes())?;
        self.output.write_all(header)?;
        self.output.write_all(payload)?;
        self.frames += 1;
        self.bytes += (header.len() + payload.len()) as u64;
        Ok(())
    }

    #[cfg(test)]
    fn write_frame(&mut self, received_at: Instant, frame: &[u8]) -> Result<()> {
        self.write_frame_parts(received_at, frame, &[])
    }
}

impl Drop for LiveRecordingWriter {
    fn drop(&mut self) {
        if let Err(error) = self.output.flush() {
            log::error!(
                "could not finish live video recording path={:?}: {error}",
                self.path
            );
        } else {
            log::info!(
                "live video recording finished path={:?} frames={} frame_bytes={}",
                self.path,
                self.frames,
                self.bytes
            );
        }
    }
}

fn write_recording_bytes(output: &mut impl Write, bytes: &[u8]) -> Result<()> {
    let len = u32::try_from(bytes.len()).context("live recording field exceeds u32")?;
    output.write_all(&len.to_le_bytes())?;
    output.write_all(bytes)?;
    Ok(())
}

pub struct LiveStreamSource {
    queue: Arc<NutQueue>,
    control: Arc<UnixStream>,
    reader: Option<JoinHandle<()>>,
}

#[derive(Default)]
pub(crate) struct LiveInputGate {
    released: AtomicBool,
    lock: Mutex<()>,
    ready: Condvar,
}

impl LiveInputGate {
    pub(crate) fn release(&self) -> bool {
        let _guard = self.lock.lock().unwrap();
        if self.released.swap(true, Ordering::AcqRel) {
            return false;
        }
        self.ready.notify_all();
        true
    }

    fn is_released(&self) -> bool {
        self.released.load(Ordering::Acquire)
    }

    fn wait(&self) {
        let mut guard = self.lock.lock().unwrap();
        while !self.is_released() {
            guard = self.ready.wait(guard).unwrap();
        }
    }
}

struct NutQueue {
    stream_id: u32,
    header: Vec<u8>,
    input_gate: Option<Arc<LiveInputGate>>,
    state: Mutex<QueueState>,
    ready: Condvar,
}

#[derive(Default)]
struct QueueState {
    frames: VecDeque<NutFrame>,
    pending_bytes: usize,
    recycled: Vec<Vec<u8>>,
    recycled_bytes: usize,
    seen_keyframe: bool,
    awaiting_keyframe: bool,
    bootstrapping: bool,
    bootstrap_end_received: bool,
    closed: bool,
    dropped_before_key: u64,
    overflow_count: u64,
}

struct NutFrame {
    bytes: Vec<u8>,
}

impl NutFrame {
    fn retained_bytes(&self) -> usize {
        self.bytes.capacity()
    }
}

struct NutCursor {
    queue: Arc<NutQueue>,
    header_offset: usize,
    current: Option<NutFrame>,
    current_offset: usize,
    logged_first_read: bool,
    logged_first_frame: bool,
    delivered_frames: u64,
    logged_input_gate: bool,
}

impl LiveStreamSource {
    pub fn start(
        mpv: Arc<Mpv>,
        share: LiveShare,
        stream: UnixStream,
        diagnostics: Arc<LiveDiagnostics>,
        controls: mpsc::Sender<ControlCommand>,
        errors: mpsc::Sender<String>,
        gpui_wakeup: async_channel::Sender<()>,
        input_gate: Option<Arc<LiveInputGate>>,
    ) -> Result<Self> {
        let codec = codec_from_string(&share.codec)?;
        let recorder = LiveRecordingWriter::from_env(&share);
        let extradata = bitstream::configuration_to_annex_b(codec, &share.extradata)
            .map_err(|error| anyhow!(error))?;
        let header = nut_header(codec, share.coded_width, share.coded_height, &extradata);
        log::info!(
            "live video bridge initialized codec={:?} stream_id={} size={}x{} descriptor_bytes={} nut_header_bytes={}",
            codec,
            share.stream_id.0,
            share.coded_width,
            share.coded_height,
            share.extradata.len(),
            header.len()
        );
        let queue = Arc::new(NutQueue {
            stream_id: share.stream_id.0,
            header,
            input_gate,
            state: Mutex::new(QueueState {
                bootstrapping: true,
                ..QueueState::default()
            }),
            ready: Condvar::new(),
        });
        unsafe {
            protocol::register_owned(
                mpv.as_ref(),
                "chatt-live",
                queue.clone(),
                open_stream,
                close_stream,
                read_stream,
                None,
                None,
            )
        }
        .context("register chatt-live mpv protocol")?;

        let control = Arc::new(stream.try_clone().context("clone live video socket")?);
        let reader_queue = queue.clone();
        let reader_control = controls.clone();
        let reader_mpv = mpv.clone();
        let expected_stream_id = share.stream_id;
        let reader = thread::Builder::new()
            .name(format!("chatt-live-read-{}", expected_stream_id.0))
            .spawn(move || {
                let result = read_frames(
                    stream,
                    expected_stream_id.0,
                    codec,
                    &reader_queue,
                    &reader_control,
                    &reader_mpv,
                    recorder,
                    &diagnostics,
                );
                reader_queue.close();
                if let Err(error) = result {
                    let _ = errors.send(error.to_string());
                    let _ = gpui_wakeup.try_send(());
                    reader_mpv.wakeup();
                }
            })
            .context("spawn live video reader")?;
        Ok(Self {
            queue,
            control,
            reader: Some(reader),
        })
    }
}

fn codec_from_string(codec: &str) -> Result<Codec> {
    if codec.starts_with("avc1.") || codec.eq_ignore_ascii_case("h264") {
        Ok(Codec::H264)
    } else if codec.starts_with("hvc1.")
        || codec.starts_with("hev1.")
        || codec.eq_ignore_ascii_case("hevc")
    {
        Ok(Codec::Hevc)
    } else {
        bail!("unsupported live video codec {codec:?}")
    }
}

fn read_frames(
    mut stream: UnixStream,
    expected_stream_id: u32,
    _codec: Codec,
    queue: &NutQueue,
    controls: &mpsc::Sender<ControlCommand>,
    mpv: &Mpv,
    mut recorder: Option<LiveRecordingWriter>,
    diagnostics: &LiveDiagnostics,
) -> Result<()> {
    let mut base_timestamp = None;
    let mut received_frames = 0u64;
    let mut received_bytes = 0u64;
    let mut saw_keyframe = false;
    let mut wrote_initial_syncpoint = false;
    loop {
        let mut wire_header = [0u8; rpc::video::VIDEO_FRAME_HEADER_LEN];
        if !read_frame_header(&mut stream, &mut wire_header)
            .context("read live video frame header")?
        {
            log::info!(
                "live video socket closed stream_id={} frames={} bytes={} saw_keyframe={}",
                expected_stream_id,
                received_frames,
                received_bytes,
                saw_keyframe
            );
            return Ok(());
        }
        let header = rpc::video::parse_video_frame_header(&wire_header)
            .map_err(|error| anyhow!("invalid live video frame header: {error:?}"))?
            .expect("fixed-size video header is complete");
        if header.stream_id != expected_stream_id {
            bail!(
                "live video frame belongs to stream {}, expected {expected_stream_id}",
                header.stream_id
            )
        }
        if header.bootstrap_end {
            queue.finish_bootstrap();
            log::info!(
                "live video cached GOP complete stream_id={} cached_frames={}",
                header.stream_id,
                received_frames
            );
            continue;
        }
        let payload_len = header.size - rpc::video::VIDEO_FRAME_HEADER_LEN;
        let base = *base_timestamp.get_or_insert(header.ts_ms);
        let pts = header.ts_ms.saturating_sub(base).max(0) as u64;
        let initial_syncpoint = header.is_key && !wrote_initial_syncpoint;
        let mut nut_bytes = queue.take_buffer(payload_len + NUT_FRAME_OVERHEAD_BUDGET);
        let payload_offset = start_nut_frame(
            &mut nut_bytes,
            pts,
            header.is_key,
            initial_syncpoint,
            payload_len,
        );
        if let Err(error) = read_exact_append(&stream, &mut nut_bytes, payload_len) {
            queue.recycle_buffer(nut_bytes);
            return Err(error).context("read live video frame payload");
        }
        let received_at = Instant::now();
        if let Some(active) = recorder.as_mut()
            && let Err(error) = active.write_frame_parts(
                received_at,
                &wire_header,
                &nut_bytes[payload_offset..],
            )
        {
            log::error!(
                "live video recording failed; disabling recording path={:?}: {error:#}",
                active.path
            );
            recorder = None;
        }
        if let Err(error) =
            bitstream::length_prefixed_to_annex_b_in_place(&mut nut_bytes[payload_offset..])
        {
            queue.recycle_buffer(nut_bytes);
            return Err(anyhow!(error));
        }
        received_frames += 1;
        received_bytes += header.size as u64;
        if received_frames == 1 {
            log::info!(
                "live video first socket frame stream_id={} keyframe={} timestamp_ms={} frame_bytes={} bitstream_bytes={}",
                header.stream_id,
                header.is_key,
                header.ts_ms,
                header.size,
                payload_len
            );
        }
        if header.is_key && !saw_keyframe {
            saw_keyframe = true;
            log::info!(
                "live video first keyframe received stream_id={} frame_number={} timestamp_ms={} bitstream_bytes={}",
                header.stream_id,
                received_frames,
                header.ts_ms,
                payload_len
            );
        }
        let caught_up = queue.push(NutFrame { bytes: nut_bytes }, header.is_key);
        diagnostics.record_input(received_frames, pts, received_at, queue.pending_len());
        if initial_syncpoint {
            wrote_initial_syncpoint = true;
        }
        if caught_up {
            let _ = controls.send(ControlCommand::DropBuffers);
            mpv.wakeup();
        }
    }
}

fn read_frame_header(
    stream: &mut UnixStream,
    header: &mut [u8; rpc::video::VIDEO_FRAME_HEADER_LEN],
) -> io::Result<bool> {
    let mut filled = 0;
    while filled < header.len() {
        match stream.read(&mut header[filled..]) {
            Ok(0) if filled == 0 => return Ok(false),
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "live video socket closed during a frame header",
                ));
            }
            Ok(count) => filled += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(true)
}

fn read_exact_append(stream: &UnixStream, output: &mut Vec<u8>, len: usize) -> io::Result<()> {
    let final_len = output
        .len()
        .checked_add(len)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "video frame is too large"))?;
    output.reserve_exact(len);
    while output.len() < final_len {
        let remaining = final_len - output.len();
        let result = unsafe {
            libc::read(
                stream.as_raw_fd(),
                output.as_mut_ptr().add(output.len()).cast(),
                remaining,
            )
        };
        if result > 0 {
            unsafe {
                output.set_len(output.len() + result as usize);
            }
        } else if result == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "live video socket closed during a frame payload",
            ));
        } else {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
    Ok(())
}

impl NutQueue {
    fn take_buffer(&self, min_capacity: usize) -> Vec<u8> {
        let recycled = {
            let mut state = self.state.lock().unwrap();
            let index = state
                .recycled
                .iter()
                .enumerate()
                .filter(|(_, buffer)| buffer.capacity() >= min_capacity)
                .min_by_key(|(_, buffer)| buffer.capacity())
                .map(|(index, _)| index)
                .or_else(|| {
                    state
                        .recycled
                        .iter()
                        .enumerate()
                        .max_by_key(|(_, buffer)| buffer.capacity())
                        .map(|(index, _)| index)
                });
            index.map(|index| {
                let mut buffer = state.recycled.swap_remove(index);
                state.recycled_bytes -= buffer.capacity();
                buffer.clear();
                buffer
            })
        };
        recycled.unwrap_or_else(|| Vec::with_capacity(min_capacity))
    }

    fn recycle_buffer(&self, buffer: Vec<u8>) {
        let mut state = self.state.lock().unwrap();
        if !state.closed {
            state.recycle_buffer(buffer);
        }
    }

    fn push(&self, frame: NutFrame, is_key: bool) -> bool {
        let mut state = self.state.lock().unwrap();
        if state.closed {
            return false;
        }
        if !state.seen_keyframe {
            if !is_key {
                state.dropped_before_key += 1;
                if state.dropped_before_key == 1 {
                    log::warn!(
                        "live video waiting for keyframe stream_id={}",
                        self.stream_id
                    );
                }
                state.recycle_frame(frame);
                return false;
            }
            state.seen_keyframe = true;
        }
        let mut caught_up = false;
        if state.awaiting_keyframe {
            if !is_key {
                state.recycle_frame(frame);
                return false;
            }
            state.clear_frames();
            state.awaiting_keyframe = false;
            caught_up = true;
            log::info!(
                "live video resumed at fresh keyframe stream_id={} overflow_count={}",
                self.stream_id,
                state.overflow_count
            );
        } else {
            let frame_limit = if state.bootstrapping {
                MAX_BOOTSTRAP_FRAMES
            } else {
                MAX_PENDING_FRAMES
            };
            let over_limit = state.frames.len() >= frame_limit
                || state
                    .pending_bytes
                    .saturating_add(frame.retained_bytes())
                    > MAX_PENDING_BYTES;
            if over_limit {
                if is_key {
                    state.clear_frames();
                    caught_up = true;
                } else {
                    state.awaiting_keyframe = true;
                    state.bootstrapping = false;
                    state.overflow_count += 1;
                    log::warn!(
                        "live video decoder input queue full; waiting for keyframe stream_id={} pending_frames={} pending_bytes={} overflow_count={}",
                        self.stream_id,
                        state.frames.len(),
                        state.pending_bytes,
                        state.overflow_count
                    );
                    state.recycle_frame(frame);
                    return false;
                }
            }
        }
        if frame.retained_bytes() > MAX_PENDING_BYTES {
            state.awaiting_keyframe = true;
            state.bootstrapping = false;
            state.recycle_frame(frame);
            return false;
        }
        state.pending_bytes += frame.retained_bytes();
        state.frames.push_back(frame);
        self.ready.notify_one();
        caught_up
    }

    fn pop(&self) -> Option<NutFrame> {
        let mut state = self.state.lock().unwrap();
        loop {
            if let Some(frame) = state.pop_frame() {
                return Some(frame);
            }
            if state.closed {
                return None;
            }
            state = self.ready.wait(state).unwrap();
        }
    }

    fn try_pop(&self) -> Option<NutFrame> {
        self.state.lock().unwrap().pop_frame()
    }

    fn pending_len(&self) -> usize {
        self.state.lock().unwrap().frames.len()
    }

    fn finish_bootstrap(&self) {
        let mut state = self.state.lock().unwrap();
        state.bootstrap_end_received = true;
        if state.frames.is_empty() {
            state.bootstrapping = false;
        }
    }

    fn close(&self) {
        {
            let mut state = self.state.lock().unwrap();
            state.closed = true;
            state.clear_frames();
            state.recycled.clear();
            state.recycled_bytes = 0;
        }
        self.ready.notify_all();
        if let Some(gate) = self.input_gate.as_ref() {
            gate.release();
        }
    }
}

impl QueueState {
    fn pop_frame(&mut self) -> Option<NutFrame> {
        let frame = self.frames.pop_front()?;
        self.pending_bytes -= frame.retained_bytes();
        if self.frames.is_empty() && self.bootstrap_end_received {
            self.bootstrapping = false;
        }
        Some(frame)
    }

    fn clear_frames(&mut self) {
        self.pending_bytes = 0;
        while let Some(frame) = self.frames.pop_front() {
            self.recycle_frame(frame);
        }
    }

    fn recycle_frame(&mut self, frame: NutFrame) {
        self.recycle_buffer(frame.bytes);
    }

    fn recycle_buffer(&mut self, mut buffer: Vec<u8>) {
        let capacity = buffer.capacity();
        if capacity == 0 || capacity > MAX_RECYCLED_BYTES {
            return;
        }
        while self.recycled_bytes.saturating_add(capacity) > MAX_RECYCLED_BYTES {
            let Some(discarded) = self.recycled.pop() else {
                return;
            };
            self.recycled_bytes -= discarded.capacity();
        }
        buffer.clear();
        self.recycled_bytes += capacity;
        self.recycled.push(buffer);
    }
}

fn open_stream(queue: &mut Arc<NutQueue>, _uri: &str) -> NutCursor {
    log::info!(
        "mpv opened live NUT stream stream_id={} header_bytes={}",
        queue.stream_id,
        queue.header.len()
    );
    NutCursor {
        queue: queue.clone(),
        header_offset: 0,
        current: None,
        current_offset: 0,
        logged_first_read: false,
        logged_first_frame: false,
        delivered_frames: 0,
        logged_input_gate: false,
    }
}

fn close_stream(mut cursor: Box<NutCursor>) {
    if let Some(frame) = cursor.current.take() {
        cursor.queue.recycle_buffer(frame.bytes);
    }
    log::info!(
        "mpv closed live NUT stream stream_id={} header_bytes_read={} had_frame={}",
        cursor.queue.stream_id,
        cursor.header_offset,
        cursor.logged_first_frame
    );
}

fn read_stream(cursor: &mut NutCursor, output: &mut [std::os::raw::c_char]) -> i64 {
    let output = unsafe {
        std::slice::from_raw_parts_mut(output.as_mut_ptr().cast::<u8>(), output.len())
    };
    if !cursor.logged_first_read {
        cursor.logged_first_read = true;
        log::info!(
            "mpv requested first live NUT bytes stream_id={} requested_bytes={}",
            cursor.queue.stream_id,
            output.len()
        );
    }
    let mut written = 0usize;
    while written < output.len() {
        if cursor.header_offset < cursor.queue.header.len() {
            let count = (output.len() - written)
                .min(cursor.queue.header.len() - cursor.header_offset);
            output[written..written + count].copy_from_slice(
                &cursor.queue.header[cursor.header_offset..cursor.header_offset + count],
            );
            cursor.header_offset += count;
            written += count;
            continue;
        }
        if cursor.current.is_none() {
            if cursor.delivered_frames != 0
                && let Some(gate) = cursor.queue.input_gate.as_ref()
                && !gate.is_released()
            {
                if written != 0 {
                    break;
                }
                if !cursor.logged_input_gate {
                    cursor.logged_input_gate = true;
                    log::info!(
                        "holding live decoder input after first frame until video output is ready stream_id={}",
                        cursor.queue.stream_id
                    );
                }
                gate.wait();
                log::info!(
                    "released remaining live decoder input stream_id={}",
                    cursor.queue.stream_id
                );
            }
            let frame = if written == 0 {
                cursor.queue.pop()
            } else {
                cursor.queue.try_pop()
            };
            let Some(frame) = frame else {
                break;
            };
            if !cursor.logged_first_frame {
                cursor.logged_first_frame = true;
                log::info!(
                    "mpv received first live NUT frame stream_id={} nut_frame_bytes={}",
                    cursor.queue.stream_id,
                    frame.bytes.len()
                );
            }
            cursor.current = Some(frame);
            cursor.current_offset = 0;
        }
        let current = &cursor.current.as_ref().unwrap().bytes;
        let count = (output.len() - written).min(current.len() - cursor.current_offset);
        output[written..written + count]
            .copy_from_slice(&current[cursor.current_offset..cursor.current_offset + count]);
        cursor.current_offset += count;
        written += count;
        if cursor.current_offset == current.len() {
            let frame = cursor.current.take().unwrap();
            cursor.queue.recycle_buffer(frame.bytes);
            cursor.current_offset = 0;
            cursor.delivered_frames += 1;
        }
    }
    written as i64
}

fn nut_header(codec: Codec, width: u32, height: u32, extradata: &[u8]) -> Vec<u8> {
    let mut out = NUT_MAGIC.to_vec();
    out.extend_from_slice(&main_header());
    out.extend_from_slice(&stream_header(codec, width, height, extradata));
    out
}

fn main_header() -> Vec<u8> {
    let mut payload = Vec::new();
    put_v(&mut payload, NUT_VERSION);
    put_v(&mut payload, NUT_MINOR_VERSION);
    put_v(&mut payload, 1);
    put_v(&mut payload, NUT_MAX_DISTANCE);
    put_v(&mut payload, 1);
    put_v(&mut payload, 1);
    put_v(&mut payload, 1000);
    put_v(&mut payload, FLAG_CODED);
    put_v(&mut payload, 6);
    put_s(&mut payload, 0);
    put_v(&mut payload, 1);
    put_v(&mut payload, 0);
    put_v(&mut payload, 0);
    put_v(&mut payload, 0);
    put_v(&mut payload, 255);
    put_v(&mut payload, 0);
    put_v(&mut payload, NUT_FLAG_PIPE);
    packet(MAIN_STARTCODE, &payload)
}

fn stream_header(codec: Codec, width: u32, height: u32, extradata: &[u8]) -> Vec<u8> {
    let mut payload = Vec::new();
    put_v(&mut payload, 0);
    put_v(&mut payload, 0);
    put_v(&mut payload, 4);
    payload.extend_from_slice(match codec {
        Codec::H264 => b"H264",
        Codec::Hevc => b"HEVC",
    });
    put_v(&mut payload, 0);
    put_v(&mut payload, PTS_SHIFT);
    put_v(&mut payload, 10_000);
    put_v(&mut payload, 0);
    put_v(&mut payload, 0);
    put_v(&mut payload, extradata.len() as u64);
    payload.extend_from_slice(extradata);
    for value in [width as u64, height as u64, 0, 0, 0] {
        put_v(&mut payload, value);
    }
    packet(STREAM_STARTCODE, &payload)
}

fn start_nut_frame(
    out: &mut Vec<u8>,
    pts: u64,
    is_key: bool,
    initial_syncpoint: bool,
    payload_len: usize,
) -> usize {
    out.clear();
    if initial_syncpoint {
        let mut sync = Vec::new();
        put_v(&mut sync, pts);
        put_v(&mut sync, 0);
        append_packet(out, SYNCPOINT_STARTCODE, &sync);
    }
    let frame_header_start = out.len();
    out.push(0);
    let flags = (if is_key { FLAG_KEY } else { 0 })
        | FLAG_CODED_PTS
        | FLAG_SIZE_MSB
        | FLAG_CHECKSUM;
    put_v(out, FLAG_CODED ^ flags);
    put_v(out, pts + (1 << PTS_SHIFT));
    put_v(out, payload_len as u64);
    let checksum = nut_crc(&out[frame_header_start..]);
    out.extend_from_slice(&checksum.to_le_bytes());
    out.reserve_exact(payload_len);
    out.len()
}

#[cfg(test)]
fn nut_frame(pts: u64, is_key: bool, initial_syncpoint: bool, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + NUT_FRAME_OVERHEAD_BUDGET);
    start_nut_frame(
        &mut out,
        pts,
        is_key,
        initial_syncpoint,
        payload.len(),
    );
    out.extend_from_slice(payload);
    out
}

fn packet(startcode: u64, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 16);
    append_packet(&mut out, startcode, payload);
    out
}

fn append_packet(out: &mut Vec<u8>, startcode: u64, payload: &[u8]) {
    let prefix_start = out.len();
    out.extend_from_slice(&startcode.to_be_bytes());
    put_v(out, payload.len() as u64 + 4);
    if payload.len() + 4 > 4096 {
        let checksum = nut_crc(&out[prefix_start..]);
        out.extend_from_slice(&checksum.to_le_bytes());
    }
    out.extend_from_slice(payload);
    out.extend_from_slice(&nut_crc(payload).to_le_bytes());
}

fn nut_crc(bytes: &[u8]) -> u32 {
    let mut crc = 0u32;
    for byte in bytes {
        let mut entry = (((crc as u8) ^ *byte) as u32) << 24;
        for _ in 0..8 {
            entry = if entry & 0x8000_0000 != 0 {
                (entry << 1) ^ 0x04C1_1DB7
            } else {
                entry << 1
            };
        }
        crc = entry.swap_bytes() ^ (crc >> 8);
    }
    crc
}

fn put_v(out: &mut Vec<u8>, value: u64) {
    let groups = ((64 - value.leading_zeros() as usize).max(1) + 6) / 7;
    for index in (0..groups).rev() {
        let mut byte = ((value >> (index * 7)) & 0x7f) as u8;
        if index != 0 {
            byte |= 0x80;
        }
        out.push(byte);
    }
}

fn put_s(out: &mut Vec<u8>, value: i64) {
    let coded = if value > 0 {
        value as u64 * 2 - 1
    } else {
        value.unsigned_abs() * 2
    };
    put_v(out, coded);
}

impl Drop for LiveStreamSource {
    fn drop(&mut self) {
        self.queue.close();
        let _ = self.control.shutdown(Shutdown::Both);
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

#[cfg(test)]
struct LiveRecording {
    started_unix_ns: u64,
    stream_id: u32,
    coded_width: u32,
    coded_height: u32,
    codec: String,
    extradata: Vec<u8>,
    frames: Vec<RecordedLiveFrame>,
}

#[cfg(test)]
struct RecordedLiveFrame {
    received_ns: u64,
    wire: Vec<u8>,
}

#[cfg(test)]
fn read_live_recording(path: &Path) -> Result<LiveRecording> {
    let mut input = std::io::BufReader::new(
        File::open(path).with_context(|| format!("open recording at {}", path.display()))?,
    );
    let mut magic = vec![0; LIVE_RECORDING_MAGIC.len()];
    input.read_exact(&mut magic)?;
    if magic != LIVE_RECORDING_MAGIC {
        bail!("not a chatt live RPC recording")
    }
    let version = read_recording_u32(&mut input)?;
    if version != LIVE_RECORDING_VERSION {
        bail!("unsupported chatt live RPC recording version {version}")
    }
    let started_unix_ns = read_recording_u64(&mut input)?;
    let stream_id = read_recording_u32(&mut input)?;
    let coded_width = read_recording_u32(&mut input)?;
    let coded_height = read_recording_u32(&mut input)?;
    let codec = String::from_utf8(read_recording_bytes(&mut input)?)
        .context("recording codec is not UTF-8")?;
    let extradata = read_recording_bytes(&mut input)?;
    let mut frames = Vec::new();
    loop {
        let mut received = [0; 8];
        match input.read_exact(&mut received) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(error) => return Err(error).context("read recorded video receive timestamp"),
        }
        let received_ns = u64::from_le_bytes(received);
        let mut prefix = [0; 4];
        input
            .read_exact(&mut prefix)
            .context("read recorded video frame length")?;
        let size = u32::from_le_bytes(prefix) as usize;
        if !(rpc::video::VIDEO_FRAME_HEADER_LEN..=rpc::video::MAX_VIDEO_FRAME_LEN).contains(&size) {
            bail!("invalid recorded video frame length {size}")
        }
        let mut frame = vec![0; size];
        frame[..4].copy_from_slice(&prefix);
        input.read_exact(&mut frame[4..])?;
        frames.push(RecordedLiveFrame {
            received_ns,
            wire: frame,
        });
    }
    Ok(LiveRecording {
        started_unix_ns,
        stream_id,
        coded_width,
        coded_height,
        codec,
        extradata,
        frames,
    })
}

#[cfg(test)]
fn read_recording_u32(input: &mut impl Read) -> Result<u32> {
    let mut bytes = [0; 4];
    input.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

#[cfg(test)]
fn read_recording_u64(input: &mut impl Read) -> Result<u64> {
    let mut bytes = [0; 8];
    input.read_exact(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

#[cfg(test)]
fn read_recording_bytes(input: &mut impl Read) -> Result<Vec<u8>> {
    let len = read_recording_u32(input)? as usize;
    if len > rpc::video::MAX_VIDEO_FRAME_LEN {
        bail!("live recording metadata field is too large: {len}")
    }
    let mut bytes = vec![0; len];
    input.read_exact(&mut bytes)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use libmpv2::events::Event;
    use rpc::ids::{RoomId, StreamId};
    use std::{
        io::Write,
        process::{Command, Stdio},
        time::{Duration, Instant},
    };

    fn queued(bytes: &[u8]) -> NutFrame {
        NutFrame {
            bytes: bytes.to_vec(),
        }
    }

    #[test]
    fn crc_matches_ffmpeg_nut_polynomial_vector() {
        assert_eq!(nut_crc(b"123456789"), 0x7F89_A189);
    }

    #[test]
    fn live_rpc_recording_round_trips_metadata_timestamps_and_frames() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("live.rpc");
        let share = LiveShare {
            room_id: RoomId(1),
            stream_id: StreamId(42),
            sender_name: "test".into(),
            codec: "avc1.64001F".into(),
            coded_width: 1920,
            coded_height: 1080,
            extradata: vec![1, 2, 3, 4],
        };
        let mut first = Vec::new();
        rpc::video::write_video_frame(&mut first, 100, true, 42, &[5, 6, 7]);
        let mut second = Vec::new();
        rpc::video::write_video_frame(&mut second, 9_100, false, 42, &[8, 9]);
        {
            let mut recording = LiveRecordingWriter::create(&path, &share).unwrap();
            let started = recording.started;
            recording
                .write_frame(started + Duration::from_millis(10), &first)
                .unwrap();
            recording
                .write_frame(started + Duration::from_millis(9_010), &second)
                .unwrap();
        }

        let recording = read_live_recording(&path).unwrap();
        assert!(recording.started_unix_ns > 0);
        assert_eq!(recording.stream_id, 42);
        assert_eq!(recording.coded_width, 1920);
        assert_eq!(recording.coded_height, 1080);
        assert_eq!(recording.codec, "avc1.64001F");
        assert_eq!(recording.extradata, [1, 2, 3, 4]);
        assert_eq!(recording.frames[0].received_ns, 10_000_000);
        assert_eq!(recording.frames[1].received_ns, 9_010_000_000);
        assert_eq!(recording.frames[0].wire, first);
        assert_eq!(recording.frames[1].wire, second);
        assert_eq!(
            i64::from_le_bytes(recording.frames[1].wire[4..12].try_into().unwrap()),
            9_100
        );
    }

    #[test]
    #[ignore = "requires CHATT_LIVE_REPLAY=/path/to/chatt-live.rpc"]
    fn replays_external_live_rpc_recording_through_libmpv() {
        let path = std::env::var_os("CHATT_LIVE_REPLAY")
            .map(PathBuf::from)
            .expect("set CHATT_LIVE_REPLAY to a live RPC recording");
        let recording = read_live_recording(&path).unwrap();
        assert!(!recording.frames.is_empty(), "recording has no video frames");
        let first_pts = i64::from_le_bytes(
            recording.frames[0].wire[4..12].try_into().unwrap(),
        );
        let final_pts = i64::from_le_bytes(
            recording.frames.last().unwrap().wire[4..12]
                .try_into()
                .unwrap(),
        );
        let expected_position = (final_pts - first_pts).max(0) as f64 / 1_000.0;
        let frame_count = recording.frames.len();
        let share = LiveShare {
            room_id: RoomId(1),
            stream_id: StreamId(recording.stream_id),
            sender_name: "recording".into(),
            codec: recording.codec,
            coded_width: recording.coded_width,
            coded_height: recording.coded_height,
            extradata: recording.extradata,
        };
        let (mut daemon_stream, gui_stream) = UnixStream::pair().unwrap();
        let mpv = Arc::new(
            Mpv::with_initializer(|initializer| {
                initializer.set_option("vo", "libmpv")?;
                initializer.set_option("profile", "low-latency")?;
                initializer.set_option("audio", "no")?;
                initializer.set_option("cache", "no")?;
                initializer.set_option("demuxer-thread", "yes")?;
                initializer.set_option("demuxer-readahead-secs", "0")?;
                initializer.set_option("demuxer-lavf-format", "nut")?;
                initializer.set_option("demuxer-lavf-probe-info", "nostreams")?;
                initializer.set_option("demuxer-lavf-analyzeduration", "0")?;
                initializer.set_option("untimed", "yes")?;
                initializer.set_option("video-latency-hacks", "yes")?;
                initializer.set_option("vd-lavc-threads", "1")?;
                initializer.set_option("vd-lavc-low-latency", "yes")?;
                initializer.set_option("stream-buffer-size", "4k")?;
                Ok(())
            })
            .unwrap(),
        );
        let mut render = mpv.create_software_render_context(true).unwrap();
        render.set_update_callback(|| {});
        let diagnostics = Arc::new(LiveDiagnostics::new());
        let (control_sender, _control_receiver) = mpsc::channel();
        let (error_sender, error_receiver) = mpsc::channel();
        let (wakeup_sender, _wakeup_receiver) = async_channel::bounded(1);
        let _source = LiveStreamSource::start(
            mpv.clone(),
            share,
            gui_stream,
            diagnostics,
            control_sender,
            error_sender,
            wakeup_sender,
            None,
        )
        .unwrap();
        mpv.command("loadfile", &["chatt-live://stream", "replace"])
            .unwrap();

        // Preserve ordering and burst structure but compress wall-clock gaps so
        // long damage-idle periods do not make an experimental test take as
        // long as the original recording.
        let replay_started = Instant::now();
        let receive_origin = recording.frames[0].received_ns;
        for frame in &recording.frames {
            let target = Duration::from_nanos((frame.received_ns - receive_origin) / 10);
            if let Some(wait) = target.checked_sub(replay_started.elapsed()) {
                std::thread::sleep(wait);
            }
            daemon_stream.write_all(&frame.wire).unwrap();
            render.update();
        }

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut reached_final_frame = false;
        while Instant::now() < deadline {
            render.update();
            if render.next_frame_video_pts().is_ok_and(|pts| {
                pts.is_finite() && pts + 0.001 >= expected_position
            }) {
                reached_final_frame = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(
            reached_final_frame,
            "libmpv did not retain the recording's final frame at {expected_position:.3}s"
        );
        eprintln!(
            "replayed {frame_count} frames spanning {expected_position:.3}s in {:.3}s",
            replay_started.elapsed().as_secs_f64()
        );
        assert!(
            error_receiver.try_recv().is_err(),
            "live socket reader reported an error"
        );
    }

    #[test]
    fn queue_drops_to_the_next_keyframe() {
        let queue = NutQueue {
            stream_id: 1,
            header: Vec::new(),
            input_gate: None,
            state: Mutex::new(QueueState::default()),
            ready: Condvar::new(),
        };
        assert!(!queue.push(queued(&[0]), false));
        assert!(queue.state.lock().unwrap().frames.is_empty());
        assert!(!queue.push(queued(&[1]), true));
        for marker in 0..MAX_PENDING_FRAMES {
            assert!(!queue.push(queued(&[marker as u8]), false));
        }
        assert!(queue.push(queued(&[4]), true));
        assert_eq!(queue.pop().unwrap().bytes, [4]);
    }

    #[test]
    fn bootstrap_only_ends_at_the_explicit_daemon_boundary() {
        let queue = NutQueue {
            stream_id: 1,
            header: Vec::new(),
            input_gate: None,
            state: Mutex::new(QueueState {
                bootstrapping: true,
                ..QueueState::default()
            }),
            ready: Condvar::new(),
        };
        assert!(!queue.push(queued(&[0]), true));
        assert_eq!(queue.pop().unwrap().bytes, [0]);

        // mpv can drain the first cached frame before the rest of the cached
        // GOP crosses the socket. That temporary emptiness is not a boundary.
        for marker in 1..=MAX_PENDING_FRAMES + 1 {
            assert!(!queue.push(queued(&[marker as u8]), false));
        }
        assert!(queue.state.lock().unwrap().bootstrapping);

        queue.finish_bootstrap();
        assert!(queue.state.lock().unwrap().bootstrapping);
        while queue.try_pop().is_some() {}
        assert!(!queue.state.lock().unwrap().bootstrapping);
    }

    #[test]
    fn empty_cached_gop_boundary_enters_live_mode_immediately() {
        let queue = NutQueue {
            stream_id: 1,
            header: Vec::new(),
            input_gate: None,
            state: Mutex::new(QueueState {
                bootstrapping: true,
                ..QueueState::default()
            }),
            ready: Condvar::new(),
        };
        queue.finish_bootstrap();
        let state = queue.state.lock().unwrap();
        assert!(state.bootstrap_end_received);
        assert!(!state.bootstrapping);
    }

    #[test]
    fn protocol_read_returns_a_complete_frame_without_waiting_for_the_next() {
        let queue = Arc::new(NutQueue {
            stream_id: 1,
            header: vec![1],
            input_gate: None,
            state: Mutex::new(QueueState::default()),
            ready: Condvar::new(),
        });
        queue.push(queued(&[2, 3, 4]), true);
        let mut cursor = open_stream(&mut queue.clone(), "chatt-live://test");
        let mut output = [0i8; 64];
        assert_eq!(read_stream(&mut cursor, &mut output), 4);
        assert_eq!(&output[..4], &[1, 2, 3, 4]);
        assert_eq!(queue.state.lock().unwrap().recycled.len(), 1);
    }

    #[test]
    fn protocol_holds_frames_after_key_until_video_output_is_ready() {
        let gate = Arc::new(LiveInputGate::default());
        let queue = Arc::new(NutQueue {
            stream_id: 1,
            header: vec![1],
            input_gate: Some(gate.clone()),
            state: Mutex::new(QueueState::default()),
            ready: Condvar::new(),
        });
        queue.push(queued(&[2, 3]), true);
        queue.push(queued(&[4, 5]), false);
        let mut cursor = open_stream(&mut queue.clone(), "chatt-live://test");
        let mut output = [0i8; 64];

        assert_eq!(read_stream(&mut cursor, &mut output), 3);
        assert_eq!(&output[..3], &[1, 2, 3]);
        assert_eq!(queue.pending_len(), 1);

        assert!(gate.release());
        assert_eq!(read_stream(&mut cursor, &mut output), 2);
        assert_eq!(&output[..2], &[4, 5]);
    }

    #[test]
    fn socket_payload_is_appended_and_converted_in_the_final_nut_buffer() {
        let body = [0, 0, 0, 2, 0x65, 0x88];
        let wire = rpc::video::encode_video_frame(40, true, 7, &body);
        let (mut writer, mut reader) = UnixStream::pair().unwrap();
        writer.write_all(&wire).unwrap();

        let mut header_bytes = [0; rpc::video::VIDEO_FRAME_HEADER_LEN];
        assert!(read_frame_header(&mut reader, &mut header_bytes).unwrap());
        let header = rpc::video::parse_video_frame_header(&header_bytes)
            .unwrap()
            .unwrap();
        let mut output = Vec::with_capacity(body.len() + NUT_FRAME_OVERHEAD_BUDGET);
        let payload_offset = start_nut_frame(&mut output, 0, true, true, body.len());
        let allocation = output.as_ptr();
        read_exact_append(&reader, &mut output, body.len()).unwrap();
        assert_eq!(allocation, output.as_ptr());
        bitstream::length_prefixed_to_annex_b_in_place(&mut output[payload_offset..]).unwrap();
        assert_eq!(&output[payload_offset..], [0, 0, 0, 1, 0x65, 0x88]);
        assert_eq!(header.size, wire.len());
    }

    #[test]
    fn consumed_nut_buffers_are_reused() {
        let queue = NutQueue {
            stream_id: 1,
            header: Vec::new(),
            input_gate: None,
            state: Mutex::new(QueueState::default()),
            ready: Condvar::new(),
        };
        let mut bytes = Vec::with_capacity(4096);
        bytes.extend_from_slice(&[1, 2, 3]);
        let allocation = bytes.as_ptr();
        queue.push(NutFrame { bytes }, true);
        let consumed = queue.pop().unwrap();
        queue.recycle_buffer(consumed.bytes);

        let reused = queue.take_buffer(1024);
        assert_eq!(reused.as_ptr(), allocation);
        assert!(reused.is_empty());
    }

    #[test]
    fn queue_byte_overflow_waits_for_a_fresh_keyframe() {
        let queue = NutQueue {
            stream_id: 1,
            header: Vec::new(),
            input_gate: None,
            state: Mutex::new(QueueState::default()),
            ready: Condvar::new(),
        };
        let key = Vec::with_capacity(MAX_PENDING_BYTES / 2 + 1);
        queue.push(NutFrame { bytes: key }, true);
        let delta = Vec::with_capacity(MAX_PENDING_BYTES / 2 + 1);
        queue.push(NutFrame { bytes: delta }, false);
        {
            let state = queue.state.lock().unwrap();
            assert!(state.awaiting_keyframe);
            assert_eq!(state.frames.len(), 1);
            assert!(state.pending_bytes <= MAX_PENDING_BYTES);
        }
        assert!(queue.push(queued(&[9]), true));
        assert_eq!(queue.pop().unwrap().bytes, [9]);
    }

    #[test]
    fn ffprobe_accepts_large_sparse_frames_in_nut_pipe_mode() {
        let sps = [
            0x67, 0x42, 0xc0, 0x0d, 0xda, 0x05, 0x07, 0xec, 0x04, 0x40, 0x00, 0x00, 0x03,
            0x00, 0x40, 0x00, 0x00, 0x0f, 0x03, 0xc5, 0x0a, 0xa8,
        ];
        let pps = [0x68, 0xce, 0x0f, 0xc8];
        let avcc = rpc::bitstream::h264::build_avcc_extra_data(&sps, &pps);
        let extradata = rpc::bitstream::configuration_to_annex_b(Codec::H264, &avcc).unwrap();
        let mut nut = nut_header(Codec::H264, 320, 240, &extradata);
        let mut keyframe = vec![0x88; 140_000];
        keyframe[..5].copy_from_slice(&[0, 0, 0, 1, 0x65]);
        nut.extend_from_slice(&nut_frame(0, true, true, &keyframe));
        nut.extend_from_slice(&nut_frame(
            30_000,
            false,
            false,
            &[0, 0, 0, 1, 0x41, 0x9a],
        ));

        let mut child = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-f",
                "nut",
                "-show_entries",
                "packet=pts,size,flags",
                "-of",
                "csv=p=0",
                "-i",
                "pipe:0",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("ffprobe is available with the required libmpv dependency");
        child.stdin.take().unwrap().write_all(&nut).unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "ffprobe rejected generated NUT: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            !output.stdout.is_empty(),
            "ffprobe did not emit the large, sparsely timestamped frames"
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).lines().count(),
            2,
            "ffprobe did not demux both sparse NUT PIPE frames: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn libmpv_outputs_each_sparse_live_frame_without_waiting_for_the_next() {
        let encoded = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=320x240:rate=30",
                "-frames:v",
                "1",
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-tune",
                "zerolatency",
                "-bf",
                "0",
                "-g",
                "1",
                "-f",
                "h264",
                "pipe:1",
            ])
            .output()
            .expect("ffmpeg is available with the required libmpv dependency");
        assert!(
            encoded.status.success(),
            "ffmpeg failed: {}",
            String::from_utf8_lossy(&encoded.stderr)
        );
        let params = bitstream::parse_keyframe(Codec::H264, &encoded.stdout)
            .expect("encoded keyframe contains H.264 stream parameters");
        let body = bitstream::annex_b_to_length_prefixed(Codec::H264, &encoded.stdout);
        let share = LiveShare {
            room_id: RoomId(1),
            stream_id: StreamId(7),
            sender_name: "test".into(),
            codec: params.codec,
            coded_width: params.width,
            coded_height: params.height,
            extradata: params.extra_data,
        };
        let (mut daemon_stream, gui_stream) = UnixStream::pair().unwrap();
        let mpv = Arc::new(
            Mpv::with_initializer(|initializer| {
                initializer.set_option("vo", "null")?;
                initializer.set_option("profile", "low-latency")?;
                initializer.set_option("audio", "no")?;
                initializer.set_option("cache", "no")?;
                initializer.set_option("demuxer-thread", "yes")?;
                initializer.set_option("demuxer-lavf-format", "nut")?;
                initializer.set_option("demuxer-lavf-probe-info", "nostreams")?;
                initializer.set_option("demuxer-lavf-analyzeduration", "0")?;
                initializer.set_option("untimed", "yes")?;
                initializer.set_option("video-latency-hacks", "yes")?;
                initializer.set_option("vd-lavc-threads", "1")?;
                initializer.set_option("vd-lavc-low-latency", "yes")?;
                initializer.set_option("stream-buffer-size", "4k")?;
                Ok(())
            })
            .unwrap(),
        );
        mpv.request_log_messages("trace").unwrap();
        mpv.observe_property("time-pos", libmpv2::Format::Double, 88)
            .unwrap();
        let (control_sender, _control_receiver) = mpsc::channel();
        let (error_sender, error_receiver) = mpsc::channel();
        let (wakeup_sender, _wakeup_receiver) = async_channel::bounded(1);
        let _source = LiveStreamSource::start(
            mpv.clone(),
            share,
            gui_stream,
            Arc::new(LiveDiagnostics::new()),
            control_sender,
            error_sender,
            wakeup_sender,
            None,
        )
        .unwrap();
        mpv.command("loadfile", &["chatt-live://stream", "replace"])
            .unwrap();

        let mut wire = Vec::new();
        rpc::video::write_video_frame(&mut wire, 0, true, 7, &body);
        daemon_stream.write_all(&wire).unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut logs = String::new();
        let mut opened = false;
        while Instant::now() < deadline {
            match mpv.wait_event(0.1) {
                Some(Ok(Event::VideoReconfig | Event::PlaybackRestart)) => {
                    opened = true;
                    break;
                }
                Some(Ok(Event::LogMessage {
                    prefix,
                    level,
                    text,
                    ..
                })) => {
                    logs.push_str(&format!("[{prefix}/{level}] {text}"));
                }
                Some(Err(error)) => logs.push_str(&format!("[event error] {error}\n")),
                _ => {}
            }
        }
        assert!(opened, "libmpv did not open the live NUT stream:\n{logs}");

        wire.clear();
        rpc::video::write_video_frame(&mut wire, 30_000, true, 7, &body);
        daemon_stream.write_all(&wire).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut advanced = false;
        while Instant::now() < deadline {
            if mpv
                .get_property::<f64>("time-pos")
                .is_ok_and(|position| position >= 29.0)
            {
                advanced = true;
                break;
            }
            if let Some(Ok(Event::LogMessage {
                prefix,
                level,
                text,
                ..
            })) = mpv.wait_event(0.1)
            {
                logs.push_str(&format!("[{prefix}/{level}] {text}"));
            }
        }
        assert!(
            advanced,
            "libmpv waited for another packet instead of presenting the sparse update:\n{logs}"
        );
        assert!(
            error_receiver.try_recv().is_err(),
            "live socket reader reported an error"
        );
    }

    #[test]
    fn live_render_context_discards_the_newest_unconsumed_frame_when_source_closes() {
        let encoded = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=320x240:rate=30",
                "-frames:v",
                "1",
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-tune",
                "zerolatency",
                "-bf",
                "0",
                "-g",
                "1",
                "-f",
                "h264",
                "pipe:1",
            ])
            .output()
            .expect("ffmpeg is available with the required libmpv dependency");
        assert!(
            encoded.status.success(),
            "ffmpeg failed: {}",
            String::from_utf8_lossy(&encoded.stderr)
        );
        let params = bitstream::parse_keyframe(Codec::H264, &encoded.stdout)
            .expect("encoded keyframe contains H.264 stream parameters");
        let body = bitstream::annex_b_to_length_prefixed(Codec::H264, &encoded.stdout);
        let share = LiveShare {
            room_id: RoomId(1),
            stream_id: StreamId(8),
            sender_name: "test".into(),
            codec: params.codec,
            coded_width: params.width,
            coded_height: params.height,
            extradata: params.extra_data,
        };
        let (mut daemon_stream, gui_stream) = UnixStream::pair().unwrap();
        let mpv = Arc::new(
            Mpv::with_initializer(|initializer| {
                initializer.set_option("vo", "libmpv")?;
                initializer.set_option("profile", "low-latency")?;
                initializer.set_option("audio", "no")?;
                initializer.set_option("cache", "no")?;
                initializer.set_option("demuxer-thread", "yes")?;
                initializer.set_option("demuxer-readahead-secs", "0")?;
                initializer.set_option("demuxer-lavf-format", "nut")?;
                initializer.set_option("demuxer-lavf-probe-info", "nostreams")?;
                initializer.set_option("demuxer-lavf-analyzeduration", "0")?;
                initializer.set_option("untimed", "yes")?;
                initializer.set_option("video-latency-hacks", "yes")?;
                initializer.set_option("vd-lavc-threads", "1")?;
                initializer.set_option("vd-lavc-low-latency", "yes")?;
                initializer.set_option("stream-buffer-size", "4k")?;
                Ok(())
            })
            .unwrap(),
        );
        mpv.request_log_messages("trace").unwrap();
        let mut render = mpv.create_software_render_context(true).unwrap();
        render.set_update_callback(|| {});
        let (control_sender, _control_receiver) = mpsc::channel();
        let (error_sender, error_receiver) = mpsc::channel();
        let (wakeup_sender, _wakeup_receiver) = async_channel::bounded(1);
        let source = LiveStreamSource::start(
            mpv.clone(),
            share,
            gui_stream,
            Arc::new(LiveDiagnostics::new()),
            control_sender,
            error_sender,
            wakeup_sender,
            None,
        )
        .unwrap();
        mpv.command("loadfile", &["chatt-live://stream", "replace"])
            .unwrap();

        let mut wire = Vec::new();
        for timestamp_ms in 0..8 {
            rpc::video::write_video_frame(
                &mut wire,
                timestamp_ms * 1_000,
                true,
                8,
                &body,
            );
        }
        daemon_stream.write_all(&wire).unwrap();

        let deadline = Instant::now() + Duration::from_secs(1);
        let mut logs = String::new();
        let mut advanced = false;
        while Instant::now() < deadline {
            render.update();
            if mpv
                .get_property::<f64>("time-pos")
                .is_ok_and(|position| position >= 6.9)
            {
                advanced = true;
                break;
            }
            if let Some(Ok(Event::LogMessage {
                prefix,
                level,
                text,
                ..
            })) = mpv.wait_event(0.01)
            {
                logs.push_str(&format!("[{prefix}/{level}] {text}"));
            }
        }
        assert!(
            advanced,
            "render output held decoder progress behind unconsumed frames:\n{logs}"
        );
        assert!(
            render.next_frame_info().is_ok_and(|frame| frame.is_present()),
            "the newest frame was not retained for the render loop"
        );
        drop(source);
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut frame_discarded = false;
        while Instant::now() < deadline {
            render.update();
            if render
                .next_frame_info()
                .is_ok_and(|frame| !frame.is_present())
            {
                frame_discarded = true;
                break;
            }
            let _ = mpv.wait_event(0.01);
        }
        assert!(
            frame_discarded,
            "closing live video left a frame pending after its render configuration was cleared"
        );
        assert!(
            error_receiver.try_recv().is_err(),
            "live socket reader reported an error"
        );
    }
}
