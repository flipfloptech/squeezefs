# sqz kernel — series manifest (base, message-ids, conflict resolutions)

The sqz kernel = **linux-6.19.14** (kernel.org stable, sha256
`cde8bf6739be4a0777fedbbba5330b8188c55680c45a922a4dfa289cbec6f185`)
+ the 30 patches in `patches/` + the client base config + `config-fragment`,
built `LOCALVERSION=-sqz` → `uname -r` = `6.19.14-sqz`.

**v2 delta (2026-08-04 + 2026-08-09):** the 2026-08-04 scoping campaign
authored **0028** (was 0027: `FUSE_TIME_LIMITS`). The 2026-08-09
zc-write charter (`docs/design-zc-write-kernel-v2.md`) inserted
**0025** (abort-race folio refs) immediately after Koong 0024 and
appended **0029** (payload retention). Old 0025–0027 renumbered
0026–0028 (no hunk overlap; both tracks apply `--fuzz=0`). Selective
zc delivery (**0030**) was **not** built — the §4.3 gate row ran on the
booted 7.1.6-sqz-v2 kernel (2026-08-09): armed WINS the 4 KiB randread
A-B-B-A outright (+40.7 %/+27.8 %, both orders; register/unregister
~0.25 % of cycles) — **refusal FINAL on measurement** (evidence
`.benchmarks/2026-08-09-kernel-zc-write-v2.md` §0030).

## What was taken, exactly

**Joanne Koong, "[PATCH v4 00/25] fuse/io-uring: add kernel-managed
buffer rings and zero-copy"**, 2026-01-16, fetched via `b4 am` from the
lore thread (`t.mbox.gz` of the cover):

* Cover message-id: `20260116233044.1532965-1-joannelkoong@gmail.com`
  (patches N/25 are `20260116233044.1532965-{N+1}-joannelkoong@gmail.com`)
* Lore: <https://lore.kernel.org/all/20260116233044.1532965-1-joannelkoong@gmail.com/>
* Declared base (cover): commit `b71e635feefc` in the io-uring tree =
  mainline `b71e635feefc852405b14620a7fc58c4c80c0f73` (6.19-rc5 era).
* The series is **self-contained**: patches 01–12 are the kmbuf io_uring
  infrastructure, 13–19 the FUSE refactors + kmbuf ring, 20–23 the rsrc
  bvec-registration refactor, 24 the FUSE zero-copy, 25 docs.
* UAPI surface it adds: the `fuse_uring_cmd_req.init.flags` union
  (`FUSE_URING_BUF_RING` / `FUSE_URING_ZERO_COPY` + `queue_depth`;
  the series adds a "7.46" version-history comment but leaves
  `FUSE_KERNEL_MINOR_VERSION` at 45 — negotiation rides the init
  flags, not a minor bump),
  io_uring `IORING_REGISTER_KMBUF_RING=37` / `IORING_UNREGISTER_KMBUF_RING=38`,
  `IORING_OFF_KMBUF_RING`. It adds **no new Kconfig symbols** (rides
  `CONFIG_FUSE_IO_URING` + `CONFIG_IO_URING`).

## Why v4 and why base 6.19.14 (the "latest coherent revision" ruling)

The series lineage after v4 (surveyed on lore, 2026-08-01):

| Series | Latest | Cover message-id | FUSE consumer? |
|---|---|---|---|
| fuse/io-uring kmbuf + zero-copy | **v4, 25 patches, 2026-01-16** | `20260116233044.1532965-1-…` | **yes — the only revision carrying the FUSE-side patches** |
| io_uring: add kernel-managed buffer rings (split-out infra) | v3, 8 patches, 2026-03-06 | `20260306003224.3620942-1-…` | no (API evolved: `IOU_PBUF_RING_KERNEL_MANAGED` folded into pbuf reg; no reposted FUSE consumer) |
| io_uring: extend bvec registration (split-out infra) | v7, 4 patches, 2026-06-12 | `20260612184840.4058966-1-…` | no |

Taking the evolved split-out infra (kmbuf v3 / bvec v7) without a
reposted FUSE consumer would have required hand-porting FUSE core onto
a changed API — banned ("NEVER hand-hack FUSE core logic silently").
**v4 is the latest coherent revision**, so v4 it is, on the base family
it targets.

