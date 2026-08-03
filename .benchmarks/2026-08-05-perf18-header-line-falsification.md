# PERF-18 — header cache-line layout: FALSIFIED (counted)

**Date:** 2026-08-05 · **Branch:** `perf/board-remainder` · **Item:** pre-RC
engineering spec §9 PERF-18 · **Closure class:** counted falsification
(execution-plan exit criterion E11 — "a counted A/B falsification is a valid
closure").

## The proposal

> Header cache-line layout — `doorbell` (client RMW) shares a line with
> `daemon_parked` (daemon store); `CqeDoorbell.seq`/`parked` share 8 bytes.
> Both are opposite-side producer/consumer pairs written on every submit and
> every completion. — *Expected: qd1 clat; reaper rows. Instrument: `perf
> c2c` on the header page.*

Anchor (corrected): `crates/squeezefs-ipc/src/layout.rs` — `SessionHeader`
lines 1–2 and the `const _: ()` layout pins (the spec's `layout.rs:268-288`
still points into the struct, but by symbol: `SessionHeader.doorbell` /
`.daemon_parked` / `.cqe`).

## What was built and measured

The change was implemented in full: one cache line per WRITER
(`doorbell` + `WakeCoalescer` | `daemon_parked` | `cqe.seq` | `cqe.parked`),
`CqeDoorbell` self-padded to two 64 B lines, layout pins updated, `IPC_ABI`
bumped 3 → 4. It built, and the whole IPC/preload surface stayed green
(`squeezefs-ipc` 35+3+4 tests, `ipc_host_tests` 33, `ipc_op_economy_tests` 3,
`preload_parity_tests` 14, loom `ipc_cqe_parked_reaper_never_stranded`).

Then it was measured. `benches/ipc_hop_bench.rs`, group
`ipc_wake_pair_lines` — the exact traffic shape of one such pair, two
threads, each RMW-ing its own word and loading the other's, 20 000
iterations per thread per sample, 100 samples, criterion medians. Box: 24-CPU
AMD Ryzen AI MAX+ PRO 395, single NUMA node, `clocksource=tsc`, dev station
under concurrent sibling-agent load (load average ≈ 9–18 — the absolute
numbers are noisy, the A/B ratio is not: both arms interleave inside one
criterion run).

| Arm | Same line | Split lines | Verdict |
|---|---|---|---|
| **Field shape** — one HOT writer, the other writes 1-in-1000 | **113.1 µs** | 112.3 µs | **0.8 % — nothing** |
| **Symmetric** — both writers hot every iteration | **407.1 µs** | 999.6 µs | **split is 2.46× WORSE** |

## Why the proposal is wrong

The spec's premise counts the WRITES and ignores the READS. Each side reads
the other's word on its own cadence:

* **Same line:** the hot side's RMW *and* its read of the partner word both
  land in one line it already holds exclusively. Steady-state coherence
  traffic ≈ 0 as long as the partner writes rarely.
* **Split lines:** the hot side RMWs its own line (fine) and then reads a
  line the partner keeps dirtying — and the partner's next write must
  re-acquire a line the hot side keeps pulling shared. Two lines
  ping-ponging instead of one, which is exactly the 2.46× symmetric result.

And the partner IS rare by design: the wake coalescer elides doorbell wakes,
`daemon_parked` is stored only around real parks, and `REAP_EVENT_PARK_MAX =
2` keeps deep-queue reapers from parking at all (2026-07-28 op-economy
campaign). The header's existing layout already separates the three
*classes* — write-once identity, submit wake words, completion wake words —
which is the separation that pays.

## Disposition

Reverted. `IPC_ABI` stays 3 (the change would have cost a bump for a 0.8 %
noise band). The falsification is recorded twice so it cannot be re-attempted
by inspection: as a comment on the layout pins themselves, and by the
standing `ipc_wake_pair_lines` bench group, whose two shapes make the
mechanism visible in ~20 s.

**Not measured here:** `perf c2c` on a live mount's header page, which the
spec names as the instrument. A live cluster mount was held by another
workload for this session's duration, and no cluster venue was available. If
a future campaign gets `perf c2c` evidence of a real HITM storm on the header
page, the mechanism above says the fix is to make the second writer sparser,
not to split the line.
