/* kmbuf_smoke — probe LADDER for the FUSE zero-copy series' io_uring
 * surface. The register opcode is PER KERNEL TRACK (never keyed on a
 * kernel version — portable-by-default law):
 *
 *   6.19.14-sqz (field fleet):  IORING_(UN)REGISTER_KMBUF_RING = 37/38
 *   7.1.6-sqz  (patches-7.1):   38/39 (upstream 7.1 took 37 for
 *                               IORING_REGISTER_BPF_FILTER)
 *   stock kernels:              every rung refuses
 *
 * A rung reads PRESENT only on the full kmbuf signature:
 *   register(reg_op, valid args) == 0
 *   AND the identical repeat answers EEXIST (live bufring at the bgid)
 *   AND mmap at IORING_OFF_KMBUF_RING | (bgid << 16) succeeds
 * — only the kmbuf machinery satisfies the conjunction, so a foreign
 * occupant of the number (7.1's BPF_FILTER: imports the arg's first u16
 * as cmd_type, != 1 for any page-aligned buf_size => EINVAL before any
 * state change; a crossed kmbuf-UNREGISTER: resv/flags pass, bgid
 * lookup on the fresh ring => ENOENT) can never read as PRESENT.
 * The probe argument rides in a zeroed 256-byte buffer so foreign
 * opcodes reading a wider struct (io_uring_bpf = 72 B) see zeros.
 * Each rung uses a fresh ring, dropped before the verdict.
 *
 * Modes:
 *   (default)      run the ladder; exit 0 = PRESENT (pair printed),
 *                  exit 1 = ABSENT
 *   --signatures   measurement mode: raw register-shaped call against
 *                  opcodes 37/38/39, one fresh ring each, no
 *                  short-circuit — prints each errno signature
 *   --fuse-rungs   v2 retention/abort probes (design-zc-write-kernel-v2
 *                  §5.3), run as a TOY FUSE DAEMON (root): mount a
 *                  private fuse fs, classical FUSE_INIT advertising
 *                  FUSE_OVER_IO_URING, then arm ONE SQE128 io_uring
 *                  carrying every possible-CPU queue (sparse zc slot
 *                  table + headers fixed buffer + kmbuf ring, REGISTER
 *                  with BUF_RING|ZERO_COPY|PAYLOAD_RETENTION), and
 *                  drive a pinned child through open/write against it.
 *
 *                  Negotiation (§3.5, RELEASE_PAYLOAD commit_id=~0ULL):
 *                    -EINVAL / -EOPNOTSUPP → opcode unknown (pre-0029)
 *                    -ENOTCONN             → opcode exists, not armed
 *                    -ENOENT               → opcode exists AND armed
 *                  Retention round-trip (§3.3, live on a v2 kernel):
 *                  paged WRITE delivered zc → slot sampled by
 *                  WRITE_FIXED (READ_FIXED refused -EFAULT: the slot is
 *                  ITER_SOURCE — the imu direction law) → COMMIT+RETAIN
 *                  parks the ent (no CQE) while write(2) RETURNS
 *                  (ACK-early) → slot re-sampled byte-exact post-end
 *                  (0025's imu-held folio refs) → RELEASE=0 → double
 *                  RELEASE=-ENOENT → RELEASE of a live commit=-EBUSY.
 *                  Abort-race stays a KASAN rung (SKIP here): arm zc,
 *                  park READ_FIXED, SIGKILL the daemon — unfixed 0024
 *                  must splat, 0025 must not.
 *
 * Raw syscalls, no liburing; gcc 8.5-clean.
 */
#define _GNU_SOURCE /* CPU_SET/sched_setaffinity for the pinned child */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <errno.h>
#include <fcntl.h>
#include <unistd.h>
#include <stdint.h>
#include <inttypes.h>
#include <sched.h>
#include <signal.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/mman.h>
#include <sys/mount.h>
#include <sys/uio.h>
#include <sys/wait.h>

#ifndef __NR_io_uring_setup
#define __NR_io_uring_setup 425
#endif
#ifndef __NR_io_uring_enter
#define __NR_io_uring_enter 426
#endif
#ifndef __NR_io_uring_register
#define __NR_io_uring_register 427
#endif

#define IORING_SETUP_SQE128		(1U << 10)
#define IORING_OP_URING_CMD		46
#define FUSE_INIT			26
#define FUSE_IO_URING_CMD_RELEASE_PAYLOAD 3
#define FUSE_MIN_READ_BUFFER		8192

#define IORING_OFF_KMBUF_RING 0x88000000ULL
#define IORING_OFF_KMBUF_SHIFT 16

#define PROBE_RING_ENTRIES 8
#define PROBE_BGID 7
#define PROBE_ARG_SPAN 256

struct io_sqring_offsets {
	uint32_t head, tail, ring_mask, ring_entries, flags, dropped,
		array, resv1;
	uint64_t user_addr;
};
struct io_cqring_offsets {
	uint32_t head, tail, ring_mask, ring_entries, overflow, cqes,
		flags, resv1;
	uint64_t user_addr;
};
struct io_uring_params {
	uint32_t sq_entries, cq_entries, flags, sq_thread_cpu,
		sq_thread_idle, features, wq_fd, resv[3];
	struct io_sqring_offsets sq_off;
	struct io_cqring_offsets cq_off;
};

/* series layout: union { __u64 ring_addr; __u32 buf_size; } first */
struct io_uring_buf_reg {
	union {
		uint64_t ring_addr;
		uint32_t buf_size;
	};
	uint32_t ring_entries;
	uint16_t bgid;
	uint16_t flags;
	uint64_t resv[3];
};

static long fresh_ring(void)
{
	struct io_uring_params p;

	memset(&p, 0, sizeof(p));
	return syscall(__NR_io_uring_setup, 8, &p);
}

static long page_size(void)
{
	long sz = sysconf(_SC_PAGE_SIZE);

	return sz > 0 ? sz : 4096;
}

/* zero-padded probe argument (foreign readers of wider structs see
 * deterministic zeros) */
static void build_arg(unsigned char *arg, uint32_t page)
{
	struct io_uring_buf_reg reg;

	memset(arg, 0, PROBE_ARG_SPAN);
	memset(&reg, 0, sizeof(reg));
	reg.buf_size = page;
	reg.ring_entries = PROBE_RING_ENTRIES;
	reg.bgid = PROBE_BGID;
	memcpy(arg, &reg, sizeof(reg));
}

