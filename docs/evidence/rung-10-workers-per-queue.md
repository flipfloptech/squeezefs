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
rule) is the next lever. Until then the row is a defect on the two-ring arm. **The derived default
went back to 1 ring the same evening** (orchestrator, the hour rule): the train on `039ae68` ran 3 reds
in 7 — THE KILL MATRIX twice, with a `lookup` and a `create` parked > 5 s at the metadata backend
station (`stalest_worker_pass_ms` 30 s) and the watchdog's error lines failing the row, and lever (a)'s
lane-park unit test once — where every one-ring head that day was 3/3. Two rings stay the A/B lever
`SQUEEZEFS_FUSE_RINGS_PER_QUEUE=2` until the stall and the write row are fixed; then the derivation
flips back (its shape is kept). **Flipped back the same night**, once the three reds were pinned (next
section) and the last of them — THE KILL MATRIX's fsck `[C4] staging-custody orphan`, a ring record a
kill left between a promotion's commit and its ring release — was fixed by the dismount pass judging
its residents (`rung-10-dismount-corpse.md`; 180 two-ring kill-matrix runs clean after it). The 4k
write IOPS gap (≈ 380–430 k vs the 727 k bar) stays open.

## The stall and the kill matrix (2026-10-19, the reds behind the one-ring return)

The train's 3 reds in 7 on `039ae68` read, once the census could name a worker's state, as three
unrelated things — none a stall of the two-ring daemon:

