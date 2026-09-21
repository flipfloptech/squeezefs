# docker/kernel-sqz — the sqz custom kernel recipe

USER-AUTHORIZED program (2026-08-02): containerized custom kernel build
carrying the `sqz` tag, unlocking the two interface-frontier mechanisms
the 2026-08-02 census (`.benchmarks/2026-08-02-interface-frontier.md`)
found closed on the field kernel:

1. **io_uring zcrx + mlx5 tcp-data-split (HDS)** — in-tree since 6.17
   for mlx5/CX-7 (no MOFED).
2. **The FUSE zero-copy reply ABI** — Joanne Koong's in-flight lore
   series (kmbuf + FUSE-zc), not in any released kernel. Exact
   message-ids, versions, base commit, and every conflict resolution:
   **`SERIES.md`** (the manifest — read it first).

Result: **`6.19.14-sqz`** EL8 kernel RPMs (Rocky 8.10 installable),
built in a pinned Rocky 8 + gcc-toolset-14 + pahole v1.30 container.

**v2 (2026-08-04 + 2026-08-09):** the zc-write wave took the series to
29 patches. 2026-08-04 authored `FUSE_TIME_LIMITS` (now **0028**).
2026-08-09 inserted **0025** (abort-race: imu-held folio refs on zc
registrations) after Koong 0024 and appended **0029** (zc payload
retention — Approach B ACK-early accelerator). Old 0025–0027 became
0026–0028. Selective zc delivery was refused FINAL on measurement —
§4.3 gate rode the boot-test 4 KiB armed-vs-disarmed row (its retired
"0030" shorthand slot is reused by the unrelated nvme patch below).
Spec `docs/design-zc-write-kernel-v2.md`, evidence
`.benchmarks/2026-08-09-kernel-zc-write-v2.md`.

**Rung 5b (2026-08-15):** the series is **30 patches** — **0030** adds
opt-in **host-scoped fabric subsystems**
(`nvme_core.fabrics_host_scoped_subsystems=Y`: fabric subsystems group
by `(subsysnqn, hostnqn)` so co-located per-mount identities never
merge under one multipath head — the multi-writer rung-6 STOP finding's
fix; default off = byte-identical). Authored on the **7.1 track first**
(the authoring-order ruling), backported offsets-only to 6.19.14.
Design `docs/design-mw-multipath-kernel.md`; boot validation is rung
6b's qemu guest.

**0031 (2026-09-06): the series is 31 patches on BOTH the 6.19.14 field
track and the 7.1 track** — the per-queue FUSE background accounting
(the COMMIT-lock split), authored on the 7.2 track as **0026** and
backported the same day. The finding was measured on the FIELD kernel
(`6.19.14-sqz`: the R-4 ledger's 1.9 µs/op of `bg_lock` contention, paid
TWICE per uring completion on 6.19 — `fuse_uring_req_end` + the inline
finish in `fuse_request_end`). Compile-proven three ways per track
(build clean, `W=1` identical to the 0001–0030 control, a
`PROVE_LOCKING`/`DEBUG_SPINLOCK` build clean). **Boot-tested and
MEASURED the same day** (`.benchmarks/2026-09-06-kernel-bg-per-queue-ab.md`,
A A B B on squeeze-test, same 1.2.1 `dist` daemon): the `fuse3-ur`
worker 17.4 → 14.3 µs/op (−18 %) with `native_queued_spin_lock_slowpath`
12.8 % → 0.11 % of its cycles, kern rand-4k +9 % IOPS (508 → 556k
sustained), p99 −13…−17 %, seq and il control par, zero fuse/io_uring
kernel-log lines on both B boots — **B ships**; squeeze-test runs 0031
from 2026-09-06 19:48 UTC. The 7.2 track's 0026 booted the dev box the
same day (design §5a: zc gate 149/149, no WARN); 7.1's 0031 is
compile-proven only. Design + the per-track adaptation ledger:
`docs/design-kernel-bg-per-queue.md` (§5/§6); SERIES.md item 10 + the
7.1 paragraph.

**Strata ruling (USER DECISION 2026-08-02): the kmod-sqzfuse stratum
is KILLED.** There are exactly **two strata**: (1) **stock-graceful**
— the daemon runs capability-probed on stock kernels, degrading
gracefully (no kmbuf, no zcrx, no time limits); (2) the **full `-sqz`
kernel** — this recipe's RPMs, carrying the whole series. No
middle stratum of out-of-tree fuse/io_uring kmods will be built or
maintained; do not resurrect it.

## Contents

