#define _GNU_SOURCE
#include <dlfcn.h>
#include <string.h>
#include <unistd.h>

/* Simulate the system helper's root check while D-Bus keeps the real UID. */
uid_t geteuid(void)
{
    Dl_info caller;
    if (dladdr(__builtin_return_address(0), &caller) && caller.dli_fname) {
        const char *name = strrchr(caller.dli_fname, '/');
        name = name ? name + 1 : caller.dli_fname;
        if (strcmp(name, "uflinux-settings-system") == 0 ||
            strcmp(name, "uflinux-settings-system-bolt-instrumented") == 0 ||
            strcmp(name, "uflinux-settings-system-bolt-optimized") == 0)
            return 0;
    }
    return getuid();
}
