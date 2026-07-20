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