| Path | What |
|---|---|
| `Dockerfile` | pinned EL8 build image (gcc-toolset-14, from-source pahole v1.30 for BTF) |
| `build.sh` | host wrapper: image build + capped (`--cpus 16`, `nice`) kernel build; artifacts → `dist/kernel-sqz/` |
| `build-kernel.sh` | in-container: sha256-pinned tarball → the track's series → config assembly → checklist assertion (fail loud) → `make binrpm-pkg`. **`TRACK=6.19.14` (default, the FIELD build) / `7.1` / `7.2`** selects `patches-$TRACK/` + the pinned `KVER`/sha256 from its table — see *Build* below |
| `SERIES.md` | the series manifest: message-ids, base ruling, conflict resolutions, 0025/0029, the 7.2 track's per-patch table + the 7.2.3 → 7.2.6 rebase ledger |
| `V2-CANDIDATES.md` | the v2 scoping manifest (ranked candidates; rank 1 = TIME_LIMITS, now patch 0028) |
| `patches/` | `git format-patch` export (0001–0024 Koong + 0025 abort-race + 0026 docs + 0027 seam + 0028 TIME_LIMITS + 0029 retention + 0030 nvme host-scoped subsystems — rung 5b + **0031 per-queue bg accounting — the 6.19.14 backport of 7.2's 0026, 2026-09-06**) |
| `patches-7.1/` | the **linux-7.1.6 rebase** of the same 30 patches (the D13 latest-mainline track — 0030 was AUTHORED here first; see *The 7.1 track* below) **+ 0031** (the 7.1 backport of the per-queue bg accounting, 2026-09-06) |
| `patches-7.2/` | the **linux-7.2.6 rebase** (base bumped from 7.2.3 on 2026-09-21 — the CachyOS manager moved to 7.2.6 and three `fs/fuse/dev_uring.c` hunks stopped applying; see *The 7.2 track* below) — **26 patches**: the same series minus the five FUSE prep-refactors upstream 7.2 landed (renumbered contiguously; mapping in *The 7.2 track* below and SERIES.md) **plus 0026**, the per-queue background accounting (COMMIT-lock split) — the first patch AUTHORED on this track (2026-09-06; boot-tested on the dev box the same day) |
| `config-base-7.1.2-1.el8.elrepo.x86_64` | the field client's running config (the base; copied read-only 2026-08-01) |
| `config-fragment` | the ENABLE CHECKLIST — every entry asserted in the final `.config` |
| `probes/` | capability probes (see below) |

## Probes (`probes/`)

All gcc-8.5-clean, no libnl/liburing — they compile on the field box.

* `hds_query.c` — ethtool-netlink `RINGS_GET`/`RINGS_SET` for
  `tcp-data-split` + `hds-thresh` (the box's pre-6.15 ethtool-era
  probe; GET is read-only, `set on|off` is the Phase-0 arm).
* `zcrx_smoke.c` — `IORING_REGISTER_ZCRX_IFQ` smoke: `surface` mode is
  read-only-safe (valid args, if_idx=0 → ENODEV/EPERM ⇒ opcode
  present); `bind <ifidx> <rxq>` does a real queue bind (restarts the
  rx queue — flagged, not default).
* `kmbuf_smoke.c` — the `IORING_REGISTER_KMBUF_RING` opcode LADDER
  (37/38 = 6.19-sqz field track, then 38/39 = 7.1-sqz track; see the
  7.1-track audit section): a rung reads PRESENT only on register 0 +
  repeat-EEXIST + kmbuf-offset mmap; every rung refused ⇒ ABSENT
  (stock kernels). `--signatures` prints the raw per-opcode errno
  signature (the per-class measurement mode). `--fuse-rungs` is the
  v2 retention/abort probe: RELEASE_PAYLOAD negotiation after
  FUSE_INIT (`-ENOENT`/`-ENOTCONN` = opcode present, `-EINVAL` =
  pre-0029); retention round-trip + abort-race SKIP until the
  boot-test plan's armed mount / KASAN dir-build.
* `capability_matrix.sh` — the before/after matrix runner (kernel id,
  HDS attr, HW-GRO, zcrx surface, kmbuf surface, fuse_uring kallsyms,
  storage modules).

## Build

```bash
docker/kernel-sqz/build.sh            # podman or docker
# → dist/kernel-sqz/kernel-*.rpm + config-6.19.14-sqz + SHA256SUMS
TRACK=7.2 docker/kernel-sqz/build.sh  # the 7.2.6 track (26 patches) → config-7.2.6-sqz
```

`TRACK` (default `6.19.14` — **the FIELD build, unchanged**) selects the
row in `build-kernel.sh`'s table: `6.19.14` → `patches/`, `7.1` →
`patches-7.1/` on linux-7.1.6, `7.2` → `patches-7.2/` on linux-7.2.6
(each row pins its tarball sha256; an unknown value refuses loud). The
config assembly is the same for every track (client base config →
`olddefconfig` → `config-fragment` checklist), so the checklist
assertion — not the track — decides whether a build is acceptable.

## Install on the field client (safe-boot discipline)

The ELRepo 7.1.2 kernel stays the **permanent grub default**; the sqz
kernel boots as a **one-shot** so a failed boot self-reverts on power
cycle (the operator power-cycles; agents cannot):

```bash
rpm -ivh --oldpackage kernel-6.19.14_sqz-1.x86_64.rpm   # installs modules + BLS entry
grub2-reboot '<the 6.19.14-sqz entry>'                   # ONE-SHOT; default untouched
reboot
# after boot: uname -r == 6.19.14-sqz; run probes/capability_matrix.sh
```

## The 7.1 track (`patches-7.1/`) — D13 latest-mainline rebase

**Base: linux-7.1.6** (kernel.org; the CachyOS 7.1.6-1 line). The same 30
patches, semantically rebased 2026-08-06 onto 7.1.6 (0025/0029 landed
2026-08-09; **0030 nvme host-scoped fabric subsystems was AUTHORED on
this track first** — rung-5b authoring-order ruling 2026-08-15 — then
backported to 6.19.14) for local zc-capable boots via the CachyOS
kernel manager, **plus 0031 (2026-09-06)** — the per-queue background
accounting backported FROM the 7.2 track's 0026 (the reverse of 0030's
order: 0026 was authored where the `fuse_chan` design read was done).
**6.19.14 stays the FIELD series** (the EL8 fleet RPMs, `patches/`); 7.1
is the D13 latest-mainline track. Concatenated manager-ready form:
`~/sqz-kmbuf-zc-7.1.6-v3.patch` = `cat patches-7.1/00*.patch` **as of 30 patches** (v2 = the 29-patch predecessor; a 31-file `cat` mints the 0031 arm when the box that boots it is chosen — v3 on disk is the A arm) — applies
sequentially `patch -p1 --fuzz=0` clean (verified on a fresh pristine
7.1.6 extraction). Compile-proof: `make io_uring/ fs/fuse/` with the
running CachyOS 7.1.6 config, zero new warnings (0025 and 0029). The
v1 concat `~/sqz-kmbuf-zc-7.1.6.patch` is the 27-patch predecessor.

