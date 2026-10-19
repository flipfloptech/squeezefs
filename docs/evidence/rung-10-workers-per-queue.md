# Rung 10 — lever 5: two workers per kernel FUSE queue

Lever 5 of `rung-10-perf-baseline-workstation.md` (decision 0003 §AB.6): one io_uring submitter's
whole depth lands on ONE FUSE queue, so the all-own write arm (`SQUEEZEFS_FUSE_WRITE_OWN_DEPTH=0`,
`rung-10-write-lane.md`) saturated that queue's worker and shipped at depth 8. Venue: the Linux
workstation (`local`), the whole venue held per session (`scripts/perf/quiet.sh`), sessions
`ram-sqz-rl5-*` (`/build/perf/mx/`), 20 s rows.

**The kernel contract (7.2.8 + series 0001–0029, read).** `fuse_uring_task_to_queue` picks `qid =
task_cpu(current)` with no fallback; `fuse_uring_queue_fuse_req` hands a request to the first
entry of `ent_avail_queue` (LIFO) whatever ring registered it, and nothing in REGISTER or COMMIT
names the ring — so on **UserEnts** several rings per queue work as §AB.6 recorded. On
**BufRing/ZeroCopy** they do not: the queue pins ONE kmbuf payload ring from the first REGISTER's
ctx (`queue->bufring`), every entry selects from it, and the recycle resolves the buffer group in
the ENTRY's ring (`io_uring_kmbuf_recycle`) — a second ring's entry would drain ring A's list into
ring B's. §AB.6 is corrected. Profiled (`rl5-prof-r1`): the one busy worker is the queue of fio's
CPU at 0.9–1.0 cores per 250 ms window (fio issues the write inline, ASYNC_DIO; the per-row
"busiest thread 13–40 %" is fio's migration), ≥ half of it in the ring ops the queue binds to that
thread (commits, the slot DMA, delivery task work, cq-wait). An in-process hand-off to a sibling
worker moves only the handler's CPU — the depth-8 spill already does that — so the lever needed
the kernel.

**Patch 0030** (`scripts/kernel/patches-7.2/`, booted as `7.2.8-9.30-omarchy-sqz`): the buffer ring
becomes a property of the registering ring (`struct fuse_ring_src` per ring ctx on a queue: its
pinned kmbuf ring, its zc depth), each entry points at its source, a commit from another ring is
refused `-EXDEV`, one ring still serves one queue (`-EALREADY`), and a FEATURES command (cmd_op 4)
answers `FUSE_URING_FEAT_RINGS_PER_QUEUE` inline — an unpatched kernel's `-EINVAL` is the probe.

**The daemon half.** `probes.driver.probeFeatures` runs once per mount after the retention probe
(here `FEATURES → 1`; a negative result reads no features, fail-safe), published as
`transport_probe_rings_per_queue`. `geometry.ringsPerQueue(mode, feature)` derives 2 on a kmbuf/zc
session where it reads present, 1 on UserEnts and wherever absent (tie test); `SQUEEZEFS_FUSE_RINGS_PER_QUEUE`
(1–2) is the A/B pin, an explicit 2 the probe refuses fails the mount loud. Each drain group becomes
`rings` groups — two `sqz-fuse-ur<qid>` workers per queue (the identical comm: the daemon's class is
a prefix match but the perf harness strips trailing digits, so a suffix would split the class), each
with its own io_uring, kmbuf ring and `geometry.ringShare` (ring 0 `ceil(depth/2)` entries at
queue-level slots `0..`, ring 1 the rest: the depth and pinned bytes unchanged). A slot address
carries the queue-level index (`ring_base + ent`); `buf_index`, the slot table and `gent` stay
ring-local; the session resolves an address to its ring (`locate`); the arm barrier counts member
rings; `.stats` `fuse_uring_rings_per_queue`. Red → green: `session` e2e "rings per queue" (`depth`
LOOKUPs of distinct park names from threads pinned to one CPU fill both rings' shares exactly; the
control arm fills its one ring; skipped naming the feature where absent). Suites: transport 142,
session 126, e2e 110, exec 141, kv_fs 93, lifecycle 80, probes 56, geometry 109, knobs 55, rails 32;
the smoke's kernel-aio, data-plane and KILL MATRIX rows green.

**A/B, 4k rows** (A = `dev` 941d343, B = the branch; legs A B B A B A A B):

| row | A mean (spread) | B mean (spread) | B ÷ A | cores A → B |
|---|---|---|---|---|
| 4k randread QD1 / QD32×1 / QD32×4 | 89 k (33 %) · 281 k (17 %) · 871 k (13 %) | 97 k (31 %) · 487 k (27 %) · **1,249 k** (2 %) | 1.09 · **1.73** · **1.43** | 0.60 → 0.59 · 1.02 → 2.01 · 4.07 → 7.96 |
| 4k randwrite QD1 / QD32×1 / QD32×4 | 72 k (13 %) · 199 k (13 %) · 384 k (4 %) | 78 k (9 %) · 202 k (12 %) · 373 k (11 %) | 1.08 · 1.02 · 0.97 | 0.67 → 0.68 · 1.79 → 2.27 · 6.93 → 8.12 |

One submitter's QD32 reads spread over two workers (p50 118 → 64 µs; fio's thread at 89–94 % is
the ceiling now); the §1.4 read-IOPS bar (942 k) is cleared.

