use std::{
    collections::VecDeque,
    fs::{File, OpenOptions},
    io::{BufWriter, Read, Write},
    net::Shutdown,
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex, mpsc},
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
    inputs: VecDeque<(u64, u64)>,
    latest_input: Option<(u64, u64)>,
    latest_render: Option<(u64, u64)>,
    rendered_outputs: u64,
    input_queue_depth: usize,
    last_report: Option<Instant>,
}

impl LiveDiagnostics {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(LiveDiagnosticState::default()),
        }
    }

    fn record_input(&self, sequence: u64, pts_ms: u64, input_queue_depth: usize) {
        let mut state = self.state.lock().unwrap();
        state.inputs.push_back((sequence, pts_ms));
        if state.inputs.len() > LIVE_DIAGNOSTIC_HISTORY {
            state.inputs.pop_front();
        }
        state.latest_input = Some((sequence, pts_ms));
        state.input_queue_depth = input_queue_depth;
        Self::maybe_report(&mut state, false);
    }

    pub(crate) fn record_render(&self, pts_seconds: f64) {
        if !pts_seconds.is_finite() || pts_seconds < 0.0 {
            return;
        }
        let pts_ms = (pts_seconds * 1_000.0).round() as u64;
        let mut state = self.state.lock().unwrap();
        let sequence = state
            .inputs
            .iter()
            .min_by_key(|(_, input_pts)| input_pts.abs_diff(pts_ms))
            .map(|(sequence, _)| *sequence)
            .unwrap_or(0);
        state.latest_render = Some((sequence, pts_ms));
        state.rendered_outputs += 1;
        let first_render = state.rendered_outputs == 1;
        Self::maybe_report(&mut state, first_render);
    }

    fn maybe_report(state: &mut LiveDiagnosticState, force: bool) {
        let (Some((input_sequence, input_pts)), Some((render_sequence, render_pts))) =
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
        log::info!(
            "live latency input_frame={} input_pts_ms={} rendered_input_frame={} rendered_pts_ms={} lag_frames={} lag_ms={} render_outputs={} input_queue={}",
            input_sequence,
            input_pts,
            render_sequence,
            render_pts,
            input_sequence.saturating_sub(render_sequence),
            input_pts.saturating_sub(render_pts),
            state.rendered_outputs,
            state.input_queue_depth,
        );
    }
}