### ABI/opcode audit (the one collision — RENUMBERED, loudly)

* **`IORING_REGISTER_KMBUF_RING` 37 → 38, `IORING_UNREGISTER_KMBUF_RING`
  38 → 39.** Upstream 7.1 allocated **37 = `IORING_REGISTER_BPF_FILTER`**
  (`CONFIG_IO_URING_BPF`, enabled in the CachyOS config), so keeping 37
  was impossible (duplicate case in `io_uring/register.c`; a userspace
  probe of 37 would hit BPF-filter semantics, not EINVAL). This breaks
  numeric lockstep with the deployed 6.19.14-sqz field kernel — **the
  daemon-side follow-up is DONE**: the fuse3 fork and `kmbuf_smoke.c`
  resolve the pair by a **probe LADDER** (never a kernel-version check —
  the portable-by-default law): field track 37/38 first, then 38/39,
  where a rung reads Present only on the full kmbuf signature —
  register 0 **+** identical-repeat `EEXIST` **+** a successful mmap at
  `IORING_OFF_KMBUF_RING | (bgid<<16)`. False Present is unreachable
  (7.1's BPF_FILTER imports the arg's first u16 as `cmd_type` ≠ 1 for
  any page-aligned `buf_size` ⇒ EINVAL before any state change; a
  crossed kmbuf-UNREGISTER answers ENOENT at the bgid lookup; stock
  dispatch EINVALs at `IORING_REGISTER_LAST`), the probe arg rides
  zero-padded past any foreign reader's struct width, and each rung's
  scratch ring is dropped before the verdict. Resolution logged at arm
  (`kmbuf_ops=37/38 (6.19-sqz)` / `38/39 (7.1-sqz)` / `absent`);
  decision table pinned in `crates/fuse3/src/raw/connection/kmbuf.rs`
  tests; `kmbuf_smoke.c --signatures` is the per-class measurement
  mode.
* `IORING_OFF_KMBUF_RING 0x88000000` — free in 7.1.6 (PBUF 0x80000000,
  PARAM 0x20000000, ZCRX 0x30000000, mask 0xf8000000): **unchanged**.
* `FUSE_URING_BUF_RING (1<<0)` / `FUSE_URING_ZERO_COPY (1<<1)` /
  `FUSE_URING_PAYLOAD_RETENTION (1<<2)` in the `fuse_uring_cmd_req`
  init union, `FUSE_IO_URING_CMD_RELEASE_PAYLOAD=3`,
  `FUSE_URING_COMMIT_RETAIN (1<<0)` on `commit.flags`, and
  `FUSE_TIME_LIMITS (1ULL<<62)` — FUSE values identical on 6.19.14 and
  7.1.6 (uapi collision audit 2026-08-09). Struct stays 24 bytes.

### Port ledger (patch → what changed vs the 6.19 series)

Unlisted patches applied identically (same patch-id). Every adapted
patch carries its `[sqz 7.1.6 rebase]` note in the commit body.

