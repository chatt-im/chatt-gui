#[cfg(not(target_family = "wasm"))]
use anyhow::Context as _;
#[cfg(not(target_family = "wasm"))]
use gpui_util::ResultExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use wgpu::TextureFormat;
#[cfg(target_os = "linux")]
use wgpu::hal::vulkan::Api as VulkanApi;

pub struct WgpuContext {
    pub instance: wgpu::Instance,
    pub adapter: wgpu::Adapter,
    pub device: Arc<wgpu::Device>,
    pub queue: Arc<wgpu::Queue>,
    dual_source_blending: bool,
    color_texture_format: wgpu::TextureFormat,
    device_lost: Arc<AtomicBool>,
}

#[derive(Clone, Copy)]
pub struct CompositorGpuHint {
    pub vendor_id: u32,
    pub device_id: u32,
}

#[cfg(target_os = "linux")]
fn create_vulkan_video_device(
    adapter: &wgpu::Adapter,
    descriptor: &wgpu::DeviceDescriptor<'_>,
) -> Option<anyhow::Result<(wgpu::Device, wgpu::Queue, crate::VulkanVideoContext)>> {
    let hal_adapter = unsafe { adapter.as_hal::<VulkanApi>() }?;
    let core_extensions = [
        ash::khr::video_queue::NAME,
        ash::khr::video_decode_queue::NAME,
        ash::khr::video_maintenance1::NAME,
    ];
    if !core_extensions.iter().all(|extension| {
        hal_adapter
            .physical_device_capabilities()
            .supports_extension(extension)
    }) {
        return None;
    }
    let codec_extensions = [
        ("H.264", ash::khr::video_decode_h264::NAME),
        ("H.265", ash::khr::video_decode_h265::NAME),
        ("AV1", ash::khr::video_decode_av1::NAME),
    ];
    let enabled_codecs = codec_extensions
        .into_iter()
        .filter(|(_, extension)| {
            hal_adapter
                .physical_device_capabilities()
                .supports_extension(extension)
        })
        .collect::<Vec<_>>();
    if enabled_codecs.is_empty() {
        return None;
    }
    let video_extensions = core_extensions
        .into_iter()
        .chain(enabled_codecs.iter().map(|(_, extension)| *extension))
        .collect::<Vec<_>>();

    let instance = hal_adapter.shared_instance().raw_instance();
    let physical_device = hal_adapter.raw_physical_device();
    let properties = unsafe { instance.get_physical_device_properties(physical_device) };
    let synchronization2_extension_required =
        properties.api_version < ash::vk::API_VERSION_1_3;
    if synchronization2_extension_required
        && !hal_adapter
            .physical_device_capabilities()
            .supports_extension(ash::khr::synchronization2::NAME)
    {
        return None;
    }
    let mut synchronization2 =
        ash::vk::PhysicalDeviceSynchronization2Features::default();
    let mut video_maintenance1 =
        ash::vk::PhysicalDeviceVideoMaintenance1FeaturesKHR::default();
    let mut features = ash::vk::PhysicalDeviceFeatures2::default()
        .push_next(&mut synchronization2)
        .push_next(&mut video_maintenance1);
    unsafe { instance.get_physical_device_features2(physical_device, &mut features) };
    if synchronization2.synchronization2 != ash::vk::TRUE
        || video_maintenance1.video_maintenance1 != ash::vk::TRUE
    {
        return None;
    }

    let queue_families = unsafe {
        hal_adapter
            .shared_instance()
            .raw_instance()
            .get_physical_device_queue_family_properties(hal_adapter.raw_physical_device())
    };
    let video_queue_family = queue_families.iter().position(|properties| {
        properties
            .queue_flags
            .contains(ash::vk::QueueFlags::VIDEO_DECODE_KHR)
    })? as u32;
    let video_queue_priorities = [1.0_f32];
    let mut synchronization2 =
        ash::vk::PhysicalDeviceSynchronization2Features::default().synchronization2(true);
    let mut video_maintenance1 =
        ash::vk::PhysicalDeviceVideoMaintenance1FeaturesKHR::default().video_maintenance1(true);
    let callback: Box<wgpu::hal::vulkan::CreateDeviceCallback<'_>> = Box::new(|args| {
        for extension in &video_extensions {
            if !args.extensions.contains(&extension) {
                args.extensions.push(*extension);
            }
        }
        if synchronization2_extension_required
            && !args
                .extensions
                .contains(&ash::khr::synchronization2::NAME)
        {
            args.extensions.push(ash::khr::synchronization2::NAME);
        }
        *args.create_info = (*args.create_info)
            .push_next(&mut synchronization2)
            .push_next(&mut video_maintenance1);
        if !args
            .queue_create_infos
            .iter()
            .any(|queue| queue.queue_family_index == video_queue_family)
        {
            args.queue_create_infos.push(
                ash::vk::DeviceQueueCreateInfo::default()
                    .queue_family_index(video_queue_family)
                    .queue_priorities(&video_queue_priorities),
            );
        }
    });
    let open_device = match unsafe {
        hal_adapter.open_with_callback(
            descriptor.required_features,
            &descriptor.required_limits,
            &descriptor.memory_hints,
            Some(callback),
        )
    } {
        Ok(device) => device,
        Err(error) => {
            return Some(Err(anyhow::anyhow!(
                "create Vulkan device with a decode queue: {error}"
            )));
        }
    };
    let device = unsafe { adapter.create_device_from_hal(open_device, descriptor) }
        .map_err(|error| anyhow::anyhow!("import Vulkan Video device into wgpu: {error}"));
    Some(device.map(|(device, queue)| {
        let enabled_codecs = enabled_codecs
            .iter()
            .map(|(codec, _)| *codec)
            .collect::<Vec<_>>()
            .join(", ");
        log::info!(
            "Enabled Vulkan Video ({enabled_codecs}) on GPUI device queue_family={video_queue_family}"
        );
        (
            device,
            queue,
            crate::VulkanVideoContext {
                additional_queue_families: vec![(video_queue_family, 1)],
                synchronization2: true,
                video_maintenance1: true,
            },
        )
    }))
}

