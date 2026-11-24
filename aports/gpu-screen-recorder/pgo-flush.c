#include <stdlib.h>
#include <unistd.h>

/*
 * The recorder normally uses _exit() to bypass CUDA's exit handlers.
 * The headless trainer does not load CUDA and needs the profiler's handlers.
 */
void _exit(int status)
{
	exit(status);
}
