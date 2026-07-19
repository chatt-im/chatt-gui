use std::{
    ffi::{CStr, CString},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, anyhow, bail};
use ash::vk;
use async_channel::Sender as AsyncSender;
use gpui::PlatformSurface;
use gpui_wgpu::{
    VideoTextureGeneration, VulkanVideoTexture, WgpuVideoSurface,
    wgpu::hal::vulkan::QueueHostLock,
};
use libmpv2::{
    Format, Mpv,
    events::{Event, PropertyData},
    render::{
        RenderContext, SoftwareRenderTarget, VulkanFeatures, VulkanInitParams, VulkanQueueFamily,
        VulkanQueueLock, VulkanRenderTarget, mpv_render_update,
    },
};

const FORMAT_RGBA: &CStr = c"rgba";
const RENDER_SUMMARY_INTERVAL: Duration = Duration::from_secs(10);
const RENDER_PRESSURE_LOG_INTERVAL: Duration = Duration::from_secs(5);

/// A libmpv client with dedicated control and render threads.
pub struct MpvPlayer {
    control_sender: mpsc::Sender<ControlCommand>,
    control_thread: Option<thread::JoinHandle<()>>,
    render_sender: mpsc::Sender<RenderMessage>,
    render_thread: Option<thread::JoinHandle<()>>,
    render_stopping: Arc<AtomicBool>,
    playback: Arc<SharedPlaybackState>,
    errors: mpsc::Receiver<String>,
    requested_paused: bool,
    surface: PlatformSurface,
    mpv: Arc<Mpv>,
}

impl MpvPlayer {
    pub fn new(gpui_wakeup: AsyncSender<()>) -> Result<Self> {
        let mpv = Arc::new(Mpv::with_initializer(|initializer| {
            initializer.set_option("vo", "libmpv")?;
            initializer.set_option("keep-open", "no")?;
            initializer.set_option("idle", "yes")?;
            initializer.set_option("osc", "no")?;
            initializer.set_option("profile", "gpu-hq")?;
            initializer.set_option("hwdec", "vulkan,auto-safe")?;
            initializer.set_option("sws-allow-zimg", "no")?;
            initializer.set_option("sws-scaler", "bilinear")?;
            initializer.set_option("sws-fast", "yes")?;
            Ok(())
        })?);

        mpv.observe_property("time-pos", Format::Double, 1)?;
        mpv.observe_property("duration", Format::Double, 2)?;
        mpv.observe_property("pause", Format::Flag, 3)?;
        mpv.observe_property("eof-reached", Format::Flag, 4)?;
        mpv.observe_property("idle-active", Format::Flag, 5)?;
        mpv.observe_property("hwdec-current", Format::String, 6)?;
        mpv.observe_property("video-codec", Format::String, 7)?;
        mpv.observe_property("current-vo", Format::String, 8)?;
        mpv.observe_property("hwdec-interop", Format::String, 9)?;

        let mpv_log_level = std::env::var("CHATT_MPV_LOG").unwrap_or_else(|_| "warn".into());
        mpv.request_log_messages(&mpv_log_level)
            .with_context(|| format!("request native mpv log level {mpv_log_level:?}"))?;
        log::info!("native mpv logging enabled min_level={mpv_log_level:?}");

        let (render_sender, render_messages) = mpsc::channel();
        let resize_sender = render_sender.clone();
        let video_surface = WgpuVideoSurface::new(move |width, height| {
            let _ = resize_sender.send(RenderMessage::Resize { width, height });
        })?;
        let surface = video_surface.platform_surface();

        let mut backend = match create_vulkan_context(&mpv, &video_surface) {
            Ok(context) => {
                log::info!("video render backend selected backend=vulkan sharing=wgpu-device");
                RenderBackend::Vulkan {
                    context,
                    generation: None,
                    next_texture: 0,
                }
            }
            Err(error) => {
                log::warn!("Vulkan libmpv interop unavailable, using software fallback: {error:#}");
                let context = mpv
                    .create_software_render_context()
                    .context("create libmpv software render context after Vulkan fallback")?;
                log::info!("video render backend selected backend=software upload=wgpu");
                RenderBackend::Software {
                    context,
                    generation: None,
                    next_texture: 0,
                    aligned: Vec::new(),
                    tight: Vec::new(),
                }
            }
        };

        let render_pending = Arc::new(AtomicBool::new(false));
        let render_stopping = Arc::new(AtomicBool::new(false));
        backend.context_mut().set_update_callback({
            let sender = render_sender.clone();
            let pending = render_pending.clone();
            let stopping = render_stopping.clone();
            move || {
                if stopping.load(Ordering::Acquire) {
                    return;
                }
                if !pending.swap(true, Ordering::AcqRel) {
                    let _ = sender.send(RenderMessage::Update);
                }
            }
        });

        let (error_sender, errors) = mpsc::channel();
        let render_error_sender = error_sender.clone();
        let render_gpui_wakeup = gpui_wakeup.clone();
        let render_thread = thread::Builder::new()
            .name("mpv-render".into())
            .spawn(move || {
                render_worker(
                    backend,
                    video_surface,
                    render_pending,
                    render_messages,
                    render_gpui_wakeup,
                    render_error_sender,
                );
            })
            .context("spawn mpv render thread")?;

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
                render_stopping.store(true, Ordering::Release);
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
            render_stopping,
            playback,
            errors,
            requested_paused: false,
            surface,
            mpv,
        })
    }

    pub fn surface(&self) -> PlatformSurface {
        self.surface.clone()
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

    pub fn drain_events(&mut self) -> Result<PlaybackState> {
        if let Ok(error) = self.errors.try_recv() {
            bail!(error);
        }
        Ok(self.playback.snapshot())
    }

    fn send_control(&self, command: ControlCommand) -> Result<()> {
        self.control_sender
            .send(command)
            .map_err(|_| anyhow!("mpv control thread stopped"))?;
        self.mpv.wakeup();
        Ok(())
    }
}

