#define CHATT_VAAPI_LOADER_IMPL
#include "vaapi_loader.h"

#include <dlfcn.h>
#include <pthread.h>
#include <stdlib.h>
#include <string.h>

static pthread_once_t load_once = PTHREAD_ONCE_INIT;
static struct chatt_vaapi_functions functions;
static VADisplay (*get_display_drm)(int fd);
static void *libva;
static void *libva_drm;
static int available;

static int disabled(void)
{
    const char *value = getenv("CHATT_DISABLE_VAAPI");
    return value && value[0] && strcmp(value, "0") != 0;
}

static void load_vaapi(void)
{
    if (disabled())
        return;

    libva = dlopen("libva.so.2", RTLD_NOW | RTLD_LOCAL);
    if (!libva)
        return;
    libva_drm = dlopen("libva-drm.so.2", RTLD_NOW | RTLD_LOCAL);
    if (!libva_drm)
        goto fail;

#define LOAD_CORE(name) do {                                                   \
        *(void **)(&functions.p_##name) = dlsym(libva, #name);                 \
        if (!functions.p_##name)                                                \
            goto fail;                                                          \
    } while (0)
    LOAD_CORE(vaAcquireBufferHandle);
    LOAD_CORE(vaBeginPicture);
    LOAD_CORE(vaCreateBuffer);
    LOAD_CORE(vaCreateConfig);
    LOAD_CORE(vaCreateContext);
    LOAD_CORE(vaCreateImage);
    LOAD_CORE(vaCreateSurfaces);
    LOAD_CORE(vaDeriveImage);
    LOAD_CORE(vaDestroyBuffer);
    LOAD_CORE(vaDestroyConfig);
    LOAD_CORE(vaDestroyContext);
    LOAD_CORE(vaDestroyImage);
    LOAD_CORE(vaDestroySurfaces);
    LOAD_CORE(vaEndPicture);
    LOAD_CORE(vaErrorStr);
    LOAD_CORE(vaExportSurfaceHandle);
    LOAD_CORE(vaGetConfigAttributes);
    LOAD_CORE(vaGetDisplayAttributes);
    LOAD_CORE(vaGetImage);
    LOAD_CORE(vaInitialize);
    LOAD_CORE(vaMapBuffer);
    LOAD_CORE(vaMapBuffer2);
    LOAD_CORE(vaMaxNumEntrypoints);
    LOAD_CORE(vaMaxNumImageFormats);
    LOAD_CORE(vaMaxNumProfiles);
    LOAD_CORE(vaPutImage);
    LOAD_CORE(vaQueryConfigEntrypoints);
    LOAD_CORE(vaQueryConfigProfiles);
    LOAD_CORE(vaQueryImageFormats);
    LOAD_CORE(vaQuerySurfaceAttributes);
    LOAD_CORE(vaQueryVendorString);
    LOAD_CORE(vaQueryVideoProcFilterCaps);
    LOAD_CORE(vaQueryVideoProcFilters);
    LOAD_CORE(vaQueryVideoProcPipelineCaps);
    LOAD_CORE(vaReleaseBufferHandle);
    LOAD_CORE(vaRenderPicture);
    LOAD_CORE(vaSetDriverName);
    LOAD_CORE(vaSetErrorCallback);
    LOAD_CORE(vaSetInfoCallback);
    LOAD_CORE(vaSyncBuffer);
    LOAD_CORE(vaSyncSurface);
    LOAD_CORE(vaTerminate);
    LOAD_CORE(vaUnmapBuffer);
#undef LOAD_CORE

    *(void **)(&get_display_drm) = dlsym(libva_drm, "vaGetDisplayDRM");
    if (!get_display_drm)
        goto fail;
    available = 1;
    return;

fail:
    memset(&functions, 0, sizeof(functions));
    get_display_drm = NULL;
    if (libva_drm)
        dlclose(libva_drm);
    if (libva)
        dlclose(libva);
    libva_drm = NULL;
    libva = NULL;
}

const struct chatt_vaapi_functions *chatt_vaapi_functions(void)
{
    pthread_once(&load_once, load_vaapi);
    return &functions;
}

VADisplay chatt_vaGetDisplayDRM(int fd)
{
    pthread_once(&load_once, load_vaapi);
    return get_display_drm ? get_display_drm(fd) : NULL;
}

int chatt_vaapi_runtime_available(void)
{
    pthread_once(&load_once, load_vaapi);
    return available;
}