| Patch | Adaptation on 7.1.6 |
|---|---|
| 0001 kbuf refactor | hand-merged onto 7.1's `io_register_pbuf_ring`: `min_left` validation into `io_validate_buf_reg()`, `min_left_sub_one` consumption kept in the caller, `kzalloc_obj()`, `bl->nr_entries` dropped (field removed upstream) |
| 0003 kmbuf rings | **opcodes renumbered 38/39** (see audit); `io_setup_kmbuf_ring()` uses `reg->ring_entries` |
| 0007 kmbuf recycle | `bl->nr_entries` → `bl->mask + 1` |
| 0011 buffer id | context-only drift (7.1 `io_buffer_select` locals) |
| 0013 next-req refactor | unified `fuse_uring_send()` carries 7.1's send-time `fuse_uring_add_to_pq()` (guarded on `ent->fuse_req`); composed with 7.1's restructured `send_in_task` cancel-arm teardown |
| 0015 hdr-from-ring | 7.1's control flow kept (upstream dropped the `req->out.h.error` assignment) |
| 0016 enum types | context-only drift from 0015 |
| 0019 FUSE kmbuf | `io_uring_sqe128_cmd()` (7.1 dropped 1-arg `io_uring_sqe_cmd`), `kzalloc_obj/objs` idioms, ERR_PTR `create_queue` merged with 7.1's shape, `send_in_task` rework composed with the 7.1 cancel arm |
| 0020 bvec rename | rename extended to the 7.1-only ublk call sites (batch dispatch / auto-buf-reg paths) |
| 0021 register split | `io_kernel_buffer_init()` in 7.1 idioms: `io_cache_free(&ctx->node_cache, node)`, `imu->flags = IO_REGBUF_F_KBUF` (replaces `is_kbuf`) |
| 0024 FUSE zc | `issue_flags` threading composed with 7.1's `prepare_send` error arm + the 7.1-only cancel-arm `fuse_uring_req_end()` site; `io_uring_sqe_cmd` → `io_uring_sqe128_cmd` |
| 0025 abort-race | identical besides context offsets; `io_buffer_register_bvec` + `fuse_uring_set_up_zero_copy` shape is shared |
| 0027 seam (was 26) | ported as-is — **still required**: 7.1's `io_buffer_add_list()` is the int-returning stable form, and patch 03's register path inherits the unchecked call |
| 0029 retention | `io_uring_sqe128_cmd`; cancel composes FRRS_RETAINED with 7.1's list_del+kfree AVAILABLE path; 6.19 cancel moves RETAINED to `ent_in_userspace` |
| 0030 nvme host-scoped fabric subsystems | **authoring order reversed** (rung-5b ruling 2026-08-15): authored + compile-verified ON 7.1.6 first, then backported — the 6.19 adaptations are offsets-only (`subsys->awupf` / `kzalloc` idiom sit outside every hunk); first `drivers/nvme/` patch in the series |
| **0031 per-queue bg accounting** (2026-09-06) | **backported FROM 7.2's 0026** (authored there — the `fuse_chan` design read): `fc->` for `fch->` / `ring->fc` / `fuse_abort_conn()`; `fuse_request_bg_finish(fc, req)` exists and goes `static` (declaration leaves `fuse_dev_i.h`); no `fuse_chan_{num,max}_background` accessors, so `file.c` 911/2306 read the new `fuse_num_background()` inline and `control.c` 133's inline write gains `WRITE_ONCE` + the re-gate hook (both files gain `#include "dev_uring_i.h"`); `fuse_block_alloc()` is one expression (the `!fuse_uring_ready(fc)` clause on its `blocked` arm); the abort arm is 0026's (7.1 already walks the queues unconditionally). Compile-proven with `config-7.1.8-cachyos` (clean, `W=1` identical to the 0001–0030 control, lockdep build clean); 31/31 chain fuzz=0 on a fresh 7.1.6. The 6.19 twin additionally gains the `queue_refs == 0` waiter release (SERIES.md item 10) |

### CachyOS kernel manager notes

CachyOS applies its own patchset (BORE scheduler etc.) **before** user
patches. Its io_uring/fuse overlap is unlikely but not impossible —
watch the manager's apply log; any FAILED hunk there means the CachyOS
base drifted from vanilla 7.1.6 in a touched file, and the failure
should be reported (with the .rej) rather than fuzzed through. The
concatenated patch is stacked per-commit diffs: it applies sequentially
(`patch -p1 --fuzz=0`) but a naive single-shot `--dry-run` of the whole
file against a pristine tree reports false failures for later hunks
that depend on earlier patches in the same file (identical behavior to
the 6.19 artifact — verified as the control).

## The 7.2 track (`patches-7.2/`) — D13 latest-mainline rebase, 2026-09-03

**Base: linux-7.2.6 since 2026-09-21** (kernel.org stable; sha256
`039aef84f2b0994aeda3f4fcfc3d02ec9d7a9bbb9020ea264c43f446c860f606`
from `v7.x/sha256sums.asc`) — the CachyOS kernel manager moved its
7.2 line to `linux-cachyos-bore-7.2.6` and the 7.2.3 concat stopped
applying (three FAILED hunks, all `fs/fuse/dev_uring.c`); the bump is
recorded in *The 7.2.6 bump* below and in SERIES.md's rebase ledger.
The paragraphs that follow describe the 2026-09-03 import, whose base
was **linux-7.2.3** (the newest 7.2.y on 2026-09-03 per
`releases.json`; sha256
`8ba259e8e7b13ec6ef0941c8a39ad90b24bd4a4d6c0010ba6bafb794550ecd03`
from `v7.x/sha256sums.asc`) — every statement about "7.2.3" there is
the history of that import. The 7.1 track's 30 patches semantically
rebased by the 7.1 recipe (scratch git repo, 7.1.6 orphan base as the
apply control, `git rebase --onto` 7.2.3 for true three-way merges,
`git format-patch` export, `patch -p1 --fuzz=0` re-verified on a fresh
extraction). **6.19.14 stays the FIELD series**; 7.1 and 7.2 are the
D13 latest-mainline tracks (7.2 supersedes 7.1 as "latest"; 7.1 stays
in the tree because it is the locally-booted CachyOS line).