impl Drop for MpvPlayer {
    fn drop(&mut self) {
        log::debug!("stopping mpv player threads");
        self.render_stopping.store(true, Ordering::Release);
        let _ = self.render_sender.send(RenderMessage::Shutdown);
        if let Some(thread) = self.render_thread.take() {
            let _ = thread.join();
        }

        let _ = self.control_sender.send(ControlCommand::Shutdown);
        self.mpv.wakeup();
        if let Some(thread) = self.control_thread.take() {
            let _ = thread.join();
        }
        log::debug!("mpv player threads stopped");
    }
}

struct WgpuQueueLock(Arc<QueueHostLock>);

impl VulkanQueueLock for WgpuQueueLock {
    fn lock(&self, _family: u32, _index: u32) {
        self.0.lock();
    }

    unsafe fn unlock(&self, _family: u32, _index: u32) {
        unsafe { self.0.unlock() };
    }
}

fn create_vulkan_context(mpv: &Arc<Mpv>, surface: &WgpuVideoSurface) -> Result<RenderContext> {
    let native = surface.vulkan_device()?;
    let has_external_memory_fd = native
        .device_extensions
        .iter()
        .any(|extension| *extension == c"VK_KHR_external_memory_fd");
    let has_dma_buf = native
        .device_extensions
        .iter()
        .any(|extension| *extension == c"VK_EXT_external_memory_dma_buf");
    let has_drm_modifiers = native
        .device_extensions
        .iter()
        .any(|extension| *extension == c"VK_EXT_image_drm_format_modifier");
    log::info!(
        "importing GPUI Vulkan device into libmpv queue_family={} queue_index={} instance_extensions={} device_extensions={} external_memory_fd={} dma_buf={} drm_modifiers={}",
        native.queue_family,
        native.queue_index,
        native.instance_extensions.len(),
        native.device_extensions.len(),
        has_external_memory_fd,
        has_dma_buf,
        has_drm_modifiers,
    );
    if cfg!(target_os = "linux")
        && !(has_external_memory_fd && has_dma_buf && has_drm_modifiers)
    {
        log::warn!(
            "Vulkan device lacks Linux dma-buf import extensions; hardware-decoded video may round-trip through CPU memory"
        );
    }
    let instance_extensions = native
        .instance_extensions
        .iter()
        .map(|extension| CString::new(extension.to_bytes()))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let device_extensions = native
        .device_extensions
        .iter()
        .map(|extension| CString::new(extension.to_bytes()))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let queue = VulkanQueueFamily {
        index: native.queue_family,
        count: 1,
    };
    mpv.create_vulkan_render_context(VulkanInitParams {
        instance: native.instance.handle(),
        physical_device: native.physical_device,
        device: native.device.handle(),
        get_proc_address: native.get_proc_address,
        instance_extensions,
        device_extensions,
        features: VulkanFeatures {
            core: vk::PhysicalDeviceFeatures::default(),
            timeline_semaphore: true,
            host_query_reset: true,
        },
        graphics_queue: queue,
        compute_queue: queue,
        transfer_queue: queue,
        enabled_queue_families: vec![queue],
        queue_lock: Arc::new(WgpuQueueLock(native.queue_lock)),
    })
    .context("import GPUI's Vulkan device into libmpv")
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
    Resize { width: u32, height: u32 },
    Reset,
    Shutdown,
}

