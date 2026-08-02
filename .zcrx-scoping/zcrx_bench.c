/* zcrx_bench.c — Phase-1 scoping instrument for the SqueezeFS zcrx initiator-lane
 * campaign (2026-08-03): io_uring RECV_ZC (zero-copy RX via REGISTER_ZCRX_IFQ +
 * refill ring) vs the identical classic io_uring RECV receiver, raw syscalls only
 * (no liburing — EL8 userspace). One worker thread = one ring = one listen port
 * = (zcrx mode) one ifq bound to one NIC RX queue. Steering to that queue is the
 * harness's job (ethtool -N dst-port rules).
 *
 * Both modes share ring geometry (DEFER_TASKRUN|SINGLE_ISSUER|CQE32), the same
 * accept/validation/stat machinery, and the same submit/wait loop — the only
 * delta is the receive opcode and where payload bytes land.
 *
 * Validation: sender fills every byte with PATTERN (0x5A); receiver checks the
 * first and last byte of every completed span (catches wrong-chunk mapping
 * without paying a full CPU pass — this bench prices the RX copy, so the
 * receiver must not add one).
 *
 * Build: gcc -O2 -o zcrx_bench zcrx_bench.c -lpthread   (kernel-ml-headers >= 6.15)
 */
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <stdatomic.h>
#include <unistd.h>
#include <errno.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <time.h>
#include <net/if.h>
#include <sys/socket.h>
#include <sys/resource.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <arpa/inet.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <linux/io_uring.h>

/* IORING_OP_RECV_ZC is an enum (not preprocessor-visible); the zcrx uapi
 * header's AREA_SHIFT macro is the reliable presence guard. */
#ifndef IORING_ZCRX_AREA_SHIFT
#error "kernel headers lack the zcrx uapi (need kernel-ml-headers >= 6.15)"
#endif

#define PATTERN 0x5A

static int g_mode_zcrx = 0;           /* 0 = classic RECV, 1 = RECV_ZC       */
static int g_threads = 1;
static int g_conns_per_thread = 4;
static int g_base_port = 5301;
static int g_cpu_base = 2;            /* pin worker i -> cpu_base + 2*i      */
static int g_qid_base = 24;           /* zcrx: bind ifq of worker i -> qid_base + i */
static const char *g_ifname = "ens1f0np0";
static long g_area_mb = 128;          /* zcrx area per thread                */
static unsigned g_rq_entries = 0;     /* 0 = derive 1:1 with area chunks     */
static unsigned g_rx_buf_len = 0;     /* 0 = kernel default (page size)      */
static int g_secs_max = 600;          /* safety alarm                        */
static int g_thp = 0;                 /* MADV_HUGEPAGE on the area           */
static int g_classic_buf_kb = 1024;   /* classic per-conn recv buffer        */

/* ------------------------------------------------------------------ stats */
struct tstat {
    _Atomic uint64_t bytes;
    _Atomic uint64_t cqes;
    _Atomic uint64_t rearms;
    _Atomic uint64_t val_errors;
    _Atomic int done;                  /* conns finished on this thread */
} __attribute__((aligned(64)));

static struct tstat *g_stats;
static _Atomic int g_threads_ready;
static _Atomic int g_conns_connected;
static volatile int g_stop;

/* -------------------------------------------------------------- raw uring */
static int sys_io_uring_setup(unsigned entries, struct io_uring_params *p)
{ return (int)syscall(425, entries, p); }
static int sys_io_uring_enter(int fd, unsigned to_submit, unsigned min_complete,
                              unsigned flags)
{ return (int)syscall(426, fd, to_submit, min_complete, flags, NULL, 0); }
static int sys_io_uring_register(int fd, unsigned opcode, void *arg, unsigned nr)
{ return (int)syscall(427, fd, opcode, arg, nr); }

struct uring {
    int fd;
    unsigned sq_entries, cq_entries;
    unsigned *sq_head, *sq_tail, *sq_mask, *sq_array;
    unsigned *cq_head, *cq_tail, *cq_mask, *cq_overflow;
    struct io_uring_sqe *sqes;
    unsigned char *cqes;               /* stride 32 (CQE32) */
    unsigned local_sq_tail;
    unsigned submitted;
};

