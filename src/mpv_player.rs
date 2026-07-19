use std::{
    ffi::CStr,
    os::raw::c_void,
    ptr,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread,
};

use anyhow::{Context as _, Result, anyhow, bail};
use async_channel::{Receiver as AsyncReceiver, Sender as AsyncSender, TrySendError};
use libmpv2::{
    Format, Mpv,
    events::{Event, PropertyData},
};
use libmpv2_sys as sys;

const FORMAT_BGR0: &[u8] = b"bgr0\0";

/// A libmpv client with dedicated control and render threads.
///
/// libmpv requires normal client API calls and `mpv_render_*` calls not to
/// share a thread. The render update callback wakes the render thread, which
/// acknowledges every update and only sends completed, non-repeated frames to
/// GPUI. GPUI therefore never blocks on decoding, scaling, or libmpv timing.
pub struct MpvPlayer {
    control_sender: mpsc::Sender<ControlCommand>,
    control_thread: Option<thread::JoinHandle<()>>,
    render_sender: mpsc::Sender<RenderMessage>,
    render_thread: Option<thread::JoinHandle<()>>,
    render_callback: Box<RenderCallback>,
    frames: AsyncReceiver<VideoFrame>,
    playback: Arc<SharedPlaybackState>,
    errors: mpsc::Receiver<String>,
    requested_paused: bool,
    mpv: Arc<Mpv>,
}

impl MpvPlayer {
    pub fn new(gpui_wakeup: AsyncSender<()>, width: usize, height: usize) -> Result<Self> {
        validate_surface_size(width, height)?;

        let mpv = Mpv::with_initializer(|initializer| {
            initializer.set_option("vo", "libmpv")?;
            initializer.set_option("keep-open", "no")?;
            initializer.set_option("idle", "yes")?;
            initializer.set_option("osc", "no")?;
            // The software render API otherwise defaults to high-quality zimg
            // scaling and dithering. These are the settings in mpv's sw-fast
            // profile and avoid spending more CPU scaling than decoding.
            initializer.set_option("sws-scaler", "bilinear")?;
            initializer.set_option("sws-fast", "yes")?;
            initializer.set_option("zimg-scaler", "bilinear")?;
            initializer.set_option("zimg-dither", "no")?;
            Ok(())
        })?;

        mpv.observe_property("time-pos", Format::Double, 1)?;
        mpv.observe_property("duration", Format::Double, 2)?;
        mpv.observe_property("pause", Format::Flag, 3)?;
        mpv.observe_property("eof-reached", Format::Flag, 4)?;
        mpv.observe_property("idle-active", Format::Flag, 5)?;

        let mut render_context = ptr::null_mut();
        let mut advanced_control = 1_i32;
        let mut params = [
            sys::mpv_render_param {
                type_: sys::mpv_render_param_type_MPV_RENDER_PARAM_API_TYPE,
                data: sys::MPV_RENDER_API_TYPE_SW.as_ptr() as *mut c_void,
            },
            sys::mpv_render_param {
                type_: sys::mpv_render_param_type_MPV_RENDER_PARAM_ADVANCED_CONTROL,
                data: (&mut advanced_control as *mut i32).cast(),
            },
            sys::mpv_render_param {
                type_: sys::mpv_render_param_type_MPV_RENDER_PARAM_INVALID,
                data: ptr::null_mut(),
            },
        ];

        // SAFETY: `mpv` is initialized and `params` is null-terminated. The
        // render thread frees this context before the control thread drops mpv.
        let code = unsafe {
            sys::mpv_render_context_create(
                &mut render_context,
                mpv.ctx.as_ptr(),
                params.as_mut_ptr(),
            )
        };
        check_mpv(code, "create software render context")?;

        let (render_sender, render_messages) = mpsc::channel();
        let render_pending = Arc::new(AtomicBool::new(false));
        let render_callback = Box::new(RenderCallback {
            sender: render_sender.clone(),
            pending: render_pending.clone(),
            stopping: AtomicBool::new(false),
        });
        // SAFETY: the boxed callback data has a stable address and outlives the
        // render context. The render thread disables the callback before exit.
        unsafe {
            sys::mpv_render_context_set_update_callback(
                render_context,
                Some(render_update),
                (&*render_callback as *const RenderCallback)
                    .cast_mut()
                    .cast(),
            );
        }

        let (frames_sender, frames) = async_channel::bounded(1);
        let frames_evictor = frames.clone();
        let (error_sender, errors) = mpsc::channel();
        let render_error_sender = error_sender.clone();
        let render_gpui_wakeup = gpui_wakeup.clone();
        let render_context = SendRenderContext(render_context);
        let render_thread = thread::Builder::new()
            .name("mpv-render".into())
            .spawn(move || {
                render_worker(
                    render_context,
                    render_pending,
                    render_messages,
                    frames_sender,
                    frames_evictor,
                    render_gpui_wakeup,
                    render_error_sender,
                    width,
                    height,
                );
            })
            .context("spawn mpv render thread")?;

        let mpv = Arc::new(mpv);
        let (control_sender, control_commands) = mpsc::channel();
        let playback = Arc::new(SharedPlaybackState::default());
        let control_playback = playback.clone();
        let control_mpv = mpv.clone();
        let control_thread = match thread::Builder::new()
            .name("mpv-control".into())
            .spawn(move || {
                control_worker(
                    control_mpv,
                    control_commands,
                    control_playback,
                    gpui_wakeup,
                    error_sender,
                );
            })
        {
            Ok(thread) => thread,
            Err(error) => {
                render_callback.stopping.store(true, Ordering::Release);
                let _ = render_sender.send(RenderMessage::Shutdown);
                let _ = render_thread.join();
                return Err(error).context("spawn mpv control thread");
            }
        };

        Ok(Self {
            control_sender,
            control_thread: Some(control_thread),
            render_sender,
            render_thread: Some(render_thread),
            render_callback,
            frames,
            playback,
            errors,
            requested_paused: false,
            mpv,
        })
    }