enum RenderBackend {
    Vulkan {
        context: RenderContext,
        generation: Option<VideoTextureGeneration>,
        next_texture: usize,
    },
    Software {
        context: RenderContext,
        generation: Option<VideoTextureGeneration>,
        next_texture: usize,
        aligned: Vec<u8>,
        tight: Vec<u8>,
    },
}

impl RenderBackend {
    fn name(&self) -> &'static str {
        match self {
            Self::Vulkan { .. } => "vulkan",
            Self::Software { .. } => "software",
        }
    }

    fn context(&self) -> &RenderContext {
        match self {
            Self::Vulkan { context, .. } | Self::Software { context, .. } => context,
        }
    }

    fn context_mut(&mut self) -> &mut RenderContext {
        match self {
            Self::Vulkan { context, .. } | Self::Software { context, .. } => context,
        }
    }

    fn resize(&mut self, surface: &WgpuVideoSurface, width: u32, height: u32) -> Result<()> {
        match self {
            Self::Vulkan {
                context,
                generation,
                next_texture,
            } => {
                if generation
                    .as_ref()
                    .and_then(|generation| generation.textures.first())
                    .is_some_and(|texture| texture.width() == width && texture.height() == height)
                {
                    return Ok(());
                }
                if let Some(old) = generation.take() {
                    log::debug!(
                        "retiring video texture generation backend=vulkan generation={} textures={}",
                        old.id,
                        old.textures.len(),
                    );
                    for texture in &old.textures {
                        context
                            .remove_vulkan_target(texture.image())
                            .context("remove Vulkan render target from libmpv")?;
                    }
                    surface
                        .wait_idle()
                        .context("wait before retiring Vulkan video textures")?;
                }
                let new_generation = surface.allocate_generation(width, height)?;
                log::info!(
                    "video texture generation ready backend=vulkan generation={} size={}x{} textures={}",
                    new_generation.id,
                    width,
                    height,
                    new_generation.textures.len(),
                );
                *generation = Some(new_generation);
                *next_texture = 0;
            }
            Self::Software {
                generation,
                next_texture,
                ..
            } => {
                if generation
                    .as_ref()
                    .and_then(|generation| generation.textures.first())
                    .is_some_and(|texture| texture.width() == width && texture.height() == height)
                {
                    return Ok(());
                }
                if generation.take().is_some() {
                    surface
                        .wait_idle()
                        .context("wait before retiring software-upload video textures")?;
                }
                let new_generation = surface.allocate_generation(width, height)?;
                log::info!(
                    "video texture generation ready backend=software generation={} size={}x{} textures={}",
                    new_generation.id,
                    width,
                    height,
                    new_generation.textures.len(),
                );
                *generation = Some(new_generation);
                *next_texture = 0;
            }
        }
        Ok(())
    }

    fn render(
        &mut self,
        surface: &WgpuVideoSurface,
        acknowledge_if_busy: bool,
        diagnostics: &mut RenderDiagnostics,
    ) -> Result<bool> {
        match self {
            Self::Vulkan {
                context,
                generation,
                next_texture,
            } => {
                let Some(generation) = generation else {
                    diagnostics.note_unconfigured();
                    if acknowledge_if_busy {
                        context.skip_rendering()?;
                    }
                    return Ok(false);
                };
                let Some((texture, sync)) = next_ring_texture(generation, next_texture) else {
                    diagnostics.note_ring_busy("vulkan", generation.id);
                    if acknowledge_if_busy {
                        context.skip_rendering()?;
                    }
                    return Ok(false);
                };
                let render = context.render_vulkan(VulkanRenderTarget {
                    image: texture.image(),
                    format: vk::Format::R8G8B8A8_UNORM,
                    usage: vk::ImageUsageFlags::SAMPLED
                        | vk::ImageUsageFlags::COLOR_ATTACHMENT
                        | vk::ImageUsageFlags::TRANSFER_SRC
                        | vk::ImageUsageFlags::TRANSFER_DST,
                    width: texture.width(),
                    height: texture.height(),
                    input_layout: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    output_layout: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    wait_semaphore: texture.semaphore(),
                    wait_value: sync.wait_value,
                    signal_semaphore: texture.semaphore(),
                    signal_value: sync.ready_value,
                });
                if let Err(error) = render {
                    texture.cancel_render();
                    return Err(error).context("render libmpv frame into Vulkan texture");
                }
                surface.publish(texture, sync);
                Ok(true)
            }
            Self::Software {
                context,
                generation,
                next_texture,
                aligned,
                tight,
            } => {
                let Some(generation) = generation else {
                    diagnostics.note_unconfigured();
                    if acknowledge_if_busy {
                        context.skip_rendering()?;
                    }
                    return Ok(false);
                };
                let Some((texture, _sync)) = next_ring_texture(generation, next_texture) else {
                    diagnostics.note_ring_busy("software", generation.id);
                    if acknowledge_if_busy {
                        context.skip_rendering()?;
                    }
                    return Ok(false);
                };
                let row_bytes = usize::try_from(texture.width())?
                    .checked_mul(4)
                    .ok_or_else(|| anyhow!("video row is too large"))?;
                let stride = row_bytes.next_multiple_of(64);
                let height = texture.height() as usize;
                aligned.resize(
                    stride
                        .checked_mul(height)
                        .ok_or_else(|| anyhow!("video frame is too large"))?,
                    0,
                );
                let render = context.render_software(SoftwareRenderTarget {
                    width: texture.width(),
                    height: texture.height(),
                    format: FORMAT_RGBA,
                    stride,
                    pixels: aligned,
                });
                if let Err(error) = render {
                    texture.cancel_render();
                    return Err(error).context("render libmpv frame into software buffer");
                }
                tight.resize(row_bytes * height, 0);
                for row in 0..height {
                    tight[row * row_bytes..(row + 1) * row_bytes]
                        .copy_from_slice(&aligned[row * stride..row * stride + row_bytes]);
                }
                if let Err(error) = surface.publish_rgba(texture.clone(), tight, row_bytes as u32) {
                    texture.cancel_render();
                    return Err(error);
                }
                Ok(true)
            }
        }
    }
}

