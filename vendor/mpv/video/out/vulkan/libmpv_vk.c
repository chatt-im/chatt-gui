/*
 * Same-device Vulkan render API for application-owned images.
 *
 * This file is part of mpv and is licensed under LGPL-2.1-or-later.
 */

#include "config.h"

#include "common/common.h"
#include "mpv/render_vk.h"
#include "video/out/gpu/libmpv_gpu.h"
#include "video/out/gpu/context.h"
#include "video/out/gpu/ra.h"
#include "video/out/placebo/ra_pl.h"
#include "video/out/placebo/utils.h"
#include "video/out/vulkan/common.h"

struct vk_target {
    mpv_vulkan_target desc;
    pl_tex texture;
    struct ra_tex ra_texture;
    bool held;
};

struct priv {
    struct ra_ctx *ra_ctx;
    struct mpvk_ctx vk;
    struct vk_target *targets;
    int num_targets;
    struct vk_target *active;
};

static bool valid_queue(struct mpv_vulkan_queue_family queue)
{
    return queue.count > 0;
}

static char **copy_extensions(void *ctx, const char *const *extensions, int count)
{
    if (count <= 0)
        return NULL;
    char **copy = talloc_array(ctx, char *, count);
    for (int i = 0; i < count; i++)
        copy[i] = talloc_strdup(copy, extensions[i]);
    return copy;
}

static int init(struct libmpv_gpu_context *ctx, mpv_render_param *params)
{
    mpv_vulkan_init_params *init_params =
        get_mpv_render_param(params, MPV_RENDER_PARAM_VULKAN_INIT_PARAMS, NULL);
    if (!init_params || !init_params->instance || !init_params->physical_device ||
        !init_params->device || !init_params->features ||
        !valid_queue(init_params->graphics_queue) ||
        (!!init_params->lock_queue != !!init_params->unlock_queue) ||
        init_params->num_instance_extensions < 0 ||
        init_params->num_device_extensions < 0 ||
        init_params->num_enabled_queue_families < 0) {
        MP_ERR(ctx, "Invalid application Vulkan import parameters.\n");
        return MPV_ERROR_INVALID_PARAMETER;
    }

    ctx->priv = talloc_zero(NULL, struct priv);
    struct priv *p = ctx->priv;
    p->vk.pllog = mppl_log_create(p, ctx->log);
    if (!p->vk.pllog)
        return MPV_ERROR_NOMEM;

    struct pl_vulkan_import_params import = {
        .instance = init_params->instance,
        .get_proc_addr = init_params->get_proc_address,
        .phys_device = init_params->physical_device,
        .device = init_params->device,
        .extensions = init_params->device_extensions,
        .num_extensions = init_params->num_device_extensions,
        .queue_graphics = {
            .index = init_params->graphics_queue.index,
            .count = init_params->graphics_queue.count,
        },
        .features = init_params->features,
        .lock_queue = init_params->lock_queue,
        .unlock_queue = init_params->unlock_queue,
        .queue_ctx = init_params->queue_ctx,
    };
    if (valid_queue(init_params->compute_queue)) {
        import.queue_compute = (struct pl_vulkan_queue) {
            .index = init_params->compute_queue.index,
            .count = init_params->compute_queue.count,
        };
    }
    if (valid_queue(init_params->transfer_queue)) {
        import.queue_transfer = (struct pl_vulkan_queue) {
            .index = init_params->transfer_queue.index,
            .count = init_params->transfer_queue.count,
        };
    }

    p->vk.vulkan = pl_vulkan_import(p->vk.pllog, &import);
    if (!p->vk.vulkan) {
        MP_ERR(ctx, "Failed importing application Vulkan device.\n");
        return MPV_ERROR_UNSUPPORTED;
    }
    p->vk.gpu = p->vk.vulkan->gpu;
    p->vk.get_proc_addr = init_params->get_proc_address;
    p->vk.instance_extensions = copy_extensions(
        p, init_params->instance_extensions, init_params->num_instance_extensions);
    p->vk.num_instance_extensions = init_params->num_instance_extensions;
    p->vk.enabled_queue_families = talloc_memdup(
        p, (void *)init_params->enabled_queue_families,
        init_params->num_enabled_queue_families *
            sizeof(*init_params->enabled_queue_families));
    p->vk.num_enabled_queue_families = init_params->num_enabled_queue_families;
    p->vk.lock_queue = init_params->lock_queue;
    p->vk.unlock_queue = init_params->unlock_queue;
    p->vk.queue_ctx = init_params->queue_ctx;

    p->ra_ctx = talloc_zero(p, struct ra_ctx);
    p->ra_ctx->log = ctx->log;
    p->ra_ctx->global = ctx->global;
    p->ra_ctx->opts.allow_sw = true;
    p->ra_ctx->ra = ra_create_pl(p->vk.gpu, ctx->log);
    if (!p->ra_ctx->ra)
        return MPV_ERROR_UNSUPPORTED;
    ra_add_native_resource(p->ra_ctx->ra, "mpvk_ctx", &p->vk);
    ctx->ra_ctx = p->ra_ctx;
    MP_VERBOSE(ctx, "Imported application Vulkan device with %d device extensions.\n",
               init_params->num_device_extensions);
    return 0;
}

