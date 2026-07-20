# Vendored media and graphics sources

The following source trees were imported without Git metadata or build output:

- `mpv`: <https://github.com/mpv-player/mpv>, commit `94335ab` (GPL-2.0-or-later by default; see its license files).
- `libmpv2-rs`: <https://github.com/kohsine/libmpv-rs>, commit `d7ccfaf` (LGPL-2.1; see its license file).
- `ffmpeg`: <https://ffmpeg.org/releases/ffmpeg-8.1.2.tar.xz> (LGPL-2.1-or-later in the vendored configuration; see its license files).
- `wgpu`: <https://github.com/gfx-rs/wgpu>, commit `e99f530` from the v29 branch (MIT OR Apache-2.0; see its license files).
- `hw-headers/libva`: <https://github.com/intel/libva>, tag `2.24.1` (MIT; see `hw-headers/libva/COPYING`).
- `hw-headers/nv-codec`: <https://github.com/FFmpeg/nv-codec-headers>, tag `n12.2.72.0` (MIT; see `hw-headers/nv-codec/README`).

Local changes implement same-device Vulkan rendering from libmpv into WGPU-owned textures. The WGPU fork also enables `VK_EXT_image_drm_format_modifier` when supported so libplacebo can import tiled Linux DMA-BUF hardware-decoder surfaces without a GPU-to-CPU-to-GPU copy. The mpv placebo bridge uses a bounded reusable host-memory staging ring for software/copy-decoded planes; this avoids per-plane AUTO-memory PBOs selecting uncached host-visible VRAM on discrete GPUs. Its custom Vulkan teardown mirrors mpv's native Vulkan context ownership and does not call `ra_free` on the self-freeing placebo RA. The source directories, rather than generated patch files, are the maintained forks.

The FFmpeg build is a static decode-only profile for common attachment formats
and the H.264/HEVC live-share bridge. Keeping it local prevents the binary from
inheriting every codec and hardware integration enabled by the build host's
distribution FFmpeg package.

The libva and nv-codec imports are build-time headers only. CUDA/NVDEC is
loaded at runtime by FFmpeg/mpv. VAAPI calls go through chatt-gui's small lazy
loader, which opens the system `libva.so.2` and `libva-drm.so.2` only when the
VAAPI path is probed. Those optional driver-facing libraries are not bundled
and are not startup dependencies; failure to load them leaves software decode
available. The application uses `VK_EXT_physical_device_drm` to pass libmpv an
fd for the render node belonging to WGPU's selected Vulkan device, avoiding a
libdrm dependency and device-order guesses on hybrid systems.