fn next_ring_texture(
    generation: &VideoTextureGeneration,
    next_texture: &mut usize,
) -> Option<(Arc<VulkanVideoTexture>, gpui_wgpu::VulkanTextureSync)> {
    for _ in 0..generation.textures.len() {
        let texture = generation.textures[*next_texture].clone();
        *next_texture = (*next_texture + 1) % generation.textures.len();
        if let Some(sync) = texture.try_begin_render() {
            return Some((texture, sync));
        }
    }
    None
}

struct RenderDiagnostics {
    started_at: Instant,
    last_summary: Instant,
    last_pressure_log: Option<Instant>,
    callbacks: u64,
    rendered: u64,
    repeats: u64,
    callbacks_without_frames: u64,
    unconfigured: u64,
    ring_busy: u64,
    resizes: u64,
    errors: u64,
}

impl RenderDiagnostics {
    fn new() -> Self {
        let now = Instant::now();
        Self {
            started_at: now,
            last_summary: now,
            last_pressure_log: None,
            callbacks: 0,
            rendered: 0,
            repeats: 0,
            callbacks_without_frames: 0,
            unconfigured: 0,
            ring_busy: 0,
            resizes: 0,
            errors: 0,
        }
    }

    fn note_unconfigured(&mut self) {
        self.unconfigured += 1;
        log::trace!("video frame arrived before the surface had a texture generation");
    }

