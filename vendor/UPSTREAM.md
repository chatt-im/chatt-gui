# Vendored media and graphics sources

The following source trees are imported without Git metadata or build output.
FFmpeg and libplacebo's third-party inputs are downloaded during setup. These
are build-oriented snapshots: upstream examples, tests, documentation,
application bundles, and developer tooling that are outside this client's
dependency graph are omitted.

- `mpv`: <https://github.com/mpv-player/mpv>, commit `94335ab` (GPL-2.0-or-later by default; see its license files).
- `libmpv2-rs`: <https://github.com/kohsine/libmpv-rs>, commit `d7ccfaf` (LGPL-2.1; see its license file).
- `ffmpeg`: <https://ffmpeg.org/releases/ffmpeg-8.1.2.tar.xz>, SHA-256 `464beb5e7bf0c311e68b45ae2f04e9cc2af88851abb4082231742a74d97b524c` (LGPL-2.1-or-later in the vendored configuration; see its license files). Run `scripts/fetch-ffmpeg.sh` to install the verified source and tracked VAAPI configure patch at the ignored `vendor/ffmpeg` path.
- `wgpu`: <https://github.com/gfx-rs/wgpu>, commit `e99f530` from the v29 branch (MIT OR Apache-2.0; see its license files).
- `libplacebo`: <https://code.videolan.org/videolan/libplacebo>, tag `v7.360.1`, commit `cee9b076f2c63104ccfd497fa79c39a867293ec4` (LGPL-2.1-or-later; see `libplacebo/LICENSE`). The release archive was verified with Arch Linux's SHA-512 `ea41f3852a5d877313d1969d771b0ba38338906a2a872abc67d2990f50c68848757616dc21cde1dbaa7e0fd46282e455bc7b3b14bfea14079935ff3afe7096e1` (archive SHA-256 `14c0a99f4b01557ec9826ce6b1d52f6de21be274ba03fd5aab7307f18766dc39`).
- `libplacebo/3rdparty/Vulkan-Headers`: <https://github.com/KhronosGroup/Vulkan-Headers>, commit `450bd2232225d6c7728a4108055ac2e37cef6475` (Apache-2.0; archive SHA-256 `26df9841c30806a994e2fdf42f7c87bcb1ced9db9a06033469123939fb3fa075`; see its `LICENSE.md`). Installed by `scripts/fetch-libplacebo-deps.sh`.
- `libplacebo/3rdparty/fast_float`: <https://github.com/fastfloat/fast_float>, commit `97b54ca9e75f5303507699d27c6b4f4efe4641a1` (Apache-2.0 OR Boost-1.0 OR MIT; archive SHA-256 `2b132274539286e41f37857cac22aa8441d21bd86d55de825a3342b149f66801`; see its license files).
- `libplacebo/3rdparty/jinja`: <https://github.com/pallets/jinja>, commit `15206881c006c79667fe5154fe80c01c65410679` (BSD-3-Clause; archive SHA-256 `b88a20dcc2e34072fcf4159325bc6c34cd4b29a81a8b83d15d2f28ba561da296`; see `LICENSE.txt`). Installed by `scripts/fetch-libplacebo-deps.sh`.
- `libplacebo/3rdparty/markupsafe`: <https://github.com/pallets/markupsafe>, commit `297fc8e356e6836a62087949245d09a28e9f1b13` (BSD-3-Clause; archive SHA-256 `da7c010c9c81a66ac73036558c1fcb6212b50482f43211cd1254035b94f82414`; see `LICENSE.txt`). Installed by `scripts/fetch-libplacebo-deps.sh`.
- `hw-headers/libva`: <https://github.com/intel/libva>, tag `2.24.1` (MIT; see `hw-headers/libva/COPYING`).
- `hw-headers/nv-codec`: <https://github.com/FFmpeg/nv-codec-headers>, tag `n12.2.72.0` (MIT; see `hw-headers/nv-codec/README`).

Local changes implement same-device Vulkan rendering from libmpv into WGPU-owned textures. The WGPU fork also enables `VK_EXT_image_drm_format_modifier` when supported so libplacebo can import tiled Linux DMA-BUF hardware-decoder surfaces without a GPU-to-CPU-to-GPU copy. The mpv placebo bridge uses a bounded reusable host-memory staging ring for software/copy-decoded planes; this avoids per-plane AUTO-memory PBOs selecting uncached host-visible VRAM on discrete GPUs. Its custom Vulkan teardown mirrors mpv's native Vulkan context ownership and does not call `ra_free` on the self-freeing placebo RA. The source directories, rather than generated patch files, are the maintained forks.

The mpv fork also guards its `ass/ass.h` include with `HAVE_LIBASS`, matching
the already-guarded version-property implementation and keeping the configured
libass-disabled build independent of host libass headers.

The libplacebo fork adds a versioned C ABI to Naga 29.0.4 from the pinned WGPU
tree. It enables only Naga's `glsl-in` and `spv-out` compiler features and
keeps libplacebo's `pl_spirv` allocation, source hashing, and cache flow. The
SPIR-V writer uses native GLSL semantics: no coordinate adjustment, fragment
depth clamp, injected bounds checks, forced loop bounding, or implicit
workgroup initialization. A narrow Naga frontend/backend change preserves
Vulkan GLSL combined image-sampler descriptors emitted by libplacebo as one
SPIR-V descriptor; the checked-in compatibility corpus covers the regression.
The cache compiler identity includes Naga 29.0.4, WGPU revision `e99f530`, the
two enabled compiler features, bridge revision 2, writer semantics, and the
target Vulkan/SPIR-V versions.

libplacebo is built static and PIC with Vulkan enabled. OpenGL, D3D11,
shaderc, glslang, LCMS, Dolby Vision reshaping/libdovi, unwind, xxHash, demos,
tests, benchmarks, fuzzers, and debug-abort are disabled. Dolby Vision is
therefore unsupported in this client. SDR, HDR10/HDR10+, and HLG metadata and
tone-mapping paths remain available without the Dolby Vision reshaper.

The FFmpeg build is a static decode-only profile for common attachment formats
and the H.264/HEVC live-share bridge. Keeping it local prevents the binary from
inheriting every codec and hardware integration enabled by the build host's
distribution FFmpeg package. Chatt-specific hardware discovery remains in the
libmpv loader. The two header-only VAAPI configure probes are maintained as a
small patch under `patches/`; the downloaded FFmpeg source tree is not tracked.

The libva and nv-codec imports are build-time headers only. CUDA/NVDEC is
loaded at runtime by FFmpeg/mpv. VAAPI calls go through chatt-gui's small lazy
loader, which opens the system `libva.so.2` and `libva-drm.so.2` only when the
VAAPI path is probed. Those optional driver-facing libraries are not bundled
and are not startup dependencies; failure to load them leaves software decode
available. The application uses `VK_EXT_physical_device_drm` to pass libmpv an
fd for the render node belonging to WGPU's selected Vulkan device, avoiding a
libdrm dependency and device-order guesses on hybrid systems.