static void die(const char *what)
{ fprintf(stderr, "FATAL: %s: %s\n", what, strerror(errno)); exit(1); }

static void uring_init(struct uring *r, unsigned sq, unsigned cq)
{
    struct io_uring_params p;
    memset(&p, 0, sizeof(p));
    p.flags = IORING_SETUP_DEFER_TASKRUN | IORING_SETUP_SINGLE_ISSUER |
              IORING_SETUP_CQE32 | IORING_SETUP_CQSIZE | IORING_SETUP_CLAMP;
    p.cq_entries = cq;
    int fd = sys_io_uring_setup(sq, &p);
    if (fd < 0)
        die("io_uring_setup");
    r->fd = fd;
    r->sq_entries = p.sq_entries;
    r->cq_entries = p.cq_entries;

    size_t sq_sz = p.sq_off.array + p.sq_entries * sizeof(unsigned);
    size_t cq_sz = p.cq_off.cqes + (size_t)p.cq_entries * 32;
    void *sq_ptr, *cq_ptr;
    if (p.features & IORING_FEAT_SINGLE_MMAP) {
        size_t sz = sq_sz > cq_sz ? sq_sz : cq_sz;
        sq_ptr = mmap(NULL, sz, PROT_READ | PROT_WRITE,
                      MAP_SHARED | MAP_POPULATE, fd, IORING_OFF_SQ_RING);
        if (sq_ptr == MAP_FAILED) die("mmap sq ring");
        cq_ptr = sq_ptr;
    } else {
        sq_ptr = mmap(NULL, sq_sz, PROT_READ | PROT_WRITE,
                      MAP_SHARED | MAP_POPULATE, fd, IORING_OFF_SQ_RING);
        if (sq_ptr == MAP_FAILED) die("mmap sq ring");
        cq_ptr = mmap(NULL, cq_sz, PROT_READ | PROT_WRITE,
                      MAP_SHARED | MAP_POPULATE, fd, IORING_OFF_CQ_RING);
        if (cq_ptr == MAP_FAILED) die("mmap cq ring");
    }
    r->sq_head  = (unsigned *)((char *)sq_ptr + p.sq_off.head);
    r->sq_tail  = (unsigned *)((char *)sq_ptr + p.sq_off.tail);
    r->sq_mask  = (unsigned *)((char *)sq_ptr + p.sq_off.ring_mask);
    r->sq_array = (unsigned *)((char *)sq_ptr + p.sq_off.array);
    r->cq_head  = (unsigned *)((char *)cq_ptr + p.cq_off.head);
    r->cq_tail  = (unsigned *)((char *)cq_ptr + p.cq_off.tail);
    r->cq_mask  = (unsigned *)((char *)cq_ptr + p.cq_off.ring_mask);
    r->cq_overflow = (unsigned *)((char *)cq_ptr + p.cq_off.overflow);
    r->cqes     = (unsigned char *)cq_ptr + p.cq_off.cqes;

    r->sqes = mmap(NULL, p.sq_entries * sizeof(struct io_uring_sqe),
                   PROT_READ | PROT_WRITE, MAP_SHARED | MAP_POPULATE, fd,
                   IORING_OFF_SQES);
    if (r->sqes == MAP_FAILED) die("mmap sqes");
    r->local_sq_tail = *r->sq_tail;
    r->submitted = r->local_sq_tail;
}

static struct io_uring_sqe *get_sqe(struct uring *r)
{
    unsigned head = __atomic_load_n(r->sq_head, __ATOMIC_ACQUIRE);
    if (r->local_sq_tail - head >= r->sq_entries)
        return NULL;
    unsigned idx = r->local_sq_tail & *r->sq_mask;
    struct io_uring_sqe *sqe = &r->sqes[idx];
    memset(sqe, 0, sizeof(*sqe));
    r->sq_array[idx] = idx;
    r->local_sq_tail++;
    return sqe;
}

/* returns count newly published for submission */
static unsigned flush_sq(struct uring *r)
{
    unsigned n = r->local_sq_tail - r->submitted;
    if (n)
        __atomic_store_n(r->sq_tail, r->local_sq_tail, __ATOMIC_RELEASE);
    r->submitted = r->local_sq_tail;
    return n;
}