impl WgpuContext {
    #[cfg(not(target_family = "wasm"))]
    pub fn new(
        instance: wgpu::Instance,
        surface: &wgpu::Surface<'_>,
        compositor_gpu: Option<CompositorGpuHint>,
    ) -> anyhow::Result<Self> {
        Self::new_with_options(instance, surface, compositor_gpu, false)
    }

    #[cfg(not(target_family = "wasm"))]
    pub fn new_rejecting_software(
        instance: wgpu::Instance,
        surface: &wgpu::Surface<'_>,
        compositor_gpu: Option<CompositorGpuHint>,
    ) -> anyhow::Result<Self> {
        Self::new_with_options(instance, surface, compositor_gpu, true)
    }

    #[cfg(not(target_family = "wasm"))]
    fn new_with_options(
        instance: wgpu::Instance,
        surface: &wgpu::Surface<'_>,
        compositor_gpu: Option<CompositorGpuHint>,
        reject_software: bool,
    ) -> anyhow::Result<Self> {
        let device_id_filter = match std::env::var("ZED_DEVICE_ID") {
            Ok(val) => parse_pci_id(&val)
                .context("Failed to parse device ID from `ZED_DEVICE_ID` environment variable")
                .log_err(),
            Err(std::env::VarError::NotPresent) => None,
            err => {
                err.context("Failed to read value of `ZED_DEVICE_ID` environment variable")
                    .log_err();
                None
            }
        };

        // Select an adapter by actually testing surface configuration with the real device.
        // This is the only reliable way to determine compatibility on hybrid GPU systems.
        let (
            adapter,
            device,
            queue,
            dual_source_blending,
            color_texture_format,
            video,
        ) =
            gpui::block_on(Self::select_adapter_and_device(
                &instance,
                device_id_filter,
                surface,
                compositor_gpu.as_ref(),
                reject_software,
            ))?;

        let device_lost = Arc::new(AtomicBool::new(false));
        device.set_device_lost_callback({
            let device_lost = Arc::clone(&device_lost);
            move |reason, message| {
                log::error!("wgpu device lost: reason={reason:?}, message={message}");
                if reason != wgpu::DeviceLostReason::Destroyed {
                    device_lost.store(true, Ordering::Relaxed);
                }
            }
        });

        log::info!(
            "Selected GPU adapter: {:?} ({:?})",
            adapter.get_info().name,
            adapter.get_info().backend
        );

        let context = Self {
            instance,
            adapter,
            device: Arc::new(device),
            queue: Arc::new(queue),
            dual_source_blending,
            color_texture_format,
            device_lost,
        };
        crate::register_video_context(
            &context.device,
            &context.queue,
            video,
        );
        Ok(context)
    }