**The depth-0 question** (both arms two rings; legs D B B D D B B D, write rows; D = depth 0):
QD1 67 k vs 63 k (1.06, 32 % spread); **QD32×1 169 k vs 203 k (0.83)** at 1.66 vs 2.46 cores, p50
139 → 175 µs; QD32×4 377 k vs 345 k (1.09) at 5.76 vs 7.76 cores. All-own still regresses the
one-submitter row and not for the reason lever 4 named: the two workers sit at 0.83 cores each,
not saturated. A worker holding 16 of the queue's 32 batches less and parks per CQE — 2.4–2.9
enters and 1.6–1.9 passes per write against one saturated ring's 1.2 and 0.78 — and an all-own
write crosses its pass boundary twice (delivery → store SQE; store CQE → postlude → commit), so
its latency is two park/wake cycles. **All-own does not ship.**

**The knee is a queue's.** The first full matrix (A B B A, 0–3 % spreads) read the two-ring arm
at 4k randwrite QD32×4 **0.93×** (409 → 380 k, cores 6.9 → 8.1): the knee of 8 applied per ring ran
16 own writes per queue where one ring ran 8. `worker.ownDepthShare` splits it ceil-wise over the
queue's rings (4 per ring under two; tie test). Legs E B B E E B B E (E = 4 per ring; the venue's
level dropped ≈ 25 % for both arms from the fifth leg, each half read alone): QD32×4 428 / 432 k vs
405 / 408 k, then 312 / 313 k vs 289 / 293 k — **1.06–1.07× in both halves**; QD32×1 ≥ 1.0; QD1
inside its spreads. `SQUEEZEFS_FUSE_WRITE_OWN_DEPTH` stays 8, per queue.

**The full matrix, the shipped share** (A = `dev` 941d343, B = `c1d72cf`; legs A B B A, after the
venue's level had dropped — A's sequential rows read 22–36 k vs 30–42 k an hour before — so the
pairs are judged within this matrix):

| row | A legs 1/4 | B legs 2/3 | B ÷ A | spreads A / B | cores A → B |
|---|---|---|---|---|---|
| 4k randread QD1 / QD32×1 / QD32×4 | 111 / 113 k · 384 / 404 k · 948 / 977 k | 105 / 74 k · 642 / 451 k · 1,054 / 1,109 k | 0.80 · **1.39** · **1.12** | 3 / 35 · 5 / 35 · 3 / 5 % | 0.59 → 0.60 · 1.02 → 2.00 · 4.12 → 7.98 |
| 4k randwrite QD1 / QD32×1 / QD32×4 | 88 / 94 k · 230 / 228 k · 377 / 376 k | 91 / 97 k · 222 / 252 k · 305 / 336 k | 1.04 · 1.04 · **0.85** | 7 / 6 · 1 / 13 · 0 / 10 % | 0.67 → 0.66 · 1.81 → 2.45 · 6.88 → 8.93 |
| 64k randread / randwrite QD16×4 | 485 / 472 k · 250 / 248 k | 607 / 609 k · 260 / 279 k | **1.27** · 1.08 | 3 / 0 · 1 / 7 % | 3.80 → 6.86 · 6.10 → 8.53 |
| seq read 1M ×1 / ×4 (MiB/s) | 33,001 / 35,846 · 54,293 / 59,517 | 51,541 / 51,904 · 66,306 / 65,356 | **1.50** · 1.16 | 8 / 1 · 9 / 1 % | 1.40 → 2.18 · 3.40 → 4.30 |
| seq write 1M ×1 / ×4 · 4M ×4 (MiB/s) | 21,997 / 24,974 · 40,144 / 43,469 · 38,534 / 39,281 | 27,976 / 28,416 · 42,929 / 44,019 · 40,971 / 40,403 | 1.20 · 1.04 · 1.05 | 13 / 2 · 8 / 3 · 2 / 1 % | 1.84 → 2.21 · 5.23 → 6.02 · 4.46 → 5.21 |

(The earlier matrix, whole knee per ring, same A: 4k reads 1.17 / 1.70 / 1.33, 64k read 1.35, seq
read 1.37 / 1.06, seq writes 1.05–1.07, 4k writes 1.03 / 0.99 / 0.93 — every spread 0–3 %.)

**What stands and what is red.** Two rings per queue lift every read row (4k QD32×1 1.39–1.73×,
QD32×4 1.12–1.43×, 64k 1.27–1.35×, sequential ×1 1.37–1.50×) and the sequential writes (1.04–1.20×);
4k random write QD1 / QD32×1 and 64k random writes sit inside their spreads at +0.4–0.6 daemon
cores. **The 4k randwrite QD32×4 row is the one regression — 0.93× and 0.85× against one ring in
the two full-matrix pairs at +1.2–2.0 daemon cores** — and a two-ring daemon swings more between
sessions (B's 4k spreads 10–35 % where A's are 0–5 %: eight half-loaded workers for four submitters
are more exposed to placement). The mechanism is the depth-0 question's: a worker holding half a
queue parks per CQE (2.5 enters per write against 1.2); the fix is the worker's pass structure, not
the share — a bounded spin before the park while this ring owes a device completion (the ack-spin
rule) is the next lever. Until then the row is a defect on the two-ring arm; the derived default is
2 (the read bar is cleared by it) and `SQUEEZEFS_FUSE_RINGS_PER_QUEUE=1` is the one-line return to
the one-ring posture if the ruling goes the other way. The 4k write IOPS gap (≈ 380–430 k vs the
727 k bar) stays open.