/* ------------------------------------------------------------- zcrx state */
struct zcrx {
    unsigned char *area;
    size_t area_sz;
    uint64_t area_token;
    unsigned zcrx_id;
    unsigned rq_entries, rq_mask;
    unsigned *rq_khead, *rq_ktail;
    struct io_uring_zcrx_rqe *rqes;
    unsigned rq_tail;
};

static void zcrx_setup(struct uring *r, struct zcrx *z, unsigned ifindex,
                       unsigned qid)
{
    long page = sysconf(_SC_PAGESIZE);
    z->area_sz = (size_t)g_area_mb << 20;
    z->area = mmap(NULL, z->area_sz, PROT_READ | PROT_WRITE,
                   MAP_ANONYMOUS | MAP_PRIVATE, -1, 0);
    if (z->area == MAP_FAILED) die("mmap zcrx area");
    if (g_thp)
        madvise(z->area, z->area_sz, MADV_HUGEPAGE);

    unsigned chunks = (unsigned)(z->area_sz / (g_rx_buf_len ? g_rx_buf_len
                                                            : (unsigned)page));
    unsigned rq = g_rq_entries ? g_rq_entries : chunks;
    if (rq > 32768) rq = 32768;
    /* pow2 floor */
    while (rq & (rq - 1)) rq &= rq - 1;
    z->rq_entries = rq;

    size_t ring_sz = (size_t)page + (size_t)rq * sizeof(struct io_uring_zcrx_rqe);
    ring_sz = (ring_sz + page - 1) & ~((size_t)page - 1);
    void *ring_ptr = mmap(NULL, ring_sz, PROT_READ | PROT_WRITE,
                          MAP_ANONYMOUS | MAP_PRIVATE, -1, 0);
    if (ring_ptr == MAP_FAILED) die("mmap refill ring");

    struct io_uring_region_desc rd;
    memset(&rd, 0, sizeof(rd));
    rd.user_addr = (uint64_t)(uintptr_t)ring_ptr;
    rd.size = ring_sz;
    rd.flags = IORING_MEM_REGION_TYPE_USER;

    struct io_uring_zcrx_area_reg ar;
    memset(&ar, 0, sizeof(ar));
    ar.addr = (uint64_t)(uintptr_t)z->area;
    ar.len = z->area_sz;

    struct io_uring_zcrx_ifq_reg reg;
    memset(&reg, 0, sizeof(reg));
    reg.if_idx = ifindex;
    reg.if_rxq = qid;
    reg.rq_entries = z->rq_entries;
    reg.area_ptr = (uint64_t)(uintptr_t)&ar;
    reg.region_ptr = (uint64_t)(uintptr_t)&rd;
    reg.rx_buf_len = g_rx_buf_len;

    if (sys_io_uring_register(r->fd, IORING_REGISTER_ZCRX_IFQ, &reg, 1) < 0) {
        fprintf(stderr, "REGISTER_ZCRX_IFQ(if_idx=%u rxq=%u rq=%u rx_buf_len=%u): %s\n",
                ifindex, qid, z->rq_entries, g_rx_buf_len, strerror(errno));
        exit(2);
    }
    z->rq_khead = (unsigned *)((char *)ring_ptr + reg.offsets.head);
    z->rq_ktail = (unsigned *)((char *)ring_ptr + reg.offsets.tail);
    z->rqes = (struct io_uring_zcrx_rqe *)((char *)ring_ptr + reg.offsets.rqes);
    z->rq_mask = z->rq_entries - 1;
    z->rq_tail = *z->rq_ktail;
    z->area_token = ar.rq_area_token;
    z->zcrx_id = reg.zcrx_id;
    fprintf(stderr, "zcrx ready: ifq id=%u rxq=%u rq_entries=%u area=%ldMiB token=0x%llx\n",
            z->zcrx_id, qid, z->rq_entries, g_area_mb,
            (unsigned long long)z->area_token);
}

/* -------------------------------------------------------------- worker(s) */
struct conn {
    int fd;
    unsigned char *buf;                /* classic mode only */
    int open;
};

struct worker_arg { int tid; };