    #[cfg(target_family = "wasm")]
    pub async fn new_web() -> anyhow::Result<Self> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::BROWSER_WEBGPU | wgpu::Backends::GL,
            flags: wgpu::InstanceFlags::default(),
            backend_options: wgpu::BackendOptions::default(),
            memory_budget_thresholds: wgpu::MemoryBudgetThresholds::default(),
            display: None,
        });

        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: None,
                force_fallback_adapter: false,
            })
            .await
            .map_err(|e| anyhow::anyhow!("Failed to request GPU adapter: {e}"))?;

        log::info!(
            "Selected GPU adapter: {:?} ({:?})",
            adapter.get_info().name,
            adapter.get_info().backend
        );

        let device_lost = Arc::new(AtomicBool::new(false));
        let (device, queue, dual_source_blending, color_texture_format, _) =
            Self::create_device(&adapter).await?;

        Ok(Self {
            instance,
            adapter,
            device: Arc::new(device),
            queue: Arc::new(queue),
            dual_source_blending,
            color_texture_format,
            device_lost,
        })
    }

    async fn create_device(
        adapter: &wgpu::Adapter,
    ) -> anyhow::Result<(
        wgpu::Device,
        wgpu::Queue,
        bool,
        TextureFormat,
        crate::VulkanVideoContext,
    )> {
        let dual_source_blending = adapter
            .features()
            .contains(wgpu::Features::DUAL_SOURCE_BLENDING);

        let mut required_features = wgpu::Features::empty();
        if dual_source_blending {
            required_features |= wgpu::Features::DUAL_SOURCE_BLENDING;
        } else {
            log::warn!(
                "Dual-source blending not available on this GPU. \
                Subpixel text antialiasing will be disabled."
            );
        }

        let color_atlas_texture_format = Self::select_color_texture_format(adapter)?;

        let descriptor = wgpu::DeviceDescriptor {
            label: Some("gpui_device"),
            required_features,
            required_limits: wgpu::Limits::downlevel_defaults()
                .using_resolution(adapter.limits())
                .using_alignment(adapter.limits()),
            memory_hints: wgpu::MemoryHints::MemoryUsage,
            trace: wgpu::Trace::Off,
            experimental_features: wgpu::ExperimentalFeatures::disabled(),
        };

        #[cfg(target_os = "linux")]
        let video_device = match create_vulkan_video_device(adapter, &descriptor) {
            Some(Ok(device)) => Some(device),
            Some(Err(error)) => {
                log::warn!(
                    "Could not enable Vulkan Video on the shared GPUI device; using the standard device configuration: {error:#}"
                );
                None
            }
            None => None,
        };
        #[cfg(not(target_os = "linux"))]
        let video_device: Option<(wgpu::Device, wgpu::Queue, crate::VulkanVideoContext)> = None;

        let (device, queue, video) = match video_device {
            Some((device, queue, video)) => (device, queue, video),
            None => {
                let (device, queue) = adapter
                    .request_device(&descriptor)
                    .await
                    .map_err(|e| anyhow::anyhow!("Failed to create wgpu device: {e}"))?;
                (device, queue, crate::VulkanVideoContext::default())
            }
        };

        Ok((
            device,
            queue,
            dual_source_blending,
            color_atlas_texture_format,
            video,
        ))
    }

    #[cfg(not(target_family = "wasm"))]
    pub fn instance(display: Box<dyn wgpu::wgt::WgpuHasDisplayHandle>) -> wgpu::Instance {
        wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::VULKAN,
            flags: wgpu::InstanceFlags::default(),
            backend_options: wgpu::BackendOptions::default(),
            memory_budget_thresholds: wgpu::MemoryBudgetThresholds::default(),
            display: Some(display),
        })
    }

    pub fn check_compatible_with_surface(&self, surface: &wgpu::Surface<'_>) -> anyhow::Result<()> {
        let caps = surface.get_capabilities(&self.adapter);
        if caps.formats.is_empty() {
            let info = self.adapter.get_info();
            anyhow::bail!(
                "Adapter {:?} (backend={:?}, device={:#06x}) is not compatible with the \
                 display surface for this window.",
                info.name,
                info.backend,
                info.device,
            );
        }
        Ok(())
    }

    /// Select an adapter and create a device, testing that the surface can actually be configured.
    /// This is the only reliable way to determine compatibility on hybrid GPU systems, where
    /// adapters may report surface compatibility via get_capabilities() but fail when actually
    /// configuring (e.g., NVIDIA reporting Vulkan Wayland support but failing because the
    /// Wayland compositor runs on the Intel GPU).
    #[cfg(not(target_family = "wasm"))]
    async fn select_adapter_and_device(
        instance: &wgpu::Instance,
        device_id_filter: Option<u32>,
        surface: &wgpu::Surface<'_>,
        compositor_gpu: Option<&CompositorGpuHint>,
        reject_software: bool,
    ) -> anyhow::Result<(
        wgpu::Adapter,
        wgpu::Device,
        wgpu::Queue,
        bool,
        TextureFormat,
        crate::VulkanVideoContext,
    )> {
        let mut adapters: Vec<_> = instance.enumerate_adapters(wgpu::Backends::all()).await;

        if adapters.is_empty() {
            anyhow::bail!("No GPU adapters found");
        }

        if let Some(device_id) = device_id_filter {
            log::info!("ZED_DEVICE_ID filter: {:#06x}", device_id);
        }

        // Sort adapters into a single priority order. Tiers (from highest to lowest):
        //
        // 1. ZED_DEVICE_ID match — explicit user override
        // 2. Compositor GPU match — the GPU the display server is rendering on
        // 3. Device type (Discrete > Integrated > Other > Virtual > Cpu).
        //    "Other" ranks above "Virtual" because OpenGL seems to count as "Other".
        // 4. Backend — prefer Vulkan/Metal/Dx12 over GL/etc.
        adapters.sort_by_key(|adapter| {
            let info = adapter.get_info();

            // Backends like OpenGL report device=0 for all adapters, so
            // device-based matching is only meaningful when non-zero.
            let device_known = info.device != 0;

            let user_override: u8 = match device_id_filter {
                Some(id) if device_known && info.device == id => 0,
                _ => 1,
            };

            let compositor_match: u8 = match compositor_gpu {
                Some(hint)
                    if device_known
                        && info.vendor == hint.vendor_id
                        && info.device == hint.device_id =>
                {
                    0
                }
                _ => 1,
            };

            let type_priority: u8 = if info.device_type == wgpu::DeviceType::Cpu {
                4
            } else {
                match info.device_type {
                    wgpu::DeviceType::DiscreteGpu => 0,
                    wgpu::DeviceType::IntegratedGpu => 1,
                    wgpu::DeviceType::Other => 2,
                    wgpu::DeviceType::VirtualGpu => 3,
                    wgpu::DeviceType::Cpu => 4,
                }
            };

            let backend_priority: u8 = match info.backend {
                wgpu::Backend::Vulkan | wgpu::Backend::Metal | wgpu::Backend::Dx12 => 0,
                _ => 1,
            };

            (
                user_override,
                compositor_match,
                type_priority,
                backend_priority,
            )
        });

        // Log all available adapters (in sorted order)
        log::info!("Found {} GPU adapter(s):", adapters.len());
        for adapter in &adapters {
            let info = adapter.get_info();
            log::info!(
                "  - {} (vendor={:#06x}, device={:#06x}, backend={:?}, type={:?})",
                info.name,
                info.vendor,
                info.device,
                info.backend,
                info.device_type,
            );
        }

        // Test each adapter by creating a device and configuring the surface
        for adapter in adapters {
            let info = adapter.get_info();

            if reject_software && info.device_type == wgpu::DeviceType::Cpu {
                log::info!(
                    "Skipping software renderer: {} ({:?})",
                    info.name,
                    info.backend
                );
                continue;
            }

            log::info!("Testing adapter: {} ({:?})...", info.name, info.backend);

            match Self::try_adapter_with_surface(&adapter, surface).await {
                Ok((
                    device,
                    queue,
                    dual_source_blending,
                    color_atlas_texture_format,
                    video,
                )) => {
                    log::info!(
                        "Selected GPU (passed configuration test): {} ({:?})",
                        info.name,
                        info.backend
                    );
                    return Ok((
                        adapter,
                        device,
                        queue,
                        dual_source_blending,
                        color_atlas_texture_format,
                        video,
                    ));
                }
                Err(e) => {
                    log::info!(
                        "  Adapter {} ({:?}) failed: {}, trying next...",
                        info.name,
                        info.backend,
                        e
                    );
                }
            }
        }

        anyhow::bail!("No GPU adapter found that can configure the display surface")
    }

    /// Try to use an adapter with a surface by creating a device and testing configuration.
    /// Returns the device and queue if successful, allowing them to be reused.
    #[cfg(not(target_family = "wasm"))]
    async fn try_adapter_with_surface(
        adapter: &wgpu::Adapter,
        surface: &wgpu::Surface<'_>,
    ) -> anyhow::Result<(
        wgpu::Device,
        wgpu::Queue,
        bool,
        TextureFormat,
        crate::VulkanVideoContext,
    )> {
        let caps = surface.get_capabilities(adapter);
        if caps.formats.is_empty() {
            anyhow::bail!("no compatible surface formats");
        }
        if caps.alpha_modes.is_empty() {
            anyhow::bail!("no compatible alpha modes");
        }

        let (
            device,
            queue,
            dual_source_blending,
            color_atlas_texture_format,
            video,
        ) =
            Self::create_device(adapter).await?;
        let error_scope = device.push_error_scope(wgpu::ErrorFilter::Validation);

        let test_config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: caps.formats[0],
            width: 64,
            height: 64,
            present_mode: wgpu::PresentMode::Fifo,
            desired_maximum_frame_latency: 2,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
        };

        surface.configure(&device, &test_config);

        let error = error_scope.pop().await;
        if let Some(e) = error {
            anyhow::bail!("surface configuration failed: {e}");
        }

        Ok((
            device,
            queue,
            dual_source_blending,
            color_atlas_texture_format,
            video,
        ))
    }

    fn select_color_texture_format(adapter: &wgpu::Adapter) -> anyhow::Result<wgpu::TextureFormat> {
        let required_usages = wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST;
        let bgra_features = adapter.get_texture_format_features(wgpu::TextureFormat::Bgra8Unorm);
        if bgra_features.allowed_usages.contains(required_usages) {
            return Ok(wgpu::TextureFormat::Bgra8Unorm);
        }

        let rgba_features = adapter.get_texture_format_features(wgpu::TextureFormat::Rgba8Unorm);
        if rgba_features.allowed_usages.contains(required_usages) {
            let info = adapter.get_info();
            log::warn!(
                "Adapter {} ({:?}) does not support Bgra8Unorm atlas textures with usages {:?}; \
                 falling back to Rgba8Unorm atlas textures.",
                info.name,
                info.backend,
                required_usages,
            );
            return Ok(wgpu::TextureFormat::Rgba8Unorm);
        }

        let info = adapter.get_info();
        Err(anyhow::anyhow!(
            "Adapter {} ({:?}, device={:#06x}) does not support a usable color atlas texture \
             format with usages {:?}. Bgra8Unorm allowed usages: {:?}; \
             Rgba8Unorm allowed usages: {:?}.",
            info.name,
            info.backend,
            info.device,
            required_usages,
            bgra_features.allowed_usages,
            rgba_features.allowed_usages,
        ))
    }
    pub fn supports_dual_source_blending(&self) -> bool {
        self.dual_source_blending
    }

    pub fn color_texture_format(&self) -> wgpu::TextureFormat {
        self.color_texture_format
    }

    /// Returns true if the GPU device was lost (e.g., due to driver crash, suspend/resume).
    /// When this returns true, the context should be recreated.
    pub fn device_lost(&self) -> bool {
        self.device_lost.load(Ordering::Relaxed)
    }

    /// Returns a clone of the device_lost flag for sharing with renderers.
    pub(crate) fn device_lost_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.device_lost)
    }
}