**The rebased series is 25 patches on 7.2 — five DROPPED because upstream
7.2 landed them** (0026, authored here 2026-09-06, makes the track 26 —
see below). Koong's FUSE prep refactors (v4 patches 13–17: next-req
split, header copy to/from ring, enum header types, copy-state setup)
merged in the 7.2 cycle as the `fuse-uring:` series (`6813da095068`,
`6582f8a06698`, `ba7d47897fd8`, `b2bbd7dcd243`, `c0f9203732fc` — all
2026-06-15 on torvalds/linux.git). The kmbuf io_uring infrastructure
(01–12), the rsrc bvec half (20–23), the FUSE consumers (18, 19, 24)
and the sqz patches did NOT land and stay in the series. Numbering is
contiguous after the drop: **7.1 0018→0013, 0019→0014, 0020–0024→
0015–0019, 0025–0030→0020–0025** (0001–0012 unchanged) — so the
TIME_LIMITS / retention / nvme patches are **0023 / 0024 / 0025** on
this track. The per-patch table (clean / rebased-which-hunks /
dropped-with-upstream-id) is SERIES.md → *The 7.2 track*.

Beyond the landed refactors, 7.2 changed three things the series
composes with: the **`fuse_conn → fuse_chan` split** (`ring->chan`,
`fch->lock`/`fch->ring`/`fch->connected`, `fuse_dev_i.h` split out of
`fuse_i.h` — every added `fc` site in 0014/0019/0024 renamed);
**`access_ok()` validation in `io_ring_buffer_select()`**
(`46800585ae04` — the kernel-managed arm bypasses it, the user arm
keeps it; 0005/0011); and **derived huge-page accounting in rsrc**
(`df0a52537c0f` removed `imu->acct_pages`; 0008/0016/0017). One
adaptation was found by the compile proof rather than the merge:
`7d87a5a284bb` routes `commit_fetch()`'s `set_commit` WARN arm through
`fuse_uring_req_end()`, whose zc form (0019) takes `issue_flags` and
unregisters through `ent->cmd` — that arm reaches it with
`ent->cmd == NULL`, so the unregister is now guarded on `ent->cmd`
(the slot is released at ring-fd teardown). Concatenated manager-ready
form (HISTORY — the 7.2.3-base artifacts; the current one is
`sqz-kmbuf-zc-7.2.6-v1.patch`, *The 7.2.6 bump* below):
**`~/sqz-kmbuf-zc-7.2.3-v1.patch`** = `cat patches-7.2/00*.patch`
(sha256 `d977fafffe67dfe5429226331267a2c74220ca73c3d9280390056824ca77697a`
at 25 patches; the 26-patch form the nix config booted from 2026-09-06
is `d7961949702dcf3509c2cb9a71a3e07421a4491a62b5f47eaaf2e2c8508ade6e`,
189,758 bytes) — sequential `patch -p1 --fuzz=0` clean 25/25 on a fresh
pristine 7.2.3 extraction; single-shot real apply 0 rejects, tree
identical to the per-patch apply; the naive single-shot `--dry-run`
shows the same false-FAILED class as the 6.19/7.1 artifacts.
Compile-proof:
`make io_uring/ fs/fuse/ drivers/nvme/host/ drivers/block/ublk_drv.o`
with the running CachyOS `7.1.8-cachyos-lto` config (`olddefconfig`,
gcc) on pristine vs patched 7.2.3 — **0 warnings both**, `W=1` on the
eleven touched TUs empty both. Not boot-tested (no reboot on the
critical path; the user builds kernels).

### ABI/opcode audit — NO collision, NO renumbering, NO new rung

* `include/uapi/linux/io_uring.h` is **byte-identical 7.1.6 → 7.2.3**:
  `IORING_REGISTER_BPF_FILTER` still 37, `IORING_REGISTER_LAST` = 38,
  so the 7.1 track's **38/39** pair is free and **unchanged** on 7.2
  (verified in the built tree: `IORING_REGISTER_KMBUF_RING=38`,
  `IORING_UNREGISTER_KMBUF_RING=39`, `register.c` dispatch cases).
  `IORING_OFF_KMBUF_RING 0x88000000` still free (PBUF 0x80000000,
  PARAM 0x20000000, ZCRX 0x30000000, mask 0xf8000000; `memmap.c`
  gains the case). **The daemon's probe ladder needs no new rung**: a
  7.2-sqz kernel resolves on rung 2 exactly like 7.1-sqz (rung 1's
  occupant is the same `BPF_FILTER` import-EINVAL, so the
  false-Present argument transfers verbatim), and `kmbuf_smoke.c` /
  `crates/fuse3/src/raw/connection/kmbuf.rs` are untouched. The
  `KmbufTrack::Sqz71` identity (the zc opcode mirror's key: 7.1's
  page-buffered readdir) also covers 7.2 — same FUSE readdir shape,
  same base family; re-measure the mirror if a future 7.x changes it.
