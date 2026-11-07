#define _GNU_SOURCE
#include <errno.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

#define PAGE 4096
#define CHECK(expression) do { \
    if (!(expression)) { \
        fprintf(stderr, "failed: %s (errno=%d)\n", #expression, errno); \
        return 1; \
    } \
} while (0)

static int mapping_count(void)
{
    FILE *maps = fopen("/dev/maps", "r");
    char line[512];
    int count = 0;

    if (!maps)
        return -1;
    while (fgets(line, sizeof(line), maps))
        count++;
    fclose(maps);
    return count;
}

static int child_allocations(int ready)
{
    char *memory = mmap(NULL, 16 * PAGE, PROT_READ | PROT_WRITE,
                        MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    CHECK(memory != MAP_FAILED);
    memset(memory, 42, 16 * PAGE);
    CHECK(mprotect(memory + PAGE, PAGE, PROT_READ) == 0);
    CHECK(munmap(memory + 4 * PAGE, 2 * PAGE) == 0);
    if (ready >= 0) {
        CHECK(write(ready, "x", 1) == 1);
        for (;;)
            pause();
    }
    return 0;
}

struct shared {
    char *memory;
};

static int shared_child(void *arg)
{
    struct shared *shared = arg;
    shared->memory = mmap(NULL, PAGE, PROT_READ | PROT_WRITE,
                          MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    CHECK(shared->memory != MAP_FAILED);
    shared->memory[0] = 73;
    return 0;
}

int main(int argc, char **argv)
{
    if (argc == 2 && strcmp(argv[1], "child") == 0)
        return child_allocations(-1);
    if (argc == 3 && strcmp(argv[1], "wait") == 0)
        return child_allocations(atoi(argv[2]));

    char *parent = mmap(NULL, PAGE, PROT_READ | PROT_WRITE,
                        MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    CHECK(parent != MAP_FAILED);
    parent[0] = 19;
    int before = mapping_count();
    CHECK(before > 0);
    for (int iteration = 0; iteration < 16; iteration++) {
        pid_t pid = vfork();
        CHECK(pid >= 0);
        if (!pid) {
            char *args[] = {argv[0], "child", NULL};
            execv(argv[0], args);
            _exit(127);
        }
        int status;
        CHECK(waitpid(pid, &status, 0) == pid);
        CHECK(WIFEXITED(status) && WEXITSTATUS(status) == 0);
        CHECK(parent[0] == 19);
        /* Kernel thread stacks are collected by the idle scheduler. */
        CHECK(usleep(1000) == 0);
        CHECK(mapping_count() <= before + 4);
    }

    pid_t pid = vfork();
    CHECK(pid >= 0);
    if (!pid) {
        char *args[] = {"/missing-executable", NULL};
        execv(args[0], args);
        _exit(errno == ENOENT ? 0 : 1);
    }
    int status;
    CHECK(waitpid(pid, &status, 0) == pid);
    CHECK(WIFEXITED(status) && WEXITSTATUS(status) == 0);
    CHECK(parent[0] == 19);

    void *stack = mmap(NULL, 64 * PAGE, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    CHECK(stack != MAP_FAILED);
    struct shared shared = {0};
    pid = clone(shared_child, (char *)stack + 64 * PAGE, CLONE_VM | SIGCHLD, &shared);
    CHECK(pid > 0);
    CHECK(waitpid(pid, &status, 0) == pid);
    CHECK(WIFEXITED(status) && WEXITSTATUS(status) == 0);
    CHECK(shared.memory && shared.memory[0] == 73);
    CHECK(munmap(shared.memory, PAGE) == 0);
    CHECK(munmap(stack, 64 * PAGE) == 0);

    int ready[2];
    CHECK(pipe(ready) == 0);
    char fd[16];
    snprintf(fd, sizeof(fd), "%d", ready[1]);
    pid = vfork();
    CHECK(pid >= 0);
    if (!pid) {
        char *args[] = {argv[0], "wait", fd, NULL};
        execv(argv[0], args);
        _exit(127);
    }
    char byte;
    CHECK(read(ready[0], &byte, 1) == 1);
    CHECK(kill(pid, SIGTERM) == 0);
    CHECK(waitpid(pid, &status, 0) == pid);
    CHECK(WIFSIGNALED(status) && WTERMSIG(status) == SIGTERM);
    CHECK(close(ready[0]) == 0 && close(ready[1]) == 0);
    CHECK(usleep(1000) == 0);
    CHECK(mapping_count() <= before + 4);
    CHECK(parent[0] == 19);
    CHECK(munmap(parent, PAGE) == 0);
    puts("process memory: exit, shared mappings, failed exec and signals passed");
    return 0;
}
