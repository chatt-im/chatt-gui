/*
 * Versioned C ABI implemented by chatt-gui's Rust Naga bridge.
 *
 * This file is part of chatt-gui's vendored libplacebo integration and is
 * distributed under the same LGPL-2.1-or-later terms as libplacebo.
 */

#pragma once

#include <stddef.h>
#include <stdint.h>

#define CHATT_NAGA_ABI_VERSION 1u

struct chatt_naga_request_v1 {
    uint32_t abi_version;
    uint32_t struct_size;
    uint32_t stage;
    uint32_t glsl_version;
    uint32_t vulkan_version;
    uint32_t spirv_version;
    const uint8_t *entry_point;
    size_t entry_point_len;
    const uint8_t *source;
    size_t source_len;
    uint64_t max_compute_shared_memory_size;
    uint32_t max_compute_workgroup_invocations;
    uint32_t max_compute_workgroup_size[3];
    uint32_t reserved[4];
};

struct chatt_naga_result_v1 {
    uint32_t abi_version;
    uint32_t struct_size;
    uint32_t *words;
    size_t word_count;
    uint8_t *diagnostic;
    size_t diagnostic_len;
};

int32_t chatt_naga_compile_v1(const struct chatt_naga_request_v1 *request,
                              struct chatt_naga_result_v1 *result);
void chatt_naga_result_free_v1(struct chatt_naga_result_v1 *result);
