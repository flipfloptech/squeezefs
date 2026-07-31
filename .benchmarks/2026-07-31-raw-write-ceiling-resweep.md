# 2026-07-31 — Raw write ceiling re-sweep: 49.7 GB/s, NIC-line-rate-bound (the 26.1 figure retired)

Branch `perf/raw-write-resweep` (off dev `986cffa`, **unmerged — the
orchestrator merges**). USER-APPROVED destructive follow-up to the
fio-gap-accounting campaign's open question (c)
(`.benchmarks/2026-07-31-fio-gap-accounting.md` §9): the recorded
26.1 GB/s raw write ceiling looked shape-limited after the FS beat it
at 31.3 GB/s through FUSE. This session measured the TRUE ceiling and
re-verdicts the write headroom.

## 1. Approval scope & execution discipline (all honored, journaled)

Raw O_DIRECT writes to the **DATA nvme-tcp namespaces only** — the
device guard resolved subsysnqn `*-d[0-9]` + exact 48 GiB size and
refused anything else: exactly 8 heads
(`nvme{4,6,8,10,12,14,16,18}n1` = 2 namespaces × 4 nodes, 2 fabric
paths each, iopolicy round-robin). Meta namespaces
(`nvme0n1`/`nvme2n1`) never opened; storage nodes never touched
directly. Sequence: journal SESSION START (sibling check clean) →
clean umount + daemon stop → sweep → **full restoration via
`cluster_reset_v3.sh`** (nullblk 4-wide rebuild, fresh format,
standing pair `b4edafc` remounted with the standard mount line, 26 s)
→ FS sanity row green → SESSION END (19:48–20:02 UTC). Note on scope
wording: the approval said "the 4 DATA namespaces"; the venue's data
plane is 4 *heads* × 2 namespaces = 8 block devices — all 8 were swept
(they are all data; the read-ceiling reference used the same 8),
journaled explicitly.

## 2. Venue & instrument (labeled)

