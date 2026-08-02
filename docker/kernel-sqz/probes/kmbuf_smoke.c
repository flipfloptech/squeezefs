/* kmbuf_smoke — probe for the FUSE zero-copy series' io_uring surface:
 * IORING_REGISTER_KMBUF_RING (=37, added by the kmbuf patches; the
 * FUSE zc-reply ABI rides it together with FUSE uapi 7.46's
 * fuse_uring_cmd_req.init flags FUSE_URING_BUF_RING/ZERO_COPY).
 *
 * Registers a small kernel-managed buffer ring with fully valid args:
 *   SUCCESS => kmbuf surface PRESENT (the sqz kernel);
 *   EINVAL  => opcode unknown => ABSENT (stock kernels).
 * The ring is dropped on close(fd).
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

#ifndef __NR_io_uring_setup
#define __NR_io_uring_setup 425
#endif
#ifndef __NR_io_uring_register
#define __NR_io_uring_register 427
#endif

#define IORING_REGISTER_KMBUF_RING 37

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

int main(void)
{
	struct io_uring_params p;
	struct io_uring_buf_reg reg;
	long fd, ret;

	memset(&p, 0, sizeof(p));
	fd = syscall(__NR_io_uring_setup, 8, &p);
	if (fd < 0) {
		printf("kmbuf: io_uring_setup failed: %s\n", strerror(errno));
		return 1;
	}

	memset(&reg, 0, sizeof(reg));
	reg.buf_size = 4096;
	reg.ring_entries = 8;
	reg.bgid = 7;

	ret = syscall(__NR_io_uring_register, fd, IORING_REGISTER_KMBUF_RING,
		      &reg, 1);
	if (ret == 0) {
		printf("kmbuf: REGISTER_KMBUF_RING SUCCEEDED — FUSE-zc "
		       "io_uring surface PRESENT\n");
		close((int)fd);
		return 0;
	}
	printf("kmbuf: REGISTER_KMBUF_RING errno=%d (%s) — surface %s\n",
	       errno, strerror(errno),
	       errno == EINVAL ? "ABSENT (opcode unknown)" : "ambiguous");
	close((int)fd);
	return 1;
}
