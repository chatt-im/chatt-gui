#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]

include!(concat!(env!("OUT_DIR"), "/bindings.rs"));

#[inline]
/// Returns the associated error string.
pub fn mpv_error_str(e: mpv_error) -> &'static str {
    let raw = unsafe { mpv_error_string(e) };
    unsafe { ::std::ffi::CStr::from_ptr(raw) }.to_str().unwrap()
}

#[cfg(feature = "vendored")]
unsafe extern "C" {
    fn chatt_vaapi_runtime_available() -> ::std::ffi::c_int;
}

/// Whether the optional system VAAPI runtime was loaded successfully.
#[cfg(feature = "vendored")]
pub fn vaapi_runtime_available() -> bool {
    unsafe { chatt_vaapi_runtime_available() != 0 }
}

#[cfg(feature = "vendored")]
pub type ChattFfmpegReadFn = unsafe extern "C" fn(
    opaque: *mut ::std::ffi::c_void,
    buffer: *mut u8,
    length: ::std::ffi::c_int,
) -> ::std::ffi::c_int;

#[cfg(feature = "vendored")]
pub type ChattFfmpegSeekFn = unsafe extern "C" fn(
    opaque: *mut ::std::ffi::c_void,
    offset: i64,
    whence: ::std::ffi::c_int,
) -> i64;

#[cfg(feature = "vendored")]
pub type ChattFfmpegInterruptFn =
    unsafe extern "C" fn(opaque: *mut ::std::ffi::c_void) -> ::std::ffi::c_int;

#[cfg(feature = "vendored")]
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct ChattFfmpegThumbnail {
    /// Scaled size before `rotate` is applied.
    pub width: ::std::ffi::c_int,
    pub height: ::std::ffi::c_int,
    /// Clockwise degrees the image must be rotated at display time: 0, 90, 180, or 270.
    pub rotate: ::std::ffi::c_int,
    pub reserved: ::std::ffi::c_int,
    pub duration: f64,
}

#[cfg(feature = "vendored")]
unsafe extern "C" {
    pub fn chatt_ffmpeg_extract_first_frame(
        opaque: *mut ::std::ffi::c_void,
        byte_len: i64,
        read: ChattFfmpegReadFn,
        seek: ChattFfmpegSeekFn,
        interrupt: ChattFfmpegInterruptFn,
        maximum_width: ::std::ffi::c_int,
        maximum_height: ::std::ffi::c_int,
        maximum_pixels: i64,
        probesize: i64,
        maximum_analyze_duration: i64,
        bgra: *mut u8,
        bgra_capacity: usize,
        thumbnail: *mut ChattFfmpegThumbnail,
        error: *mut ::std::ffi::c_char,
        error_capacity: usize,
    ) -> ::std::ffi::c_int;
}
