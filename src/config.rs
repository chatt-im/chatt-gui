//! Renderer-owned configuration.
//!
//! [`schema`] is the single file-facing TOML model. Filesystem policy remains
//! separate so parsing and path selection can be tested without GPUI.

pub(crate) mod io;
pub(crate) mod paths;
pub(crate) mod schema;
pub(crate) mod validation;