- **The `lookup (ino 3) parked > 5 s at [meta_backend_start]` lines belong to daemon A of the
  SHARED HOT DIR row**, not the kill matrix: a striped name whose stripe holder (B, then C) had just
  been killed — `findChild` → `st.locate` → the wire to the dead holder, the fail-stop lattice's wait
  (`cluster wire: xv_step served in 6357 ms — its caller's reply bound is 10000 ms`; A recovers the
  region ≈ 13 s later). The GREEN one-ring gates on `c533315` log the same line (`local-c53331560398-
  20261019T180109Z.log:1807`), as did the green two-ring gate `T172805Z`. The new worker census printed
  under it on a two-ring reproduction (`SQUEEZEFS_FUSE_RINGS_PER_QUEUE=2 zig build smoke`, run 2 of 2):
  `group 19 (qid 9, slots 16..): pass parked 35 ms ago, owed 1; lane live 1 ready 0 inbox 0,
  coalescer disarmed, lane-wake READ standing` — the second ring's worker passing on its 100 ms
  backstop with one parked fiber and a quiet lane: nothing daemon-side wedged, the fiber waits on the
  wire. `stalest_worker_pass_ms=30169 group 0` named an IDLE worker (a worker owing nothing parks
  unbounded, so its stamp's age is idle time); `pass_stamps` now carries the pass phase in the stamp
  word (one cache line per worker) and `stalest` names the worker held longest INSIDE a pass.
- **THE KILL MATRIX's red is its `umount2(mp, 0)` right after the first `kill -9`**, racing the
  kernel's asynchronous release of the dead daemon's last references to the mount (its own `.stats`
  file through `delayed_fput`, its rings' fixed files through the io_uring exit work — 64 rings under
  two rings). When the mid-dd `dd` ends ECANCELED at once (its request pending on an ent whose task
  work the dying ring cancelled) instead of ECONNABORTED ≈ 70–90 ms later at the connection's abort,
  the umount lands inside the window: `expected .SUCCESS, found .BUSY` at `smoke.zig:7339`, 2 of 14
  standalone two-ring runs, 0 of 8 one-ring runs (`~/tmp/r2w/killbusy.sh`: kill → reaped 20–26 ms, dd
  ended +0–92 ms, the umount2 60–90 ms after a fast dd). `Rig.umountAfterKill` waits out EBUSY for
  ≤ 1 s at every post-kill unmount (seven sites).
- **Lever (a)'s `io.nvme_dev` row** asserted a raw park on an UNHELD 4 KiB DMA to a RAM file, which
  completes before the caller reaches its wait (the cell's `poll` answers, no park: `expected 34,
  found 33`); the raw write now runs under the hold, lifted once the lane holds and the park is
  counted. `SqzIo.testInstance` carries no FUSE geometry, so lane-count arithmetic never touched it.

The instrument: `watchdog.Registry.census_hook` (installed by the daemon while its session stands,
run under a lock so a clear returns after a running census) → `Session.logWorkerCensus`: every drain
worker that holds fibers, owes a reply, has work queued or stands inside a pass, atomics only
(`slot.owedCountForeign`, the lane's live/ready/inbox and coalescer, `lane_wake_armed`). The smoke
harness passes `SQUEEZEFS_FUSE_RINGS_PER_QUEUE` from its environment to every row's daemon.

## The write row (the owed spin; the host's thermal throttle)

**The cost split, profiled** (`r2w-prof-{2r,1r}`, 4k randwrite QD32×4, dev `039ae68`'s code; taken
under the throttle, so the SHARES are what to read). Two rings: 301 k at 8.78 daemon cores (ur 5.41
over 64 workers, ln 3.34), sleeps per write ur 1.45 / ln 0.25 (`<row>.sched.txt`: voluntary switches),
ur CPU 17.9 µs per write, enters 3.76 and passes 2.59 per request, commits per flush 1.23; of the
eight busiest workers' time (`perf`, 6.56 % of samples): `io_uring_enter` 63 % — `io_submit_sqes`
31 % (the FUSE commits 12.5 %, the `WRITE_FIXED` to the block device 12 %), `io_cqring_wait` 26 %
(`schedule` 14 %, the completions' task work 11 %) — the own-lane write handler 29 %,
`__vdso_clock_gettime` 13 % self. One ring: 342 k at 6.85 cores (ur 3.85, ln 2.97), sleeps ur 0.40 /
ln 0.24, ur 11.3 µs per write, enters 1.88, passes 1.25, commits per flush 1.98; the same workers'
`io_cqring_wait` 14 %, `clock_gettime` 16 %. The +2 cores: +1.05 sleeps and +1.34 passes per write
on the workers (each sleep a `schedule` out and an IPI'd wake in, each pass its fixed cost and two
enters), +0.4 cores on the lanes serving fewer writes per wake; `perf sched` puts the scheduler's
wake latency at 4 µs (avg) per worker wake. The clock reads — 13–16 % of a worker's CPU in both arms
— are a lever of their own, not pulled here.

**The owed spin** (`worker.owedSpinBudget`, `spinForOwed`; `SQUEEZEFS_FUSE_OWED_SPIN`, 1): a pass
with nothing in hand and a PAGE-SIZED device completion owed on its ring submits its SQEs (a pure
submit) and spins on the ring — the CQ tail, or the `IORING_SQ_TASKRUN` flag a `DEFER_TASKRUN` ring
raises when a completion waits for its owner's enter — instead of parking per CQE; a hit is the reap's
one no-wait GETEVENTS enter. The budget is the spin-then-park competitive rule on measured prices:
two RFC 6298 predictors per ring over push → reap of page-sized bridges, `dev_awake` sampled when a
spin caught the completion (the device alone), `dev_slept` when the pass parked for it (the device
plus the wake); `W = slept − awake`, the spin at most `2·W`, only while the landing (`born + awake
srtt`) is predicted inside it, until one wake past the landing (the RTO is no end: a stable device
drives `rttvar` to 0 and `srtt + 4·rttvar` onto the landing, where every jitter fell through — 82 %
skips on QD1 reads in the first cut); no slept sample ⇒ park (the first parks measure), none caught ⇒
one probe to the slept RTO. Three cuts measured: a `min_complete = 1`, zero-timeout flush before the
spin armed and fired an hrtimer around a `schedule()` twice per spin (QD1 randread p50 7.3 → 12.2 µs);
the predictors' gauges stored per bridge CQE on a shared line (now read at stats time); and bridges
of every size sampled into one estimator — a 1 MiB store's milliseconds made `W` read milliseconds and
the sequential rows spun 55–80 s of CPU per 20 s row (`sw1m-q16j4` 72–116 µs spun per request,
+2.7 cores) — so the spin and its predictors are the page-sized completion's only. Ledger:
`transport_owed_spin{s,_hits,_falls,_skips,_ns}`, the e2e row (64 QD1 held writes: the first park
measures, the probe catches, `spins ≡ hits + falls`), the tie test.

**The host's thermal throttle.** Every number taken on 2026-10-19 before 20:50 UTC is suspect in
absolute terms: Tctl read 87 °C mid-series, the venue's level moved ≈ 25 % between legs (A's 4k
randwrite QD32×4 383–426 k across one afternoon, its 4k read spreads 13–35 %), and the hotter two-ring
daemon throttled harder — the 0.85× write row of the morning's matrices was mostly that. From `5ca46c3`
the matrix waits for Tctl ≤ 70 °C before every row and records `tctl_c` (start>end °C); rows still
end at 85–98 °C (a 20 s row heats the package from 55–65 °C), so the two arms are throttled alike
within a row and the pairs interleave. Cooled, the spreads collapsed: A's 4k randwrite QD32×4 437–442 k
(1.1 %) where the morning saw 383–426 k; the 4k reads' remaining A spread (286–395 k at QD32×1) is the
one-ring worker's placement, not heat.

**A/B, cooled, one leg at a time** (`ram-sqz-rw5-*`, legs A B B A B A A B; A = dev `5ca46c3`, one
ring derived; B = this branch `668da74`, two rings + the spin; 20 s rows; IOPS, mean (min–max), B ÷ A,
daemon cores A / B, `tctl_c` per leg):

| row | A | B | B ÷ A | cores | tctl A / B |
|---|---|---|---|---|---|
| rr4k-q1j1 | 105,790 (85,862–119,690) | 117,827 (110,278–121,818) | 1.11× | 0.60 / 0.64 | 62>66 57>71 58>83 58>85 / 56>80 56>85 57>86 57>83 |
| rr4k-q32j1 | 359,934 (286,380–394,780) | 611,827 (472,519–670,611) | 1.70× | 1.02 / 2.01 | 66>74 55>90 57>90 56>93 / 54>96 56>96 56>97 55>83 |
| rr4k-q32j4 | 990,167 (895,682–1,023,557) | 1,280,464 (1,275,150–1,283,042) | 1.29× | 4.09 / 7.98 | 53>87 57>97 57>98 69>98 / 59>98 58>98 60>98 59>98 |
| rw4k-q1j1 | 96,740 (95,262–98,311) | 100,075 (99,084–101,080) | 1.03× | 0.68 / 0.72 | 58>77 61>81 61>79 63>91 / 62>82 63>81 63>80 64>87 |
| rw4k-q32j1 | 261,536 (258,637–265,043) | 277,764 (276,113–279,822) | 1.06× | 1.81 / 2.51 | 54>82 55>94 58>93 60>91 / 56>95 58>95 56>97 58>92 |
| rw4k-q32j4 | 439,388 (436,699–441,632) | 433,903 (430,190–438,662) | 0.99× | 6.90 / 9.11 | 56>89 59>90 59>88 58>93 / 59>90 60>89 60>92 60>89 |

The controls (`ram-sqz-rw5-{A2,B1}-*`, legs A2 B1 B1 A2): **A2** = dev with `SQUEEZEFS_FUSE_RINGS_PER_
QUEUE=2` (two rings, no spin) against B: rr4k 1.02 / 1.05 / 1.00×, rw4k 1.02 / 0.99 / 1.01× (QD32×4
427,814 (412,102–443,527) vs 433,903), cores equal (9.00 / 9.11) — **the spin is neutral within ±5 %
on two rings**; **B1** = the branch with `=1` (one ring + the spin) against A: rr4k 1.09 / 1.07 /
1.03×, rw4k 0.97 / 1.00 / 1.02× (QD32×4 446,516 (445,902–447,130) vs 439,388), cores equal — neutral
on one ring. On the write row the spin engaged on 27–49 % of writes (every spin a hit, 0 falls,
0.6–1.2 µs spun per write) and took enters from 3.76 to 3.2–3.4 per write; the passes (2.55) and the
sleeps (1.1–1.2 per write) stand, and so do the two-ring arm's +2.2 cores on that row (cooled: A2 −
A = 9.00 − 6.90). What the write row's two-ring regression is, cooled: A2 ÷ A = 0.97× (427.8 k vs
439.4 k, the A2 spread 7 %) — a ≤ 3 % deficit at +2.2 cores, not the 15 % the throttled matrices
read; the spin does not move it because this host's wake is cheap (4 µs) and the half-loaded worker's
cost is its passes, not its sleeps. The lever stays for a venue whose wake is dearer (a VM's), `0` the
control arm.

**The full matrix, cooled** (`ram-sqz-rfull-*`, A B B A; B `eb240b3`, before the page-sized cut):

| row | A | B | B ÷ A | cores A / B |
|---|---|---|---|---|
| rr4k-q1j1 | 111,422 (105,390–117,453) | 116,798 (116,156–117,440) | 1.05× | 0.60 / 0.63 |
| rr4k-q32j1 | 388,107 (387,067–389,147) | 643,068 (634,184–651,953) | 1.66× | 1.02 / 2.01 |
| rr4k-q32j4 | 1,005,313 (995,967–1,014,659) | 1,283,270 (1,282,400–1,284,140) | 1.28× | 4.11 / 8.00 |
| rw4k-q1j1 | 96,325 (96,309–96,341) | 96,868 (95,642–98,094) | 1.01× | 0.68 / 0.68 |
| rw4k-q32j1 | 259,661 (256,089–263,233) | 271,166 (269,263–273,069) | 1.04× | 1.81 / 2.51 |
| rw4k-q32j4 | 440,202 (438,777–441,626) | 425,864 (417,753–433,976) | 0.97× | 6.90 / 9.06 |
| rr64k-q16j4 MiB/s | 36,187 (36,004–36,370) | 45,017 (44,792–45,243) | 1.24× | 3.66 / 6.88 |
| rw64k-q16j4 MiB/s | 18,459 (18,306–18,613) | 19,876 (19,683–20,069) | 1.08× | 6.15 / 8.62 |
| sr1m-q16j1 MiB/s | 43,024 (42,808–43,240) | 60,028 (59,772–60,284) | 1.40× | 1.31 / 2.23 |
| sr1m-q16j4 MiB/s | 57,328 (57,137–57,519) | 59,282 (59,118–59,446) | 1.03× | 2.99 / 5.16 |
| sw1m-q16j1 MiB/s | 31,123 (31,096–31,150) | 32,164 (31,941–32,387) | 1.03× | 1.78 / 2.20 |
| sw1m-q16j4 MiB/s | 35,702 (34,826–36,578) | 36,067 (34,071–38,063) | 1.01× | 4.11 / 6.81 |
| sw4m-q4j4 MiB/s | 36,657 (36,584–36,730) | 34,717 (33,403–36,031) | 0.95× | 3.76 / 6.39 |

The large-I/O rows' cores there are the third cut's defect (the ms-sized stores sampled into the
spin's predictors: 2.7–4 cores of spinning on `sw1m-q16j4`, `sw4m-q4j4`, `sr1m-q16j4`); the
page-sized spin (`61b046b`) re-measured on those rows:

| row (`ram-sqz-rfix-*`, B A B A; B `61b046b`) | A | B | B ÷ A | cores A / B | spins per request (B) |
|---|---|---|---|---|---|
| rw4k-q1j1 | 95,760 (95,615–95,906) | 96,791 (89,164–104,418) | 1.01× | 0.68 / 0.79 | — |
| rw4k-q32j1 | 258,521 (257,625–259,417) | 261,718 (258,552–264,885) | 1.01× | 1.81 / 2.72 | — |
| rw4k-q32j4 | 441,266 (438,834–443,699) | 437,068 (433,532–440,605) | 0.99× | 6.90 / 9.17 | 0.64–0.66, every one a hit, 1.5 µs spun per write |
| rw64k-q16j4 MiB/s | 18,395 (18,384–18,406) | 19,585 (19,449–19,721) | 1.06× | 6.15 / 8.46 | 0 (skips: no page-sized bridge) |
| sr1m-q16j1 MiB/s | 42,749 (42,620–42,878) | 59,585 (59,327–59,842) | 1.39× | 1.31 / 1.90 | 0 |
| sr1m-q16j4 MiB/s | 57,402 (57,288–57,516) | 59,206 (59,038–59,374) | 1.03× | 3.00 / 3.46 | 0 |
| sw1m-q16j1 MiB/s | 31,120 (30,951–31,289) | 32,084 (31,864–32,303) | 1.03× | 1.79 / 2.02 | 0 |
| sw1m-q16j4 MiB/s | 36,680 (34,767–38,593) | 38,115 (37,954–38,276) | 1.04× | 4.26 / 5.11 | 0 |
| sw4m-q4j4 MiB/s | 34,978 (34,443–35,514) | 33,984 (33,691–34,278) | 0.97× | 3.55 / 4.00 | 0 |

The sequential rows' cores came back to the two-ring arm's own (+0.2–0.9 over one ring, where the
third cut read +2.6); `sw4m-q4j4` stays 0.97× (the A spread 3 %), the 4k write row 0.99× (A 1.1 %,
B 1.6 %) at +2.3 cores. **Standing:** two rings are the derived default again (`61b046b`): every read
row 1.05–1.70× and the 64k / sequential writes 1.03–1.06× at the cores the second worker costs; the
4k randwrite QD32×4 row is within its spread of one ring (0.97–0.99× cooled, 0.85× was the throttle)
at +2.2–2.3 daemon cores, and that CPU — the half-loaded worker's 2.55 passes per write against 1.25
— is the open lever (fewer, fatter passes: a `min_wait` batching wait, or fewer clock reads per pass),
the owed spin being neutral where the wake costs 4 µs. The 4k write IOPS gap (≈ 440 k vs the 727 k
bar) stays open.
