use crate::{Error, Mpv, Result, mpv::mpv_err};
use ash::vk;
use std::{
    ffi::{CStr, CString, c_char, c_void},
    ptr::{self, NonNull},
    sync::Arc,
};

const PARAM_INVALID: u32 = 0;
const PARAM_API_TYPE: u32 = 1;
const PARAM_OPENGL_INIT_PARAMS: u32 = 2;
const PARAM_OPENGL_FBO: u32 = 3;
const PARAM_FLIP_Y: u32 = 4;
const PARAM_ADVANCED_CONTROL: u32 = 10;
const PARAM_NEXT_FRAME_INFO: u32 = 11;
const PARAM_SKIP_RENDERING: u32 = 13;
const PARAM_SW_SIZE: u32 = 17;
const PARAM_SW_FORMAT: u32 = 18;
const PARAM_SW_STRIDE: u32 = 19;
const PARAM_SW_POINTER: u32 = 20;
const PARAM_VULKAN_INIT_PARAMS: u32 = 21;
const PARAM_VULKAN_TARGET: u32 = 22;
const PARAM_VULKAN_TARGET_REMOVE: u32 = 23;
const PARAM_LATEST_FRAME: u32 = 24;
const PARAM_NEXT_FRAME_VIDEO_PTS: u32 = 25;

const API_OPENGL: &[u8] = b"opengl\0";
const API_SOFTWARE: &[u8] = b"sw\0";
const API_VULKAN: &[u8] = b"vulkan\0";

pub type MpvRenderUpdate = u64;
pub mod mpv_render_update {
    pub use libmpv2_sys::mpv_render_update_flag_MPV_RENDER_UPDATE_FRAME as Frame;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RenderFrameInfo {
    pub flags: u64,
    pub target_time: i64,
}

impl RenderFrameInfo {
    pub fn is_present(self) -> bool {
        self.flags
            & u64::from(libmpv2_sys::mpv_render_frame_info_flag_MPV_RENDER_FRAME_INFO_PRESENT)
            != 0
    }

    pub fn is_redraw(self) -> bool {
        self.flags
            & u64::from(libmpv2_sys::mpv_render_frame_info_flag_MPV_RENDER_FRAME_INFO_REDRAW)
            != 0
    }

    pub fn is_repeat(self) -> bool {
        self.flags
            & u64::from(libmpv2_sys::mpv_render_frame_info_flag_MPV_RENDER_FRAME_INFO_REPEAT)
            != 0
    }
}

/// Serializes external Vulkan queue operations with the application renderer.
pub trait VulkanQueueLock: Send + Sync + 'static {
    fn lock(&self, family: u32, index: u32);

    /// # Safety
    ///
    /// The corresponding queue lock must currently be held by this thread.
    unsafe fn unlock(&self, family: u32, index: u32);
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VulkanQueueFamily {
    pub index: u32,
    pub count: u32,
}

/// The Vulkan features enabled on the application-owned logical device.
#[derive(Clone, Debug, Default)]
pub struct VulkanFeatures {
    pub core: vk::PhysicalDeviceFeatures,
    pub timeline_semaphore: bool,
    pub host_query_reset: bool,
}

/// Parameters used to import an application-owned Vulkan device into mpv.
pub struct VulkanInitParams {
    pub instance: vk::Instance,
    pub physical_device: vk::PhysicalDevice,
    pub device: vk::Device,
    pub get_proc_address: vk::PFN_vkGetInstanceProcAddr,
    pub instance_extensions: Vec<CString>,
    pub device_extensions: Vec<CString>,
    pub features: VulkanFeatures,
    pub graphics_queue: VulkanQueueFamily,
    pub compute_queue: VulkanQueueFamily,
    pub transfer_queue: VulkanQueueFamily,
    pub enabled_queue_families: Vec<VulkanQueueFamily>,
    pub queue_lock: Arc<dyn VulkanQueueLock>,
    /// Let decoding run ahead of rendering while retaining only the newest
    /// unconsumed frame. Intended for untimed, damage-driven live video.
    pub latest_frame: bool,
}

/// An application-owned Vulkan image used as a render target.
#[derive(Clone, Copy, Debug)]
pub struct VulkanRenderTarget {
    pub image: vk::Image,
    pub format: vk::Format,
    pub usage: vk::ImageUsageFlags,
    pub width: u32,
    pub height: u32,
    pub input_layout: vk::ImageLayout,
    pub output_layout: vk::ImageLayout,
    pub wait_semaphore: vk::Semaphore,
    pub wait_value: u64,
    pub signal_semaphore: vk::Semaphore,
    pub signal_value: u64,
}

/// Parameters for mpv's portable packed-pixel software renderer.
pub struct SoftwareRenderTarget<'a> {
    pub width: u32,
    pub height: u32,
    pub format: &'a CStr,
    pub stride: usize,
    pub pixels: &'a mut [u8],
}

