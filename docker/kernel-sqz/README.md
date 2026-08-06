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

**v2 (2026-08-04 prep):** the series grew sqz patch **0027**
(`FUSE_TIME_LIMITS` INIT advertisement — V2-CANDIDATES.md rank 1, the
only kernel delta of the v2 manifest): `fuse_init_out` gains
`time_min`/`time_max` i64s carved from `unused[11]` (struct stays 64
bytes, fields naturally aligned), flags2-space bit 62, and a guarded
`sb->s_time_min/max` branch beside the `time_gran` consumption in
`process_init_reply`. Converts the fstests generic/634 release-gate
adjudication into expected-PASS **on sqz-kernel hosts only** (the
adjudication stays pinned for the fleet kernel). Feature-absent ⇒
bit-identical behavior on both sides.

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
| `build-kernel.sh` | in-container: sha256-pinned tarball → 27 patches → config assembly → checklist assertion (fail loud) → `make binrpm-pkg` |
| `SERIES.md` | the series manifest: message-ids, base ruling, conflict resolutions |
| `V2-CANDIDATES.md` | the v2 scoping manifest (ranked candidates; rank 1 = the authored 0027) |
| `patches/` | `git format-patch` export of the resolved transplant (25 series patches + 2 sqz-authored commits: 0026 seam, 0027 FUSE_TIME_LIMITS) |
| `patches-7.1/` | the **linux-7.1.6 rebase** of the same 27 patches (the D13 latest-mainline track — see *The 7.1 track* below) |
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
* `kmbuf_smoke.c` — `IORING_REGISTER_KMBUF_RING` (=37) with valid args:
  success ⇒ the FUSE-zc io_uring surface is present (the sqz kernel);
  EINVAL ⇒ absent (stock kernels).
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

**Base: linux-7.1.6** (kernel.org; the CachyOS 7.1.6-1 line). The same 27
patches, semantically rebased 2026-08-06 onto 7.1.6 for local
zc-capable boots via the CachyOS kernel manager. **6.19.14 stays the
FIELD series** (the EL8 fleet RPMs, `patches/`); 7.1 is the D13
latest-mainline track. Concatenated manager-ready form:
`sqz-kmbuf-zc-7.1.6.patch` = `cat patches-7.1/00*.patch` — applies
sequentially `patch -p1 --fuzz=0` clean (verified on a fresh pristine
7.1.6 extraction, per-patch dry-run in sequence + full-tree byte compare;
compile-proof: `make io_uring/ fs/fuse/ drivers/block/ublk_drv.o` with
the running CachyOS 7.1.6 config, zero warnings).

### ABI/opcode audit (the one collision — RENUMBERED, loudly)

* **`IORING_REGISTER_KMBUF_RING` 37 → 38, `IORING_UNREGISTER_KMBUF_RING`
  38 → 39.** Upstream 7.1 allocated **37 = `IORING_REGISTER_BPF_FILTER`**
  (`CONFIG_IO_URING_BPF`, enabled in the CachyOS config), so keeping 37
  was impossible (duplicate case in `io_uring/register.c`; a userspace
  probe of 37 would hit BPF-filter semantics, not EINVAL). **This breaks
  numeric lockstep with the deployed 6.19.14-sqz field kernel and the
  fuse3 fork's constants** (`crates/fuse3/src/raw/connection/kmbuf.rs`
  pins `IORING_REGISTER_KMBUF_RING: u32 = 37`, and
  `probes/kmbuf_smoke.c` probes 37): daemon/probe work on a 7.1.6-sqz
  host needs a per-kernel opcode (NOT retrofitted here — flagged as the
  open decision point). On a 7.1.6-sqz kernel, opcode 37 is BPF_FILTER:
  the existing probe/daemon would read the surface as Absent (or worse,
  ambiguous), never mis-arm.
* `IORING_OFF_KMBUF_RING 0x88000000` — free in 7.1.6 (PBUF 0x80000000,
  PARAM 0x20000000, ZCRX 0x30000000, mask 0xf8000000): **unchanged**.
* `FUSE_URING_BUF_RING (1<<0)` / `FUSE_URING_ZERO_COPY (1<<1)` in the
  `fuse_uring_cmd_req.init` union, and `FUSE_TIME_LIMITS (1ULL<<62)` —
  all still free in 7.1.6 (`fuse_uring_cmd_req` still `padding[6]`;
  INIT-flag watermark still bit 42): **unchanged**.

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
| 0026 seam | ported as-is — **still required**: 7.1's `io_buffer_add_list()` is the int-returning stable form, and patch 03's register path inherits the unchecked call |

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
