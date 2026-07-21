//! C ABI used by the vendored libplacebo GLSL compiler backend.
//!
//! libplacebo deliberately remains responsible for its own shader-cache and
//! allocation lifetime. The allocations returned here live only long enough
//! for `spirv_naga.c` to copy them into libplacebo-owned memory.

use std::{
    any::Any,
    borrow::Cow,
    ffi::OsString,
    mem,
    panic::{self, AssertUnwindSafe},
    path::PathBuf,
    ptr, slice,
};

use naga::{
    AddressSpace, ShaderStage,
    back::spv,
    front::glsl,
    proc::{BoundsCheckPolicies, GlobalCtx, Layouter},
    valid::{ValidationFlags, Validator},
};

pub const CHATT_NAGA_ABI_VERSION: u32 = 1;

const STAGE_VERTEX: u32 = 0;
const STAGE_FRAGMENT: u32 = 1;
const STAGE_COMPUTE: u32 = 2;
const SPIRV_1_5: u32 = 0x0001_0500;
const SPIRV_1_6: u32 = 0x0001_0600;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct ChattNagaRequestV1 {
    pub abi_version: u32,
    pub struct_size: u32,
    pub stage: u32,
    pub glsl_version: u32,
    pub vulkan_version: u32,
    pub spirv_version: u32,
    pub entry_point: *const u8,
    pub entry_point_len: usize,
    pub source: *const u8,
    pub source_len: usize,
    pub max_compute_shared_memory_size: u64,
    pub max_compute_workgroup_invocations: u32,
    pub max_compute_workgroup_size: [u32; 3],
    pub reserved: [u32; 4],
}

#[repr(C)]
pub struct ChattNagaResultV1 {
    pub abi_version: u32,
    pub struct_size: u32,
    pub words: *mut u32,
    pub word_count: usize,
    pub diagnostic: *mut u8,
    pub diagnostic_len: usize,
}

impl ChattNagaResultV1 {
    fn empty() -> Self {
        Self {
            abi_version: CHATT_NAGA_ABI_VERSION,
            struct_size: mem::size_of::<Self>() as u32,
            words: ptr::null_mut(),
            word_count: 0,
            diagnostic: ptr::null_mut(),
            diagnostic_len: 0,
        }
    }

    fn success(words: Vec<u32>) -> Self {
        let words = words.into_boxed_slice();
        let word_count = words.len();
        let words = Box::into_raw(words).cast::<u32>();
        Self {
            words,
            word_count,
            ..Self::empty()
        }
    }

    fn failure(message: String) -> Self {
        let diagnostic = message.into_bytes().into_boxed_slice();
        let diagnostic_len = diagnostic.len();
        let diagnostic = Box::into_raw(diagnostic).cast::<u8>();
        Self {
            diagnostic,
            diagnostic_len,
            ..Self::empty()
        }
    }
}

struct CompileRequest<'a> {
    stage: ShaderStage,
    stage_name: &'static str,
    entry_point: &'a str,
    source: &'a str,
    vulkan_version: u32,
    spirv_version: u32,
    max_compute_shared_memory_size: u64,
    max_compute_workgroup_invocations: u32,
    max_compute_workgroup_size: [u32; 3],
}

/// Compile one GLSL shader. Returns one for success and zero for failure.
///
/// Every failure that occurs after `result` is validated is returned as an
/// owned UTF-8 diagnostic in `result`. No unwind is permitted to cross this
/// boundary.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn chatt_naga_compile_v1(
    request: *const ChattNagaRequestV1,
    result: *mut ChattNagaResultV1,
) -> i32 {
    if result.is_null() {
        return 0;
    }

    let compiled = catch_boundary(|| {
        // SAFETY: The caller promises `request` addresses a C ABI request.
        // Field validation occurs before any pointed-to byte ranges are read.
        unsafe { compile_abi_request(request) }
    });
    let success = compiled.is_ok();
    let value = match compiled {
        Ok(words) => ChattNagaResultV1::success(words),
        Err(message) => ChattNagaResultV1::failure(message),
    };

    // SAFETY: `result` was checked for null and the C ABI requires it to point
    // to writable storage for a complete V1 result.
    unsafe { result.write(value) };
    i32::from(success)
}

/// Release allocations returned by [`chatt_naga_compile_v1`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn chatt_naga_result_free_v1(result: *mut ChattNagaResultV1) {
    if result.is_null() {
        return;
    }

    // SAFETY: The C ABI requires a result previously initialized by this
    // module. Each pointer is consumed at most once because the result is
    // cleared before returning.
    let result = unsafe { &mut *result };
    if !result.words.is_null() {
        let words = ptr::slice_from_raw_parts_mut(result.words, result.word_count);
        // SAFETY: Successful results originate in `Box<[u32]>` above.
        unsafe { drop(Box::from_raw(words)) };
    }
    if !result.diagnostic.is_null() {
        let diagnostic = ptr::slice_from_raw_parts_mut(result.diagnostic, result.diagnostic_len);
        // SAFETY: Failure results originate in `Box<[u8]>` above.
        unsafe { drop(Box::from_raw(diagnostic)) };
    }
    *result = ChattNagaResultV1::empty();
}

