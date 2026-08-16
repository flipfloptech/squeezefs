/* mdstorm — the metadata-storm driver (K7 storm shape), reconstructed
 * 2026-08-16 as a COMMITTED rig from the 2026-07-14 baseline's ad-hoc
 * harness (`~/tmp/mdbase_20260714/mdstorm.c`, lost with the box's tmp):
 * T threads barrier-start one phase; thread w takes names i = w, w+T, …
 * (the shared-dir interleave that makes the one-dir shape a parent-lock
 * crucible), plus the many-dirs mode (per-thread directory — the
 * mfcreate shape). Reports ops/s per phase; totals are the caller's
 * scale knob (`.benchmarks/2026-07-14-metadata-throughput-baseline.md`,
 * Storm counts row).
 *
 * Usage: mdstorm <dir> <threads> <count> <phase>
 *   phase: mkdir | create | stat | rename | unlink | manydirs | rmdir
 * Phases compose into the canonical sequence via tests/run_mdstorm.sh.
 *
 * Build: cc -O2 -pthread -o mdstorm mdstorm.c
 */
#define _GNU_SOURCE
#include <fcntl.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/time.h>
#include <unistd.h>

static const char *g_dir;
static int g_threads;
static long g_count;
static const char *g_phase;
static pthread_barrier_t g_bar;
static volatile int g_failed;

static double now_s(void) {
    struct timeval tv;
    gettimeofday(&tv, NULL);
    return (double)tv.tv_sec + (double)tv.tv_usec / 1e6;
}

static void fail(const char *what, const char *path) {
    if (!__sync_fetch_and_or(&g_failed, 1))
        fprintf(stderr, "mdstorm: %s failed on %s: %m\n", what, path);
}

static void *worker(void *arg) {
    long w = (long)arg;
    char p[4096], q[4096];
    pthread_barrier_wait(&g_bar);
    for (long i = w; i < g_count && !g_failed; i += g_threads) {
        if (!strcmp(g_phase, "mkdir")) {
            snprintf(p, sizeof p, "%s/d%ld", g_dir, i);
            if (mkdir(p, 0755) != 0) fail("mkdir", p);
        } else if (!strcmp(g_phase, "create")) {
            snprintf(p, sizeof p, "%s/f%ld", g_dir, i);
            int fd = open(p, O_CREAT | O_WRONLY | O_EXCL, 0644);
            if (fd < 0) { fail("create", p); continue; }
            close(fd);
        } else if (!strcmp(g_phase, "stat")) {
            struct stat st;
            snprintf(p, sizeof p, "%s/f%ld", g_dir, i);
            if (stat(p, &st) != 0) fail("stat", p);
        } else if (!strcmp(g_phase, "rename")) {
            snprintf(p, sizeof p, "%s/f%ld", g_dir, i);
            snprintf(q, sizeof q, "%s/r%ld", g_dir, i);
            if (rename(p, q) != 0) fail("rename", p);
        } else if (!strcmp(g_phase, "unlink")) {
            snprintf(p, sizeof p, "%s/r%ld", g_dir, i);
            if (unlink(p) != 0) fail("unlink", p);
        } else if (!strcmp(g_phase, "manydirs")) {
            /* mfcreate shape: per-thread directory, create + unlink. */
            snprintf(p, sizeof p, "%s/t%ld/f%ld", g_dir, w, i);
            int fd = open(p, O_CREAT | O_WRONLY | O_EXCL, 0644);
            if (fd < 0) { fail("manydirs-create", p); continue; }
            close(fd);
            if (unlink(p) != 0) fail("manydirs-unlink", p);
        } else if (!strcmp(g_phase, "rmdir")) {
            snprintf(p, sizeof p, "%s/d%ld", g_dir, i);
            if (rmdir(p) != 0) fail("rmdir", p);
        }
    }
    return NULL;
}

int main(int argc, char **argv) {
    if (argc != 5) {
        fprintf(stderr,
                "usage: mdstorm <dir> <threads> <count> "
                "<mkdir|create|stat|rename|unlink|manydirs|rmdir>\n");
        return 2;
    }
    g_dir = argv[1];
    g_threads = atoi(argv[2]);
    g_count = atol(argv[3]);
    g_phase = argv[4];
    if (g_threads < 1 || g_threads > 512 || g_count < 1) {
        fprintf(stderr, "mdstorm: bad threads/count\n");
        return 2;
    }
    if (!strcmp(g_phase, "manydirs")) {
        /* Per-thread parents, minted OUTSIDE the timed window. */
        char p[4096];
        for (long w = 0; w < g_threads; w++) {
            snprintf(p, sizeof p, "%s/t%ld", g_dir, w);
            if (mkdir(p, 0755) != 0) {
                fprintf(stderr, "mdstorm: pre-mkdir %s: %m\n", p);
                return 2;
            }
        }
    }
    pthread_barrier_init(&g_bar, NULL, (unsigned)g_threads + 1);
    pthread_t th[512];
    for (long w = 0; w < g_threads; w++)
        if (pthread_create(&th[w], NULL, worker, (void *)w) != 0) {
            fprintf(stderr, "mdstorm: pthread_create: %m\n");
            return 2;
        }
    double t0;
    pthread_barrier_wait(&g_bar); /* release the storm */
    t0 = now_s();
    for (long w = 0; w < g_threads; w++) pthread_join(th[w], NULL);
    double wall = now_s() - t0;
    if (g_failed) return 1;
    /* manydirs counts create+unlink as the baseline did: N of each. */
    printf("%s ops=%ld wall_s=%.3f ops_s=%.0f\n", g_phase, g_count, wall,
           (double)g_count / (wall > 0 ? wall : 1e-9));
    return 0;
}