    fn note_ring_busy(&mut self, backend: &str, generation: u64) {
        self.ring_busy += 1;
        let now = Instant::now();
        if self.last_pressure_log.is_none_or(|last| {
            now.duration_since(last) >= RENDER_PRESSURE_LOG_INTERVAL
        }) {
            self.last_pressure_log = Some(now);
            log::warn!(
                "all video textures are still in use; dropping render request backend={backend} generation={generation} busy_total={}",
                self.ring_busy,
            );
        }
    }

    fn maybe_log_summary(&mut self, backend: &str) {
        let now = Instant::now();
        if now.duration_since(self.last_summary) < RENDER_SUMMARY_INTERVAL {
            return;
        }
        self.last_summary = now;
        log::debug!(
            "video render summary backend={backend} uptime_ms={} callbacks={} rendered={} repeats={} no_frame={} unconfigured={} ring_busy={} resizes={} errors={}",
            now.duration_since(self.started_at).as_millis(),
            self.callbacks,
            self.rendered,
            self.repeats,
            self.callbacks_without_frames,
            self.unconfigured,
            self.ring_busy,
            self.resizes,
            self.errors,
        );
    }

    fn log_final(&self, backend: &str) {
        log::info!(
            "video render worker stopped backend={backend} uptime_ms={} callbacks={} rendered={} repeats={} no_frame={} unconfigured={} ring_busy={} resizes={} errors={}",
            self.started_at.elapsed().as_millis(),
            self.callbacks,
            self.rendered,
            self.repeats,
            self.callbacks_without_frames,
            self.unconfigured,
            self.ring_busy,
            self.resizes,
            self.errors,
        );
    }
}