Base choice: the v4 series applies **cleanly (25/25, zero conflicts)**
on its declared base `b71e635feefc` (verified). The 7.1.x line was
tried first (client parity) and rejected honestly: 7.1.5 has rewritten
the exact files the series touches (`fs/fuse/dev.c` +257/-, `dev_uring.c`
+147/-, io_uring kbuf/rsrc/uapi heavily), and patch 1 already needed a
structural merge — continuing would have been silent FUSE-core hacking.
6.19.14 (the base's own stable line, EOL'd with .14) carries ≥6.17 mlx5
HDS (`ETHTOOL_RING_USE_TCP_DATA_SPLIT` in `en_ethtool.c`),
`mlx5e_queue_mgmt_ops`, and the full zcrx surface
(`IORING_REGISTER_ZCRX_IFQ`/`_CTRL`, `IORING_OP_RECV_ZC`) — everything
frontier cell 1 needs. The sqz kernel is a **frontier vehicle**, not the
client's daily driver; the ELRepo 7.1.2 kernel remains the permanent
grub default.

## Transplant onto 6.19.14: every conflict, honestly

`git cherry-pick` of the 25 patches onto v6.19.14 hit **3 conflict
sites** (6.19.y stable fixes landed after rc5) + **1 seam** found by
audit. Nothing in FUSE core conflicted — all 7 FUSE patches (13–19, 24)
applied clean.

1. **patch 01, `io_uring/kbuf.c` (`io_register_pbuf_ring`)** — stable
   made `io_buffer_add_list()` return int (xa_store failure) with a
   free-region+bl error path; the series refactors the function into
   helpers. Resolution: series structure kept, stable's return check +
   cleanup preserved in the refactored caller.
2. **patch 05, `io_uring/kbuf.c`** — (a) `io_should_commit()`: series
   adds the `bl` param + kernel-managed early-return; stable had
   switched the opcode test to `io_is_uring_cmd(req)`. Both kept.
   (b) `io_ring_buffer_select()`: stable added the
   `io_kbuf_commit()==false → REQ_F_BUF_MORE` incremental-consumption
   fix; series changes the `io_should_commit` call signature. Both kept.
3. **patch 20, `drivers/block/ublk_drv.c`** — stable moved the
   `io_buffer_unregister_bvec()` call earlier (before
   `ublk_need_complete_req()`); the rename patch edits the old location.
   Resolution: rename applied at the moved location, no re-insertion at
   the old one.
4. **patch 26 (docs; was 25)** — Koong "docs: fuse: add io-uring bufring
   and zero-copy documentation". Renumbered 0025 → 0026; no hunk overlap
   with new 0025 (docs-only vs `dev_uring.c`/`rsrc.c`/`cmd.h`).
5. **patch 27 (was 26; sqz-authored seam commit)** — the kmbuf register
   path (patch 03) inherits the pre-stable unchecked `io_buffer_add_list()`
   call. Extends the same stable error handling to
   `io_register_kmbuf_ring()` (free internal ring struct + buffers
   region + bl; bl not yet xarray-visible so no double-free). Commit
   message carries the full rationale. Renumbered 0026 → 0027 when
   0025 (abort-race) was inserted; no hunk overlap with 0025.
6. **patch 28 (was 27; sqz-authored, v2 — `FUSE_TIME_LIMITS`)** — no
   upstream original (the V2-CANDIDATES.md candidate-5 sketch, authored
   2026-08-04): `fuse_init_out` carves `time_min`/`time_max` i64s out
   of `unused[11]` (→ `unused[3]` placed FIRST so the i64s stay
   naturally aligned and the struct stays 64 bytes), init flag
   `FUSE_TIME_LIMITS (1ULL << 62)` (deliberately far above upstream's
   bit-42 watermark), kernel advertises it in `fuse_send_init`, and
   `process_init_reply` applies `sb->s_time_min/max` beside the
   existing `time_gran` block when the daemon echoes the flag with a
   nonzero `time_max`. Applies fuzz=0 on the fully-patched tree; zero
   overlap with the series' FUSE hunks (different functions). Design
   precedent: djwong's fuse-iomap `FUSE_IOMAP_CONFIG_TIME`.
   Renumbered 0027 → 0028 with the 0025 insert.