pub struct OpenGlInitParams {
    pub get_proc_address: Arc<dyn Fn(&CStr) -> *mut c_void + Send + Sync>,
}

#[repr(C)]
struct RawVulkanQueueFamily {
    index: u32,
    count: u32,
}

impl From<VulkanQueueFamily> for RawVulkanQueueFamily {
    fn from(value: VulkanQueueFamily) -> Self {
        Self {
            index: value.index,
            count: value.count,
        }
    }
}

type RawQueueCallback = unsafe extern "C" fn(*mut c_void, u32, u32);

#[repr(C)]
struct RawVulkanInitParams {
    instance: vk::Instance,
    physical_device: vk::PhysicalDevice,
    device: vk::Device,
    get_proc_address: vk::PFN_vkGetInstanceProcAddr,
    instance_extensions: *const *const c_char,
    num_instance_extensions: i32,
    device_extensions: *const *const c_char,
    num_device_extensions: i32,
    features: *const c_void,
    graphics_queue: RawVulkanQueueFamily,
    compute_queue: RawVulkanQueueFamily,
    transfer_queue: RawVulkanQueueFamily,
    enabled_queue_families: *const RawVulkanQueueFamily,
    num_enabled_queue_families: i32,
    lock_queue: Option<RawQueueCallback>,
    unlock_queue: Option<RawQueueCallback>,
    queue_ctx: *mut c_void,
}

#[repr(C)]
struct RawVulkanTarget {
    image: vk::Image,
    format: vk::Format,
    usage: vk::ImageUsageFlags,
    width: i32,
    height: i32,
    input_layout: vk::ImageLayout,
    output_layout: vk::ImageLayout,
    wait_semaphore: vk::Semaphore,
    wait_value: u64,
    signal_semaphore: vk::Semaphore,
    signal_value: u64,
}

struct QueueCallbackContext(Arc<dyn VulkanQueueLock>);

unsafe extern "C" fn lock_queue(context: *mut c_void, family: u32, index: u32) {
    if let Some(context) = unsafe { (context as *const QueueCallbackContext).as_ref() } {
        context.0.lock(family, index);
    }
}

unsafe extern "C" fn unlock_queue(context: *mut c_void, family: u32, index: u32) {
    if let Some(context) = unsafe { (context as *const QueueCallbackContext).as_ref() } {
        unsafe { context.0.unlock(family, index) };
    }
}

struct UpdateCallback(Box<dyn Fn() + Send>);

unsafe extern "C" fn update_callback(context: *mut c_void) {
    if let Some(callback) = unsafe { (context as *const UpdateCallback).as_ref() } {
        (callback.0)();
    }
}

struct OpenGlCallback(Arc<dyn Fn(&CStr) -> *mut c_void + Send + Sync>);

unsafe extern "C" fn get_proc_address(
    context: *mut c_void,
    name: *const c_char,
) -> *mut c_void {
    let Some(callback) = (unsafe { (context as *const OpenGlCallback).as_ref() }) else {
        return ptr::null_mut();
    };
    (callback.0)(unsafe { CStr::from_ptr(name) })
}

