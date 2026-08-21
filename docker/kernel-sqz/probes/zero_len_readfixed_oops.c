// SPDX-License-Identifier: MIT
//
// zero_len_readfixed_oops — minimal reproducer for a NULL pointer
// dereference in iov_iter_alignment_bvec() reached from a ZERO-LENGTH
// IORING_OP_READ_FIXED against an O_DIRECT file.
//
// Found 2026-08-21 by the SqueezeFS test suite on 7.1.8 (a zero-length
// durable clip legitimately asks the device for 0 bytes). The kernel
// oopses and SIGKILLs the SUBMITTING TASK ONLY, so a threaded program
// silently loses one worker and strands whatever that worker owned.
//
//   BUG: kernel NULL pointer dereference, address: 0000000000000008
//   RIP: 0010:iov_iter_alignment_bvec+0xf/0x70
//   Call Trace:
//     iov_iter_alignment
//     btrfs_direct_read / <fs>_file_read_iter
//     __io_read
//     io_read_fixed
//     io_issue_sqe / io_submit_sqes / __do_sys_io_uring_enter
//
// Mechanism (vanilla sources, verified byte-identical to kernel.org):
//   io_uring/rsrc.c io_import_fixed():
//       if (unlikely(!len)) {
//               iov_iter_bvec(iter, ddir, NULL, 0, 0);   // NULL bvec
//               return 0;
//       }
//   lib/iov_iter.c iov_iter_alignment():
//       ubuf arm  -> "if (size) ... ; return 0;"          // guarded
//       bvec arm  -> iov_iter_alignment_bvec(i);          // NOT guarded
//   lib/iov_iter.c iov_iter_alignment_bvec() is a do/while: it reads
//   bvec->bv_len BEFORE testing size, so the NULL bvec is dereferenced
//   at offset 8 (bv_len) => the observed fault address 0x8.
//
// Reachability is per-filesystem: a filesystem is affected iff its
// O_DIRECT read path calls iov_iter_alignment() WITHOUT first returning
// early on a zero-length iter. btrfs does not guard (btrfs_file_read_iter
// -> btrfs_direct_read -> check_direct_read); ext4 does
// (ext4_file_read_iter: "if (!iov_iter_count(to)) return 0;").
//
// Build:
//   gcc -O2 -o zero_len_readfixed_oops zero_len_readfixed_oops.c -luring
// Run (the path selects the filesystem under test):
//   ./zero_len_readfixed_oops /mnt/testfs/probe.bin
//
// WARNING: on an affected kernel this OOPSES the kernel (taint G D, the
// process dies by SIGKILL). Run it on a box you can reboot.

#define _GNU_SOURCE // O_DIRECT

#include <errno.h>
#include <fcntl.h>
#include <liburing.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

#define FILE_BYTES (1u << 20)
#define BUF_BYTES 4096u

static int wait_one(struct io_uring *ring, const char *what) {
  struct io_uring_cqe *cqe;
  int ret = io_uring_wait_cqe(ring, &cqe);
  if (ret < 0) {
    fprintf(stderr, "  %s: io_uring_wait_cqe: %s\n", what, strerror(-ret));
    return ret;
  }
  ret = cqe->res;
  io_uring_cqe_seen(ring, cqe);
  printf("  %s: res=%d%s\n", what, ret,
         ret < 0 ? strerror(-ret) : "");
  fflush(stdout);
  return ret;
}

int main(int argc, char **argv) {
  const char *path = argc > 1 ? argv[1] : "./zero_len_readfixed_probe.bin";
  struct io_uring ring;
  struct io_uring_sqe *sqe;
  struct iovec iov;
  void *buf = NULL;
  int fd, ret;

  // 1. Materialize a file on the filesystem under test.
  fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0644);
  if (fd < 0) {
    perror("open (create)");
    return 1;
  }
  {
    void *fill = calloc(1, FILE_BYTES);
    if (!fill || write(fd, fill, FILE_BYTES) != (ssize_t)FILE_BYTES) {
      perror("write");
      return 1;
    }
    free(fill);
  }
  if (fsync(fd)) {
    perror("fsync");
    return 1;
  }
  close(fd);

  // 2. Reopen O_DIRECT — the alignment check only runs on the DIO path.
  fd = open(path, O_RDONLY | O_DIRECT);
  if (fd < 0) {
    fprintf(stderr, "open O_DIRECT %s: %s\n", path, strerror(errno));
    fprintf(stderr, "(a filesystem without O_DIRECT support cannot be "
                    "affected by this bug)\n");
    return 1;
  }
  printf("file: %s (O_DIRECT ok)\n", path);

  if (posix_memalign(&buf, 4096, BUF_BYTES)) {
    perror("posix_memalign");
    return 1;
  }
  memset(buf, 0, BUF_BYTES);

  ret = io_uring_queue_init(8, &ring, 0);
  if (ret < 0) {
    fprintf(stderr, "io_uring_queue_init: %s\n", strerror(-ret));
    return 1;
  }

  // 3. CONTROL A — zero-length NON-fixed read. Builds a ubuf iter, whose
  //    arm in iov_iter_alignment() IS zero-length guarded. Expect res=0.
  printf("control A: zero-length IORING_OP_READ (non-fixed)\n");
  fflush(stdout);
  sqe = io_uring_get_sqe(&ring);
  io_uring_prep_read(sqe, fd, buf, 0, 0);
  io_uring_submit(&ring);
  wait_one(&ring, "control A");

  // 4. Register the buffer: everything below rides the bvec path.
  iov.iov_base = buf;
  iov.iov_len = BUF_BYTES;
  ret = io_uring_register_buffers(&ring, &iov, 1);
  if (ret < 0) {
    fprintf(stderr, "io_uring_register_buffers: %s\n", strerror(-ret));
    return 1;
  }

  // 5. CONTROL B — a NORMAL-length fixed read. Proves the registered
  //    path itself works on this filesystem. Expect res=4096.
  printf("control B: %u-byte IORING_OP_READ_FIXED\n", BUF_BYTES);
  fflush(stdout);
  sqe = io_uring_get_sqe(&ring);
  io_uring_prep_read_fixed(sqe, fd, buf, BUF_BYTES, 0, 0);
  io_uring_submit(&ring);
  wait_one(&ring, "control B");

  // 6. THE CRASHER — zero-length fixed read: io_import_fixed() installs a
  //    NULL bvec with nr_segs=0/count=0, and the filesystem's DIO
  //    alignment check walks it.
  printf("TRIGGER: zero-length IORING_OP_READ_FIXED "
         "(oopses affected kernels; this task dies by SIGKILL)\n");
  fflush(stdout);
  sqe = io_uring_get_sqe(&ring);
  io_uring_prep_read_fixed(sqe, fd, buf, 0, 0, 0);
  io_uring_submit(&ring);
  ret = wait_one(&ring, "TRIGGER");

  printf("\nSURVIVED: this kernel/filesystem pair is NOT affected "
         "(trigger res=%d)\n", ret);
  io_uring_queue_exit(&ring);
  close(fd);
  return 0;
}
