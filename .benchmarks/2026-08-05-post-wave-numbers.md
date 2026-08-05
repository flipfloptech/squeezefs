# 2026-08-05 — Post-wave numbers (sessions 1+2 + kmbuf A/B + il ladder)

**Venue:** squeeze-test, pair `85b79c40` (the full D12 wave). **Instruments:**
`tests/fio/perf_session.sh` (sessions ×2, reproducible within noise),
`transport_ingress_sweep.sh` (A-B-B-A kmbuf bracket), ad-hoc il ladder
(engagement exact per row). Raw-ceiling rows: sessions' row 0 remains
instrument-junk (unprivileged colon-set fio) — grading uses the standing
41.8 GB/s / 3.36 M ceilings; the driver now label-and-skips without sudo.

## Scoreboard vs targets (user: "reads closer to writes, iops closer to 1M")

| row | pre-wave | post-wave | raw ceiling |
|---|---|---|---|
| seq write (kernel, 60 s) | 36–39 | 34.1–34.4 GB/s | ~42–44 |
| seq read (kernel, 60 s) | ~31 | 26.9 GB/s (shape differs — exa battery re-row owed) | 41.8 |
| rand-4k kernel | 273–280k | **364–393k (+33 %)** | 3.36 M |
| rand-4k il qd8 | 208k (−26 % vs kern) | **438–451k (+20 % vs kern)** | 3.36 M |
| rand-4k il qd32 | — | **531k** (new saturation, see below) | 3.36 M |
| durable write il/kern | 0.70–0.75 | **0.93–1.00 (parity)** | — |
| read il vs kern (all shapes) | −18..−21 % | **+13..+28 %** | — |

## Field verdicts closed

- **D12 item 4 (sharding):** il ≥ kernel MET (+17..+22 %, two sessions).
- **D12 item 2 (convoy):** durable-write parity, two sessions.
- **Lever-2 interleave fix engagement:** `groups=8 width=4` live on the
  2×16 interleaved field box (was 32×1 twice — first the contiguity bug,
  then the kmbuf carve-out).

## The kmbuf A-B-B-A (drain groups vs kmbuf — the displacement bracket)

A=kmbuf/width-1, B=user-ents/groups-8×4; both orders: PAR within the
bracket's noise (16×8: 388/396/396/382k; 32×8: 387/380/384/345k; 8×32:
437/433/432/416k). Verdict: no mount-option flip warranted; the kernel-
series kmbuf-bgid work (`.benchmarks/2026-08-05-drain-group-queue-workers.md`
citations: `FUSE_URING_RINGBUF_GROUP 0` hardcoded, `IOBL_PINNED`
exclusivity) files as speculative-composition, LOW priority. Cumulative
kernel-path movement on this instrument since the knee was named:
**32×8 273k → ~385k (+41 %); 8×32 357k → ~435k (+22 %)**.

## The new saturation (next iteration): il ~530k

il ladder 451k → 507k → 531k (qd 8/16/32) with clat DOUBLING per step —
queuing against a ~530k ceiling, far below the 3.36 M raw and below the
local 32×32 result (653k at the local raw ceiling). Live `ipc_direct_shards`
read 4–5 of the derived 8 (demand-driven spawn: governed submits are
landing on 4–5 service-thread lanes). Candidates for the decomposition:
lane concentration (fd-sharding of 32 sessions onto 8 svc threads),
shard-reaper CPU at the 235 µs RTT shape, sync-lane probe overhead at
qd32, session count (32 processes) vs the local 32-job repro. The 1M
program's next row.
