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

**v2 (2026-08-04 + 2026-08-09):** the series is 29 patches. 2026-08-04
authored `FUSE_TIME_LIMITS` (now **0028**). 2026-08-09 inserted **0025**
(abort-race: imu-held folio refs on zc registrations) after Koong 0024
and appended **0029** (zc payload retention — Approach B ACK-early
accelerator). Old 0025–0027 became 0026–0028. Selective zc delivery
(0030) is not in the series — §4.3 gate rides the boot-test 4 KiB
armed-vs-disarmed row. Spec `docs/design-zc-write-kernel-v2.md`,
evidence `.benchmarks/2026-08-09-kernel-zc-write-v2.md`.

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
| `build-kernel.sh` | in-container: sha256-pinned tarball → 29 patches → config assembly → checklist assertion (fail loud) → `make binrpm-pkg` |
| `SERIES.md` | the series manifest: message-ids, base ruling, conflict resolutions, 0025/0029 |
| `V2-CANDIDATES.md` | the v2 scoping manifest (ranked candidates; rank 1 = TIME_LIMITS, now patch 0028) |
| `patches/` | `git format-patch` export (0001–0024 Koong + 0025 abort-race + 0026 docs + 0027 seam + 0028 TIME_LIMITS + 0029 retention) |
| `patches-7.1/` | the **linux-7.1.6 rebase** of the same 29 patches (the D13 latest-mainline track — see *The 7.1 track* below) |
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
```

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

**Base: linux-7.1.6** (kernel.org; the CachyOS 7.1.6-1 line). The same 29
patches, semantically rebased 2026-08-06 onto 7.1.6 (0025/0029 landed
2026-08-09) for local zc-capable boots via the CachyOS kernel manager.
**6.19.14 stays the FIELD series** (the EL8 fleet RPMs, `patches/`); 7.1
is the D13 latest-mainline track. Concatenated manager-ready form:
`~/sqz-kmbuf-zc-7.1.6-v2.patch` = `cat patches-7.1/00*.patch` — applies
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
