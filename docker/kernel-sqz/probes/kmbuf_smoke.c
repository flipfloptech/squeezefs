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
 *
 * Raw syscalls, no liburing; gcc 8.5-clean.
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <errno.h>
#include <unistd.h>
#include <stdint.h>
#include <sys/syscall.h>
#include <sys/mman.h>

#ifndef __NR_io_uring_setup
#define __NR_io_uring_setup 425
#endif
#ifndef __NR_io_uring_register
#define __NR_io_uring_register 427
#endif

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

int main(int argc, char **argv)
{
	if (argc > 1 && strcmp(argv[1], "--signatures") == 0)
		return signatures();

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
