/* zcrx_smoke — minimal IORING_REGISTER_ZCRX_IFQ capability probe.
 * Raw syscalls only (no liburing; gcc 8.5-clean).
 *
 * Modes:
 *   zcrx_smoke surface              — opcode/path surface probe: fully
 *       valid args except if_idx=0. ENODEV/ENXIO => the zcrx register
 *       path is present and walked to netdev lookup (the signal);
 *       EINVAL => opcode or arg-shape unknown to this kernel.
 *   zcrx_smoke bind <ifidx> <rxq>   — real registration against a NIC
 *       rx queue (requires CAP_NET_ADMIN, HDS enabled, driver
 *       memory-provider support; the queue is restarted by the bind).
 *       Success => zcrx end-to-end OPEN; the ring is closed
 *       immediately, unbinding the queue.
 *
 * Struct layouts from include/uapi/linux/io_uring.h @ 6.19.14 (also
 * present in 7.1.2's uapi — the probe runs on both kernels).
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <errno.h>
#include <unistd.h>
#include <stdint.h>
#include <sys/mman.h>
#include <sys/syscall.h>

#ifndef __NR_io_uring_setup
#define __NR_io_uring_setup 425
#endif
#ifndef __NR_io_uring_register
#define __NR_io_uring_register 427
#endif

/* io_uring_setup flags */
#define IORING_SETUP_CQE32		(1U << 11)
#define IORING_SETUP_SINGLE_ISSUER	(1U << 12)
#define IORING_SETUP_DEFER_TASKRUN	(1U << 13)
#define IORING_SETUP_NO_SQARRAY		(1U << 16)

#define IORING_REGISTER_ZCRX_IFQ	32

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

struct io_uring_region_desc {
	uint64_t user_addr;
	uint64_t size;
	uint32_t flags;
	uint32_t id;
	uint64_t mmap_offset;
	uint64_t __resv[4];
};
#define IORING_MEM_REGION_TYPE_USER 1

struct io_uring_zcrx_area_reg {
	uint64_t addr;
	uint64_t len;
	uint64_t rq_area_token;
	uint32_t flags;
	uint32_t dmabuf_fd;
	uint64_t __resv2[2];
};
struct io_uring_zcrx_offsets {
	uint32_t head, tail, rqes, __resv2;
	uint64_t __resv[2];
};
struct io_uring_zcrx_ifq_reg {
	uint32_t if_idx;
	uint32_t if_rxq;
	uint32_t rq_entries;
	uint32_t flags;
	uint64_t area_ptr;
	uint64_t region_ptr;
	struct io_uring_zcrx_offsets offsets;
	uint32_t zcrx_id;
	uint32_t __resv2;
	uint64_t __resv[3];
};

int main(int argc, char **argv)
{
	struct io_uring_params p;
	struct io_uring_zcrx_ifq_reg reg;
	struct io_uring_zcrx_area_reg area;
	struct io_uring_region_desc rd;
	void *area_mem, *ring_mem;
	long fd, ret;
	uint32_t ifidx = 0, rxq = 0;
	const char *mode = argc > 1 ? argv[1] : "surface";

	if (!strcmp(mode, "bind")) {
		if (argc < 4) {
			fprintf(stderr,
				"usage: %s bind <ifidx> <rxq>\n", argv[0]);
			return 2;
		}
		ifidx = (uint32_t)atoi(argv[2]);
		rxq = (uint32_t)atoi(argv[3]);
	}

	memset(&p, 0, sizeof(p));
	p.flags = IORING_SETUP_CQE32 | IORING_SETUP_SINGLE_ISSUER |
		  IORING_SETUP_DEFER_TASKRUN;
	fd = syscall(__NR_io_uring_setup, 8, &p);
	if (fd < 0) {
		/* older kernels may reject newer setup flags */
		memset(&p, 0, sizeof(p));
		p.flags = IORING_SETUP_CQE32;
		fd = syscall(__NR_io_uring_setup, 8, &p);
	}
	if (fd < 0) {
		printf("zcrx: io_uring_setup failed: %s\n", strerror(errno));
		return 1;
	}

	area_mem = mmap(NULL, 2 * 1024 * 1024, PROT_READ | PROT_WRITE,
			MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
	ring_mem = mmap(NULL, 64 * 1024, PROT_READ | PROT_WRITE,
			MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
	if (area_mem == MAP_FAILED || ring_mem == MAP_FAILED) {
		perror("mmap");
		return 1;
	}

	memset(&area, 0, sizeof(area));
	area.addr = (uint64_t)(uintptr_t)area_mem;
	area.len = 2 * 1024 * 1024;

	memset(&rd, 0, sizeof(rd));
	rd.user_addr = (uint64_t)(uintptr_t)ring_mem;
	rd.size = 64 * 1024;
	rd.flags = IORING_MEM_REGION_TYPE_USER;

	memset(&reg, 0, sizeof(reg));
	reg.if_idx = ifidx;
	reg.if_rxq = rxq;
	reg.rq_entries = 64;
	reg.area_ptr = (uint64_t)(uintptr_t)&area;
	reg.region_ptr = (uint64_t)(uintptr_t)&rd;

	ret = syscall(__NR_io_uring_register, fd, IORING_REGISTER_ZCRX_IFQ,
		      &reg, 1);
	if (ret == 0) {
		printf("zcrx %s: REGISTER_ZCRX_IFQ SUCCEEDED (ifidx=%u rxq=%u"
		       " rq_entries=%u) — zcrx OPEN end-to-end\n",
		       mode, ifidx, rxq, reg.rq_entries);
		close((int)fd); /* unbinds the ifq / restores the queue */
		return 0;
	}
	printf("zcrx %s: REGISTER_ZCRX_IFQ errno=%d (%s)", mode, errno,
	       strerror(errno));
	if (!strcmp(mode, "surface"))
		printf(" — %s\n",
		       (errno == ENODEV || errno == ENXIO) ?
		       "opcode+path PRESENT (walked to netdev lookup)" :
		       errno == EPERM ?
		       "opcode PRESENT (hit the CAP_NET_ADMIN check; rerun as root)" :
		       "surface ambiguous/absent");
	else
		printf("\n");
	close((int)fd);
	return errno == ENODEV || errno == ENXIO || errno == EPERM ? 0 : 1;
}