* `include/uapi/linux/fuse.h` is **byte-identical 7.1.6 → 7.2.3**:
  `FUSE_KERNEL_MINOR_VERSION` **45** (the daemon replies
  `min(kernel, 36)` — negotiation unaffected), init-flag watermark
  still bit 42 (`FUSE_REQUEST_TIMEOUT`) so `FUSE_TIME_LIMITS
  (1ULL<<62)` stays clear, `fuse_uring_cmd_req` still 24 bytes with the
  `init`/`commit` union free (`FUSE_URING_BUF_RING`/`ZERO_COPY`/
  `PAYLOAD_RETENTION` bits 0/1/2, `FUSE_URING_COMMIT_RETAIN` bit 0),
  `enum fuse_uring_cmd` ends at 2 so `FUSE_IO_URING_CMD_RELEASE_PAYLOAD=3`
  is free, `fuse_init_out.unused[11]` still the carve-out 0023 uses.
* nvme: no upstream `host_scope`/`fabrics_host_scoped_subsystems`
  symbol in 7.2.3; 0025 applies with an identical patch-id (the touched
  regions are code-identical 7.1.6 → 7.2.3).

### Port ledger (patch → what changed vs the 7.1 series)

Unlisted patches applied identically (same patch-id). 7.2 numbers.

| Patch | Adaptation on 7.2.3 |
|---|---|
| 0005 kmbuf selection | kernel-managed arm assigns `sel.kaddr` and bypasses 7.2's new `access_ok()` user-pointer check; user arm keeps the check verbatim |
| 0008 fixed_index get/put | add/add adjacency with upstream's `io_buffer_acct_cloned_hpages()`; both kept |
| 0011 buffer id | only the `sel.buf_id` line lands (addr assignment relocated by 05) |
| 0014 FUSE kmbuf (was 0019) | `fuse_chan` split (`ring->chan`, `fch->*`), header helpers re-expressed on upstream's offset-returning `ring_header_type_offset()` (`get_kernel_ring_header()` advances the iter by that offset; user arms compute `ent->headers + offset`), `fuse_pqueue_alloc()`, `kzalloc_obj(*ent)`, `READ_ONCE(ring->queues[qid])`, `FUSE_URING_IOV_{HEADERS,PAYLOAD}`, `#include "fuse_dev_i.h"` |
| 0015 bvec rename (was 0020) | context-only drift (`blk_rq_has_data()` in ublk) |
| 0016 register split (was 0021) | `imu->acct_pages = 0` dropped (field removed upstream) |
| 0017 optional release (was 0022) | composes with the derived-local `acct_pages` in `io_buffer_unmap()` |
| 0019 FUSE zc (was 0024) | `fch->ring`; the new 7.2 `req_end` site in `commit_fetch` gains `issue_flags` and the zc unregister is guarded on `ent->cmd` (see above) |
| 0024 retention (was 0029) | `fuse_uring_release_payload(…, struct fuse_chan *fch)`, `READ_ONCE` queue load, `zero_copied && !retain && ent->cmd`, the 7.2 `req_end` site passes `false` |

7.2 lives in the same CachyOS kernel-manager posture as 7.1 (the notes
above apply verbatim); the manager's next base bump to a 7.2.y is
where this concat gets its first boot.

### The 7.2.6 bump (2026-09-21) — base linux-7.2.3 → linux-7.2.6

The CachyOS manager moved to `linux-cachyos-bore-7.2.6` and the nix
source-patching phase (`cachyos-7.2.6-1.tar.gz` + bridge-stp +
request-key + `0001-bore-cachy.patch`, then the sqz concat) reported
**three FAILED hunks, all `fs/fuse/dev_uring.c`**: 0014 hunk 17 at
~1448, 0019 hunk 23 at ~1551, 0024 hunk 22 at ~1703 — the same block
three times, `fuse_uring_register()`'s create-queue arm, which each of
the three patches extends (`use_bufring` → `zero_copy` → `retention`).
The ground moved under it in **7.2.4** and **7.2.6** (kernel.org
`ChangeLog-7.2.{4,6}`; the 7.2.3 → 7.2.6 delta of every series-touched
file: `fs/fuse/dev_uring.c` 65 changed lines, `dev.c` 19,
`dev_uring_i.h` 4, `inode.c` 12 (unrelated), `fuse_i.h` 1 (unrelated),
`drivers/block/ublk_drv.c` 122 (context only for 0015),
`drivers/nvme/host/{core.c,nvme.h}` 30/6 (outside every 0025 hunk);
`io_uring/{kbuf,rsrc,register,memmap,uring_cmd}.c`, `kbuf.h`,
`memmap.h`, `include/linux/io_uring/cmd.h`, `io_uring_types.h`, BOTH
uapi headers, `nvme/host/sysfs.c` and the two rst files are
**byte-identical**):