static struct vk_target *find_target(struct priv *p, VkImage image)
{
    for (int i = 0; i < p->num_targets; i++) {
        if (p->targets[i].desc.image == image)
            return &p->targets[i];
    }
    return NULL;
}

static bool same_image(const mpv_vulkan_target *a, const mpv_vulkan_target *b)
{
    return a->image == b->image && a->format == b->format &&
           a->usage == b->usage && a->width == b->width &&
           a->height == b->height;
}

static struct vk_target *create_target(struct libmpv_gpu_context *ctx,
                                       const mpv_vulkan_target *desc)
{
    struct priv *p = ctx->priv;
    pl_tex texture = pl_vulkan_wrap(p->vk.gpu, pl_vulkan_wrap_params(
        .image = desc->image,
        .width = desc->width,
        .height = desc->height,
        .format = desc->format,
        .usage = desc->usage,
    ));
    if (!texture) {
        MP_ERR(ctx, "Failed wrapping application Vulkan image.\n");
        return NULL;
    }

    struct vk_target target = {
        .desc = *desc,
        .texture = texture,
        .held = true,
    };
    if (!mppl_wrap_tex(p->ra_ctx->ra, texture, &target.ra_texture)) {
        MP_ERR(ctx, "Failed exposing wrapped Vulkan image to the renderer.\n");
        pl_tex_destroy(p->vk.gpu, &texture);
        return NULL;
    }
    MP_TARRAY_APPEND(p, p->targets, p->num_targets, target);
    MP_VERBOSE(ctx, "Registered application Vulkan target %p (%dx%d).\n",
               (void *)desc->image, desc->width, desc->height);
    return &p->targets[p->num_targets - 1];
}

static int wrap_fbo(struct libmpv_gpu_context *ctx, mpv_render_param *params,
                    struct ra_tex **out)
{
    struct priv *p = ctx->priv;
    mpv_vulkan_target *desc =
        get_mpv_render_param(params, MPV_RENDER_PARAM_VULKAN_TARGET, NULL);
    if (!desc || !desc->image || !desc->wait_semaphore ||
        !desc->signal_semaphore || desc->width <= 0 || desc->height <= 0) {
        MP_ERR(ctx, "Invalid Vulkan render target or synchronization parameters.\n");
        return MPV_ERROR_INVALID_PARAMETER;
    }

    struct vk_target *target = find_target(p, desc->image);
    if (!target) {
        target = create_target(ctx, desc);
        if (!target)
            return MPV_ERROR_UNSUPPORTED;
    } else if (!same_image(&target->desc, desc)) {
        MP_ERR(ctx, "Vulkan target immutable fields changed while registered.\n");
        return MPV_ERROR_INVALID_PARAMETER;
    }