    pub fn load(&mut self, path: &str) -> Result<()> {
        self.playback.reset();
        self.requested_paused = false;
        self.render_sender
            .send(RenderMessage::Reset)
            .map_err(|_| anyhow!("mpv render thread stopped"))?;
        self.send_control(ControlCommand::Load(path.to_owned()))
    }

    pub fn toggle_pause(&mut self) -> Result<bool> {
        self.requested_paused = !self.requested_paused;
        let paused = self.requested_paused;
        self.playback.paused.store(paused, Ordering::Release);
        self.send_control(ControlCommand::SetPause(paused))?;
        Ok(paused)
    }

    pub fn seek_relative(&self, seconds: f64) -> Result<()> {
        self.send_control(ControlCommand::SeekRelative(seconds))
    }

    pub fn set_volume(&self, volume: f64) -> Result<()> {
        self.send_control(ControlCommand::SetVolume(volume.clamp(0.0, 100.0)))
    }

    /// Return the latest event state without making a normal libmpv call on
    /// GPUI's thread.
    pub fn drain_events(&mut self) -> Result<PlaybackState> {
        if let Ok(error) = self.errors.try_recv() {
            bail!(error);
        }
        Ok(self.playback.snapshot())
    }

    /// Take the newest completed frame. Older queued frames are discarded by
    /// the render worker when GPUI cannot upload them quickly enough.
    pub fn render_frame(&self) -> Option<VideoFrame> {
        let mut newest = None;
        while let Ok(frame) = self.frames.try_recv() {
            newest = Some(frame);
        }
        newest
    }

    fn send_control(&self, command: ControlCommand) -> Result<()> {
        self.control_sender
            .send(command)
            .map_err(|_| anyhow!("mpv control thread stopped"))?;
        // SAFETY: mpv_wakeup is explicitly safe on render API threads. The
        // control handle remains alive until after all callers and render work
        // have stopped in Drop.
        unsafe { sys::mpv_wakeup(self.mpv.ctx.as_ptr()) };
        Ok(())
    }
}