* **7.2.4** `303b6eeedf29` = upstream `6330b1f61ed1` **"fuse: decouple
  fuse_ring creation from ent registration"** (Koong; Stable-dep-of the
  next) — the ring is created at FUSE_INIT-reply time
  (`fuse_uring_conn_init()` from `fuse_chan_set_initialized()` when the
  INIT reply set `FUSE_OVER_IO_URING`), `fuse_uring_create()` lost its
  "another thread created the ring" race arm, and
  `fuse_uring_register()` no longer creates the ring (`-EINVAL` when
  absent) — its `err` local went with that, so the create-queue arm
  reads `return -ENOMEM;` where the series' pre-image had `return err;`.
  **This is the line all three hunks tripped on.**
* **7.2.4** `a47a416ff68d` = upstream `fd10f40af314` **"fuse: copy
  request headers via a stack buffer for io-uring"** (Xiang Mei) —
  `req->in.h` / `req->out.h` bounce through on-stack `in_header` /
  `out_header` (the `fuse_request` slab has no usercopy whitelist;
  `CONFIG_HARDENED_USERCOPY` panicked). Context for 0014/0019/0024.
* **7.2.6** `8f9a725d8971` = upstream `1f59015e9581` **"fuse: Fix the
  condition to enable over-io-uring"** (Bernd Schubert) —
  `fuse_uring_cmd()` gates on `smp_load_acquire(&fch->initialized)`
  FIRST and REFUSES (`-EOPNOTSUPP`) a connection whose INIT reply did
  not set `FUSE_OVER_IO_URING`. **Behavior change for servers**: the
  daemon already advertises the flag unconditionally (`session.rs`,
  "Required: always advertise FUSE_OVER_IO_URING") and REGISTERs only
  after the INIT reply, so it is unaffected. Context for 0024's
  `BUILD_BUG_ON`.
* **7.2.6** `f940f3d3fd0b` = upstream `4ef7c8cc9894` **"fuse: use
  release/acquire for fch->initialized"** (Koong; Stable-dep-of the
  previous) — `smp_wmb`/`smp_rmb` → `smp_store_release`/
  `smp_load_acquire`. Context for 0026's `fuse_block_alloc()`.

