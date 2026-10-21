# Porting spec — mainline FUSE-over-io_uring buffer pools and per-file zero-copy (Linux ≥ 7.3)

Scope: the kernel half of the transport's PRIMARY buffer rung, `MainlinePool` (charter §4
amended 2026-10-21, decision 0003 §BE). This spec SUPERSEDES `fuse_kmbuf_zc.md` for the
contract the daemon ships; that document stays as the spec of the TRANSITIONAL rungs
(`BufRing`, `ZeroCopy`) until their owner-ruled deletion. Every claim below cites mainline
`v7.3` (`/build/kernel/linux-mainline-v7.3`, `c03c1a14507d`): `fs/fuse/dev_uring.c`
(`u:`), `fs/fuse/dev_uring_i.h`, `include/uapi/linux/fuse.h` (`h:`), `fs/fuse/file.c`,
`Documentation/filesystems/fuse/uapi/fuse-uapi-io-uring.rst`, and the merge commits
`b45aaabc628b` (pools), `96caf2496e15` (registered pools), `43f8343858eb` (zero-copy),
`767094250c6c` (docs). The daemon's mirror: `src/fuse/abi.zig`, `transport/probes.zig`,
`transport/worker.zig` (`Pool`, `setupMainlinePool`, `prep*Mainline`, `tryHoldPooled`),
`transport/session.zig` (`resolveMode`, `openPosture`), `handler/lifecycle.zig`.

---

## 0. The three strata, the one degrade point

- **Mainline** (7.3+): the INIT offer carries `FUSE_HAS_IO_URING_BUFPOOL` (bit 43) beside
  `FUSE_OVER_IO_URING` whenever `fuse.enable_uring` reads Y when the kernel SENDS the INIT
  (`inode.c:1483`). The daemon writes Y BEFORE `mount(2)`, so the bit is reliable on every
  mount including the first after boot (`session.mount` step 0).
- **Legacy** (7.2-sqz): the kmbuf opcode ladder reads present (`fuse_kmbuf_zc.md` §2.1).
- **Stock** (6.14–7.2): neither.

