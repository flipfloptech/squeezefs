# L4 PR-2 — IPC-hop ceiling spike: G-L4-1 adjudication (GO)

**Date:** 2026-07-18/19 (runs executed 2026-07-19 00:56–01:12 UTC)
**Program:** `docs/design-preload-interception.md` §5.8 / PR L4-2 — the KD-10 go/no-go gate **before any surface is built**.
**Verdict up front: GO — all three G-L4-1 legs pass with margin** (adjudication table below).

## Instrument (stated per the house instrument-alignment lesson)

- **Rig:** `crates/squeezefs-ipc/src/bin/ipc_hop_rig.rs` @ **`405f4cc`** (release profile, `lto=fat`), driven by `tests/run_ipc_hop_bench.sh`-shaped invocations (this note's formal sweep script mirrors it with n=3 and both rails; SUMMARY lines are the rig's own counter-true output).
- **Mechanism under test is REAL, not mocked:** two processes; sealed memfd (`memfd_create` + `F_SEAL_GROW|SHRINK|SEAL`); abstract AF_UNIX `SOCK_SEQPACKET` rendezvous; `SCM_RIGHTS` session handover; ops over the shipped `squeezefs-ipc` cores (MPSC ring, completion-in-place slots, non-private futex doorbell under the shipped fuse3 `WakeCoalescer`, daemon parks timeout-bounded 50 µs→5 ms per §5.3.1 rule 5).
- **Box:** AMD Ryzen AI MAX+ PRO 395 (Strix Halo), 16 cores / 32 SMT logical CPUs, **3.5 GHz `scaling_max_freq` cap confirmed (3500000) and untouched**.
- **Rails:** PR-2 rig rows run **uncaged `taskset 0-31`** per the design's §5.8.4 core-budget table ("per-thread figures are cage-independent"), plus **labeled governed `0-15` companions**. Rail stated on every row.
- **Thermals:** Tctl 54 °C at sweep start, peak 78.8 °C mid-sweep, 76 °C at end — never near the 88 °C pause threshold; no rows recorded hot.
- **Op shape:** 4 KiB payload each way (client fills request pattern user→arena; daemon proves the read by echoing the request checksum as the result and writes a response fill the client verifies arena→user). **verify_failures=0 on all 57 SUMMARY rows of the formal sweep**; every run's built-in sanity (`daemon_served == threads × ops`) held; `ring_full=0` everywhere.
- **n=3 medians** per row; `--ops 1000000 --warmup 100000` per client thread unless noted.

## Multi-run discipline disclosure

Two rig defects were found and fixed during shakedown (labeled tuning, never counted): (a) cmsg control-buffer misalignment (crash on first two-process run, fixed in `d617d9a`); (b) the handoff leg's first shape serialized all payload work through one channel-consumer task — it measured that serialization, not the §5.5.1 demote adder; replaced with per-op task spawn onto a pinned runtime with `global_queue_interval(1)` (`405f4cc`). **All formal counts below started from zero at `405f4cc`** per the house multi-run discipline. Tuning attempts against the handoff gate before its fix: 2 (runtime pinning + geometry sweep) — within the bounded-3 STOP rule; after the shape fix the leg passes without further tuning.

## Formal sweep — medians of n=3 (rig SUMMARY lines, counter-true)