fn catch_boundary<F>(compile: F) -> Result<Vec<u32>, String>
where
    F: FnOnce() -> Result<Vec<u32>, String>,
{
    match panic::catch_unwind(AssertUnwindSafe(compile)) {
        Ok(result) => result,
        Err(payload) => Err(format!(
            "unknown shader: Naga bridge panic: {}",
            panic_message(payload.as_ref())
        )),
    }
}

fn panic_message(payload: &(dyn Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("non-string panic payload")
}

unsafe fn compile_abi_request(request: *const ChattNagaRequestV1) -> Result<Vec<u32>, String> {
    if request.is_null() {
        return Err("unknown shader: null Naga bridge request".into());
    }

    // SAFETY: Null was rejected. Reading the fixed V1 header is part of the
    // versioned ABI contract and precedes reading the rest of the request.
    let abi_version = unsafe { ptr::addr_of!((*request).abi_version).read() };
    // SAFETY: Same fixed-header contract as above.
    let struct_size = unsafe { ptr::addr_of!((*request).struct_size).read() };
    if abi_version != CHATT_NAGA_ABI_VERSION {
        return Err(format!(
            "unknown shader: unsupported Naga bridge ABI version {abi_version}"
        ));
    }
    if struct_size < mem::size_of::<ChattNagaRequestV1>() as u32 {
        return Err(format!(
            "unknown shader: Naga bridge request is too small ({struct_size} bytes)"
        ));
    }

    // SAFETY: The header reports a complete V1 request.
    let request = unsafe { request.read() };
    let (stage, stage_name) = stage_from_abi(request.stage)?;
    let prefix = |message: &str| format!("{stage_name} shader: {message}");

    if request.glsl_version != 450 {
        return Err(prefix(&format!(
            "Naga bridge requires GLSL 450, got {}",
            request.glsl_version
        )));
    }
    validate_target_versions(stage_name, request.vulkan_version, request.spirv_version)?;

    // SAFETY: The ABI requires non-null pointers for non-empty byte ranges.
    let entry_point = unsafe {
        utf8_field(
            stage_name,
            "entry point",
            request.entry_point,
            request.entry_point_len,
        )?
    };
    if entry_point.is_empty() {
        return Err(prefix("entry point must not be empty"));
    }
    // SAFETY: The ABI requires non-null pointers for non-empty byte ranges.
    let source = unsafe {
        utf8_field(
            stage_name,
            "GLSL source",
            request.source,
            request.source_len,
        )?
    };
    if source.is_empty() {
        return Err(prefix("GLSL source must not be empty"));
    }

    let request = CompileRequest {
        stage,
        stage_name,
        entry_point,
        source,
        vulkan_version: request.vulkan_version,
        spirv_version: request.spirv_version,
        max_compute_shared_memory_size: request.max_compute_shared_memory_size,
        max_compute_workgroup_invocations: request.max_compute_workgroup_invocations,
        max_compute_workgroup_size: request.max_compute_workgroup_size,
    };
    compile_glsl(request)
}

unsafe fn utf8_field<'a>(
    stage_name: &str,
    field_name: &str,
    bytes: *const u8,
    len: usize,
) -> Result<&'a str, String> {
    if len > isize::MAX as usize {
        return Err(format!("{stage_name} shader: {field_name} is too large"));
    }
    if len != 0 && bytes.is_null() {
        return Err(format!("{stage_name} shader: {field_name} pointer is null"));
    }
    let bytes = if len == 0 {
        &[]
    } else {
        // SAFETY: The caller owns a readable range of `len` bytes by the ABI
        // contract; the null and maximum-length cases were rejected above.
        unsafe { slice::from_raw_parts(bytes, len) }
    };
    std::str::from_utf8(bytes)
        .map_err(|error| format!("{stage_name} shader: {field_name} is not UTF-8: {error}"))
}

fn stage_from_abi(stage: u32) -> Result<(ShaderStage, &'static str), String> {
    match stage {
        STAGE_VERTEX => Ok((ShaderStage::Vertex, "vertex")),
        STAGE_FRAGMENT => Ok((ShaderStage::Fragment, "fragment")),
        STAGE_COMPUTE => Ok((ShaderStage::Compute, "compute")),
        _ => Err(format!(
            "invalid shader stage {stage}: Naga compilation rejected"
        )),
    }
}

fn validate_target_versions(
    stage_name: &str,
    vulkan_version: u32,
    spirv_version: u32,
) -> Result<(), String> {
    let vulkan_major = (vulkan_version >> 22) & 0x7f;
    let vulkan_minor = (vulkan_version >> 12) & 0x3ff;
    if vulkan_major != 1 || !(2..=4).contains(&vulkan_minor) {
        return Err(format!(
            "{stage_name} shader: unsupported Vulkan target {vulkan_major}.{vulkan_minor}"
        ));
    }

    match spirv_version {
        SPIRV_1_5 => Ok(()),
        SPIRV_1_6 if vulkan_minor >= 3 => Ok(()),
        SPIRV_1_6 => Err(format!(
            "{stage_name} shader: SPIR-V 1.6 requires Vulkan 1.3 or newer"
        )),
        _ => Err(format!(
            "{stage_name} shader: unsupported SPIR-V target {}.{}",
            spirv_version >> 16,
            (spirv_version >> 8) & 0xff
        )),
    }
}