static void arm_recv_zc(struct uring *r, struct zcrx *z, int connfd, int idx)
{
    struct io_uring_sqe *sqe = get_sqe(r);
    if (!sqe) die("sq full on arm");
    sqe->opcode = IORING_OP_RECV_ZC;
    sqe->fd = connfd;
    sqe->ioprio = IORING_RECV_MULTISHOT;
    sqe->zcrx_ifq_idx = z->zcrx_id;
    sqe->len = 0;
    sqe->user_data = (uint64_t)idx;
}

static void arm_recv_classic(struct uring *r, struct conn *c, int idx)
{
    struct io_uring_sqe *sqe = get_sqe(r);
    if (!sqe) die("sq full on arm");
    sqe->opcode = IORING_OP_RECV;
    sqe->fd = c->fd;
    sqe->addr = (uint64_t)(uintptr_t)c->buf;
    sqe->len = (unsigned)g_classic_buf_kb << 10;
    sqe->user_data = (uint64_t)idx;
}

static void *worker(void *argp)
{
    struct worker_arg *wa = argp;
    int tid = wa->tid;
    struct tstat *st = &g_stats[tid];

    cpu_set_t set;
    CPU_ZERO(&set);
    CPU_SET(g_cpu_base + 2 * tid, &set);
    if (sched_setaffinity(0, sizeof(set), &set) != 0)
        fprintf(stderr, "warn: pin tid %d failed\n", tid);

    /* listen */
    int lsock = socket(AF_INET, SOCK_STREAM, 0);
    int one = 1;
    setsockopt(lsock, SOL_SOCKET, SO_REUSEADDR, &one, sizeof(one));
    struct sockaddr_in sa;
    memset(&sa, 0, sizeof(sa));
    sa.sin_family = AF_INET;
    sa.sin_addr.s_addr = INADDR_ANY;
    sa.sin_port = htons((uint16_t)(g_base_port + tid));
    if (bind(lsock, (struct sockaddr *)&sa, sizeof(sa)) < 0) die("bind");
    if (listen(lsock, 64) < 0) die("listen");

    struct uring ring;
    uring_init(&ring, 1024, 65536);

    struct zcrx z;
    memset(&z, 0, sizeof(z));
    if (g_mode_zcrx) {
        unsigned ifindex = if_nametoindex(g_ifname);
        if (!ifindex) die("if_nametoindex");
        zcrx_setup(&ring, &z, ifindex, (unsigned)(g_qid_base + tid));
    }

    atomic_fetch_add(&g_threads_ready, 1);

    int n = g_conns_per_thread;
    struct conn *conns = calloc((size_t)n, sizeof(*conns));
    for (int i = 0; i < n; i++) {
        int fd = accept(lsock, NULL, NULL);
        if (fd < 0) die("accept");
        conns[i].fd = fd;
        conns[i].open = 1;
        if (!g_mode_zcrx) {
            conns[i].buf = aligned_alloc(4096, (size_t)g_classic_buf_kb << 10);
            if (!conns[i].buf) die("alloc conn buf");
            memset(conns[i].buf, 0, (size_t)g_classic_buf_kb << 10);
        }
        atomic_fetch_add(&g_conns_connected, 1);
    }
    close(lsock);

    for (int i = 0; i < n; i++) {
        if (g_mode_zcrx)
            arm_recv_zc(&ring, &z, conns[i].fd, i);
        else
            arm_recv_classic(&ring, &conns[i], i);
    }

    int live = n;
    uint64_t bytes = 0, cqes = 0, rearms = 0, verr = 0;
    unsigned rq_dirty = 0;

    while (live > 0 && !g_stop) {
        unsigned to_submit = flush_sq(&ring);
        int ret = sys_io_uring_enter(ring.fd, to_submit, 1,
                                     IORING_ENTER_GETEVENTS);
        if (ret < 0) {
            if (errno == EINTR) continue;
            die("io_uring_enter");
        }
        unsigned head = *ring.cq_head;
        unsigned tail = __atomic_load_n(ring.cq_tail, __ATOMIC_ACQUIRE);
        unsigned mask = *ring.cq_mask;
        while (head != tail) {
            struct io_uring_cqe *cqe =
                (struct io_uring_cqe *)(ring.cqes + ((size_t)(head & mask) << 5));
            int idx = (int)cqe->user_data;
            cqes++;
            if (g_mode_zcrx) {
                if (cqe->res > 0) {
                    struct io_uring_zcrx_cqe *rc =
                        (struct io_uring_zcrx_cqe *)(cqe + 1);
                    uint64_t off = rc->off & ((1ULL << IORING_ZCRX_AREA_SHIFT) - 1);
                    unsigned char *data = z.area + off;
                    unsigned len = (unsigned)cqe->res;
                    if (data[0] != PATTERN || data[len - 1] != PATTERN)
                        verr++;
                    bytes += len;
                    /* recycle the chunk span */
                    while (z.rq_tail - __atomic_load_n(z.rq_khead,
                                                       __ATOMIC_ACQUIRE) >=
                           z.rq_entries)
                        ; /* 1:1 sizing makes this unreachable; spin if not */
                    struct io_uring_zcrx_rqe *rqe =
                        &z.rqes[z.rq_tail & z.rq_mask];
                    rqe->off = (rc->off & ~IORING_ZCRX_AREA_MASK) | z.area_token;
                    rqe->len = cqe->res;
                    z.rq_tail++;
                    rq_dirty = 1;
                    if (!(cqe->flags & IORING_CQE_F_MORE)) {
                        arm_recv_zc(&ring, &z, conns[idx].fd, idx);
                        rearms++;
                    }
                } else if (cqe->res == 0 && !(cqe->flags & IORING_CQE_F_MORE)) {
                    if (conns[idx].open) { conns[idx].open = 0; live--; }
                } else if (cqe->res < 0) {
                    if (cqe->res == -EAGAIN || cqe->res == -ENOBUFS) {
                        arm_recv_zc(&ring, &z, conns[idx].fd, idx);
                        rearms++;
                    } else {
                        fprintf(stderr, "recv_zc err conn %d: %s\n", idx,
                                strerror(-cqe->res));
                        if (conns[idx].open) { conns[idx].open = 0; live--; }
                    }
                }
            } else {
                if (cqe->res > 0) {
                    unsigned len = (unsigned)cqe->res;
                    unsigned char *data = conns[idx].buf;
                    if (data[0] != PATTERN || data[len - 1] != PATTERN)
                        verr++;
                    bytes += len;
                    arm_recv_classic(&ring, &conns[idx], idx);
                    rearms++;
                } else if (cqe->res == 0) {
                    if (conns[idx].open) { conns[idx].open = 0; live--; }
                } else {
                    fprintf(stderr, "recv err conn %d: %s\n", idx,
                            strerror(-cqe->res));
                    if (conns[idx].open) { conns[idx].open = 0; live--; }
                }
            }
            head++;
            /* publish stats in chunks so the sampler sees progress */
            if ((cqes & 0x3ff) == 0) {
                atomic_store_explicit(&st->bytes, bytes, memory_order_relaxed);
                atomic_store_explicit(&st->cqes, cqes, memory_order_relaxed);
            }
        }
        __atomic_store_n(ring.cq_head, head, __ATOMIC_RELEASE);
        if (rq_dirty) {
            __atomic_store_n(z.rq_ktail, z.rq_tail, __ATOMIC_RELEASE);
            rq_dirty = 0;
        }
    }
    atomic_store_explicit(&st->bytes, bytes, memory_order_relaxed);
    atomic_store_explicit(&st->cqes, cqes, memory_order_relaxed);
    atomic_store_explicit(&st->rearms, rearms, memory_order_relaxed);
    atomic_store_explicit(&st->val_errors, verr, memory_order_relaxed);
    atomic_store_explicit(&st->done, 1, memory_order_relaxed);
    for (int i = 0; i < n; i++)
        if (conns[i].fd >= 0) close(conns[i].fd);
    close(ring.fd);                    /* unregisters the ifq, restarts queue */
    return NULL;
}

