use std::{
    ffi::{CStr, CString},
    fs::OpenOptions,
    os::unix::fs::OpenOptionsExt as _,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
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
    VideoTextureGeneration, VulkanVideoDevice, VulkanVideoTexture, WgpuVideoSurface,
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
const RENDER_RETRY_INTERVAL: Duration = Duration::from_millis(16);
const INITIAL_RENDER_TRACE_LIMIT: u64 = 8;
// The update callback coalesces notifications while one render update is
// pending, so at most one pre-seek frame update can still reach the render
// worker. Preserve one additional invalidation for the post-seek frame.
const SEEK_INVALIDATION_FRAME_UPDATES: u8 = 2;

#[derive(Default)]
struct SeekFrameInvalidation {
    remaining_frame_updates: AtomicU8,
}

impl SeekFrameInvalidation {
    fn invalidate(&self) {
        self.remaining_frame_updates
            .store(SEEK_INVALIDATION_FRAME_UPDATES, Ordering::Release);
    }

    fn take_for_update(&self, updates: u64) -> bool {
        if updates & u64::from(mpv_render_update::Frame) == 0 {
            return false;
        }
        self.remaining_frame_updates
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok()
    }
}

/// The render path selected by the first attachment player. Later players can
/// reuse this decision instead of repeating capability probing and a known-to-
/// fail Vulkan context import.
#[derive(Clone)]
pub(crate) enum AttachmentRenderBackend {
    Vulkan(Arc<VulkanVideoDevice>),
    Software,
}

impl AttachmentRenderBackend {
    fn name(&self) -> &'static str {
        match self {
            Self::Vulkan(_) => "vulkan",
            Self::Software => "software",
        }
    }
}

/// A libmpv client with dedicated control and render threads.
pub struct MpvPlayer {
    control_sender: mpsc::Sender<ControlCommand>,
    control_thread: Option<thread::JoinHandle<()>>,
    render_sender: mpsc::Sender<RenderMessage>,
    render_thread: Option<thread::JoinHandle<()>>,
    render_stopping: Arc<AtomicBool>,
    playback: Arc<SharedPlaybackState>,
    errors: mpsc::Receiver<String>,
    error_sender: mpsc::Sender<String>,
    requested_paused: bool,
    surface: PlatformSurface,
    mpv: Arc<Mpv>,
    live_diagnostics: Option<Arc<crate::live_stream::LiveDiagnostics>>,
    live_input_gate: Option<Arc<crate::live_stream::LiveInputGate>>,
    live_source: Option<crate::live_stream::LiveStreamSource>,
}

impl MpvPlayer {
    pub(crate) fn new_attachment(
        gpui_wakeup: AsyncSender<()>,
        preferred_backend: Option<AttachmentRenderBackend>,
    ) -> Result<(Self, AttachmentRenderBackend)> {
        Self::new_internal(gpui_wakeup, false, None, preferred_backend, None)
    }

    pub fn new_live(
        gpui_wakeup: AsyncSender<()>,
        share: local_rpc::model::LiveShare,
        stream: std::os::unix::net::UnixStream,
    ) -> Result<Self> {
        let source_wakeup = gpui_wakeup.clone();
        let render_size = (share.coded_width, share.coded_height);
        let (mut player, _) = Self::new_internal(
            gpui_wakeup,
            true,
            Some(render_size),
            None,
            Some(&share.codec),
        )?;
        let source = crate::live_stream::LiveStreamSource::start(
            player.mpv.clone(),
            share,
            stream,
            player
                .live_diagnostics
                .as_ref()
                .expect("live player has latency diagnostics")
                .clone(),
            player.control_sender.clone(),
            player.error_sender.clone(),
            source_wakeup,
            player.live_input_gate.clone(),
        )?;
        player.live_source = Some(source);
        player.load("chatt-live://stream")?;
        Ok(player)
    }

    fn new_internal(
        gpui_wakeup: AsyncSender<()>,
        live: bool,
        fixed_render_size: Option<(u32, u32)>,
        preferred_backend: Option<AttachmentRenderBackend>,
        live_codec: Option<&str>,
    ) -> Result<(Self, AttachmentRenderBackend)> {
        let force_live_software = live
            && std::env::var("CHATT_LIVE_RENDER_BACKEND")
                .is_ok_and(|value| value.eq_ignore_ascii_case("software"));
        let preferred_backend = if force_live_software {
            Some(AttachmentRenderBackend::Software)
        } else {
            preferred_backend
        };
        let preferred_backend_name = match preferred_backend.as_ref() {
            Some(AttachmentRenderBackend::Vulkan(_)) => "vulkan",
            Some(AttachmentRenderBackend::Software) => "software",
            None => "probe",
        };
        log::info!(
            "video player construction started live={live} preferred_backend={preferred_backend_name}"
        );
        let live_diagnostics = live.then(|| Arc::new(crate::live_stream::LiveDiagnostics::new()));
        let live_input_gate = live.then(|| Arc::new(crate::live_stream::LiveInputGate::default()));
        // The texture size is intrinsic to the media, not the element displaying
        // it. GPUI scales this persistent surface when its viewport changes.
        let video_surface = WgpuVideoSurface::new(|_, _| {})?;
        let surface = video_surface.platform_surface();
        let native_candidate = match preferred_backend.as_ref() {
            Some(AttachmentRenderBackend::Software) => None,
            Some(AttachmentRenderBackend::Vulkan(native)) => Some(Ok(native.clone())),
            None => Some(probe_vulkan_device(&video_surface)),
        };
        let vaapi_device = native_candidate
            .as_ref()
            .and_then(|candidate| candidate.as_ref().ok())
            .and_then(|native| native.drm_render_node.as_ref())
            .and_then(|path| path.to_str())
            .map(str::to_owned);
        let default_hwdec = if live {
            let supports_vulkan_decode = native_candidate
                .as_ref()
                .and_then(|candidate| candidate.as_ref().ok())
                .is_some_and(|native| {
                    supports_vulkan_video_decode(
                        &native.device_extensions,
                        native.queue_flags,
                        live_codec,
                    )
                });
            if supports_vulkan_decode {
                "vulkan,auto-copy-safe"
            } else if cfg!(target_os = "linux") {
                // Rendering through Vulkan does not imply support for Vulkan
                // Video. Prefer the Linux copy decoder that auto-probing chose
                // successfully in practice, without first consuming the only
                // keyframe in failed Vulkan and CUDA decoder attempts.
                "vaapi-copy,auto-copy-safe"
            } else {
                "auto-copy-safe"
            }
        } else {
            "vulkan,auto-safe"
        };
        let hwdec = if live {
            std::env::var("CHATT_LIVE_HWDEC")
                .ok()
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| default_hwdec.to_owned())
        } else {
            default_hwdec.to_owned()
        };
        log::info!(
            "initializing embedded libmpv live={live} hwdec={hwdec} codec={} vaapi_device={} forced_software_render={force_live_software}",
            live_codec.unwrap_or("probe-at-load"),
            vaapi_device.as_deref().unwrap_or("none")
        );
        let mpv = Mpv::with_initializer(|initializer| {
            macro_rules! set_option {
                ($name:literal, $value:expr) => {
                    if let Err(error) = initializer.set_option($name, $value) {
                        log::error!(
                            "libmpv option rejected name={} value={:?}: {error}",
                            $name,
                            $value
                        );
                        return Err(error);
                    }
                };
            }
            set_option!("vo", "libmpv");
            set_option!("keep-open", "no");
            set_option!("idle", "yes");
            set_option!("sub", "no");
            set_option!("sub-auto", "no");
            set_option!("osd-level", "0");
            // The vendored libmpv has Lua disabled, so it has no `osc`
            // option. Embedded rendering does not use mpv's OSC anyway.
            set_option!("profile", if live { "low-latency" } else { "fast" });
            set_option!("hwdec", hwdec.as_str());
            if let Some(device) = vaapi_device.as_deref() {
                set_option!("vaapi-device", device);
            }
            set_option!("sws-allow-zimg", "no");
            set_option!("sws-scaler", "bilinear");
            set_option!("sws-fast", "yes");
            if live {
                set_option!("audio", "no");
                set_option!("cache", "no");
                // Damage-tracked streams can be idle indefinitely. Keep the
                // blocking callback read off mpv's playback/render core; cache
                // and readahead stay disabled, so this does not add a playout
                // buffer.
                set_option!("demuxer-thread", "yes");
                set_option!("demuxer-readahead-secs", "0");
                set_option!("demuxer-lavf-format", "nut");
                set_option!("demuxer-lavf-probe-info", "nostreams");
                set_option!("demuxer-lavf-analyzeduration", "0");
                set_option!("untimed", "yes");
                set_option!("video-latency-hacks", "yes");
                set_option!("swapchain-depth", "1");
                set_option!("vd-lavc-threads", "1");
                // Screen-share encoders emit frames in display order. Avoid
                // mpv's two-frame hwdec-copy delay queue: with damage-driven
                // input those retained frames could remain stale indefinitely.
                set_option!("vd-lavc-low-latency", "yes");
                set_option!("interpolation", "no");
                set_option!("stream-buffer-size", "4k");
            }
            Ok(())
        })
        .map_err(|error| {
            log::error!("embedded libmpv initialization failed live={live}: {error}");
            error
        })?;
        log::info!("embedded libmpv initialized live={live}");
        let mpv = Arc::new(mpv);

