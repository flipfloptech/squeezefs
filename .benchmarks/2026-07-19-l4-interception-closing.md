# L4 LD_PRELOAD interception — program closing report (PR L4-8)

Program: `docs/design-preload-interception.md` (PRs L4-0 … L4-8).
Box: 32-CPU dev box, devsub (`tests/dev_substrate.sh` nvmet-loop: null_blk
meta + zram data) for every gate number; scoreboard btrfs-image substrate
for the harness-mode validation only. Every row states its instrument.

## G-L4-2 adjudication — MET on both rows

Drivers are the §5.8.3-fixed lines: **elbencho sync positional
`--rand -b 4k --direct`**, warm `-t 8`, device-true `-t 256`, **no
`--iodepth`** (libaio is not intercepted in v1). Engagement was exact on
every run: `ipc_ops_read` delta = 524,288 = the full 2 GiB dataset
coverage (elbencho is coverage-bound at these rates, so runs end early —
the deltas equal the workload's total op count, the strongest form of
the §3 rule-4 proof). 3 runs per leg, medians adjudicate.

| Leg | Runs (IOPS) | Median | Floor | Target | Verdict |
|---|---|---|---|---|---|
| **il-warm** (mount `--interception`, hot tier resident, svc threads auto=8) | 971,709 / 1,017,548 / 1,020,676 | **1,017,548** | 644,726 (kernel-FUSE warm) | 1.0 M | **floor ✓ 1.58×, target ✓ at median** (1 of 3 runs at 972 k — stated) |
| **il-device-true** (`-o direct_device_true`) | 523,995 / 622,112 / 642,504 | **622,112** | 600 k (≈ kernel-FUSE dt 604,313) | substrate-class | **floor ✓; beats the kernel-FUSE reference** (r1 below floor — first-pass-after-mount cold effects; median governs) |

Serve composition: warm = 97 % sync fast-path serves (the §5.5.1
tier→arena engine); device-true = 100 % handoffs by design (tier serves
are gated off under the diagnostic posture — a quiet tier serve there
would relabel the row warm).

## What it took (the pre-closing lever sweep, all profile-driven)

| Lever | Finding | Effect (device-true / warm) |
|---|---|---|
| Arc'd handoffs | per-op `SqueezefsFilesystem::clone` = 48.9 % of daemon CPU (arc-swap Debt serialization) | dt 123 k → 285 k |
| fd-sharded sessions (`SQUEEZEFS_IL_SESSIONS`, default 4) | one session = one pinned service thread (§5.5.1); single-process clients were bound to one dequeue thread | dt 285 k → 380 k @ t16 |
| Adaptive spin (park-history bit; `SQUEEZEFS_IL_SPINS` pins) | fixed 4096-spin pre-park window was client CPU theft — inverse thread scaling | dt monotone to 672 k @ fio j256 |
| Service threads default `clamp(cpus/4,2,8)` | fast-path serves execute ON service threads; 2 ⇒ 643 k warm, 8 ⇒ 1.48 M (fio) | warm target reachable |
| Sync tier serve (L4-8 prerequisite commit) | v1 fast path served only active buffers — every warm read paid the handoff | warm 308 k → 1.27 M (fio j16) |

## KD-11 write-through cost (the priced A/B)

Buffered **kernel-path** 4 KiB sequential dd (256 MiB), 3 runs each:
write-through (interception mount) 44/46/46 MiB/s vs writeback (default
mount) 143/140/140 MiB/s — **≈ 3× tax on unintercepted buffered small
writes**, as the design priced ("the tax falls on the residual
unintercepted-writer mix"). Intercepted writers don't pay it: il
seq-write measured 2.6–2.8 GiB/s durable through the ring on the
scoreboard substrate.

## Scoreboard il mode (landed; the release-gate surface)

`SQUEEZEFS_SB_MODES=il`: sqz-only separately-labeled table, engagement
INVALID machinery (exit-nonzero), §5.8.3 driver + §5.8.4 rail lines
per row, off-rail companions, N/S rows for metadata storms and
buffered-il. Full non-smoke validation run: rc=0, 22/22 rows engaged
exactly. Findings the machinery caught during bring-up, kept as
regression posture:

- **Static-instrument fraud**: the pinned scoreboard elbencho (3.1-9)
  is statically linked — LD_PRELOAD cannot load; rows silently measure
  kernel FUSE. `il_preflight` refuses static instruments and records
  the dynamic substitution (3.1-10 system binary) per row.
- **KD-7 mid-run skew**: a commit landing between daemon and shim
  builds refused every session (inequality is never forgiven, even
  with the dev override) — all 12 rows INVALIDed exactly as chartered;
  `il_preflight` now builds both ends from one tree state.
- The harness runs **unprivileged** (rootless FUSE): under sudo, the
  mountpoint wait fails because FUSE denies non-mounter access without
  `allow_other` — root included.

## Cost-model reconciliation (§5.8 assumptions vs measured)

- Warm per-op: predicted ≥ 650 k/core serve-shaped (G-L4-1); measured
  1.02 M across 8 service threads *including* client cost on the same
  box — consistent once the serve is a real tier lookup rather than the
  rig's stand-in.
- Device-true: §5.8.3's "(a) `-t 256` sync threads ⇒ 1.2–1.5 M
  plausible" was optimistic on this substrate — the row is
  daemon-CPU-bound (handler + router + uring per op), not
  Little's-law-bound; 622 k ≈ kernel parity + spare. The §5.8.4
  occupancy table's "client cost ~5–6 µs/op" held; the spin-window
  correction (adaptive spin) was the missing term.

## OQ-1 (libaio) — recommendation

**Keep in v1.1, but demoted from floor-rescue to economics.** Both
G-L4-2 floors are met with sync-thread drivers; libaio interposers now
buy (a) client-thread economy (one submitter holding hundreds in
flight frees cage CPUs for service threads — the measured contention
direction), and (b) instrument compatibility (the literal
`--iodepth 16` charter line). The mixed-batch completion-merge surface
remains the cost; no floor depends on it.

## Honest residuals

- The warm target is met at median with one run at 972 k — the row is
  service-thread-CPU-bound and shares the box with clients; the
  0-15-cage occupancy story in §5.8.4 stands.
- il metadata ops (stat/del) are kernel-FUSE by design: N/S, not a
  regression.
- The elbencho il legs are coverage-bound at 2 GiB (engagement = total
  ops); longer datasets would exercise eviction under the same driver —
  the fio sweeps (larger, time-based) bound that behavior separately.
- Scoreboard-substrate il rows (btrfs images, 16 GiB dataset vs 4 GiB
  budget) are tier-miss-dominated by construction — comparable only
  within that table, never to the devsub gate numbers above.