impl Drop for MpvPlayer {
    fn drop(&mut self) {
        self.render_callback.stopping.store(true, Ordering::Release);
        let _ = self.render_sender.send(RenderMessage::Shutdown);
        if let Some(thread) = self.render_thread.take() {
            let _ = thread.join();
        }

        let _ = self.control_sender.send(ControlCommand::Shutdown);
        // SAFETY: the control thread still owns this handle until it observes
        // Shutdown and exits.
        unsafe { sys::mpv_wakeup(self.mpv.ctx.as_ptr()) };
        if let Some(thread) = self.control_thread.take() {
            let _ = thread.join();
        }
    }
}

enum ControlCommand {
    Load(String),
    SetPause(bool),
    SeekRelative(f64),
    SetVolume(f64),
    Shutdown,
}

enum RenderMessage {
    Update,
    Reset,
    Shutdown,
}

struct RenderCallback {
    sender: mpsc::Sender<RenderMessage>,
    pending: Arc<AtomicBool>,
    stopping: AtomicBool,
}

struct SendRenderContext(*mut sys::mpv_render_context);

// SAFETY: libmpv permits moving a render context to a dedicated thread as long
// as all subsequent render API calls for it remain serialized on that thread.
unsafe impl Send for SendRenderContext {}

impl Drop for SendRenderContext {
    fn drop(&mut self) {
        if self.0.is_null() {
            return;
        }
        // SAFETY: ownership of this unique context moved with the wrapper. A
        // final update acknowledges any callback that raced with shutdown;
        // advanced-control contexts must not leave such an update pending.
        unsafe {
            sys::mpv_render_context_set_update_callback(self.0, None, ptr::null_mut());
            let updates = sys::mpv_render_context_update(self.0);
            if updates & u64::from(sys::mpv_render_update_flag_MPV_RENDER_UPDATE_FRAME) != 0 {
                let mut skip = 1_i32;
                let mut params = [
                    sys::mpv_render_param {
                        type_: sys::mpv_render_param_type_MPV_RENDER_PARAM_SKIP_RENDERING,
                        data: (&mut skip as *mut i32).cast(),
                    },
                    sys::mpv_render_param {
                        type_: sys::mpv_render_param_type_MPV_RENDER_PARAM_INVALID,
                        data: ptr::null_mut(),
                    },
                ];
                let _ = sys::mpv_render_context_render(self.0, params.as_mut_ptr());
            }
            sys::mpv_render_context_free(self.0);
        }
        self.0 = ptr::null_mut();
    }
}

unsafe extern "C" fn render_update(context: *mut c_void) {
    let Some(callback) = (unsafe { (context as *const RenderCallback).as_ref() }) else {
        return;
    };
    if callback.stopping.load(Ordering::Acquire) {
        return;
    }
    if !callback.pending.swap(true, Ordering::AcqRel) {
        let _ = callback.sender.send(RenderMessage::Update);
    }
}

fn control_worker(
    mpv: Arc<Mpv>,
    commands: mpsc::Receiver<ControlCommand>,
    playback: Arc<SharedPlaybackState>,
    gpui_wakeup: AsyncSender<()>,
    errors: mpsc::Sender<String>,
) {
    let mut state = PlaybackState::default();
    loop {
        while let Ok(command) = commands.try_recv() {
            let result = match command {
                ControlCommand::Load(path) => {
                    state = PlaybackState::default();
                    playback.publish(state);
                    mpv.command("loadfile", &[&path, "replace"])
                }
                ControlCommand::SetPause(paused) => mpv.set_property("pause", paused),
                ControlCommand::SeekRelative(seconds) => {
                    mpv.command("seek", &[&seconds.to_string(), "relative+exact"])
                }
                ControlCommand::SetVolume(volume) => mpv.set_property("volume", volume),
                ControlCommand::Shutdown => return,
            };
            if let Err(error) = result {
                let _ = errors.send(error.to_string());
                let _ = gpui_wakeup.try_send(());
            }
        }

        if let Some(event) = mpv.wait_event(-1.0) {
            let notify_gpui = match apply_event(event, &mut state) {
                Ok(notify_gpui) => notify_gpui,
                Err(error) => {
                    let _ = errors.send(error.to_string());
                    true
                }
            };
            playback.publish(state);
            if notify_gpui {
                let _ = gpui_wakeup.try_send(());
            }
        }
    }
}