        mpv.observe_property("time-pos", Format::Double, 1)
            .context("observe mpv property time-pos")?;
        mpv.observe_property("duration", Format::Double, 2)
            .context("observe mpv property duration")?;
        mpv.observe_property("pause", Format::Flag, 3)
            .context("observe mpv property pause")?;
        mpv.observe_property("eof-reached", Format::Flag, 4)
            .context("observe mpv property eof-reached")?;
        mpv.observe_property("idle-active", Format::Flag, 5)
            .context("observe mpv property idle-active")?;
        mpv.observe_property("hwdec-current", Format::String, 6)
            .context("observe mpv property hwdec-current")?;
        mpv.observe_property("video-codec", Format::String, 7)
            .context("observe mpv property video-codec")?;
        mpv.observe_property("current-vo", Format::String, 8)
            .context("observe mpv property current-vo")?;
        mpv.observe_property("hwdec-interop", Format::String, 9)
            .context("observe mpv property hwdec-interop")?;
        log::info!("libmpv playback properties registered live={live}");

        let mpv_log_level = std::env::var("CHATT_MPV_LOG").unwrap_or_else(|_| "warn".into());
        mpv.request_log_messages(&mpv_log_level)
            .with_context(|| format!("request native mpv log level {mpv_log_level:?}"))?;
        log::info!("native mpv logging enabled min_level={mpv_log_level:?}");
        if live {
            log::info!(
                "live playback latency mode enabled cache=false demux_readahead_secs=0 hwdec_copy_delay_frames=0 latest_frame=true"
            );
        }

        let (render_sender, render_messages) = mpsc::channel();

        let (mut backend, selected_backend) = match preferred_backend {
            Some(AttachmentRenderBackend::Software) => {
                let context = mpv
                    .create_software_render_context(live)
                    .context("create preferred libmpv software render context")?;
                log::info!(
                    "video render backend selected backend=software upload=wgpu latest_frame={live} cached_decision=true"
                );
                (
                    RenderBackend::Software {
                        context,
                        generation: None,
                        next_texture: 0,
                        aligned: Vec::new(),
                        tight: Vec::new(),
                    },
                    AttachmentRenderBackend::Software,
                )
            }
            preferred => {
                let cached_decision = preferred.is_some();
                let native = native_candidate
                    .expect("non-software render preference must have a Vulkan probe result");
                log::info!(
                    "creating libmpv Vulkan render context live={live} cached_decision={cached_decision}"
                );
                match native.and_then(|native| {
                    create_vulkan_context(&mpv, &native, live).map(|context| (context, native))
                }) {
                    Ok((context, native)) => {
                        log::info!(
                            "video render backend selected backend=vulkan sharing=wgpu-device latest_frame={live} cached_decision={cached_decision}"
                        );
                        (
                            RenderBackend::Vulkan {
                                context,
                                generation: None,
                                next_texture: 0,
                            },
                            AttachmentRenderBackend::Vulkan(native),
                        )
                    }
                    Err(error) => {
                        log::warn!(
                            "Vulkan libmpv interop unavailable, using software fallback: {error:#}"
                        );
                        let context = mpv.create_software_render_context(live).context(
                            "create libmpv software render context after Vulkan fallback",
                        )?;
                        log::info!(
                            "video render backend selected backend=software upload=wgpu latest_frame={live}"
                        );
                        (
                            RenderBackend::Software {
                                context,
                                generation: None,
                                next_texture: 0,
                                aligned: Vec::new(),
                                tight: Vec::new(),
                            },
                            AttachmentRenderBackend::Software,
                        )
                    }
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
        let playback = Arc::new(SharedPlaybackState::default());
        let frame_invalidated = Arc::new(SeekFrameInvalidation::default());
        let render_playback = playback.clone();
        let render_frame_invalidated = frame_invalidated.clone();
        let render_error_sender = error_sender.clone();
        let render_gpui_wakeup = gpui_wakeup.clone();
        let render_live_diagnostics = live_diagnostics.clone();
        let render_live_input_gate = live_input_gate.clone();
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
                    render_live_diagnostics,
                    render_live_input_gate,
                    render_playback,
                    render_frame_invalidated,
                );
            })
            .context("spawn mpv render thread")?;