The lattice (`probes.resolveBufferMode`): bit 43 ⇒ `mainline_pool` whatever the ladder says
(the two UAPIs cannot coexist in one `dev_uring.c`); else kmbuf present ⇒ `zero_copy` /
`buf_ring`; else `user_ents`. The explicit pin `SQUEEZEFS_FUSE_BUFFER_MODE=pool` is refused
loud without the offer. **The opcode collision**: the series' `RELEASE_PAYLOAD` = 3 and
`FEATURES` = 4 are mainline's `ADD_QUEUE` and `ADD_BUFPOOL`; the retention and FEATURES
probes run only on a legacy rung (`probes.legacyProbesApply`), so no mainline kernel sees
opcode 3 with the legacy meaning (it would create queue 0 without zero-copy:
`u:fuse_uring_add_queue`) and no legacy kernel sees it with the mainline one (the seam
`SQUEEZEFS_TEST_MAINLINE_POOLS` proves the latter fails loud: `-ENOTCONN` from
`RELEASE_PAYLOAD`'s "no queue" arm, the typed `MainlinePoolRefused`).

---

## 1. UAPI (uapi 7.46; `h:1264-1370`)

| Constant | Value | Source |
|---|---|---|
| `FUSE_HAS_IO_URING_BUFPOOL` | `1ULL << 43` (init flag, flags2 bit 11) | `h:514`; offered at `inode.c:1484` |
| `FUSE_IO_URING_CMD_ADD_QUEUE` | 3 | `h:1323` |
| `FUSE_IO_URING_CMD_ADD_BUFPOOL` | 4 | `h:1326` |
| `FUSE_URING_ZERO_COPY` (ADD_QUEUE `cmd_req.flags`) | `1 << 0`; the only admitted flag (`FUSE_URING_ADD_QUEUE_FLAGS`) | `h:1335`; `u:25,1555` |
| `FUSE_URING_ENT_ZERO_COPY` (`ent_in_out.flags`) | `1 << 0` | `h:1276` |
| `FOPEN_IO_URING_ZERO_COPY` | `1 << 8` | `h:409` |
| `IORING_URING_CMD_FIXED` (`sqe.uring_cmd_flags`) | `1U << 0` | `include/uapi/linux/io_uring.h:334` |
| `FUSE_KERNEL_MINOR_VERSION` | 46 | `h:286` |

`struct fuse_uring_ent_in_out` (32 B, `h:1279-1295`): `flags u64 @0` ‖ `commit_id u64 @8` ‖
`payload_sz u32 @16` ‖ **`offset u32 @20`** (the series' padding) ‖ `reserved u64 @24`.
The daemon's `abi.fuse_uring_ent_in_out` names the word `offset` on both strata (zero on the
series).

`struct fuse_uring_cmd_req` (40 B, `h:1340-1368`; `abi.fuse_uring_cmd_req_mainline`):
`flags u64 @0` ‖ `commit_id u64 @8` ‖ `qid u16 @16` ‖ `padding[6] @18` ‖ union @24 {
`bufpool { uaddr u64 @24; len u32 @32; reserved u32 @36 }` | `ent_zero_copy_buf_index u16
@24` }. The series' 24-byte struct puts `init_flags`/`queue_depth` at 18/20 — inside
mainline's padding, which the kernel never reads — and leaves the union zero, so a legacy
REGISTER is well-formed on mainline (`abi.zig`'s comptime asserts pin both facts); the
converse is never sent.

---

## 2. The setup dance, per queue (`u:1537-1639`)

1. **`ADD_QUEUE`** (`u:fuse_uring_add_queue`): `qid < nr_queues` else EINVAL; `flags ⊆
   {ZERO_COPY}` else EINVAL; zero-copy without `capable(CAP_SYS_ADMIN)` ⇒ **EPERM**; a queue
   that exists ⇒ **EEXIST** (`fuse_uring_create_queue(…, fail_if_exists = true)`). Completes
   INLINE: `fuse_uring_cmd` returns the verdict (0), never `-EIOCBQUEUED` (`u:1712-1717`).
   A REGISTER on a queue nobody added creates it copying (`u:1518-1523`): the legacy
   per-entry payload mode (`FUSE_PAYLOAD_PER_ENT`), never a pool.
2. **`ADD_BUFPOOL`** (`u:fuse_uring_add_bufpool`): `flags == 0` and `bufpool.reserved == 0`
   else EINVAL; the queue must exist (EINVAL) and not have committed to a payload mode
   (EINVAL: so BEFORE any payload-carrying REGISTER); `nr_bufs = len / ring->max_payload_sz`
   (`≥ 1` else EINVAL) — the kernel splits the region by ITS `max_payload_sz`, which
   `geometry.payloadFor` mirrors exactly (`plan.payload_sz`), so the daemon's buffer `k` is at
   `k × payload_sz`. With `IORING_URING_CMD_FIXED` on the SQE the pool is **registered**:
   `registered_index = sqe->buf_index` is recorded and EVERY later REGISTER and
   COMMIT_AND_FETCH on the queue must carry the flag and that index
   (`u:fuse_uring_cmd_index_ok`, else EINVAL). Inline too.
3. **`REGISTER`** (`u:fuse_uring_create_ring_ent`): `sqe.addr` → `iov[2]`, `iov[0]` the 288 B
   header (`len ≥ 288`), `iov[1]` MUST be `{NULL, 0}` on a pool queue (`payload->iov_base ||
   payload->iov_len` ⇒ EINVAL); `ent_zero_copy_buf_index != 0` on a non-zero-copy queue ⇒
   EINVAL; the entry's zc slot is that index in the ISSUING ring's registered-buffer table.
4. **`COMMIT_AND_FETCH`** as before (`commit_id` the delivered one), plus the FIXED flag and
   the pool index.

**Several rings per queue.** The pool is keyed by offset and imported through the issuing
command's ring (`io_uring_cmd_import_fixed` → the ring's table at `buf_index`), so N rings
serving one queue each register the queue's region at the SAME index (`validate_fixed_range`
checks the address range against that ring's registration). No kernel patch is needed (the
series' 0030 problem — the per-ring buffer ring — does not exist here). An entry's COMMIT must
come from the ring that REGISTERed it (`io_buffer_unregister(ent->cmd …)` targets the commit's
ring; the kernel does not check — the `-EXDEV` hardening is a proposable patch); the
daemon's workers never cross. The daemon: ONE session pool (`worker.Pool`: `depth ×
payload_sz` per queue, page-aligned, THP-hinted, pinned once by the registration), the
queue's first ring (`ring_base == 0`) issues ADD_QUEUE + ADD_BUFPOOL and sets
`Shared.pool_added[qid]`; later rings find it set (the session starts the rings in order).
Table layout per ring: **index 0 = the pool** (`Pool.POOL_INDEX`), **`1 + ent`** = the
entry's zc slot (`worker.zcSlotOf`; the legacy rung keeps `ent` with the headers at
`depth`). `depth` buffers per queue = one per entry across the queue's rings, so the kernel
never parks a fetch for want of a buffer (`u:1319-1323`: a fetch with no free buffer leaves
the cmd parked until one frees).

---

## 3. A delivery (`u:847-918`, `u:952-1049`)

Buffer selection is per REQUEST, not per entry: a request with copyable payload
(`fuse_uring_req_has_copyable_payload`: `in_numargs > 1 || out_numargs` when not zero-copied;
when zero-copied, the non-page args beyond the per-op header and the data) takes a free
buffer (`find_first_bit`) at `fuse_uring_prep_buffer`/`next_req_update_buffer`; one with
none takes nothing; an entry keeps its buffer across consecutive payload-carrying requests
and recycles it when a payload-less one is fetched or none is pending. The header word
`offset` = `payload.iov_base − base_uaddr` when a buffer is attached, else 0 — so **offset
0 is both "buffer 0" and "none"**; harmless, since a request with no buffer never carries a
body nor expects one (its reply is header-only). The daemon reads the window per delivery
(`Group.poolWindow`): within the region else a breach that fails the session
(`transport_pool_offset_breach`); off the `payload_sz` grid ⇒ `transport_pool_offset_
off_boundary` (the mirror's claim, counted not fatal).

`payload_sz` = `cs.ring.copied_sz` (the in-payload the kernel copied into the buffer) plus,
on a zero-copied WRITE, the page arg's size (`u:908-910`) — so a zc WRITE announces its data
length with nothing in the pool. A READ announces 0.

**Zero-copy** (`u:123-134`, `u:799-842`): `can_zero_copy_req = queue->zero_copy &&
args->zero_copy && opcode ∈ {READ, WRITE} && (in_pages || out_pages)`; the request's folios
are `folio_get`'d into a carrier and registered as a bvec at `ent->zero_copy_index`
(`io_buffer_register_bvec`, `IO_BUF_DEST` for READ, `IO_BUF_SOURCE` for WRITE; the carrier's
release callback puts them when the slot is gone and every in-flight fixed op dropped its
node — the series' 0020 law), `FUSE_URING_ENT_ZERO_COPY` set in the header, the folio copy
skipped at delivery and at commit (`cs->skip_folio_copy`). The daemon reads the direction
off the flag + opcode (`zc.mainlineDirOf`): no opcode mirror, no readdir latch (readdir is
never zero-copied here). Slot addressing as before: `imu->ubuf = 0`, so `sqe.addr` is the
byte offset into the slot, `buf_index` = `1 + ent`. Unregistered at `fuse_uring_req_end`
(the COMMIT), `zero_copy_unregister` through the commit's cmd.

**`args->zero_copy`** is `ff->open_flags & FOPEN_IO_URING_ZERO_COPY` (`file.c:608, 1157`),
set only by an OPEN/CREATE reply (`fuse_file_open`: a `no_open` open gets `FOPEN_KEEP_CACHE`
alone). Hence decision 0003 §BE.3 (serve opens) and the RFC (`scripts/kernel/upstream/
fuse-zc-no-open/`).

---

## 4. A reply (`u:1153-1173`, `dev.c:fuse_copy_out_args`)

COMMIT reads the header's `fuse_out_header`, then `ring_in_out.payload_sz` and copies the
out-args from the pool buffer (`fuse_copy_out_args(cs, args, payload_sz)`: uring counts the
payload alone, no out-header; `reqsize < nbytes` EINVAL; a shorter `out_argvar` reply shrinks
the last arg). A zero-copied READ: `payload_sz = n` bytes read, nothing copied — the
daemon's `prefilled` commit after its `READ_FIXED(fd → slot)` (the direct device leg, the
fd-source leg, the memfd bounce leg: `commitReadyZc` unchanged over the shifted slot);
`page_zeroing` zeroes the folio tail past `n` (`fuse_copy_folio` with `skip_folio_copy`).
A zc WRITE's reply (`fuse_write_out`, 16 B) goes through the pool buffer the kernel
selected for it (out_numargs = 1 is copyable payload). A zero-length paged READ reply is
header-only — never a zero-length `READ_FIXED` (the series' law stands).

**The held write** (charter §6.2 item 1): a zc WRITE (SOURCE slot) is held or extracted as
on the legacy rung (`deliverPagedWrite`); a COPIED WRITE (no flag) is held in its POOL
buffer when `zc.holdShape` and the filesystem's gate admit it (`tryHoldPooled`,
`PayloadLease.acquireHeldPooled`: the bytes in hand AND the slot coordinates), the direct
store `WRITE_FIXED(device ← pool)` on `POOL_INDEX` by user address (`prepPoolStore`). The
pool is an ordinary user-registered buffer (`imu->dir` both ways; the entry owns the buffer
until its COMMIT's fetch recycles it), so the only law is the one the zc slot already has:
every pool-sourcing DMA is handler-awaited strictly before the reply exists — the reply's
`fuse_write_out` lands in the same buffer's first 16 bytes at the commit.

---

## 5. Teardown

No retention on this rung (the series' RETAIN/RELEASE are kmbuf-only); every slot is
unregistered at its COMMIT; `fuse_uring_destruct` frees the pools (`u:262-300`). The
daemon's drain is the UserEnts one (synthesize the owed, unmount, join); the pool is unmapped
after every group (its registrations went with the rings).

---

## 6. Tests (the contract, red first)

`abi` "mainline uring ABI pins"; `probes` "the mainline stratum" (the lattice, the gates,
the gauges); `geometry` "rings per queue" (mainline derives 3 with no FEATURES); `zc` "the
mainline direction is the kernel's word"; `worker` "mainline SQE bytes" + "the mainline
pool"; `slot` "a held lease …" (the pooled hold); `lifecycle` "mainline zero-copy per open
file"; `session` "the open posture table", "e2e lattice: the mainline stratum" (the seam:
fail-loud on a legacy kernel, mounts on 7.3) and "e2e mainline pools" (live, skips without
bit 43).