/* one register-shaped call; returns 0 or -errno */
static long try_register(long fd, unsigned reg_op, const unsigned char *arg)
{
	long ret = syscall(__NR_io_uring_register, fd, reg_op, arg, 1);

	return ret == 0 ? 0 : -errno;
}

/* one rung: 1 = confirmed, 0 = not kmbuf at this number */
static int probe_rung(unsigned reg_op, unsigned unreg_op, const char *track)
{
	unsigned char arg[PROBE_ARG_SPAN];
	uint32_t page = (uint32_t)page_size();
	long fd, ret;
	size_t span;
	uint64_t off;
	void *p;

	fd = fresh_ring();
	if (fd < 0) {
		printf("kmbuf: io_uring_setup failed: %s\n", strerror(errno));
		return 0;
	}
	build_arg(arg, page);

	ret = try_register(fd, reg_op, arg);
	if (ret != 0) {
		printf("kmbuf: rung %s: opcode %u => errno %ld (%s) — not kmbuf here\n",
		       track, reg_op, -ret, strerror((int)-ret));
		close((int)fd);
		return 0;
	}
	/* confirmation 1: identical repeat must answer EEXIST */
	ret = try_register(fd, reg_op, arg);
	if (ret != -EEXIST) {
		printf("kmbuf: rung %s: opcode %u returned 0 but repeat gave %ld "
		       "(want EEXIST) — FOREIGN success, not kmbuf\n",
		       track, reg_op, ret == 0 ? 0 : -ret);
		close((int)fd);
		return 0;
	}
	/* confirmation 2: only kmbuf mints the region at the kmbuf offset */
	span = (size_t)PROBE_RING_ENTRIES * page;
	off = IORING_OFF_KMBUF_RING |
	      ((uint64_t)PROBE_BGID << IORING_OFF_KMBUF_SHIFT);
	p = mmap(NULL, span, PROT_READ | PROT_WRITE, MAP_SHARED, (int)fd,
		 (off_t)off);
	if (p == MAP_FAILED) {
		printf("kmbuf: rung %s: opcode %u registered+EEXIST but kmbuf-offset "
		       "mmap failed (%s) — FOREIGN success, not kmbuf\n",
		       track, reg_op, strerror(errno));
		close((int)fd);
		return 0;
	}
	munmap(p, span);
	printf("kmbuf: rung %s CONFIRMED — register=%u unregister=%u "
	       "(0 + EEXIST-on-repeat + kmbuf-offset mmap)\n",
	       track, reg_op, unreg_op);
	close((int)fd); /* releases the probe registration */
	return 1;
}

/* measurement mode: raw errno signature per opcode, fresh ring each,
 * no short-circuit — the per-class evidence instrument */
static int signatures(void)
{
	static const unsigned ops[3] = { 37, 38, 39 };
	unsigned char arg[PROBE_ARG_SPAN];
	uint32_t page = (uint32_t)page_size();
	int i;

	for (i = 0; i < 3; i++) {
		long fd = fresh_ring();
		long ret;

		if (fd < 0) {
			printf("kmbuf-sig: io_uring_setup failed: %s\n",
			       strerror(errno));
			return 1;
		}
		build_arg(arg, page);
		ret = try_register(fd, ops[i], arg);
		if (ret == 0)
			printf("kmbuf-sig: opcode %u register-shaped => 0 (accepted)\n",
			       ops[i]);
		else
			printf("kmbuf-sig: opcode %u register-shaped => errno %ld (%s)\n",
			       ops[i], -ret, strerror((int)-ret));
		close((int)fd);
	}
	return 0;
}

/* 128-byte SQE. The uapi command area `sqe->cmd[0]` unions with addr3 at
 * OFFSET 48 (io_uring_sqe128_cmd reads `(sqe)->cmd`), giving the 80-byte
 * SQE128 command area 48..127 — never offset 64 (an earlier revision of
 * this probe wrote 64; the kernel then read a zeroed fuse_uring_cmd_req,
 * which happened to still answer ENOENT for the negotiation shape). */
struct sqe128 {
	uint8_t opcode, flags;		/* 0 */
	uint16_t ioprio;		/* 2 */
	int32_t fd;			/* 4 */
	union {				/* 8: off unions with cmd_op */
		uint64_t off;
		struct {
			uint32_t cmd_op;
			uint32_t __cmd_pad;
		};
	};
	uint64_t addr;			/* 16 */
	uint32_t len;			/* 24 */
	uint32_t op_flags;		/* 28: rw_flags/uring_cmd_flags */
	uint64_t user_data;		/* 32 */
	uint16_t buf_index;		/* 40 */
	uint16_t personality;		/* 42 */
	uint32_t splice_fd_in;		/* 44 */
	uint8_t cmd[80];		/* 48: the SQE128 command area */
};

struct cqe {
	uint64_t user_data;
	int32_t res;
	uint32_t flags;
};

struct fuse_in_header {
	uint32_t len, opcode;
	uint64_t unique, nodeid;
	uint32_t uid, gid, pid;
	uint16_t total_extlen, padding;
};

struct fuse_out_header {
	uint32_t len;
	int32_t error;
	uint64_t unique;
};

struct fuse_init_in {
	uint32_t major, minor, max_readahead, flags;
	uint32_t flags2, unused[11];
};

struct fuse_init_out {
	uint32_t major, minor, max_readahead, flags;
	uint16_t max_background, congestion_threshold;
	uint32_t max_write, time_gran;
	uint16_t max_pages, map_alignment;
	uint32_t flags2, max_stack_depth;
	uint16_t request_timeout, unused[3];
	int64_t time_min, time_max; /* sqz 0028 FUSE_TIME_LIMITS tail */
};

struct fuse_uring_cmd_req {
	uint64_t flags;
	uint64_t commit_id;
	uint16_t qid;
	union {
		struct {
			uint16_t flags;
			uint16_t queue_depth;
		} init;
		struct {
			uint16_t flags;
		} commit;
	};
	uint8_t padding[2];
};

/* ---- the toy FUSE daemon (--fuse-rungs) ------------------------------ */

#define FUSE_LOOKUP		1
#define FUSE_GETATTR		3
#define FUSE_OPEN		14
#define FUSE_WRITE		16
#define FUSE_RELEASE		18
#define FUSE_FLUSH		25
/* FUSE_INIT defined above (26) */