        // Live streams declare their coded size up front. Attachment players
        // configure later from mpv's decoded display dimensions.
        if let Some((width, height)) = fixed_render_size {
            render_sender
                .send(RenderMessage::Resize {
                    width,
                    height,
                    redraw: false,
                })
                .map_err(|_| anyhow!("mpv render thread stopped during initial resize"))?;
        }

        let (control_sender, control_commands) = mpsc::channel();
        let control_playback = playback.clone();
        let control_frame_invalidated = frame_invalidated.clone();
        let control_mpv = mpv.clone();
        let control_error_sender = error_sender.clone();
        let control_render_sender = render_sender.clone();
        let control_thread =
            match thread::Builder::new()
                .name("mpv-control".into())
                .spawn(move || {
                    control_worker(
                        control_mpv,
                        control_commands,
                        control_playback,
                        gpui_wakeup,
                        control_error_sender,
                        control_render_sender,
                        fixed_render_size,
                        control_frame_invalidated,
                    );
                }) {
                Ok(thread) => thread,
                Err(error) => {
                    render_stopping.store(true, Ordering::Release);
                    let _ = render_sender.send(RenderMessage::Shutdown);
                    let _ = render_thread.join();
                    return Err(error).context("spawn mpv control thread");
                }
            };

        log::info!(
            "video player construction completed live={live} backend={}",
            selected_backend.name()
        );

        Ok((
            Self {
                control_sender,
                control_thread: Some(control_thread),
                render_sender,
                render_thread: Some(render_thread),
                render_stopping,
                playback,
                errors,
                error_sender,
                requested_paused: false,
                surface,
                mpv,
                live_diagnostics,
                live_input_gate,
                live_source: None,
            },
            selected_backend,
        ))
    }

    pub fn surface(&self) -> PlatformSurface {
        self.surface.clone()
    }

    pub fn load(&mut self, path: &str) -> Result<()> {
        self.load_at(path, false, 100.0, 0.0)
    }

    pub(crate) fn load_at(
        &mut self,
        path: &str,
        paused: bool,
        volume: f64,
        position: f64,
    ) -> Result<()> {
        self.playback.reset();
        self.requested_paused = paused;
        self.render_sender
            .send(RenderMessage::Reset)
            .map_err(|_| anyhow!("mpv render thread stopped"))?;
        self.send_control(ControlCommand::Load {
            path: path.to_owned(),
            paused,
            volume: volume.clamp(0.0, 100.0),
            position: position.max(0.0),
        })
    }

    pub fn toggle_pause(&mut self) -> Result<bool> {
        let paused = !self.requested_paused;
        self.set_paused(paused)?;
        Ok(paused)
    }

    pub(crate) fn set_paused(&mut self, paused: bool) -> Result<()> {
        self.requested_paused = paused;
        self.playback.paused.store(paused, Ordering::Release);
        self.send_control(ControlCommand::SetPause(paused))
    }

    pub(crate) fn seek_absolute(&self, seconds: f64) -> Result<()> {
        let seconds = seconds.max(0.0);
        self.playback.seek_to(seconds);
        self.send_control(ControlCommand::SeekAbsolute {
            seconds,
            mode: SeekMode::Exact,
        })
    }

    pub(crate) fn seek_percent(&self, percent: f64, position: f64, mode: SeekMode) -> Result<()> {
        let percent = percent.clamp(0.0, 100.0);
        let position = position.max(0.0);
        self.playback.seek_to(position);
        self.send_control(ControlCommand::SeekPercent {
            percent,
            position,
            mode,
        })
    }

    pub fn set_volume(&self, volume: f64) -> Result<()> {
        self.send_control(ControlCommand::SetVolume(volume.clamp(0.0, 100.0)))
    }

    pub(crate) fn stop(&mut self) -> Result<()> {
        self.requested_paused = true;
        self.playback.reset();
        self.render_sender
            .send(RenderMessage::ReleaseResources)
            .map_err(|_| anyhow!("mpv render thread stopped"))?;
        self.send_control(ControlCommand::Stop)
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
        self.live_source.take();
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
        // `mpv_destroy` only detaches this client handle and permits the core,
        // its internal clients, and registered stream callbacks to outlive it.
        // All application workers are joined now, so synchronously terminate
        // the core when the final Arc is released below.
        self.mpv.terminate_on_drop();
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

fn supports_vulkan_video_decode(
    device_extensions: &[&CStr],
    queue_flags: vk::QueueFlags,
    codec: Option<&str>,
) -> bool {
    let has = |required: &CStr| {
        device_extensions
            .iter()
            .any(|extension| *extension == required)
    };
    if !queue_flags.contains(vk::QueueFlags::VIDEO_DECODE_KHR)
        || !has(c"VK_KHR_video_queue")
        || !has(c"VK_KHR_video_decode_queue")
    {
        return false;
    }
    match codec {
        Some(codec) if codec.starts_with("avc1.") || codec.eq_ignore_ascii_case("h264") => {
            has(c"VK_KHR_video_decode_h264")
        }
        Some(codec)
            if codec.starts_with("hvc1.")
                || codec.starts_with("hev1.")
                || codec.eq_ignore_ascii_case("hevc") =>
        {
            has(c"VK_KHR_video_decode_h265")
        }
        Some(codec) if codec.starts_with("av01.") || codec.eq_ignore_ascii_case("av1") => {
            has(c"VK_KHR_video_decode_av1")
        }
        _ => false,
    }
}

fn probe_vulkan_device(surface: &WgpuVideoSurface) -> Result<Arc<VulkanVideoDevice>> {
    let native = Arc::new(surface.vulkan_device()?);
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
    let has_vulkan_video_core = [c"VK_KHR_video_queue", c"VK_KHR_video_decode_queue"]
        .iter()
        .all(|required| {
            native
                .device_extensions
                .iter()
                .any(|extension| extension == required)
        });
    let has_vulkan_video_h264 = native
        .device_extensions
        .iter()
        .any(|extension| *extension == c"VK_KHR_video_decode_h264");
    let has_vulkan_video_h265 = native
        .device_extensions
        .iter()
        .any(|extension| *extension == c"VK_KHR_video_decode_h265");
    let has_vulkan_video_av1 = native
        .device_extensions
        .iter()
        .any(|extension| *extension == c"VK_KHR_video_decode_av1");
    let queue_has_video_decode = native
        .queue_flags
        .contains(vk::QueueFlags::VIDEO_DECODE_KHR);
    let drm_render_node = native
        .drm_render_node
        .as_deref()
        .map_or_else(|| "none".into(), |path| path.display().to_string());
    log::info!(
        "importing GPUI Vulkan device into libmpv queue_family={} queue_index={} queue_video_decode={} enabled_queue_families={} instance_extensions={} device_extensions={} external_memory_fd={} dma_buf={} drm_modifiers={} drm_render_node={} vulkan_video_core={} vulkan_video_h264={} vulkan_video_h265={} vulkan_video_av1={}",
        native.queue_family,
        native.queue_index,
        queue_has_video_decode,
        native.enabled_queue_families.len(),
        native.instance_extensions.len(),
        native.device_extensions.len(),
        has_external_memory_fd,
        has_dma_buf,
        has_drm_modifiers,
        drm_render_node,
        has_vulkan_video_core,
        has_vulkan_video_h264,
        has_vulkan_video_h265,
        has_vulkan_video_av1,
    );
    if cfg!(target_os = "linux") && !(has_external_memory_fd && has_dma_buf && has_drm_modifiers) {
        log::warn!(
            "Vulkan device lacks Linux dma-buf import extensions; hardware-decoded video may round-trip through CPU memory"
        );
    }
    Ok(native)
}

fn create_vulkan_context(
    mpv: &Arc<Mpv>,
    native: &VulkanVideoDevice,
    latest_frame: bool,
) -> Result<RenderContext> {
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
    let enabled_queue_families = native
        .enabled_queue_families
        .iter()
        .map(|(index, count)| VulkanQueueFamily {
            index: *index,
            count: *count,
        })
        .collect();
    let drm_render_fd = native.drm_render_node.as_ref().and_then(|path| {
        match OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_CLOEXEC)
            .open(path)
        {
            Ok(file) => Some(file.into()),
            Err(error) => {
                log::warn!(
                    "Could not open matching DRM render node {} for VAAPI interop; continuing without it: {error}",
                    path.display()
                );
                None
            }
        }
    });
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
        enabled_queue_families,
        queue_lock: Arc::new(WgpuQueueLock(native.queue_lock.clone())),
        drm_render_fd,
        latest_frame,
    })
    .context("import GPUI's Vulkan device into libmpv")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SeekMode {
    Exact,
    Keyframes,
}

