# Pause state — 2026-09-05 (reboot for the firmware-latched CPUs)

Everything is landed and pushed: `dev` = `origin/dev` = `db5f08d5`, clean
tree, no unmerged branches, no worktrees besides the checkout, no daemons /
mounts / nvmet state on the box. Release **1.2.1** shipped earlier today
(`stable-2026.09.1` = `27a396e1`; artifacts in `dist/*-dist/`, the rocky8
pair live on `squeeze-test:/scratch/tmp/`).

## Why the reboot

The 1.2.1 fstests `-g auto` run's CPU-hotplug test (`generic/650`,
2026-09-04 22:48) left CPUs **8, 10, 20, 22, 28, 30 firmware-latched
offline** (26 of 32 online; every re-online attempt "failed to report alive
state" — the same hazard as the 2026-08-31 incident, reboot-only). Two
consequences on this box today:

1. default-queue-count nvme-tcp connects fail at I/O queue 15 (`Connect
   command failed, errno: -18`) while 4-queue connects (the plain dev
   substrate's) succeed — this blocks the zc-capability gate's bundled live
   NVMe-reservation leg and the multi-writer fleet's identity connect;
2. every A/B row of the day ran on 26 cores — identical for both arms of
   each bracket, so the verdicts stand; absolute numbers are 26-core
   numbers (noted in each record).

## Since the 1.2.1 tag (all on `dev`, 33 commits)

The five campaigns the user asked for (D-3, D-4, W-2, W-4, R-4 — W-1 had
already landed as the W-3 note; W-4 was substituted), each adjudicated by
same-binary A-B-B-A rows and landed on evidence:

| campaign | verdict | record |
|---|---|---|
| D-3 `perf/dlm-stripe-derivation` | LANDS — the 4a DLM tables (held across the commit park) were the colliding table, not the board's 1024-way suspects; mdstorm: collisions −76 %, 4a wait −58 %/op, throughput par | `2026-09-05-d3-dlm-stripe-derivation.md` |
| D-4 `perf/free-grace-accept` | design was ALREADY landed 2026-08-25 (status never flipped); the rate-equation harness + a seam fix landed; **fleet acceptance row blocked by the box** (below) | `2026-09-05-d4-free-grace-sustain.md` |
| W-2 `perf/write-stream-guard` | LANDS default on — the guard was already dropped before block I/O; the term was its exclusive MODE; w_fresh par-or-up both aged brackets, exclusive stream waits 16,384 → 241/leg | `2026-09-05-w2-write-stream-guard.md` |
| W-4 `perf/reclaim-derivation` | LANDS as derivation + instrument + event-driven park; **board premise withdrawn** — with discard elision on, the reclaim queue is off the rewrite path; the 2026-09-01 field rewrite tail is UNATTRIBUTED (open) | `2026-09-05-w4-reclaim-derivation.md` |
| R-4 `perf/read-zc-serve` | LANDS, **default flipped ON** — daemon CPU/GiB −24 % cold / −25 % warm on unaligned reads, `fuse3-ur` −37..−46 %, throughput ≥ par both orders; aligned direct leg untouched (null row) | `2026-09-05-r4-read-zc-serve.md` |

Plus two test-contract fixes the batch gate surfaced (both pre-existing or
default-flip related, neither a lever regression): the data-path
correctness suite's "durable view" steps now fsync before dropping the
caches (`bbf7138f` — the dirty size floor is ACKED-ONLY-HERE state by
design; ~1/30 short read under load on every tree back to 1.2.1), and the
bounce-pool routing test pins the size-class law rather than the heap
pool's identity (`db5f08d5`).

## Resume checklist (after the reboot, in this order)

1. `nproc` → 32; `cat /sys/devices/system/cpu/offline` → empty.
2. **Batch `task check` from zero on `dev`** (`db5f08d5` or later) — the
   five campaigns + the two test fixes have had two partial launches (each
   red on a since-fixed test contract, launches 1 and 2) and a third that
   the reboot interrupted at ~40 min. The gate is the landing condition.
   ~1 h. Log convention: `/tmp/five/gate/taskcheck.log`.
3. **zc-capability gate** as root here (`sudo tests/run_zc_capability_gate.sh`),
   under BOTH `SQUEEZEFS_READ_ZC_SERVE=1` and `=0`: the zero-copy surface
   was green ×2 today; the bundled live WERO leg is what the box state
   blocked. Also passes on `squeeze-test` (toolchain at
   `/scratch/tmp/sqz-agent/src-1.2`, fetch a bundle of `dev`).
4. **D-4 fleet acceptance row** (`.benchmarks/2026-09-05-d4-free-grace-sustain.md` §6):
   `sudo SQZ_DEVSUB_TRANSPORT=tcp tests/dev_substrate.sh create` →
   `sudo SQZ_MWFLEET_OSS_GB=32 SQZ_MWFLEET_RANGE_CUSTODY=1 tests/mw_fleet.sh create N=1 --cowriters=8`
   → `sudo tests/run_mw_matrix.sh s11-mpiio` from zero on a QUIET box
   (probe ≥ 750 MiB/s), then the sustain-rig columns per member and the
   A/B leg (`SQUEEZEFS_FREE_GRACE_DEMAND=0 SQUEEZEFS_FREE_GRACE_ACK_PIPELINE=0`).
   Then `tests/mw_fleet.sh teardown` + devsub teardown.
5. Then the **kernel COMMIT-lock split** campaign (7.2 patch track; the
   user builds kernels) — the next item the user named. R-4's note §8 names
   the optional kernel op-count rung beside it.

## Standing hazards worth remembering

- `generic/650` (fstests CPU hotplug) can latch cores off on this box;
  check `nproc` after every full fstests run before measuring anything.
- Release drivers: `/run/wrappers/bin` first on `PATH` (setuid
  `fusermount3`); `dist` builds run in a pinned worktree (the artifact check
  compares against the checkout's HEAD); assert the TEST's exit status when
  chaining a landing, not grep's; `skip_ledger_tests` mutates env — run it
  `--test-threads=1`.
- Finished subagent worktrees under `~/.grok/worktrees/source-squeezefs/`
  accumulate ~100 GB `target/` each — deleted today; root-owned rig logs
  remain (need `sudo rm` with a password).
- The `actions ⚠` flag in the user's EXA script is still unexplained.