/* ------------------------------------------------------------------ main */
static double now_s(void)
{
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (double)ts.tv_sec + (double)ts.tv_nsec / 1e9;
}

int main(int argc, char **argv)
{
    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "--mode") && i + 1 < argc)
            g_mode_zcrx = !strcmp(argv[++i], "zcrx");
        else if (!strcmp(argv[i], "--threads")) g_threads = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--conns")) g_conns_per_thread = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--port")) g_base_port = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--cpu-base")) g_cpu_base = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--qid-base")) g_qid_base = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--ifname")) g_ifname = argv[++i];
        else if (!strcmp(argv[i], "--area-mb")) g_area_mb = atol(argv[++i]);
        else if (!strcmp(argv[i], "--rq-entries")) g_rq_entries = (unsigned)atoi(argv[++i]);
        else if (!strcmp(argv[i], "--rx-buf-len")) g_rx_buf_len = (unsigned)atoi(argv[++i]);
        else if (!strcmp(argv[i], "--max-secs")) g_secs_max = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--thp")) g_thp = 1;
        else if (!strcmp(argv[i], "--buf-kb")) g_classic_buf_kb = atoi(argv[++i]);
        else { fprintf(stderr, "unknown arg %s\n", argv[i]); return 1; }
    }
    signal(SIGPIPE, SIG_IGN);
    alarm((unsigned)g_secs_max);

    g_stats = calloc((size_t)g_threads, sizeof(*g_stats));
    pthread_t *tids = calloc((size_t)g_threads, sizeof(*tids));
    struct worker_arg *was = calloc((size_t)g_threads, sizeof(*was));
    for (int t = 0; t < g_threads; t++) {
        was[t].tid = t;
        if (pthread_create(&tids[t], NULL, worker, &was[t]))
            die("pthread_create");
    }
    while (atomic_load(&g_threads_ready) < g_threads)
        usleep(10000);
    fprintf(stderr, "READY mode=%s threads=%d conns/thread=%d ports=%d..%d qids=%d..%d\n",
            g_mode_zcrx ? "zcrx" : "copy", g_threads, g_conns_per_thread,
            g_base_port, g_base_port + g_threads - 1,
            g_mode_zcrx ? g_qid_base : -1,
            g_mode_zcrx ? g_qid_base + g_threads - 1 : -1);
    fflush(stderr);

    int total_conns = g_threads * g_conns_per_thread;
    while (atomic_load(&g_conns_connected) < total_conns)
        usleep(20000);
    double t0 = now_s();
    fprintf(stderr, "ALL-CONNECTED %d conns, measuring\n", total_conns);

    uint64_t last = 0;
    double last_t = t0;
    int alldone = 0;
    while (!alldone) {
        usleep(1000000);
        uint64_t sum = 0;
        alldone = 1;
        for (int t = 0; t < g_threads; t++) {
            sum += atomic_load_explicit(&g_stats[t].bytes, memory_order_relaxed);
            if (!atomic_load_explicit(&g_stats[t].done, memory_order_relaxed))
                alldone = 0;
        }
        double t1 = now_s();
        printf("SAMPLE t=%.1f GBps=%.3f\n", t1 - t0,
               (double)(sum - last) / (t1 - last_t) / 1e9);
        fflush(stdout);
        last = sum;
        last_t = t1;
    }
    double t1 = now_s();
    for (int t = 0; t < g_threads; t++)
        pthread_join(tids[t], NULL);

    uint64_t bytes = 0, cqes = 0, rearms = 0, verr = 0;
    for (int t = 0; t < g_threads; t++) {
        bytes += atomic_load_explicit(&g_stats[t].bytes, memory_order_relaxed);
        cqes += atomic_load_explicit(&g_stats[t].cqes, memory_order_relaxed);
        rearms += atomic_load_explicit(&g_stats[t].rearms, memory_order_relaxed);
        verr += atomic_load_explicit(&g_stats[t].val_errors, memory_order_relaxed);
    }
    struct rusage ru;
    getrusage(RUSAGE_SELF, &ru);
    double secs = t1 - t0;
    printf("RESULT mode=%s threads=%d conns=%d bytes=%llu secs=%.3f GBps=%.3f "
           "cqes=%llu rearms=%llu val_errors=%llu utime=%.2f stime=%.2f\n",
           g_mode_zcrx ? "zcrx" : "copy", g_threads, total_conns,
           (unsigned long long)bytes, secs, (double)bytes / secs / 1e9,
           (unsigned long long)cqes, (unsigned long long)rearms,
           (unsigned long long)verr,
           (double)ru.ru_utime.tv_sec + (double)ru.ru_utime.tv_usec / 1e6,
           (double)ru.ru_stime.tv_sec + (double)ru.ru_stime.tv_usec / 1e6);
    return verr ? 3 : 0;
}
