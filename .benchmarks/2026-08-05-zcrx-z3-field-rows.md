# 2026-08-05 — zcrx Z3 field rows: eight rounds to correct-and-safe; engagement economics filed

**Venue:** squeeze-test (real nvme-tcp fabric, 10 data namespaces behind one
mlx5 rail, 32 RX queues, MTU 9000, kernel 6.19-sqz). **Instrument:**
`tests/fio/zcrx_field_rows.sh` (A-B-B-A lane-on/off remounts, 60 s sustained
cold seq-read + rand-4k per side, engagement verdicts, mpstat/pidstat).
**Binaries:** wave `e1076bfc` → `3e052e0f` (rounds landed per table).
Artifacts: `/scratch/tmp/logs/zcrx_rows{,2..8}` on the cluster.

## What the campaign fixed (each round field-verified, red-first, on the wave)

| Round | Root cause (field → kernel/source-adjudicated) | Landed |
|---|---|---|
| 1 | Per-NIC rxq collision (EEXIST — same derived queue per session) → process-wide rxq arbiter, NIC-derived pool | `49ee9860` |
| 2 | mlx5 advertises a 0-slot ntuple table WITH the feature on (the advertisement lie); EEXIST recycle race; net_iov-unreadable admin flows (EFAULT) → kernel-verdict rule slots, fresh-first lease reuse, RSS-exclusion-before-register | `6e412ffe` |
| 3 | mlx5 refuses `RX_CLS_LOC_ANY` at the ioctl (ethtool userspace silently self-selects locs — source-cited); cancellable arm future leaked RSS exclusion; grant spread | `9dbec6d0` |
| 4 | Native-multipath c-path names broke the device census (want 4 instead of 1); ENOMEM-as-terminal; statics-never-drop teardown; admin-flow EFAULT window | `70e3acb0` |
| 5 | Park-never-wakes deadlock (partial fills hold every chunk ⇒ no release ⇒ no wake) → RecvGovernor unconditional bounded poll; ~937 ms failover bound (`LANE_READ_TIMEOUT/32`) | `1aefdc2c` |
| 6 | **The pool arithmetic**: the NIC RX descriptor ring is a standing consumer of the zcrx provider pool — 8192 descs × ⌈9000/4096⌉ = 96 MiB standing demand vs the 64 MiB area (v6.19.14 zcrx.c cited; freelist starts full, our returns were correct) → area = fill window + probed ring-standing bytes (160 MiB/queue, R5-gauged) | `798c01fc` |
| 6b | Teardown/watchdog race flake (abort has no completion edge) → take-abort-AWAIT ownership; ×10 tape | `ece3a416` |
| 7 | CQ sized `sq×4` = 512 vs chunk-grain demand 16,384 (≈256 CQEs per 1 MiB fill; -ENOSPC = CQ-full, zcrx never takes the NODROP path — cited) → `next_pow2(depth×⌈max_xfer/chunk⌉+sq)` clamp 65536; ENOSPC parks; gather≡fill moved to the whole-read boundary | `c9798b36` |
| 8 | Silent poison increment paths; per-segment admission shredding the window (provable worst case 0 completions); the standing-RSS no-harm violation → ONE loud poison funnel, whole-read atomic admission with clean declines, structural-starvation full teardown | `3e052e0f` |

## Final field state (round 8, binary `3e052e0f`)

| Row | seq-read | rand-4k | poisoned | closure |
|---|---|---|---|---|
| A1 (lane on) | 22.85 GB/s | 353k | **0** | `fill ≡ gather` = 704,774,144 byte-exact |
| B1 (off) | 26.44 GB/s | 382k | — | — |
| B2 (off) | 27.58 GB/s | 328k | — | — |
| A2 (lane on) | 25.68 GB/s | 338k | **0** | byte-exact |

`armed = 1` at snapshot (sessions survive their rows — first time);
`parks = 8` / `failovers = 16`, all bounded (no 30 s stalls); six consecutive
pristine teardowns (0 rules, full RSS after every mount, incl. failure paths).
Poison-free lane-on gap vs off: **−6.9 % / −13.6 %** (round 7: −27/−43;
round 3: −45/−69 with live NIC damage).

## Verdict (D12 board item 1)

The D5 acceptance — CPU/byte drop on sustained cold seq-read, throughput no
worse — is **not met at this geometry**: engagement is ~0.05 % of row bytes
(8 lanes × 1 queue × whole-read-atomic admission against 128 concurrent
4 MiB fills), so the copy-elimination thesis cannot pay the standing RSS-
exclusion rent (8 of 32 queues) the armed sessions charge the kernel path
that still serves the bulk. **The lane parks OPT-IN (`SQUEEZEFS_ZCRX_LANE=1`)
in a correct, honest, safe-by-construction state**; default stays off
(user sequencing 2026-08-03: default-on last, and only on a counted win).

**Filed follow-on (the engagement-economics program):** for the lane to WIN
its row it must serve the bulk, which needs (a) the §8 real derivations
(NIC-queue want + BDP depth) at a geometry where lane queues carry line-rate
share, (b) admission windows scaled to offered concurrency (the decline path
is now clean and cheap, so scaling is safe), and (c) lazy re-arm after
structural teardown. Until then every arm is opt-in measurement machinery.
The eight rounds' laws are all pinned in `zcrx_lane_tests` (58) /
`zcrx_steering_tests` (20) / in-module (36) — regression is red, not
re-diagnosis.
