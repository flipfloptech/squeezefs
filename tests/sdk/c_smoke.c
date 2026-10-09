/*
 * The SDK's C smoke (charter §12 "the SDK exports a C ABI"; il_sdk §10.2):
 * a C program compiled against `include/squeezefs_sdk.h` and linked with
 * the static library, driven by `c_smoke_main.zig` (which hosts the
 * in-process stub daemon and hands over a "mount" directory, a file in it
 * and the kill switch). Every symbol of the header is called; every size
 * is pinned; the lease law's -EBUSY, the batch API, the death ladder's
 * -EIO then -ENOTCONN.
 *
 * Returns 0, or the line of the first failed check.
 */
#include "squeezefs_sdk.h"

#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

/* The Zig driver: freeze the stub daemon and sever its socket. */
extern void smoke_host_kill(void);

_Static_assert(sizeof(sqz_sqe_t) == 48, "sqe");
_Static_assert(sizeof(sqz_cqe_t) == 16, "cqe");
_Static_assert(offsetof(sqz_sqe_t, user_data) == 40, "user_data");
_Static_assert(sizeof(sqz_iovec_t) == 24, "iovec");
_Static_assert(sizeof(sqz_stat_t) == 96, "stat");

#define CHECK(cond)                                                                        \
    do {                                                                                   \
        if (!(cond)) {                                                                     \
            fprintf(stderr, "c_smoke: line %d: CHECK(%s) failed (last error %d: %s)\n",   \
                    __LINE__, #cond, sqz_last_error(), sqz_last_error_text());             \
            return __LINE__;                                                               \
        }                                                                                  \
    } while (0)

static int reap_all(sqz_session_t *s, sqz_cqe_t *cqes, unsigned want)
{
    unsigned got = 0;
    struct timespec ts = { 2, 0 };
    while (got < want) {
        int n = sqz_wait(s, cqes + got, want - got, &ts);
        if (n <= 0) return -1;
        got += (unsigned)n;
    }
    return 0;
}

int c_smoke_run(const char *mount_dir, const char *file_path)
{
    /* The ABI word agrees with the header. */
    CHECK(sqz_abi() == SQZ_ABI_WORD);
    CHECK((sqz_abi() >> 16) == SQZ_ABI_WIRE);
    CHECK((sqz_abi() & 0xffffu) == SQZ_ABI_SDK);
    CHECK(strstr(sqz_version(), "squeezefs-sdk") != NULL);
    CHECK(strcmp(sqz_strerror(0), "success") == 0);
    CHECK(strstr(sqz_strerror(-EBUSY), "SUBMITTED") != NULL);

    /* NULL handles: -EINVAL everywhere, never a crash. */
    CHECK(sqz_session_close(NULL) == -EINVAL);
    CHECK(sqz_close(NULL) == -EINVAL);
    CHECK(sqz_fsync(NULL) == -EINVAL);
    CHECK(sqz_fstat(NULL, NULL) == -EINVAL);
    CHECK(sqz_fsize(NULL) == -EINVAL);
    CHECK(sqz_fd(NULL) == -EINVAL);
    CHECK(sqz_durability(NULL) == -EINVAL);
    CHECK(sqz_buf_free(NULL) == -EINVAL);
    CHECK(sqz_buf_addr(NULL) == NULL);
    CHECK(sqz_buf_len(NULL) == 0);
    CHECK(sqz_pread(NULL, NULL, 0, 1, 0) == -EINVAL);
    CHECK(sqz_pwrite(NULL, NULL, 0, 1, 0) == -EINVAL);
    CHECK(sqz_preadv(NULL, NULL, 1, 0) == -EINVAL);
    CHECK(sqz_pwritev(NULL, NULL, 1, 0) == -EINVAL);
    CHECK(sqz_submit(NULL, NULL, 1) == -EINVAL);
    CHECK(sqz_poll(NULL, NULL, 1) == -EINVAL);
    CHECK(sqz_wait(NULL, NULL, 1, NULL) == -EINVAL);
    CHECK(sqz_session_arena_bytes(NULL) == 0);
    CHECK(sqz_session_slots(NULL) == 0);
    CHECK(sqz_session_max_op_bytes(NULL) == 0);
    CHECK(sqz_open(NULL, file_path, O_RDWR, 0, NULL) == NULL);
    CHECK(sqz_last_error() == -EINVAL);
    CHECK(sqz_buf_alloc(NULL, 4096) == NULL);
    CHECK(sqz_session_open(NULL, NULL) == NULL);
    CHECK(sqz_last_error() == -EINVAL);

    /* A bad struct_size refuses before any socket. */
    sqz_session_opts_t bad_opts = { .struct_size = 4, .slots = 0, .arena_bytes = 0, .op_timeout_ms = 0, ._reserved = 0 };
    CHECK(sqz_session_open(mount_dir, &bad_opts) == NULL);
    CHECK(sqz_last_error() == -EINVAL);

    /* The session: a geometry request above the default arena, granted. */
    sqz_session_opts_t so = { .struct_size = sizeof so, .slots = 0, .arena_bytes = 32u << 20, .op_timeout_ms = 20000, ._reserved = 0 };
    sqz_session_t *s = sqz_session_open(mount_dir, &so);
    CHECK(s != NULL);
    CHECK(sqz_session_arena_bytes(s) == (32u << 20));
    CHECK(sqz_session_slots(s) > 0);
    CHECK(sqz_session_max_op_bytes(s) == (1u << 20));

    /* Buffers BEFORE any file: 4 KiB-aligned, the requested length. */
    const size_t len = 3 * 4096 + 17;
    sqz_buf_t *b = sqz_buf_alloc(s, len);
    CHECK(b != NULL);
    CHECK(((uintptr_t)sqz_buf_addr(b) % SQZ_BUF_ALIGN) == 0);
    CHECK(sqz_buf_len(b) == len);
    CHECK(sqz_buf_alloc(s, 0) == NULL);
    CHECK(sqz_last_error() == -EINVAL);
    CHECK(sqz_buf_alloc(s, (64u << 20)) == NULL); /* above the 32 MiB arena */
    CHECK(sqz_last_error() == -ENOMEM);

    /* Files: the durability classes, O_APPEND refused, O_SYNC raises. */
    sqz_open_opts_t oo = { .struct_size = sizeof oo, .durability = SQZ_D_MOUNT };
    sqz_file_t *f = sqz_open(s, file_path, O_RDWR, 0, &oo);
    CHECK(f != NULL);
    CHECK(sqz_durability(f) == (int)SQZ_D_MOUNT);
    CHECK(sqz_fd(f) >= 0);
    oo.durability = 7;
    CHECK(sqz_open(s, file_path, O_RDWR, 0, &oo) == NULL);
    CHECK(sqz_last_error() == -EINVAL);
    oo.durability = SQZ_D_SYNC;
    sqz_file_t *fsync_handle = sqz_open(s, file_path, O_RDWR, 0, &oo);
    CHECK(fsync_handle != NULL);
    CHECK(sqz_durability(fsync_handle) == (int)SQZ_D_SYNC);
    CHECK(sqz_fsync(fsync_handle) == 0);
    CHECK(sqz_close(fsync_handle) == 0);
    CHECK(sqz_open(s, file_path, O_WRONLY | O_APPEND, 0, NULL) == NULL);
    CHECK(sqz_last_error() == -EINVAL);
    CHECK(sqz_open(s, "/nonexistent/sqz-smoke", O_RDONLY, 0, NULL) == NULL);
    CHECK(sqz_last_error() == -ENOENT);

    /* pwrite / pread round trip, in place. */
    unsigned char *p = sqz_buf_addr(b);
    for (size_t i = 0; i < len; i++) p[i] = (unsigned char)(i * 7 + 3);
    CHECK(sqz_pwrite(f, b, 0, len, 4096) == (ssize_t)len);
    memset(p, 0, len);
    CHECK(sqz_pread(f, b, 0, len, 4096) == (ssize_t)len);
    for (size_t i = 0; i < len; i++) CHECK(p[i] == (unsigned char)(i * 7 + 3));
    /* base + offset addressing; a window past the buffer is -EINVAL. */
    CHECK(sqz_pread(f, b, 4096, 100, 8192) == 100);
    CHECK(sqz_pread(f, b, 4096, len, 0) == -EINVAL);
    CHECK(sqz_pread(f, b, 0, len, -1) == -EINVAL);
    /* fsync / fstat / fsize on the real fd. */
    CHECK(sqz_fsync(f) == 0);
    sqz_stat_t st;
    CHECK(sqz_fstat(f, &st) == 0);
    CHECK((st.mode & 0170000) == 0100000);
    CHECK(sqz_fsize(f) == (int64_t)st.size);

    /* The batch API: 8 tickets under one doorbell; -EBUSY while SUBMITTED. */
    sqz_buf_t *bufs[8];
    sqz_sqe_t sqes[8];
    for (unsigned i = 0; i < 8; i++) {
        bufs[i] = sqz_buf_alloc(s, 4096);
        CHECK(bufs[i] != NULL);
        memset(sqz_buf_addr(bufs[i]), (int)(i + 1), 4096);
        sqz_sqe_t q = { .opcode = SQZ_OP_PWRITE, .flags = 0, ._pad = 0, .len = 4096, .offset = (uint64_t)i * 4096,
                        .file = f, .buf = bufs[i], .buf_off = 0, .user_data = 100 + i };
        sqes[i] = q;
    }
    CHECK(sqz_submit(s, sqes, 8) == 8);
    for (unsigned i = 0; i < 8; i++) CHECK(sqz_buf_free(bufs[i]) == -EBUSY);
    sqz_cqe_t cqes[8];
    CHECK(reap_all(s, cqes, 8) == 0);
    unsigned seen = 0;
    for (unsigned i = 0; i < 8; i++) {
        CHECK(cqes[i].res == 4096);
        CHECK(cqes[i].user_data >= 100 && cqes[i].user_data < 108);
        seen |= 1u << (cqes[i].user_data - 100);
    }
    CHECK(seen == 0xffu);
    CHECK(sqz_poll(s, cqes, 8) == 0);
    struct timespec zero = { 0, 0 };
    CHECK(sqz_wait(s, cqes, 8, &zero) == 0);
    /* Read the 8 pages back through one chunked op into one buffer. */
    sqz_buf_t *eight = sqz_buf_alloc(s, 8 * 4096);
    CHECK(eight != NULL);
    sqz_sqe_t one = { .opcode = SQZ_OP_PREAD, .flags = 0, ._pad = 0, .len = 8 * 4096, .offset = 0,
                      .file = f, .buf = eight, .buf_off = 0, .user_data = 77 };
    CHECK(sqz_submit(s, &one, 1) == 1);
    CHECK(reap_all(s, cqes, 1) == 0);
    CHECK(cqes[0].user_data == 77);
    CHECK(cqes[0].res == 8 * 4096);
    const unsigned char *ep = sqz_buf_addr(eight);
    for (unsigned i = 0; i < 8; i++)
        for (unsigned j = 0; j < 4096; j++) CHECK(ep[i * 4096 + j] == (unsigned char)(i + 1));
    CHECK(sqz_buf_free(eight) == 0);
    /* After the CQEs: free succeeds once, then -EINVAL; a batch naming a
     * freed buffer is -EINVAL with nothing submitted. */
    for (unsigned i = 0; i < 8; i++) CHECK(sqz_buf_free(bufs[i]) == 0);
    CHECK(sqz_buf_free(bufs[0]) == -EINVAL);
    CHECK(sqz_buf_addr(bufs[0]) == NULL);
    CHECK(sqz_buf_len(bufs[0]) == 0);
    CHECK(sqz_submit(s, sqes, 8) == -EINVAL);
    sqes[0].opcode = 9;
    CHECK(sqz_submit(s, sqes, 1) == -EINVAL);

    /* preadv / pwritev: two windows of one buffer, folded. */
    sqz_iovec_t iov[2] = { { b, 0, 4096 }, { b, 8192, 4096 } };
    CHECK(sqz_pwritev(f, iov, 2, 0) == 8192);
    CHECK(sqz_preadv(f, iov, 2, 0) == 8192);
    CHECK(sqz_preadv(f, iov, 0, 0) == 0);

    /* The wrong direction is -EBADF, kernel-identically (last: the stub
     * host keeps one rights word, that of its latest BIND). */
    sqz_file_t *ro = sqz_open(s, file_path, O_RDONLY, 0, NULL);
    CHECK(ro != NULL);
    CHECK(sqz_pwrite(ro, b, 0, 4096, 0) == -EBADF);
    CHECK(sqz_close(ro) == 0);

    /* Close order: the session refuses -EBUSY while a file is open. */
    CHECK(sqz_session_close(s) == -EBUSY);
    CHECK(sqz_close(f) == 0);
    CHECK(sqz_buf_free(b) == 0);
    CHECK(sqz_session_close(s) == 0);
    return 0;
}

int c_smoke_death(const char *mount_dir, const char *file_path)
{
    sqz_session_t *s = sqz_session_open(mount_dir, NULL);
    CHECK(s != NULL);
    sqz_file_t *f = sqz_open(s, file_path, O_RDWR, 0, NULL);
    CHECK(f != NULL);
    sqz_buf_t *b = sqz_buf_alloc(s, 4096);
    CHECK(b != NULL);
    sqz_buf_t *idle = sqz_buf_alloc(s, 4096);
    CHECK(idle != NULL);
    /* The daemon dies with one op in flight. */
    smoke_host_kill();
    sqz_sqe_t q = { .opcode = SQZ_OP_PREAD, .flags = 0, ._pad = 0, .len = 4096, .offset = 0,
                    .file = f, .buf = b, .buf_off = 0, .user_data = 42 };
    CHECK(sqz_submit(s, &q, 1) == 1);
    sqz_cqe_t cqe;
    struct timespec ts = { 10, 0 };
    CHECK(sqz_wait(s, &cqe, 1, &ts) == 1);
    CHECK(cqe.user_data == 42);
    CHECK(cqe.res == -EIO);
    /* Every later call: -ENOTCONN. */
    CHECK(sqz_wait(s, &cqe, 1, &ts) == -ENOTCONN);
    CHECK(sqz_poll(s, &cqe, 1) == -ENOTCONN);
    CHECK(sqz_submit(s, &q, 1) == -ENOTCONN);
    CHECK(sqz_pread(f, idle, 0, 4096, 0) == -ENOTCONN);
    CHECK(sqz_buf_alloc(s, 4096) == NULL);
    CHECK(sqz_last_error() == -ENOTCONN);
    CHECK(sqz_open(s, file_path, O_RDWR, 0, NULL) == NULL);
    CHECK(sqz_last_error() == -ENOTCONN);
    /* The poisoned buffer stays readable, is refused new ops, and frees to
     * the quarantine (0); the idle one recycles. */
    CHECK(sqz_buf_addr(b) != NULL);
    CHECK(sqz_buf_free(b) == 0);
    CHECK(sqz_buf_free(idle) == 0);
    CHECK(sqz_session_close(s) == -EBUSY);
    CHECK(sqz_close(f) == 0);
    CHECK(sqz_session_close(s) == 0);
    return 0;
}