fn control_worker(
    mpv: Arc<Mpv>,
    commands: mpsc::Receiver<ControlCommand>,
    playback: Arc<SharedPlaybackState>,
    gpui_wakeup: AsyncSender<()>,
    errors: mpsc::Sender<String>,
) {
    log::info!("mpv control worker started");
    let mut state = PlaybackState::default();
    loop {
        while let Ok(command) = commands.try_recv() {
            let result = match command {
                ControlCommand::Load(path) => {
                    log::info!("loading media path={path:?}");
                    state = PlaybackState::default();
                    playback.publish(state);
                    mpv.command("loadfile", &[&path, "replace"])
                }
                ControlCommand::SetPause(paused) => {
                    log::debug!("setting mpv pause={paused}");
                    mpv.set_property("pause", paused)
                }
                ControlCommand::SeekRelative(seconds) => {
                    log::debug!("seeking mpv relative_seconds={seconds}");
                    mpv.command("seek", &[&seconds.to_string(), "relative+exact"])
                }
                ControlCommand::SetVolume(volume) => {
                    log::debug!("setting mpv volume={volume}");
                    mpv.set_property("volume", volume)
                }
                ControlCommand::Shutdown => {
                    log::info!("mpv control worker stopped");
                    return;
                }
            };
            if let Err(error) = result {
                log::error!("mpv control command failed: {error}");
                let _ = errors.send(error.to_string());
                let _ = gpui_wakeup.try_send(());
            }
        }

        if let Some(event) = mpv.wait_event(-1.0) {
            let notify_gpui = match apply_event(event, &mut state) {
                Ok(notify_gpui) => notify_gpui,
                Err(error) => {
                    log::error!("mpv event handling failed: {error:#}");
                    let _ = errors.send(format!("{error:#}"));
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
            ("hwdec-current", PropertyData::Str(value)) => {
                if value.ends_with("-copy") {
                    log::info!(
                        "mpv decoder selected hardware_decoder={value:?} transfer=gpu-to-cpu-before-render"
                    );
                } else {
                    log::info!("mpv decoder selected hardware_decoder={value:?}");
                }
                false
            }
            ("video-codec", PropertyData::Str(value)) => {
                log::info!("mpv video stream selected codec={value:?}");
                false
            }
            ("current-vo", PropertyData::Str(value)) => {
                log::info!("mpv video output selected vo={value:?}");
                false
            }
            ("hwdec-interop", PropertyData::Str(value)) => {
                log::info!("mpv hardware frame interop available interop={value:?}");
                false
            }
            _ => false,
        },
        Event::LogMessage {
            prefix,
            text,
            log_level,
            ..
        } => {
            relay_mpv_log(log_level, prefix, text);
            false
        }
        Event::StartFile => {
            log::debug!("mpv started opening media");
            false
        }
        Event::FileLoaded => {
            log::info!("mpv media loaded");
            false
        }
        Event::VideoReconfig => {
            log::debug!("mpv video output reconfigured");
            false
        }
        Event::AudioReconfig => {
            log::debug!("mpv audio output reconfigured");
            false
        }
        Event::Seek => {
            log::debug!("mpv seek started");
            false
        }
        Event::PlaybackRestart => {
            log::debug!("mpv playback restarted");
            false
        }
        Event::EndFile(reason) => {
            log::info!("mpv media ended reason={reason:?}");
            state.finished = true;
            true
        }
        Event::Shutdown => {
            log::warn!("mpv core requested shutdown");
            state.finished = true;
            true
        }
        Event::QueueOverflow => bail!("mpv event queue overflowed; playback state may be stale"),
        _ => false,
    };
    Ok(notify_gpui)
}

fn relay_mpv_log(level: libmpv2::LogLevel, prefix: &str, text: &str) {
    let text = text.trim_end_matches(['\r', '\n']);
    match level {
        libmpv2::mpv_log_level::Fatal | libmpv2::mpv_log_level::Error => {
            log::error!("mpv[{prefix}] {text}")
        }
        libmpv2::mpv_log_level::Warn => log::warn!("mpv[{prefix}] {text}"),
        libmpv2::mpv_log_level::Info => log::info!("mpv[{prefix}] {text}"),
        libmpv2::mpv_log_level::V | libmpv2::mpv_log_level::Debug => {
            log::debug!("mpv[{prefix}] {text}")
        }
        libmpv2::mpv_log_level::Trace => log::trace!("mpv[{prefix}] {text}"),
        _ => {}
    }
}

fn render_worker(
    mut backend: RenderBackend,
    surface: WgpuVideoSurface,
    pending: Arc<AtomicBool>,
    messages: mpsc::Receiver<RenderMessage>,
    gpui_wakeup: AsyncSender<()>,
    errors: mpsc::Sender<String>,
) {
    let backend_name = backend.name();
    log::info!("video render worker started backend={backend_name}");
    let mut diagnostics = RenderDiagnostics::new();
    let mut has_frame = false;
    while let Ok(message) = messages.recv() {
        let (operation, result) = match message {
            RenderMessage::Update => {
                diagnostics.callbacks += 1;
                pending.store(false, Ordering::Release);
                let updates = backend.context().update();
                let frame_info = backend.context().next_frame_info().ok();
                let result = match render_action(updates, frame_info, has_frame) {
                    RenderAction::None => {
                        diagnostics.callbacks_without_frames += 1;
                        Ok::<bool, anyhow::Error>(false)
                    }
                    RenderAction::Skip => {
                        diagnostics.repeats += 1;
                        backend
                            .context()
                            .skip_rendering()
                            .map(|()| false)
                            .map_err(Into::into)
                    }
                    RenderAction::Render => {
                        backend.render(&surface, true, &mut diagnostics)
                    }
                };
                ("update", result)
            }
            RenderMessage::Resize { width, height } => {
                diagnostics.resizes += 1;
                let result = backend.resize(&surface, width, height).and_then(|()| {
                    if has_frame {
                        backend.render(&surface, false, &mut diagnostics)
                    } else {
                        Ok(false)
                    }
                });
                ("resize", result)
            }
            RenderMessage::Reset => {
                log::debug!("resetting published video frame backend={backend_name}");
                has_frame = false;
                surface.clear();
                ("reset", Ok(false))
            }
            RenderMessage::Shutdown => break,
        };
        match result {
            Ok(rendered) => {
                if rendered {
                    diagnostics.rendered += 1;
                    has_frame = true;
                    let _ = gpui_wakeup.try_send(());
                }
            }
            Err(error) => {
                diagnostics.errors += 1;
                log::error!(
                    "video render worker operation failed backend={backend_name} operation={operation}: {error:#}"
                );
                let _ = errors.send(format!("{error:#}"));
                let _ = gpui_wakeup.try_send(());
            }
        }
        diagnostics.maybe_log_summary(backend_name);
    }
    diagnostics.log_final(backend_name);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RenderAction {
    None,
    Render,
    Skip,
}

fn render_action(
    updates: u64,
    frame_info: Option<libmpv2::render::RenderFrameInfo>,
    has_frame: bool,
) -> RenderAction {
    if updates & u64::from(mpv_render_update::Frame) == 0 {
        return RenderAction::None;
    }
    let repeat = frame_info.is_some_and(|info| info.is_repeat());
    let redraw = frame_info.is_some_and(|info| info.is_redraw());
    if repeat && !redraw && has_frame {
        RenderAction::Skip
    } else {
        RenderAction::Render
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn info(flags: u64) -> libmpv2::render::RenderFrameInfo {
        libmpv2::render::RenderFrameInfo {
            flags,
            target_time: 0,
        }
    }

    #[test]
    fn ignores_callback_without_frame_update() {
        assert_eq!(render_action(0, None, true), RenderAction::None);
    }

    #[test]
    fn skips_exact_repeat_when_previous_frame_exists() {
        assert_eq!(
            render_action(
                u64::from(mpv_render_update::Frame),
                Some(info(4)),
                true,
            ),
            RenderAction::Skip
        );
    }

    #[test]
    fn renders_repeat_when_surface_has_no_previous_frame() {
        assert_eq!(
            render_action(
                u64::from(mpv_render_update::Frame),
                Some(info(4)),
                false,
            ),
            RenderAction::Render
        );
    }

    #[test]
    fn redraw_is_not_discarded_as_repeat() {
        assert_eq!(
            render_action(
                u64::from(mpv_render_update::Frame),
                Some(info(4 | 2)),
                true,
            ),
            RenderAction::Render
        );
    }

}