fn apply_event(event: libmpv2::Result<Event<'_>>, state: &mut PlaybackState) -> Result<bool> {
    let notify_gpui = match event? {
        Event::PropertyChange { name, change, .. } => match (name, change) {
            ("time-pos", PropertyData::Double(value)) => {
                state.position = value.max(0.0);
                false
            }
            ("duration", PropertyData::Double(value)) => {
                state.duration = value.max(0.0);
                true
            }
            ("pause", PropertyData::Flag(value)) => {
                state.paused = value;
                true
            }
            ("eof-reached", PropertyData::Flag(value)) => {
                state.finished |= value;
                value
            }
            ("idle-active", PropertyData::Flag(value)) => {
                let finished = value && state.duration > 0.0;
                state.finished |= finished;
                finished
            }
            _ => false,
        },
        Event::EndFile(_) | Event::Shutdown => {
            state.finished = true;
            true
        }
        _ => false,
    };
    Ok(notify_gpui)
}

fn render_worker(
    render_context: SendRenderContext,
    pending: Arc<AtomicBool>,
    messages: mpsc::Receiver<RenderMessage>,
    frames: AsyncSender<VideoFrame>,
    frames_evictor: AsyncReceiver<VideoFrame>,
    gpui_wakeup: AsyncSender<()>,
    errors: mpsc::Sender<String>,
    width: usize,
    height: usize,
) {
    let render_context_ptr = render_context.0;
    let mut surface = AlignedSurface::new(0);
    let mut has_frame = false;

    while let Ok(message) = messages.recv() {
        match message {
            RenderMessage::Update => {
                // Clear before acknowledging the update. A callback racing
                // with update() will set this again and enqueue another wake.
                pending.store(false, Ordering::Release);
                match render_pending_frame(
                    render_context_ptr,
                    &mut surface,
                    width,
                    height,
                    has_frame,
                ) {
                    Ok(RenderResult::Frame(frame)) => {
                        has_frame = true;
                        send_latest_frame(&frames, &frames_evictor, frame);
                        let _ = gpui_wakeup.try_send(());
                    }
                    Ok(RenderResult::Repeated) | Ok(RenderResult::NoFrame) => {}
                    Err(error) => {
                        let _ = errors.send(error.to_string());
                        let _ = gpui_wakeup.try_send(());
                    }
                }
            }
            RenderMessage::Reset => {
                has_frame = false;
                while frames_evictor.try_recv().is_ok() {}
            }
            RenderMessage::Shutdown => break,
        }
    }

    // `SendRenderContext` disables the callback and frees the context here.
}

