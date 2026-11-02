# Rung 10 — the stock kernel: no kernel patches (2026-11-02, decision 0003 §DB)

**The ruling.** The owner, 2026-11-02: "no kernel patches". The product's floor is stock
kernel.org Linux ≥ 7.3; the repo carries no kernel patch. The venue boots two kernels: the DEFAULT
entry an Omarchy-flavoured 7.3.1 (another branch; no FUSE, io_uring or NVMe change), the SECOND
this note's pure stock kernel.org 7.3.1 + `scripts/kernel/config-fragment`
(`linux-mainline-sqz`, `uname -r` `7.3.1-1-mainline-sqz`).

## What changed

| | change |
|---|---|
| kernel | `build-host-kernel` builds kernel.org 7.3.1 from the tarball (signature checked), the fragment asserted as a checklist, no patch, pkgrel 1. `--prepare-only`: PREPARE-OK, signature Passed, checklist OK, 6,315 modules. `patches-mainline/` (0001–0007) and `upstream/` deleted (git history) |
| 0005 `FUSE_TIME_LIMITS` | retired: no echo of bit 62, `init_out` = upstream's (`unused[11]` zero); two gauges, one e2e row, two abi units gone; one codec unit added (bit 62 never echoed, bytes 42..64 zero) |
| 0006 `FUSE_SYNCFS` | probed at ready: one syncfs(2) on the mount, judged by whether a FUSE_SYNCFS reached the dispatcher (`fuse_syncfs_reaches`, the probe's own in `fuse_syncfs_probes`); the loud line when absent; seam `SQUEEZEFS_TEST_FUSE_SYNCFS_UNOFFERED` (stock 7.3's offer); syncfs-durability smoke rows skip by the probe, naming it |
| 0007 host-scoped subsystems | the dead parameter read, gauge and provisioning deleted; the merged-head refusals stay and name stock remedies; the policy is an owner call (§DB.4) |
| docs | charter §1 items 4 and 10, §3, §4, §6.1; AGENTS.md venue; README, QUICKSTART; provisioning messages |

## What 7.3.1 lacks — and what is verified only on stock

| patch | without it on stock 7.3.1 | daemon | verify on stock (after the reboot) |
|---|---|---|---|
| 0001 shmem `IOCB_NOWAIT` | `mm/shmem.c` sets no `FMODE_NOWAIT` (7.3.1 line 2939): every io_uring read of a memfd punts to io-wq | none cheap (the zc slot is reachable by READ_FIXED alone) — performance | `iou-wrk-*` threads appear under warm zc reads; byte-exact rows unchanged |
| 0002 folio coalescing | per-page pins/bvecs for THP buffers | none — performance | none (correctness unchanged) |
| 0003 region short bulk alloc | `io_region_allocate_pages` fails a short bulk allocation with ENOMEM: a ring setup under fragmented memory fails loud (`RingSetupFailed`) | prepared, not built: `IORING_SETUP_NO_MMAP` with daemon-provided ring memory | any `RingSetupFailed`/ENOMEM in a gate log |
| 0004 abort of unclaimed requests | a stopped daemon's queued requests outlive the request-timeout abort | the supervisor's ladder bounds it (30 s, SIGKILL frees them) | THE ORPHANED SUPERVISOR (d) row |
| 0005 | no timestamp-range clamp in the VFS | the daemon saturates | fstests generic/634 |
| 0006 | syncfs/sync reach no daemon | the probe, the loud line, the skips | the probe row reads `fuse_syncfs_reaches 0`; the gated rows skip by name; the rows that land custody with `sync -f` (§DB.6 c) |
| 0007 | two identities on one subsystem merge under multipath=Y | refused, never served | nvmet two-identity fidelity rows need `nvme_core.multipath=N` |

## Checks on the patched kernel (`7.3.0-1.7-mainline-sqz`) before the reboot

- `zig fmt --check`, `zig build rails`: clean.
- Units `fuse.`/`nvmet`/`knobs`/`probe`: 519 pass, 18 skip (the nvmet fidelity tier's own step), 0 fail —
  the new dispatcher probe unit, the kv_fs probe unit, the codec bit-62 unit included.
- Smoke, normal offer: 50 init/syncfs rows green; THE FUSE_SYNCFS PROBE AT MOUNT prints
  `armed 1, reaches 1, probes 1, fuse_syncfs 0` and, under the seam, `armed 0, reaches 0, probes 0`.
- Smoke, stock offer (`SQUEEZEFS_TEST_FUSE_SYNCFS_UNOFFERED=1`, every daemon): 63 rows; the
  syncfs-durability rows skip naming the probe; five rows that landed custody with `sync -f` were red
  and are fixed (§DB.6 c), then green in both postures, the shim re-join row included.
- nvmet-smoke (multipath-tcp, daemon-connect-loop, fidelity-mount, …): 55 pass.
- `provision-local-venue`: no kernel demand; notes FUSE_SYNCFS and `nvme_core.multipath`.

Not run (venue-exclusive or the full gate's): the frozen-daemon and bring-up-failure host-sync rows;
the full gate (the orchestrator's).

## On the venue's 7.3.1-1-omarchy-mainline under `nvme_core.multipath=N` (decision 0003 §DE)

The first gate there (dev baf1fcc) had five nvmet-smoke rows red: daemon-connect-{loop,tcp} and sigstop-preempt refused by the head verdict (stock 7.3.1 links both identities' controllers into one headless `nvme-subsys` dir — a dir without a head now answers no head), fidelity-mount (loop) and guard-tcp read an empty payload after kill -9 (`sync -f` reaches no daemon — the rows sync by the file). At code head 2dc8ad5: nvmet-smoke 54 pass, 1 skip (multipath-tcp, by name) ×3; units `nvmet` 85 pass, 15 skip (the tier's own step); `zig fmt --check`, `zig build rails` clean.
