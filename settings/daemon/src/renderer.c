/* A short-lived hardware probe: never pick lavapipe/llvmpipe automatically.
 *
 * Vulkan and EGL are opened on demand instead of linked, so every other
 * uflinux-settingsctl subcommand starts without mapping the Vulkan loader or
 * Mesa. The headers below are used for their types and constants only. */
#include "renderer_ids.h"
#include <vulkan/vulkan.h>
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES2/gl2.h>
#include <dlfcn.h>
#include <stdlib.h>
#include <string.h>

typedef VkResult (*probe_create_instance)(const VkInstanceCreateInfo *, const VkAllocationCallbacks *, VkInstance *);
typedef VkResult (*probe_enumerate_devices)(VkInstance, uint32_t *, VkPhysicalDevice *);
typedef void (*probe_device_properties)(VkPhysicalDevice, VkPhysicalDeviceProperties *);
typedef void (*probe_destroy_instance)(VkInstance, const VkAllocationCallbacks *);

typedef EGLBoolean (*probe_initialize)(EGLDisplay, EGLint *, EGLint *);
typedef EGLBoolean (*probe_bind_api)(EGLenum);
typedef EGLBoolean (*probe_choose_config)(EGLDisplay, const EGLint *, EGLConfig *, EGLint, EGLint *);
typedef EGLContext (*probe_create_context)(EGLDisplay, EGLConfig, EGLContext, const EGLint *);
typedef EGLSurface (*probe_create_pbuffer)(EGLDisplay, EGLConfig, const EGLint *);
typedef EGLBoolean (*probe_make_current)(EGLDisplay, EGLSurface, EGLSurface, EGLContext);
typedef EGLBoolean (*probe_destroy_surface)(EGLDisplay, EGLSurface);
typedef EGLBoolean (*probe_destroy_context)(EGLDisplay, EGLContext);
typedef EGLBoolean (*probe_terminate)(EGLDisplay);
typedef const GLubyte *(*probe_get_string)(GLenum);

/* Core EGL entry points come from libEGL itself; the device extension and the
 * GL entry points are resolved through eglGetProcAddress. */
typedef struct {
    void *library;
    PFNEGLGETPLATFORMDISPLAYEXTPROC get_platform_display;
    PFNEGLQUERYDEVICESEXTPROC query_devices;
    PFNEGLQUERYDEVICESTRINGEXTPROC query_device_string;
    probe_initialize initialize;
    probe_bind_api bind_api;
    probe_choose_config choose_config;
    probe_create_context create_context;
    probe_create_pbuffer create_pbuffer_surface;
    probe_make_current make_current;
    probe_destroy_surface destroy_surface;
    probe_destroy_context destroy_context;
    probe_terminate terminate;
    probe_get_string get_string;
} gl_api;

/* A DRM node alone does not prove acceleration (e.g. virtio without virgl).
 * Initialize the device before deciding; Mesa can fall back to llvmpipe. */
static int accelerated_gl(const gl_api *api, EGLDeviceEXT device) {
    EGLDisplay display = api->get_platform_display(EGL_PLATFORM_DEVICE_EXT, device, NULL);
    if (display == EGL_NO_DISPLAY || !api->initialize(display, NULL, NULL)) return 0;
    EGLConfig config; EGLint count = 0;
    const EGLint attributes[] = {EGL_SURFACE_TYPE, EGL_PBUFFER_BIT,
        EGL_RENDERABLE_TYPE, EGL_OPENGL_ES2_BIT, EGL_NONE};
    const EGLint context_attributes[] = {EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE};
    const EGLint surface_attributes[] = {EGL_WIDTH, 1, EGL_HEIGHT, 1, EGL_NONE};
    EGLContext context = EGL_NO_CONTEXT;
    EGLSurface surface = EGL_NO_SURFACE;
    int accelerated = 0;
    if (api->bind_api(EGL_OPENGL_ES_API) && api->choose_config(display, attributes, &config, 1, &count) && count) {
        context = api->create_context(display, config, EGL_NO_CONTEXT, context_attributes);
        surface = api->create_pbuffer_surface(display, config, surface_attributes);
        if (context != EGL_NO_CONTEXT && surface != EGL_NO_SURFACE && api->make_current(display, surface, surface, context)) {
            const char *renderer = (const char *)api->get_string(GL_RENDERER);
            accelerated = renderer && !strstr(renderer, "llvmpipe") && !strstr(renderer, "softpipe") &&
                !strstr(renderer, "Software") && !strstr(renderer, "SWR") && !strstr(renderer, "lavapipe");
            api->make_current(display, EGL_NO_SURFACE, EGL_NO_SURFACE, EGL_NO_CONTEXT);
        }
    }
    if (surface != EGL_NO_SURFACE) api->destroy_surface(display, surface);
    if (context != EGL_NO_CONTEXT) api->destroy_context(display, context);
    api->terminate(display);
    return accelerated;
}

/* Resolves every entry point the probe needs, or NULL when EGL is unusable.
 * The caller keeps ownership of the handle and closes it. */
