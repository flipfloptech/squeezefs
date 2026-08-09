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
 *                  §5.3). Negotiation runs after a classical FUSE_INIT
 *                  on /dev/fuse (no REGISTER): RELEASE_PAYLOAD with
 *                  commit_id=~0ULL.
 *                    -EINVAL / -EOPNOTSUPP → opcode unknown (pre-0029)
 *                    -ENOTCONN             → opcode exists, not armed
 *                    -ENOENT               → opcode exists AND a queue
 *                                            is armed (post-arm probe)
 *                  Retention round-trip + abort-race need an armed
 *                  zc+retention queue and a paged request — they SKIP
 *                  here and are the boot-test plan's live rungs
 *                  (evidence note). Under KASAN the abort-race must
 *                  splat on unfixed 0024 and stay silent on 0025.
 *
 * Raw syscalls, no liburing; gcc 8.5-clean.
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <errno.h>
#include <fcntl.h>
#include <unistd.h>
#include <stdint.h>
#include <sys/syscall.h>
#include <sys/mman.h>

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

/* 128-byte SQE: first 64 are the classic sqe, cmd occupies the tail. */
struct sqe128 {
	uint8_t opcode, flags;
	uint16_t ioprio;
	int32_t fd;
	uint32_t cmd_op, pad1;
	uint64_t addr;
	uint32_t len, uring_cmd_flags;
	uint64_t user_data;
	uint16_t buf_index, personality;
	uint32_t splice_fd_in;
	uint64_t addr3;
	uint64_t __pad2;
	/* 80-byte cmd area starting at offset 48? layout: after 64 B classic */
	uint8_t cmd[64];
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

struct fuse_init_in {
	uint32_t major, minor, max_readahead, flags;
	uint32_t flags2, unused[11];
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

/*
 * Classical FUSE_INIT so fuse_uring_cmd gets past !fc->initialized.
 * Then one SQE128 URING_CMD RELEASE_PAYLOAD with an impossible commit_id.
 */
static int fuse_init_dev(int fuse_fd)
{
	struct {
		struct fuse_in_header h;
		struct fuse_init_in init;
	} req;
	unsigned char buf[FUSE_MIN_READ_BUFFER];
	ssize_t n;

	memset(&req, 0, sizeof(req));
	req.h.len = sizeof(req);
	req.h.opcode = FUSE_INIT;
	req.h.unique = 1;
	req.init.major = 7;
	req.init.minor = 40;
	if (write(fuse_fd, &req, sizeof(req)) != (ssize_t)sizeof(req))
		return -errno;
	n = read(fuse_fd, buf, sizeof(buf));
	if (n < 0)
		return -errno;
	if (n < 16)
		return -EPROTO;
	return 0;
}

static int submit_release_cmd(int fuse_fd)
{
	struct io_uring_params p;
	struct fuse_uring_cmd_req creq;
	struct sqe128 *sqes;
	struct cqe *cqes;
	void *sq_ring, *cq_ring;
	uint32_t *sq_tail, *cq_head, *cq_tail, *sq_array;
	uint32_t sq_off_tail, cq_off_head, cq_off_tail, sq_off_array;
	uint32_t cq_off_cqes;
	long fd, ret;
	int err = 0;

	memset(&p, 0, sizeof(p));
	p.flags = IORING_SETUP_SQE128;
	fd = syscall(__NR_io_uring_setup, 8, &p);
	if (fd < 0)
		return -errno;

	sq_off_tail = p.sq_off.tail;
	sq_off_array = p.sq_off.array;
	cq_off_head = p.cq_off.head;
	cq_off_tail = p.cq_off.tail;
	cq_off_cqes = p.cq_off.cqes;

	{
		uint64_t sq_sz = (uint64_t)p.sq_off.array + p.sq_entries * sizeof(uint32_t);
		uint64_t cq_sz = (uint64_t)p.cq_off.cqes + p.cq_entries * sizeof(struct cqe);
		sq_ring = mmap(NULL, sq_sz, PROT_READ | PROT_WRITE, MAP_SHARED,
			       (int)fd, 0);
		if (sq_ring == MAP_FAILED) {
			err = -errno;
			goto out_fd;
		}
		cq_ring = mmap(NULL, cq_sz, PROT_READ | PROT_WRITE, MAP_SHARED,
			       (int)fd, 0x8000000ULL); /* IORING_OFF_CQ_RING */
		if (cq_ring == MAP_FAILED) {
			/* single-mmap kernels: cq shares sq mapping */
			cq_ring = sq_ring;
		}
		sqes = mmap(NULL, (size_t)p.sq_entries * 128,
			    PROT_READ | PROT_WRITE, MAP_SHARED, (int)fd,
			    0x10000000ULL); /* IORING_OFF_SQES */
		if (sqes == MAP_FAILED) {
			err = -errno;
			goto out_maps;
		}
	}

	sq_tail = (uint32_t *)((char *)sq_ring + sq_off_tail);
	sq_array = (uint32_t *)((char *)sq_ring + sq_off_array);
	cq_head = (uint32_t *)((char *)cq_ring + cq_off_head);
	cq_tail = (uint32_t *)((char *)cq_ring + cq_off_tail);
	cqes = (struct cqe *)((char *)cq_ring + cq_off_cqes);

	memset(&creq, 0, sizeof(creq));
	creq.commit_id = ~(uint64_t)0;
	creq.qid = 0;

	memset(&sqes[0], 0, sizeof(sqes[0]));
	sqes[0].opcode = IORING_OP_URING_CMD;
	sqes[0].fd = fuse_fd;
	sqes[0].cmd_op = FUSE_IO_URING_CMD_RELEASE_PAYLOAD;
	sqes[0].user_data = 1;
	/* cmd lives at offset 48 of the 128-byte SQE (after the 48-byte prefix
	 * up to buf_index… actually classic sqe is 64 B; cmd starts at 64). */
	memcpy((char *)&sqes[0] + 64, &creq, sizeof(creq));
	sq_array[0] = 0;
	__sync_synchronize();
	*sq_tail = 1;
	__sync_synchronize();

	ret = syscall(__NR_io_uring_enter, (int)fd, 1, 1, 1 /* GETEVENTS */,
		      NULL, 0);
	if (ret < 0) {
		err = -errno;
		goto out_sqes;
	}
	if (*cq_head == *cq_tail) {
		err = -EAGAIN;
		goto out_sqes;
	}
	err = cqes[*cq_head & (p.cq_entries - 1)].res;
	if (err > 0)
		err = 0;

out_sqes:
	munmap(sqes, (size_t)p.sq_entries * 128);
out_maps:
	if (cq_ring && cq_ring != sq_ring)
		munmap(cq_ring, (size_t)p.cq_off.cqes +
				     p.cq_entries * sizeof(struct cqe));
	munmap(sq_ring, (size_t)p.sq_off.array + p.sq_entries * sizeof(uint32_t));
out_fd:
	close((int)fd);
	return err;
}

static int fuse_rungs(void)
{
	int fuse_fd, rc, init_rc;

	fuse_fd = open("/dev/fuse", O_RDWR | O_CLOEXEC);
	if (fuse_fd < 0) {
		printf("fuse-rungs: SKIP /dev/fuse open: %s\n", strerror(errno));
		printf("retention-rt: SKIP (needs armed zc+retention queue)\n");
		printf("abort-race: SKIP (needs KASAN + SIGKILL during READ_FIXED)\n");
		return 0;
	}

	init_rc = fuse_init_dev(fuse_fd);
	if (init_rc < 0) {
		printf("fuse-rungs: SKIP FUSE_INIT errno %d (%s) — "
		       "need a real fuse mount to pass INIT "
		       "(boot-test plan runs this on the v2 kernel)\n",
		       -init_rc, strerror(-init_rc));
		printf("retention-rt: SKIP (needs armed zc+retention queue)\n");
		printf("abort-race: SKIP (needs KASAN + SIGKILL during READ_FIXED)\n");
		close(fuse_fd);
		return 0;
	}

	rc = submit_release_cmd(fuse_fd);
	close(fuse_fd);

	if (rc == -ENOENT) {
		printf("retention-neg: opcode PRESENT (armed) — RELEASE "
		       "impossible-commit_id => ENOENT\n");
	} else if (rc == -ENOTCONN) {
		printf("retention-neg: opcode PRESENT (not armed) — RELEASE "
		       "after INIT-only => ENOTCONN (handler entered)\n");
	} else if (rc == -EINVAL || rc == -EOPNOTSUPP) {
		printf("retention-neg: opcode ABSENT — RELEASE => errno %d (%s) "
		       "(pre-0029 / stock fuse-uring)\n",
		       -rc, strerror(-rc));
	} else {
		printf("retention-neg: unexpected errno %d (%s)\n",
		       rc < 0 ? -rc : rc, strerror(rc < 0 ? -rc : rc));
	}

	printf("retention-rt: SKIP (needs armed zc+retention queue + paged WRITE; "
	       "boot-test plan: COMMIT+RETAIN, READ_FIXED still reads pages, "
	       "second RELEASE => ENOENT, RELEASE of live commit => EBUSY)\n");
	printf("abort-race: SKIP (needs KASAN dir-build: arm zc, park READ_FIXED, "
	       "SIGKILL daemon; unfixed 0024 must splat, 0025 must not)\n");
	return 0;
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
