// ocloop — the open/close loops of the dev-perf A-B-B-A (rung-10-dev-perf.md §10.5,
// rung-10-writeback-timer.md): THREADS threads, each opening and closing PATH for SECS
// seconds (mode `oc`), with a 4 KiB pread between (`ocr`); prints the aggregate rate.
//   zig cc -O2 -o ocloop scripts/perf/ocloop.c -lpthread
//   ocloop oc|ocr PATH THREADS SECS
#define _GNU_SOURCE
#include <fcntl.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

static const char *path;
static int with_read;
static atomic_int stop;
static atomic_ulong total;

static void *run(void *arg) {
    (void)arg;
    char buf[4096];
    unsigned long n = 0;
    while (!atomic_load_explicit(&stop, memory_order_relaxed)) {
        int fd = open(path, O_RDONLY | O_CLOEXEC);
        if (fd < 0) {
            perror("open");
            exit(1);
        }
        if (with_read && pread(fd, buf, sizeof buf, 0) < 0) {
            perror("pread");
            exit(1);
        }
        close(fd);
        n++;
    }
    atomic_fetch_add(&total, n);
    return NULL;
}

int main(int argc, char **argv) {
    if (argc != 5) {
        fprintf(stderr, "usage: ocloop oc|ocr PATH THREADS SECS\n");
        return 2;
    }
    with_read = strcmp(argv[1], "ocr") == 0;
    path = argv[2];
    int threads = atoi(argv[3]);
    int secs = atoi(argv[4]);
    pthread_t t[64];
    if (threads < 1 || threads > 64) return 2;
    struct timespec a, b;
    clock_gettime(CLOCK_MONOTONIC, &a);
    for (int i = 0; i < threads; i++) pthread_create(&t[i], NULL, run, NULL);
    sleep((unsigned)secs);
    atomic_store(&stop, 1);
    for (int i = 0; i < threads; i++) pthread_join(t[i], NULL);
    clock_gettime(CLOCK_MONOTONIC, &b);
    double el = (double)(b.tv_sec - a.tv_sec) + (double)(b.tv_nsec - a.tv_nsec) / 1e9;
    printf("%s %d %.0f\n", argv[1], threads, (double)atomic_load(&total) / el);
    return 0;
}