/// An owned libmpv render context. All methods must be serialized on one
/// render thread after the context is created.
pub struct RenderContext {
    ctx: NonNull<libmpv2_sys::mpv_render_context>,
    _mpv: Arc<Mpv>,
    update_callback: Option<Box<UpdateCallback>>,
    queue_callback: Option<Box<QueueCallbackContext>>,
    opengl_callback: Option<Box<OpenGlCallback>>,
}

// Moving a newly created context to its dedicated render thread is supported
// by libmpv. RenderContext intentionally does not implement Sync.
unsafe impl Send for RenderContext {}

impl Mpv {
    pub fn create_vulkan_render_context(
        self: &Arc<Self>,
        params: VulkanInitParams,
    ) -> Result<RenderContext> {
        let instance_extensions = params
            .instance_extensions
            .iter()
            .map(|extension| extension.as_ptr())
            .collect::<Vec<_>>();
        let device_extensions = params
            .device_extensions
            .iter()
            .map(|extension| extension.as_ptr())
            .collect::<Vec<_>>();
        let enabled_queues = params
            .enabled_queue_families
            .iter()
            .copied()
            .map(Into::into)
            .collect::<Vec<_>>();
        let mut vulkan_11 = vk::PhysicalDeviceVulkan11Features::default();
        let mut vulkan_12 = vk::PhysicalDeviceVulkan12Features::default()
            .timeline_semaphore(params.features.timeline_semaphore)
            .host_query_reset(params.features.host_query_reset);
        let features = vk::PhysicalDeviceFeatures2::default()
            .features(params.features.core)
            .push_next(&mut vulkan_11)
            .push_next(&mut vulkan_12);
        let mut queue_callback = Box::new(QueueCallbackContext(params.queue_lock));
        let mut raw_init = RawVulkanInitParams {
            instance: params.instance,
            physical_device: params.physical_device,
            device: params.device,
            get_proc_address: params.get_proc_address,
            instance_extensions: instance_extensions.as_ptr(),
            num_instance_extensions: instance_extensions.len() as i32,
            device_extensions: device_extensions.as_ptr(),
            num_device_extensions: device_extensions.len() as i32,
            features: (&features as *const vk::PhysicalDeviceFeatures2<'_>).cast(),
            graphics_queue: params.graphics_queue.into(),
            compute_queue: params.compute_queue.into(),
            transfer_queue: params.transfer_queue.into(),
            enabled_queue_families: enabled_queues.as_ptr(),
            num_enabled_queue_families: enabled_queues.len() as i32,
            lock_queue: Some(lock_queue),
            unlock_queue: Some(unlock_queue),
            queue_ctx: (&mut *queue_callback as *mut QueueCallbackContext).cast(),
        };
        let mut advanced = 1_i32;
        let mut latest_frame = i32::from(params.latest_frame);
        let mut raw_params = [
            raw_param(PARAM_API_TYPE, API_VULKAN.as_ptr().cast_mut().cast()),
            raw_param(PARAM_VULKAN_INIT_PARAMS, (&mut raw_init as *mut RawVulkanInitParams).cast()),
            raw_param(PARAM_ADVANCED_CONTROL, (&mut advanced as *mut i32).cast()),
            raw_param(PARAM_LATEST_FRAME, (&mut latest_frame as *mut i32).cast()),
            raw_param(PARAM_INVALID, ptr::null_mut()),
        ];
        create_context(self, &mut raw_params, Some(queue_callback), None)
    }

    pub fn create_software_render_context(
        self: &Arc<Self>,
        latest_frame: bool,
    ) -> Result<RenderContext> {
        let mut advanced = 1_i32;
        let mut latest_frame = i32::from(latest_frame);
        let mut params = [
            raw_param(PARAM_API_TYPE, API_SOFTWARE.as_ptr().cast_mut().cast()),
            raw_param(PARAM_ADVANCED_CONTROL, (&mut advanced as *mut i32).cast()),
            raw_param(PARAM_LATEST_FRAME, (&mut latest_frame as *mut i32).cast()),
            raw_param(PARAM_INVALID, ptr::null_mut()),
        ];
        create_context(self, &mut params, None, None)
    }

    pub fn create_opengl_render_context(
        self: &Arc<Self>,
        params: OpenGlInitParams,
    ) -> Result<RenderContext> {
        let mut callback = Box::new(OpenGlCallback(params.get_proc_address));
        let mut raw_init = libmpv2_sys::mpv_opengl_init_params {
            get_proc_address: Some(get_proc_address),
            get_proc_address_ctx: (&mut *callback as *mut OpenGlCallback).cast(),
        };
        let mut advanced = 1_i32;
        let mut raw_params = [
            raw_param(PARAM_API_TYPE, API_OPENGL.as_ptr().cast_mut().cast()),
            raw_param(
                PARAM_OPENGL_INIT_PARAMS,
                (&mut raw_init as *mut libmpv2_sys::mpv_opengl_init_params).cast(),
            ),
            raw_param(PARAM_ADVANCED_CONTROL, (&mut advanced as *mut i32).cast()),
            raw_param(PARAM_INVALID, ptr::null_mut()),
        ];
        create_context(self, &mut raw_params, None, Some(callback))
    }
}

fn create_context(
    mpv: &Arc<Mpv>,
    params: &mut [libmpv2_sys::mpv_render_param],
    queue_callback: Option<Box<QueueCallbackContext>>,
    opengl_callback: Option<Box<OpenGlCallback>>,
) -> Result<RenderContext> {
    let mut context = ptr::null_mut();
    let code = unsafe {
        libmpv2_sys::mpv_render_context_create(
            &mut context,
            mpv.ctx.as_ptr(),
            params.as_mut_ptr(),
        )
    };
    mpv_err((), code)?;
    Ok(RenderContext {
        ctx: NonNull::new(context).ok_or(Error::Raw(
            libmpv2_sys::mpv_error_MPV_ERROR_GENERIC,
        ))?,
        _mpv: mpv.clone(),
        update_callback: None,
        queue_callback,
        opengl_callback,
    })
}

impl RenderContext {
    pub fn as_ptr(&self) -> *mut libmpv2_sys::mpv_render_context {
        self.ctx.as_ptr()
    }