Client squeeze-test (32 CPU / 2 nodes, dual-200GbE: `ens1f0np0` =
10.181.177.194, `ens2f0np0` = 10.181.178.194); targets = 4-node
reset-v3 nullblk 4-wide (memory-backed null_blk, 2 subsystems/node,
both paths shared). **Instrument:** fio-3.36 via
`tests/fio/run_fio_row.sh` raw mode (`raw_ceiling_write.job`,
per-device sections, jobs start at offset 0 — the prior recorded
row's shape class), FS unmounted for the sweep. Per row: both NICs'
tx/rx byte deltas, /proc/stat busy% (total + per-node), 5 s NIC-rate
samples; per-target balance from /proc/diskstats during the sustained
window. Rows journaled with fill=`raw nullblk backing (destroyed
format; approved)`. Artifacts: `/scratch/tmp/raw_resweep/` +
`/scratch/tmp/raw_write_resweep.{sh,out}`.

## 3. The sweep

Probe ladder (30 s + 5 ramp each; GB/s decimal):

| Shape (bs / qd / njobs-per-dev × 8 devs) | GB/s | clat mean | CPU busy |
|---|---|---|---|
| **prior-record shape** 4M / 16 / 8 (64 jobs) | 49.63 | 87 ms | 21.7 % |
| EXA-ish 1M / 8 / 4 (32 jobs) | 49.55 | 5.3 ms | 24.9 % |
| 1M / 8 / 8 | 49.53 | 10.8 ms | 27.9 % |
| 4M / 8 / 4 | 49.52 | 21.4 ms | 24.1 % |
| 4M / 32 / 8 | 49.57 | 176 ms | 22.5 % |
| 1M / 32 / 8 | 49.47 | 43 ms | 27.2 % |
| 4M / 16 / 16 (128 jobs) | **49.91** | 174 ms | 21.4 % |
| 1M / 16 / 16 | 49.75 | 43 ms | 27.7 % |

**Sustained rows (90 s + 10 ramp):**

* winner `4M qd16 nj16`: **49.73 GB/s**, NIC-rate samples FLAT —
  49.9 GB/s on every 5 s sample after ramp (first-third 49.09 vs
  last-third 49.91, +1.7 % = ramp edge only).
* prior-record shape `4M qd16 nj8`: **49.53 GB/s** sustained — the
  26.1 GB/s record does not reproduce even at its own shape; it was
  a stale/under-scaled row (history shows 2-device sweeps around
  that era; the reset script's own comment records "2-target raw
  ceiling ~16.6 GB/s" — 26.1 was plausibly a narrower plane), **not**
  a shape limitation. Retire the figure.

## 4. Where it binds

**Dual-NIC line rate.** Every row: tx split EXACTLY across both ports
(e.g. sustained winner 2,524.22 vs 2,524.21 GB — 0.004 % apart) ⇒
**≈ 24.8–24.9 GB/s per port ≈ 200 Gb/s line rate on each**. CPU
21–28 % busy, both sockets even (node0/node1 within 1–2 points) —
not socket-bound; per-target balance 5.4–7.3 GB/s across the 8
namespaces during the sustained window — no straggler target. The
ceiling is the fabric, full stop: **49.7 GB/s ≈ 2 × 200GbE**.

Write-vs-read raw asymmetry RESOLVED (inverted): raw READ measured
44.0–45.0 GB/s on this venue (fio-gap note §6.2) — reads bind ~10 %
BELOW the write line rate (client-side RX path: the nvme-tcp
one-copy-per-read-byte + interrupt/softirq path; writes TX zero-copy),
not the other way around. Writes do not stop below reads.

## 5. Verdict — FS write headroom (re-verdicting the fio-gap note §8.1)

| Reference | GB/s | FS-kernel 31.3 GB/s as ratio |
|---|---|---|
| retired record | 26.1 | 1.20× (the artifact that triggered this re-sweep) |
| **true raw write ceiling** | **49.7** | **0.63×** |

The fio-gap campaign's "no FS write-bandwidth term" holds only against
the stale reference. Against the true ceiling: **FS writes at 31.3
GB/s use 63 % of the fabric (≈ 69 % device-side including the ~1.1×
rewrite amp); ~18 GB/s of write headroom exists.** The §8.1 finding
that survives unchanged: the FS write path is not shape-bound (deep
fio shapes and the governor reach 30–34 GB/s; sub-30 rows remain
Little-closed instrument shapes) — the next write term is the FS's own
plateau at ~31 (phase table: admit_wait-governed with dma+publish
in-pipe; where the governor's delivery converges), NOT instrument or
fabric.

## 6. Recommendation

1. **Read-lane campaign stays first** (unchanged from fio-gap §9):
   0.49× of raw at the identical shape with the mechanism
   counter-named and an acceptance instrument in place — the largest,
   best-understood win.
2. **Write follow-on is now a real, bounded campaign** (second):
   target the 31 → 40+ GB/s band. First instrument, not build:
   `write_pipeline_depth_target` vs delivery at the 31 GB/s wall
   (probe-up governor engagement, publish-leg residence 4.3 ms/block
   at the wall, per-CPU handler-lane saturation) on this venue with
   `tests/fio/exa_write_bw.job` as the fixed row. The fabric can
   absorb 1.59× what the FS delivers today.
3. Update any doc citing 26.1 GB/s as the venue raw write ceiling to
   49.7 (this note is the source of record).

## 7. Restoration proof

`cluster_reset_v3.sh` from-zero rebuild (log
`/scratch/tmp/raw_resweep/reset.log`): backings recreated, fresh
format (meta-slots 8, 8-namespace data plane), standing pair
`b4edafc30a08` remounted with the standard line, mount verified.
Sanity row (EXA write shape, kernel, nj16, 20 s): **29.29 GB/s** —
consistent with the pre-sweep posture (28.6–31.3 band). Store fresh,
journal SESSION END 20:02:20Z; the client was never left without its
standing mount outside the approved sweep window.