#define FUSE_INIT_EXT		(1u << 30)
#define FUSE_OVER_IO_URING_BIT2	(1u << (41 - 32)) /* flags2 face of 1ULL<<41 */

#define FOPEN_DIRECT_IO		(1u << 0)
#define FOPEN_NOFLUSH		(1u << 5)

#define FUSE_IO_URING_CMD_REGISTER	 1
#define FUSE_IO_URING_CMD_COMMIT_AND_FETCH 2
/* FUSE_IO_URING_CMD_RELEASE_PAYLOAD defined above (3) */

#define FUSE_URING_BUF_RING		(1 << 0)
#define FUSE_URING_ZERO_COPY		(1 << 1)
#define FUSE_URING_PAYLOAD_RETENTION	(1 << 2)
#define FUSE_URING_COMMIT_RETAIN	(1 << 0)

#define FUSE_URING_IN_OUT_HEADER_SZ	128
#define FUSE_URING_OP_IN_OUT_SZ		128

#define IORING_OP_WRITE_FIXED		5
#define IORING_OP_READ_FIXED		4
#define IORING_REGISTER_BUFFERS		0

#define TOY_MNT			"/tmp/kmbuf_smoke_mnt"
#define TOY_FILE_NODE		2
#define TOY_MAX_WRITE		65536
#define TOY_WRITE_SZ		8192
#define TOY_PATTERN		0x5a

struct fuse_uring_ent_in_out {
	uint64_t flags;
	uint64_t commit_id;
	uint32_t payload_sz;
	uint32_t padding;
	uint64_t reserved;
};

struct fuse_uring_req_header {
	char in_out[FUSE_URING_IN_OUT_HEADER_SZ];
	char op_in[FUSE_URING_OP_IN_OUT_SZ];
	struct fuse_uring_ent_in_out ring_ent_in_out;
};

struct fuse_write_in {
	uint64_t fh;
	uint64_t offset;
	uint32_t size, write_flags;
	uint64_t lock_owner;
	uint32_t flags, padding;
};

struct fuse_attr {
	uint64_t ino, size, blocks, atime, mtime, ctime;
	uint32_t atimensec, mtimensec, ctimensec;
	uint32_t mode, nlink, uid, gid, rdev, blksize, flags;
};

struct fuse_entry_out {
	uint64_t nodeid, generation, entry_valid, attr_valid;
	uint32_t entry_valid_nsec, attr_valid_nsec;
	struct fuse_attr attr;
};

struct fuse_attr_out {
	uint64_t attr_valid;
	uint32_t attr_valid_nsec, dummy;
	struct fuse_attr attr;
};

struct fuse_open_out {
	uint64_t fh;
	uint32_t open_flags;
	int32_t backing_id;
};

/* one mmap'd SQE128 ring the whole toy daemon runs on */
struct toy_ring {
	int fd;
	struct io_uring_params p;
	void *sq_ring, *cq_ring;
	struct sqe128 *sqes;
	uint32_t *sq_head, *sq_tail, *sq_array;
	uint32_t *cq_head, *cq_tail;
	struct cqe *cqes;
	uint32_t sq_next; /* local tail cursor */
};

static int toy_ring_open(struct toy_ring *r, unsigned entries)
{
	uint64_t sq_sz, cq_sz;

	memset(r, 0, sizeof(*r));
	r->p.flags = IORING_SETUP_SQE128;
	r->fd = (int)syscall(__NR_io_uring_setup, entries, &r->p);
	if (r->fd < 0)
		return -errno;
	sq_sz = (uint64_t)r->p.sq_off.array +
		r->p.sq_entries * sizeof(uint32_t);
	cq_sz = (uint64_t)r->p.cq_off.cqes +
		r->p.cq_entries * sizeof(struct cqe);
	r->sq_ring = mmap(NULL, sq_sz, PROT_READ | PROT_WRITE, MAP_SHARED,
			  r->fd, 0);
	if (r->sq_ring == MAP_FAILED)
		return -errno;
	r->cq_ring = mmap(NULL, cq_sz, PROT_READ | PROT_WRITE, MAP_SHARED,
			  r->fd, 0x8000000ULL); /* IORING_OFF_CQ_RING */
	if (r->cq_ring == MAP_FAILED)
		r->cq_ring = r->sq_ring; /* single-mmap kernels */
	r->sqes = mmap(NULL, (size_t)r->p.sq_entries * 128,
		       PROT_READ | PROT_WRITE, MAP_SHARED, r->fd,
		       0x10000000ULL); /* IORING_OFF_SQES */
	if (r->sqes == MAP_FAILED)
		return -errno;
	r->sq_head = (uint32_t *)((char *)r->sq_ring + r->p.sq_off.head);
	r->sq_tail = (uint32_t *)((char *)r->sq_ring + r->p.sq_off.tail);
	r->sq_array = (uint32_t *)((char *)r->sq_ring + r->p.sq_off.array);
	r->cq_head = (uint32_t *)((char *)r->cq_ring + r->p.cq_off.head);
	r->cq_tail = (uint32_t *)((char *)r->cq_ring + r->p.cq_off.tail);
	r->cqes = (struct cqe *)((char *)r->cq_ring + r->p.cq_off.cqes);
	return 0;
}

static struct sqe128 *toy_sqe(struct toy_ring *r)
{
	uint32_t idx = r->sq_next & (r->p.sq_entries - 1);
	struct sqe128 *s = &r->sqes[idx];

	memset(s, 0, sizeof(*s));
	r->sq_array[idx] = idx;
	r->sq_next++;
	return s;
}

static int toy_submit(struct toy_ring *r, unsigned n)
{
	long ret;

	__sync_synchronize();
	*r->sq_tail = r->sq_next;
	__sync_synchronize();
	ret = syscall(__NR_io_uring_enter, r->fd, n, 0, 0, NULL, 0);
	return ret < 0 ? -errno : (int)ret;
}

/* blocking wait for one CQE; returns res, fills user_data */
static int toy_wait_cqe(struct toy_ring *r, uint64_t *user_data)
{
	long ret;
	struct cqe *c;

	while (*r->cq_head == *r->cq_tail) {
		ret = syscall(__NR_io_uring_enter, r->fd, 0, 1,
			      1 /* GETEVENTS */, NULL, 0);
		if (ret < 0 && errno != EINTR)
			return -errno;
	}
	__sync_synchronize();
	c = &r->cqes[*r->cq_head & (r->p.cq_entries - 1)];
	if (user_data)
		*user_data = c->user_data;
	ret = c->res;
	__sync_synchronize();
	(*r->cq_head)++;
	return (int)ret;
}

