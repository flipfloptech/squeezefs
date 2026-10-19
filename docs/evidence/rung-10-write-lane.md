# Rung 10 — lever 4: the held WRITE on the worker's own lane (write-lane)

Lever 4 of `rung-10-perf-baseline-workstation.md`: every 4 KiB write crossed from the drain worker
to a foreign handler lane and back — 19 µs of daemon CPU per write over two thread classes. Venue:
the Linux workstation (`local`), RAM-NVMe over nvme-loop, dist builds, the whole venue held per
session (`scripts/perf/quiet.sh`), 20 s rows, legs A B B A (`/build/perf/mx/results.tsv`, labels
`ram-sqz-rl4-*`). Other agents' sessions ran between the legs; the interleaving absorbs the drift,
the A spreads say how much (QD1 rows ± 10–25 %, the rest ± 1–5 %).

**Where the 19 µs went** (`perf record -a -g` mid-row, 4k randwrite QD32×4 on `dev` 887c334,
`ram-sqz-rl4-prof-r1/`). Per write: 4 cross-thread wakes (`exec_cross_lane_wakes` 2.07 — the
handler's spawn and its store CQE, worker → lane; `transport_wake_writes` 1.41 — the store request and
the reply, lane → worker), 3.5 `io_uring_enter`s, 2.4 passes. The lane (10.7 µs): `writeStriped` 60 %
— `readInodeOpt` 26 % (two inode reads plus the layout kind, each through `tokenServe` → `homeOf`: a
`SpinLock` in `SlotHolderCache.holderOf` and two RMWs on the route table's arc-swap word, 6 per
write), `WritePath.write` 36 % (`mapLookup` 20 %, `beginPatch` 11 %, the store request's eventfd
write 6.5 %); its own ring park and fiber plumbing ≈ 10 %. The worker (8.0 µs): 62 % inside
`io_uring_enter` — the FUSE commit 13 %, the `WRITE_FIXED` submission 11 %, the completions' task work
12 %, `io_cqring_wait` 20 % — and the lane wakes (`futexWake`, `try_to_wake_up`, `__schedule`)
≈ 20 %. Not the journal (`fuse_write_times_parked` ≡ writes, 808 entries per 7.9 M writes: §5.2
holds), not the device wait (`held_stores` ≡ `patch_writes` ≡ writes: W1's DMA is `WRITE_FIXED(slot →
device)` on the worker's own ring and the store cell already parks the fiber), not lock waits.

**What changed.** (1) The dispatch law's write arm (`worker.onWorkerLane`): a HELD WRITE — the
zero-copy slot the hold gate admitted at delivery (W1-shaped on a striped passthrough file; with
lever (b), the overlay class too) — runs on the delivering worker's own lane: its device leg is the
bridge on this worker's ring (the fiber parks on the store cell, the CQE is this thread's reap,
`wakeSelf` elides the eventfd), its fall-through the RAM extent park. Per write: enters 3.5 → 1.07,
passes 2.4 → 1.06, cross-lane wakes 2.07 → 0.01, wake writes 1.41 → 0; the handler's wall time became
≈ fio's clat. (2) `exec.pickLane`: an anonymous `concurrent` from a fiber on a DRIVEN lane lands on
the fuse lane homed on the owner's CPU, never the owner's own (a held write's upload, fold or overlay
drain). (3) Three RAM lookups a striped write paid for nothing: the staged arm's reach (an inode
read) only for a non-striped file; the ENOSPC prelude's custody probe (a map lookup per block) only
once fresh supply is exhausted; `holderOf` skips its spinlock while `learnt` is empty (`learnt_n`,
mirrored under the lock — a reader racing a `learn` reads as if it ran just before it). (4)
`SQUEEZEFS_FUSE_WRITE_OWN_DEPTH` (default 8): past this many live handlers on the lane a held write
takes the CPU's lane (`transport_write_spills`) — the ruling and the trade below. Tried and dropped:
batching the lane's ready fibers with a submit between batches (passes per request did not move).

**Red → green.** `session` e2e "a HELD write's handler runs on the thread that delivered it": 16 held
4 KiB writes, every dispatch `own`, the handler's tid a drain worker's, `write_spills` and
`slot.wake_writes` unmoved; the unaligned write `foreign`, off every worker; the control arm dispatches
nothing `own` and writes wakes (red before: `onWorker` false at the first write). `exec.sqz_io` "an
anonymous concurrent spawned from a fiber on a DRIVEN lane lands on a threaded lane" (red: `expected 0,
found 1`). The dispatch-law row pins the write arm. `worker_pass_model`'s park law is unchanged.

**A/B, the shipped default** (depth 8; series 4: legs A7 B7 B8 A8 on the 4k randwrite rows, A =
`dev` acc4b55, B = 46a49a6 — the branch rebased on it):

| row | A legs 1/4 | B legs 2/3 | A mean | B mean | B ÷ A | p50 A → B µs | daemon cores A → B (`ur`/`ln`) |
|---|---|---|---|---|---|---|---|
| 4k randwrite QD1 | 45.9 k / 41.0 k | 96.9 k / 82.1 k | 43.4 k | 89.5 k | **2.06** | 18.8 → 9.8 | 0.84 → 0.68 (0.46/0.37 → 0.67/0) |
| 4k randwrite QD32×1 | 241.3 k / 212.0 k | 231.3 k / 219.9 k | 226.7 k | 225.6 k | **1.00** | 130 → 134 | 1.99 → 1.81 (0.94/1.04 → 1.00/0.79) |
| 4k randwrite QD32×4 | 404.9 k / 416.0 k | 424.7 k / 419.5 k | 410.4 k | 422.1 k | **1.03** | 272 → 285 | 7.46 → 7.05 (3.23/4.20 → 3.98/3.05) |

Series 2 (depth 8 on 887c334, the full matrix) read the same: QD1 2.08×, QD32×1 0.99×, QD32×4 1.06×
at 7.42 → 7.06 cores, 64k randwrite 0.96× (3 % spread), reads 1.07–1.27× (A legs 16–39 % spreads),
sequential rows within noise. **The all-own arm (depth 0)** — series 1 (changes 1 + 3 on 887c334, the
full matrix) and series 3 (the branch rebased on c28babc with levers (a) and (b) landed; the write rows):

| row | A mean (series 1 / 3) | B mean | B ÷ A | p50 A → B µs | daemon cores A → B (`ur`/`ln`) |
|---|---|---|---|---|---|
| 4k randwrite QD1 | 45.6 k / 53.2 k | 81.5 k / 81.6 k | **1.79 / 1.53** | 18.8 → 10.3 | 0.91 → 0.83 (0.48/0.41 → 0.81/0) |
| 4k randwrite QD32×1 | 209.8 k / 229.3 k | 163.4 k / 175.3 k | **0.78 / 0.76** | 139 → 187 | 1.96 → 1.14 (0.92/1.02 → 1.12/0) |
| 4k randwrite QD32×4 | 389.7 k / 402.2 k | 448.7 k / 432.2 k | **1.15 / 1.07** | 289 → 270 | 7.36 → 4.05 (3.17/4.17 → 4.03/0) |
| 64k randwrite QD16×4 | 297.6 k / 307.4 k | 301.4 k / 321.9 k | **1.01 / 1.05** | 197 → 184 | 7.04 → 3.99 (2.96/4.04 → 3.95/0) |
| 1M seq write QD16×4 (series 3) | 26.6 k | 26.5 k | **1.00** | 2245 → 2163 | 5.77 → 5.38 |
| 4M seq write QD4×4 (series 3) | 7.20 k | 7.10 k (one leg 5.42 k under foreign load) | **0.99** | 1868 → 1843 | 5.55 → 5.15 |
| 4k randread QD1 / QD32×1 / QD32×4 (series 1) | 103 k / 282 k / 788 k | 107 k / 322 k / 828 k | 1.04 / 1.14 / 1.05 | | unchanged |

Series 1's remaining rows: 64k randread 0.96, seq read 0.92–0.94 (2–4 % spreads, the reads are
untouched), seq writes bimodal on the pre-(b) tree independent of the binary (`dev` itself read 2,195
/ 3,563 / 7,154 on sw4m across legs). The depth sweep (one leg each, 9696e5a): depth 16 spills 44–48 %
at QD32 — QD32×1 219 k at 1.54 cores, QD32×4 371 k at 6.07; depth 8 spills 70 %; depth 0 none.

**The trade, the ruling, and what remains.** The kernel queues a request on the submitter's CPU's
FUSE queue, so ONE io_uring thread's whole depth lands on ONE queue: all-own makes that queue's worker
its one saturated CPU (≈ 6 µs of worker time per write; `daemon_cores` ≡ 1.0 at QD32×1) where the
two-thread arm had two — the 0.76–0.78× row — while every many-submitter row gains at half the CPU
(18.5 → 9 µs per write at QD32×4). **Ruling (the orchestrator, 2026-10-19, under the charter's "a
regression is a defect" and the owner's "performance first"): the default is 8, the measured knee** —
QD1 2.06×, QD32×1 1.00×, QD32×4 1.03× at 0.95× the cores; all-own (0) becomes the default once lever
5 (N workers per kernel queue, §AB.6) removes the one-queue saturation, and the dispatch-law row pins
the default until then. Lever 5 as measured (`rung-10-workers-per-queue.md`): the saturation is real —
the one busy worker is the queue of fio's CPU, 0.9–1.0 cores per window — but this kernel series
admits one ring per kmbuf/zc queue, so the lever waits on the proposed patch 0030 and the knee stays 8. The write handler is now ≈ 6 µs of worker CPU, all RAM: `readInodeOpt`
(the probe and the postlude's re-read under the shared guard), `mapLookup` (the prelude's and
`beginInPlace`'s), per lookup the route table's arc-swap RMW pair (`MetaSet.volumeOfSlot`, 4.8 % self —
a shared-line RMW on the hot path; a lock-free-core change with its model) and `homeOf`. With lever
(a) landed the hold gate is the dispatch law's boundary only because the extracted class was measured
on the CPU's lane; moving the whole WRITE class is its own A/B.
