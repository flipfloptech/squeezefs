/*
 * squeezefs_sdk.h — the SqueezeFS native SDK (libsqueezefs_sdk).
 *
 * The second client kind on the SqueezeFS shared-memory wire: positional-only
 * I/O on arena-backed, 4 KiB-aligned buffers the app LEASES to the daemon for
 * the duration of each op (THE BUFFER-LEASE LAW, below), an io_uring-shaped
 * batch API, strengthen-only per-open durability, and a one-release ABI window.
 * Hand-written; pinned to the library by `tests/rails.zig` (every exported
 * `sqz_*` symbol appears here with the same arity and types, and vice versa)
 * and compiled by the gate's C smoke (`tests/sdk/c_smoke.c`).
 *
 * THE ONE ERROR CONVENTION. Every fallible call returns a NEGATIVE errno
 * (-EBUSY, -EINVAL, …) and never writes errno(3); success is >= 0 (a byte
 * count, a count of entries, 0). Constructors (sqz_session_open, sqz_open,
 * sqz_buf_alloc) return NULL with the cause in sqz_last_error() (the negative
 * errno) and sqz_last_error_text() (the reason line), both thread-local.
 * sqz_strerror() gives the SDK's meaning of a code. A NULL or stale handle is
 * -EINVAL, never a crash in the SDK's own screens. Errno VALUES are the
 * kernel's (a wrong-direction write is -EBADF exactly as pwrite(2) would be).
 *
 * THE BUFFER-LEASE LAW. A buffer is APP_OWNED when allocated. Every SQE or
 * sync op referencing it makes it SUBMITTED until that op's CQE is reaped (or
 * the sync call returns). While SUBMITTED, sqz_buf_free() returns -EBUSY —
 * decided by ONE atomic compare-and-swap, never a deferred free, because a
 * hidden deferral would recycle memory the daemon may still be serving. If
 * the session POISONS (the daemon died, an op deadline passed, this process
 * forked) while a buffer is SUBMITTED, that buffer becomes POISONED: its
 * bytes stay readable, no new op may reference it (-EINVAL), sqz_buf_free()
 * returns 0 and the range is QUARANTINED for the session's life — never
 * recycled, because the daemon may complete into it arbitrarily late.
 * Writing a SUBMITTED or POISONED buffer is a contract violation that can
 * only corrupt the app's own file bytes (the daemon validates every
 * descriptor on a private snapshot). Reading a SUBMITTED read buffer sees a
 * torn serve.
 *
 * POISON. After the session poisons, the ops then in flight complete -EIO
 * (sqz_poll/sqz_wait deliver their CQEs; a sync call returns its prefix or
 * -EIO) and EVERY later call returns -ENOTCONN (constructors NULL with
 * -ENOTCONN). sqz_close() and sqz_session_close() still work, so the app can
 * release its handles and reconnect. Nothing is silently served another way.
 *
 * THREADS. A session's data-path calls may be made from any thread; the
 * batch API is multi-reaper-safe; sqz_open/sqz_close serialize on the
 * session's control lock. A handle is invalid after its close/free.
 *
 * FORK / EXEC / DUP. The child of fork() finds every session poisoned (its
 * sessions are per process; the parent's are untouched). exec() inherits
 * nothing (every descriptor is close-on-exec). The binding belongs to the
 * sqz_file_t, which owns ONE fd (sqz_fd()); dup it and the dup is a plain
 * kernel fd, as with any wrapper.
 */
#ifndef SQUEEZEFS_SDK_H
#define SQUEEZEFS_SDK_H

#include <stddef.h>
#include <stdint.h>
#include <sys/types.h>
#include <time.h>

