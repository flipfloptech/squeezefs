# Stuck-request unmount wedge — root cause and fix (fix/unmount-stuck-request)

Closes follow-up 1 of `.benchmarks/2026-07-08-zero-copy-write-path-closing.md`:
the pre-existing intermittent wedge where one kernel request never completes
(`/sys/fs/fuse/connections/N/waiting == 1`), syncfs blocks, plain `umount`
returns EBUSY forever, `fusectl` abort recovers, and the filesystem keeps
serving every other request. Reproduced byte-for-byte on dev @ `ffc5fe0`
(pre-lease) and dev @ `2580006` — never lease-shaped (transport lease stats
clean at wedge time).

Environment: kernel `7.1.3-1-cachyos`, 32 CPUs, unprivileged mounts,
FUSE-over-io_uring armed (32 queues × depth 4), `max_write = 1 MiB`.

## Root cause

**The daemon stopped servicing the classical `/dev/fuse` channel after
FUSE-over-io_uring armed, but the kernel keeps routing traffic there by
design.** Three kernel-side classes land on the classical `fiq` even with the
ring armed (verified in fs/fuse/dev_uring.c and dev.c, v6.14 through v7.1 —
the v7.1 sources match this kernel's behavior):

1. `fuse_io_uring_ops.send_forget = fuse_dev_queue_forget` — every
   FORGET/BATCH_FORGET rides the classical queue, forever.
2. `fuse_io_uring_ops.send_interrupt = fuse_dev_queue_interrupt` — every
   INTERRUPT rides the classical queue, forever.
3. `fuse_send_one` reads `fiq->ops` **without** `fiq->lock`, and the arm-time
   switch is a bare `WRITE_ONCE(fiq->ops, &fuse_io_uring_ops)` — a regular
   request (any opcode, including an async writeback FUSE_WRITE) that races
   the switchover window is queued to the classical `fiq->pending`.
   `fuse_resend` (FUSE_NOTIFY_RESEND) also splices requests directly onto the
   classical pending list.

A real request stranded by class 3 pins `fc->num_waiting` at ≥ 1 → syncfs
blocks (`fuse_sync_fs_writes` waits on the write bucket when it was a
writeback WRITE; the caller's blocked syscall holds the sb reference
otherwise) → `umount` EBUSY forever. Class 1 strands forget links (nlookup
accounting never runs; daemon-side inode bookkeeping leaks) and keeps
`/dev/fuse` POLLIN asserted.

**Why the existing guard never worked:** `drain_classical_stranded` — the
one-shot / 150×20 ms post-arm drain — read with an **8 KiB buffer**. The
kernel requires every `/dev/fuse` read buffer to be at least
`sizeof(fuse_in_header) + sizeof(fuse_write_in) + max_write` (≈ 1 MiB here;
fs/fuse/dev.c `fuse_dev_do_read`), so **every drain read failed with EINVAL
and drained nothing** — silently, because the failure was logged via a
`tracing` warn and the daemon installs no tracing subscriber.

## Live evidence (wedged production mount, conn 70, 23 h old)

- `waiting == 1` persistent for 23 h; syncfs on that mount fine (the stranded
  request was not a writeback WRITE); all other traffic serviced.
- Daemon side clean: gdb dump of the transport showed **pending map empty**,
  all 128 ring ents armed-and-replied, `cqe_errors == 0`, lease counters 0/0.
- `poll(fd)` on the daemon's primary `/dev/fuse` fd from inside the process:
  **POLLIN asserted** — unread classical data.
- gdb-driven `read(fd, buf, 4 MiB)` of the classical queue returned, in order:
  - `OPCODE=3 (GETATTR) unique=4 nodeid=1 uid=1000 pid=111131` — a real
    request stranded at arm time (unique=4 ⇒ the second request ever on the
    connection: the switchover race, class 3);
  - `OPCODE=42 (BATCH_FORGET) unique=366164` carrying **3030 forget entries**
    (class 1 pile-up from 23 h of unlink/evict traffic).
  - a 1 MiB read attempt returned **EINVAL** (kernel minimum-buffer rule) —
    direct proof of the drainer's failure mode.
- Writing a 16-byte ENOSYS reply to `unique=4` dropped `waiting` 1 → 0
  instantly. The wedge was the stranded classical request, nothing else.

Storm-day variant (PR 5/6/7 reports): the same class with an async writeback
FUSE_WRITE stranded in the switchover window explains `sync -f` blocking —
`fuse_sync_fs_writes` waits forever on the write bucket.

Kernel disposition: **not a kernel bug** — libfuse's over-uring
implementation keeps servicing `/dev/fuse` alongside the ring for exactly
this traffic. Ours did not. Daemon-side defect; no kernel workaround needed.

## Fix (io_uring-first compliant)

`third_party/fuse3` (vendored, edited in place):

- **Classical sideband servicer**: after arm, the primary session keeps
  reading `/dev/fuse` — via the existing io_uring `Readv` path
  (`BlockFuseConnection`, fixed-file registration; no classical read/write
  syscalls added) — with full dispatch semantics. FORGET/INTERRUPT/resends
  and switchover stragglers are serviced properly instead of stranded (or
  ENOSYS-bounced, as the old drain did when it predated `max_write`
  negotiation). The request **hot path stays FUSE-over-io_uring only**; the
  sideband handles precisely the traffic the kernel refuses to put on the
  ring.
- One uring worker session per queue **including qid 0** (previously the
  primary doubled as qid 0).
- `drain_classical_stranded` deleted (EINVAL-broken and semantically wrong).
- `classical_inflight` no longer tracks FORGET/BATCH_FORGET uniques (never
  replied; the set leaked).
- FUSE_DESTROY replies route through `write_vectored` *before* pool shutdown
  — closes a latent no-reply gap for classically-delivered DESTROY.
- New stats-inode counter `transport_classical_sideband` (post-arm classical
  deliveries) + `SQUEEZEFS_TRANSPORT_DEBUG=1` per-request transport tracing
  (deliver/reply/commit/park/cqe-err/stale-pending) for future forensics.

## Tests

- New (red → green): `multi_queue_tests::storm::`
  `test_classical_sideband_serviced_and_unmount_clean` — unlink storm (kernel
  FORGET generator), asserts `transport_classical_sideband > 0` and a clean
  unmount. Red on dev: metric absent, sideband stranded.
- `test_single_queue_qdepth4_small_file_storm_no_starvation` (the wedge
  classifier) green.
- Real-mount probes on the fixed binary: bench-shaped (16/16 across
  `-t 10 --large-size 1024 --only large-seq-write` and full `-t 10`),
  storm-shaped dirty-small-file teardown (3/3), and a mount-time arm-race
  stressor hammering per-CPU getattr during REGISTER (6/6) — all
  `waiting == 0`, clean first-attempt unmount, daemon exit, sideband counter
  moving (forgets serviced).

## Gate

- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --check` clean
- `cargo test --all-features -- --test-threads=1` green (every binary)
- `cargo doc --no-deps` — one pre-existing warning
  (`src/meta_backend/alloc.rs`, untouched — same disposition as the PR 5 and
  closing gates)
- `cargo bench --benches -- --test` green (bench smoke)
- `tests/run_loom.sh` green (12 models, incl. `ent_lease_*`)

## Teardown proof (storm suite, release, consecutive)

6/6 consecutive `cargo test --release --test multi_queue_tests storm` runs
green — no `[WEDGE]`, no EBUSY retries, daemon exits — on the box that
previously reproduced the wedge 3/3 after storm workloads.
