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
    VideoTextureGeneration, VulkanQueueLocks, VulkanVideoDevice, VulkanVideoTexture,
    WgpuVideoSurface,
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
const AUDIO_POSITION_WAKE_INTERVAL: Duration = Duration::from_millis(100);
const INITIAL_RENDER_TRACE_LIMIT: u64 = 8;
static NEXT_MPV_PLAYER_ID: AtomicU64 = AtomicU64::new(1);
#[cfg(any(test, feature = "diagnostic-logs"))]
static NEXT_MPV_LOAD_ID: AtomicU64 = AtomicU64::new(1);
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
    player_id: u64,
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

/// A headless libmpv client for streamed audio attachments.
///
/// Audio shares the attachment protocol and playback state machine with video,
/// but deliberately owns no GPUI surface or render thread.
pub(crate) struct MpvAudioPlayer {
    player_id: u64,
    control_sender: mpsc::Sender<ControlCommand>,
    control_thread: Option<thread::JoinHandle<()>>,
    playback: Arc<SharedPlaybackState>,
    errors: mpsc::Receiver<String>,
    requested_paused: bool,
    mpv: Arc<Mpv>,
}

impl MpvPlayer {
    pub(crate) fn new_attachment(
        gpui_wakeup: AsyncSender<()>,
        preferred_backend: Option<AttachmentRenderBackend>,
        source_registry: crate::attachment_source::AttachmentSourceRegistry,
    ) -> Result<(Self, AttachmentRenderBackend)> {
        Self::new_internal(
            gpui_wakeup,
            false,
            None,
            preferred_backend,
            None,
            Some(source_registry),
        )
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
            None,
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
        source_registry: Option<crate::attachment_source::AttachmentSourceRegistry>,
    ) -> Result<(Self, AttachmentRenderBackend)> {
        let player_id = NEXT_MPV_PLAYER_ID.fetch_add(1, Ordering::Relaxed);
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
        kvlog::info!(
            "video player construction started",
            player_id,
            live,
            preferred_backend = preferred_backend_name
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
        let supports_vulkan_decode = native_candidate
            .as_ref()
            .and_then(|candidate| candidate.as_ref().ok())
            .is_some_and(|native| {
                let queue_family_properties = unsafe {
                    native
                        .instance
                        .get_physical_device_queue_family_properties(native.physical_device)
                };
                supports_vulkan_video_decode(
                    &native.device_extensions,
                    &native.enabled_queue_families,
                    &queue_family_properties,
                    native.synchronization2,
                    native.video_maintenance1,
                    live_codec,
                )
            });
        let default_hwdec = if live {
            if supports_vulkan_decode {
                "vulkan"
            } else if cfg!(target_os = "linux") {
                // Rendering through Vulkan does not imply support for Vulkan
                // Video. Prefer the Linux copy decoder that auto-probing chose
                // successfully in practice, without first consuming the only
                // keyframe in failed Vulkan and CUDA decoder attempts.
                "vaapi-copy,auto-copy-safe"
            } else {
                "auto-copy-safe"
            }
        } else if supports_vulkan_decode {
            attachment_hwdec_policy(true)
        } else {
            attachment_hwdec_policy(false)
        };
        let hwdec = if live {
            std::env::var("CHATT_LIVE_HWDEC")
                .ok()
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| default_hwdec.to_owned())
        } else {
            default_hwdec.to_owned()
        };
        if !live {
            kvlog::info!(
                "attachment decoder policy selected",
                player_id,
                vulkan_video_usable = supports_vulkan_decode,
                hwdec = %hwdec
            );
        }
        let require_direct_vulkan = supports_vulkan_decode && hwdec == "vulkan";
        kvlog::info!(
            "initializing embedded libmpv",
            player_id,
            live,
            hwdec = %hwdec,
            codec = live_codec.unwrap_or("probe-at-load"),
            vaapi_device = vaapi_device.as_deref().unwrap_or("none"),
            forced_software_render = force_live_software,
            direct_vulkan_required = require_direct_vulkan
        );
        let mpv = Mpv::with_initializer(|initializer| {
            macro_rules! set_option {
                ($name:literal, $value:expr) => {
                    if let Err(error) = initializer.set_option($name, $value) {
                        kvlog::error!(
                            "libmpv option rejected",
                            name = $name,
                            err = %error
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
            if require_direct_vulkan {
                // A capable shared Vulkan device must not silently regress to
                // decoded frames in system RAM followed by a GPU upload.
                set_option!("hwdec-software-fallback", "no");
            }
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
                //
                // This disables FFmpeg frame threading. The Vulkan libmpv
                // backend therefore has to return its mapped source frame
                // immediately after submission so inter-frame decode can
                // reacquire a reference frame's update mutex.
                set_option!("vd-lavc-low-latency", "yes");
                set_option!("interpolation", "no");
                set_option!("stream-buffer-size", "4k");
            }
            Ok(())
        })
        .map_err(|error| {
            kvlog::error!("embedded libmpv initialization failed", live, err = %error);
            error
        })?;
        kvlog::info!("embedded libmpv initialized", player_id, live);
        let mpv = Arc::new(mpv);
        if let Some(registry) = source_registry {
            crate::attachment_source::register_mpv_attachment_protocol(&mpv, registry)?;
        }

        observe_playback_properties(&mpv)?;
        mpv.observe_property("hwdec-current", Format::String, 6)
            .context("observe mpv property hwdec-current")?;
        mpv.observe_property("video-codec", Format::String, 7)
            .context("observe mpv property video-codec")?;
        mpv.observe_property("current-vo", Format::String, 8)
            .context("observe mpv property current-vo")?;
        mpv.observe_property("hwdec-interop", Format::String, 9)
            .context("observe mpv property hwdec-interop")?;
        kvlog::info!("libmpv playback properties registered", player_id, live);

        let mpv_log_level = crate::logger::native_mpv_log_level();
        mpv.request_log_messages(mpv_log_level)
            .with_context(|| format!("request native mpv log level {mpv_log_level}"))?;
        kvlog::info!(
            "native mpv logging enabled",
            player_id,
            min_level = %mpv_log_level
        );
        if live {
            kvlog::info!(
                "live playback latency mode enabled",
                cache = false,
                demux_readahead_seconds = 0u32,
                hwdec_copy_delay_frames = 0u32,
                latest_frame = true
            );
        }

        let (render_sender, render_messages) = mpsc::channel();

        let (mut backend, selected_backend) = match preferred_backend {
            Some(AttachmentRenderBackend::Software) => {
                let context = mpv
                    .create_software_render_context(live)
                    .context("create preferred libmpv software render context")?;
                kvlog::info!(
                    "video render backend selected",
                    player_id,
                    backend = "software",
                    upload = "wgpu",
                    latest_frame = live,
                    cached_decision = true
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
                kvlog::info!(
                    "creating libmpv Vulkan render context",
                    player_id,
                    live,
                    cached_decision
                );
                match native.and_then(|native| {
                    create_vulkan_context(&mpv, &native, live).map(|context| (context, native))
                }) {
                    Ok((context, native)) => {
                        kvlog::info!(
                            "video render backend selected",
                            player_id,
                            backend = "vulkan",
                            sharing = "wgpu-device",
                            latest_frame = live,
                            cached_decision
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
                        kvlog::warn!(
                            "Vulkan libmpv interop unavailable; using software fallback",
                            err = %error
                        );
                        let context = mpv.create_software_render_context(live).context(
                            "create libmpv software render context after Vulkan fallback",
                        )?;
                        kvlog::info!(
                            "video render backend selected",
                            player_id,
                            backend = "software",
                            upload = "wgpu",
                            latest_frame = live
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
        let render_sizing = fixed_render_size
            .map_or(RenderSizing::PendingFrame, |(width, height)| {
                RenderSizing::Fixed { width, height }
            });
        let render_thread = thread::Builder::new()
            .name("mpv-render".into())
            .spawn(move || {
                render_worker(
                    player_id,
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
                    render_sizing,
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
                        player_id,
                        control_mpv,
                        control_commands,
                        control_playback,
                        gpui_wakeup,
                        control_error_sender,
                        Some(RenderControl {
                            sender: control_render_sender,
                            initial_size: fixed_render_size,
                            frame_invalidated: control_frame_invalidated,
                        }),
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

        kvlog::info!(
            "video player construction completed",
            player_id,
            live,
            backend = selected_backend.name()
        );

        Ok((
            Self {
                player_id,
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
        let startup = StartupLogContext::new(self.player_id);
        self.playback.reset();
        self.requested_paused = paused;
        self.render_sender
            .send(RenderMessage::Reset { startup })
            .map_err(|_| anyhow!("mpv render thread stopped"))?;
        self.send_control(ControlCommand::Load {
            startup,
            path: path.to_owned(),
            paused,
            volume: volume.clamp(0.0, 100.0),
            speed: 1.0,
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

    pub(crate) fn adjust_video(&self, adjustment: VideoAdjustment) -> Result<()> {
        self.send_control(ControlCommand::AdjustVideo(adjustment))
    }

    pub(crate) fn set_video_effect(&self, effect: VideoEffect, value: f64) -> Result<()> {
        self.send_control(ControlCommand::SetVideoEffect {
            effect,
            value: value.clamp(-100.0, 100.0),
        })
    }

    pub(crate) fn step_frame(&mut self, backwards: bool) -> Result<()> {
        self.requested_paused = true;
        self.playback.paused.store(true, Ordering::Release);
        self.send_control(ControlCommand::StepFrame { backwards })
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

fn observe_playback_properties(mpv: &Mpv) -> Result<()> {
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
    Ok(())
}

impl MpvAudioPlayer {
    pub(crate) fn new_attachment(
        gpui_wakeup: AsyncSender<()>,
        source_registry: crate::attachment_source::AttachmentSourceRegistry,
    ) -> Result<Self> {
        Self::new_attachment_with_audio_output(gpui_wakeup, source_registry, None)
    }

    fn new_attachment_with_audio_output(
        gpui_wakeup: AsyncSender<()>,
        source_registry: crate::attachment_source::AttachmentSourceRegistry,
        audio_output: Option<&str>,
    ) -> Result<Self> {
        let player_id = NEXT_MPV_PLAYER_ID.fetch_add(1, Ordering::Relaxed);
        kvlog::info!("audio player construction started", player_id);
        let mpv = Mpv::with_initializer(|initializer| {
            macro_rules! set_option {
                ($name:literal, $value:expr) => {
                    if let Err(error) = initializer.set_option($name, $value) {
                        kvlog::error!(
                            "libmpv audio option rejected",
                            name = $name,
                            err = %error
                        );
                        return Err(error);
                    }
                };
            }
            set_option!("vo", "null");
            set_option!("video", "no");
            // A scrub can briefly reach EOF before the pointer moves back.
            // Retain the loaded file so that backward seek still has a target.
            set_option!("keep-open", "yes");
            set_option!("idle", "yes");
            set_option!("sub", "no");
            set_option!("sub-auto", "no");
            set_option!("osd-level", "0");
            set_option!("profile", "fast");
            if let Some(audio_output) = audio_output {
                set_option!("ao", audio_output);
            }
            Ok(())
        })
        .context("initialize headless libmpv audio player")?;
        let mpv = Arc::new(mpv);
        crate::attachment_source::register_mpv_attachment_protocol(&mpv, source_registry)?;
        observe_playback_properties(&mpv)?;
        let mpv_log_level = crate::logger::native_mpv_log_level();
        mpv.request_log_messages(mpv_log_level)
            .with_context(|| format!("request native mpv log level {mpv_log_level}"))?;

        let (control_sender, control_commands) = mpsc::channel();
        let playback = Arc::new(SharedPlaybackState::default());
        let control_playback = playback.clone();
        let control_mpv = mpv.clone();
        let (error_sender, errors) = mpsc::channel();
        let control_thread = thread::Builder::new()
            .name("mpv-audio-control".into())
            .spawn(move || {
                control_worker(
                    player_id,
                    control_mpv,
                    control_commands,
                    control_playback,
                    gpui_wakeup,
                    error_sender,
                    None,
                );
            })
            .context("spawn mpv audio control thread")?;
        kvlog::info!("audio player construction completed", player_id);
        Ok(Self {
            player_id,
            control_sender,
            control_thread: Some(control_thread),
            playback,
            errors,
            requested_paused: false,
            mpv,
        })
    }

    pub(crate) fn load_at(
        &mut self,
        path: &str,
        paused: bool,
        volume: f64,
        speed: f64,
        position: f64,
    ) -> Result<()> {
        self.playback.reset();
        self.requested_paused = paused;
        self.send_control(ControlCommand::Load {
            startup: StartupLogContext::new(self.player_id),
            path: path.to_owned(),
            paused,
            volume: volume.clamp(0.0, 100.0),
            speed: speed.clamp(0.25, 4.0),
            position: position.max(0.0),
        })
    }

    pub(crate) fn toggle_pause(&mut self) -> Result<bool> {
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

    pub(crate) fn set_volume(&self, volume: f64) -> Result<()> {
        self.send_control(ControlCommand::SetVolume(volume.clamp(0.0, 100.0)))
    }

    pub(crate) fn set_speed(&self, speed: f64) -> Result<()> {
        self.send_control(ControlCommand::SetSpeed(speed.clamp(0.25, 4.0)))
    }

    pub(crate) fn stop(&mut self) -> Result<()> {
        self.requested_paused = true;
        self.playback.reset();
        self.send_control(ControlCommand::Stop)
    }

    pub(crate) fn drain_events(&mut self) -> Result<PlaybackState> {
        if let Ok(error) = self.errors.try_recv() {
            bail!(error);
        }
        Ok(self.playback.snapshot())
    }

    fn send_control(&self, command: ControlCommand) -> Result<()> {
        self.control_sender
            .send(command)
            .map_err(|_| anyhow!("mpv audio control thread stopped"))?;
        self.mpv.wakeup();
        Ok(())
    }
}

impl Drop for MpvAudioPlayer {
    fn drop(&mut self) {
        kvlog::info!("stopping mpv audio control thread");
        shutdown_owned_mpv_control(&self.control_sender, &mut self.control_thread, &self.mpv);
        kvlog::info!("mpv audio control thread stopped");
    }
}

impl Drop for MpvPlayer {
    fn drop(&mut self) {
        kvlog::info!("stopping mpv player threads");
        self.live_source.take();
        self.render_stopping.store(true, Ordering::Release);
        let _ = self.render_sender.send(RenderMessage::Shutdown);
        if let Some(thread) = self.render_thread.take() {
            let _ = thread.join();
        }

        shutdown_owned_mpv_control(&self.control_sender, &mut self.control_thread, &self.mpv);
        kvlog::info!("mpv player threads stopped");
    }
}

fn shutdown_owned_mpv_control(
    control_sender: &mpsc::Sender<ControlCommand>,
    control_thread: &mut Option<thread::JoinHandle<()>>,
    mpv: &Mpv,
) {
    let _ = control_sender.send(ControlCommand::Shutdown);
    mpv.wakeup();
    if let Some(thread) = control_thread.take() {
        let _ = thread.join();
    }
    // `mpv_destroy` only detaches this client handle and permits the core,
    // its internal clients, and registered stream callbacks to outlive it.
    // All application workers are joined now, so synchronously terminate the
    // owned core when the final Arc is released.
    mpv.terminate_on_drop();
}

struct WgpuQueueLock {
    inner: Arc<VulkanQueueLocks>,
}

impl WgpuQueueLock {
    fn new(inner: Arc<VulkanQueueLocks>) -> Self {
        Self { inner }
    }
}

impl VulkanQueueLock for WgpuQueueLock {
    fn lock(&self, family: u32, index: u32) {
        self.inner.lock(family, index);
    }

    unsafe fn unlock(&self, family: u32, index: u32) {
        unsafe { self.inner.unlock(family, index) };
    }
}

fn attachment_hwdec_policy(supports_vulkan_decode: bool) -> &'static str {
    if supports_vulkan_decode {
        "vulkan,auto-safe"
    } else if cfg!(target_os = "linux") {
        "vaapi-copy,auto-copy-safe"
    } else {
        "auto-copy-safe"
    }
}

fn supports_vulkan_video_decode(
    device_extensions: &[&CStr],
    enabled_queue_families: &[(u32, u32)],
    queue_family_properties: &[vk::QueueFamilyProperties],
    synchronization2: bool,
    video_maintenance1: bool,
    codec: Option<&str>,
) -> bool {
    let has = |required: &CStr| {
        device_extensions
            .iter()
            .any(|extension| *extension == required)
    };
    let has_decode_queue = enabled_queue_families.iter().any(|&(family, count)| {
        count > 0
            && queue_family_properties
                .get(family as usize)
                .is_some_and(|properties| {
                    properties
                        .queue_flags
                        .contains(vk::QueueFlags::VIDEO_DECODE_KHR)
                })
    });
    if !has_decode_queue
        || !synchronization2
        || !video_maintenance1
        || !has(c"VK_KHR_video_queue")
        || !has(c"VK_KHR_video_decode_queue")
        || !has(c"VK_KHR_video_maintenance1")
    {
        return false;
    }
    let supports_h264 = has(c"VK_KHR_video_decode_h264");
    let supports_h265 = has(c"VK_KHR_video_decode_h265");
    let supports_av1 = has(c"VK_KHR_video_decode_av1");
    match codec {
        Some(codec) if codec.starts_with("avc1.") || codec.eq_ignore_ascii_case("h264") => {
            supports_h264
        }
        Some(codec)
            if codec.starts_with("hvc1.")
                || codec.starts_with("hev1.")
                || codec.eq_ignore_ascii_case("hevc") =>
        {
            supports_h265
        }
        Some(codec) if codec.starts_with("av01.") || codec.eq_ignore_ascii_case("av1") => {
            supports_av1
        }
        None => supports_h264 || supports_h265 || supports_av1,
        _ => false,
    }
}

fn probe_vulkan_device(surface: &WgpuVideoSurface) -> Result<Arc<VulkanVideoDevice>> {
    let native = Arc::new(surface.vulkan_device()?);
    #[cfg(feature = "diagnostic-logs")]
    let queue_family_properties = unsafe {
        native
            .instance
            .get_physical_device_queue_family_properties(native.physical_device)
    };
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
    #[cfg(feature = "diagnostic-logs")]
    let has_vulkan_video_core = [c"VK_KHR_video_queue", c"VK_KHR_video_decode_queue"]
        .iter()
        .all(|required| {
            native
                .device_extensions
                .iter()
                .any(|extension| extension == required)
        });
    #[cfg(feature = "diagnostic-logs")]
    let has_vulkan_video_h264 = native
        .device_extensions
        .iter()
        .any(|extension| *extension == c"VK_KHR_video_decode_h264");
    #[cfg(feature = "diagnostic-logs")]
    let has_vulkan_video_h265 = native
        .device_extensions
        .iter()
        .any(|extension| *extension == c"VK_KHR_video_decode_h265");
    #[cfg(feature = "diagnostic-logs")]
    let has_vulkan_video_av1 = native
        .device_extensions
        .iter()
        .any(|extension| *extension == c"VK_KHR_video_decode_av1");
    #[cfg(feature = "diagnostic-logs")]
    let render_queue_has_video_decode = native
        .queue_flags
        .contains(vk::QueueFlags::VIDEO_DECODE_KHR);
    #[cfg(feature = "diagnostic-logs")]
    let enabled_video_decode_queue_families = native
        .enabled_queue_families
        .iter()
        .filter_map(|&(family, count)| {
            (count > 0
                && queue_family_properties
                    .get(family as usize)
                    .is_some_and(|properties| {
                        properties
                            .queue_flags
                            .contains(vk::QueueFlags::VIDEO_DECODE_KHR)
                    }))
            .then_some(family.to_string())
        })
        .collect::<Vec<_>>()
        .join(",");
    #[cfg(feature = "diagnostic-logs")]
    let enabled_video_decode_queue_families = if enabled_video_decode_queue_families.is_empty() {
        "none".to_owned()
    } else {
        enabled_video_decode_queue_families
    };
    #[cfg(feature = "diagnostic-logs")]
    let drm_render_node = native
        .drm_render_node
        .as_deref()
        .map_or_else(|| "none".into(), |path| path.display().to_string());
    #[cfg(feature = "diagnostic-logs")]
    if crate::logger::render_logging_enabled() {
        kvlog::info!(
            "importing GPUI Vulkan device into libmpv",
            group = "render",
            queue_family = native.queue_family,
            queue_index = native.queue_index,
            render_queue_video_decode = render_queue_has_video_decode,
            enabled_queue_families = native.enabled_queue_families.len(),
            enabled_video_decode_queue_families = %enabled_video_decode_queue_families,
            synchronization2 = native.synchronization2,
            video_maintenance1 = native.video_maintenance1,
            instance_extensions = native.instance_extensions.len(),
            device_extensions = native.device_extensions.len(),
            external_memory_fd = has_external_memory_fd,
            dma_buf = has_dma_buf,
            drm_modifiers = has_drm_modifiers,
            drm_render_node = %drm_render_node,
            vulkan_video_core = has_vulkan_video_core,
            vulkan_video_h264 = has_vulkan_video_h264,
            vulkan_video_h265 = has_vulkan_video_h265,
            vulkan_video_av1 = has_vulkan_video_av1
        );
    }
    if cfg!(target_os = "linux") && !(has_external_memory_fd && has_dma_buf && has_drm_modifiers) {
        kvlog::warn!(
            "Vulkan device lacks full Linux dma-buf import support; DRM/VAAPI hardware-frame interop may be unavailable, but native Vulkan Video remains device-local"
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
                kvlog::warn!(
                    "could not open matching DRM render node for VAAPI interop",
                    path = %path.display(),
                    err = %error
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
            synchronization2: native.synchronization2,
            video_maintenance1: native.video_maintenance1,
        },
        graphics_queue: queue,
        compute_queue: queue,
        transfer_queue: queue,
        enabled_queue_families,
        queue_lock: Arc::new(WgpuQueueLock::new(native.queue_locks.clone())),
        drm_render_fd,
        latest_frame,
    })
    .context("import GPUI's Vulkan device into libmpv")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SeekMode {
    Exact,
    // Retained for approximate scrubbing, where avoiding exact frame decoding
    // is more important than landing on the precise requested timestamp.
    #[allow(dead_code)]
    Keyframes,
}

impl SeekMode {
    #[cfg(feature = "diagnostic-logs")]
    fn as_str(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::Keyframes => "keyframes",
        }
    }

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

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum VideoAdjustment {
    Contrast(f64),
    Brightness(f64),
    Gamma(f64),
    Saturation(f64),
    PlaybackSpeed(f64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VideoEffect {
    Contrast,
    Brightness,
    Gamma,
    Saturation,
}

impl VideoEffect {
    pub(crate) const ALL: [Self; 4] = [
        Self::Contrast,
        Self::Brightness,
        Self::Gamma,
        Self::Saturation,
    ];

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Contrast => "Contrast",
            Self::Brightness => "Brightness",
            Self::Gamma => "Gamma",
            Self::Saturation => "Saturation",
        }
    }

    const fn property(self) -> &'static str {
        match self {
            Self::Contrast => "contrast",
            Self::Brightness => "brightness",
            Self::Gamma => "gamma",
            Self::Saturation => "saturation",
        }
    }

    pub(crate) const fn index(self) -> usize {
        match self {
            Self::Contrast => 0,
            Self::Brightness => 1,
            Self::Gamma => 2,
            Self::Saturation => 3,
        }
    }
}

impl VideoAdjustment {
    fn mpv_command(self) -> (&'static str, &'static str, f64) {
        match self {
            Self::Contrast(delta) => ("add", "contrast", delta),
            Self::Brightness(delta) => ("add", "brightness", delta),
            Self::Gamma(delta) => ("add", "gamma", delta),
            Self::Saturation(delta) => ("add", "saturation", delta),
            Self::PlaybackSpeed(factor) => ("multiply", "speed", factor),
        }
    }

    pub(crate) const fn effect_delta(self) -> Option<(VideoEffect, f64)> {
        match self {
            Self::Contrast(delta) => Some((VideoEffect::Contrast, delta)),
            Self::Brightness(delta) => Some((VideoEffect::Brightness, delta)),
            Self::Gamma(delta) => Some((VideoEffect::Gamma, delta)),
            Self::Saturation(delta) => Some((VideoEffect::Saturation, delta)),
            Self::PlaybackSpeed(_) => None,
        }
    }
}

pub(crate) enum ControlCommand {
    Load {
        startup: StartupLogContext,
        path: String,
        paused: bool,
        volume: f64,
        speed: f64,
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
    SetSpeed(f64),
    AdjustVideo(VideoAdjustment),
    SetVideoEffect {
        effect: VideoEffect,
        value: f64,
    },
    StepFrame {
        backwards: bool,
    },
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
    Reset {
        startup: StartupLogContext,
    },
    Shutdown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RenderSizing {
    Fixed { width: u32, height: u32 },
    PendingFrame,
}

#[derive(Clone, Copy)]
pub(crate) struct StartupLogContext {
    #[cfg(feature = "diagnostic-logs")]
    player_id: u64,
    #[cfg(any(test, feature = "diagnostic-logs"))]
    load_id: u64,
    #[cfg(feature = "diagnostic-logs")]
    started_at: Instant,
}

impl StartupLogContext {
    fn new(player_id: u64) -> Self {
        #[cfg(feature = "diagnostic-logs")]
        {
            return Self {
                player_id,
                load_id: NEXT_MPV_LOAD_ID.fetch_add(1, Ordering::Relaxed),
                started_at: Instant::now(),
            };
        }
        #[cfg(all(test, not(feature = "diagnostic-logs")))]
        {
            let _ = player_id;
            return Self {
                load_id: NEXT_MPV_LOAD_ID.fetch_add(1, Ordering::Relaxed),
            };
        }
        #[cfg(not(any(test, feature = "diagnostic-logs")))]
        {
            let _ = player_id;
            Self {}
        }
    }

    #[cfg(test)]
    fn for_test(_player_id: u64, load_id: u64) -> Self {
        Self {
            #[cfg(feature = "diagnostic-logs")]
            player_id: _player_id,
            load_id,
            #[cfg(feature = "diagnostic-logs")]
            started_at: Instant::now(),
        }
    }

    #[cfg(feature = "diagnostic-logs")]
    fn elapsed_ms(self) -> f64 {
        self.started_at.elapsed().as_secs_f64() * 1_000.0
    }
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

    fn configured_size(&self) -> Option<(u32, u32)> {
        match self {
            Self::Vulkan { generation, .. } | Self::Software { generation, .. } => generation
                .as_ref()
                .and_then(|generation| generation.textures.first())
                .map(|texture| (texture.width(), texture.height())),
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
                    #[cfg(feature = "diagnostic-logs")]
                    if crate::logger::render_logging_enabled() {
                        kvlog::info!(
                            "retiring video texture generation",
                            group = "render",
                            backend = "vulkan",
                            generation = old.id,
                            textures = old.textures.len()
                        );
                    }
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
                #[cfg(feature = "diagnostic-logs")]
                if crate::logger::render_logging_enabled() {
                    kvlog::info!(
                        "video texture generation ready",
                        group = "render",
                        backend = "vulkan",
                        generation = new_generation.id,
                        width,
                        height,
                        textures = new_generation.textures.len()
                    );
                }
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
                #[cfg(feature = "diagnostic-logs")]
                if crate::logger::render_logging_enabled() {
                    kvlog::info!(
                        "video texture generation ready",
                        group = "render",
                        backend = "software",
                        generation = new_generation.id,
                        width,
                        height,
                        textures = new_generation.textures.len()
                    );
                }
                *generation = Some(new_generation);
                *next_texture = 0;
            }
        }
        Ok(true)
    }

    fn prepare_pending_frame(&mut self, surface: &WgpuVideoSurface) -> Result<Option<(u32, u32)>> {
        let Some((width, height)) = self.context().next_frame_video_size().with_context(|| {
            format!(
                "read pending video size from {} render context",
                self.name()
            )
        })?
        else {
            return Ok(None);
        };
        self.resize(surface, width, height)?;
        Ok(Some((width, height)))
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
                let generation = generation
                    .as_ref()
                    .context("Vulkan video render backend has no texture generation")?;
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
                let generation = generation
                    .as_ref()
                    .context("software video render backend has no texture generation")?;
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
    startup: Option<StartupLogContext>,
    started_at: Instant,
    last_summary: Instant,
    last_pressure_log: Option<Instant>,
    callbacks: u64,
    rendered: u64,
    repeats: u64,
    callbacks_without_frames: u64,
    ring_busy: u64,
    resizes: u64,
    errors: u64,
}

impl RenderDiagnostics {
    fn new() -> Self {
        let now = Instant::now();
        Self {
            startup: None,
            started_at: now,
            last_summary: now,
            last_pressure_log: None,
            callbacks: 0,
            rendered: 0,
            repeats: 0,
            callbacks_without_frames: 0,
            ring_busy: 0,
            resizes: 0,
            errors: 0,
        }
    }

    fn start_load(&mut self, startup: StartupLogContext) {
        self.startup = Some(startup);
    }

    fn note_first_render(&mut self, _operation: &str, _operation_started_at: Instant) {
        let Some(_startup) = self.startup.take() else {
            return;
        };
        #[cfg(feature = "diagnostic-logs")]
        if crate::logger::render_logging_enabled() {
            kvlog::info!(
                "mpv startup first render completed",
                group = "render",
                player_id = _startup.player_id,
                load_id = _startup.load_id,
                startup_elapsed_ms = _startup.elapsed_ms(),
                operation_elapsed_ms = _operation_started_at.elapsed().as_secs_f64() * 1_000.0,
                operation = _operation
            );
        }
    }

    fn note_ring_busy(&mut self, backend: &str, generation: u64) {
        self.ring_busy += 1;
        let now = Instant::now();
        if self
            .last_pressure_log
            .is_none_or(|last| now.duration_since(last) >= RENDER_PRESSURE_LOG_INTERVAL)
        {
            self.last_pressure_log = Some(now);
            kvlog::warn!(
                "all video textures are in use; deferring render request",
                backend,
                generation,
                busy_total = self.ring_busy
            );
        }
    }

    fn maybe_log_summary(&mut self, _backend: &str) {
        let now = Instant::now();
        if now.duration_since(self.last_summary) < RENDER_SUMMARY_INTERVAL {
            return;
        }
        self.last_summary = now;
        #[cfg(feature = "diagnostic-logs")]
        if crate::logger::render_logging_enabled() {
            kvlog::info!(
                "video render summary",
                group = "render",
                backend = _backend,
                uptime_ms = now.duration_since(self.started_at).as_secs_f64() * 1_000.0,
                callbacks = self.callbacks,
                rendered = self.rendered,
                repeats = self.repeats,
                no_frame = self.callbacks_without_frames,
                ring_busy = self.ring_busy,
                resizes = self.resizes,
                errors = self.errors
            );
        }
    }

    fn log_final(&self, backend: &str) {
        kvlog::info!(
            "video render worker stopped",
            backend,
            uptime_ms = self.started_at.elapsed().as_secs_f64() * 1_000.0,
            callbacks = self.callbacks,
            rendered = self.rendered,
            repeats = self.repeats,
            no_frame = self.callbacks_without_frames,
            ring_busy = self.ring_busy,
            resizes = self.resizes,
            errors = self.errors
        );
    }
}

struct RenderControl {
    sender: mpsc::Sender<RenderMessage>,
    initial_size: Option<(u32, u32)>,
    frame_invalidated: Arc<SeekFrameInvalidation>,
}

fn control_worker(
    player_id: u64,
    mpv: Arc<Mpv>,
    commands: mpsc::Receiver<ControlCommand>,
    playback: Arc<SharedPlaybackState>,
    gpui_wakeup: AsyncSender<()>,
    errors: mpsc::Sender<String>,
    render: Option<RenderControl>,
) {
    let rendered = render.is_some();
    kvlog::info!("mpv control worker started", player_id, rendered);
    let mut state = PlaybackState::default();
    let initial_render_size = render.as_ref().and_then(|render| render.initial_size);
    let mut pending_start = None;
    let delay_render_until_reconfiguration = initial_render_size.is_some();
    let mut render_enabled = false;
    let mut startup = None;
    let mut seek_started_at = None;
    let mut last_audio_position_wakeup = Instant::now()
        .checked_sub(AUDIO_POSITION_WAKE_INTERVAL)
        .unwrap_or_else(Instant::now);
    let mut deferred_command = None;
    loop {
        while let Some(mut command) = deferred_command.take().or_else(|| commands.try_recv().ok()) {
            if is_seek_command(&command) {
                let mut coalesced = 0u32;
                while let Ok(next) = commands.try_recv() {
                    if is_seek_command(&next) {
                        command = next;
                        coalesced = coalesced.saturating_add(1);
                    } else {
                        deferred_command = Some(next);
                        break;
                    }
                }
                if coalesced != 0 {
                    #[cfg(feature = "diagnostic-logs")]
                    if crate::logger::media_logging_enabled() {
                        kvlog::info!(
                            "coalesced stale media seek commands",
                            group = "media",
                            count = coalesced
                        );
                    }
                }
            }
            let result = match command {
                ControlCommand::Load {
                    startup: new_startup,
                    path,
                    paused,
                    volume,
                    speed,
                    position,
                } => {
                    #[cfg(feature = "diagnostic-logs")]
                    if crate::logger::media_logging_enabled() {
                        kvlog::info!(
                            "loading media",
                            group = "media",
                            player_id = new_startup.player_id,
                            load_id = new_startup.load_id,
                            startup_elapsed_ms = new_startup.elapsed_ms(),
                            path = %path
                        );
                    }
                    startup = Some(new_startup);
                    seek_started_at = None;
                    state = PlaybackState {
                        paused,
                        ..PlaybackState::default()
                    };
                    pending_start = (position > 0.0).then_some(position);
                    render_enabled = false;
                    playback.publish_control(state);
                    // `stop` is synchronous. Drain the events it enqueued
                    // before opening the replacement so a pooled core cannot
                    // mistake the previous file's FileLoaded/EndFile for the
                    // new session.
                    let result = mpv
                        .command("stop", &[])
                        .map(|()| while mpv.wait_event(0.0).is_some() {})
                        .and_then(|()| mpv.set_property("pause", paused))
                        .and_then(|()| mpv.set_property("volume", volume))
                        .and_then(|()| mpv.set_property("speed", speed))
                        .and_then(|()| mpv.command("loadfile", &[&path, "replace"]));
                    if result.is_ok() {
                        #[cfg(feature = "diagnostic-logs")]
                        if crate::logger::media_logging_enabled() {
                            kvlog::info!(
                                "mpv accepted loadfile",
                                group = "media",
                                player_id = new_startup.player_id,
                                load_id = new_startup.load_id,
                                startup_elapsed_ms = new_startup.elapsed_ms(),
                                paused,
                                volume,
                                speed,
                                start_position = position
                            );
                        }
                    }
                    result
                }
                ControlCommand::SetPause(paused) => {
                    #[cfg(feature = "diagnostic-logs")]
                    if crate::logger::media_logging_enabled() {
                        kvlog::info!("setting mpv pause", group = "media", paused);
                    }
                    mpv.set_property("pause", paused)
                }
                ControlCommand::SeekAbsolute { seconds, mode } => {
                    #[cfg(feature = "diagnostic-logs")]
                    if crate::logger::media_logging_enabled() {
                        kvlog::info!(
                            "seeking mpv",
                            group = "media",
                            seconds,
                            mode = mode.as_str()
                        );
                    }
                    seek_started_at = Some((Instant::now(), seconds, mode));
                    state.position = seconds;
                    state.finished = false;
                    playback.publish_control(state);
                    if let Some(render) = render.as_ref() {
                        render.frame_invalidated.invalidate();
                    }
                    mpv.command("seek", &[&seconds.to_string(), mode.absolute_flag()])
                }
                ControlCommand::SeekPercent {
                    percent,
                    position,
                    mode,
                } => {
                    #[cfg(feature = "diagnostic-logs")]
                    if crate::logger::media_logging_enabled() {
                        kvlog::info!(
                            "seeking mpv by percentage",
                            group = "media",
                            percent,
                            expected_seconds = position,
                            mode = mode.as_str()
                        );
                    }
                    seek_started_at = Some((Instant::now(), position, mode));
                    state.position = position;
                    state.finished = false;
                    playback.publish_control(state);
                    if let Some(render) = render.as_ref() {
                        render.frame_invalidated.invalidate();
                    }
                    mpv.command(
                        "seek",
                        &[&percent.to_string(), mode.absolute_percent_flag()],
                    )
                }
                ControlCommand::SetVolume(volume) => {
                    #[cfg(feature = "diagnostic-logs")]
                    if crate::logger::media_logging_enabled() {
                        kvlog::info!("setting mpv volume", group = "media", volume);
                    }
                    mpv.set_property("volume", volume)
                }
                ControlCommand::SetSpeed(speed) => {
                    #[cfg(feature = "diagnostic-logs")]
                    if crate::logger::media_logging_enabled() {
                        kvlog::info!("setting mpv speed", group = "media", speed);
                    }
                    mpv.set_property("speed", speed)
                }
                ControlCommand::AdjustVideo(adjustment) => {
                    let (command, property, amount) = adjustment.mpv_command();
                    #[cfg(feature = "diagnostic-logs")]
                    if crate::logger::media_logging_enabled() {
                        kvlog::info!(
                            "adjusting mpv video property",
                            group = "media",
                            property,
                            amount
                        );
                    }
                    mpv.command(command, &[property, &amount.to_string()])
                }
                ControlCommand::SetVideoEffect { effect, value } => {
                    let property = effect.property();
                    #[cfg(feature = "diagnostic-logs")]
                    if crate::logger::media_logging_enabled() {
                        kvlog::info!(
                            "setting mpv video property",
                            group = "media",
                            property,
                            value
                        );
                    }
                    mpv.set_property(property, value)
                }
                ControlCommand::StepFrame { backwards } => {
                    #[cfg(feature = "diagnostic-logs")]
                    if crate::logger::media_logging_enabled() {
                        kvlog::info!("stepping mpv video frame", group = "media", backwards);
                    }
                    state.paused = true;
                    playback.publish_control(state);
                    mpv.command(
                        if backwards {
                            "frame-back-step"
                        } else {
                            "frame-step"
                        },
                        &[],
                    )
                }
                ControlCommand::Stop => {
                    #[cfg(feature = "diagnostic-logs")]
                    if crate::logger::media_logging_enabled() {
                        kvlog::info!("stopping media while retaining mpv core", group = "media");
                    }
                    startup = None;
                    seek_started_at = None;
                    state = PlaybackState {
                        paused: true,
                        ..PlaybackState::default()
                    };
                    pending_start = None;
                    playback.publish_control(state);
                    mpv.command("stop", &[])
                }
                ControlCommand::DropBuffers => {
                    #[cfg(feature = "diagnostic-logs")]
                    if crate::logger::media_logging_enabled() {
                        kvlog::info!(
                            "dropping buffered live video at fresh keyframe",
                            group = "media"
                        );
                    }
                    mpv.command("drop-buffers", &[])
                }
                ControlCommand::Shutdown => {
                    kvlog::info!("mpv control worker stopped");
                    return;
                }
            };
            if let Err(error) = result {
                kvlog::error!("mpv control command failed", err = %error);
                let _ = errors.send(error.to_string());
                let _ = gpui_wakeup.try_send(());
            }
        }

        if let Some(event) = mpv.wait_event(-1.0) {
            if matches!(event.as_ref(), Ok(Event::FileLoaded))
                && let Some(position) = pending_start.take()
                && let Err(error) = mpv.command("seek", &[&position.to_string(), "absolute+exact"])
            {
                kvlog::warn!("failed to restore retained playback position", err = %error);
            }
            let file_loaded = matches!(event.as_ref(), Ok(Event::FileLoaded));
            let video_reconfigured = matches!(event.as_ref(), Ok(Event::VideoReconfig));
            let playback_restarted = matches!(event.as_ref(), Ok(Event::PlaybackRestart));
            let position_changed = matches!(
                event.as_ref(),
                Ok(Event::PropertyChange {
                    name: "time-pos",
                    change: PropertyData::Double(_),
                    ..
                })
            );
            if file_loaded && let Some(_startup) = startup {
                #[cfg(feature = "diagnostic-logs")]
                if crate::logger::media_logging_enabled() {
                    kvlog::info!(
                        "mpv media demux ready",
                        group = "media",
                        player_id = _startup.player_id,
                        load_id = _startup.load_id,
                        elapsed_ms = _startup.elapsed_ms()
                    );
                }
            }
            if playback_restarted {
                if let Some((_started_at, _target, _mode)) = seek_started_at.take() {
                    #[cfg(feature = "diagnostic-logs")]
                    if crate::logger::media_logging_enabled() {
                        kvlog::info!(
                            "mpv seek completed",
                            group = "media",
                            target_seconds = _target,
                            mode = _mode.as_str(),
                            elapsed_ms = _started_at.elapsed().as_secs_f64() * 1_000.0
                        );
                    }
                } else if let Some(_startup) = startup.take() {
                    #[cfg(feature = "diagnostic-logs")]
                    if crate::logger::media_logging_enabled() {
                        kvlog::info!(
                            "mpv initial playback ready",
                            group = "media",
                            player_id = _startup.player_id,
                            load_id = _startup.load_id,
                            elapsed_ms = _startup.elapsed_ms()
                        );
                    }
                }
            }
            if video_reconfigured && let Some(_startup) = startup {
                #[cfg(feature = "diagnostic-logs")]
                if crate::logger::render_logging_enabled() {
                    kvlog::info!(
                        "mpv startup video reconfiguration received",
                        group = "render",
                        player_id = _startup.player_id,
                        load_id = _startup.load_id,
                        startup_elapsed_ms = _startup.elapsed_ms()
                    );
                }
            }
            let should_enable_render = rendered
                && !render_enabled
                && ((!delay_render_until_reconfiguration && file_loaded)
                    || (delay_render_until_reconfiguration && video_reconfigured));
            if should_enable_render {
                kvlog::info!(
                    "enabling video rendering",
                    trigger = if video_reconfigured {
                        "video-reconfiguration"
                    } else {
                        "file-loaded"
                    },
                    preserved_frame = true
                );
                let render_sender = &render
                    .as_ref()
                    .expect("rendered control worker has render state")
                    .sender;
                let enabled = render_sender.send(RenderMessage::Enable).is_ok()
                    && render_sender.send(RenderMessage::Update).is_ok();
                if enabled {
                    render_enabled = true;
                } else {
                    let message = "mpv render thread stopped while enabling the new media";
                    kvlog::error!("could not enable video rendering", err = message);
                    let _ = errors.send(message.into());
                }
            }
            let mut notify_gpui = match apply_event(event, &mut state) {
                Ok(notify_gpui) => notify_gpui,
                Err(error) => {
                    kvlog::error!("mpv event handling failed", err = %error);
                    let _ = errors.send(format!("{error:#}"));
                    true
                }
            };
            if !rendered
                && position_changed
                && last_audio_position_wakeup.elapsed() >= AUDIO_POSITION_WAKE_INTERVAL
            {
                last_audio_position_wakeup = Instant::now();
                notify_gpui = true;
            }
            playback.publish_control(state);
            if notify_gpui {
                let _ = gpui_wakeup.try_send(());
            }
        }
    }
}

fn is_seek_command(command: &ControlCommand) -> bool {
    matches!(
        command,
        ControlCommand::SeekAbsolute { .. } | ControlCommand::SeekPercent { .. }
    )
}

#[cfg(test)]
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

#[cfg(test)]
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
                    kvlog::info!(
                        "mpv decoder selected",
                        hardware_decoder = %value,
                        transfer = "gpu-to-cpu-before-render"
                    );
                } else if value == "vulkan" {
                    kvlog::info!(
                        "mpv decoder selected",
                        hardware_decoder = %value,
                        transfer = "vulkan-hardware-frames-direct",
                        no_system_memory_round_trip = true
                    );
                } else {
                    kvlog::info!("mpv decoder selected", hardware_decoder = %value);
                }
                false
            }
            ("video-codec", PropertyData::Str(value)) => {
                kvlog::info!("mpv video stream selected", codec = %value);
                false
            }
            ("current-vo", PropertyData::Str(value)) => {
                kvlog::info!("mpv video output selected", output = %value);
                false
            }
            ("hwdec-interop", PropertyData::Str(value)) => {
                kvlog::info!("mpv hardware frame interop available", interop = %value);
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
            kvlog::info!("mpv started opening media");
            let was_finished = state.finished;
            let was_ready = state.ready;
            state.ready = false;
            state.finished = false;
            was_finished || was_ready
        }
        Event::FileLoaded => {
            kvlog::info!("mpv media loaded");
            let changed = !state.ready;
            state.ready = true;
            changed
        }
        Event::VideoReconfig => {
            kvlog::info!("mpv video output reconfigured");
            false
        }
        Event::AudioReconfig => {
            #[cfg(feature = "diagnostic-logs")]
            if crate::logger::media_logging_enabled() {
                kvlog::info!("mpv audio output reconfigured", group = "media");
            }
            false
        }
        Event::Seek => {
            #[cfg(feature = "diagnostic-logs")]
            if crate::logger::media_logging_enabled() {
                kvlog::info!("mpv seek started", group = "media");
            }
            false
        }
        Event::PlaybackRestart => {
            #[cfg(feature = "diagnostic-logs")]
            if crate::logger::media_logging_enabled() {
                kvlog::info!("mpv playback resumed after discontinuity", group = "media");
            }
            false
        }
        Event::EndFile(reason) => {
            kvlog::info!(
                "mpv media ended",
                failed = reason == libmpv2::mpv_end_file_reason::Error
            );
            if reason == libmpv2::mpv_end_file_reason::Error {
                bail!("mpv could not decode or play the media");
            }
            state.finished = true;
            true
        }
        Event::Shutdown => {
            kvlog::warn!("mpv core requested shutdown");
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
    let level = match level {
        libmpv2::mpv_log_level::Fatal | libmpv2::mpv_log_level::Error => log::Level::Error,
        libmpv2::mpv_log_level::Warn => log::Level::Warn,
        libmpv2::mpv_log_level::Info => log::Level::Info,
        libmpv2::mpv_log_level::V | libmpv2::mpv_log_level::Debug => log::Level::Debug,
        libmpv2::mpv_log_level::Trace => log::Level::Trace,
        _ => return,
    };
    log::log!(target: "chatt_mpv", level, "mpv[{prefix}] {text}");
}

fn render_worker(
    player_id: u64,
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
    sizing: RenderSizing,
) {
    let backend_name = backend.name();
    kvlog::info!(
        "video render worker started",
        player_id,
        backend = backend_name
    );
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
            let result = render_prepared_frame(
                &mut backend,
                &surface,
                sizing,
                &playback,
                false,
                None,
                &mut diagnostics,
            );
            match result {
                Ok(true) => {
                    diagnostics.rendered += 1;
                    has_frame = true;
                    redraw_pending = false;
                    playback.frame_ready.store(true, Ordering::Release);
                    let _ = gpui_wakeup.try_send(());
                    if live_input_gate.as_ref().is_some_and(|gate| gate.release()) {
                        kvlog::info!(
                            "released live decoder input after first rendered output",
                            backend = backend_name
                        );
                    }
                    if diagnostics.rendered <= INITIAL_RENDER_TRACE_LIMIT {
                        #[cfg(feature = "diagnostic-logs")]
                        if crate::logger::render_logging_enabled() {
                            kvlog::info!(
                                "video render output published",
                                group = "render",
                                ordinal = diagnostics.rendered,
                                operation = "deferred-redraw"
                            );
                        }
                    }
                }
                Ok(false) => {
                    redraw_pending = backend.is_configured();
                }
                Err(error) => {
                    diagnostics.errors += 1;
                    redraw_pending = false;
                    kvlog::error!(
                        "video render worker operation failed",
                        backend = backend_name,
                        operation = "deferred-redraw",
                        err = %error
                    );
                    let _ = errors.send(format!("{error:#}"));
                    let _ = gpui_wakeup.try_send(());
                }
            }
            diagnostics.maybe_log_summary(backend_name);
            continue;
        }
        let message = message.unwrap();
        let operation_started_at = Instant::now();
        let (operation, result) = match message {
            RenderMessage::Update => {
                diagnostics.callbacks += 1;
                pending.store(false, Ordering::Release);
                let updates = backend.context().update();
                if frame_invalidated.take_for_update(updates) {
                    has_frame = false;
                    redraw_pending = false;
                    #[cfg(feature = "diagnostic-logs")]
                    if crate::logger::render_logging_enabled() {
                        kvlog::info!(
                            "invalidated displayed video frame after seek",
                            group = "render"
                        );
                    }
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
                        #[cfg(feature = "diagnostic-logs")]
                        if crate::logger::render_logging_enabled() {
                            kvlog::info!(
                                "video render callback",
                                group = "render",
                                ordinal = diagnostics.callbacks,
                                updates,
                                action = action.as_str(),
                                frame_info_available = frame_info.is_some(),
                                frame_flags = frame_info.map_or(0, |info| info.flags),
                                present = frame_info.is_some_and(|info| info.is_present()),
                                repeat = frame_info.is_some_and(|info| info.is_repeat()),
                                redraw = frame_info.is_some_and(|info| info.is_redraw()),
                                target_time_ns = frame_info.map(|info| info.target_time),
                                pts = frame_pts,
                                has_frame
                            );
                        }
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
                            let result = render_prepared_frame(
                                &mut backend,
                                &surface,
                                sizing,
                                &playback,
                                true,
                                frame_info,
                                &mut diagnostics,
                            );
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
                        render_prepared_frame(
                            &mut backend,
                            &surface,
                            sizing,
                            &playback,
                            false,
                            None,
                            &mut diagnostics,
                        )
                    } else {
                        Ok(false)
                    }
                });
                ("resize", result)
            }
            RenderMessage::Reset { startup } => {
                diagnostics.start_load(startup);
                #[cfg(feature = "diagnostic-logs")]
                if crate::logger::render_logging_enabled() {
                    kvlog::info!(
                        "resetting published video frame",
                        group = "render",
                        player_id = startup.player_id,
                        load_id = startup.load_id,
                        startup_elapsed_ms = startup.elapsed_ms(),
                        backend = backend_name
                    );
                }
                enabled = false;
                has_frame = false;
                redraw_pending = false;
                playback.clear_display_size();
                playback.frame_ready.store(false, Ordering::Release);
                surface.clear();
                ("reset", Ok(false))
            }
            RenderMessage::Shutdown => break,
        };
        match result {
            Ok(rendered) => {
                if rendered {
                    diagnostics.rendered += 1;
                    diagnostics.note_first_render(operation, operation_started_at);
                    has_frame = true;
                    redraw_pending = false;
                    playback.frame_ready.store(true, Ordering::Release);
                    let _ = gpui_wakeup.try_send(());
                    if live_input_gate.as_ref().is_some_and(|gate| gate.release()) {
                        kvlog::info!(
                            "released live decoder input after first rendered output",
                            backend = backend_name
                        );
                    }
                    if diagnostics.rendered <= INITIAL_RENDER_TRACE_LIMIT {
                        #[cfg(feature = "diagnostic-logs")]
                        if crate::logger::render_logging_enabled() {
                            kvlog::info!(
                                "video render output published",
                                group = "render",
                                ordinal = diagnostics.rendered,
                                operation
                            );
                        }
                    }
                }
            }
            Err(error) => {
                diagnostics.errors += 1;
                kvlog::error!(
                    "video render worker operation failed",
                    backend = backend_name,
                    operation,
                    err = %error
                );
                let _ = errors.send(format!("{error:#}"));
                let _ = gpui_wakeup.try_send(());
            }
        }
        diagnostics.maybe_log_summary(backend_name);
    }
    diagnostics.log_final(backend_name);
}

fn render_prepared_frame(
    backend: &mut RenderBackend,
    surface: &WgpuVideoSurface,
    sizing: RenderSizing,
    playback: &SharedPlaybackState,
    acknowledge_if_busy: bool,
    frame_info: Option<libmpv2::render::RenderFrameInfo>,
    diagnostics: &mut RenderDiagnostics,
) -> Result<bool> {
    let selected_size = match sizing {
        RenderSizing::Fixed { width, height } => Some((width, height)),
        RenderSizing::PendingFrame => {
            let frame_info = match frame_info {
                Some(info) => info,
                None => backend.context().next_frame_info().with_context(|| {
                    format!(
                        "read next-frame flags from {} render context",
                        backend.name()
                    )
                })?,
            };
            if !frame_info.is_present() {
                None
            } else {
                let previous_size = backend.configured_size();
                let size = backend.prepare_pending_frame(surface)?.ok_or_else(|| {
                    anyhow!(
                        "attachment {} render backend has a present frame without a valid pending video size (frame_flags={:#x})",
                        backend.name(),
                        frame_info.flags
                    )
                })?;
                if previous_size != Some(size) {
                    diagnostics.resizes += 1;
                }
                Some(size)
            }
        }
    };
    let rendered = backend.render(surface, acknowledge_if_busy, diagnostics)?;
    if rendered && let Some(size) = selected_size {
        playback.set_display_size(size);
    }
    Ok(rendered)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RenderAction {
    None,
    Render,
    Skip,
}

impl RenderAction {
    #[cfg(feature = "diagnostic-logs")]
    fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Render => "render",
            Self::Skip => "skip",
        }
    }
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
    pub ready: bool,
    pub frame_ready: bool,
    pub display_size: Option<(u32, u32)>,
}

#[derive(Default)]
struct SharedPlaybackState {
    position: AtomicU64,
    duration: AtomicU64,
    paused: AtomicBool,
    finished: AtomicBool,
    ready: AtomicBool,
    frame_ready: AtomicBool,
    display_size: AtomicU64,
}

impl SharedPlaybackState {
    fn publish_control(&self, state: PlaybackState) {
        self.position
            .store(state.position.to_bits(), Ordering::Relaxed);
        self.duration
            .store(state.duration.to_bits(), Ordering::Relaxed);
        self.paused.store(state.paused, Ordering::Relaxed);
        self.ready.store(state.ready, Ordering::Relaxed);
        self.finished.store(state.finished, Ordering::Release);
    }

    fn set_display_size(&self, size: (u32, u32)) -> bool {
        let packed_size = (u64::from(size.0) << 32) | u64::from(size.1);
        self.display_size.swap(packed_size, Ordering::Release) != packed_size
    }

    fn clear_display_size(&self) {
        self.display_size.store(0, Ordering::Release);
    }

    fn snapshot(&self) -> PlaybackState {
        let finished = self.finished.load(Ordering::Acquire);
        let frame_ready = self.frame_ready.load(Ordering::Acquire);
        let packed_size = self.display_size.load(Ordering::Acquire);
        PlaybackState {
            position: f64::from_bits(self.position.load(Ordering::Relaxed)),
            duration: f64::from_bits(self.duration.load(Ordering::Relaxed)),
            paused: self.paused.load(Ordering::Relaxed),
            finished,
            ready: self.ready.load(Ordering::Relaxed),
            frame_ready,
            display_size: (packed_size != 0)
                .then_some(((packed_size >> 32) as u32, packed_size as u32)),
        }
    }

    fn seek_to(&self, position: f64) {
        self.position.store(position.to_bits(), Ordering::Relaxed);
        self.finished.store(false, Ordering::Release);
    }

    fn reset(&self) {
        self.publish_control(PlaybackState::default());
        self.clear_display_size();
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
    fn video_adjustments_use_native_mpv_property_operations() {
        assert_eq!(
            VideoAdjustment::Contrast(-1.0).mpv_command(),
            ("add", "contrast", -1.0)
        );
        assert_eq!(
            VideoAdjustment::Brightness(1.0).mpv_command(),
            ("add", "brightness", 1.0)
        );
        assert_eq!(
            VideoAdjustment::Gamma(-1.0).mpv_command(),
            ("add", "gamma", -1.0)
        );
        assert_eq!(
            VideoAdjustment::Saturation(1.0).mpv_command(),
            ("add", "saturation", 1.0)
        );
        assert_eq!(
            VideoAdjustment::PlaybackSpeed(1.0 / 1.1).mpv_command(),
            ("multiply", "speed", 1.0 / 1.1)
        );
        assert_eq!(
            VideoAdjustment::PlaybackSpeed(1.1).mpv_command(),
            ("multiply", "speed", 1.1)
        );
        assert_eq!(
            VideoAdjustment::Gamma(-1.0).effect_delta(),
            Some((VideoEffect::Gamma, -1.0))
        );
        assert_eq!(VideoAdjustment::PlaybackSpeed(1.1).effect_delta(), None);
        assert_eq!(VideoEffect::Saturation.property(), "saturation");
    }

    #[test]
    fn render_owned_display_size_survives_control_publication_and_resets() {
        let shared = SharedPlaybackState::default();
        assert!(shared.set_display_size((1_080, 1_920)));
        assert!(!shared.set_display_size((1_080, 1_920)));
        shared.publish_control(PlaybackState {
            ready: true,
            ..PlaybackState::default()
        });

        assert_eq!(shared.snapshot().display_size, Some((1_080, 1_920)));
        shared.reset();
        assert_eq!(shared.snapshot().display_size, None);
    }

    #[test]
    fn startup_render_diagnostics_scope_pending_frame_to_current_load() {
        let mut diagnostics = RenderDiagnostics::new();
        let first = StartupLogContext::for_test(7, 11);
        diagnostics.start_load(first);
        assert_eq!(diagnostics.startup.unwrap().load_id, 11);

        let second = StartupLogContext::for_test(7, 12);
        diagnostics.start_load(second);
        diagnostics.note_first_render("resize", Instant::now());
        assert!(diagnostics.startup.is_none());
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

    fn write_one_frame_fixture(path: &std::path::Path, sample_aspect_ratio: &str) {
        let output = std::process::Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=black:s=320x180:d=0.04",
                "-vf",
                &format!("setsar={sample_aspect_ratio}"),
                "-frames:v",
                "1",
                "-c:v",
                "mjpeg",
                "-y",
            ])
            .arg(path)
            .output()
            .expect("ffmpeg is available with the required libmpv dependency");
        assert!(
            output.status.success(),
            "ffmpeg could not create the attachment fixture: {}",
            String::from_utf8_lossy(&output.stderr),
        );
    }

    fn write_seek_fixture(path: &std::path::Path) {
        let output = std::process::Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=black:s=320x180:d=2:r=10",
                "-c:v",
                "mjpeg",
                "-y",
            ])
            .arg(path)
            .output()
            .expect("ffmpeg is available with the required libmpv dependency");
        assert!(
            output.status.success(),
            "ffmpeg could not create the seek fixture: {}",
            String::from_utf8_lossy(&output.stderr),
        );
    }

    fn property_display_size(path: &std::path::Path) -> (u32, u32) {
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
        while Instant::now() < deadline {
            match mpv.wait_event(0.1) {
                Some(Ok(Event::FileLoaded | Event::VideoReconfig)) => {
                    if let Ok(size) = video_display_size(&mpv) {
                        return size;
                    }
                }
                Some(Err(error)) => panic!("libmpv event failed: {error}"),
                _ => {}
            }
        }
        panic!("libmpv did not publish display-size properties");
    }

    fn pending_render_size(path: &std::path::Path) -> ((u32, u32), f64) {
        let mpv = Arc::new(
            Mpv::with_initializer(|initializer| {
                initializer.set_option("vo", "libmpv")?;
                initializer.set_option("audio", "no")?;
                initializer.set_option("pause", "yes")?;
                initializer.set_option("hwdec", "no")?;
                Ok(())
            })
            .unwrap(),
        );
        let mut context = mpv.create_software_render_context(false).unwrap();
        let (sender, updates) = mpsc::channel();
        context.set_update_callback(move || {
            let _ = sender.send(());
        });
        mpv.command("loadfile", &[&path.to_string_lossy(), "replace"])
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            let _ = updates.recv_timeout(Duration::from_millis(100));
            let update = context.update();
            if update & u64::from(mpv_render_update::Frame) == 0 {
                continue;
            }
            let info = context.next_frame_info().unwrap();
            if !info.is_present() {
                continue;
            }
            let size = context
                .next_frame_video_size()
                .unwrap()
                .expect("a pending video image has a display size");
            let pts = context.next_frame_video_pts().unwrap();
            let stride = size.0 as usize * 4;
            let mut pixels = vec![0; stride * size.1 as usize];
            context
                .render_software(SoftwareRenderTarget {
                    width: size.0,
                    height: size.1,
                    format: FORMAT_RGBA,
                    stride,
                    pixels: &mut pixels,
                })
                .unwrap();
            context.report_swap();
            mpv.command("stop", &[]).unwrap();
            let stop_deadline = Instant::now() + Duration::from_secs(1);
            loop {
                let _ = updates.recv_timeout(Duration::from_millis(20));
                context.update();
                if context.next_frame_video_size().unwrap().is_none() {
                    break;
                }
                assert!(
                    Instant::now() < stop_deadline,
                    "a no-frame update exposed stale dimensions after stop"
                );
            }
            return (size, pts);
        }
        panic!("libmpv did not expose a pending frame");
    }

    #[test]
    fn pending_frame_size_matches_display_properties_before_first_render() {
        let directory = tempfile::tempdir().unwrap();
        for (name, sample_aspect_ratio) in [("square-pixels.mkv", "1/1"), ("anamorphic.mkv", "2/1")]
        {
            let path = directory.path().join(name);
            write_one_frame_fixture(&path, sample_aspect_ratio);
            let expected = property_display_size(&path);
            let (pending, first_pts) = pending_render_size(&path);
            assert_eq!(pending, expected);
            assert!(first_pts.abs() < f64::EPSILON);
        }
    }

    #[test]
    fn headless_attachment_player_decodes_common_audio_formats_without_video_frames() {
        use local_rpc::{
            ids::{FileTransferId, RoomId},
            model::AttachmentId,
        };

        let directory = tempfile::tempdir().unwrap();
        let formats = [
            ("sample.wav", "pcm_s16le"),
            ("sample.mp3", "libmp3lame"),
            ("sample.opus", "libopus"),
            ("sample.ogg", "libvorbis"),
            ("sample.m4a", "aac"),
            ("sample.flac", "flac"),
        ];
        let registry = crate::attachment_source::AttachmentSourceRegistry::new(1);
        let (wakeup, _) = async_channel::bounded(1);
        let mut player = MpvAudioPlayer::new_attachment_with_audio_output(
            wakeup,
            registry.clone(),
            Some("null"),
        )
        .unwrap();

        for (index, (file_name, codec)) in formats.into_iter().enumerate() {
            let path = directory.path().join(file_name);
            let output = std::process::Command::new("ffmpeg")
                .args([
                    "-v",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    "sine=frequency=440:duration=0.6",
                    "-vn",
                    "-c:a",
                    codec,
                    "-y",
                ])
                .arg(&path)
                .output()
                .expect("ffmpeg is available with the required libmpv dependency");
            assert!(
                output.status.success(),
                "ffmpeg could not create {file_name}: {}",
                String::from_utf8_lossy(&output.stderr),
            );
            let byte_len = std::fs::metadata(&path).unwrap().len();
            let source = crate::attachment_source::AttachmentSource::direct(
                crate::attachment_source::AttachmentSourceKey {
                    namespace: 1,
                    room_id: RoomId(1),
                    attachment_id: AttachmentId {
                        timestamp_ms: index as u64 + 1,
                        transfer_id: FileTransferId(index as u64 + 1),
                    },
                },
                std::fs::File::open(&path).unwrap(),
                byte_len,
            );
            let source = registry.register(source);
            player.load_at(source.url(), true, 100.0, 1.0, 0.0).unwrap();

            let deadline = Instant::now() + Duration::from_secs(3);
            let playback = loop {
                assert!(Instant::now() < deadline, "timed out decoding {file_name}");
                let playback = player.drain_events().unwrap();
                if playback.ready && playback.duration > 0.0 {
                    break playback;
                }
                std::thread::sleep(Duration::from_millis(10));
            };
            assert!(!playback.frame_ready);
            if index == 0 {
                let duration = playback.duration;
                player.seek_absolute(duration).unwrap();
                let deadline = Instant::now() + Duration::from_secs(3);
                loop {
                    assert!(
                        Instant::now() < deadline,
                        "timed out seeking {file_name} to its end"
                    );
                    let playback = player.drain_events().unwrap();
                    if playback.position >= duration - 0.1 {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }

                let target = playback.duration * 0.5;
                player.seek_absolute(target).unwrap();
                player.set_speed(1.5).unwrap();
                let deadline = Instant::now() + Duration::from_secs(3);
                loop {
                    assert!(Instant::now() < deadline, "timed out seeking {file_name}");
                    let playback = player.drain_events().unwrap();
                    if (playback.position - target).abs() <= 0.1 {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
            player.stop().unwrap();
        }
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
                "mjpeg",
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
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("seek-fixture.mkv");
        write_seek_fixture(&path);
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
    fn vulkan_decode_accepts_an_enabled_dedicated_decode_queue() {
        let h264 = [
            c"VK_KHR_video_queue",
            c"VK_KHR_video_decode_queue",
            c"VK_KHR_video_decode_h264",
            c"VK_KHR_video_maintenance1",
        ];
        let queue_families = [
            vk::QueueFamilyProperties {
                queue_flags: vk::QueueFlags::GRAPHICS,
                queue_count: 1,
                ..Default::default()
            },
            vk::QueueFamilyProperties {
                queue_flags: vk::QueueFlags::VIDEO_DECODE_KHR,
                queue_count: 1,
                ..Default::default()
            },
        ];
        assert!(supports_vulkan_video_decode(
            &h264,
            &[(0, 1), (1, 1)],
            &queue_families,
            true,
            true,
            Some("avc1.64001F")
        ));
        assert!(supports_vulkan_video_decode(
            &h264,
            &[(0, 1), (1, 1)],
            &queue_families,
            true,
            true,
            None
        ));
        assert!(!supports_vulkan_video_decode(
            &h264,
            &[(0, 1), (1, 1)],
            &queue_families,
            true,
            true,
            Some("hvc1.1.6.L93")
        ));
        assert!(!supports_vulkan_video_decode(
            &[c"VK_KHR_video_decode_h264"],
            &[(0, 1), (1, 1)],
            &queue_families,
            true,
            true,
            Some("h264")
        ));
        assert!(!supports_vulkan_video_decode(
            &h264,
            &[(0, 1)],
            &queue_families,
            true,
            true,
            Some("h264")
        ));
        assert!(!supports_vulkan_video_decode(
            &h264,
            &[(0, 1), (1, 1)],
            &queue_families,
            false,
            true,
            Some("h264")
        ));
    }

    #[test]
    fn attachment_decoder_policy_skips_only_impossible_vulkan_video() {
        assert_eq!(
            attachment_hwdec_policy(true),
            "vulkan,auto-safe",
            "libmpv retains authoritative stream probing when the imported device is usable"
        );
        assert_eq!(
            attachment_hwdec_policy(false),
            if cfg!(target_os = "linux") {
                "vaapi-copy,auto-copy-safe"
            } else {
                "auto-copy-safe"
            }
        );
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
    fn fixed_size_live_generation_renders_after_initial_resize() {
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