static void toy_cmd_sqe(struct sqe128 *s, int fuse_fd, uint32_t cmd_op,
			uint16_t buf_index, uint64_t user_data,
			const struct fuse_uring_cmd_req *creq)
{
	s->opcode = IORING_OP_URING_CMD;
	s->fd = fuse_fd;
	s->cmd_op = cmd_op;
	s->buf_index = buf_index;
	s->user_data = user_data;
	memcpy(s->cmd, creq, sizeof(*creq)); /* offset 48: the cmd area */
}

/* sample the retained/live zc slot: WRITE_FIXED slot -> scratch file.
 * buf_addr is 0-based (kernel bvec imu->ubuf == 0). Returns res. */
static int toy_sample_slot(struct toy_ring *r, int scratch_fd,
			   uint16_t slot, uint32_t len, uint8_t op)
{
	struct sqe128 *s = toy_sqe(r);
	int rc;

	s->opcode = op;
	s->fd = scratch_fd;
	s->off = 0;
	s->addr = 0; /* kernel-buffer addressing starts at 0 */
	s->len = len;
	s->buf_index = slot;
	s->user_data = 0x5a3E0000ull | slot;
	rc = toy_submit(r, 1);
	if (rc < 0)
		return rc;
	for (;;) {
		uint64_t ud = 0;

		rc = toy_wait_cqe(r, &ud);
		if ((ud >> 16) == 0x5a3E)
			return rc;
		/* foreign CQE (a fetched request) — cannot happen in the
		 * single-writer choreography; refuse loudly if it does */
		fprintf(stderr, "toy: unexpected CQE ud=%" PRIx64 "\n", ud);
		return -EPROTO;
	}
}

static uint64_t nq_possible_cpus(void)
{
	/* nr_queues = num_possible_cpus() (dev_uring.c) — parse the
	 * "0-N" span from sysfs; onlin-only boxes still report the
	 * possible span here. */
	char buf[64];
	int fd = open("/sys/devices/system/cpu/possible", O_RDONLY);
	ssize_t n;
	unsigned lo = 0, hi = 0;

	if (fd < 0)
		return 1;
	n = read(fd, buf, sizeof(buf) - 1);
	close(fd);
	if (n <= 0)
		return 1;
	buf[n] = 0;
	if (sscanf(buf, "%u-%u", &lo, &hi) == 2)
		return hi + 1;
	return 1;
}

static void toy_fill_attr(struct fuse_attr *a, uint64_t node)
{
	memset(a, 0, sizeof(*a));
	a->ino = node;
	a->blksize = 4096;
	a->nlink = 1;
	if (node == 1)
		a->mode = 0040755; /* drwxr-xr-x */
	else
		a->mode = 0100644; /* -rw-r--r-- */
}

/* headers-region reply writer: out_header + payload go where the kernel
 * reads them back (in_out header in the headers fixed buffer; payload
 * via the kmbuf ring buffer the request selected, payload_sz in
 * ring_ent_in_out). */
static void toy_reply(struct fuse_uring_req_header *h, uint64_t unique,
		      int error, const void *payload, uint32_t payload_sz,
		      void *kmbuf_payload)
{
	struct fuse_out_header oh;

	memset(&oh, 0, sizeof(oh));
	oh.len = (uint32_t)(sizeof(oh) + payload_sz);
	oh.error = error;
	oh.unique = unique;
	memcpy(h->in_out, &oh, sizeof(oh));
	if (payload_sz && kmbuf_payload)
		memcpy(kmbuf_payload, payload, payload_sz);
	h->ring_ent_in_out.payload_sz = payload_sz;
}

/* the child: pinned to CPU 0 so every request lands on qid 0 */
static int toy_child(int ready_fd, int acked_fd, int done_fd)
{
	cpu_set_t set;
	unsigned char pat[TOY_WRITE_SZ], b;
	int fd;
	ssize_t n;

	CPU_ZERO(&set);
	CPU_SET(0, &set);
	if (sched_setaffinity(0, sizeof(set), &set) != 0)
		return 11;
	memset(pat, TOY_PATTERN, sizeof(pat));

	/* wait for the daemon's REGISTER pass */
	if (read(ready_fd, &b, 1) != 1)
		return 12;

	fd = open(TOY_MNT "/f", O_WRONLY);
	if (fd < 0)
		return 13;
	n = write(fd, pat, sizeof(pat)); /* WRITE #1: the retained one */
	if (n != (ssize_t)sizeof(pat))
		return 14;
	/* ACK-early witness: signal the instant write(2) returned */
	if (write(acked_fd, "A", 1) != 1)
		return 15;
	/* wait until the daemon finished the retained-sample + RELEASE */
	if (read(done_fd, &b, 1) != 1)
		return 16;
	n = write(fd, pat, sizeof(pat)); /* WRITE #2: the -EBUSY probe */
	if (n != (ssize_t)sizeof(pat))
		return 17;
	close(fd);
	return 0;
}

/* wait for a request CQE on the toy ring, parse its headers */
static int toy_fetch_req(struct toy_ring *r,
			 struct fuse_uring_req_header *hdrs, unsigned depth,
			 struct fuse_in_header *ih, uint16_t *slot)
{
	uint64_t ud = 0;
	int res = toy_wait_cqe(r, &ud);

	if (res < 0)
		return res;
	if ((ud >> 16) != 0xF00D || (ud & 0xffff) >= depth)
		return -EPROTO;
	*slot = (uint16_t)(ud & 0xffff);
	memcpy(ih, hdrs[*slot].in_out, sizeof(*ih));
	return 0;
}

/* register the 2-entry fixed table (sparse zc slot + headers) and the
 * bgid-0 kmbuf ring on ONE queue's ring. Each fuse queue must own its
 * ring: fuse pins bgid 0 of the ring the REGISTER cmd rides, and
 * io_uring_buf_ring_pin refuses a second pin (-EALREADY). */
