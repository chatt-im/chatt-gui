/*
 * This file is part of chatt-gui's vendored libplacebo integration and is
 * distributed under the same LGPL-2.1-or-later terms as libplacebo.
 */

#include <limits.h>
#include <stdint.h>
#include <string.h>

#include "hash.h"
#include "naga_bridge.h"
#include "spirv.h"
#include "utils.h"

#define CHATT_NAGA_COMPILER_ID                                                \
    "naga-29.0.4-wgpu-e99f530-glsl-in+spv-out-bridge-r2-"                    \
    "writer=label-varyings,unchecked-bounds,no-coordinate-adjust,"           \
    "no-frag-depth-clamp,no-loop-bounding,no-workgroup-init"

const struct spirv_compiler pl_spirv_naga;

static void naga_destroy(pl_spirv spirv)
{
    pl_free((void *) spirv);
}

static pl_spirv naga_create(pl_log log, struct pl_spirv_version spirv_ver)
{
    if (spirv_ver.spv_version != PL_SPV_VERSION(1, 5) &&
        spirv_ver.spv_version != PL_SPV_VERSION(1, 6))
    {
        pl_fatal(log, "Naga only supports this build's SPIR-V 1.5/1.6 targets");
        return NULL;
    }

    struct pl_spirv_t *spirv = pl_alloc_ptr(NULL, spirv);
    if (!spirv)
        return NULL;

    *spirv = (struct pl_spirv_t) {
        .signature = pl_str0_hash(CHATT_NAGA_COMPILER_ID),
        .impl      = &pl_spirv_naga,
        .version   = spirv_ver,
        .log       = log,
    };
    pl_hash_merge(&spirv->signature, (uint64_t) spirv_ver.spv_version << 32 |
                                                spirv_ver.env_version);
    PL_INFO(spirv, "Naga 29.0.4 (WGPU e99f530, glsl-in + spv-out, bridge r2)");
    return spirv;
}

static pl_str naga_compile(pl_spirv spirv, void *alloc,
                           struct pl_glsl_version glsl_ver,
                           enum glsl_shader_stage stage,
                           const char *shader)
{
    static const uint8_t entry_point[] = "main";
    struct chatt_naga_request_v1 request = {
        .abi_version = CHATT_NAGA_ABI_VERSION,
        .struct_size = sizeof(request),
        .stage = stage,
        .glsl_version = glsl_ver.version,
        .vulkan_version = spirv->version.env_version,
        .spirv_version = spirv->version.spv_version,
        .entry_point = entry_point,
        .entry_point_len = sizeof(entry_point) - 1,
        .source = (const uint8_t *) shader,
        .source_len = strlen(shader),
        .max_compute_shared_memory_size = glsl_ver.max_shmem_size,
        .max_compute_workgroup_invocations = glsl_ver.max_group_threads,
        .max_compute_workgroup_size = {
            glsl_ver.max_group_size[0],
            glsl_ver.max_group_size[1],
            glsl_ver.max_group_size[2],
        },
    };
    struct chatt_naga_result_v1 result = {
        .abi_version = CHATT_NAGA_ABI_VERSION,
        .struct_size = sizeof(result),
    };

    if (!chatt_naga_compile_v1(&request, &result)) {
        if (result.diagnostic && result.diagnostic_len) {
            int len = result.diagnostic_len > INT_MAX
                    ? INT_MAX : (int) result.diagnostic_len;
            PL_ERR(spirv, "Naga compilation failed:\n%.*s", len,
                   (const char *) result.diagnostic);
        } else {
            PL_ERR(spirv, "Naga compilation failed without a diagnostic");
        }
        chatt_naga_result_free_v1(&result);
        return (pl_str) {0};
    }

    pl_str output = {0};
    if (result.abi_version != CHATT_NAGA_ABI_VERSION ||
        result.struct_size < sizeof(result) ||
        result.word_count > SIZE_MAX / sizeof(uint32_t))
    {
        PL_ERR(spirv, "Naga returned an invalid bridge result");
    } else {
        output.len = result.word_count * sizeof(uint32_t);
        output.buf = pl_memdup(alloc, result.words, output.len);
    }
    chatt_naga_result_free_v1(&result);
    return output;
}

const struct spirv_compiler pl_spirv_naga = {
    .name       = "naga",
    .destroy    = naga_destroy,
    .create     = naga_create,
    .compile    = naga_compile,
};