fn compile_glsl(request: CompileRequest<'_>) -> Result<Vec<u32>, String> {
    dump_shader_if_requested(&request);

    let parse_source = promote_vulkan_glsl_version(request.source);
    let mut frontend = glsl::Frontend::default();
    let options = glsl::Options::from(request.stage);
    let module = frontend.parse(&options, &parse_source).map_err(|error| {
        format!(
            "{} shader: Naga parse failed:\n{}",
            request.stage_name,
            error.emit_to_string_with_path(request.source, "libplacebo.glsl")
        )
    })?;

    if frontend.metadata().version != 450 {
        return Err(format!(
            "{} shader: Naga parsed GLSL {}, expected GLSL 450",
            request.stage_name,
            frontend.metadata().version
        ));
    }

    enforce_compute_limits(&module, &request)?;

    let mut validator = Validator::new(ValidationFlags::all(), spv::supported_capabilities());
    validator.allow_glsl_scalar_atomics(true);
    validator.allow_glsl_write_only_storage_buffers(true);
    let info = validator.validate(&module).map_err(|error| {
        format!(
            "{} shader: Naga validation failed:\n{}",
            request.stage_name,
            error.emit_to_string_with_path(request.source, "libplacebo.glsl")
        )
    })?;

    let lang_version = match request.spirv_version {
        SPIRV_1_5 => (1, 5),
        SPIRV_1_6 => (1, 6),
        _ => unreachable!("target version was validated before compilation"),
    };
    let binding_map = module
        .global_variables
        .iter()
        .filter_map(|(_, variable)| variable.binding)
        .map(|binding| {
            (
                binding,
                spv::BindingInfo {
                    descriptor_set: binding.group,
                    binding: binding.binding,
                    binding_array_size: None,
                },
            )
        })
        .collect();
    let writer_options = spv::Options {
        lang_version,
        flags: spv::WriterFlags::LABEL_VARYINGS,
        fake_missing_bindings: false,
        binding_map,
        capabilities: None,
        bounds_check_policies: BoundsCheckPolicies::default(),
        zero_initialize_workgroup_memory: spv::ZeroInitializeWorkgroupMemoryMode::None,
        force_loop_bounding: false,
        ray_query_initialization_tracking: false,
        use_storage_input_output_16: true,
        debug_info: None,
        task_dispatch_limits: None,
        mesh_shader_primitive_indices_clamp: false,
    };
    let pipeline_options = spv::PipelineOptions {
        shader_stage: request.stage,
        entry_point: request.entry_point.into(),
    };
    spv::write_vec(&module, &info, &writer_options, Some(&pipeline_options)).map_err(|error| {
        format!(
            "{} shader: Naga SPIR-V emission failed for Vulkan {}.{} / SPIR-V {}.{}: {error}",
            request.stage_name,
            (request.vulkan_version >> 22) & 0x7f,
            (request.vulkan_version >> 12) & 0x3ff,
            request.spirv_version >> 16,
            (request.spirv_version >> 8) & 0xff,
        )
    })
}

fn promote_vulkan_glsl_version(source: &str) -> Cow<'_, str> {
    let first_line_end = source.find('\n').unwrap_or(source.len());
    let first_line = &source[..first_line_end];
    let Some(hash) = first_line.find('#') else {
        return Cow::Borrowed(source);
    };
    if !first_line[..hash].chars().all(char::is_whitespace) {
        return Cow::Borrowed(source);
    }

    let after_hash = &first_line[hash + 1..];
    let directive_start = after_hash.len() - after_hash.trim_start().len();
    let directive = &after_hash[directive_start..];
    let Some(after_version) = directive.strip_prefix("version") else {
        return Cow::Borrowed(source);
    };
    if after_version
        .chars()
        .next()
        .is_some_and(|ch| !ch.is_whitespace())
    {
        return Cow::Borrowed(source);
    }

    let digits_ws = after_version.len() - after_version.trim_start().len();
    let digits = after_version[digits_ws..]
        .chars()
        .take_while(char::is_ascii_digit)
        .count();
    if digits != 3 {
        return Cow::Borrowed(source);
    }
    let digits_start = hash + 1 + directive_start + "version".len() + digits_ws;
    let version = &source[digits_start..digits_start + digits];
    if !matches!(version, "410" | "420" | "430" | "440") {
        return Cow::Borrowed(source);
    }

    let mut promoted = source.to_owned();
    promoted.replace_range(digits_start..digits_start + digits, "450");
    Cow::Owned(promoted)
}

