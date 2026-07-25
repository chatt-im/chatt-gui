#pragma once

#include <stdlib.h>
#include <stdio.h>
#include <stdint.h>
#include <stdbool.h>
#include <assert.h>

#include "config.h"

#include "common/common.h"
#include "common/msg.h"

// We need to define all platforms we want to support. Since we have
// our own mechanism for checking this, we re-define the right symbols
#if HAVE_WAYLAND
#define VK_USE_PLATFORM_WAYLAND_KHR
#endif
#if HAVE_X11
#define VK_USE_PLATFORM_XLIB_KHR
#endif
#if HAVE_WIN32_DESKTOP
#define VK_USE_PLATFORM_WIN32_KHR
#endif
#if HAVE_COCOA
#define VK_USE_PLATFORM_METAL_EXT
#endif

#include <libplacebo/vulkan.h>
#include "mpv/render_vk.h"

// Shared struct used to hold vulkan context information
struct mpvk_ctx {
    pl_log pllog;
    pl_vk_inst vkinst;
    pl_vulkan vulkan;
    pl_gpu gpu; // points to vulkan->gpu for convenience
    pl_swapchain swapchain;
    VkSurfaceKHR surface;

    // Application-owned Vulkan imports do not have a pl_vk_inst. Keep the
    // corresponding data here for FFmpeg hardware contexts.
    PFN_vkGetInstanceProcAddr get_proc_addr;
    char **instance_extensions;
    int num_instance_extensions;
    char **device_extensions;
    int num_device_extensions;
    struct mpv_vulkan_queue_family *enabled_queue_families;
    int num_enabled_queue_families;
    mpv_vulkan_lock_queue_fn lock_queue;
    mpv_vulkan_lock_queue_fn unlock_queue;
    void *queue_ctx;
};