enum RenderResult {
    Frame(VideoFrame),
    Repeated,
    NoFrame,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RenderAction {
    None,
    Render,
    Skip,
}

fn render_action(updates: u64, frame_flags: u64, has_frame: bool) -> RenderAction {
    if updates & u64::from(sys::mpv_render_update_flag_MPV_RENDER_UPDATE_FRAME) == 0 {
        return RenderAction::None;
    }
    let repeat = frame_flags
        & u64::from(sys::mpv_render_frame_info_flag_MPV_RENDER_FRAME_INFO_REPEAT)
        != 0;
    let redraw = frame_flags
        & u64::from(sys::mpv_render_frame_info_flag_MPV_RENDER_FRAME_INFO_REDRAW)
        != 0;
    if repeat && !redraw && has_frame {
        RenderAction::Skip
    } else {
        RenderAction::Render
    }
}

fn render_pending_frame(
    render_context: *mut sys::mpv_render_context,
    surface: &mut AlignedSurface,
    width: usize,
    height: usize,
    has_frame: bool,
) -> Result<RenderResult> {
    // SAFETY: every render API call for this context occurs on this worker.
    let updates = unsafe { sys::mpv_render_context_update(render_context) };
    let mut info = sys::mpv_render_frame_info {
        flags: 0,
        target_time: 0,
    };
    if updates & u64::from(sys::mpv_render_update_flag_MPV_RENDER_UPDATE_FRAME) != 0 {
        let param = sys::mpv_render_param {
            type_: sys::mpv_render_param_type_MPV_RENDER_PARAM_NEXT_FRAME_INFO,
            data: (&mut info as *mut sys::mpv_render_frame_info).cast(),
        };
        // NEXT_FRAME_INFO is an optimization. Older runtime libmpv versions
        // may not implement it, in which case a normal render is still valid.
        let _ = unsafe { sys::mpv_render_context_get_info(render_context, param) };
    }

    match render_action(updates, info.flags, has_frame) {
        RenderAction::None => Ok(RenderResult::NoFrame),
        RenderAction::Skip => {
            let mut skip = 1_i32;
            let mut params = [
                sys::mpv_render_param {
                    type_: sys::mpv_render_param_type_MPV_RENDER_PARAM_SKIP_RENDERING,
                    data: (&mut skip as *mut i32).cast(),
                },
                sys::mpv_render_param {
                    type_: sys::mpv_render_param_type_MPV_RENDER_PARAM_INVALID,
                    data: ptr::null_mut(),
                },
            ];
            let code = unsafe {
                sys::mpv_render_context_render(render_context, params.as_mut_ptr())
            };
            check_mpv(code, "acknowledge repeated video frame")?;
            Ok(RenderResult::Repeated)
        }
        RenderAction::Render => {
            let stride = width * 4;
            surface.resize(stride * height);
            let mut size = [width as i32, height as i32];
            let mut stride_value = stride;
            let mut params = [
                sys::mpv_render_param {
                    type_: sys::mpv_render_param_type_MPV_RENDER_PARAM_SW_SIZE,
                    data: size.as_mut_ptr().cast(),
                },
                sys::mpv_render_param {
                    type_: sys::mpv_render_param_type_MPV_RENDER_PARAM_SW_FORMAT,
                    data: FORMAT_BGR0.as_ptr() as *mut c_void,
                },
                sys::mpv_render_param {
                    type_: sys::mpv_render_param_type_MPV_RENDER_PARAM_SW_STRIDE,
                    data: (&mut stride_value as *mut usize).cast(),
                },
                sys::mpv_render_param {
                    type_: sys::mpv_render_param_type_MPV_RENDER_PARAM_SW_POINTER,
                    data: surface.as_mut_ptr().cast(),
                },
                sys::mpv_render_param {
                    type_: sys::mpv_render_param_type_MPV_RENDER_PARAM_INVALID,
                    data: ptr::null_mut(),
                },
            ];
            let code = unsafe {
                sys::mpv_render_context_render(render_context, params.as_mut_ptr())
            };
            check_mpv(code, "render video frame")?;

            let mut pixels = surface.as_bytes().to_vec();
            for alpha in pixels.iter_mut().skip(3).step_by(4) {
                *alpha = 255;
            }
            Ok(RenderResult::Frame(VideoFrame {
                width: width as u32,
                height: height as u32,
                pixels,
            }))
        }
    }
}

fn send_latest_frame(
    frames: &AsyncSender<VideoFrame>,
    evictor: &AsyncReceiver<VideoFrame>,
    frame: VideoFrame,
) {
    match frames.try_send(frame) {
        Ok(()) => {}
        Err(TrySendError::Full(frame)) => {
            let _ = evictor.try_recv();
            let _ = frames.try_send(frame);
        }
        Err(TrySendError::Closed(_)) => {}
    }
}

fn validate_surface_size(width: usize, height: usize) -> Result<()> {
    if width == 0 || height == 0 {
        bail!("software render surface must not be empty");
    }
    let stride = width
        .checked_mul(4)
        .ok_or_else(|| anyhow!("video frame width is too large"))?;
    if stride % 64 != 0 {
        bail!("software render width must be a multiple of 16 pixels");
    }
    stride
        .checked_mul(height)
        .ok_or_else(|| anyhow!("video frame is too large"))?;
    Ok(())
}

#[derive(Clone, Copy, Debug, Default)]
pub struct PlaybackState {
    pub position: f64,
    pub duration: f64,
    pub paused: bool,
    pub finished: bool,
}

#[derive(Default)]
struct SharedPlaybackState {
    position: AtomicU64,
    duration: AtomicU64,
    paused: AtomicBool,
    finished: AtomicBool,
}

impl SharedPlaybackState {
    fn publish(&self, state: PlaybackState) {
        self.position.store(state.position.to_bits(), Ordering::Relaxed);
        self.duration.store(state.duration.to_bits(), Ordering::Relaxed);
        self.paused.store(state.paused, Ordering::Relaxed);
        self.finished.store(state.finished, Ordering::Release);
    }