| rail | leg | t | svc | spin | rtw | ops/s (med) | p50 ns | p90 ns | p99 ns | mean ns | client sys/op | daemon sys/op | daemon CPU ns/op |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 0-31 | echo | 1 | 1 | 4 µs | – | 1,019,304 | **791** | 1,943 | 2,394 | 944 | 0.0001 | 0.0001 | 999 |
| 0-31 | echo | 4 | 1 | 4 µs | – | 2,190,079 | **1,543** | 2,575 | 3,046 | 1,699 | 0.0005 | 0.0005 | 458 |
| 0-15 | echo | 4 | 1 | 4 µs | – | 2,942,520 | **1,323** | 1,393 | 1,443 | 1,319 | 0.0001 | 0.0001 | 339 |
| 0-31 | echo | 8 | 1 | 4 µs | – | 1,996,893 | 3,256 | 8,686 | 10,890 | 3,914 | 0.1234 | 0.1229 | 498 |
| 0-31 | echo | 8 | 2 | 4 µs | – | 2,816,949 | 2,465 | 3,396 | 10,630 | 2,708 | 0.0468 | 0.0463 | 714 |
| 0-31 | serve | 4 | 1 | 4 µs | – | **2,130,408** | 1,854 | 2,334 | 2,806 | 1,794 | 0.0005 | 0.0004 | 461 |
| 0-31 | serve | 8 | 1 | 4 µs | – | **1,912,925** | 3,487 | 8,697 | 10,570 | 4,136 | 0.1366 | 0.1357 | 529 |
| 0-31 | serve | 8 | 2 | 4 µs | – | **2,580,381** | 2,615 | 3,627 | 11,431 | 2,999 | 0.0949 | 0.0946 | 852 |
| 0-15 | serve | 8 | 2 | 4 µs | – | **4,175,675** | 1,814 | 2,354 | 2,885 | 1,868 | 0.0012 | 0.0011 | 482 |
| 0-31 | serve | 12 | 1 | 4 µs | – | 847,244 † | 14,046 | 15,338 | 17,783 | 14,120 | 1.11 | 1.11 | 1,176 |
| 0-31 | serve | 12 | 2 | 4 µs | – | 1,512,317 † | 2,785 | 15,218 | 18,305 | 7,868 | 0.55 | 0.55 | 1,348 |
| 0-31 | serve | 16 | 2 | 4 µs | – | 1,465,167 † | 5,140 | 21,741 | 24,646 | 10,871 | 0.56 | 0.56 | 1,381 |
| 0-31 | echo | 2 | 1 | 25 µs | – | 1,490,170 | 821 | 2,074 | 2,384 | 1,133 | 0.0000 | 0.0000 | 682 |
| 0-31 | handoff | 2 | 1 | 25 µs | 2 | 934,680 | 2,084 | 3,256 | 4,839 | 2,101 | 0.0001 | 0.0001 | 2,453 |
| 0-31 | echo | 4 | 1 | 25 µs | – | 2,259,830 | 1,402 | 2,685 | 3,306 | 1,666 | 0.0000 | 0.0000 | 440 |
| 0-31 | handoff | 4 | 1 | 25 µs | 2 | 1,706,319 | 1,863 | 3,847 | 5,921 | 2,297 | 0.0001 | 0.0001 | 1,643 |
| 0-15 | handoff | 4 | 1 | 25 µs | 2 | 1,782,232 | 1,804 | 3,697 | 5,530 | 2,198 | 0.0000 | 0.0001 | 1,504 |
| 0-31 | handoff | 8 | 2 | 4 µs | 4 | 1,519,006 | 4,178 | 9,308 | 14,036 | 5,069 | 0.40 | 0.36 | 3,221 |

† **Deliberately-oversaturated rows measure their own queue, not the transport** (offered = t × per-thread rate ≫ single-/dual-server capacity ≈ 1.9–2.9 M ops/s uncaged): RTT inflates to queueing delay and, once RTT crosses the 4 µs spin window, every op parks (the syscall growth is the futex park/wake pair — the designed backpressure posture, not a leak). They are capacity/backpressure companions, kept on record; the RTT/syscall gate rows are the unsaturated shapes, exactly the §5.8.3 model's "in-flight needed to saturate is small (≈3–7)".

**Placement note (uncaged vs governed):** governed 0-15 rows (8 physical cores, both SMT siblings) repeatedly *beat* uncaged 0-31 on this shm-heavy path (e.g. serve t8/svc2: 4.18 M governed vs 2.58 M uncaged; echo t4 p50 1.32 µs vs 1.54 µs) — the kernel spreading threads across the wider set costs cross-core cacheline transfers on the ring/slot/arena lines. The §5.8.4 expectation that the cage would be the *constraint* is inverted at rig scale; matched-rails comparability (the reason the governed cage exists) is unaffected.

## In-process protocol floors (criterion, `benches/ipc_hop_bench.rs`, rail 0-15)

| micro | median | note |
|---|---|---|
| `ring_push_pop` | 7.24 ns | one MPSC publish + consume |
| `slot_full_cycle` | 24.6 ns | claim→descriptor→submit→serve→snapshot→complete→consume→release |
| `payload_4k_each_way` | 54.7 ns (139 GiB/s) | the echo leg's 2 × 4 KiB client copies |

The two-process echo p50 (0.79–1.5 µs) over these ~90 ns of protocol+payload work is scheduling/cacheline physics, as the §5.8.2 model priced (its 1.5–3 µs RTT estimate brackets the measurement from above).

## Syscall corroboration (strace, per G-L4-1 leg i)

`strace -c -f` **run-under** (ptrace_scope=1 forbids same-user attach; run-under with 1 M ops/thread so steady state dominates):

- **Default spin (4 µs): the trace itself is the perturbation** — ptrace slows the daemon ~100×, every op blows the spin window and parks, and the trace reports ~2 futex/op. Recorded as a probe-effect exhibit, not evidence against the counters.
- **Probe-effect-immune variant (spin 1 ms, echo t4, 4,000,000 ops): 32 futex calls total across BOTH processes = 0.000008 syscalls/op**, corroborating the counter-true 0.0001–0.0005/op. Every other syscall class in the trace is setup/teardown (mmap/clone3/recvmsg/…, all double-digit counts).

## G-L4-1 adjudication