7. **patch 25 (NEW 2026-08-09; sqz-authored — abort-race folio refs)** —
   inserted IMMEDIATELY after Koong 0024. `io_buffer_register_bvec()`
   grows `release`/`priv` (patch 0022's optional-callback machinery;
   same shape as `io_buffer_register_request()`).
   `fuse_uring_set_up_zero_copy()` `folio_get()`s into a GFP_KERNEL_ACCOUNT
   carrier; the imu release callback puts them when the last rsrc node
   drops. Closes three windows: abort-without-unregister UAF, in-flight
   FIXED I/O across COMMIT, and the retention (0029) steady state.
   Teardown does **not** gain an unregister (no uring-cmd issue context).
   Files: `fs/fuse/dev_uring.c`, `io_uring/rsrc.c`,
   `include/linux/io_uring/cmd.h`. Zero overlap with 0026–0028.
8. **patch 29 (NEW 2026-08-09; sqz-authored — zc payload retention)** —
   Approach B ACK-early accelerator. uapi: `FUSE_IO_URING_CMD_RELEASE_PAYLOAD=3`,
   `FUSE_URING_PAYLOAD_RETENTION (1<<2)`, `FUSE_URING_COMMIT_RETAIN (1<<0)`;
   `fuse_uring_cmd_req` stays 24 bytes (union absorbs `commit.flags`;
   `BUILD_BUG_ON` in `dev_uring.c`). Arming requires ZERO_COPY.
   COMMIT+RETAIN parks the ent in `FRRS_RETAINED` (no unregister, no
   fetch, cmd pending). RELEASE: `-ENOENT` unknown / `-EBUSY` live
   un-retained / `0` then re-arm. Teardown drains retained (pr_warn on
   nonzero). Depends on 0025. FUSE values identical on both tracks
   (uapi collision audit 2026-08-09: opcode 3 and bits 2/0 free in
   6.19.14 and 7.1.6). 7.1 adaptations: `io_uring_sqe128_cmd`; cancel
   composes with 7.1's list_del+kfree AVAILABLE path. 6.19: `io_uring_sqe_cmd`;
   cancel moves RETAINED onto `ent_in_userspace` like AVAILABLE.