    pub fn set_update_callback(&mut self, callback: impl Fn() + Send + 'static) {
        let mut callback = Box::new(UpdateCallback(Box::new(callback)));
        unsafe {
            libmpv2_sys::mpv_render_context_set_update_callback(
                self.ctx.as_ptr(),
                Some(update_callback),
                (&mut *callback as *mut UpdateCallback).cast(),
            );
        }
        self.update_callback = Some(callback);
    }

    pub fn update(&self) -> MpvRenderUpdate {
        unsafe { libmpv2_sys::mpv_render_context_update(self.ctx.as_ptr()) }
    }

    pub fn next_frame_info(&self) -> Result<RenderFrameInfo> {
        let mut info = libmpv2_sys::mpv_render_frame_info {
            flags: 0,
            target_time: 0,
        };
        let parameter = raw_param(
            PARAM_NEXT_FRAME_INFO,
            (&mut info as *mut libmpv2_sys::mpv_render_frame_info).cast(),
        );
        let code = unsafe {
            libmpv2_sys::mpv_render_context_get_info(self.ctx.as_ptr(), parameter)
        };
        mpv_err(
            RenderFrameInfo {
                flags: info.flags,
                target_time: info.target_time,
            },
            code,
        )
    }

    /// Source PTS in seconds for the next pending video image, or NaN if the
    /// update contains no video image.
    pub fn next_frame_video_pts(&self) -> Result<f64> {
        let mut pts = f64::NAN;
        let parameter = raw_param(
            PARAM_NEXT_FRAME_VIDEO_PTS,
            (&mut pts as *mut f64).cast(),
        );
        let code = unsafe {
            libmpv2_sys::mpv_render_context_get_info(self.ctx.as_ptr(), parameter)
        };
        mpv_err(pts, code)
    }

    pub fn skip_rendering(&self) -> Result<()> {
        let mut skip = 1_i32;
        let mut params = [
            raw_param(PARAM_SKIP_RENDERING, (&mut skip as *mut i32).cast()),
            raw_param(PARAM_INVALID, ptr::null_mut()),
        ];
        self.render_raw(&mut params)
    }