Rebased by the track's own recipe (scratch git repo: 7.2.3 orphan base +
the 26 patches `git am`'d as the control, 7.2.6 orphan base, `git rebase
--onto` for true three-way merges, `git format-patch` export). Git's
merge conflicted in three places (its resolution units differ from
`patch`'s hunk units — same root causes):

| Patch | 7.2.6 conflict | Resolution |
|---|---|---|
| 0014 FUSE kmbuf ring | `fuse_uring_register()` create-queue arm: upstream `return -ENOMEM;` vs the series' `if (IS_ERR(queue)) return PTR_ERR(queue); } else { if (queue->use_bufring != use_bufring) return -EINVAL;` | the series' arm verbatim (`fuse_uring_create_queue()` returns `ERR_PTR` in the series, so upstream's `-ENOMEM` line has no counterpart); `fuse_uring_buf_ring_setup()` now inserts below the new `fuse_uring_conn_init()`; the kmbuf header arm copies from the stack-bounced `in_header` |
| 0024 zc payload retention | `fuse_uring_cmd()`: the `BUILD_BUG_ON(sizeof(struct fuse_uring_cmd_req) != 24)` was anchored on the old "Once a connection has io-uring enabled" block, which 7.2.6 moved below the new acquire gate | `BUILD_BUG_ON` kept right after `fch = fud->chan;`, ahead of upstream's reordered gates, which are kept verbatim; `fuse_uring_commit()`'s RETAIN arm composes with the stack-bounced `out_header` |
| 0026 per-queue bg accounting | `fuse_block_alloc()`: the series' split gate carried 7.2.3's `smp_rmb()` after the `initialized` test; 7.2.6 made the test `smp_load_acquire()` and deleted the barrier | upstream's acquire load + the series' split logic (ring-not-ready first, then the classical `fch->blocked` arm guarded by `!fuse_uring_ready()`), no `smp_rmb()` — the acquire subsumes it |

0019 (zc) auto-merged once 0014's arm was resolved (its `zero_copy`
hunk lands on the composed arm; context-only vs 7.2.3), 0015 is
context-only drift in `ublk_drv.c` (7.2.6's auto-buf-reg fixes), every
other patch has an **identical patch-id** to its 7.2.3 form. **Nothing
was dropped** — no series hunk landed upstream in 7.2.4–7.2.6. The
whole-series diff on 7.2.6 vs on 7.2.3 differs ONLY in those context
lines; the tip-to-tip `dev_uring.c` diff IS the upstream delta. Each
adapted patch carries its `[sqz 7.2.6 rebase]` note beneath the 7.2.3
one; the export renumbers the `[PATCH NN/26]` prefix uniformly (the
2026-09-03 files read `NN/25` + a `26/26`).

**Proofs (2026-09-21, dev box — scoping venue):** chain
`patch -p1 --fuzz=0` **26/26, fuzz 0, offset 0** on a fresh pristine
7.2.6 extraction, byte-identical to the git tip; the concat
**`sqz-kmbuf-zc-7.2.6-v1.patch`** = `cat patches-7.2/00*.patch` (sha256
`75a8e290747cd6e9cd6c84fca2bb125840cf51d17310cd0c45a9eb8140edef9c`,
192,243 bytes, 26 `From` records) applied in the nix derivation's EXACT
shape — `cachyos-7.2.6-1.tar.gz` (its sqz-touched files are
byte-identical to vanilla 7.2.6) + bridge-stp + request-key + BORE,
then the concat single-shot with default fuzz — **0 FAILED, 0 fuzz, 0
offset lines, 0 rejects**, tree identical to the per-patch apply; the
same on vanilla 7.2.6 + BORE (BORE itself rejects 6 `kernel/sched/fair.c`
hunks on VANILLA — it is authored against the CachyOS tree — and touches
none of the sqz files); the naive single-shot `--dry-run` shows the
known false-FAILED class. **Compile proof**: running CachyOS
`7.2.3-cachyos-lto` config (`/proc/config.gz`, `CONFIG_LTO_CLANG_FULL=y`,
`HARDENED_USERCOPY=y`, `FUSE_IO_URING=y`) → `olddefconfig` → `make
LLVM=1` (unwrapped clang 21.1.8 + LLD 21.1.8 — the running kernel's
toolchain; the nix cc-wrapper's injected `-nostdlibinc` trips the
kernel's `-Werror=unused-command-line-argument`, so `CC` is the
unwrapped binary and `HOSTCC=gcc`) `fs/fuse/ io_uring/
drivers/nvme/host/` on vanilla-pristine vs +26 AND on
CachyOS+3-prepatches vs +concat: **0 warnings / 0 errors on all four**,
`W=1` on the same dirs: **identical one-line sets** (upstream's
pre-existing `fs/fuse/ioctl.c:133` tautological compare, on every
tree). **The nix `linux-src-patched` derivation BUILT**
(`/nix/store/varfq7jb4qsfr25jk1cl9khf1jjwqxpr-linux-src-patched.drv` →
`/nix/store/lqvqzxh8nrb1jw5136mp64admjk3vpq0-linux-src-patched`; the sqz
section of its log has zero non-trivial lines and its 23 touched files
are byte-identical to the git tip) — the full kernel build is the
owner's. Not boot-tested here. kernel.org's `latest_stable` was
**7.2.7** on the day; the track follows the CachyOS manager's base
(7.2.6). Read off the cumulative `patch-7.2.{6,7}.xz`: 7.2.7 changes
NO `fs/fuse/` or `io_uring/` file the series touches, but it does
touch `drivers/nvme/host/{core.c,nvme.h,sysfs.c}` (0025's files) and
`drivers/block/ublk_drv.c` (0015's context) — re-run the chain check
at the next bump before assuming fuzz 0 there.

### 0026 — per-queue background accounting (the COMMIT-lock split), 2026-09-06

The first patch **authored on the 7.2 track** (every earlier 7.2 patch is
a rebase). The e2e perf audit's R-4 ledger
(`.benchmarks/2026-09-03-r4-reap-thread-economy.md` §2) measured the
FUSE-over-io_uring queue worker paying **1.9 µs/op of spinlock
contention** on the kern rand-4k row — `fuse_uring_req_end`'s queue lock
with the connection-wide `fch->bg_lock` nested in it, plus
`fuse_request_end`'s own `bg_lock` take — a lock every submitting fio
thread also takes. 0026 moves background admission to a **per-queue
ledger under `queue->lock`** (this queue's share of `max_background`;
for SqueezeFS's `max_background = queues × q_depth` the share is exactly
the queue's ent count) so neither uring path takes `bg_lock`; the
classical `/dev/fuse` path, fusectl and the congestion checks keep
working (details + the five correctness points: the patch's message,
`docs/design-kernel-bg-per-queue.md`, SERIES.md's 0026 paragraph).
**Compile-proven** (`make fs/fuse/ io_uring/` clean, `W=1` identical to
pristine, a `CONFIG_PROVE_LOCKING`/`DEBUG_SPINLOCK` build clean),
**not boot-tested** — the lever lands on the field A/B row
(`.benchmarks/rigs/2026-09-06-kernel-bg-per-queue-ab.sh`: kernel A =
0001–0025 vs B = +0026, same daemon, A A B B across reboots). **Backports
to 7.1.6 and 6.19.14 LANDED the same day as 0031 on each track**
(compile-proven three ways each; the adaptation ledger — 7.1's
`fuse_conn`-resident accounting with no accessors; 6.19's inline finish
in `fuse_request_end`, the ledger's second caller, plus its
`queue_refs == 0` abort arm — is the design note's §6, now "done").