static const gl_api *open_gl(void) {
    static gl_api api;
    PFNEGLGETPROCADDRESSPROC get_proc_address;
    api.library = dlopen("libEGL.so.1", RTLD_NOW | RTLD_LOCAL);
    if (!api.library) return NULL;
    get_proc_address = (PFNEGLGETPROCADDRESSPROC)dlsym(api.library, "eglGetProcAddress");
    api.get_platform_display = (PFNEGLGETPLATFORMDISPLAYEXTPROC)(get_proc_address ? get_proc_address("eglGetPlatformDisplayEXT") : NULL);
    api.query_devices = (PFNEGLQUERYDEVICESEXTPROC)(get_proc_address ? get_proc_address("eglQueryDevicesEXT") : NULL);
    api.query_device_string = (PFNEGLQUERYDEVICESTRINGEXTPROC)(get_proc_address ? get_proc_address("eglQueryDeviceStringEXT") : NULL);
    api.initialize = (probe_initialize)dlsym(api.library, "eglInitialize");
    api.bind_api = (probe_bind_api)dlsym(api.library, "eglBindAPI");
    api.choose_config = (probe_choose_config)dlsym(api.library, "eglChooseConfig");
    api.create_context = (probe_create_context)dlsym(api.library, "eglCreateContext");
    api.create_pbuffer_surface = (probe_create_pbuffer)dlsym(api.library, "eglCreatePbufferSurface");
    api.make_current = (probe_make_current)dlsym(api.library, "eglMakeCurrent");
    api.destroy_surface = (probe_destroy_surface)dlsym(api.library, "eglDestroySurface");
    api.destroy_context = (probe_destroy_context)dlsym(api.library, "eglDestroyContext");
    api.terminate = (probe_terminate)dlsym(api.library, "eglTerminate");
    api.get_string = (probe_get_string)(get_proc_address ? get_proc_address("glGetString") : NULL);
    if (!api.get_platform_display || !api.query_devices || !api.query_device_string ||
        !api.initialize || !api.bind_api || !api.choose_config || !api.create_context ||
        !api.create_pbuffer_surface || !api.make_current || !api.destroy_surface ||
        !api.destroy_context || !api.terminate || !api.get_string) {
        dlclose(api.library);
        api.library = NULL;
        return NULL;
    }
    return &api;
}

static int probe_vulkan(void) {
    int accelerated = 0;
    void *library = dlopen("libvulkan.so.1", RTLD_NOW | RTLD_LOCAL);
    if (!library) return 0;
    probe_create_instance create = (probe_create_instance)dlsym(library, "vkCreateInstance");
    probe_enumerate_devices enumerate = (probe_enumerate_devices)dlsym(library, "vkEnumeratePhysicalDevices");
    probe_device_properties properties = (probe_device_properties)dlsym(library, "vkGetPhysicalDeviceProperties");
    probe_destroy_instance destroy = (probe_destroy_instance)dlsym(library, "vkDestroyInstance");
    if (create && enumerate && properties && destroy) {
        VkInstance instance;
        VkApplicationInfo app = {.sType=VK_STRUCTURE_TYPE_APPLICATION_INFO, .apiVersion=VK_API_VERSION_1_1};
        VkInstanceCreateInfo info = {.sType=VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, .pApplicationInfo=&app};
        if (create(&info, NULL, &instance) == VK_SUCCESS) {
            uint32_t count = 0;
            if (enumerate(instance, &count, NULL) == VK_SUCCESS && count && count < 64) {
                VkPhysicalDevice devices[64];
                if (enumerate(instance, &count, devices) == VK_SUCCESS) {
                    for (uint32_t i=0; i<count; ++i) {
                        VkPhysicalDeviceProperties p;
                        properties(devices[i], &p);
                        if ((p.deviceType==VK_PHYSICAL_DEVICE_TYPE_INTEGRATED_GPU ||
                             p.deviceType==VK_PHYSICAL_DEVICE_TYPE_DISCRETE_GPU) &&
                            !uflinux_legacy_intel_gpu(p.vendorID, p.deviceID)) {
                            accelerated = 1;
                            break;
                        }
                    }
                }
            }
            destroy(instance, NULL);
        }
    }
    dlclose(library);
    return accelerated;
}

static int probe_gl(void) {
    int accelerated = 0;
    const gl_api *api = open_gl();
    if (!api) return 0;
    EGLDeviceEXT devices[32]; EGLint count=0;
    if (api->query_devices(32, devices, &count)) {
        for (int i=0; i<count; ++i) {
            const char *ext = api->query_device_string(devices[i], EGL_EXTENSIONS);
            if (ext && !strstr(ext, "EGL_MESA_device_software") &&
                (strstr(ext, "EGL_EXT_device_drm") || strstr(ext, "EGL_EXT_device_drm_render_node")) &&
                accelerated_gl(api, devices[i])) { accelerated = 1; break; }
        }
    }
    dlclose(api->library);
    return accelerated;
}

int uflinux_renderer_probe(void) {
    if (probe_vulkan()) return 2;
    if (probe_gl()) return 1;
    return 0;
}
