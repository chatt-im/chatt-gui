# Vendored media and graphics sources

The following source trees were imported without Git metadata or build output:

- `mpv`: <https://github.com/mpv-player/mpv>, commit `94335ab` (GPL-2.0-or-later by default; see its license files).
- `libmpv2-rs`: <https://github.com/kohsine/libmpv-rs>, commit `d7ccfaf` (LGPL-2.1; see its license file).
- `wgpu`: <https://github.com/gfx-rs/wgpu>, commit `e99f530` from the v29 branch (MIT OR Apache-2.0; see its license files).

Local changes implement same-device Vulkan rendering from libmpv into WGPU-owned textures. The WGPU fork also enables `VK_EXT_image_drm_format_modifier` when supported so libplacebo can import tiled Linux DMA-BUF hardware-decoder surfaces without a GPU-to-CPU-to-GPU copy. The mpv placebo bridge uses a bounded reusable host-memory staging ring for software/copy-decoded planes; this avoids per-plane AUTO-memory PBOs selecting uncached host-visible VRAM on discrete GPUs. Its custom Vulkan teardown mirrors mpv's native Vulkan context ownership and does not call `ra_free` on the self-freeing placebo RA. The source directories, rather than generated patch files, are the maintained forks.
