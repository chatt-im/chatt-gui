/* Copyright (C) 2026 chatt-gui contributors
 *
 * This file is part of mpv.
 *
 * mpv is free software; you can redistribute it and/or modify it under the
 * terms of the GNU Lesser General Public License as published by the Free
 * Software Foundation; either version 2.1 of the License, or (at your option)
 * any later version.
 */
#ifndef MPV_CLIENT_API_RENDER_VK_H_
#define MPV_CLIENT_API_RENDER_VK_H_

#include <stdint.h>
#include <vulkan/vulkan.h>

#include "render.h"

#ifdef __cplusplus
extern "C" {
#endif

/** A queue family and the number of queues actually enabled on the device. */
typedef struct mpv_vulkan_queue_family {
    uint32_t index;
    uint32_t count;
} mpv_vulkan_queue_family;

typedef void (*mpv_vulkan_lock_queue_fn)(void *ctx, uint32_t family,
                                         uint32_t index);

/**
 * Parameters for importing an application-owned Vulkan device.
 *
 * Extension and feature arrays only need to remain valid until
 * mpv_render_context_create() returns. The Vulkan handles, callback functions,
 * and queue_ctx must remain valid until the render context has been freed.
 */
typedef struct mpv_vulkan_init_params {
    VkInstance instance;
    VkPhysicalDevice physical_device;
    VkDevice device;
    PFN_vkGetInstanceProcAddr get_proc_address;

    const char *const *instance_extensions;
    int num_instance_extensions;
    const char *const *device_extensions;
    int num_device_extensions;
    const VkPhysicalDeviceFeatures2 *features;

    mpv_vulkan_queue_family graphics_queue;
    mpv_vulkan_queue_family compute_queue;
    mpv_vulkan_queue_family transfer_queue;

    /** Exact list of all queue families enabled when VkDevice was created. */
    const mpv_vulkan_queue_family *enabled_queue_families;
    int num_enabled_queue_families;

    mpv_vulkan_lock_queue_fn lock_queue;
    mpv_vulkan_lock_queue_fn unlock_queue;
    void *queue_ctx;

    /**
     * DRM render-node fd for the imported physical device, or -1 if unknown.
     * mpv duplicates this fd before mpv_render_context_create() returns. A
     * missing or unusable fd only disables DRM-backed hardware interop.
     */
    int drm_render_fd;
} mpv_vulkan_init_params;

/**
 * An application-owned Vulkan image used as a render target.
 *
 * The image is imported on first use and remains registered until it is passed
 * to MPV_RENDER_PARAM_VULKAN_TARGET_REMOVE or the render context is freed.
 * Immutable image fields must not change while registered. The image and both
 * semaphores must remain alive for that period.
 *
 * Before rendering, mpv waits for wait_semaphore/wait_value and assumes the
 * image is in input_layout. After rendering, mpv transitions to output_layout
 * and signals signal_semaphore/signal_value. Timeline semaphores are required.
 */
typedef struct mpv_vulkan_target {
    VkImage image;
    VkFormat format;
    VkImageUsageFlags usage;
    int width;
    int height;
    VkImageLayout input_layout;
    VkImageLayout output_layout;
    VkSemaphore wait_semaphore;
    uint64_t wait_value;
    VkSemaphore signal_semaphore;
    uint64_t signal_value;
} mpv_vulkan_target;

#ifdef __cplusplus
}
#endif

#endif