fn enforce_compute_limits(
    module: &naga::Module,
    request: &CompileRequest<'_>,
) -> Result<(), String> {
    if request.stage != ShaderStage::Compute {
        return Ok(());
    }

    let entry_point = module
        .entry_points
        .iter()
        .find(|entry| entry.stage == request.stage && entry.name == request.entry_point)
        .ok_or_else(|| {
            format!(
                "{} shader: entry point {:?} was not found",
                request.stage_name, request.entry_point
            )
        })?;
    for (dimension, (&actual, &limit)) in entry_point
        .workgroup_size
        .iter()
        .zip(request.max_compute_workgroup_size.iter())
        .enumerate()
    {
        if actual > limit {
            return Err(format!(
                "compute shader: workgroup dimension {dimension} is {actual}, exceeding limit {limit}"
            ));
        }
    }
    let invocations = entry_point
        .workgroup_size
        .iter()
        .try_fold(1_u32, |product, value| product.checked_mul(*value))
        .ok_or_else(|| "compute shader: workgroup invocation count overflowed".to_string())?;
    if invocations > request.max_compute_workgroup_invocations {
        return Err(format!(
            "compute shader: workgroup has {invocations} invocations, exceeding limit {}",
            request.max_compute_workgroup_invocations
        ));
    }

    let mut layouter = Layouter::default();
    layouter
        .update(GlobalCtx {
            types: &module.types,
            constants: &module.constants,
            overrides: &module.overrides,
            global_expressions: &module.global_expressions,
        })
        .map_err(|error| format!("compute shader: shared-memory layout failed: {error}"))?;
    let shared_bytes = module
        .global_variables
        .iter()
        .filter(|(_, variable)| variable.space == AddressSpace::WorkGroup)
        .try_fold(0_u64, |sum, (_, variable)| {
            sum.checked_add(u64::from(layouter[variable.ty].size))
        })
        .ok_or_else(|| "compute shader: shared-memory size overflowed".to_string())?;
    if shared_bytes > request.max_compute_shared_memory_size {
        return Err(format!(
            "compute shader: uses {shared_bytes} shared-memory bytes, exceeding limit {}",
            request.max_compute_shared_memory_size
        ));
    }
    Ok(())
}

