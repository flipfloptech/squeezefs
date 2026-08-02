# 2026-08-02 — FUSE-1: minor 36 + FUSE_INIT_EXT — live sqz-kernel A/B (pre-window leg)

Branch `fix/fuse-init-ext` (off dev `391dec2`, **unmerged — the orchestrator
merges**). Charter: the D6 ruling (`docs/pre-rc-execution-plan.md` —
mainline-correct, kernel patch set frozen): land FUSE-1
(`docs/pre-rc-engineering-spec.md` §4) and prove the negotiation live on the
cluster's running v1 sqz kernel BEFORE the reset-v5 window, so the window's
generic/634-on-live-bit-62 row tests an engaged arm.

## What shipped

| SHA | Commit |
|---|---|
| `53b887d` | test(fuse3): the INIT_EXT + minor-36 negotiation contract (RED — 2 tests against the pinned 31) |
| `3049b6d` | fix(fuse3): minor 36 + FUSE_INIT_EXT so the kernel folds the reply's flags2 (GREEN — 68/68) |

Reply minor = `min(kernel_minor, 36)`; `FUSE_INIT_EXT` iff the kernel offered
it AND reply `flags2 ≠ 0` (+ the ≥ 36 coherence guard). The 31→36 audit found
**zero kernel `fc->minor` gates in (23, 45]** (fs/fuse exhaustive, v6.12 AND
master, identical sets; the 27-patch sqz series adds none) — the bump
advertises no capability; the only behavioral delta is the INIT_EXT fold
engaging (`fc->io_uring` latches from the negotiated bit; patch-0027's
`FUSE_TIME_LIMITS` branch becomes reachable).

## The live venue (converged reset-v5 shape, REAL namespaces — zero local files)

`cluster_reset_v4.sh` first-ever execution (user-authorized), 5 nodes × (1
meta + 2 data) memory-backed null_blk over **nvme-tcp**, cache-less format,
bit 6 stamped (`meta_routing_width: 65536`). Kernel: 6.19.14-sqz (v1 — no
patch 0027). Two first-run script defects were found and fixed on
`fix/cluster-reset-ssh-quoting` (`a97204b` remote re-parse of the multi-word
`IPS` env; `a1d559a` enumeration teardown must sweep the product share
LEDGER, not just configfs — only oss2 collided because its old/new epoch NQNs
are identical). Third run: end-to-end clean, RESET-EXIT=0.

## Evidence

**1. Fold engagement + transport arm (the risk-note posture change).** The
FUSE-1 daemon (`3049b6d`, rocky8 pair, KD-7, sha `d08b5601…` on client + all
5 nodes) mounted and armed cleanly on FUSE 7.45:
`FUSE-over-io_uring registered: queues=32 depth=32 payload_sz=1048576
max_write=1048576 max_pages=256 buffers=kmbuf-bufring` → session armed →
classical sideband armed. With `FUSE_INIT_EXT` now set, `fc->io_uring`
latches from the *negotiated* bit for the first time (the kernel's
`fuse_block_alloc` arm-window gating engages) — no stalls, no dmesg fuse
warnings, clean umount/remount cycles ×5 across the bracket.

**2. A-B-B-A parity bracket** (same aged store, both orders; instrument:
`dd` 2 GiB O_DIRECT single-stream seq per leg — a PARITY/engagement smoke,
deliberately not a sustained headline row; the canonical counted rows are the
window's):

| Leg | Binary | write | read |
|---|---|---|---|
| A1 | `3049b6d` (FUSE-1) | 3.3 GB/s | 4.4 GB/s |
| B1 | `391dec2` (control = A minus the 2 FUSE-1 commits) | 3.4 GB/s | 4.4 GB/s |
| B2 | `391dec2` | 3.3 GB/s | 4.3 GB/s |
| A2 | `3049b6d` | 3.3 GB/s | 4.5 GB/s |

Parity within noise, both orders. Final state: A mounted, cluster healthy.

**3. The v1 timestamp "before" face** (recorded for the window's delta):
`touch -d 2400-01-01` on the mount returns `2262-04-11 23:47:16` — the
DAEMON's clamp echoed through the SETATTR round-trip. The kernel-local faces
that diff generic/634 remain on v1 (no `s_time_max` without patch 0027, by
design). The window's v2 boot + this daemon = the full bit-62 engagement row.

**4. Bonus observation** (not FUSE-1's): the v1 kernel offers unknown bit 42
(`FUSE_REQUEST_TIMEOUT`) — the daemon logs it as a D2.d adoption follow-up.

## Accidental cross-validation

The first B-side attempt used the pre-merge-train `.prev` binary (Aug 1): it
**refused the fresh volume loud** — `unknown incompatible feature bits on v3
superblock: bit 6 (0x40) — upgrade squeezefs` — the KD-14/bit-6 refusal
working verbatim in the field. The proper control (`391dec2`, bit-6-capable)
was built and staged instead (`5bfa7d2f…`).

## Standing state after this note

- Cluster: converged reset-v5 shape, FUSE-1 pair mounted at
  `/scratch/tmp/test`, staged v2 kernel RPMs untouched, `.prev`/campaign
  binaries retained.
- Merge order per D6: this branch (+ the reset-script fixes) → dev before the
  window opens; the window pair inherits FUSE-1; generic/634 row is then
  honest.