    if (p->active) {
        if (p->active != target) {
            MP_ERR(ctx, "Render attempted to switch Vulkan targets mid-frame.\n");
            return MPV_ERROR_INVALID_PARAMETER;
        }
        *out = &target->ra_texture;
        return 0;
    }
    if (!target->held) {
        MP_ERR(ctx, "Vulkan target reused before ownership returned to the application.\n");
        return MPV_ERROR_INVALID_PARAMETER;
    }

    target->desc = *desc;
    pl_vulkan_release_ex(p->vk.gpu, pl_vulkan_release_params(
        .tex = target->texture,
        .layout = desc->input_layout,
        .qf = VK_QUEUE_FAMILY_IGNORED,
        .semaphore = {
            .sem = desc->wait_semaphore,
            .value = desc->wait_value,
        },
    ));
    target->held = false;
    p->active = target;
    *out = &target->ra_texture;
    return 0;
}

static int done_frame(struct libmpv_gpu_context *ctx, bool display_synced)
{
    struct priv *p = ctx->priv;
    struct vk_target *target = p->active;
    if (!target) {
        MP_ERR(ctx, "Vulkan frame completed without an active render target.\n");
        return MPV_ERROR_INVALID_PARAMETER;
    }

    bool ok = pl_vulkan_hold_ex(p->vk.gpu, pl_vulkan_hold_params(
        .tex = target->texture,
        .layout = target->desc.output_layout,
        .qf = VK_QUEUE_FAMILY_IGNORED,
        .semaphore = {
            .sem = target->desc.signal_semaphore,
            .value = target->desc.signal_value,
        },
    ));
    target->held = ok;
    p->active = NULL;
    if (!ok)
        MP_ERR(ctx, "Failed returning Vulkan target ownership to the application.\n");
    return ok ? 0 : MPV_ERROR_GENERIC;
}

static int set_parameter(struct libmpv_gpu_context *ctx, mpv_render_param param)
{
    if (param.type != MPV_RENDER_PARAM_VULKAN_TARGET_REMOVE)
        return MPV_ERROR_NOT_IMPLEMENTED;
    if (!param.data)
        return MPV_ERROR_INVALID_PARAMETER;

    struct priv *p = ctx->priv;
    VkImage image = *(VkImage *)param.data;
    struct vk_target *target = find_target(p, image);
    if (!target)
        return 0;
    if (target == p->active || !target->held) {
        MP_ERR(ctx, "Cannot remove a Vulkan target while rendering or externally owned.\n");
        return MPV_ERROR_INVALID_PARAMETER;
    }

    int index = target - p->targets;
    pl_tex_destroy(p->vk.gpu, &target->texture);
    MP_TARRAY_REMOVE_AT(p->targets, p->num_targets, index);
    MP_VERBOSE(ctx, "Removed application Vulkan target %p.\n", (void *)image);
    return 0;
}

static void destroy(struct libmpv_gpu_context *ctx)
{
    struct priv *p = ctx->priv;
    if (!p)
        return;
    MP_VERBOSE(ctx, "Destroying application Vulkan renderer with %d registered targets.\n",
               p->num_targets);
    if (p->vk.gpu)
        pl_gpu_finish(p->vk.gpu);
    for (int i = 0; i < p->num_targets; i++)
        pl_tex_destroy(p->vk.gpu, &p->targets[i].texture);
    // ra_create_pl() returns an RA whose destroy callback frees the RA itself.
    // Calling ra_free() would invoke that callback and then free the same
    // allocation a second time. Mirror ra_vk_ctx_uninit() instead.
    if (p->ra_ctx && p->ra_ctx->ra) {
        p->ra_ctx->ra->fns->destroy(p->ra_ctx->ra);
        p->ra_ctx->ra = NULL;
    }
    if (p->vk.vulkan)
        pl_vulkan_destroy(&p->vk.vulkan);
}

const struct libmpv_gpu_context_fns libmpv_gpu_context_vk = {
    .api_name = MPV_RENDER_API_TYPE_VULKAN,
    .init = init,
    .wrap_fbo = wrap_fbo,
    .set_parameter = set_parameter,
    .done_frame = done_frame,
    .destroy = destroy,
};