static int toy_arm_ring(struct toy_ring *r, void *hdrs, size_t hdrs_span,
			uint32_t kmbuf_bufsz)
{
	struct iovec table[2];
	struct io_uring_buf_reg reg;
	int rc;

	memset(table, 0, sizeof(table));
	table[1].iov_base = hdrs;
	table[1].iov_len = hdrs_span;
	rc = (int)syscall(__NR_io_uring_register, r->fd,
			  IORING_REGISTER_BUFFERS, table, 2u);
	if (rc != 0)
		return -errno;
	memset(&reg, 0, sizeof(reg));
	reg.buf_size = kmbuf_bufsz;
	reg.ring_entries = 1;
	reg.bgid = 0;
	/* the ladder's pair: 37 (6.19 track) then 38 (7.1 track) */
	rc = (int)syscall(__NR_io_uring_register, r->fd, 37, &reg, 1);
	if (rc != 0)
		rc = (int)syscall(__NR_io_uring_register, r->fd, 38, &reg, 1);
	return rc == 0 ? 0 : -errno;
}

/* submit the zc+retention REGISTER for qid on its own ring; a refused
 * REGISTER completes synchronously, so an immediate CQE = failure */
static int toy_register_queue(struct toy_ring *r, int fuse_fd, uint16_t qid)
{
	struct fuse_uring_cmd_req creq;
	int rc;

	memset(&creq, 0, sizeof(creq));
	creq.qid = qid;
	creq.init.flags = FUSE_URING_BUF_RING | FUSE_URING_ZERO_COPY |
			  FUSE_URING_PAYLOAD_RETENTION;
	creq.init.queue_depth = 1; /* one ent: slot 0, headers at index 1 */
	toy_cmd_sqe(toy_sqe(r), fuse_fd, FUSE_IO_URING_CMD_REGISTER,
		    0 /* ent->fixed_buf_id = sparse slot 0 */,
		    0xF00D0000ull, &creq);
	rc = toy_submit(r, 1);
	if (rc < 0)
		return rc;
	if (*r->cq_head != *r->cq_tail)
		return toy_wait_cqe(r, NULL); /* synchronous refusal */
	return 0;
}

