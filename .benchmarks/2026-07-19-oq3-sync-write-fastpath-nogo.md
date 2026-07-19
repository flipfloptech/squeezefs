# OQ-3 sync-write fast path — measured NO-GO (2026-07-19)

**Question** (design-preload-interception §12 OQ-3): build a
service-thread-driven **sync write fast path** (skip the tokio handoff for
the rand-write il shape)? The design deferred the decision to measurement,
with the L4-7 law governing: **< 10 % on the target row ⇒ do not build**
(no dead code).

**Verdict: NO-GO.** The handoff wake is 1.5–4 % of the write op across
every measured shape — an Amdahl ceiling well under the build gate. The
rand-write lever is delivered concurrency (the shipped v1.1 libaio
interposers), not wake elision. No code was written.

## Evidence

Box/substrate: 32-CPU dev box, devsub (nvme1n1 meta null_blk, nvme5n1+6n1
zram data); interception mount; preallocated striped 16 × 128 MiB dataset;
engagement exact on every il row (`ipc_ops_write` ≡ fio ops; writes are
100 % `ipc_async_handoffs` by v1 design, `fast=0`).

### A/B: il (ring handoff) vs kernel FUSE — interleaved pairs, drain settles

**Instrument:** fio psync randwrite 4k `--direct=1 --thread` numjobs=16,
12 s; `parked_extent_bytes`-quiet settle between rows.

| Leg | Runs (IOPS) | Median |
|---|---|---|
| il (100 % handoffs) | 126,592 / 105,991 / 125,696 / 86,175 | **~116 k** |
| kernel FUSE (same mount) | 49,346 / 48,866 / 49,978 / 50,242 | **~50 k** |

il already beats kernel FUSE ≈ 2.3× on this row (the round-trip delete).

### The Amdahl arithmetic

At t16/qd1, 116 k IOPS ⇒ **~138 µs in-flight per op** — direct 4k
overwrites are device-DMA-bound (sole-owner W1 patch ≈ 129 µs/op at this
substrate's full-queue point; `patch_writes`/`ipc_ops_write` ≈ 0.6–0.85
across rows). Buffered il writes (fio psync j8, direct=0): 107–112 k ⇒
~70 µs/op body. The handoff wake is **≤ 3 µs** (G-L4-1 leg (iii),
measured in L4-4): **1.5–4 % of the op**. The read program's 4× sync-serve
win (308 k → 1.27 M) does not transfer because a warm read's body is
~2 µs — wake ≈ body there; a write's body is 25–65× the wake.

### The lever that DOES move writes (already shipped)

Same ladder, libaio engine (v1.1 interposers): j16 qd32 reached
**251,434 IOPS** (single run; vs 95,183 psync best in the same sequence)
— delivered concurrency through the ring, exactly the OQ-1 story.
Indicative, not adjudicated: rand-write rows on this substrate are
bimodal (~50 k stall mode) when a prior row's fold/writeback backlog
shares the zram devices — the settle discipline above is required for
clean pairs, and remaining variance is writeback interference, not
transport.

## Standing consequences

- §12 OQ-3 is resolved **no-build** (Rev 6). Do not add a service-thread
  sync write path on this evidence; revisit only if the write body ever
  becomes µs-class (e.g., a RAM-absorb-only write mode), which would
  change the Amdahl term by an order of magnitude.
- The v1 posture stands: reads = sync fast path + handoff demotes;
  **all writes = async handoff** (`ipc_async_handoffs` growth on write
  rows is by design, not rot).
- Rand-write il guidance for operators/benchmarks: use libaio/iodepth
  drivers (QUICKSTART §5), not more sync threads.