9. **patch 30 (NEW 2026-08-15; sqz-authored — nvme host-scoped fabric
   subsystems, opt-in)** — the multi-writer program's rung-5b fix
   (design note `docs/design-mw-multipath-kernel.md`; the rung-6 STOP
   finding, design-full-multi-writer §5.2): on `nvme_core.multipath=Y`
   kernels `__nvme_find_get_subsystem()` matches subsysnqn ALONE, so
   two co-located per-mount host identities merge under ONE multipath
   head whose round-robin voids per-mount PR fencing. New opt-in
   module param **`nvme_core.fabrics_host_scoped_subsystems`** (bool,
   default off, perm 0444 — boot-scoped so the subsystem match can
   never go asymmetric): fabric controllers group subsystems by
   `(subsysnqn, hostnqn)`; `struct nvme_subsystem` gains
   `host_scope[NVMF_NQN_SIZE]` (empty = unscoped — param off and PCIe
   are byte-identical to upstream); `nvme_global_check_duplicate_ids()`
   skips host-scoped SIBLINGS of one subsysnqn (they present the same
   target namespaces on purpose — without the skip the fabric arm
   refuses the second identity's namespace as a duplicate ID); new
   read-only `sqz_host_scope` subsystem sysfs attr (guest-validation /
   daemon observability). Connect-time dedup needs no change
   (`nvmf_ctlr_matches_baseopts` already compares the host pair);
   target-side PR state is per-namespace keyed by hostid, so host-side
   splitting cannot fork fencing truth. Files:
   `drivers/nvme/host/{core.c,nvme.h,sysfs.c}` — the series' first
   `drivers/nvme/` patch, zero hunk overlap with 0001–0029.
   **AUTHORING ORDER REVERSED (user ruling 2026-08-15)**: authored and
   compile-verified on the **7.1.6 track FIRST** (the locally-running
   kernel — `make LLVM=1 drivers/nvme/host/` with the running
   `7.1.6-1-cachyos-sqz` config on the fully-patched pristine tree,
   zero new warnings; full 30-patch series re-verified `patch -p1
   --fuzz=0` clean on a fresh extraction), then backported to 6.19.14.
   Backport adaptations are **offsets-only** (the touched regions are
   code-identical across the trees; 6.19's extra
   `subsys->awupf = …` line and `kzalloc` vs `kzalloc_obj` idiom sit
   outside every hunk — the adaptation table lives in the design
   note §4). BOOT-VERIFIED on this (6.19.14) track by rung 6b's qemu
   guest legs (2026-08-15 — `tests/run_mw_matrix.sh
   vm-hostscope-validate` both arms + `vm-multi-identity`; ledger in
   the design note §6); the 7.1 track stays compile-verified only (no
   host reboot on the critical path).
   Number-reuse note: "0030" was earlier planning shorthand for
   selective zc delivery, whose refusal is FINAL on measurement
   (2026-08-09) — that slot was never built, and this unrelated nvme
   patch takes the number.

`patches/` is the `git format-patch` export of the resolved transplant;
`build-kernel.sh` applies it with `patch -p1 --fuzz=0` (any regression
in the transplant fails loud at apply time).

## The 7.2 track (`patches-7.2/`) — 26 patches on linux-7.2.3

**2026-09-06: the track is 26 patches** — **0026** (`sqz: fuse-uring
per-queue background accounting (COMMIT-lock split)`) is the first patch
AUTHORED on 7.2 (not rebased); see its row and paragraph below. The
rebase record that follows describes the 25-patch import of 2026-09-03
and is unchanged.

**Base: linux-7.2.3** (kernel.org stable, the newest 7.2.y on
2026-09-03 — `releases.json` `latest_stable`; sha256
`8ba259e8e7b13ec6ef0941c8a39ad90b24bd4a4d6c0010ba6bafb794550ecd03`
from `v7.x/sha256sums.asc`). The 30-patch 7.1 series rebased
2026-09-03 by the 7.1 track's own recipe — pristine tarball in a
scratch git repo (7.1.6 as an orphan base, the 30 patches applied
there clean as the control, then `git rebase --onto` the 7.2.3 base so
every conflict is a real three-way merge with its true ancestor),
`git format-patch` export, `patch -p1 --fuzz=0` re-verified on a fresh
extraction. Every adapted patch carries its `[sqz 7.2.3 rebase]` note
in the commit body beneath the 7.1 note it inherited.

**Five patches DROPPED — landed upstream in 7.2.** Koong's FUSE
prep-refactor half (v4 patches 13–17) merged through the fuse tree in
the 7.2 cycle as the `fuse-uring:` series (all 2026-06-15,
`git.kernel.org` torvalds/linux.git, `v7.2` log of
`fs/fuse/dev_uring.c`), so the 7.2 series is **25 patches, renumbered
contiguously** (the mapping below is authoritative; the 6.19/7.1
numbers stay in prose about those tracks). The io_uring halves (kmbuf
kbuf/register/memmap 01–12, rsrc bvec 20–23) did **not** land — the
`v7.2` logs of `io_uring/kbuf.c` and `io_uring/rsrc.c` carry no kmbuf
or `io_buffer_register_bvec()` commit, and 7.2.3's
`include/uapi/linux/io_uring.h` has no `KMBUF` symbol — so all 12 + 4
stay in the series, as do the 7 FUSE consumer/sqz patches and 0030.

| 7.1 # | 7.2 # | Patch | 7.2.3 verdict |
|---|---|---|---|
| 0001 | 0001 | kbuf refactor | applied clean (identical patch-id to 7.1) |
| 0002 | 0002 | kbuf rename | applied clean |
| 0003 | 0003 | kmbuf rings (**38/39**) | applied clean — opcodes unchanged, see the audit |
| 0004 | 0004 | kmbuf mmap | applied clean |
| 0005 | 0005 | kmbuf buffer selection | **rebased** (`io_ring_buffer_select`): 7.2's `46800585ae04` "validate ring provided buffer addresses with access_ok()" moved the `sel.addr` assignment ahead of any `req->flags` mutation and gates it on `access_ok()`; the kernel-managed arm assigns `sel.kaddr` and bypasses that check (a kernel address is never user-accessible), the user arm keeps 7.2's check verbatim |
| 0006 | 0006 | pin/unpin | applied clean |
| 0007 | 0007 | kmbuf recycle | applied clean |
| 0008 | 0008 | `io_uring_fixed_index_{get,put}` | **rebased** (add/add adjacency): 7.2's `df0a52537c0f` "add huge page accounting for registered buffers" inserted `io_buffer_acct_cloned_hpages()` at the same point after `io_import_reg_buf()`; both kept, no content change |
| 0009 | 0009 | `io_uring_is_kmbuf_ring` | applied clean |
| 0010 | 0010 | export `io_ring_buffer_select` | applied clean |
| 0011 | 0011 | buffer id | **rebased**: only the `sel.buf_id` line lands after `sel.buf_list` (the addr assignment moved in the 7.2 form of 05) |
| 0012 | 0012 | cmd buffer index | applied clean |
| 0013 | — | refactor next-req | **DROPPED — landed** as `6813da095068` "fuse-uring: separate next request fetching from sending logic". The 7.1 rebase's `if (ent->fuse_req)` guard in `fuse_uring_send()` was redundant: every caller (COMMIT_AND_FETCH, `send_in_task`, 0029's RELEASE) is gated on a fetched request |
| 0014 | — | hdr-to-ring | **DROPPED — landed** as `6582f8a06698` "fuse-uring: refactor io-uring header copying to ring" |
| 0015 | — | hdr-from-ring | **DROPPED — landed** as `ba7d47897fd8` "fuse-uring: refactor io-uring header copying from ring" |
| 0016 | — | enum header types | **DROPPED — landed** as `b2bbd7dcd243` "fuse-uring: use enum types for header copying" (7.2's form returns an OFFSET from `ring_header_type_offset()` where v4 returned a pointer from `get_user_ring_header()` — the shape 0019 rebases onto) |
| 0017 | — | copy-state setup | **DROPPED — landed** as `c0f9203732fc` "fuse-uring: refactor setting up copy state for payload copying" (the rebase auto-dropped it as an empty commit — the strongest "already there" evidence) |
| 0018 | 0013 | kaddr copy support (`dev.c`) | applied clean |
| 0019 | 0014 | FUSE kmbuf ring | **rebased** (9 hunks): composed with 7.2's `fuse_conn → fuse_chan` split (`b03404ea3a05` ring->chan, `bf9932623d20` fch->lock, `0ea79b7d077f` fch->ring; every added `fc` site renamed) and with the landed offset form of the header helpers — `get_kernel_ring_header()` derives its `iov_iter_advance()` from `ring_header_type_offset()`, the user arms of `copy_header_{to,from}_ring()` compute `ent->headers + offset`; 7.2 idioms `fuse_pqueue_alloc()` (`48649c0603bd`), `kzalloc_obj(*ent)`, `READ_ONCE(ring->queues[qid])`, the `FUSE_URING_IOV_{HEADERS,PAYLOAD}` accessors (`8bbb2ad1f687`) in `create_ring_ent`; `#include "fuse_dev_i.h"` (`c0f817320d6a` dropped `fuse_i.h` from dev_uring) |
| 0020 | 0015 | bvec rename | applied — context-only drift (ublk's `ublk_rq_has_data()` became `blk_rq_has_data()`), rename hunks unchanged |
| 0021 | 0016 | register split | **rebased**: `imu->acct_pages` no longer exists (`df0a52537c0f` derives accounting at unmap via `io_buffer_unaccount_pages()`); the assignment is dropped from `io_kernel_buffer_init()` |
| 0022 | 0017 | optional release | **rebased**: `io_buffer_unmap()` unaccounts from a derived local; the `if (imu->release)` guard composes with that form |
| 0023 | 0018 | `io_buffer_register_bvec` | applied clean |
| 0024 | 0019 | FUSE zc | **rebased**: `fch->ring` in `fuse_uring_register()`; and 7.2's `7d87a5a284bb` "fuse-uring: clear ent->fuse_req in commit_fetch error path" routes the `set_commit` `WARN_ON_ONCE` arm through `fuse_uring_req_end()` (background accounting) — the compile proof caught it ("too few arguments"): that site gains `issue_flags`, and because it reaches `req_end` with an ent still in userspace handoff (`ent->cmd == NULL` — no cmd owns the ent), the zc unregister is guarded on `ent->cmd` (the slot is released at ring-fd teardown like every other abandoned registration) |
| 0025 | 0020 | abort-race folio refs | applied clean |
| 0026 | 0021 | docs | applied clean |
| 0027 | 0022 | seam (`io_buffer_add_list` check) | applied clean — **still required** on 7.2 (int-returning `io_buffer_add_list()` unchanged) |
| 0028 | 0023 | `FUSE_TIME_LIMITS` | applied clean — hunks verified in 7.2's `process_init_reply` `time_gran` block and `fuse_new_init` flag mask |
| 0029 | 0024 | zc payload retention | **rebased**: `fuse_uring_release_payload()` takes `struct fuse_chan *fch` (`fch->ring` / `fch->connected`), loads the queue with `READ_ONCE(ring->queues[qid])`, dispatch passes `fch`; the RETAIN arm composes with the `ent->cmd` guard (`zero_copied && !retain && ent->cmd`) and the 7.2 `req_end` site passes `false` |
| 0030 | 0025 | nvme host-scoped fabric subsystems | applied clean (identical patch-id — the region is code-identical 7.1.6 → 7.2.3) |
| — | **0026** | **sqz: fuse-uring per-queue background accounting (COMMIT-lock split)** | **AUTHORED on 7.2.3, 2026-09-06** (no 7.1/6.19 original yet — backports owed, adaptation ledger in `docs/design-kernel-bg-per-queue.md` §6). `fs/fuse/{dev_uring.c,dev.c,dev_uring_i.h,fuse_dev_i.h}` + the fuse-io-uring rst; zero uapi/Kconfig change; zero hunk overlap with 0001–0025 (it edits `fuse_uring_req_end()`'s bg arm and `fuse_uring_queue_bq_req()`, which 0019/0024 only pass through). Applies `--fuzz=0` on the 0001–0025 tree; the 26-patch chain re-verified fuzz=0 on a fresh base and byte-identical to the git series tip |

**Patch 0026 (sqz-authored, 2026-09-06 — the R-4 ledger's kernel item).**
The e2e perf audit's reap-thread ledger
(`.benchmarks/2026-09-03-r4-reap-thread-economy.md` §2, field
6.19.14-sqz, kern rand-4k 24 × qd8): the FUSE-over-io_uring queue worker
pays **1.9 µs/op of spinlock contention** on `fuse_uring_req_end`'s queue
lock + the connection-wide `fch->bg_lock` nested in it and on
`fuse_request_end`'s second `bg_lock` take — 32 workers ending requests
against 24 submitters (`fuse_uring_queue_bq_req` takes the same lock).
0026 gives every `fuse_ring_queue` its own background ledger under the
`queue->lock` it already holds (`num_background`, `active_background`,
`bg_blocked`, `bg_waitq`) and a per-queue budget = its share of
`max_background` (`max_background / nr_queues`, remainder to the lowest
qids, floor 1 — for SqueezeFS's INIT reply `max_background = queues ×
q_depth` the share is exactly the queue's ent count), so neither the
uring submit nor the uring end path takes `bg_lock`; `fuse_get_req`'s
background wait moves to the submitting task's queue (`fuse_uring_bg_wait`);
`fuse_chan_num_background` (fusectl, the congestion checks) sums the
queues locklessly; fusectl `max_background` writes re-gate every queue;
the classical path is verbatim; `FR_BG_URING` marks the ledger a request
was charged to so the `fiq->ops` switch-over stragglers stay on the
connection ledger and abort/teardown credit exactly what was charged (the
five correctness points are in the commit message). **Semantics change
stated:** a saturated queue blocks ITS submitters even while siblings have
room. Design `docs/design-kernel-bg-per-queue.md`; A/B rig
`.benchmarks/rigs/2026-09-06-kernel-bg-per-queue-ab.sh` (kernel A = 0001–0025
vs B = +0026, same daemon, A A B B across reboots). **Compile-proven,
not boot-tested** — the lever holds its slot on the field row.

**Compile proof, 0026** (2026-09-06; `~/sqz-kernel-scratch/build-patched-0026*.log`):
the 2026-09-03 patched 7.2.3 tree + 0026 (`patch -p1 --fuzz=0`) →
`make -j16 fs/fuse/ io_uring/` — **0 warnings / 0 errors** (gcc 15.3,
the same CachyOS `7.1.8-cachyos-lto`-derived config); `W=1` on the eight
fs/fuse TUs that include the touched headers, patched vs pristine:
**identical warning sets** (the only lines are cuse.c's pre-existing
kernel-doc note, both trees); and a second **lockdep build**
(`CONFIG_PROVE_LOCKING=y CONFIG_DEBUG_SPINLOCK=y CONFIG_DEBUG_LOCK_ALLOC=y
CONFIG_LOCKDEP=y`, `O=` on the clean 0001–0026 git tree, `olddefconfig`) —
**0 warnings / 0 errors**. `git diff --stat` of 0026 alone: 5 files,
+335/−44. The runtime lockdep run is the boot test's.

**Compile proof** (2026-09-03): running CachyOS `7.1.8-cachyos-lto`
config (`/proc/config.gz`; `CONFIG_FUSE_IO_URING=y`,
`CONFIG_IO_URING_BPF=y`, `CONFIG_IO_URING_ZCRX=y`, nvme-tcp/fabrics
`=m`, ublk `=m`) → `make olddefconfig` (gcc 15.3, LTO/BTF/module-sign
dropped — host-tool availability, not a series concern) →
`make io_uring/ fs/fuse/ drivers/nvme/host/ drivers/block/ublk_drv.o`
on the pristine AND the fully-patched 7.2.3 tree: **0 warnings /
0 errors on both**, and `W=1` on exactly the eleven touched
translation units: **empty warning set on both**. Every allocated ABI
value verified in the built tree (`IORING_REGISTER_KMBUF_RING=38`,
`UNREGISTER=39`, `IORING_REGISTER_LAST` past them,
`IORING_OFF_KMBUF_RING 0x88000000`, `register.c` dispatch cases,
`memmap.c` `IORING_OFF_KMBUF_RING` case, the four FUSE init/commit
bits, `FUSE_IO_URING_CMD_RELEASE_PAYLOAD=3`, `FUSE_TIME_LIMITS
1ULL<<62`, `nvme_core.fabrics_host_scoped_subsystems`, the
`BUILD_BUG_ON(sizeof(struct fuse_uring_cmd_req) != 24)`).

**Concatenated manager-ready form:** `~/sqz-kmbuf-zc-7.2.3-v1.patch`
= `cat patches-7.2/00*.patch` (sha256
`d977fafffe67dfe5429226331267a2c74220ca73c3d9280390056824ca77697a`,
158,611 bytes) — sequential per-patch `patch -p1 --fuzz=0` dry-run +
apply CLEAN 25/25 on a fresh pristine extraction; the single-shot real
apply of the concat lands 0 rejects and produces a tree identical to
the per-patch apply; the naive single-shot `--dry-run` of the whole
file reports 92 false FAILED hunks (later hunks depending on earlier
patches in the same file — the same control the 6.19 and 7.1
artifacts show). **v1 is the 25-patch (0001–0025) artifact — the A arm
of the 0026 A/B**; a `-v2` concat carrying 0026 is minted when the box
that boots it is chosen (same `cat` recipe, 26 files).

## Config

Base: `config-base-7.1.2-1.el8.elrepo.x86_64` (the client's running
config, copied read-only 2026-08-01) → `make olddefconfig` on 6.19.14 →
`config-fragment` (each entry asserted in the final `.config`; the
assertion failing fails the build). `CONFIG_ULP_DDP` does not exist in
this tree (never merged upstream) — documented absence.

## Rebuild

```bash
docker/kernel-sqz/build.sh            # podman/docker, 16 cpus, nice
# artifacts: dist/kernel-sqz/*.rpm + config-6.19.14-sqz + SHA256SUMS
```