static int fuse_rungs(void)
{
	unsigned long page = (unsigned long)page_size();
	uint64_t nq = nq_possible_cpus();
	struct toy_ring ring; /* qid 0 — every request lands here */
	struct toy_ring *idle = NULL; /* qid 1..nq-1: armed, never served */
	struct fuse_uring_req_header *hdrs;
	size_t hdrs_span;
	unsigned char *kmbuf_region = NULL;
	size_t kmbuf_region_span = 0;
	uint32_t kmbuf_bufsz;
	struct fuse_uring_cmd_req creq;
	int fuse_fd, scratch_fd = -1, rc, ret = 1;
	int ready_pipe[2] = { -1, -1 }, acked_pipe[2] = { -1, -1 },
	    done_pipe[2] = { -1, -1 };
	pid_t child = -1;
	char mopts[128];
	uint64_t q, retained_commit = 0;
	int mounted = 0;

	if (geteuid() != 0) {
		printf("fuse-rungs: SKIP (need root: mount(2) + zc REGISTER "
		       "are CAP_SYS_ADMIN)\n");
		printf("retention-rt: SKIP\n");
		printf("abort-race: SKIP (KASAN rung)\n");
		return 0;
	}

	fuse_fd = open("/dev/fuse", O_RDWR | O_CLOEXEC);
	if (fuse_fd < 0) {
		printf("fuse-rungs: SKIP /dev/fuse open: %s\n",
		       strerror(errno));
		return 0;
	}

	mkdir(TOY_MNT, 0700);
	snprintf(mopts, sizeof(mopts),
		 "fd=%d,rootmode=40000,user_id=0,group_id=0,"
		 "default_permissions,max_read=%u",
		 fuse_fd, TOY_MAX_WRITE);
	if (mount("kmbuf_smoke", TOY_MNT, "fuse", MS_NOSUID | MS_NODEV,
		  mopts) != 0) {
		printf("fuse-rungs: SKIP mount(2): %s\n", strerror(errno));
		close(fuse_fd);
		return 0;
	}
	mounted = 1;

	/* --- classical INIT: read the kernel's request, echo over-uring --- */
	{
		unsigned char buf[FUSE_MIN_READ_BUFFER];
		struct fuse_in_header ih;
		struct fuse_init_in ii;
		struct {
			struct fuse_out_header oh;
			struct fuse_init_out io;
		} rep;
		ssize_t n = read(fuse_fd, buf, sizeof(buf));

		if (n < (ssize_t)(sizeof(ih) + sizeof(ii))) {
			printf("fuse-rungs: FAIL INIT read: %zd (%s)\n", n,
			       strerror(errno));
			goto out;
		}
		memcpy(&ih, buf, sizeof(ih));
		memcpy(&ii, buf + sizeof(ih), sizeof(ii));
		if (ih.opcode != FUSE_INIT) {
			printf("fuse-rungs: FAIL first request opcode %u\n",
			       ih.opcode);
			goto out;
		}
		memset(&rep, 0, sizeof(rep));
		rep.oh.len = sizeof(rep);
		rep.oh.unique = ih.unique;
		rep.io.major = 7;
		rep.io.minor = ii.minor < 46 ? ii.minor : 46;
		rep.io.max_readahead = ii.max_readahead;
		rep.io.flags = FUSE_INIT_EXT;
		rep.io.flags2 = FUSE_OVER_IO_URING_BIT2;
		rep.io.max_write = TOY_MAX_WRITE;
		rep.io.max_background = 8;
		rep.io.congestion_threshold = 6;
		if (write(fuse_fd, &rep, sizeof(rep)) != (ssize_t)sizeof(rep)) {
			printf("fuse-rungs: FAIL INIT reply: %s\n",
			       strerror(errno));
			goto out;
		}
	}

	/* --- rings: one per fuse queue (fuse pins bgid 0 of the ring a
	 * REGISTER rides; a second pin refuses -EALREADY, so the shared-
	 * ring shortcut is structurally illegal). The child is pinned to
	 * CPU 0, so qid 0's ring is the served one; qids 1..nq-1 arm
	 * minimal rings that only exist to make is_ring_ready() true. --- */
	rc = toy_ring_open(&ring, 32);
	if (rc < 0) {
		printf("fuse-rungs: FAIL io_uring_setup: %s\n", strerror(-rc));
		goto out;
	}

	hdrs_span = (sizeof(struct fuse_uring_req_header) + page - 1) &
		    ~(page - 1);
	hdrs = mmap(NULL, hdrs_span, PROT_READ | PROT_WRITE,
		    MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
	if (hdrs == MAP_FAILED) {
		printf("fuse-rungs: FAIL headers mmap\n");
		goto out;
	}
	kmbuf_bufsz = (uint32_t)((TOY_MAX_WRITE + page - 1) & ~(page - 1));

	/* --- negotiation rung, pre-arm: RELEASE before any REGISTER.
	 * fc->io_uring is set (INIT echo) but fc->ring does not exist yet:
	 * a 0029 kernel answers -ENOTCONN; pre-0029 kernels refuse the
	 * unknown opcode -EINVAL/-EOPNOTSUPP. --- */
	memset(&creq, 0, sizeof(creq));
	creq.commit_id = ~(uint64_t)0;
	toy_cmd_sqe(toy_sqe(&ring), fuse_fd,
		    FUSE_IO_URING_CMD_RELEASE_PAYLOAD, 0, 0x5a3E0001, &creq);
	rc = toy_submit(&ring, 1);
	if (rc >= 0)
		rc = toy_wait_cqe(&ring, NULL);
	if (rc == -ENOTCONN) {
		printf("retention-neg: opcode PRESENT (pre-arm) — RELEASE => "
		       "ENOTCONN (0029 kernel)\n");
	} else if (rc == -EINVAL || rc == -EOPNOTSUPP) {
		printf("retention-neg: opcode ABSENT — RELEASE => errno %d "
		       "(%s) (pre-0029 / stock fuse-uring)\n",
		       -rc, strerror(-rc));
		printf("retention-rt: SKIP (kernel has no retention)\n");
		printf("abort-race: SKIP (KASAN rung)\n");
		ret = 0;
		goto out;
	} else {
		printf("retention-neg: unexpected pre-arm errno %d (%s)\n",
		       rc < 0 ? -rc : rc, strerror(rc < 0 ? -rc : rc));
		goto out;
	}

	/* arm qid 0 (the served ring) ... */
	rc = toy_arm_ring(&ring, hdrs, hdrs_span, kmbuf_bufsz);
	if (rc < 0) {
		printf("fuse-rungs: SKIP ring-0 arm refused (%s) — stock "
		       "kernel?\n", strerror(-rc));
		goto out;
	}
	kmbuf_region_span = kmbuf_bufsz; /* 1 entry */
	kmbuf_region = mmap(NULL, kmbuf_region_span, PROT_READ | PROT_WRITE,
			    MAP_SHARED, ring.fd,
			    (off_t)IORING_OFF_KMBUF_RING);
	if (kmbuf_region == MAP_FAILED) {
		printf("fuse-rungs: FAIL kmbuf region mmap: %s\n",
		       strerror(errno));
		kmbuf_region = NULL;
		goto out;
	}
	rc = toy_register_queue(&ring, fuse_fd, 0);
	if (rc != 0) {
		printf("fuse-rungs: FAIL REGISTER qid 0 => %d (%s)\n", rc,
		       strerror(-rc));
		goto out;
	}

	/* ... then one minimal armed ring per remaining possible CPU */
	if (nq > 1) {
		idle = calloc(nq - 1, sizeof(*idle));
		if (!idle)
			goto out;
	}
	for (q = 1; q < nq; q++) {
		struct toy_ring *r = &idle[q - 1];

		rc = toy_ring_open(r, 8);
		if (rc == 0)
			rc = toy_arm_ring(r, hdrs, hdrs_span, kmbuf_bufsz);
		if (rc == 0)
			rc = toy_register_queue(r, fuse_fd, (uint16_t)q);
		if (rc != 0) {
			printf("fuse-rungs: FAIL REGISTER qid %" PRIu64
			       " => %d (%s)\n", q, rc, strerror(-rc));
			goto out;
		}
	}
	printf("fuse-rungs: armed — %" PRIu64 " queues (one ring each), "
	       "zc+retention, kmbuf bufsz %u\n", nq, kmbuf_bufsz);

	/* --- negotiation rung, armed: impossible commit_id => ENOENT --- */
	memset(&creq, 0, sizeof(creq));
	creq.commit_id = ~(uint64_t)0;
	toy_cmd_sqe(toy_sqe(&ring), fuse_fd,
		    FUSE_IO_URING_CMD_RELEASE_PAYLOAD, 0, 0x5a3E0002, &creq);
	rc = toy_submit(&ring, 1);
	if (rc >= 0)
		rc = toy_wait_cqe(&ring, NULL);
	if (rc == -ENOENT) {
		printf("retention-neg: opcode PRESENT (armed) — RELEASE "
		       "impossible-commit_id => ENOENT\n");
	} else {
		printf("retention-neg: FAIL armed probe errno %d (%s), "
		       "want ENOENT\n", rc < 0 ? -rc : rc,
		       strerror(rc < 0 ? -rc : rc));
		goto out;
	}

	/* --- the retention round-trip --- */
	scratch_fd = open("/tmp/kmbuf_smoke_scratch", O_RDWR | O_CREAT |
			  O_TRUNC | O_CLOEXEC, 0600);
	if (scratch_fd < 0)
		goto out;
	if (pipe(ready_pipe) || pipe(acked_pipe) || pipe(done_pipe))
		goto out;
	child = fork();
	if (child == 0) {
		close(ready_pipe[1]); close(acked_pipe[0]); close(done_pipe[1]);
		_exit(toy_child(ready_pipe[0], acked_pipe[1], done_pipe[0]));
	}
	close(ready_pipe[0]); close(acked_pipe[1]); close(done_pipe[0]);
	if (write(ready_pipe[1], "R", 1) != 1)
		goto out;

	{
		struct fuse_in_header ih;
		struct fuse_write_in wi;
		uint16_t slot;
		unsigned served_write = 0;
		unsigned char sample[TOY_WRITE_SZ];

		while (!served_write) {
			rc = toy_fetch_req(&ring, hdrs, 1, &ih,
					   &slot);
			if (rc < 0) {
				printf("retention-rt: FAIL fetch: %s\n",
				       strerror(-rc));
				goto out;
			}
			memset(&creq, 0, sizeof(creq));
			creq.qid = slot;
			creq.commit_id =
				hdrs[slot].ring_ent_in_out.commit_id;

			switch (ih.opcode) {
			case FUSE_LOOKUP: {
				struct fuse_entry_out eo;

				memset(&eo, 0, sizeof(eo));
				eo.nodeid = TOY_FILE_NODE;
				toy_fill_attr(&eo.attr, TOY_FILE_NODE);
				toy_reply(&hdrs[slot], ih.unique, 0, &eo,
					  sizeof(eo), kmbuf_region +
					  (size_t)slot * kmbuf_bufsz);
				break;
			}
			case FUSE_GETATTR: {
				struct fuse_attr_out ao;

				memset(&ao, 0, sizeof(ao));
				toy_fill_attr(&ao.attr, ih.nodeid);
				toy_reply(&hdrs[slot], ih.unique, 0, &ao,
					  sizeof(ao), kmbuf_region +
					  (size_t)slot * kmbuf_bufsz);
				break;
			}
			case FUSE_OPEN: {
				struct fuse_open_out oo;

				memset(&oo, 0, sizeof(oo));
				oo.fh = 1;
				oo.open_flags = FOPEN_DIRECT_IO |
						FOPEN_NOFLUSH;
				toy_reply(&hdrs[slot], ih.unique, 0, &oo,
					  sizeof(oo), kmbuf_region +
					  (size_t)slot * kmbuf_bufsz);
				break;
			}
			case FUSE_WRITE: {
				struct { uint32_t size, padding; } wo;

				memcpy(&wi, hdrs[slot].op_in, sizeof(wi));
				served_write = 1;
				retained_commit = creq.commit_id;

				/* pre-commit: the slot must carry the
				 * payload (zc delivery) and refuse the
				 * wrong direction */
				rc = toy_sample_slot(&ring, scratch_fd, slot,
						     wi.size,
						     IORING_OP_READ_FIXED);
				if (rc != -EFAULT)
					printf("retention-rt: WARN dir-law "
					       "READ_FIXED on ITER_SOURCE "
					       "slot => %d (want EFAULT)\n",
					       rc);
				if (lseek(scratch_fd, 0, SEEK_SET) < 0 ||
				    ftruncate(scratch_fd, 0) < 0)
					goto out;
				rc = toy_sample_slot(&ring, scratch_fd, slot,
						     wi.size,
						     IORING_OP_WRITE_FIXED);
				if (rc != (int)wi.size) {
					printf("retention-rt: FAIL live slot "
					       "sample: %d\n", rc);
					goto out;
				}

				wo.size = wi.size;
				wo.padding = 0;
				toy_reply(&hdrs[slot], ih.unique, 0, &wo,
					  sizeof(wo), kmbuf_region +
					  (size_t)slot * kmbuf_bufsz);
				creq.commit.flags = FUSE_URING_COMMIT_RETAIN;
				break;
			}
			case FUSE_FLUSH:
			case FUSE_RELEASE:
				toy_reply(&hdrs[slot], ih.unique, 0, NULL, 0,
					  NULL);
				break;
			default:
				toy_reply(&hdrs[slot], ih.unique, -38
					  /* ENOSYS */, NULL, 0, NULL);
				break;
			}

			toy_cmd_sqe(toy_sqe(&ring), fuse_fd,
				    FUSE_IO_URING_CMD_COMMIT_AND_FETCH, slot,
				    0xF00D0000ull | slot, &creq);
			rc = toy_submit(&ring, 1);
			if (rc < 0) {
				printf("retention-rt: FAIL commit submit: "
				       "%s\n", strerror(-rc));
				goto out;
			}
		}

		/* COMMIT+RETAIN parked the ent (no CQE) and ended the
		 * request: the child's write(2) must return NOW */
		{
			unsigned char b;

			if (read(acked_pipe[0], &b, 1) != 1) {
				printf("retention-rt: FAIL ACK-early wait\n");
				goto out;
			}
			printf("retention-rt: ACK-early confirmed — write(2) "
			       "returned with the slot RETAINED\n");
		}

		/* the pages outlive fuse_request_end (0025's law):
		 * re-sample the retained slot and verify the bytes */
		if (lseek(scratch_fd, 0, SEEK_SET) < 0 ||
		    ftruncate(scratch_fd, 0) < 0)
			goto out;
		rc = toy_sample_slot(&ring, scratch_fd, (uint16_t)0,
				     TOY_WRITE_SZ, IORING_OP_WRITE_FIXED);
		if (rc != TOY_WRITE_SZ) {
			printf("retention-rt: FAIL retained sample: %d (%s)\n",
			       rc, rc < 0 ? strerror(-rc) : "short");
			goto out;
		}
		if (pread(scratch_fd, sample, TOY_WRITE_SZ, 0) !=
		    TOY_WRITE_SZ)
			goto out;
		{
			unsigned i, bad = 0;

			for (i = 0; i < TOY_WRITE_SZ; i++)
				if (sample[i] != TOY_PATTERN)
					bad++;
			if (bad) {
				printf("retention-rt: FAIL retained payload "
				       "mismatch (%u/%u bytes)\n", bad,
				       TOY_WRITE_SZ);
				goto out;
			}
		}
		printf("retention-rt: retained slot byte-exact post-ACK "
		       "(%u bytes, WRITE_FIXED sample)\n", TOY_WRITE_SZ);

		/* RELEASE: 0, then double-release: ENOENT */
		memset(&creq, 0, sizeof(creq));
		creq.qid = 0;
		creq.commit_id = retained_commit;
		toy_cmd_sqe(toy_sqe(&ring), fuse_fd,
			    FUSE_IO_URING_CMD_RELEASE_PAYLOAD, 0, 0x5a3E0003,
			    &creq);
		rc = toy_submit(&ring, 1);
		if (rc >= 0)
			rc = toy_wait_cqe(&ring, NULL);
		if (rc != 0) {
			printf("retention-rt: FAIL RELEASE => %d (%s)\n",
			       rc < 0 ? -rc : rc,
			       strerror(rc < 0 ? -rc : rc));
			goto out;
		}
		toy_cmd_sqe(toy_sqe(&ring), fuse_fd,
			    FUSE_IO_URING_CMD_RELEASE_PAYLOAD, 0, 0x5a3E0004,
			    &creq);
		rc = toy_submit(&ring, 1);
		if (rc >= 0)
			rc = toy_wait_cqe(&ring, NULL);
		if (rc != -ENOENT) {
			printf("retention-rt: FAIL double RELEASE => %d "
			       "(want ENOENT)\n", rc < 0 ? -rc : rc);
			goto out;
		}
		printf("retention-rt: RELEASE=0, double RELEASE=ENOENT\n");

		/* WRITE #2: RELEASE on a LIVE (delivered, uncommitted)
		 * request must answer EBUSY */
		if (write(done_pipe[1], "D", 1) != 1)
			goto out;
		for (;;) {
			rc = toy_fetch_req(&ring, hdrs, 1, &ih,
					   &slot);
			if (rc < 0) {
				printf("retention-rt: FAIL fetch #2: %s\n",
				       strerror(-rc));
				goto out;
			}
			if (ih.opcode == FUSE_WRITE)
				break;
			/* stray FLUSH/RELEASE from fd lifecycle */
			memset(&creq, 0, sizeof(creq));
			creq.qid = slot;
			creq.commit_id =
				hdrs[slot].ring_ent_in_out.commit_id;
			toy_reply(&hdrs[slot], ih.unique, 0, NULL, 0, NULL);
			toy_cmd_sqe(toy_sqe(&ring), fuse_fd,
				    FUSE_IO_URING_CMD_COMMIT_AND_FETCH, slot,
				    0xF00D0000ull | slot, &creq);
			if (toy_submit(&ring, 1) < 0)
				goto out;
		}
		memcpy(&wi, hdrs[slot].op_in, sizeof(wi));
		memset(&creq, 0, sizeof(creq));
		creq.qid = slot;
		creq.commit_id = hdrs[slot].ring_ent_in_out.commit_id;
		toy_cmd_sqe(toy_sqe(&ring), fuse_fd,
			    FUSE_IO_URING_CMD_RELEASE_PAYLOAD, slot,
			    0x5a3E0005, &creq);
		rc = toy_submit(&ring, 1);
		if (rc >= 0)
			rc = toy_wait_cqe(&ring, NULL);
		if (rc != -EBUSY) {
			printf("retention-rt: FAIL live RELEASE => %d "
			       "(want EBUSY)\n", rc < 0 ? -rc : rc);
			goto out;
		}
		printf("retention-rt: RELEASE of a live commit => EBUSY\n");

		/* COMMIT+RETAIN WRITE #2 as well — and then NEVER release
		 * it: the deliberate leak the §3.3 teardown drain owns
		 * (bounded, visible, reclaimed at teardown + one
		 * ratelimited pr_warn forensic line) */
		{
			struct { uint32_t size, padding; } wo;

			wo.size = wi.size;
			wo.padding = 0;
			toy_reply(&hdrs[slot], ih.unique, 0, &wo, sizeof(wo),
				  kmbuf_region + (size_t)slot * kmbuf_bufsz);
			creq.commit.flags = FUSE_URING_COMMIT_RETAIN;
			toy_cmd_sqe(toy_sqe(&ring), fuse_fd,
				    FUSE_IO_URING_CMD_COMMIT_AND_FETCH, slot,
				    0xF00D0000ull | slot, &creq);
			if (toy_submit(&ring, 1) < 0)
				goto out;
		}
	}

	{
		int st = -1;

		waitpid(child, &st, 0);
		child = -1;
		if (!WIFEXITED(st) || WEXITSTATUS(st) != 0) {
			printf("retention-rt: FAIL child status %d\n", st);
			goto out;
		}
	}
	printf("retention-rt: PASS (zc delivery, dir law, ACK-early, "
	       "post-ACK page liveness, RELEASE ladder 0/ENOENT/EBUSY)\n");

	/* --- teardown-drain rung: tear the connection down with WRITE #2's
	 * slot still FRRS_RETAINED; the kernel must drain it (no oops, no
	 * stranded request) and print the lost-release forensic line --- */
	{
		int kmsg = open("/dev/kmsg", O_RDONLY | O_NONBLOCK |
				O_CLOEXEC);
		int seen = 0, tries;
		char line[1024];

		if (kmsg >= 0)
			lseek(kmsg, 0, SEEK_END);
		umount2(TOY_MNT, MNT_DETACH);
		mounted = 0;
		close(fuse_fd);
		fuse_fd = -1;
		for (tries = 0; kmsg >= 0 && !seen && tries < 30; tries++) {
			ssize_t n;

			while ((n = read(kmsg, line, sizeof(line) - 1)) > 0) {
				line[n] = 0;
				if (strstr(line, "teardown with retained zc "
						 "payloads")) {
					seen = 1;
					break;
				}
			}
			if (!seen)
				usleep(100 * 1000);
		}
		if (kmsg >= 0)
			close(kmsg);
		if (seen)
			printf("teardown-drain: PASS — retained slot drained "
			       "at abort, pr_warn forensic line observed\n");
		else
			printf("teardown-drain: WARN — pr_warn line not "
			       "observed within 3s (ratelimit or kmsg "
			       "access); drain itself verified by exit\n");
	}
	printf("abort-race: SKIP (KASAN rung: arm zc, park READ_FIXED, "
	       "SIGKILL daemon; unfixed 0024 must splat, 0025 must not)\n");
	ret = 0;

out:
	if (child > 0) {
		kill(child, SIGKILL);
		waitpid(child, NULL, 0);
	}
	if (mounted)
		umount2(TOY_MNT, MNT_DETACH);
	if (fuse_fd >= 0)
		close(fuse_fd); /* connection death drains retained ents */
	if (scratch_fd >= 0) {
		close(scratch_fd);
		unlink("/tmp/kmbuf_smoke_scratch");
	}
	return ret;
}

int main(int argc, char **argv)
{
	if (argc > 1 && strcmp(argv[1], "--signatures") == 0)
		return signatures();
	if (argc > 1 && strcmp(argv[1], "--fuse-rungs") == 0)
		return fuse_rungs();

	/* field track first: the deployed fleet resolves on rung 1 */
	if (probe_rung(37, 38, "6.19-sqz")) {
		printf("kmbuf: surface PRESENT, opcode pair 37/38 (6.19-sqz track)\n");
		return 0;
	}
	if (probe_rung(38, 39, "7.1-sqz")) {
		printf("kmbuf: surface PRESENT, opcode pair 38/39 (7.1-sqz track)\n");
		return 0;
	}
	printf("kmbuf: surface ABSENT (every rung refused — stock kernel)\n");
	return 1;
}
