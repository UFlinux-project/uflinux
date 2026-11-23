#define _GNU_SOURCE
#include <assert.h>
#include <dlfcn.h>
#include <errno.h>
#include <malloc.h>
#include <pthread.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

static _Thread_local volatile unsigned thread_slot;

static void exercise(void)
{
    void *slots[256] = {0};
    unsigned state = 12345;
    for (unsigned i = 0; i < 40000; i++) {
        state = state * 1664525u + 1013904223u;
        unsigned slot = (state >> 16) % 256;
        size_t size = (state % 16384) + 1;
        void *next = realloc(slots[slot], size);
        assert(next);
        memset(next, (unsigned char)i, size);
        slots[slot] = next;
    }
    for (unsigned i = 0; i < 256; i++)
        free(slots[i]);
}

static void *worker(void *allocation)
{
    assert(thread_slot == 0);
    thread_slot = 2;
    free(allocation);
    exercise();
    return malloc(32768);
}

int main(int argc, char **argv)
{
    (void)argv;
    assert(thread_slot == 0);
    thread_slot = 1;
    unsigned loops = argc > 1 ? 100 : 1;
    volatile size_t huge = SIZE_MAX;
    errno = 0;
    assert(!calloc(huge, 2) && errno == ENOMEM);
    void *aligned = NULL;
    assert(!posix_memalign(&aligned, 4096, 257));
    assert((uintptr_t)aligned % 4096 == 0 && malloc_usable_size(aligned) >= 257);
    free(aligned);
    for (unsigned i = 0; i < loops; i++) {
        pthread_t thread;
        void *result;
        void *initial = malloc(123);
        assert(initial);
        assert(!pthread_create(&thread, NULL, worker, initial));
        exercise();
        assert(!pthread_join(thread, &result) && result);
        free(result);
        assert(thread_slot == 1);
    }
#ifndef STATIC_TEST
    void *self = dlopen(NULL, RTLD_NOW);
    assert(self && dlsym(self, "malloc") && !dlclose(self));
#endif
    pid_t child = fork();
    assert(child >= 0);
    if (child == 0) {
        exercise();
        _exit(0);
    }
    int status;
    assert(waitpid(child, &status, 0) == child && WIFEXITED(status) && WEXITSTATUS(status) == 0);
}