#[cfg(not(target_family = "wasm"))]
fn parse_pci_id(id: &str) -> anyhow::Result<u32> {
    let mut id = id.trim();

    if id.starts_with("0x") || id.starts_with("0X") {
        id = &id[2..];
    }
    let is_hex_string = id.chars().all(|c| c.is_ascii_hexdigit());
    let is_4_chars = id.len() == 4;
    anyhow::ensure!(
        is_4_chars && is_hex_string,
        "Expected a 4 digit PCI ID in hexadecimal format"
    );

    u32::from_str_radix(id, 16).context("parsing PCI ID as hex")
}

#[cfg(test)]
mod tests {
    use super::parse_pci_id;

    #[test]
    fn test_parse_device_id() {
        assert!(parse_pci_id("0xABCD").is_ok());
        assert!(parse_pci_id("ABCD").is_ok());
        assert!(parse_pci_id("abcd").is_ok());
        assert!(parse_pci_id("1234").is_ok());
        assert!(parse_pci_id("123").is_err());
        assert_eq!(
            parse_pci_id(&format!("{:x}", 0x1234)).unwrap(),
            parse_pci_id(&format!("{:X}", 0x1234)).unwrap(),
        );

        assert_eq!(
            parse_pci_id(&format!("{:#x}", 0x1234)).unwrap(),
            parse_pci_id(&format!("{:#X}", 0x1234)).unwrap(),
        );
    }
}
