use anyhow::{Context as _, Result, anyhow};
use ash::vk;
use gpui::{DevicePixels, PlatformSurface, PlatformSurfaceSource, Size};
use parking_lot::Mutex;
use std::{
    any::Any,
    ffi::CStr,
    path::PathBuf,
    sync::{
        Arc, OnceLock, Weak,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
};
use wgpu::hal::vulkan::{Api as VulkanApi, QueueHostLock};

const VIDEO_TEXTURE_COUNT: usize = 3;
const VIDEO_SURFACE_TRACE_LIMIT: u64 = 16;

struct RegisteredContext {
    device: Weak<wgpu::Device>,
    queue: Weak<wgpu::Queue>,
}

static VIDEO_CONTEXT: OnceLock<Mutex<Option<RegisteredContext>>> = OnceLock::new();

pub(crate) fn register_video_context(device: &Arc<wgpu::Device>, queue: &Arc<wgpu::Queue>) {
    *VIDEO_CONTEXT.get_or_init(Default::default).lock() = Some(RegisteredContext {
        device: Arc::downgrade(device),
        queue: Arc::downgrade(queue),
    });
}

fn active_context() -> Result<(Arc<wgpu::Device>, Arc<wgpu::Queue>)> {
    let context = VIDEO_CONTEXT
        .get_or_init(Default::default)
        .lock();
    let context = context
        .as_ref()
        .ok_or_else(|| anyhow!("GPUI has not initialized its wgpu device"))?;
    Ok((
        context
            .device
            .upgrade()
            .ok_or_else(|| anyhow!("GPUI's wgpu device was destroyed"))?,
        context
            .queue
            .upgrade()
            .ok_or_else(|| anyhow!("GPUI's wgpu queue was destroyed"))?,
    ))
}

/// Native Vulkan objects belonging to GPUI's wgpu device.
#[derive(Clone)]
pub struct VulkanVideoDevice {
    pub instance: ash::Instance,
    pub physical_device: vk::PhysicalDevice,
    pub device: ash::Device,
    pub get_proc_address: vk::PFN_vkGetInstanceProcAddr,
    pub queue_family: u32,
    pub queue_index: u32,
    /// Capabilities of the queue family wgpu actually created and exported.
    pub queue_flags: vk::QueueFlags,
    pub instance_extensions: Vec<&'static CStr>,
    pub device_extensions: Vec<&'static CStr>,
    /// Queue families created on the logical device and safe for imported
    /// libmpv work. Each tuple is `(family_index, queue_count)`.
    pub enabled_queue_families: Vec<(u32, u32)>,
    pub queue_lock: Arc<QueueHostLock>,
    /// DRM render node belonging to `physical_device`, when Vulkan exposes an
    /// exact device identity. This is never guessed from enumeration order.
    pub drm_render_node: Option<PathBuf>,
}

#[cfg(target_os = "linux")]
fn drm_render_node(
    instance: &ash::Instance,
    physical_device: vk::PhysicalDevice,
    instance_extensions: &[&'static CStr],
) -> Option<PathBuf> {
    if !instance_extensions.contains(&ash::ext::physical_device_drm::NAME) {
        return None;
    }

    let mut drm = vk::PhysicalDeviceDrmPropertiesEXT::default();
    let mut properties = vk::PhysicalDeviceProperties2::default().push_next(&mut drm);
    unsafe { instance.get_physical_device_properties2(physical_device, &mut properties) };
    if drm.has_render != vk::TRUE || drm.render_major < 0 || drm.render_minor < 0 {
        return None;
    }

    use std::os::unix::fs::MetadataExt as _;
    let entries = std::fs::read_dir("/dev/dri").ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if !entry.file_name().to_string_lossy().starts_with("renderD") {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let device = metadata.rdev();
        if i64::from(libc::major(device)) == drm.render_major
            && i64::from(libc::minor(device)) == drm.render_minor
        {
            return Some(path);
        }
    }
    None
}

#[cfg(not(target_os = "linux"))]
fn drm_render_node(
    _instance: &ash::Instance,
    _physical_device: vk::PhysicalDevice,
    _instance_extensions: &[&'static CStr],
) -> Option<PathBuf> {
    None
}

/// One application-owned texture that libmpv may render into.
pub struct VulkanVideoTexture {
    texture: Arc<wgpu::Texture>,
    view: wgpu::TextureView,
    raw_device: Option<ash::Device>,
    image: Option<vk::Image>,
    semaphore: Option<vk::Semaphore>,
    available_value: AtomicU64,
    reusable: AtomicBool,
    width: u32,
    height: u32,
}

impl VulkanVideoTexture {
    pub fn image(&self) -> Result<vk::Image> {
        self.image
            .ok_or_else(|| anyhow!("video texture has no Vulkan image"))
    }

    pub fn semaphore(&self) -> Result<vk::Semaphore> {
        self.semaphore
            .ok_or_else(|| anyhow!("video texture has no Vulkan semaphore"))
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    /// Reserve the next pair of timeline values for an mpv render and the
    /// subsequent wgpu sample.
    pub fn try_begin_render(&self) -> Option<VulkanTextureSync> {
        if self
            .reusable
            .compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return None;
        }
        let available = self.available_value.load(Ordering::Acquire);
        Some(VulkanTextureSync {
            wait_value: available,
            ready_value: available + 1,
            available_value: available + 2,
        })
    }

    pub fn cancel_render(&self) {
        self.reusable.store(true, Ordering::Release);
    }
}

impl Drop for VulkanVideoTexture {
    fn drop(&mut self) {
        self.texture.destroy();
        if let (Some(raw_device), Some(semaphore)) = (&self.raw_device, self.semaphore) {
            unsafe { raw_device.destroy_semaphore(semaphore, None) };
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct VulkanTextureSync {
    pub wait_value: u64,
    pub ready_value: u64,
    pub available_value: u64,
}

/// A resize generation containing a triple-buffered texture ring.
pub struct VideoTextureGeneration {
    pub id: u64,
    pub textures: Vec<Arc<VulkanVideoTexture>>,
}

#[derive(Clone)]
struct PublishedFrame {
    sequence: u64,
    texture: Arc<VulkanVideoTexture>,
    ready_value: Option<u64>,
    available_value: u64,
    claimed: bool,
}

impl PublishedFrame {
    fn release_if_unclaimed(self) -> Result<(), Self> {
        if self.claimed {
            return Err(self);
        }
        if let Some(ready_value) = self.ready_value {
            // This frame was never sampled by wgpu. The next mpv render only
            // needs to wait for the render that produced it, not for a
            // consumer signal that will never exist.
            self.texture
                .available_value
                .store(ready_value, Ordering::Release);
        }
        self.texture.reusable.store(true, Ordering::Release);
        Ok(())
    }
}

#[derive(Default)]
struct SurfaceState {
    current: Option<PublishedFrame>,
    obsolete: Vec<PublishedFrame>,
    prepared_releases: Vec<Arc<VulkanVideoTexture>>,
}

struct VideoSurfaceInner {
    device: Arc<wgpu::Device>,
    queue: Arc<wgpu::Queue>,
    width: AtomicU32,
    height: AtomicU32,
    requested_width: AtomicU32,
    requested_height: AtomicU32,
    next_generation: AtomicU64,
    next_publication: AtomicU64,
    state: Mutex<SurfaceState>,
    resize: Box<dyn Fn(u32, u32) + Send + Sync>,
}

/// A persistent GPUI video surface backed by application-owned wgpu textures.
#[derive(Clone)]
pub struct WgpuVideoSurface {
    inner: Arc<VideoSurfaceInner>,
}

impl WgpuVideoSurface {
    pub fn new(resize: impl Fn(u32, u32) + Send + Sync + 'static) -> Result<Self> {
        let (device, queue) = active_context()?;
        Ok(Self {
            inner: Arc::new(VideoSurfaceInner {
                device,
                queue,
                width: AtomicU32::new(1),
                height: AtomicU32::new(1),
                requested_width: AtomicU32::new(0),
                requested_height: AtomicU32::new(0),
                next_generation: AtomicU64::new(1),
                next_publication: AtomicU64::new(1),
                state: Mutex::new(SurfaceState::default()),
                resize: Box::new(resize),
            }),
        })
    }

    pub fn platform_surface(&self) -> PlatformSurface {
        PlatformSurface(Arc::new(self.clone()))
    }

    pub fn vulkan_device(&self) -> Result<VulkanVideoDevice> {
        let hal_device = unsafe { self.inner.device.as_hal::<VulkanApi>() }
            .ok_or_else(|| anyhow!("GPUI is not using wgpu's Vulkan backend"))?;
        let hal_queue = unsafe { self.inner.queue.as_hal::<VulkanApi>() }
            .ok_or_else(|| anyhow!("GPUI is not using wgpu's Vulkan backend"))?;
        let queue_family = hal_device.queue_family_index();
        let device_extensions = hal_device.enabled_device_extensions().to_vec();
        let instance = hal_device.shared_instance().raw_instance().clone();
        let physical_device = hal_device.raw_physical_device();
        let instance_extensions = hal_device.shared_instance().extensions().to_vec();
        let drm_render_node = drm_render_node(&instance, physical_device, &instance_extensions);
        let queue_flags = unsafe {
            instance.get_physical_device_queue_family_properties(physical_device)
        }
        .get(queue_family as usize)
        .ok_or_else(|| anyhow!("wgpu exported an unknown Vulkan queue family"))?
        .queue_flags;
        // A raw VkDevice cannot acquire another physical-device queue family
        // after creation. Only advertise the family wgpu actually requested.
        let enabled_queue_families = vec![(queue_family, 1)];
        Ok(VulkanVideoDevice {
            instance,
            physical_device,
            device: hal_device.raw_device().clone(),
            get_proc_address: hal_device
                .shared_instance()
                .entry()
                .static_fn()
                .get_instance_proc_addr,
            queue_family,
            queue_index: hal_device.queue_index(),
            queue_flags,
            instance_extensions,
            device_extensions,
            enabled_queue_families,
            queue_lock: hal_queue.host_lock(),
            drm_render_node,
        })
    }

    pub fn allocate_generation(&self, width: u32, height: u32) -> Result<VideoTextureGeneration> {
        if width == 0 || height == 0 {
            return Err(anyhow!("video texture dimensions must not be zero"));
        }
        let native = self.vulkan_device()?;
        let mut textures = Vec::with_capacity(VIDEO_TEXTURE_COUNT);
        for _ in 0..VIDEO_TEXTURE_COUNT {
            let texture = Arc::new(self.inner.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("mpv_video_texture"),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING
                    | wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::COPY_SRC
                    | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            }));
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            let image = {
                let hal_texture = unsafe { texture.as_hal::<VulkanApi>() }
                    .context("get Vulkan video texture")?;
                unsafe { hal_texture.raw_handle() }
            };
            let semaphore = unsafe {
                native.device.create_semaphore(
                    &vk::SemaphoreCreateInfo::default().push_next(
                        &mut vk::SemaphoreTypeCreateInfo::default()
                            .semaphore_type(vk::SemaphoreType::TIMELINE)
                            .initial_value(0),
                    ),
                    None,
                )
            }
            .context("create video timeline semaphore")?;
            textures.push(Arc::new(VulkanVideoTexture {
                texture,
                view,
                raw_device: Some(native.device.clone()),
                image: Some(image),
                semaphore: Some(semaphore),
                available_value: AtomicU64::new(1),
                reusable: AtomicBool::new(true),
                width,
                height,
            }));
        }

        let mut encoder = self
            .inner
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("initialize_mpv_video_textures"),
            });
        for texture in &textures {
            // Raw Vulkan writes performed by libmpv are invisible to wgpu's
            // lazy memory-initialization tracker. Explicitly initialize every
            // image through wgpu before exporting it; otherwise the first
            // sampled binding can inject a zero-clear after libmpv has rendered
            // the decoded frame.
            let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("initialize_mpv_video_texture"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &texture.view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                ..Default::default()
            });
        }
        encoder.transition_resources(
            std::iter::empty(),
            textures.iter().map(|texture| wgpu::TextureTransition {
                texture: texture.texture.as_ref(),
                selector: None,
                state: wgpu::TextureUses::RESOURCE,
            }),
        );
        {
            let hal_queue = unsafe { self.inner.queue.as_hal::<VulkanApi>() }
                .ok_or_else(|| anyhow!("GPUI is not using wgpu's Vulkan backend"))?;
            for texture in &textures {
                hal_queue.add_signal_semaphore(texture.semaphore()?, Some(1));
            }
        }
        self.inner.queue.submit([encoder.finish()]);
        self.inner.width.store(width, Ordering::Release);
        self.inner.height.store(height, Ordering::Release);

        Ok(VideoTextureGeneration {
            id: self.inner.next_generation.fetch_add(1, Ordering::Relaxed),
            textures,
        })
    }

    /// Allocates a texture ring for frames uploaded through wgpu itself. Unlike
    /// Vulkan interop, this works with every wgpu backend GPUI can select.
    pub fn allocate_software_generation(
        &self,
        width: u32,
        height: u32,
    ) -> Result<VideoTextureGeneration> {
        if width == 0 || height == 0 {
            return Err(anyhow!("video texture dimensions must not be zero"));
        }
        let mut textures = Vec::with_capacity(VIDEO_TEXTURE_COUNT);
        for _ in 0..VIDEO_TEXTURE_COUNT {
            let texture = Arc::new(self.inner.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("software_video_texture"),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING
                    | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            }));
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            textures.push(Arc::new(VulkanVideoTexture {
                texture,
                view,
                raw_device: None,
                image: None,
                semaphore: None,
                available_value: AtomicU64::new(0),
                reusable: AtomicBool::new(true),
                width,
                height,
            }));
        }
        self.inner.width.store(width, Ordering::Release);
        self.inner.height.store(height, Ordering::Release);
        Ok(VideoTextureGeneration {
            id: self.inner.next_generation.fetch_add(1, Ordering::Relaxed),
            textures,
        })
    }

    pub fn publish(&self, texture: Arc<VulkanVideoTexture>, sync: VulkanTextureSync) {
        texture
            .available_value
            .store(sync.available_value, Ordering::Release);
        let sequence = self.inner.next_publication.fetch_add(1, Ordering::Relaxed);
        let mut state = self.inner.state.lock();
        if let Some(previous) = state.current.replace(PublishedFrame {
            sequence,
            texture,
            ready_value: Some(sync.ready_value),
            available_value: sync.available_value,
            claimed: false,
        }) && let Err(previous) = previous.release_if_unclaimed()
        {
            state.obsolete.push(previous);
        }
        if sequence <= VIDEO_SURFACE_TRACE_LIMIT {
            log::info!("video surface published Vulkan frame sequence={sequence}");
        }
    }

    pub fn clear(&self) {
        let mut state = self.inner.state.lock();
        if let Some(previous) = state.current.take()
            && let Err(previous) = previous.release_if_unclaimed()
        {
            state.obsolete.push(previous);
        }
    }

    pub fn wait_idle(&self) -> Result<()> {
        self.inner
            .device
            .poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: None,
            })
            .map_err(|error| anyhow!("wait for video GPU work: {error:?}"))?;
        Ok(())
    }

    /// Upload an RGBA software-rendered frame into an existing ring texture.
    pub fn publish_rgba(
        &self,
        texture: Arc<VulkanVideoTexture>,
        pixels: &[u8],
        bytes_per_row: u32,
    ) -> Result<()> {
        let required = usize::try_from(bytes_per_row)?
            .checked_mul(texture.height as usize)
            .ok_or_else(|| anyhow!("software video frame is too large"))?;
        if pixels.len() < required || bytes_per_row < texture.width * 4 {
            return Err(anyhow!("software video frame buffer is too small"));
        }
        self.inner.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: texture.texture.as_ref(),
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            pixels,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(bytes_per_row),
                rows_per_image: Some(texture.height),
            },
            wgpu::Extent3d {
                width: texture.width,
                height: texture.height,
                depth_or_array_layers: 1,
            },
        );
        let mut state = self.inner.state.lock();
        let sequence = self.inner.next_publication.fetch_add(1, Ordering::Relaxed);
        if let Some(previous) = state.current.replace(PublishedFrame {
            sequence,
            texture,
            ready_value: None,
            available_value: 0,
            claimed: false,
        }) && let Err(previous) = previous.release_if_unclaimed()
        {
            state.obsolete.push(previous);
        }
        if sequence <= VIDEO_SURFACE_TRACE_LIMIT {
            log::info!("video surface published software frame sequence={sequence}");
        }
        Ok(())
    }

    pub(crate) fn prepare_draw(&self) -> Option<wgpu::TextureView> {
        struct DrawSync {
            texture: Arc<VulkanVideoTexture>,
            wait_value: Option<u64>,
            signal_value: Option<u64>,
            release_after_submit: bool,
        }

        let (texture, syncs, current_sequence, newly_claimed) = {
            let mut state = self.inner.state.lock();
            let mut syncs = state
                .obsolete
                .drain(..)
                .map(|obsolete| {
                    let signal_value = if obsolete.ready_value.is_some() {
                        let available_value = if obsolete.claimed {
                            obsolete.available_value + 1
                        } else {
                            obsolete.available_value
                        };
                        obsolete
                            .texture
                            .available_value
                            .store(available_value, Ordering::Release);
                        Some(available_value)
                    } else {
                        None
                    };
                    DrawSync {
                        texture: obsolete.texture,
                        wait_value: (!obsolete.claimed)
                            .then_some(obsolete.ready_value)
                            .flatten(),
                        signal_value,
                        release_after_submit: true,
                    }
                })
                .collect::<Vec<_>>();
            let mut current_sequence = None;
            let mut newly_claimed = false;
            let texture = state.current.as_mut().map(|current| {
                current_sequence = Some(current.sequence);
                if !current.claimed {
                    current.claimed = true;
                    newly_claimed = true;
                    syncs.push(DrawSync {
                        texture: current.texture.clone(),
                        wait_value: current.ready_value,
                        signal_value: current.ready_value.map(|_| current.available_value),
                        // The displayed texture remains reserved until a newer
                        // publication replaces it. GPUI can sample the current
                        // surface again on any unrelated window redraw.
                        release_after_submit: false,
                    });
                }
                current.texture.view.clone()
            });
            (texture, syncs, current_sequence, newly_claimed)
        };
        if newly_claimed
            && let Some(sequence) = current_sequence
            && sequence <= VIDEO_SURFACE_TRACE_LIMIT
        {
            log::info!("video surface prepared published frame sequence={sequence}");
        }
        if !syncs.is_empty() {
            let requires_vulkan_sync = syncs
                .iter()
                .any(|sync| sync.wait_value.is_some() || sync.signal_value.is_some());
            let hal_queue = if requires_vulkan_sync {
                match unsafe { self.inner.queue.as_hal::<VulkanApi>() } {
                    Some(queue) => Some(queue),
                    None => {
                        log::error!(
                            "Vulkan video synchronization requested on a non-Vulkan wgpu backend"
                        );
                        return None;
                    }
                }
            } else {
                None
            };
            let mut state = self.inner.state.lock();
            for sync in syncs {
                if let Some(ready_value) = sync.wait_value {
                    let semaphore = match sync.texture.semaphore() {
                        Ok(semaphore) => semaphore,
                        Err(error) => {
                            log::error!("could not prepare Vulkan video wait: {error:#}");
                            return None;
                        }
                    };
                    if let Some(hal_queue) = hal_queue.as_ref() {
                        hal_queue.add_wait_semaphore(
                            semaphore,
                            Some(ready_value),
                            vk::PipelineStageFlags::FRAGMENT_SHADER,
                        );
                    }
                }
                if let Some(available_value) = sync.signal_value {
                    let semaphore = match sync.texture.semaphore() {
                        Ok(semaphore) => semaphore,
                        Err(error) => {
                            log::error!("could not prepare Vulkan video signal: {error:#}");
                            return None;
                        }
                    };
                    if let Some(hal_queue) = hal_queue.as_ref() {
                        hal_queue.add_signal_semaphore(semaphore, Some(available_value));
                    }
                }
                if sync.release_after_submit {
                    state.prepared_releases.push(sync.texture);
                }
            }
        }
        texture
    }

    pub(crate) fn finish_draw(&self) {
        let prepared = {
            let mut state = self.inner.state.lock();
            std::mem::take(&mut state.prepared_releases)
        };
        for texture in prepared {
            texture.reusable.store(true, Ordering::Release);
        }
    }
}

impl PlatformSurfaceSource for WgpuVideoSurface {
    fn size(&self) -> Size<DevicePixels> {
        gpui::size(
            DevicePixels(self.inner.width.load(Ordering::Acquire) as i32),
            DevicePixels(self.inner.height.load(Ordering::Acquire) as i32),
        )
    }

    fn request_size(&self, size: Size<DevicePixels>) {
        let width = size.width.0.max(1) as u32;
        let height = size.height.0.max(1) as u32;
        let old_width = self.inner.requested_width.swap(width, Ordering::AcqRel);
        let old_height = self.inner.requested_height.swap(height, Ordering::AcqRel);
        if old_width != width || old_height != height {
            (self.inner.resize)(width, height);
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

pub(crate) fn video_surface_from_platform(
    source: &PlatformSurface,
) -> Option<WgpuVideoSurface> {
    source.0.as_any().downcast_ref::<WgpuVideoSurface>().cloned()
}
