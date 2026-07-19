use std::{ffi::CStr, os::raw::c_void, ptr};

use anyhow::{Result, anyhow, bail};
use async_channel::Sender;
use libmpv2::{
    Format, Mpv,
    events::{Event, PropertyData},
};
use libmpv2_sys as sys;

const FORMAT_BGR0: &[u8] = b"bgr0\0";

/// A libmpv client paired with its software render context.
///
/// GPUI owns the final GPU texture. libmpv renders each video frame into this
/// process' memory, which is then handed to GPUI as BGRA pixels.
pub struct MpvPlayer {
    render_context: *mut sys::mpv_render_context,
    mpv: Mpv,
    _render_wakeup: Box<Sender<()>>,
    last_render_size: Option<(usize, usize)>,
    surface: AlignedSurface,
    playback: PlaybackState,
}

impl MpvPlayer {
    pub fn new(wakeup: Sender<()>) -> Result<Self> {
        let mut mpv = Mpv::with_initializer(|initializer| {
            initializer.set_option("vo", "libmpv")?;
            initializer.set_option("keep-open", "no")?;
            initializer.set_option("idle", "yes")?;
            initializer.set_option("osc", "no")?;
            Ok(())
        })?;

        let mut render_context = ptr::null_mut();
        let mut params = [
            sys::mpv_render_param {
                type_: sys::mpv_render_param_type_MPV_RENDER_PARAM_API_TYPE,
                data: sys::MPV_RENDER_API_TYPE_SW.as_ptr() as *mut c_void,
            },
            sys::mpv_render_param {
                type_: sys::mpv_render_param_type_MPV_RENDER_PARAM_INVALID,
                data: ptr::null_mut(),
            },
        ];

        // SAFETY: `mpv` has been initialized, `params` is null-terminated, and
        // the render context is freed before the owning mpv handle in Drop.
        let code = unsafe {
            sys::mpv_render_context_create(
                &mut render_context,
                mpv.ctx.as_ptr(),
                params.as_mut_ptr(),
            )
        };
        check_mpv(code, "create software render context")?;

        mpv.observe_property("time-pos", Format::Double, 1)?;
        mpv.observe_property("duration", Format::Double, 2)?;
        mpv.observe_property("pause", Format::Flag, 3)?;
        mpv.observe_property("eof-reached", Format::Flag, 4)?;
        mpv.observe_property("idle-active", Format::Flag, 5)?;

        let event_wakeup = wakeup.clone();
        mpv.set_wakeup_callback(move || {
            let _ = event_wakeup.try_send(());
        });
        let render_wakeup = Box::new(wakeup);
        // SAFETY: the boxed sender remains at a stable address until Drop,
        // where the callback is disabled before the box is released.
        unsafe {
            sys::mpv_render_context_set_update_callback(
                render_context,
                Some(render_update),
                (&*render_wakeup as *const Sender<()>).cast_mut().cast(),
            );
        }

        Ok(Self {
            render_context,
            mpv,
            _render_wakeup: render_wakeup,
            last_render_size: None,
            surface: AlignedSurface::new(0),
            playback: PlaybackState::default(),
        })
    }

    pub fn load(&mut self, path: &str) -> Result<()> {
        self.mpv.command("loadfile", &[path, "replace"])?;
        self.last_render_size = None;
        self.playback = PlaybackState::default();
        Ok(())
    }

    pub fn toggle_pause(&self) -> Result<bool> {
        let paused = self.mpv.get_property::<bool>("pause").unwrap_or(false);
        self.mpv.set_property("pause", !paused)?;
        Ok(!paused)
    }

    pub fn seek_relative(&self, seconds: f64) -> Result<()> {
        self.mpv
            .command("seek", &[&seconds.to_string(), "relative+exact"])?;
        Ok(())
    }

    pub fn set_volume(&self, volume: f64) -> Result<()> {
        self.mpv.set_property("volume", volume.clamp(0.0, 100.0))?;
        Ok(())
    }