fn dump_shader_if_requested(request: &CompileRequest<'_>) {
    let Some(directory) = std::env::var_os("CHATT_NAGA_SHADER_DUMP_DIR") else {
        return;
    };
    let directory = PathBuf::from(directory);
    if std::fs::create_dir_all(&directory).is_err() {
        return;
    }

    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in request.source.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let extension = match request.stage {
        ShaderStage::Vertex => "vert",
        ShaderStage::Fragment => "frag",
        ShaderStage::Compute => "comp",
        _ => "glsl",
    };
    let mut filename = OsString::from(format!("{hash:016x}."));
    filename.push(extension);
    let _ = std::fs::write(directory.join(filename), request.source);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::process::Command;

    const VULKAN_1_2: u32 = (1 << 22) | (2 << 12);
    const VULKAN_1_3: u32 = (1 << 22) | (3 << 12);
    const VERTEX: &str = "#version 450\nlayout(location=0) in vec2 position;\nvoid main() { gl_Position = vec4(position, 0.0, 1.0); }\n";
    const FRAGMENT: &str = "#version 450\nlayout(set=2, binding=3) uniform sampler2D image;\nlayout(location=0) out vec4 color;\nvoid main() { color = texture(image, vec2(0.5)); }\n";
    const COMPUTE: &str = "#version 450\nlayout(local_size_x=8, local_size_y=4, local_size_z=2) in;\nlayout(set=0, binding=0, std430) buffer Data { uint values[]; } data;\nvoid main() { data.values[gl_GlobalInvocationID.x] = 1u; }\n";

    fn request(stage: u32, source: &str, spirv_version: u32) -> ChattNagaRequestV1 {
        ChattNagaRequestV1 {
            abi_version: CHATT_NAGA_ABI_VERSION,
            struct_size: mem::size_of::<ChattNagaRequestV1>() as u32,
            stage,
            glsl_version: 450,
            vulkan_version: if spirv_version == SPIRV_1_6 {
                VULKAN_1_3
            } else {
                VULKAN_1_2
            },
            spirv_version,
            entry_point: b"main".as_ptr(),
            entry_point_len: 4,
            source: source.as_ptr(),
            source_len: source.len(),
            max_compute_shared_memory_size: 32 * 1024,
            max_compute_workgroup_invocations: 1024,
            max_compute_workgroup_size: [1024, 1024, 64],
            reserved: [0; 4],
        }
    }

    fn compile(request: &ChattNagaRequestV1) -> Result<Vec<u32>, String> {
        let mut result = ChattNagaResultV1::empty();
        // SAFETY: Both pointers remain valid for the duration of the call.
        let success = unsafe { chatt_naga_compile_v1(request, &mut result) };
        let output = if success == 1 {
            // SAFETY: A successful bridge result owns `word_count` words.
            Ok(unsafe { slice::from_raw_parts(result.words, result.word_count) }.to_vec())
        } else {
            // SAFETY: A failed bridge result owns `diagnostic_len` bytes.
            let bytes = unsafe { slice::from_raw_parts(result.diagnostic, result.diagnostic_len) };
            Err(String::from_utf8(bytes.to_vec()).expect("diagnostic is UTF-8"))
        };
        // SAFETY: `result` was initialized by the bridge and is freed once.
        unsafe { chatt_naga_result_free_v1(&mut result) };
        output
    }

    fn instructions(words: &[u32]) -> impl Iterator<Item = (u32, &[u32])> {
        let mut offset = 5;
        std::iter::from_fn(move || {
            if offset >= words.len() {
                return None;
            }
            let instruction_len = (words[offset] >> 16) as usize;
            assert!(instruction_len > 0);
            let opcode = words[offset] & 0xffff;
            let operands = &words[offset + 1..offset + instruction_len];
            offset += instruction_len;
            Some((opcode, operands))
        })
    }

    fn contains_opcode(words: &[u32], opcode: u32) -> bool {
        instructions(words).any(|(candidate, _)| candidate == opcode)
    }

    #[test]
    fn compiles_vertex_fragment_and_compute_shaders() {
        for (stage, source) in [
            (STAGE_VERTEX, VERTEX),
            (STAGE_FRAGMENT, FRAGMENT),
            (STAGE_COMPUTE, COMPUTE),
        ] {
            let words = compile(&request(stage, source, SPIRV_1_5)).unwrap();
            assert_eq!(words[0], 0x0723_0203);
        }
    }

    #[test]
    fn promotes_older_libplacebo_raster_shaders_for_vulkan() {
        let vertex = "#version 410\nlayout(location=0) in vec2 vertex_pos;\nlayout(location=1) in vec3 vertex_color;\nlayout(location=0) out vec3 frag_color;\nvoid main() { gl_Position = vec4(vertex_pos, 0, 1); frag_color = vertex_color; }\n";
        let fragment = "#version 410\nlayout(location=0) in vec3 frag_color;\nlayout(location=0) out vec4 out_color;\nvoid main() { out_color = vec4(frag_color, 1.0); }\n";

        assert!(compile(&request(STAGE_VERTEX, vertex, SPIRV_1_5)).is_ok());
        assert!(compile(&request(STAGE_FRAGMENT, fragment, SPIRV_1_5)).is_ok());
        assert!(matches!(
            promote_vulkan_glsl_version(VERTEX),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn emits_requested_spirv_target_versions() {
        for version in [SPIRV_1_5, SPIRV_1_6] {
            let words = compile(&request(STAGE_VERTEX, VERTEX, version)).unwrap();
            assert_eq!(words[1], version);
        }
    }

    #[test]
    fn preserves_explicit_descriptor_set_and_binding() {
        let words = compile(&request(STAGE_FRAGMENT, FRAGMENT, SPIRV_1_5)).unwrap();
        let mut descriptor_set = false;
        let mut binding = false;
        let mut offset = 5;
        while offset < words.len() {
            let instruction_len = (words[offset] >> 16) as usize;
            let opcode = words[offset] & 0xffff;
            assert!(instruction_len > 0);
            if opcode == 71 && instruction_len >= 4 {
                descriptor_set |= words[offset + 2] == 34 && words[offset + 3] == 2;
                binding |= words[offset + 2] == 33 && words[offset + 3] == 3;
            }
            offset += instruction_len;
        }
        assert!(descriptor_set);
        assert!(binding);
    }

    #[test]
    fn emitted_combined_sampler_spirv_passes_spirv_val_when_available() {
        if Command::new("spirv-val").arg("--version").output().is_err() {
            return;
        }

        let directory = tempfile::tempdir().unwrap();
        for (name, stage, source) in [
            ("combined-sampler", STAGE_FRAGMENT, FRAGMENT),
            (
                "libplacebo-runtime",
                STAGE_FRAGMENT,
                include_str!("../tests/shaders/libplacebo_runtime.frag"),
            ),
            (
                "polar-gather",
                STAGE_FRAGMENT,
                include_str!("../tests/shaders/polar_gather.frag"),
            ),
            (
                "peak-detect",
                STAGE_COMPUTE,
                include_str!("../tests/shaders/peak_detect.comp"),
            ),
            (
                "texel-buffer",
                STAGE_FRAGMENT,
                include_str!("../tests/shaders/texel_buffer.frag"),
            ),
            (
                "storage-texel-buffer",
                STAGE_COMPUTE,
                include_str!("../tests/shaders/storage_texel_buffer.comp"),
            ),
            (
                "write-only-storage-buffer",
                STAGE_COMPUTE,
                include_str!("../tests/shaders/storage_buffer_writeonly.comp"),
            ),
            (
                "formatless-storage-texel-buffer",
                STAGE_COMPUTE,
                include_str!("../tests/shaders/formatless_storage_texel_buffer.comp"),
            ),
            (
                "h274-vector-modulo",
                STAGE_COMPUTE,
                include_str!("../tests/shaders/h274_vector_modulo.comp"),
            ),
            (
                "error-diffusion-specialized-shared",
                STAGE_COMPUTE,
                include_str!("../tests/shaders/error_diffusion_specialized_shared.comp"),
            ),
            (
                "compute-implicit-texture",
                STAGE_COMPUTE,
                include_str!("../tests/shaders/compute_implicit_texture.comp"),
            ),
            (
                "coherent-storage",
                STAGE_FRAGMENT,
                include_str!("../tests/shaders/coherent_storage.frag"),
            ),
        ] {
            let words = compile(&request(stage, source, SPIRV_1_5)).unwrap();
            let bytes: Vec<u8> = words.iter().flat_map(|word| word.to_le_bytes()).collect();
            let path = directory.path().join(format!("{name}.spv"));
            std::fs::write(&path, bytes).unwrap();
            let output = Command::new("spirv-val")
                .arg("--target-env")
                .arg("vulkan1.2")
                .arg(path)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "spirv-val rejected {name}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    #[test]
    fn preserves_libplacebo_block_offsets_and_std140_matrix_stride() {
        let source = include_str!("../tests/shaders/libplacebo_runtime.frag");
        let words = compile(&request(STAGE_FRAGMENT, source, SPIRV_1_5)).unwrap();
        let mut binding_two = None;
        let mut variable_types = HashMap::new();
        let mut pointer_types = HashMap::new();
        let mut struct_members = HashMap::new();
        let mut member_decorations = Vec::new();

        for (opcode, operands) in instructions(&words) {
            match opcode {
                71 if operands.len() >= 3 && operands[1] == 33 && operands[2] == 2 => {
                    binding_two = Some(operands[0]);
                }
                59 if operands.len() >= 3 => {
                    variable_types.insert(operands[1], operands[0]);
                }
                32 if operands.len() >= 3 => {
                    pointer_types.insert(operands[0], operands[2]);
                }
                30 if !operands.is_empty() => {
                    struct_members.insert(operands[0], operands[1..].to_vec());
                }
                72 if operands.len() >= 4 => member_decorations.push(operands.to_vec()),
                _ => {}
            }
        }

        let variable = binding_two.expect("fixture UBO binding");
        let pointer = variable_types[&variable];
        let wrapper = pointer_types[&pointer];
        let descriptor_struct = struct_members
            .get(&wrapper)
            .and_then(|members| members.first())
            .copied()
            .unwrap_or(wrapper);

        let mut member_offsets = Vec::new();
        let mut matrix_strides = Vec::new();
        for decoration in member_decorations {
            if decoration[0] == descriptor_struct {
                match decoration[2] {
                    7 => matrix_strides.push(decoration[3]),
                    35 => member_offsets.push(decoration[3]),
                    _ => {}
                }
            }
        }

        member_offsets.sort_unstable();
        assert_eq!(member_offsets, [0, 48, 80]);
        assert_eq!(matrix_strides.len(), 3);
        assert!(matrix_strides.iter().all(|stride| *stride == 16));
    }

    #[test]
    fn emits_native_operations_for_libplacebo_accelerated_paths() {
        let gather = compile(&request(
            STAGE_FRAGMENT,
            include_str!("../tests/shaders/polar_gather.frag"),
            SPIRV_1_5,
        ))
        .unwrap();
        assert!(contains_opcode(&gather, 96), "missing OpImageGather");

        let peak = compile(&request(
            STAGE_COMPUTE,
            include_str!("../tests/shaders/peak_detect.comp"),
            SPIRV_1_5,
        ))
        .unwrap();
        assert!(contains_opcode(&peak, 349), "missing OpGroupNonUniformIAdd");
        assert!(
            contains_opcode(&peak, 333),
            "missing OpGroupNonUniformElect"
        );
        assert!(
            contains_opcode(&peak, 336),
            "missing OpGroupNonUniformAllEqual"
        );
        assert!(
            contains_opcode(&peak, 339),
            "missing OpGroupNonUniformBallot"
        );
        assert!(
            contains_opcode(&peak, 342),
            "missing OpGroupNonUniformBallotBitCount"
        );
        assert!(
            !contains_opcode(&peak, 338),
            "subgroupAllEqual was expanded through OpGroupNonUniformBroadcastFirst"
        );
        assert!(contains_opcode(&peak, 234), "missing OpAtomicIAdd");

        let texel_buffer = compile(&request(
            STAGE_FRAGMENT,
            include_str!("../tests/shaders/texel_buffer.frag"),
            SPIRV_1_5,
        ))
        .unwrap();
        assert!(contains_opcode(&texel_buffer, 95), "missing OpImageFetch");
        assert!(
            instructions(&texel_buffer).any(|(opcode, operands)| {
                opcode == 25 && operands.len() >= 3 && operands[2] == 5
            }),
            "missing Buffer-dimensional OpTypeImage"
        );
        assert!(
            instructions(&texel_buffer)
                .any(|(opcode, operands)| { opcode == 17 && operands.first() == Some(&46) }),
            "missing SampledBuffer capability"
        );

        let storage_texel_buffer = compile(&request(
            STAGE_COMPUTE,
            include_str!("../tests/shaders/storage_texel_buffer.comp"),
            SPIRV_1_5,
        ))
        .unwrap();
        assert!(
            contains_opcode(&storage_texel_buffer, 98),
            "missing OpImageRead"
        );
        assert!(
            contains_opcode(&storage_texel_buffer, 99),
            "missing OpImageWrite"
        );
        assert!(
            instructions(&storage_texel_buffer).any(|(opcode, operands)| {
                opcode == 25 && operands.len() >= 7 && operands[2] == 5 && operands[6] == 2
            }),
            "missing storage Buffer-dimensional OpTypeImage"
        );
        assert!(
            instructions(&storage_texel_buffer)
                .any(|(opcode, operands)| { opcode == 17 && operands.first() == Some(&47) }),
            "missing ImageBuffer capability"
        );

        let write_only_storage = compile(&request(
            STAGE_COMPUTE,
            include_str!("../tests/shaders/storage_buffer_writeonly.comp"),
            SPIRV_1_5,
        ))
        .unwrap();
        assert!(
            contains_opcode(&write_only_storage, 50),
            "missing OpSpecConstant"
        );
        assert!(
            instructions(&write_only_storage).any(|(opcode, operands)| {
                opcode == 71 && operands.get(1) == Some(&1) && operands.get(2) == Some(&0)
            }),
            "missing SpecId 0 decoration"
        );
        assert!(
            instructions(&write_only_storage).any(|(opcode, operands)| {
                opcode == 71 && operands.get(1) == Some(&1) && operands.get(2) == Some(&1)
            }),
            "missing SpecId 1 decoration"
        );
        assert!(
            instructions(&write_only_storage)
                .any(|(opcode, operands)| { opcode == 71 && operands.get(1) == Some(&25) }),
            "missing NonReadable decoration for write-only storage buffer"
        );

        let formatless_storage = compile(&request(
            STAGE_COMPUTE,
            include_str!("../tests/shaders/formatless_storage_texel_buffer.comp"),
            SPIRV_1_5,
        ))
        .unwrap();
        assert!(
            contains_opcode(&formatless_storage, 98),
            "missing OpImageRead"
        );
        assert!(
            contains_opcode(&formatless_storage, 99),
            "missing OpImageWrite"
        );
        assert!(
            instructions(&formatless_storage).any(|(opcode, operands)| {
                opcode == 25
                    && operands.len() >= 8
                    && operands[2] == 5
                    && operands[6] == 2
                    && operands[7] == 0
            }),
            "missing unknown-format storage Buffer OpTypeImage"
        );
        assert!(
            instructions(&formatless_storage)
                .any(|(opcode, operands)| { opcode == 17 && operands.first() == Some(&55) }),
            "missing StorageImageReadWithoutFormat capability"
        );

        let specialized_shared = compile(&request(
            STAGE_COMPUTE,
            include_str!("../tests/shaders/error_diffusion_specialized_shared.comp"),
            SPIRV_1_5,
        ))
        .unwrap();
        let spec_constant = instructions(&specialized_shared)
            .find_map(|(opcode, operands)| (opcode == 52).then(|| operands[1]))
            .expect("missing derived specialized shared-array length");
        assert!(
            instructions(&specialized_shared).any(|(opcode, operands)| {
                opcode == 28 && operands.get(2) == Some(&spec_constant)
            }),
            "OpTypeArray does not use the specialization constant length"
        );

        let compute_texture = compile(&request(
            STAGE_COMPUTE,
            include_str!("../tests/shaders/compute_implicit_texture.comp"),
            SPIRV_1_5,
        ))
        .unwrap();
        assert!(
            contains_opcode(&compute_texture, 88),
            "missing explicit-LOD sample"
        );
        assert!(
            !contains_opcode(&compute_texture, 87),
            "compute texture() used implicit LOD"
        );

        let coherent_storage = compile(&request(
            STAGE_FRAGMENT,
            include_str!("../tests/shaders/coherent_storage.frag"),
            SPIRV_1_5,
        ))
        .unwrap();
        assert_eq!(
            instructions(&coherent_storage)
                .filter(|(opcode, operands)| { *opcode == 71 && operands.get(1) == Some(&23) })
                .count(),
            2,
            "missing Coherent decorations for storage image and buffer"
        );
    }

    #[test]
    fn glsl_spirv_validation_relaxations_are_bridge_scoped() {
        let source = "#version 450\nlayout(local_size_x=1) in;\nshared uint value;\nvoid main() { atomicAdd(value, 1u); }\n";
        let mut frontend = glsl::Frontend::default();
        let module = frontend
            .parse(&glsl::Options::from(ShaderStage::Compute), source)
            .unwrap();

        let mut portable = Validator::new(ValidationFlags::all(), spv::supported_capabilities());
        assert!(portable.validate(&module).is_err());

        let mut glsl_spirv = Validator::new(ValidationFlags::all(), spv::supported_capabilities());
        glsl_spirv.allow_glsl_scalar_atomics(true);
        assert!(glsl_spirv.validate(&module).is_ok());

        let source = include_str!("../tests/shaders/storage_buffer_writeonly.comp");
        let mut frontend = glsl::Frontend::default();
        let module = frontend
            .parse(&glsl::Options::from(ShaderStage::Compute), source)
            .unwrap();

        let mut portable = Validator::new(ValidationFlags::all(), spv::supported_capabilities());
        assert!(portable.validate(&module).is_err());

        let mut glsl_spirv = Validator::new(ValidationFlags::all(), spv::supported_capabilities());
        glsl_spirv.allow_glsl_write_only_storage_buffers(true);
        assert!(glsl_spirv.validate(&module).is_ok());
    }

    #[test]
    fn enforces_compute_workgroup_limits() {
        let mut limited = request(STAGE_COMPUTE, COMPUTE, SPIRV_1_5);
        limited.max_compute_workgroup_size[0] = 4;
        let diagnostic = compile(&limited).unwrap_err();
        assert!(diagnostic.contains("dimension 0 is 8, exceeding limit 4"));

        limited.max_compute_workgroup_size[0] = 8;
        limited.max_compute_workgroup_invocations = 32;
        let diagnostic = compile(&limited).unwrap_err();
        assert!(diagnostic.contains("64 invocations, exceeding limit 32"));

        let shared = "#version 450\nlayout(local_size_x=1) in;\nshared uint values[16];\nvoid main() { values[0] = 1u; }\n";
        let mut limited = request(STAGE_COMPUTE, shared, SPIRV_1_5);
        limited.max_compute_shared_memory_size = 32;
        let diagnostic = compile(&limited).unwrap_err();
        assert!(diagnostic.contains("64 shared-memory bytes, exceeding limit 32"));
    }

    #[test]
    fn rejects_invalid_utf8_and_shader_stages() {
        let invalid = [0xff];
        let mut bad_utf8 = request(STAGE_VERTEX, VERTEX, SPIRV_1_5);
        bad_utf8.source = invalid.as_ptr();
        bad_utf8.source_len = invalid.len();
        assert!(compile(&bad_utf8).unwrap_err().contains("vertex shader"));
        assert!(compile(&bad_utf8).unwrap_err().contains("not UTF-8"));

        let invalid_stage = request(99, VERTEX, SPIRV_1_5);
        assert!(
            compile(&invalid_stage)
                .unwrap_err()
                .contains("invalid shader stage 99")
        );
    }

    #[test]
    fn parse_diagnostics_include_stage_and_source_location() {
        let invalid = "#version 450\nvoid main() {\n    this is not GLSL;\n}\n";
        let diagnostic = compile(&request(STAGE_FRAGMENT, invalid, SPIRV_1_5)).unwrap_err();
        assert!(diagnostic.contains("fragment shader: Naga parse failed"));
        assert!(diagnostic.contains("libplacebo.glsl:3"));
    }

    #[test]
    fn validation_diagnostics_include_stage_and_source_location() {
        let invalid = "#version 450\nlayout(set=0, binding=0) uniform texture2D image;\nlayout(set=0, binding=0) uniform sampler image_sampler;\nlayout(location=0) out vec4 color;\nvoid main() { color = texture(sampler2D(image, image_sampler), vec2(0.5)); }\n";
        let diagnostic = compile(&request(STAGE_FRAGMENT, invalid, SPIRV_1_5)).unwrap_err();
        assert!(
            diagnostic.contains("fragment shader: Naga validation failed"),
            "{diagnostic}"
        );
        assert!(diagnostic.contains("libplacebo.glsl:3"), "{diagnostic}");
    }

    #[test]
    fn contains_panics_at_the_boundary() {
        let diagnostic = catch_boundary(|| -> Result<Vec<u32>, String> {
            panic!("intentional bridge test panic")
        })
        .unwrap_err();
        assert!(diagnostic.contains("Naga bridge panic"));
        assert!(diagnostic.contains("intentional bridge test panic"));
    }

    #[test]
    fn repeatedly_allocates_and_frees_results() {
        let request = request(STAGE_VERTEX, VERTEX, SPIRV_1_5);
        for _ in 0..128 {
            let mut result = ChattNagaResultV1::empty();
            // SAFETY: Both pointers remain valid for the call.
            assert_eq!(unsafe { chatt_naga_compile_v1(&request, &mut result) }, 1);
            assert!(!result.words.is_null());
            // SAFETY: The result is initialized by the bridge and freed once.
            unsafe { chatt_naga_result_free_v1(&mut result) };
            assert!(result.words.is_null());
            assert!(result.diagnostic.is_null());
        }
    }

    #[test]
    fn compatibility_corpus_parses_validates_and_emits_spirv() {
        let fixtures = [
            (
                STAGE_FRAGMENT,
                include_str!("../tests/shaders/packed_rgb.frag"),
            ),
            (
                STAGE_FRAGMENT,
                include_str!("../tests/shaders/packed_bgra.frag"),
            ),
            (
                STAGE_FRAGMENT,
                include_str!("../tests/shaders/planar_yuv.frag"),
            ),
            (
                STAGE_FRAGMENT,
                include_str!("../tests/shaders/nv12_8bit.frag"),
            ),
            (
                STAGE_FRAGMENT,
                include_str!("../tests/shaders/p010_10bit.frag"),
            ),
            (
                STAGE_FRAGMENT,
                include_str!("../tests/shaders/chroma_reconstruction.frag"),
            ),
            (
                STAGE_FRAGMENT,
                include_str!("../tests/shaders/fragment_scaling.frag"),
            ),
            (
                STAGE_COMPUTE,
                include_str!("../tests/shaders/compute_scaling.comp"),
            ),
            (
                STAGE_FRAGMENT,
                include_str!("../tests/shaders/sdr_tone_map.frag"),
            ),
            (
                STAGE_FRAGMENT,
                include_str!("../tests/shaders/hdr10_tone_map.frag"),
            ),
            (
                STAGE_FRAGMENT,
                include_str!("../tests/shaders/hlg_tone_map.frag"),
            ),
            (
                STAGE_FRAGMENT,
                include_str!("../tests/shaders/software_upload.frag"),
            ),
            (
                STAGE_FRAGMENT,
                include_str!("../tests/shaders/hardware_frame.frag"),
            ),
            (
                STAGE_FRAGMENT,
                include_str!("../tests/shaders/libplacebo_runtime.frag"),
            ),
            (
                STAGE_FRAGMENT,
                include_str!("../tests/shaders/crop_flip.frag"),
            ),
            (
                STAGE_FRAGMENT,
                include_str!("../tests/shaders/channel_reorder.frag"),
            ),
            (
                STAGE_FRAGMENT,
                include_str!("../tests/shaders/polar_gather.frag"),
            ),
            (
                STAGE_COMPUTE,
                include_str!("../tests/shaders/peak_detect.comp"),
            ),
            (
                STAGE_FRAGMENT,
                include_str!("../tests/shaders/texel_buffer.frag"),
            ),
            (
                STAGE_COMPUTE,
                include_str!("../tests/shaders/storage_texel_buffer.comp"),
            ),
        ];

        for (stage, source) in fixtures {
            let words = compile(&request(stage, source, SPIRV_1_5)).unwrap();
            assert_eq!(words[0], 0x0723_0203);
        }
    }
}
