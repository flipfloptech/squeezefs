# Handoff — 2026-10-29, paused for a host reboot

`dev` = `origin/dev` = `0a93a6a`. The venue is quiet: no daemon, no mount, no venue slot held, the RAM
namespaces down. The host runs `7.3.0-1.7-mainline-sqz`; the product is **Linux ≥ 7.3 only** since
`1df4bdd` (owner ruling 2026-10-29). Owner rulings and standing rules: the memory topic
"owner preferences"; the decision ledger 0003 runs through §CU on `dev`.

## What landed since the 10-21 note (all gated, every perf change A-B-B-A'd; tables in `docs/evidence/`)

- **Metadata performance:** the create path's six cuts (§BY…), adaptive pass claims (§BX.12), the maintenance
  queue's coalescing (§BY), directory placement by depth over the metadata members (§BZ, `format
  --dir-spread-depth`, `config set-dir-spread-depth`; 2 MDT +16…+55 %), the overlay chunk bounded by bytes
  (§CA: 4 KiB-per-file creates ×2.7), small files through the write cache (§CD), fsync barriers only the
  members it depends on (§CH: fio fsync +54 %), the ino-keyed maps without tombstones (§CU: a reused mount
  no longer collapses — run 8 creates 62 → 206 k/s).
- **Interposer:** the interception mount isolated, coherence by invalidation (§CE: stats ×2–7 under the
  shim), FIEMAP through the interposer only (§CJ; the 12 grain-only xfstests by-design for preloaded runs).
- **Defects fixed (red-first):** a data-loss race in the rewrite epoch (§CN), the replay gate dropping a
  straggler (§CG), the writer's projection (§CF), the move matrix's lost create (§CQ), EAGAIN to RELEASE
  under fan-out (§CP), the `.stats` wedge (§CC), the RTTIME kill, generic/013 under the shim (§CK),
  generic/311's checkpoint tick (§CL), the handoff list (§CI), the fallocate double allocation (§CR), the
  follow-ups batch (§CS), host-noise test robustness (§CO), the 7.2 removal (§CT, −11.9 k lines).
- **README.md and QUICKSTART.md** exist (`9811f84`).

## In flight — three branches, each paused with its tree clean and resume steps in its status file

| Branch (worktree) | State | Next | Status file |
|---|---|---|---|
| `fix/pass-recall-wedge` (`.wt-recall-wedge`, `bbc104e`) | **THE WEDGE'S CAUSE FOUND**: a lost wake in `FutexTable.wake` (a fiber's queue link cleared after the bucket lock; a re-parked fiber's follower is never found). Fix + red unit + model committed; 21/21 repeated-mdtest runs clean | finish the ×10 mounts and rw4k ×10 loops, smoke rows, generic/001 013, §CM.6… rows, evidence; then gate + A-B-B-A | `~/tmp/journal-wedge-status.md` |
| `fix/orphan-supervisor` (`.wt-orphan-sup`, `25ff6df`) | a supervisor never outlives its mount (§CV); its gate (186) refused a stale smoke cost table; 3 rows red from the rig's `held` dir name colliding with a row's marker file (ENOTDIR → StatFailed) | the red + fix for a non-directory held path, rename the rig dir, the cache handoff ACK (a data-loss window: the old supervisor drops its copy at the send), regenerate `tests/costs/` (`git rm` the smoke table, `zig build costs`), gate | `~/tmp/orphan-sup-status.md` |
| `test/defrag-job-hold` (`.wt-defrag-hold`, `be35cde` wip) | `SQUEEZEFS_TEST_JOB_TASK_HOLD`; the DEFRAG, jobs-matrix and rekey rows act on a held job; 26/26 loaded, 20/20 idle | the final-head loops, CO.30's counts, the evidence section, turn the `wip:` commit into docs; gate | `~/tmp/defrag-hold-status.md` |

## Open defects (no owner yet)

- **FALLOCATE — EVERY MODE'S LAW fails on `dev` alone 2 of 3** (`error(squeezefs): data volume 1:
  StorageFull after parking 1000 ms … the plane is broken`, `smoke.zig:16585`); suspect the reservation
  fix `6e9fbdf` (§CR) — bisect first.
- generic/103's dismount promotion FAILED (a reservation ignores the write cache's claim — §CR.7).
- The other ino-keyed std maps with removals (read cache `InoMap`, pending times, `kv_fs_stripes` plain).
- The reclaim tick starves its other work during a long removal storm; `transport_lease_overlong` lines at
  load ~50; the 30 s unmount backstop seen once (not reproduced, §CS).
- §CI.8 not reproduced: the pending-free wedge under the HARD BACKSTOP, `block_claim_anomalies`.

## Owed to the owner

- **The 7.3 patches** (`scripts/kernel/patches-mainline/`): option 1 (recommended) drop the private 0005
  (`FUSE_TIME_LIMITS`) and 0007 (host-scoped NVMe-oF) with their daemon code, keep the upstream-bound
  0001–0004 + 0006 (FUSE_SYNCFS, in 7.4) in the venue kernel until upstream carries them; option 2 stock
  7.3, no patches (syncfs not durable until 7.4).
- RDMA as a third NVMe-oF transport (a few days, testable over soft-RoCE here); the `~/tmp` sweep; a LICENSE.

## Resume

1. `scripts/venue status` (kernel, modules, build root); `mount | grep squeezefs` empty.
2. Resume each paused agent from its status file (the three rows above), one gate at a time.
3. Benchmarks: AC power (`/sys/class/power_supply/AC/online` = 1), the scripts in `~/tmp/abba-*.sh` hold
   the gate lock and every slot, lift `RLIMIT_RTTIME` and wait out foreign workloads.