    pub fn drain_events(&mut self) -> Result<PlaybackState> {
        while let Some(event) = self.mpv.wait_event(0.0) {
            match event? {
                Event::PropertyChange { name, change, .. } => match (name, change) {
                    ("time-pos", PropertyData::Double(value)) => {
                        self.playback.position = value.max(0.0)
                    }
                    ("duration", PropertyData::Double(value)) => {
                        self.playback.duration = value.max(0.0)
                    }
                    ("pause", PropertyData::Flag(value)) => self.playback.paused = value,
                    ("eof-reached", PropertyData::Flag(value)) => {
                        self.playback.finished |= value
                    }
                    ("idle-active", PropertyData::Flag(value)) => {
                        self.playback.finished |= value && self.playback.duration > 0.0
                    }
                    _ => {}
                },
                Event::EndFile(_) => self.playback.finished = true,
                Event::Shutdown => self.playback.finished = true,
                _ => {}
            }
        }
        Ok(self.playback)
    }

    /// Render a pending frame. `None` means libmpv had no new frame and the
    /// existing GPUI image should remain on screen.
    pub fn render_frame(&mut self, width: usize, height: usize) -> Result<Option<VideoFrame>> {
        if width == 0 || height == 0 {
            return Ok(None);
        }

        // SAFETY: calls on a render context stay on GPUI's foreground thread.
        let updates = unsafe { sys::mpv_render_context_update(self.render_context) };
        let frame_pending =
            updates & u64::from(sys::mpv_render_update_flag_MPV_RENDER_UPDATE_FRAME) != 0;
        let size_changed = self.last_render_size != Some((width, height));
        if !frame_pending && !size_changed {
            return Ok(None);
        }

        let stride = width
            .checked_mul(4)
            .ok_or_else(|| anyhow!("video frame width is too large"))?;
        if stride % 64 != 0 {
            bail!("software render width must be a multiple of 16 pixels");
        }

        let byte_len = stride
            .checked_mul(height)
            .ok_or_else(|| anyhow!("video frame is too large"))?;
        self.surface.resize(byte_len);
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
                data: self.surface.as_mut_ptr().cast(),
            },
            sys::mpv_render_param {
                type_: sys::mpv_render_param_type_MPV_RENDER_PARAM_INVALID,
                data: ptr::null_mut(),
            },
        ];

        // SAFETY: all parameter pointers remain valid for the duration of this
        // synchronous call, and the output allocation is 64-byte aligned.
        let code =
            unsafe { sys::mpv_render_context_render(self.render_context, params.as_mut_ptr()) };
        check_mpv(code, "render video frame")?;

        let mut pixels = self.surface.as_bytes().to_vec();
        // bgr0 matches GPUI's BGRA channel order, but mpv leaves the last byte
        // unspecified. GPUI needs it to be fully opaque.
        for alpha in pixels.iter_mut().skip(3).step_by(4) {
            *alpha = 255;
        }

        self.last_render_size = Some((width, height));
        Ok(Some(VideoFrame {
            width: width as u32,
            height: height as u32,
            pixels,
        }))
    }
}

impl Drop for MpvPlayer {
    fn drop(&mut self) {
        if !self.render_context.is_null() {
            // SAFETY: this is the unique render context owned by this value.
            unsafe {
                sys::mpv_render_context_set_update_callback(
                    self.render_context,
                    None,
                    ptr::null_mut(),
                );
                sys::mpv_render_context_free(self.render_context);
            }
            self.render_context = ptr::null_mut();
        }
    }
}

unsafe extern "C" fn render_update(context: *mut c_void) {
    if let Some(wakeup) = unsafe { (context as *const Sender<()>).as_ref() } {
        let _ = wakeup.try_send(());
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct PlaybackState {
    pub position: f64,
    pub duration: f64,
    pub paused: bool,
    pub finished: bool,
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
