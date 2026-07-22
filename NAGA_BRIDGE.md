# Naga shader bridge

The vendored libplacebo 7.360.1 build uses chatt-gui's versioned Rust C ABI as
its only GLSL compiler. The ABI is reentrant, owns no mutable compiler state,
and catches Rust panics before returning to C. libplacebo copies successful
SPIR-V into its own allocator and retains its existing source-keyed Vulkan
shader cache.

The compiler is Naga 29.0.4 from WGPU revision `e99f530`, with only `glsl-in`
and `spv-out` added for this path. Writer semantics are native Vulkan GLSL:
varying labels are retained, while coordinate adjustment, fragment-depth
clamping, bounds-check injection, loop bounding, and workgroup-memory
initialization are disabled. Targets are Vulkan 1.2/SPIR-V 1.5 or Vulkan
1.3+/SPIR-V 1.6. Vulkan descriptor bindings must be explicit.

Bridge revision 2 adds representation-only support for libplacebo's explicit
interface-block member offsets and std140 matrix strides. It preserves the
source operations and emits the native `Offset`, `ColMajor`, and
`MatrixStride` decorations without rewriting matrix expressions.

Set `CHATT_NAGA_SHADER_DUMP_DIR` at runtime to capture each unique generated
source as a hash-named `.vert`, `.frag`, or `.comp` file. Capture failures do
not change compilation behavior.

## Build configuration

The native build uses only the generated pkg-config staging directory. It
contains the six enabled FFmpeg library descriptions, libplacebo, the libva
and nv-codec header/loader shims, and copied-value descriptions for the system
ALSA and Vulkan loaders. An inherited `PKG_CONFIG_PATH` is removed.

Resolved libplacebo options on the 2026-07-22 verification build were:

```text
buildtype=release
default_library=static
auto_features=disabled
prefer_static=true
b_staticpic=true
c_args=-ffunction-sections -fdata-sections
vulkan=enabled
naga=enabled
rust-num-convert=enabled
vk-proc-addr=disabled
opengl=disabled
d3d11=disabled
shaderc=disabled
glslang=disabled
lcms=disabled
dovi=disabled
libdovi=disabled
unwind=disabled
xxhash=disabled
demos=false
tests=false
bench=false
fuzz=false
debug-abort=false
```

The Chatt build supplies libplacebo's locale-independent numeric conversion
symbols from Rust. `zmij` formats floats, `fast-float` parses them, and the
integer paths use allocation-free stack buffers. The normal static archive
link extracts only referenced `libplacebo.a` members, without
`--whole-archive`; no C++ runtime is linked. ALSA and Vulkan remain dynamic.

## Compatibility and verification record

Bridge tests cover all shader stages, SPIR-V 1.5/1.6 selection, combined and
explicit descriptor bindings, compute dimension/invocation/shared-memory
limits, invalid UTF-8 and stages, source-located parse and validation errors,
panic containment, repeated allocation/free, and SPIR-V Tools validation when
`spirv-val` is installed. The corpus under `tests/shaders` covers packed RGB
and BGRA, planar YUV, NV12, P010, 8/10-bit paths, chroma reconstruction,
fragment and compute scaling, SDR, HDR10/HDR10+, HLG, software uploads,
hardware-frame sampling, crop/flip, and channel reordering.

The verification host used an AMD Radeon RX 580 (RADV POLARIS10), Mesa
26.1.4, Vulkan device API 1.4.354, and Vulkan loader 1.4.350. `cargo check`,
`cargo test`, and `cargo build --profile dist` completed successfully. The
dist ELF had no dynamic dependency or versioned import from libplacebo,
libstdc++, shaderc, glslang, SPIR-V Tools, LCMS, libdovi, unwind, or lzma.

For the same non-LTO `release` profile, the previous stripped binary was
48,813,192 bytes and the static-libplacebo/Naga binary was 50,770,704 bytes: a
1,957,512-byte (4.01%) increase. `libplacebo.a` itself was 2,241,550 bytes.
Enabling `glsl-in` increased the same-profile Naga rlib by 5,022,116 bytes;
linked symbols explicitly attributed to `naga::front::glsl` occupied 312,000
bytes before stripping (generic shared code and inlining make this a lower
bound, not a complete attribution).

GUI startup and idle rendering were smoke-tested on that Vulkan device. A
daemon-backed media corpus was not available in the verification environment,
and `VK_LAYER_KHRONOS_validation` was not installed. Attachment interaction,
hardware import, HDR frame comparison against the old compiler, and a
validation-layer media run therefore remain device/session-level release
checks rather than automated results.

On the 2026-07-22 build host and release profile, the numeric C ABI benchmark
parsed a synthetic 65³ RGB LUT in 9.66 ms with libstdc++ `from_chars` and
9.05 ms with Rust `fast-float`. One million double formats took 35.82 ms with
libstdc++ `to_chars` and 22.56 ms with Rust `zmij`. No cross-language LTO was
enabled.