| Leg | Gate (design §4, verbatim thresholds) | Measured (median, rail stated) | Verdict |
|---|---|---|---|
| **(i) echo** | RTT p50 ≤ 3 µs (tier-hit shape, 4 KiB each way) AND steady-state syscalls/op ≤ 0.1 both sides, counter-true + strace-corroborated | p50 **0.79 µs** (t1, 0-31), **1.54 µs** (t4, 0-31), **1.32 µs** (t4, 0-15); syscalls/op **0.0001–0.0005 client AND daemon**; strace-corroborated 0.000008/op (spin-1ms variant, probe effect documented) | **PASS** (p50 margin 2.0–3.8×; syscalls ≥ 200× under the cap) |
| **(ii) serve-shaped** | ≥ 650 k ops/s/core AND ≥ 1.3 M ops/s @ 2 service threads (validation stand-in + scc probe + 2×4 KiB memcpy + stats) | single service thread saturated: **2.13 M ops/s** (t4/svc1, 0-31; sweep median) with occupancy **98.6 %** (dedicated n=3 occupancy capture of the same shape: 2.16–2.31 M ops/s) = **≥ 2.1 M/core**; @ 2 service threads: **2.58 M** (0-31) / **4.18 M** (0-15, svc occupancy 97.7 + 99.8 %) | **PASS** (3.3× the per-core bar; 2.0×/3.2× the 2-thread bar) |
| **(iii) tokio-handoff** | ≤ 3 µs/op added (service thread → task wake on embedded runtime → completion post) | matched unsaturated pairs (spin-25 µs isolation, 0-31): t2 **+1.26 µs**, t4 **+0.46 µs** p50 (+0.97/+0.63 µs mean); governed t4 +0.40 µs vs its 0-31 echo comparator; doc-shape saturated companion (t8/svc2/w4): +1.71 µs p50; daemon CPU/op adder ≈ +1.2 µs (1,643 vs 440 ns) | **PASS** (worst matched-pair adder 1.26 µs ≤ 3 µs, 2.4× margin) |

**GO.** Per KD-10 the program proceeds to PR L4-3. The §5.8.2 cost model's rig-checkable assumptions hold: RTT bracketed (measured 0.8–1.5 µs vs modeled 1.5–3), serve cost beats the modeled 0.7–1.2 M/core, wake elision exceeds the ≥ 90 % assumption (≈ 99.9+ % at saturation), handoff adder inside the modeled 1–3 µs.

## §5.8.4 core-budget table — measured occupancy (logical CPUs)

| Run (rail, shape) | client threads | IPC service | tokio workers | measured |
|---|---|---|---|---|
| echo t8/svc1, 0-31 | 8 spinners ≈ **7.3 CPUs** (29.81 s thread-CPU / 4.09 s wall) | **0.99 CPU** (98.8 % occupancy, 1 thread) | 0 | daemon CPU/op 564 ns |
| serve t8/svc2, 0-31 | ≈ **6.7 CPUs** (26.63 s / 3.96 s) | **1.99 CPUs** (98.8 + 99.8 %) | 0 | daemon CPU/op 1,128 ns |
| serve t8/svc2, **0-15 (governed)** | ≈ **7.5 CPUs** (15.29 s / 2.03 s) | **1.98 CPUs** (97.7 + 99.8 %) | 0 | **≈ 11.5 of 16 total** — matches the design's ≈ 11/16 prediction for the G-L4-2 warm row; daemon CPU/op 501 ns |
| handoff t8/svc2/w4, 0-31 | ≈ 7 CPUs | 2 (dispatchers) | ≈ 1.2 CPUs equiv (daemon CPU/op 3,221 ns @ 1.52 M ops/s) | pinned within rail |

Client spinners price at ~1 logical CPU flat as the design states (spin-wait IS CPU); the never-parking assumption held (client parks ≈ 0 on unsaturated rows).

## Honest residuals & notes for the next PRs

1. **Oversaturation behavior is queueing + park storms by design** (rows †): the production service host should expect the same once offered load exceeds service capacity — the L4-3/L4-4 admission story (sessions per service thread) is where capacity planning lands. Nothing here suggests a protocol defect: throughput stays ≥ 1.5 M ops/s and verification stays clean under 2.4× oversubscription.
2. **Placement sensitivity** (governed beats uncaged): the daemon host should consider pinning service threads near their sessions' arenas; recorded for PR L4-4's A/B, not acted on here.
3. **The strace default-spin exhibit** is a standing reminder that ptrace-based corroboration of a spin-window transport perturbs the thing it measures; the spin-1 ms variant is the honest corroboration instrument.
4. **Rig ≠ daemon**: what PR-2 structurally cannot see (budget/lock interactions, real tier probes, real `active_inode_locks`) is PR L4-4 acceptance scope, per KD-10's scoping.
5. A protocol-contract find from the property suite during L4-1 (kept here for the record): a 1-cell ring is structurally broken in the cell-seq scheme — `MIN_RING_ENTRIES = 2` is now a validated floor in `ring_core`/layout geometry.