/// A self-contained capture of the decrypted video RPC boundary. The header
/// retains the decoder description and dimensions; every following record is
/// an untouched video RPC frame, including its source timestamp and key flag.
/// This deliberately records before the NUT bridge so playback experiments can
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

    fn write_frame(&mut self, received_at: Instant, frame: &[u8]) -> Result<()> {
        let received_ns = u64::try_from(received_at.duration_since(self.started).as_nanos())
            .context("live recording receive timestamp exceeds u64")?;
        self.output.write_all(&received_ns.to_le_bytes())?;
        self.output.write_all(frame)?;
        self.frames += 1;
        self.bytes += frame.len() as u64;
        Ok(())
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

struct NutQueue {
    stream_id: u32,
    header: Vec<u8>,
    state: Mutex<QueueState>,
    ready: Condvar,
}

#[derive(Default)]
struct QueueState {
    frames: VecDeque<Vec<u8>>,
    seen_keyframe: bool,
    awaiting_keyframe: bool,
    bootstrapping: bool,
    closed: bool,
    dropped_before_key: u64,
    overflow_count: u64,
}

struct NutCursor {
    queue: Arc<NutQueue>,
    header_offset: usize,
    current: Vec<u8>,
    current_offset: usize,
    logged_first_read: bool,
    logged_first_frame: bool,
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
        let mut prefix = [0u8; 4];
        match stream.read_exact(&mut prefix) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
                log::info!(
                    "live video socket closed stream_id={} frames={} bytes={} saw_keyframe={}",
                    expected_stream_id,
                    received_frames,
                    received_bytes,
                    saw_keyframe
                );
                return Ok(());
            }
            Err(error) => return Err(error).context("read live video frame length"),
        }
        let size = u32::from_le_bytes(prefix) as usize;
        if !(rpc::video::VIDEO_FRAME_HEADER_LEN..=rpc::video::MAX_VIDEO_FRAME_LEN).contains(&size) {
            bail!("invalid live video frame length {size}")
        }
        let mut frame = vec![0u8; size];
        frame[..4].copy_from_slice(&prefix);
        stream
            .read_exact(&mut frame[4..])
            .context("read live video frame")?;
        let received_at = Instant::now();
        let timestamp = i64::from_le_bytes(frame[4..12].try_into().unwrap());
        let is_key = frame[12] == 1;
        let stream_id = u32::from_le_bytes(frame[13..17].try_into().unwrap());
        if stream_id != expected_stream_id {
            bail!("live video frame belongs to stream {stream_id}, expected {expected_stream_id}")
        }
        if let Some(active) = recorder.as_mut()
            && let Err(error) = active.write_frame(received_at, &frame)
        {
            log::error!(
                "live video recording failed; disabling recording path={:?}: {error:#}",
                active.path
            );
            recorder = None;
        }
        let annex_b = bitstream::length_prefixed_to_annex_b(
            &frame[rpc::video::VIDEO_FRAME_HEADER_LEN..],
        )
        .map_err(|error| anyhow!(error))?;
        received_frames += 1;
        received_bytes += size as u64;
        if received_frames == 1 {
            log::info!(
                "live video first socket frame stream_id={} keyframe={} timestamp_ms={} frame_bytes={} bitstream_bytes={}",
                stream_id,
                is_key,
                timestamp,
                size,
                annex_b.len()
            );
        }
        if is_key && !saw_keyframe {
            saw_keyframe = true;
            log::info!(
                "live video first keyframe received stream_id={} frame_number={} timestamp_ms={} bitstream_bytes={}",
                stream_id,
                received_frames,
                timestamp,
                annex_b.len()
            );
        }
        let base = *base_timestamp.get_or_insert(timestamp);
        let pts = timestamp.saturating_sub(base).max(0) as u64;
        let initial_syncpoint = is_key && !wrote_initial_syncpoint;
        let caught_up = queue.push(
            nut_frame(pts, is_key, initial_syncpoint, &annex_b),
            is_key,
        );
        diagnostics.record_input(received_frames, pts, queue.pending_len());
        if initial_syncpoint {
            wrote_initial_syncpoint = true;
        }
        if caught_up {
            let _ = controls.send(ControlCommand::DropBuffers);
            mpv.wakeup();
        }
    }
}

impl NutQueue {
    fn push(&self, frame: Vec<u8>, is_key: bool) -> bool {
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
                return false;
            }
            state.seen_keyframe = true;
        }
        let mut caught_up = false;
        if state.awaiting_keyframe {
            if !is_key {
                return false;
            }
            state.frames.clear();
            state.awaiting_keyframe = false;
            caught_up = true;
            log::info!(
                "live video resumed at fresh keyframe stream_id={} overflow_count={}",
                self.stream_id,
                state.overflow_count
            );
        } else if state.frames.len()
            >= if state.bootstrapping {
                MAX_BOOTSTRAP_FRAMES
            } else {
                MAX_PENDING_FRAMES
            }
        {
            if is_key {
                state.frames.clear();
                caught_up = true;
            } else {
                state.awaiting_keyframe = true;
                state.bootstrapping = false;
                state.overflow_count += 1;
                log::warn!(
                    "live video decoder input queue full; waiting for keyframe stream_id={} pending_frames={} overflow_count={}",
                    self.stream_id,
                    state.frames.len(),
                    state.overflow_count
                );
                return false;
            }
        }
        state.frames.push_back(frame);
        self.ready.notify_one();
        caught_up
    }

    fn pop(&self) -> Option<Vec<u8>> {
        let mut state = self.state.lock().unwrap();
        loop {
            if let Some(frame) = state.frames.pop_front() {
                if state.frames.is_empty() {
                    state.bootstrapping = false;
                }
                return Some(frame);
            }
            if state.closed {
                return None;
            }
            state = self.ready.wait(state).unwrap();
        }
    }

    fn try_pop(&self) -> Option<Vec<u8>> {
        self.state.lock().unwrap().frames.pop_front()
    }

    fn pending_len(&self) -> usize {
        self.state.lock().unwrap().frames.len()
    }

    fn close(&self) {
        let mut state = self.state.lock().unwrap();
        state.closed = true;
        state.frames.clear();
        self.ready.notify_all();
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
        current: Vec::new(),
        current_offset: 0,
        logged_first_read: false,
        logged_first_frame: false,
    }
}

