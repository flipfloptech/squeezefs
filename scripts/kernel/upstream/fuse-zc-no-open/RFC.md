# RFC: fuse: io-uring zero-copy for zero-message opens (`FUSE_IO_URING_ZERO_COPY_DEFAULT`)

To: Miklos Szeredi <mszeredi@redhat.com>, fuse-devel
Cc: Joanne Koong <joannelkoong@gmail.com>, Bernd Schubert <bernd@bsbernd.com>, linux-fsdevel

Base: mainline `v7.3` (`c03c1a14507d`); applies to `fuse/for-next` (the INIT bit below is
a placeholder — bit 46 is the next free one after `FUSE_PASSTHROUGH_V2` (45) in for-next —
and is the maintainer's to allocate). The patch beside this text,
`0001-fuse-io-uring-zero-copy-default-for-zero-message-opens.patch`, is the sketch; the
owner posts it after the daemon's mainline port measured the two postures on 7.3
(`docs/evidence/rung-10-fuse-mainline-uapi.md`).

## The problem

7.3 grants io-uring zero-copy PER OPEN FILE: the server sets `FOPEN_IO_URING_ZERO_COPY` in
the `FUSE_OPEN` / `FUSE_CREATE` reply and `fuse_read_args_fill()` / `fuse_write_args_fill()`
read `args->zero_copy` off `ff->open_flags`. A server that implements zero-message opens —
answers the first `FUSE_OPEN` with `ENOSYS`, so `fc->no_open` latches and every later open
and release of a regular file is local (`fuse_file_open()`: `ff->open_flags =
FOPEN_KEEP_CACHE`, no request) — has no reply to carry the flag, so none of its files can
ever be zero-copied, although the queue was created with `FUSE_URING_ZERO_COPY` and the
server is privileged. The two features are each worth having and today exclude each other.

What zero-message opens are worth (SqueezeFS, 7.2.6 + the kmbuf series, same binary, A/B
on one mount, `docs/evidence/rung-10-dev-perf.md` §10.5): open/close from four threads
6.6× (487 k/s against 74 k/s — the lane's RELEASE work was the bound), open + 4 KiB read +
close ×4 5.6×, one thread 1.2–1.4×; mdtest file creation 1.18×; per open/close pair the
requests go 2 → 1 (the control arm's OPEN 13.2 µs send → end, its RELEASE ≈ 60 µs of
server thread time, asynchronous). On mdtest's census OPEN + RELEASE were 27 % of the
requests (24,000 of 88,018).

What zero-copy is worth (same tree, the held WRITE stored by `WRITE_FIXED(device ←
slot)`, `docs/evidence/rung-10-zero-copy-writes.md`): 1 MiB sequential writes 1.9–2.5×
(13.9 → 27.5 GB/s on four jobs) with io-wq's share of the server's cores 26 % → 2.6 %;
4 KiB random reads 1.04–1.08×; 64 KiB random reads 1.03×.

A server must pick: serve OPEN (one synchronous round trip per open, the RELEASE per last
close) to get zero-copy on every file, or keep opens local and copy every byte through the
pool. For a data-heavy mount the first wins; for a metadata-heavy one (mdtest's
create+close, the open/close loops) the second. The choice is per MOUNT, not per file, and
the kernel can state it in one bit.

## The proposal

An INIT flag the server echoes, `FUSE_IO_URING_ZERO_COPY_DEFAULT`: every open the kernel
performs locally under `fc->no_open` carries `FOPEN_IO_URING_ZERO_COPY` in
`ff->open_flags`, as if a reply had set it. Semantics:

- Honoured only when the connection negotiated `FUSE_OVER_IO_URING` (the flag is about the
  ring's queues); `fuse_uring_args_to_ring()` already falls back to copying when the
  serving queue was not created zero-copy, so a server that echoes it on a copying queue
  loses nothing (`can_zero_copy_req()`: `queue->zero_copy && args->zero_copy`).
- It changes ONLY the default of a zero-message open. A served OPEN/CREATE reply still
  states its own flags — a server that serves opens does not need the bit, and one that
  wants a file copied can serve that one open (`fc->no_open` is latched by the FIRST
  ENOSYS; before it, replies rule).
- Directories are untouched (`FOPEN_IO_URING_ZERO_COPY` is read by the regular-file
  read/write paths only).
- The capability check stays where it is: `FUSE_URING_ZERO_COPY` on `ADD_QUEUE` requires
  `CAP_SYS_ADMIN`; the INIT bit grants nothing a zero-copy queue did not already.

The patch: one `fuse_conn` bit (`io_uring_zc_default`), set in `process_init_reply()` when
the echo carries the flag beside `FUSE_OVER_IO_URING`; one line in `fuse_file_open()`'s
no-open default; the kernel advertises the bit in `fuse_send_init()` beside
`FUSE_HAS_IO_URING_BUFPOOL` so a server can tell. ~25 lines, no UAPI struct change.

## Alternatives considered

1. **Serve OPEN with the flag** (what the SqueezeFS daemon does today on 7.3 under its
   default, `SQUEEZEFS_FUSE_ZC_OPEN=1`): correct, and the per-mount choice stays in the
   server; the cost is the round trip the no-open posture removed (measured above). This
   RFC lets a server keep both.
2. **Grant zero-copy on CREATE alone** (the one reply a no-open connection still sends):
   a created file's first open is zero-copy, a re-opened one copies. A partial answer,
   available today, and what the daemon does when the lever is off.
3. **A per-inode flag** (e.g. in `fuse_attr.flags`): finer than needed and paid on every
   LOOKUP/GETATTR; the mount-wide default covers the measured workloads.
4. **A connection-wide default that also covers served opens** (every reply gets the bit
   unless it says otherwise): changes the meaning of existing replies; rejected.

## Measurement the posting will carry (owed: the 7.3 boot)

The SqueezeFS daemon on `linux-mainline-sqz 7.3.0-1.7` (mainline + 7 unrelated fixes), the
RAM-NVMe matrix (`scripts/perf/quiet.sh ram-sqz`), legs A B B A: `MainlinePool` with
`SQUEEZEFS_FUSE_ZC_OPEN=1` (opens served, every op zero-copy) against `=0` (zero-message
opens, CREATE-only zero-copy) on the data rows (1M seq write ×1/×4, 4k randread QD32 ×1/×4,
4k randwrite QD32 ×4) and the metadata rows (mdtest 4 × 2,000, open/close ×1/×4) — the
gap between the two columns IS the value of this bit: with it a server has the first
column's data rows and the second column's metadata rows at once.