impl SeekMode {
    fn absolute_flag(self) -> &'static str {
        match self {
            Self::Exact => "absolute+exact",
            Self::Keyframes => "absolute+keyframes",
        }
    }

    fn absolute_percent_flag(self) -> &'static str {
        match self {
            Self::Exact => "absolute-percent+exact",
            // MPV's OSC leaves drag precision at the player's default so its
            // keyframe/exact heuristics remain available.
            Self::Keyframes => "absolute-percent",
        }
    }
}

pub(crate) enum ControlCommand {
    Load {
        path: String,
        paused: bool,
        volume: f64,
        position: f64,
    },
    SetPause(bool),
    SeekAbsolute {
        seconds: f64,
        mode: SeekMode,
    },
    SeekPercent {
        percent: f64,
        position: f64,
        mode: SeekMode,
    },
    SetVolume(f64),
    Stop,
    DropBuffers,
    Shutdown,
}

enum RenderMessage {
    Update,
    Enable,
    Resize {
        width: u32,
        height: u32,
        redraw: bool,
    },
    Reset,
    ReleaseResources,
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

    fn is_configured(&self) -> bool {
        match self {
            Self::Vulkan { generation, .. } | Self::Software { generation, .. } => {
                generation.is_some()
            }
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

    fn skip_rendering(&self) -> Result<()> {
        let context = self.context();
        context.skip_rendering()?;
        context.report_swap();
        Ok(())
    }

    fn resize(&mut self, surface: &WgpuVideoSurface, width: u32, height: u32) -> Result<bool> {
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
                    return Ok(false);
                }
                if let Some(old) = generation.take() {
                    log::debug!(
                        "retiring video texture generation backend=vulkan generation={} textures={}",
                        old.id,
                        old.textures.len(),
                    );
                    for texture in &old.textures {
                        let image = texture
                            .image()
                            .context("get Vulkan video image during resize")?;
                        context
                            .remove_vulkan_target(image)
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
                    return Ok(false);
                }
                if generation.take().is_some() {
                    surface
                        .wait_idle()
                        .context("wait before retiring software-upload video textures")?;
                }
                let new_generation = surface.allocate_software_generation(width, height)?;
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
        Ok(true)
    }

    fn release_resources(&mut self, surface: &WgpuVideoSurface) -> Result<()> {
        match self {
            Self::Vulkan {
                context,
                generation,
                next_texture,
            } => {
                if let Some(old) = generation.take() {
                    for texture in &old.textures {
                        let image = texture
                            .image()
                            .context("get Vulkan video image during release")?;
                        context
                            .remove_vulkan_target(image)
                            .context("remove recycled Vulkan render target from libmpv")?;
                    }
                    surface
                        .wait_idle()
                        .context("wait before releasing recycled Vulkan video textures")?;
                }
                *next_texture = 0;
            }
            Self::Software {
                generation,
                next_texture,
                aligned,
                tight,
                ..
            } => {
                if generation.take().is_some() {
                    surface
                        .wait_idle()
                        .context("wait before releasing recycled software video textures")?;
                }
                *next_texture = 0;
                *aligned = Vec::new();
                *tight = Vec::new();
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
                    // Keep mpv's pending frame intact until Resize installs the
                    // first texture generation. Consuming it here skips the
                    // beginning of a file when dimensions arrive after load.
                    return Ok(false);
                };
                let Some((texture, sync)) = next_ring_texture(generation, next_texture) else {
                    diagnostics.note_ring_busy("vulkan", generation.id);
                    if acknowledge_if_busy {
                        context.skip_rendering()?;
                        context.report_swap();
                    }
                    return Ok(false);
                };
                let image = texture.image().context("get Vulkan video render image")?;
                let semaphore = texture
                    .semaphore()
                    .context("get Vulkan video render semaphore")?;
                let render = context.render_vulkan(VulkanRenderTarget {
                    image,
                    format: vk::Format::R8G8B8A8_UNORM,
                    usage: vk::ImageUsageFlags::SAMPLED
                        | vk::ImageUsageFlags::COLOR_ATTACHMENT
                        | vk::ImageUsageFlags::TRANSFER_SRC
                        | vk::ImageUsageFlags::TRANSFER_DST,
                    width: texture.width(),
                    height: texture.height(),
                    input_layout: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    output_layout: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    wait_semaphore: semaphore,
                    wait_value: sync.wait_value,
                    signal_semaphore: semaphore,
                    signal_value: sync.ready_value,
                });
                if let Err(error) = render {
                    texture.cancel_render();
                    return Err(error).context("render libmpv frame into Vulkan texture");
                }
                surface.publish(texture, sync);
                context.report_swap();
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
                    // The decoded frame remains pending and is rendered by the
                    // Resize message once a backend-neutral texture exists.
                    return Ok(false);
                };
                let Some((texture, _sync)) = next_ring_texture(generation, next_texture) else {
                    diagnostics.note_ring_busy("software", generation.id);
                    if acknowledge_if_busy {
                        context.skip_rendering()?;
                        context.report_swap();
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
                context.report_swap();
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
        if self
            .last_pressure_log
            .is_none_or(|last| now.duration_since(last) >= RENDER_PRESSURE_LOG_INTERVAL)
        {
            self.last_pressure_log = Some(now);
            log::warn!(
                "all video textures are still in use; deferring render request backend={backend} generation={generation} busy_total={}",
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
    render_sender: mpsc::Sender<RenderMessage>,
    initial_render_size: Option<(u32, u32)>,
    frame_invalidated: Arc<SeekFrameInvalidation>,
) {
    log::info!("mpv control worker started");
    let mut state = PlaybackState::default();
    let mut configured_size = initial_render_size;
    let mut pending_start = None;
    let delay_render_until_reconfiguration = initial_render_size.is_some();
    let mut render_enabled = false;
    loop {
        while let Ok(command) = commands.try_recv() {
            let result = match command {
                ControlCommand::Load {
                    path,
                    paused,
                    volume,
                    position,
                } => {
                    log::info!("loading media path={path:?}");
                    state = PlaybackState {
                        paused,
                        ..PlaybackState::default()
                    };
                    configured_size = initial_render_size;
                    pending_start = (position > 0.0).then_some(position);
                    render_enabled = false;
                    playback.publish(state);
                    // `stop` is synchronous. Drain the events it enqueued
                    // before opening the replacement so a pooled core cannot
                    // mistake the previous file's FileLoaded/EndFile for the
                    // new session.
                    let result = mpv
                        .command("stop", &[])
                        .map(|()| while mpv.wait_event(0.0).is_some() {})
                        .and_then(|()| mpv.set_property("pause", paused))
                        .and_then(|()| mpv.set_property("volume", volume))
                        .and_then(|()| mpv.command("loadfile", &[&path, "replace"]));
                    if result.is_ok() {
                        log::info!(
                            "mpv accepted loadfile paused={paused} volume={volume:.1} start_position={position:.3}"
                        );
                    }
                    result
                }
                ControlCommand::SetPause(paused) => {
                    log::debug!("setting mpv pause={paused}");
                    mpv.set_property("pause", paused)
                }
                ControlCommand::SeekAbsolute { seconds, mode } => {
                    log::debug!("seeking mpv absolute_seconds={seconds} mode={mode:?}");
                    state.position = seconds;
                    state.finished = false;
                    playback.publish(state);
                    frame_invalidated.invalidate();
                    mpv.command("seek", &[&seconds.to_string(), mode.absolute_flag()])
                }
                ControlCommand::SeekPercent {
                    percent,
                    position,
                    mode,
                } => {
                    log::debug!(
                        "seeking mpv absolute_percent={percent} expected_seconds={position} mode={mode:?}"
                    );
                    state.position = position;
                    state.finished = false;
                    playback.publish(state);
                    frame_invalidated.invalidate();
                    mpv.command(
                        "seek",
                        &[&percent.to_string(), mode.absolute_percent_flag()],
                    )
                }
                ControlCommand::SetVolume(volume) => {
                    log::debug!("setting mpv volume={volume}");
                    mpv.set_property("volume", volume)
                }
                ControlCommand::Stop => {
                    log::debug!("stopping media while retaining mpv core");
                    state = PlaybackState {
                        paused: true,
                        ..PlaybackState::default()
                    };
                    pending_start = None;
                    configured_size = initial_render_size;
                    playback.publish(state);
                    mpv.command("stop", &[])
                }
                ControlCommand::DropBuffers => {
                    log::debug!("dropping buffered live video at fresh keyframe");
                    mpv.command("drop-buffers", &[])
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
            if matches!(event.as_ref(), Ok(Event::FileLoaded))
                && let Some(position) = pending_start.take()
                && let Err(error) = mpv.command("seek", &[&position.to_string(), "absolute+exact"])
            {
                log::warn!("failed to restore retained playback position: {error}");
            }
            let file_loaded = matches!(event.as_ref(), Ok(Event::FileLoaded));
            let video_reconfigured = matches!(event.as_ref(), Ok(Event::VideoReconfig));
            let mut display_size_changed = false;
            if file_loaded || video_reconfigured {
                match video_display_size(&mpv) {
                    Ok(size) => {
                        let resized = configured_size != Some(size);
                        if resized {
                            log::info!(
                                "video texture configured at decoded display size={}x{}",
                                size.0,
                                size.1,
                            );
                        }
                        configured_size = Some(size);
                        display_size_changed = state.display_size != Some(size);
                        state.display_size = Some(size);
                        if resized
                            && render_sender
                                .send(RenderMessage::Resize {
                                    width: size.0,
                                    height: size.1,
                                    redraw: false,
                                })
                                .is_err()
                        {
                            let message = "mpv render thread stopped during video configuration";
                            log::error!("{message}");
                            let _ = errors.send(message.into());
                        }
                    }
                    Err(error) => {
                        log::debug!("decoded video size is not ready: {error:#}");
                    }
                }
            }
            let should_enable_render = !render_enabled
                && ((!delay_render_until_reconfiguration && file_loaded)
                    || (delay_render_until_reconfiguration && video_reconfigured));
            if should_enable_render {
                log::info!(
                    "enabling video rendering trigger={} preserved_frame=true",
                    if video_reconfigured {
                        "video-reconfiguration"
                    } else {
                        "file-loaded"
                    }
                );
                let enabled = render_sender.send(RenderMessage::Enable).is_ok()
                    && render_sender.send(RenderMessage::Update).is_ok();
                if enabled {
                    render_enabled = true;
                } else {
                    let message = "mpv render thread stopped while enabling the new media";
                    log::error!("{message}");
                    let _ = errors.send(message.into());
                }
            }
            let notify_gpui = match apply_event(event, &mut state) {
                Ok(notify_gpui) => notify_gpui,
                Err(error) => {
                    log::error!("mpv event handling failed: {error:#}");
                    let _ = errors.send(format!("{error:#}"));
                    true
                }
            } || display_size_changed;
            playback.publish(state);
            if notify_gpui {
                let _ = gpui_wakeup.try_send(());
            }
        }
    }
}

fn video_display_size(mpv: &Mpv) -> Result<(u32, u32)> {
    let width = mpv
        .get_property::<i64>("dwidth")
        .context("read decoded video display width")?;
    let height = mpv
        .get_property::<i64>("dheight")
        .context("read decoded video display height")?;
    checked_video_size(width, height)
        .ok_or_else(|| anyhow!("invalid decoded video display size {width}x{height}"))
}

fn checked_video_size(width: i64, height: i64) -> Option<(u32, u32)> {
    Some((u32::try_from(width).ok()?, u32::try_from(height).ok()?))
        .filter(|(width, height)| *width != 0 && *height != 0)
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
                let changed = state.finished != value;
                state.finished = value;
                changed
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
            log::info!("mpv started opening media");
            let was_finished = state.finished;
            state.finished = false;
            was_finished
        }
        Event::FileLoaded => {
            log::info!("mpv media loaded");
            false
        }
        Event::VideoReconfig => {
            log::info!("mpv video output reconfigured");
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
            log::debug!("mpv playback resumed after load, seek, or discontinuity");
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
            log::error!(target: "chatt_mpv", "mpv[{prefix}] {text}")
        }
        libmpv2::mpv_log_level::Warn => {
            log::warn!(target: "chatt_mpv", "mpv[{prefix}] {text}")
        }
        libmpv2::mpv_log_level::Info => {
            log::info!(target: "chatt_mpv", "mpv[{prefix}] {text}")
        }
        libmpv2::mpv_log_level::V | libmpv2::mpv_log_level::Debug => {
            log::debug!(target: "chatt_mpv", "mpv[{prefix}] {text}")
        }
        libmpv2::mpv_log_level::Trace => {
            log::trace!(target: "chatt_mpv", "mpv[{prefix}] {text}")
        }
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
    live_diagnostics: Option<Arc<crate::live_stream::LiveDiagnostics>>,
    live_input_gate: Option<Arc<crate::live_stream::LiveInputGate>>,
    playback: Arc<SharedPlaybackState>,
    frame_invalidated: Arc<SeekFrameInvalidation>,
) {
    let backend_name = backend.name();
    log::info!("video render worker started backend={backend_name}");
    let mut diagnostics = RenderDiagnostics::new();
    let mut has_frame = false;
    let mut enabled = false;
    let mut redraw_pending = false;
    loop {
        let message = if redraw_pending {
            match messages.recv_timeout(RENDER_RETRY_INTERVAL) {
                Ok(message) => Some(message),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        } else {
            match messages.recv() {
                Ok(message) => Some(message),
                Err(_) => break,
            }
        };
        if message.is_none() {
            let result = backend.render(&surface, false, &mut diagnostics);
            match result {
                Ok(true) => {
                    diagnostics.rendered += 1;
                    has_frame = true;
                    redraw_pending = false;
                    playback.frame_ready.store(true, Ordering::Release);
                    let _ = gpui_wakeup.try_send(());
                    if live_input_gate.as_ref().is_some_and(|gate| gate.release()) {
                        log::info!(
                            "released live decoder input after first rendered output backend={backend_name}"
                        );
                    }
                    if diagnostics.rendered <= INITIAL_RENDER_TRACE_LIMIT {
                        log::info!(
                            "video render output published ordinal={} operation=deferred-redraw",
                            diagnostics.rendered
                        );
                    }
                }
                Ok(false) => {
                    redraw_pending = backend.is_configured();
                }
                Err(error) => {
                    diagnostics.errors += 1;
                    redraw_pending = false;
                    log::error!(
                        "video render worker operation failed backend={backend_name} operation=deferred redraw: {error:#}"
                    );
                    let _ = errors.send(format!("{error:#}"));
                    let _ = gpui_wakeup.try_send(());
                }
            }
            diagnostics.maybe_log_summary(backend_name);
            continue;
        }
        let message = message.unwrap();
        let (operation, result) = match message {
            RenderMessage::Update => {
                diagnostics.callbacks += 1;
                pending.store(false, Ordering::Release);
                let updates = backend.context().update();
                if frame_invalidated.take_for_update(updates) {
                    has_frame = false;
                    redraw_pending = false;
                    log::debug!("invalidated displayed video frame after seek");
                }
                if !enabled {
                    // A live stream can expose its first decoded frame before
                    // Vulkan output reconfiguration has settled. Keep that
                    // frame pending; Enable is followed by an explicit Update
                    // so it is rendered once the target is ready.
                    ("disabled update", Ok(false))
                } else {
                    let frame_info = backend.context().next_frame_info().ok();
                    let frame_pts = backend.context().next_frame_video_pts().ok();
                    let action = render_action(updates, frame_info, has_frame);
                    if diagnostics.callbacks <= INITIAL_RENDER_TRACE_LIMIT {
                        log::info!(
                            "video render callback ordinal={} updates=0x{:x} action={:?} frame_info_available={} frame_flags=0x{:x} present={} repeat={} redraw={} target_time_ns={:?} pts={:?} has_frame={}",
                            diagnostics.callbacks,
                            updates,
                            action,
                            frame_info.is_some(),
                            frame_info.map_or(0, |info| info.flags),
                            frame_info.is_some_and(|info| info.is_present()),
                            frame_info.is_some_and(|info| info.is_repeat()),
                            frame_info.is_some_and(|info| info.is_redraw()),
                            frame_info.map(|info| info.target_time),
                            frame_pts,
                            has_frame,
                        );
                    }
                    let result = match action {
                        RenderAction::None => {
                            diagnostics.callbacks_without_frames += 1;
                            Ok::<bool, anyhow::Error>(false)
                        }
                        RenderAction::Skip => {
                            diagnostics.repeats += 1;
                            backend.skip_rendering().map(|()| false).map_err(Into::into)
                        }
                        RenderAction::Render => {
                            let result = backend.render(&surface, true, &mut diagnostics);
                            redraw_pending = result.as_ref().is_ok_and(|rendered| !rendered)
                                && backend.is_configured();
                            if result.as_ref().is_ok_and(|rendered| *rendered)
                                && let (Some(diagnostics), Some(pts)) =
                                    (live_diagnostics.as_ref(), frame_pts)
                            {
                                diagnostics.record_render(pts);
                            }
                            result
                        }
                    };
                    ("update", result)
                }
            }
            RenderMessage::Enable => {
                enabled = true;
                ("enable", Ok(false))
            }
            RenderMessage::Resize {
                width,
                height,
                redraw,
            } => {
                diagnostics.resizes += 1;
                let result = backend.resize(&surface, width, height).and_then(|resized| {
                    if should_render_after_resize(enabled, resized, redraw) {
                        backend.render(&surface, false, &mut diagnostics)
                    } else {
                        Ok(false)
                    }
                });
                ("resize", result)
            }
            RenderMessage::Reset => {
                log::debug!("resetting published video frame backend={backend_name}");
                enabled = false;
                has_frame = false;
                redraw_pending = false;
                playback.frame_ready.store(false, Ordering::Release);
                surface.clear();
                ("reset", Ok(false))
            }
            RenderMessage::ReleaseResources => {
                log::debug!("releasing pooled video frame resources backend={backend_name}");
                enabled = false;
                has_frame = false;
                redraw_pending = false;
                playback.frame_ready.store(false, Ordering::Release);
                surface.clear();
                (
                    "release resources",
                    backend.release_resources(&surface).map(|()| false),
                )
            }
            RenderMessage::Shutdown => break,
        };
        match result {
            Ok(rendered) => {
                if rendered {
                    diagnostics.rendered += 1;
                    has_frame = true;
                    redraw_pending = false;
                    playback.frame_ready.store(true, Ordering::Release);
                    let _ = gpui_wakeup.try_send(());
                    if live_input_gate.as_ref().is_some_and(|gate| gate.release()) {
                        log::info!(
                            "released live decoder input after first rendered output backend={backend_name}"
                        );
                    }
                    if diagnostics.rendered <= INITIAL_RENDER_TRACE_LIMIT {
                        log::info!(
                            "video render output published ordinal={} operation={operation}",
                            diagnostics.rendered
                        );
                    }
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

fn should_render_after_resize(enabled: bool, resized: bool, redraw: bool) -> bool {
    enabled && (resized || redraw)
}

#[derive(Clone, Copy, Debug, Default)]
pub struct PlaybackState {
    pub position: f64,
    pub duration: f64,
    pub paused: bool,
    pub finished: bool,
    pub frame_ready: bool,
    pub display_size: Option<(u32, u32)>,
}

#[derive(Default)]
struct SharedPlaybackState {
    position: AtomicU64,
    duration: AtomicU64,
    paused: AtomicBool,
    finished: AtomicBool,
    frame_ready: AtomicBool,
    display_size: AtomicU64,
}

impl SharedPlaybackState {
    fn publish(&self, state: PlaybackState) {
        self.position
            .store(state.position.to_bits(), Ordering::Relaxed);
        self.duration
            .store(state.duration.to_bits(), Ordering::Relaxed);
        self.paused.store(state.paused, Ordering::Relaxed);
        self.finished.store(state.finished, Ordering::Release);
        let display_size = state.display_size.map_or(0, |(width, height)| {
            (u64::from(width) << 32) | u64::from(height)
        });
        self.display_size.store(display_size, Ordering::Release);
    }

    fn snapshot(&self) -> PlaybackState {
        let finished = self.finished.load(Ordering::Acquire);
        let packed_size = self.display_size.load(Ordering::Acquire);
        PlaybackState {
            position: f64::from_bits(self.position.load(Ordering::Relaxed)),
            duration: f64::from_bits(self.duration.load(Ordering::Relaxed)),
            paused: self.paused.load(Ordering::Relaxed),
            finished,
            frame_ready: self.frame_ready.load(Ordering::Acquire),
            display_size: (packed_size != 0)
                .then_some(((packed_size >> 32) as u32, packed_size as u32)),
        }
    }

    fn seek_to(&self, position: f64) {
        self.position.store(position.to_bits(), Ordering::Relaxed);
        self.finished.store(false, Ordering::Release);
    }

    fn reset(&self) {
        self.publish(PlaybackState::default());
        self.frame_ready.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VAAPI_DISABLED_CHILD: &str = "CHATT_TEST_VAAPI_DISABLED_CHILD";

    fn info(flags: u64) -> libmpv2::render::RenderFrameInfo {
        libmpv2::render::RenderFrameInfo {
            flags,
            target_time: 0,
        }
    }

    #[test]
    fn accepts_only_positive_decoded_video_sizes() {
        assert_eq!(checked_video_size(320, 240), Some((320, 240)));
        assert_eq!(checked_video_size(0, 240), None);
        assert_eq!(checked_video_size(320, -1), None);
        assert_eq!(checked_video_size(i64::from(u32::MAX) + 1, 240), None);
    }

    #[test]
    fn shared_playback_publishes_decoded_display_size() {
        let shared = SharedPlaybackState::default();
        shared.publish(PlaybackState {
            display_size: Some((1_080, 1_920)),
            ..PlaybackState::default()
        });

        assert_eq!(shared.snapshot().display_size, Some((1_080, 1_920)));
        shared.reset();
        assert_eq!(shared.snapshot().display_size, None);
    }

    #[test]
    fn vaapi_loader_can_be_disabled_without_preventing_startup() {
        if std::env::var_os(VAAPI_DISABLED_CHILD).is_some() {
            assert!(!libmpv2::vaapi_runtime_available());
            println!("VAAPI lazy-loader disable path reached");
            return;
        }

        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "mpv_player::tests::vaapi_loader_can_be_disabled_without_preventing_startup",
                "--nocapture",
            ])
            .env(VAAPI_DISABLED_CHILD, "1")
            .env("CHATT_DISABLE_VAAPI", "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success() && stdout.contains("VAAPI lazy-loader disable path reached"),
            "disabled VAAPI child failed\nstdout:\n{stdout}\nstderr:\n{stderr}",
        );
    }

    #[test]
    fn libmpv_reports_the_decoded_display_size_for_attachments() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("intrinsic-size.mkv");
        let output = std::process::Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=black:s=320x180:d=0.04",
                "-frames:v",
                "1",
                "-c:v",
                "ffv1",
                "-y",
            ])
            .arg(&path)
            .output()
            .expect("ffmpeg is available with the required libmpv dependency");
        assert!(
            output.status.success(),
            "ffmpeg could not create the attachment fixture: {}",
            String::from_utf8_lossy(&output.stderr),
        );

        let mpv = Mpv::with_initializer(|initializer| {
            initializer.set_option("vo", "null")?;
            initializer.set_option("audio", "no")?;
            initializer.set_option("pause", "yes")?;
            Ok(())
        })
        .unwrap();
        mpv.command("loadfile", &[&path.to_string_lossy(), "replace"])
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(3);
        let mut display_size = None;
        while Instant::now() < deadline {
            match mpv.wait_event(0.1) {
                Some(Ok(Event::FileLoaded | Event::VideoReconfig)) => {
                    if let Ok(size) = video_display_size(&mpv) {
                        display_size = Some(size);
                        break;
                    }
                }
                Some(Err(error)) => panic!("libmpv event failed: {error}"),
                _ => {}
            }
        }

        assert_eq!(display_size, Some((320, 180)));
    }

    #[test]
    fn libmpv_plays_audio_video_file_with_unsupported_subtitle_track() {
        let directory = tempfile::tempdir().unwrap();
        let subtitles = directory.path().join("unsupported.srt");
        std::fs::write(
            &subtitles,
            "1\n00:00:00,000 --> 00:00:00,750\nnot rendered by embedded mpv\n",
        )
        .unwrap();
        let path = directory.path().join("audio-video-subtitle.mkv");
        let output = std::process::Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=blue:s=320x180:d=1:r=24",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=1",
                "-i",
            ])
            .arg(&subtitles)
            .args([
                "-map",
                "0:v:0",
                "-map",
                "1:a:0",
                "-map",
                "2:s:0",
                "-c:v",
                "ffv1",
                "-c:a",
                "pcm_s16le",
                "-c:s",
                "srt",
                "-y",
            ])
            .arg(&path)
            .output()
            .expect("ffmpeg is available with the required libmpv dependency");
        assert!(
            output.status.success(),
            "ffmpeg could not create the subtitle-track fixture: {}",
            String::from_utf8_lossy(&output.stderr),
        );

        let mpv = Mpv::with_initializer(|initializer| {
            initializer.set_option("vo", "null")?;
            initializer.set_option("audio", "no")?;
            initializer.set_option("sub", "no")?;
            initializer.set_option("sub-auto", "no")?;
            initializer.set_option("pause", "no")?;
            Ok(())
        })
        .unwrap();
        mpv.command("loadfile", &[&path.to_string_lossy(), "replace"])
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(3);
        let mut file_loaded = false;
        let mut video_reconfigured = false;
        let mut playback_restarted = false;
        while Instant::now() < deadline
            && !(file_loaded && video_reconfigured && playback_restarted)
        {
            match mpv.wait_event(0.1) {
                Some(Ok(Event::FileLoaded)) => file_loaded = true,
                Some(Ok(Event::VideoReconfig)) => video_reconfigured = true,
                Some(Ok(Event::PlaybackRestart)) => playback_restarted = true,
                Some(Err(error)) => panic!("libmpv event failed: {error}"),
                _ => {}
            }
        }

        assert!(file_loaded, "subtitle-track fixture did not load");
        assert!(video_reconfigured, "video track was not configured");
        assert!(playback_restarted, "audio/video playback did not start");
        assert_eq!(mpv.get_property::<i64>("track-list/count").unwrap(), 3);
    }

    #[test]
    fn libmpv_absolute_percent_seek_reaches_requested_attachment_position() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("vendor/libmpv2-rs/test-data/jellyfish.mp4");
        let mpv = Mpv::with_initializer(|initializer| {
            initializer.set_option("vo", "null")?;
            initializer.set_option("audio", "no")?;
            initializer.set_option("pause", "yes")?;
            Ok(())
        })
        .unwrap();
        mpv.command("loadfile", &[&path.to_string_lossy(), "replace"])
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(3);
        let duration = loop {
            assert!(Instant::now() < deadline, "timed out opening seek fixture");
            match mpv.wait_event(0.1) {
                Some(Ok(Event::FileLoaded)) => {
                    break mpv.get_property::<f64>("duration").unwrap();
                }
                Some(Err(error)) => panic!("libmpv event failed: {error}"),
                _ => {}
            }
        };

        mpv.command("seek", &["50", "absolute-percent+exact"])
            .unwrap();
        let target = duration * 0.5;
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            assert!(Instant::now() < deadline, "timed out seeking attachment");
            let _ = mpv.wait_event(0.05);
            let position = mpv.get_property::<f64>("time-pos").unwrap_or_default();
            if position >= target - 0.25 {
                assert!(position <= target + 0.25, "seek landed at {position}");
                break;
            }
        }
    }

    #[test]
    fn ignores_callback_without_frame_update() {
        assert_eq!(render_action(0, None, true), RenderAction::None);
    }

    #[test]
    fn vulkan_decode_requires_core_and_matching_codec_extensions() {
        let h264 = [
            c"VK_KHR_video_queue",
            c"VK_KHR_video_decode_queue",
            c"VK_KHR_video_decode_h264",
        ];
        assert!(supports_vulkan_video_decode(
            &h264,
            vk::QueueFlags::VIDEO_DECODE_KHR,
            Some("avc1.64001F")
        ));
        assert!(!supports_vulkan_video_decode(
            &h264,
            vk::QueueFlags::VIDEO_DECODE_KHR,
            Some("hvc1.1.6.L93")
        ));
        assert!(!supports_vulkan_video_decode(
            &[c"VK_KHR_video_decode_h264"],
            vk::QueueFlags::VIDEO_DECODE_KHR,
            Some("h264")
        ));
        assert!(!supports_vulkan_video_decode(
            &h264,
            vk::QueueFlags::GRAPHICS,
            Some("h264")
        ));
    }

    #[test]
    fn skips_exact_repeat_when_previous_frame_exists() {
        assert_eq!(
            render_action(u64::from(mpv_render_update::Frame), Some(info(4)), true,),
            RenderAction::Skip
        );
    }

    #[test]
    fn renders_repeat_when_surface_has_no_previous_frame() {
        assert_eq!(
            render_action(u64::from(mpv_render_update::Frame), Some(info(4)), false,),
            RenderAction::Render
        );
    }

    #[test]
    fn stale_frame_update_does_not_consume_seek_invalidation() {
        let invalidation = SeekFrameInvalidation::default();
        let updates = u64::from(mpv_render_update::Frame);
        let repeat = Some(info(4));
        invalidation.invalidate();

        assert!(!invalidation.take_for_update(0));

        let stale_has_frame = !invalidation.take_for_update(updates);
        assert_eq!(
            render_action(updates, repeat, stale_has_frame),
            RenderAction::Render,
        );

        let post_seek_has_frame = !invalidation.take_for_update(updates);
        assert_eq!(
            render_action(updates, repeat, post_seek_has_frame),
            RenderAction::Render,
        );

        let settled_has_frame = !invalidation.take_for_update(updates);
        assert_eq!(
            render_action(updates, repeat, settled_has_frame),
            RenderAction::Skip,
        );
    }

    #[test]
    fn first_texture_generation_renders_the_pending_initial_frame() {
        assert!(should_render_after_resize(true, true, false));
        assert!(!should_render_after_resize(false, true, false));
    }

    #[test]
    fn redraw_is_not_discarded_as_repeat() {
        assert_eq!(
            render_action(u64::from(mpv_render_update::Frame), Some(info(4 | 2)), true,),
            RenderAction::Render
        );
    }

    #[test]
    fn attachment_scrub_seek_modes_match_native_mpv_flags() {
        assert_eq!(
            SeekMode::Exact.absolute_percent_flag(),
            "absolute-percent+exact"
        );
        assert_eq!(
            SeekMode::Keyframes.absolute_percent_flag(),
            "absolute-percent"
        );
    }
}
