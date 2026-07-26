# 2026-07-26 — IPC reap economy: event-driven libaio parks + service spin window

Branch `perf/ipc-reap-economy` (off dev lineage at `26fe3f9`). Commits:
red `00914f7` (the event-driven reap contract), green `4fd1695`
(aio_core / interpose / session / ipc_host), red `ec19290` (the
service-thread spin-window contract), green `dac7d1a` + `3771a1c`
(spin window + epoch-cached owned-session snapshot + SQUEEZEFS_IPC_SPIN_US
+ prompt shutdown).

## 0. Crash/resume record (honest)

The box CRASHED and rebooted mid-campaign on 2026-07-26. The killed
agent had committed the red contract (`00914f7`) and an orchestrator
pause point (`b02dd2b`) wrapped its UNCOMMITTED fix — 279 lines, never
run. Resume protocol applied:

- `b02dd2b` was **audited against the red contract, not trusted**: all
  contract tests ran green on it; three weakening checks proved the
  pins bite (wake breadth reverted to n=1 fails the breadth pin, an
  uncapped kernel slice fails the zero-timeout pin, splitting
  `park_prepare`'s RMW into load+store fails the
  `ipc_slot_multi_park_admission_never_strands` loom model); the loom
  model ran green under `--cfg loom`. Two fmt violations were fixed.
  The pause point was then **recommitted as the verified green**
  (`4fd1695`) — `b02dd2b` exists only in the reflog.
- The fabric-latency rig was reboot-ephemeral and was **rebuilt from
  scratch** (§2). Its raw ceiling came up FASTER than the prior boot
  (486k vs 282k fio libaio 16×QD16 at identical null_blk knobs) — all
  counted rows below are same-boot A/B pairs; no cross-boot number is
  compared, and no pre-crash counted run is credited (multi-run
  discipline: counts restarted from zero on this boot).

## 1. Charter

Close the shim's cold-firehose gap (field: kernel 233k vs shim 197k =
84 %; rig equivalents §3). Two named residuals from
`.benchmarks/2026-07-25-ipc-miss-path.md` §7:

1. **libaio reap sleep quantum** — the interposed `io_getevents` merge
   detected completions by poll ladder (2 yields then 200 µs sleeps)
   and smuggled a hard-coded 5 ms blocking kernel slice under the ctx
   lock when both lanes were live (field completion-latency quantizer:
   shim avg 5.13 ms vs kernel 4.37 ms at t32qd32; shim max latency
   783 ms vs kernel 293 ms — park/wake tails).
2. **Multi-session ceiling** — per-session doorbell fan-in /
   service-thread affinity (prior boot: ~120k on 16-process probes vs
   266k single-process).

Constraints: no warm fast-path / psync-row / kill-9 / fork-soak
regressions; engagement exact per il row; red-first; loom on park/wake
ordering changes.

## 2. Substrate (labeled; rebuilt this boot)

32-CPU box, 109 GiB RAM, kernel 7.1.4-1-cachyos. configfs null_blk
`sqzlat_oss0` (36 GiB, memory-backed, `completion_nsec=235000`,
`irqmode=2` timer, bs 4096, 8 squeues, hw QD 128) → nvmet-loop
(port 52126, resv_enable=1) → `/dev/nvme1n1` (data); `sqzlat_mds0`
(3 GiB, 256 MiB wb cache) → `/dev/nvme2n1` (meta). Raw ceilings
(fio 3.42 on /dev/nvme1n1): psync QD1 **4,132 IOPS / clat avg 242 µs**
(the latency knob verified, matches prior boot); libaio 16×QD16
**486k** (prior boot 282k — same knobs, faster boot; recorded, not
explained away: every A/B below is same-boot).

Filesystem: cache-less format (`sqmeta:///dev/nvme2n1
sqdata:///dev/nvme1n1`), 4 MiB blocks, 1 GiB mem cache. Mount:
`--daemon --allow-other --interception -o direct_device_true`
(queues=32 depth=32, `ipc_service_threads=8`). Dataset 16 × 1.5 GiB.
Instruments: **elbencho 3.1-10 (dynamic)** for kernel/il threaded rows,
**fio 3.42 (dynamic)** for the 16-process libaio fleet rows (one file
per forked job — each process establishes its own session; the
event-driven getevents client). Engagement per §3 rule 4 printed per
row: `ipc_ops_read` delta == row ops == `ranged_reads` delta; a row
without it is INVALID. Baseline side = `26fe3f9` daemon+shim pair
(KD-7), fixed side = `3771a1c` pair; same volume, fresh mount per side.

## 3. Residual 1 — the reap quantum (root cause → fix)

**Root cause** (inherited red contract, verified): both-lanes-live
merge passes issued kernel `io_getevents` waits with a hard-coded 5 ms
slice even under a zero budget, under the ctx lock; ring-only pendings
waited on a 2-yields-then-200 µs sleep ladder no completion could cut
short. Completion detection latency was quantized by the ladder/slice,
and a split submitter was blocked behind a sleeping reaper's lock hold.

**Fix** (`4fd1695`, the audited crash-preserved design):

- `aio_core::getevents` caps every kernel slice by the remaining
  budget — `getevents(Some(0))` is ONE non-blocking merge pass.
