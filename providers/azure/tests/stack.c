#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <stdio.h>
#include <sys/mman.h>
#include <sys/resource.h>

int main(void)
{
    struct rlimit limit = {0};
    pthread_attr_t attributes;
    void *base = NULL;
    size_t size = 0;
    int status = getrlimit(RLIMIT_STACK, &limit);
    printf("getrlimit=%d soft=%lu hard=%lu\n", status,
           (unsigned long)limit.rlim_cur, (unsigned long)limit.rlim_max);
    status = pthread_getattr_np(pthread_self(), &attributes);
    printf("pthread_getattr_np=%d\n", status);
    if (status != 0)
        return 1;
    status = pthread_attr_getstack(&attributes, &base, &size);
    printf("pthread_attr_getstack=%d base=%p size=%zu current=%p\n",
           status, base, size, (void *)&limit);
    pthread_attr_destroy(&attributes);
    errno = 0;
    void *probe = mremap((char *)base - 4096, 4096, 8192, 0);
    printf("mremap stack probe=%p errno=%d\n", probe, errno);
    FILE *maps = fopen("/proc/self/maps", "r");
    printf("proc maps=%s errno=%d\n", maps == NULL ? "unavailable" : "available", errno);
    if (maps != NULL)
        fclose(maps);
    return status == 0 ? 0 : 1;
}