fn close_stream(cursor: Box<NutCursor>) {
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
        if cursor.current_offset == cursor.current.len() {
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
                    frame.len()
                );
            }
            cursor.current = frame;
            cursor.current_offset = 0;
        }
        let count = (output.len() - written).min(cursor.current.len() - cursor.current_offset);
        output[written..written + count]
            .copy_from_slice(&cursor.current[cursor.current_offset..cursor.current_offset + count]);
        cursor.current_offset += count;
        written += count;
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

fn nut_frame(pts: u64, is_key: bool, initial_syncpoint: bool, payload: &[u8]) -> Vec<u8> {
    let mut out = if initial_syncpoint {
        let mut sync = Vec::new();
        put_v(&mut sync, pts);
        put_v(&mut sync, 0);
        packet(SYNCPOINT_STARTCODE, &sync)
    } else {
        Vec::new()
    };
    let mut frame_header = vec![0];
    let flags = (if is_key { FLAG_KEY } else { 0 })
        | FLAG_CODED_PTS
        | FLAG_SIZE_MSB
        | FLAG_CHECKSUM;
    put_v(&mut frame_header, FLAG_CODED ^ flags);
    put_v(&mut frame_header, pts + (1 << PTS_SHIFT));
    put_v(&mut frame_header, payload.len() as u64);
    frame_header.extend_from_slice(&nut_crc(&frame_header).to_le_bytes());
    out.extend_from_slice(&frame_header);
    out.extend_from_slice(payload);
    out
}

fn packet(startcode: u64, payload: &[u8]) -> Vec<u8> {
    let mut prefix = startcode.to_be_bytes().to_vec();
    put_v(&mut prefix, payload.len() as u64 + 4);
    let mut out = prefix.clone();
    if payload.len() + 4 > 4096 {
        out.extend_from_slice(&nut_crc(&prefix).to_le_bytes());
    }
    out.extend_from_slice(payload);
    out.extend_from_slice(&nut_crc(payload).to_le_bytes());
    out
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
    fn queue_drops_to_the_next_keyframe() {
        let queue = NutQueue {
            stream_id: 1,
            header: Vec::new(),
            state: Mutex::new(QueueState::default()),
            ready: Condvar::new(),
        };
        assert!(!queue.push(vec![0], false));
        assert!(queue.state.lock().unwrap().frames.is_empty());
        assert!(!queue.push(vec![1], true));
        for marker in 0..MAX_PENDING_FRAMES {
            assert!(!queue.push(vec![marker as u8], false));
        }
        assert!(queue.push(vec![4], true));
        assert_eq!(queue.pop(), Some(vec![4]));
    }

    #[test]
    fn protocol_read_returns_a_complete_frame_without_waiting_for_the_next() {
        let queue = Arc::new(NutQueue {
            stream_id: 1,
            header: vec![1],
            state: Mutex::new(QueueState::default()),
            ready: Condvar::new(),
        });
        queue.push(vec![2, 3, 4], true);
        let mut cursor = open_stream(&mut queue.clone(), "chatt-live://test");
        let mut output = [0i8; 64];
        assert_eq!(read_stream(&mut cursor, &mut output), 4);
        assert_eq!(&output[..4], &[1, 2, 3, 4]);
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
    fn live_render_context_keeps_only_the_newest_unconsumed_frame() {
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
        let _source = LiveStreamSource::start(
            mpv.clone(),
            share,
            gui_stream,
            Arc::new(LiveDiagnostics::new()),
            control_sender,
            error_sender,
            wakeup_sender,
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
        render.skip_rendering().unwrap();
        assert!(
            error_receiver.try_recv().is_err(),
            "live socket reader reported an error"
        );
    }
}