#ifdef __cplusplus
extern "C" {
#endif

/* ---- the two-part ABI word (sqz_abi() must agree with the header) ----
 * SQZ_ABI_WIRE is the frozen-subset version sent to the daemon, which accepts
 * {current, current - 1} — one release train; a daemon outside that window
 * refuses the session (sqz_session_open returns NULL, -EPROTONOSUPPORT, with
 * both values in sqz_last_error_text()). SQZ_ABI_SDK is this header's own
 * revision; it never reaches the daemon, so additive header changes can
 * never void the window. */
#define SQZ_ABI_WIRE 1u
#define SQZ_ABI_SDK 1u
#define SQZ_ABI_WORD ((SQZ_ABI_WIRE << 16) | SQZ_ABI_SDK)

/* Every buffer is aligned to this (the O_DIRECT LBA / DMA destination grain)
 * and sized in multiples of it internally. */
#define SQZ_BUF_ALIGN 4096u

/* Per-open durability classes — strengthen-only: a handle may raise its
 * class above the mount's data= class, never lower it (so there is no
 * "writeback" value here). O_SYNC/O_DSYNC in oflags raise it to SQZ_D_SYNC. */
#define SQZ_D_MOUNT 0u /* the mount's class governs (the default) */
#define SQZ_D_SYNC 1u  /* every write of this handle is durable before its CQE */

/* Batch opcodes (== the wire's READ / WRITE). */
#define SQZ_OP_PREAD 1u
#define SQZ_OP_PWRITE 2u

/* ---- opaque handles ---- */
typedef struct sqz_session sqz_session_t; /* one shm session on one mount + its arena */
typedef struct sqz_file sqz_file_t;       /* {fd, binding, durability class} */
typedef struct sqz_buf sqz_buf_t;         /* one arena range + its lease */

/* ---- records (sizes pinned below and in the library) ---- */
typedef struct sqz_session_opts {
    uint32_t struct_size;   /* = sizeof(sqz_session_opts_t) */
    uint32_t slots;         /* slot request, 0 = default; clamped to the ring */
    uint64_t arena_bytes;   /* arena request, 0 = the daemon's default; clamped under the cap, never refused */
    uint32_t op_timeout_ms; /* per-op completion deadline, 0 = 30 000; a miss POISONS the session */
    uint32_t _reserved;     /* 0 */
} sqz_session_opts_t;

typedef struct sqz_open_opts {
    uint32_t struct_size; /* = sizeof(sqz_open_opts_t) */
    uint32_t durability;  /* SQZ_D_MOUNT | SQZ_D_SYNC; anything else -EINVAL */
} sqz_open_opts_t;

/* One window of a buffer: [buf_off, buf_off + len) inside buf. */
typedef struct sqz_iovec {
    sqz_buf_t *buf;
    uint64_t buf_off;
    uint64_t len;
} sqz_iovec_t;

/* A submission entry, 48 bytes. user_data is client-private: it never
 * crosses the wire. flags is reserved (0). */
typedef struct sqz_sqe {
    uint8_t opcode; /* SQZ_OP_* */
    uint8_t flags;  /* 0 */
    uint16_t _pad;
    uint32_t len;
    uint64_t offset; /* file offset (positional only) */
    sqz_file_t *file;
    sqz_buf_t *buf;
    uint64_t buf_off; /* op window = [buf_off, buf_off + len) inside buf */
    uint64_t user_data;
} sqz_sqe_t;

/* A completion entry, 16 bytes: res = bytes or -errno. */
typedef struct sqz_cqe {
    uint64_t user_data;
    int64_t res;
} sqz_cqe_t;

/* sqz_fstat's record (statx-derived; architecture-independent, unlike
 * struct stat). */
typedef struct sqz_stat {
    uint64_t dev;
    uint64_t ino;
    uint64_t size;
    uint64_t blocks; /* 512-byte units */
    uint32_t blksize;
    uint32_t nlink;
    uint32_t mode;
    uint32_t uid;
    uint32_t gid;
    uint32_t _pad;
    int64_t atime_sec;
    int64_t mtime_sec;
    int64_t ctime_sec;
    uint32_t atime_nsec;
    uint32_t mtime_nsec;
    uint32_t ctime_nsec;
    uint32_t _pad2;
} sqz_stat_t;

#if defined(__STDC_VERSION__) && __STDC_VERSION__ >= 201112L
_Static_assert(sizeof(sqz_session_opts_t) == 24, "sqz_session_opts_t is 24 bytes");
_Static_assert(sizeof(sqz_open_opts_t) == 8, "sqz_open_opts_t is 8 bytes");
_Static_assert(sizeof(sqz_iovec_t) == 24, "sqz_iovec_t is 24 bytes");
_Static_assert(sizeof(sqz_sqe_t) == 48, "sqz_sqe_t is 48 bytes");
_Static_assert(offsetof(sqz_sqe_t, user_data) == 40, "sqz_sqe_t.user_data at 40");
_Static_assert(sizeof(sqz_cqe_t) == 16, "sqz_cqe_t is 16 bytes");
_Static_assert(sizeof(sqz_stat_t) == 96, "sqz_stat_t is 96 bytes");
#endif

/* ---- version ---- */
/* Static text naming both ABI parts. */
const char *sqz_version(void);
/* The library's two-part ABI word; compare with SQZ_ABI_WORD. */
uint32_t sqz_abi(void);

/* ---- errors (thread-local) ---- */
/* The negative errno of the last constructor that returned NULL on this
 * thread (0 if none). */
int sqz_last_error(void);
/* The reason line of that failure ("" if none); valid until the next
 * failure on this thread. */
const char *sqz_last_error_text(void);
/* Static text for an SDK result code (a negative errno, or 0). */
const char *sqz_strerror(int err);

/* ---- sessions ---- */
/* Connect to the mount at mount_path (its root directory is the credential):
 * HELLO as kind SDK with SQZ_ABI_WIRE and the geometry request, the sealed
 * arena mapped — so sqz_buf_alloc may precede any sqz_open. opts may be NULL
 * (defaults). NULL + sqz_last_error(): -ENOTSUP not a squeezefs mount / not
 * armed, -ENODATA no bootstrap xattr (mount with -o interception),
 * -EPROTONOSUPPORT abi outside the daemon's window, -EAGAIN admission cap,
 * -EACCES the daemon failed OUR authentication, -EINVAL bad struct_size. */
sqz_session_t *sqz_session_open(const char *mount_path, const sqz_session_opts_t *opts);
/* 0; -EBUSY while any file is open or any buffer is SUBMITTED. Unmaps the
 * arena (POISONED ranges with it) and closes the control socket. */
int sqz_session_close(sqz_session_t *s);
/* The granted geometry (0 for a NULL session). */
uint64_t sqz_session_arena_bytes(sqz_session_t *s);
uint32_t sqz_session_slots(sqz_session_t *s);
uint32_t sqz_session_max_op_bytes(sqz_session_t *s);

/* ---- files (the control plane rides the kernel mount) ---- */
/* The real open(2) (O_CLOEXEC forced) then BIND with the durability class.
 * opts may be NULL (SQZ_D_MOUNT). NULL + sqz_last_error(): the kernel's errno
 * for the open; -EINVAL for O_APPEND / O_PATH / O_TMPFILE, a bad durability
 * or struct_size, or the daemon's screen; -EXDEV off the mount; -ENOTCONN. */
sqz_file_t *sqz_open(sqz_session_t *s, const char *path, int oflags, uint32_t mode, const sqz_open_opts_t *opts);
/* 0 or the kernel's; -EBUSY while an op on this handle is SUBMITTED. UNBIND
 * then the real close(2); the kernel's FLUSH/RELEASE are unchanged. */
int sqz_close(sqz_file_t *f);
/* The real fsync(2): identical in every durability class. */
int sqz_fsync(sqz_file_t *f);
int sqz_fstat(sqz_file_t *f, sqz_stat_t *st);
/* The size in bytes, or -errno. */
int64_t sqz_fsize(sqz_file_t *f);
/* The real fd for everything the SDK does not do (locks, xattrs, …). */
int sqz_fd(sqz_file_t *f);
/* The handle's effective class (SQZ_D_*), or -EINVAL. */
int sqz_durability(sqz_file_t *f);

/* ---- buffers (the lease law) ---- */
/* An SQZ_BUF_ALIGN-aligned arena range of len bytes. NULL + sqz_last_error():
 * -ENOMEM no free block of that order (fragmentation or exhaustion — never a
 * hidden heap buffer), -EINVAL len == 0, -ENOTCONN. */
sqz_buf_t *sqz_buf_alloc(sqz_session_t *s, size_t len);
/* 0 and the range returns to the allocator; -EBUSY while SUBMITTED (one CAS,
 * never deferred); 0 for a POISONED buffer with its range quarantined; -EINVAL
 * for a double free. */
int sqz_buf_free(sqz_buf_t *b);
/* The buffer's address (NULL once freed). */
void *sqz_buf_addr(sqz_buf_t *b);
/* The requested length (0 once freed). */
size_t sqz_buf_len(sqz_buf_t *b);

/* ---- sync positional ops: in place, no copy in either direction ----
 * Bytes or -errno (POSIX short-count semantics across chunks): -EINVAL a
 * window past the buffer, a FREED/POISONED buffer or a negative offset;
 * -EBADF the wrong direction; -EAGAIN no slot for the first chunk; -EIO the
 * op in flight at the daemon's death; -ENOTCONN after it. There is no
 * sqz_read/sqz_write without an offset. */
ssize_t sqz_pread(sqz_file_t *f, sqz_buf_t *b, uint64_t buf_off, size_t len, int64_t offset);
ssize_t sqz_pwrite(sqz_file_t *f, sqz_buf_t *b, uint64_t buf_off, size_t len, int64_t offset);
ssize_t sqz_preadv(sqz_file_t *f, const sqz_iovec_t *iov, unsigned iovcnt, int64_t offset);
ssize_t sqz_pwritev(sqz_file_t *f, const sqz_iovec_t *iov, unsigned iovcnt, int64_t offset);

/* ---- the batch API ----
 * sqz_submit: claim and publish n SQEs under ONE doorbell. Returns the count
 * submitted (a prefix when slots run out — backpressure), -EINVAL with
 * nothing submitted on the first bad SQE (validation is atomic over the
 * batch; execution is not), -EAGAIN when not even the first could be placed,
 * -ENOTCONN. Each SQE's buffer is SUBMITTED until its CQE is reaped; an op
 * larger than the session's max_op_bytes is chunked and folded into ONE CQE
 * with short-count semantics. */
int sqz_submit(sqz_session_t *s, const sqz_sqe_t *sqes, unsigned n);
/* The CQEs ready now (0 if none); -EINVAL for max == 0; after a poison the
 * -EIO CQEs of the ops then in flight, then -ENOTCONN. */
int sqz_poll(sqz_session_t *s, sqz_cqe_t *cqes, unsigned max);
/* Park until >= 1 CQE or the timeout (NULL = forever): the count, 0 on the
 * timeout or with nothing in flight, -ENOTCONN once poisoned and drained.
 * Safe from several threads at once. */
int sqz_wait(sqz_session_t *s, sqz_cqe_t *cqes, unsigned max, const struct timespec *timeout);

#ifdef __cplusplus
}
#endif

#endif /* SQUEEZEFS_SDK_H */