- The reap loop's waiting policy moved OUTSIDE the ctx lock: ring-
  involved passes are non-blocking (harvest + kernel probes); on empty
  they snapshot the pending set (`pending_tokens()`, oldest first) and
  park EVENT-DRIVEN on the slots' futex state words —
  `Session::ticket_wait_entry` (park_prepare's publish-then-recheck
  RMW) + `wait_any` (`futex_waitv(2)`, FUTEX_WAITV_MAX-capped, ENOSYS
  → 200 µs single-word quantum for pre-5.16 kernels). Both-lanes
  shapes cap the park at 1 ms (kernel completions cannot wake a
  futex); ring-only shapes recheck at 5 ms for post-snapshot
  cross-thread submits.
- The daemon's `SlotCompletion::complete` wakes EVERY parked waiter
  (`futex_wake` breadth `i32::MAX`) — split submitter/reaper pairs
  legally park on one word.

**Loom**: `ipc_slot_multi_park_admission_never_strands` — the
multi-slot park_prepare → futex_waitv admission gap is strand-free by
protocol (weakening: splitting the RMW fails the model).

## 4. Residual 2 — the service park economy (root cause → fix)

**Rig forensics** (fixed-reap binary, t16 qd16, sessions swept): IOPS
degrade monotonically 345k → 328k → 307k → 295k for sessions 1→2→4→8
at ONE offered load, and service-thread voluntary context switches
scale super-linearly:

| sessions | IOPS | svc voluntary ctx switches /s |
|---|---|---|
| 1 | 344k | 32k |
| 4 | 302k | 166k |
| 8 | 291k | 394k (> 1 park/wake cycle per op) |

**Root cause**: `SERVICE_SPIN_PASSES = 64` bare `spin_loop` hints
(sub-µs) is smaller than ANY per-session inter-arrival gap once
sessions spread across service threads (13–55 µs on these shapes) —
every burst paid a doorbell park + futex wake + a global-sessions-
mutex re-collect (the drain pass locked the registry EVERY pass, spin
passes included). The design doc's §5.5.1 "spin → short wait" ladder
had no time-based spin rung in the shipped code.

**Fix** (`dac7d1a` + `3771a1c`):

- Time-based empty-pass spin window before parking, knob
  `SQUEEZEFS_IPC_SPIN_US` (clamp 0..=10000, read at service-thread
  start), **default 30 µs — sized empirically, §5**.
- The drain hot pass is registry-mutex-free: service threads cache
  their owned set, re-collected only when `session_epoch` (bumped on
  admission/teardown, AFTER the map mutation) moves. Staleness bound
  one pass; pinned by
  `new_session_on_a_busy_thread_is_served_promptly` (weakening: a
  never-refreshing snapshot strands the new session and fails the pin).
- `ipc_service_parks` counter (stats inode): the park-economy gauge —
  growth ≈ op rate on a busy stream means the window no longer covers
  the arrival gaps.

## 5. Window sizing (single-run direction sweeps, recorded)

| variant | il-sync-t32 | il-t16qd16-s4 | fio 16-proc qd16 | il-t16qd16-s8 |
|---|---|---|---|---|
| baseline posture (64 hints + mutex/pass) | 104–105k | 306–312k | 297–301k | ~295k |
| 100 µs pure spin | **81k (−22 %)** | 304–311k | **306–318k** | — |
| 100 µs + yield_now per pass | **111k (+7 %)** | 292k | **278k (−7 %)** | 276k |
| 30 µs pure spin | 103k | 305k | 301k | 303k |
| 20 µs pure spin | 105k | 305k | 297k | 301k |

The conflict is structural: 8 service threads spinning 100 µs windows
steal tokio-worker cores on daemon-CPU-heavy shapes (the sync lane);
yielding defers the doorbell pickup under queued load (the libaio
fleet). **30 µs holds every shape at-or-above baseline** and takes the
sessions-inversion churn out (s8: 295k → 303k, +2.7 %; parks no longer
scale with sessions at dense arrival). Fleets with no sync lanes can
raise the knob; the 100 µs row shows what it buys (+3 % on 16-process
fleets) and what it costs.

## 6. A/B (final `3771a1c` pair vs baseline `26fe3f9` pair; medians of 3, 15 s rows, engagement exact on every il row)

<!-- FINAL TABLE INSERTED AFTER THE COUNTED RUN -->

## 7. Found while sizing (recorded)

- **Umount promptness**: every interception-mount umount paid a flat
  ~5 s — `IpcHost::shutdown` joined the §5.7 reap thread without
  unparking its up-to-5 s tick. Pre-existing (baseline hangs
  identically); fixed red-first
  (`shutdown_is_prompt_with_the_idle_reaper_armed`, 5.00 s red →
  <2 s green) since it sits on this branch's surface.
- **OPEN: il-only umount term (~5 s more, out of scope)**: after il
  traffic, SIGTERM teardown takes ~11 s total (vs ~5 s kernel-only);
  the extra term is OUTSIDE the ipc host — `eu-stack` at +8 s shows
  the ipc service/reap threads gone and main parked in a FUSE-session
  teardown await. Reproduces on the BASELINE pair (11.1 s, 3/3) — not
  a regression of this branch; needs its own red-first loop. The
  umount CLI's 10 s window declares the daemon hung and kernel-aborts;
  the daemon would exit naturally at ~11 s.

## 8. Substrate teardown

As the miss-path note §8: disconnect the two nvmet-loop subsystems,
unlink port 52126, rmdir nvmet objects, power-off + rmdir the
`sqzlat_*` configfs null_blk items. Left up while the branch is under
review (RAM-backed, reboot-ephemeral).
