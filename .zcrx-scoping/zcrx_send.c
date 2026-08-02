/* zcrx_send.c — ephemeral pattern sender for the zcrx Phase-1 bracket.
 * Runs on a storage node as a plain process (no config changes): N threads,
 * each opens conns to host:(port_base + port_stride*tid%ports) and blasts a
 * PATTERN(0x5A)-filled buffer for --secs, then shuts down cleanly.
 *
 * Build: gcc -O2 -o zcrx_send zcrx_send.c -lpthread
 */
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <unistd.h>
#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <time.h>
#include <sys/socket.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <arpa/inet.h>

#define PATTERN 0x5A

static const char *g_host = NULL;
static int g_port_base = 5301;
static int g_ports = 1;               /* receiver threads = distinct ports  */
static int g_conns_per_port = 4;
static int g_secs = 30;
static int g_bs_kb = 1024;
static volatile int g_stop;

struct sarg { int port; int id; uint64_t bytes; };

static void *sender(void *argp)
{
    struct sarg *sa = argp;
    int fd = socket(AF_INET, SOCK_STREAM, 0);
    if (fd < 0) { perror("socket"); return NULL; }
    struct sockaddr_in dst;
    memset(&dst, 0, sizeof(dst));
    dst.sin_family = AF_INET;
    dst.sin_port = htons((uint16_t)sa->port);
    inet_pton(AF_INET, g_host, &dst.sin_addr);
    /* retry connect while receiver threads come up */
    int tries = 200;
    while (connect(fd, (struct sockaddr *)&dst, sizeof(dst)) < 0) {
        if (--tries == 0) { perror("connect"); close(fd); return NULL; }
        usleep(50000);
    }
    size_t bs = (size_t)g_bs_kb << 10;
    unsigned char *buf = malloc(bs);
    memset(buf, PATTERN, bs);
    uint64_t sent = 0;
    while (!g_stop) {
        ssize_t n = write(fd, buf, bs);
        if (n < 0) {
            if (errno == EINTR) continue;
            break;
        }
        sent += (uint64_t)n;
    }
    shutdown(fd, SHUT_WR);
    close(fd);
    free(buf);
    sa->bytes = sent;
    return NULL;
}

static void on_alarm(int sig) { (void)sig; g_stop = 1; }

int main(int argc, char **argv)
{
    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "--host")) g_host = argv[++i];
        else if (!strcmp(argv[i], "--port-base")) g_port_base = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--ports")) g_ports = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--conns-per-port")) g_conns_per_port = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--secs")) g_secs = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--bs-kb")) g_bs_kb = atoi(argv[++i]);
        else { fprintf(stderr, "unknown arg %s\n", argv[i]); return 1; }
    }
    if (!g_host) { fprintf(stderr, "--host required\n"); return 1; }
    signal(SIGPIPE, SIG_IGN);
    signal(SIGALRM, on_alarm);
    alarm((unsigned)g_secs);

    int total = g_ports * g_conns_per_port;
    pthread_t *tids = calloc((size_t)total, sizeof(*tids));
    struct sarg *sargs = calloc((size_t)total, sizeof(*sargs));
    int k = 0;
    for (int p = 0; p < g_ports; p++)
        for (int c = 0; c < g_conns_per_port; c++, k++) {
            sargs[k].port = g_port_base + p;
            sargs[k].id = k;
            if (pthread_create(&tids[k], NULL, sender, &sargs[k])) {
                perror("pthread_create");
                return 1;
            }
        }
    uint64_t bytes = 0;
    for (k = 0; k < total; k++) {
        pthread_join(tids[k], NULL);
        bytes += sargs[k].bytes;
    }
    printf("SENDER-DONE conns=%d bytes=%llu secs=%d GBps=%.3f\n", total,
           (unsigned long long)bytes, g_secs,
           (double)bytes / (double)g_secs / 1e9);
    return 0;
}
