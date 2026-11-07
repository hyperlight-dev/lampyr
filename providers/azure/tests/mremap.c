#define _GNU_SOURCE
#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <sys/mman.h>

#define PAGE 4096
#define CHECK(expression) do { \
    if (!(expression)) { \
        fprintf(stderr, "failed: %s (errno=%d)\n", #expression, errno); \
        return 1; \
    } \
} while (0)

int main(void)
{
    char *base = mmap(NULL, 8 * PAGE, PROT_READ | PROT_WRITE,
                      MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    CHECK(base != MAP_FAILED);
    base[0] = 42;
    base[PAGE] = 43;
    CHECK(munmap(base + 2 * PAGE, 6 * PAGE) == 0);

    CHECK(mremap(base, PAGE, 2 * PAGE, 0) == MAP_FAILED && errno == ENOMEM);
    CHECK(base[0] == 42 && base[PAGE] == 43);
    CHECK(mremap(base + 4 * PAGE, PAGE, 2 * PAGE, 0) == MAP_FAILED && errno == EFAULT);
    CHECK(mremap(base, 3 * PAGE, 4 * PAGE, 0) == MAP_FAILED && errno == EFAULT);
    CHECK(mremap(base + 1, PAGE, PAGE, 0) == MAP_FAILED && errno == EINVAL);
    CHECK(mremap(base, PAGE, 0, 0) == MAP_FAILED && errno == EINVAL);
    CHECK(mremap(base, PAGE, SIZE_MAX, 0) == MAP_FAILED && errno == ENOMEM);
    CHECK(mremap(base, PAGE, PAGE, MREMAP_FIXED, base) == MAP_FAILED && errno == EINVAL);
    CHECK(mremap(base, PAGE, PAGE, MREMAP_MAYMOVE | MREMAP_FIXED,
                 base + 4 * PAGE) == MAP_FAILED && errno == ENOMEM);
    CHECK(mremap(base, 2 * PAGE, PAGE, MREMAP_MAYMOVE | MREMAP_FIXED,
                 base + PAGE) == MAP_FAILED && errno == EINVAL);

    CHECK(mremap(base, 2 * PAGE, 3 * PAGE, 0) == base);
    CHECK(base[0] == 42 && base[PAGE] == 43 && base[2 * PAGE] == 0);
    CHECK(mremap(base, 3 * PAGE, 2 * PAGE, MREMAP_MAYMOVE) == base);
    CHECK(mremap(base, 2 * PAGE, 3 * PAGE, MREMAP_MAYMOVE) == base);
    CHECK(base[0] == 42 && base[PAGE] == 43 && base[2 * PAGE] == 0);
    base[2 * PAGE] = 44;
    CHECK(mmap(base + 3 * PAGE, PAGE, PROT_NONE,
               MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED_NOREPLACE, -1, 0) == base + 3 * PAGE);
    CHECK(mremap(base, 3 * PAGE, 4 * PAGE, 0) == MAP_FAILED && errno == ENOMEM);
    CHECK(mremap(base, 3 * PAGE, 4 * PAGE, MREMAP_MAYMOVE) == MAP_FAILED && errno == ENOMEM);
    CHECK(base[2 * PAGE] == 44);
    CHECK(mremap(base, 3 * PAGE, PAGE, 0) == base);
    CHECK(base[0] == 42);
    CHECK(mremap(base + PAGE, PAGE, PAGE, 0) == MAP_FAILED && errno == EFAULT);
    CHECK(munmap(base, 4 * PAGE) == 0);
    puts("mremap: growth, shrink, contents and boundary errors passed");
    return 0;
}
