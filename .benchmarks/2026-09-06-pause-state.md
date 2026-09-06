# Pause state — 2026-09-06 (reboot into the patched kernel)

`dev` = `origin/dev` = `08483c51`, clean, no unmerged branches; the gate
worktree `~/Source/squeezefs-gate` exists (pinned, detached) and is the ONLY
place `task check` runs from now (the 11:45 red was a rebase in the main
checkout moving HEAD under a running gate). `squeeze-test` is idle with no
mount (the A-arm rig reset the cluster; remount is part of every arm).

## Landed since the last pause (2026-09-05), all fleet- or loom-proven

| item | commit | outcome |
|---|---|---|
| Co-writer ENOSPC wedge | `ac717c7f` | fixed; fleet: watchdog 37k → 2–4, all mounts answer |
| Finding-15 supply leak | `380ea732` | fixed; refusals 3,449 → 148; the shortage moved INTO the free-grace loop |
| Hold-time levers (b) ack-on-renewal, (d) harvest-on-ack | `c1450c66` | bound age 8,994 → 7,936 ms (−12 %, as priced) |
| F15 term 2 — lane-visible push | `8326bb81` | `min_acked→released` 2,500 → 15 ms; hold 10.9 → 7.4 s |
| F15 term 3 — **fenced-close data loss** (a rotated fencing token at epoch close took the genuine-fence discard arm) | `8326bb81` | 0 fenced closes, 0 fsync failures on the fleet; anomalies −94 %; the 154 refused frees attributed (≡ Σ `own_lane_untracked` 156) |
| CQE doorbell lost wake (v3 latch: a reaped-prior completion paid a stale mark) | `08483c51` | loom-red → fixed (mark-valued latch, no parker clear); IPC_ABI stays 6 |
| Kernel COMMIT-lock split | `9edcf624`, `a05998c5` | patches on all THREE tracks (7.2 = 0026, 6.19.14 = 0031, 7.1 = 0031), compile-proven incl. lockdep |
| Kernel A/B, **A arm recorded** | `e37d1932` | un-patched 6.19.14-sqz on squeeze-test, two same-state runs: rand-4k 505–511k IOPS, p50 194 µs, `fuse3-ur` ≈ 23 µs/op, lock ledger 12.6–13.1 % slowpath + 3.3 % raw_spin_lock |
| Bench smoke race (`timeout_cycle`) | `df938b0d` | observes the first-poll race instead of asserting it |

## Open

1. **Batch `task check` on `08483c51`** — NOT yet green on this exact tip:
   the last full launch (11:50, pinned worktree) went red only on the
   doorbell test now fixed; 1,477 tests had passed before it. Run from the
   gate worktree: `bash /tmp/five/gate/gate.sh 08483c51` (the script
   checks out the commit in `~/Source/squeezefs-gate` and writes
   `/tmp/five/gate/taskcheck.{log,exit}`) — or, if `/tmp` did not survive
   the reboot, `cd ~/Source/squeezefs-gate && git checkout --detach
   08483c51 && task check`. ~50 min on a quiet box.
   **DONE — GREEN on `6cbbd848`** (16:14–17:05, pinned worktree, from
   zero): 357 suites / **4,721 tests**, every stage (root + fuse3 clippy
   both configs, fmt, test, doc, bench smoke, loom build, fuzz check,
   docs, audit — the one allowed warning is the adjudicated
   RUSTSEC-2025-0141 `bincode 1.3.3`, rc-manifest §5). The intermediate
   run on `7af1a0d2` went red ONLY in the bench-harness smoke: three
   `record.rs` unit tests raced on the process-global
   `META_KV_DELTA_ORPHANS` (serial in the test stage, parallel in the
   bench harness) — fixed `66a71e35` (a test-module mutex on exactly
   those three; 1/40 → 0/200), product path untouched. Item CLOSED.
2. **Kernel A/B, B arm** — after `squeeze-test` boots the 6.19.14 series
   WITH 0031 (or whichever box carries the patched kernel): on the box,
   `cd /scratch/tmp/sqz-agent/k26 && sudo env ARM=B KERNEL_TAG=<uname -r
   substring> BIN=/scratch/tmp/squeezefs IL=/scratch/tmp/libsqueezefs_il.so
   bash 2026-09-06-kernel-bg-per-queue-ab.sh` twice (A A B B; the rig
   refuses a wrong-arm kernel via `fuse_uring_bg_wait` in the module
   symbols — `SYMCHECK=advisory` on an LTO kernel). Same 1.2.1 dist
   binary as the A arm. Verdict columns: rand-4k IOPS vs 505–511k, p50 vs
   194 µs, `fuse3-ur` µs/op vs 23, `lock.txt` slowpath share vs 12.6–13.1 %.
   If the patched kernel is the LAPTOP's (7.1/7.2 track), the laptop needs
   its own A arm first — it was not recorded (the rig's venue is the box).
   **Update (same day, after the laptop reboot):** the laptop is on
   `7.2.3-cachyos-lto` WITH the 7.2 patch — the patch's first runtime is
   recorded as `docs/design-kernel-bg-per-queue.md` §5a (zc gate 149/149
   both postures, a 779k-IOPS rand-4k load row, every tripwire 0, zero
   fuse/uring kernel-log lines). The laptop cannot be an A/B venue (its
   kernel moved 7.1.8 → 7.2.3 in the same boot); the B arm stays the box's.
   **DONE (same day, 19:51–20:05 UTC):** both B boots on squeeze-test —
   `.benchmarks/2026-09-06-kernel-bg-per-queue-ab.md`, **B ships**
   (worker −18 % µs/op, slowpath 12.8 → 0.11 %, kern rand-4k +9 %, p99
   −13…−17 %, controls par, no WARN). Item CLOSED.
3. **Finding 15 term 1 — the user's decision**: the remaining 7.0 s of
   hold are the reader ack ladder's two derived coherence windows (qualify
   ≈ 2 s, drain ≈ 4 s, × min over 8 members). Four KD-FG-11 items
   (`.benchmarks/2026-09-06-free-grace-hold-time.md` §7) would cut
   ≈ 3–3.5 s at the same safety and put 4 GiB lanes at ≈ 50 % headroom;
   until then the s11-mpiio sustained gate stays FAIL by design.
4. **1.2.2** — five multi-writer data-path correctness fixes since 1.2.1
   (wedge, leak, fenced-close data loss, doorbell lost wake, plus the
   levers); strongly warranted once (1) is green: release gate from zero in
   a pinned worktree (`/tmp/release-1.2.1/driver.sh` pattern + the zc leg
   both postures), tag `stable-2026.09.2`, `dist:all` in the pinned
   worktree, ship.
5. Smaller named items: the `own_lane_untracked` refused-free lineage
   (156/row; a stale layout refetch on the non-recomputed paths); the ~38
   no-fence anomaly residue; the co-writer fsync wait behind the 1 s park
   wall on cache-less mounts (term 2 note §7); the two-parker cross-pending
   doorbell strand (pre-existing, needs an ABI-bumping second word).

## Standing hazards

- Check `nproc` = 32 and `/sys/devices/system/cpu/offline` empty after
  the reboot (the 09-04 latch).
- Gates and `dist` builds ONLY in pinned worktrees; measurement rows never
  concurrent with a gate; assert the TEST's exit status when chaining a
  landing; `skip_ledger_tests` needs `--test-threads=1`.