    fn snapshot(&self) -> PlaybackState {
        let finished = self.finished.load(Ordering::Acquire);
        PlaybackState {
            position: f64::from_bits(self.position.load(Ordering::Relaxed)),
            duration: f64::from_bits(self.duration.load(Ordering::Relaxed)),
            paused: self.paused.load(Ordering::Relaxed),
            finished,
        }
    }

    fn reset(&self) {
        self.publish(PlaybackState::default());
    }
}

pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

#[repr(align(64))]
#[derive(Clone)]
struct AlignedBlock {
    _bytes: [u8; 64],
}

struct AlignedSurface {
    blocks: Vec<AlignedBlock>,
    len: usize,
}

impl AlignedSurface {
    fn new(len: usize) -> Self {
        debug_assert_eq!(len % 64, 0);
        Self {
            blocks: vec![AlignedBlock { _bytes: [0; 64] }; len / 64],
            len,
        }
    }

    fn as_mut_ptr(&mut self) -> *mut u8 {
        self.blocks.as_mut_ptr().cast()
    }

    fn resize(&mut self, len: usize) {
        debug_assert_eq!(len % 64, 0);
        self.blocks
            .resize(len / 64, AlignedBlock { _bytes: [0; 64] });
        self.len = len;
    }

    fn as_bytes(&self) -> &[u8] {
        // SAFETY: AlignedBlock has no padding and `len` is exactly the byte
        // length represented by the block allocation.
        unsafe { std::slice::from_raw_parts(self.blocks.as_ptr().cast(), self.len) }
    }
}

fn check_mpv(code: i32, operation: &str) -> Result<()> {
    if code >= 0 {
        return Ok(());
    }

    // SAFETY: libmpv returns a static, null-terminated error description.
    let description = unsafe { CStr::from_ptr(sys::mpv_error_string(code)) }.to_string_lossy();
    bail!("failed to {operation}: {description} ({code})")
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME_UPDATE: u64 =
        sys::mpv_render_update_flag_MPV_RENDER_UPDATE_FRAME as u64;
    const REPEAT: u64 = sys::mpv_render_frame_info_flag_MPV_RENDER_FRAME_INFO_REPEAT as u64;
    const REDRAW: u64 = sys::mpv_render_frame_info_flag_MPV_RENDER_FRAME_INFO_REDRAW as u64;

    #[test]
    fn ignores_callback_without_frame_update() {
        assert_eq!(render_action(0, 0, true), RenderAction::None);
    }

    #[test]
    fn skips_exact_repeat_when_previous_frame_exists() {
        assert_eq!(render_action(FRAME_UPDATE, REPEAT, true), RenderAction::Skip);
    }

    #[test]
    fn renders_repeat_when_surface_has_no_previous_frame() {
        assert_eq!(render_action(FRAME_UPDATE, REPEAT, false), RenderAction::Render);
    }

    #[test]
    fn redraw_is_not_discarded_as_repeat() {
        assert_eq!(
            render_action(FRAME_UPDATE, REPEAT | REDRAW, true),
            RenderAction::Render
        );
    }
}
