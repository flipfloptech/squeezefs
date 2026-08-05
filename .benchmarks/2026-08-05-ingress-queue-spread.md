# 2026-08-05 — The rand-4k "anti-scaling knee" is queue-spread drain economics

**Venue:** squeeze-test (field cluster, real nvme-tcp fabric, 10 data
namespaces, 2×200GbE mlx5, 32 CPUs / 2 NUMA nodes). **Instrument:**
`tests/fio/transport_ingress_sweep.sh` (fio libaio bs=4k direct randread,
30 s/point, one prefilled fileset) + ad-hoc fio `--cpus_allowed` pin rows.
**Binary:** wave `9dbec6d0` (lane-off default mount — the kernel FUSE path).
Artifacts: `/tmp/ingress_sweep_20260805_004912_knee` on the cluster.

## The ladder (in-flight = njobs × qd)

| point | in-flight | IOPS | clat | queue_wait+dispatch_lag (est-mean) | dev_service |
|---|---|---|---|---|---|
| 16×8 | 128 | 316,620 | 401 µs | 97 µs | 221 µs |
| 20×8 | 160 | 305,664 | 520 µs | 156 µs | 237 µs |
| 24×8 | 192 | 290,481 | 657 µs | 226 µs | 243 µs |
| 28×8 | 224 | 278,265 | 801 µs | 291 µs | 252 µs |
| 32×8 | 256 | 272,563 | 936 µs | 325 µs | 260 µs |
| 16×16 | 256 | 303,412 | 840 µs | 304 µs | 267 µs |
| **8×32** | **256** | **357,367** | 713 µs | 236 µs | 253 µs |
| 4×32 | 128 | 275,599 | 463 µs | 122 µs | 196 µs |

There is **no sharp knee**: qd8 scaling is smoothly negative 128→256 while
device service barely moves (221→260 µs) and the transport's
queue_wait+dispatch_lag more than triples (97→325 µs). At **constant 256
in-flight**, fewer/deeper submitters win monotonically: 32×8 → 16×16 → 8×32
= 273k → 303k → 357k. 4×32 shows the offered-load floor (too few
submitters).

## The discriminator (same shape, CPU pin)

| row | IOPS | clat |
|---|---|---|
| 32×8 unpinned (repro of ladder) | 275,918 | 923 µs |
| **32×8 pinned `--cpus_allowed=0-7`** | **333,725** | 764 µs |
| 8×32 pinned `0-7` | 303,827 | 840 µs |
| 8×32 unpinned (ladder) | 357,367 | 713 µs |

Pinning the SAME 32 processes to 8 CPUs recovers **+21 %** — the cost is
**submitter CPU spread (active-queue count)**, not process count: the FUSE
transport runs one queue per possible CPU, so 32 spread submitters keep 32
queues active at ~8-deep, and per-queue drain economics (wakes per few ops,
small commit batches) dominate. The counter-row agrees from the other side:
over-constraining already-deep submitters HURTS (8×32 pinned 303.8k vs
unpinned 357.4k) — once per-queue depth is healthy, scheduler freedom wins.

## Campaign consequence (D12 board item 3 — the 1M-IOPS campaign)

Lever 2 (after the landed same-lane READ dispatch lever 1, `b6d7413b`): the
per-active-queue wake/drain overhead at shallow per-queue depth. Candidate
directions to bracket, in order: (a) cross-queue drain aggregation on the
daemon side (one drain pass services multiple shallow queues before
parking); (b) commit-batch accounting across queues sharing a handler lane;
(c) wake coalescing keyed on aggregate (not per-queue) depth. Any change
must re-run THIS ladder + the pin discriminator as its acceptance bracket
(A-B-B-A not required — the fileset does not age under randread, but state
instrument + substrate per the standing rule).

Reference points for re-grading: raw fabric ceiling 3.36 M IOPS @ 372 µs
(libaio direct on the namespaces, 2026-08-04); warm-il record 1.017 M
(G-L4-2); this shape's current best 357k (8×32).