    pub fn render_vulkan(&self, target: VulkanRenderTarget) -> Result<()> {
        let mut target = RawVulkanTarget {
            image: target.image,
            format: target.format,
            usage: target.usage,
            width: target.width as i32,
            height: target.height as i32,
            input_layout: target.input_layout,
            output_layout: target.output_layout,
            wait_semaphore: target.wait_semaphore,
            wait_value: target.wait_value,
            signal_semaphore: target.signal_semaphore,
            signal_value: target.signal_value,
        };
        let mut params = [
            raw_param(PARAM_VULKAN_TARGET, (&mut target as *mut RawVulkanTarget).cast()),
            raw_param(PARAM_INVALID, ptr::null_mut()),
        ];
        self.render_raw(&mut params)
    }

    pub fn remove_vulkan_target(&self, image: vk::Image) -> Result<()> {
        let mut image = image;
        let parameter = raw_param(PARAM_VULKAN_TARGET_REMOVE, (&mut image as *mut vk::Image).cast());
        let code = unsafe {
            libmpv2_sys::mpv_render_context_set_parameter(self.ctx.as_ptr(), parameter)
        };
        mpv_err((), code)
    }

    pub fn render_software(&self, target: SoftwareRenderTarget<'_>) -> Result<()> {
        let required = target
            .stride
            .checked_mul(target.height as usize)
            .ok_or(Error::Raw(libmpv2_sys::mpv_error_MPV_ERROR_INVALID_PARAMETER))?;
        if target.width == 0 || target.height == 0 || target.pixels.len() < required {
            return Err(Error::Raw(
                libmpv2_sys::mpv_error_MPV_ERROR_INVALID_PARAMETER,
            ));
        }
        let mut size = [target.width as i32, target.height as i32];
        let mut stride = target.stride;
        let mut params = [
            raw_param(PARAM_SW_SIZE, size.as_mut_ptr().cast()),
            raw_param(PARAM_SW_FORMAT, target.format.as_ptr().cast_mut().cast()),
            raw_param(PARAM_SW_STRIDE, (&mut stride as *mut usize).cast()),
            raw_param(PARAM_SW_POINTER, target.pixels.as_mut_ptr().cast()),
            raw_param(PARAM_INVALID, ptr::null_mut()),
        ];
        self.render_raw(&mut params)
    }

    pub fn render_opengl(&self, fbo: i32, width: i32, height: i32, flip_y: bool) -> Result<()> {
        let mut fbo = libmpv2_sys::mpv_opengl_fbo {
            fbo,
            w: width,
            h: height,
            internal_format: 0,
        };
        let mut flip_y = i32::from(flip_y);
        let mut params = [
            raw_param(
                PARAM_OPENGL_FBO,
                (&mut fbo as *mut libmpv2_sys::mpv_opengl_fbo).cast(),
            ),
            raw_param(PARAM_FLIP_Y, (&mut flip_y as *mut i32).cast()),
            raw_param(PARAM_INVALID, ptr::null_mut()),
        ];
        self.render_raw(&mut params)
    }

    pub fn report_swap(&self) {
        unsafe { libmpv2_sys::mpv_render_context_report_swap(self.ctx.as_ptr()) };
    }

    fn render_raw(&self, params: &mut [libmpv2_sys::mpv_render_param]) -> Result<()> {
        let code = unsafe {
            libmpv2_sys::mpv_render_context_render(self.ctx.as_ptr(), params.as_mut_ptr())
        };
        mpv_err((), code)
    }
}

impl Drop for RenderContext {
    fn drop(&mut self) {
        unsafe {
            libmpv2_sys::mpv_render_context_set_update_callback(
                self.ctx.as_ptr(),
                None,
                ptr::null_mut(),
            );
            libmpv2_sys::mpv_render_context_free(self.ctx.as_ptr());
        }
        self.update_callback.take();
        self.queue_callback.take();
        self.opengl_callback.take();
    }
}

fn raw_param(type_: u32, data: *mut c_void) -> libmpv2_sys::mpv_render_param {
    libmpv2_sys::mpv_render_param { type_, data }
}
