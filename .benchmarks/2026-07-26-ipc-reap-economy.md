# 2026-07-26 — IPC reap economy: event-driven libaio parks + service spin window

Branch `perf/ipc-reap-economy` (off dev lineage at `26fe3f9`). Commits:
red `00914f7` (the event-driven reap contract), green `4fd1695`
(aio_core / interpose / session / ipc_host), red `ec19290` (the
service-thread spin-window contract), greens `dac7d1a` / `3771a1c` /
`0e0488f` / `1508ea9` (spin window + epoch-cached owned-session
snapshot + SQUEEZEFS_IPC_SPIN_US + prompt shutdown + deep-qd reap
economy + the default-0 verdict).

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
  start), **default 0 — an explicit fleet lever, not an ambient tax
  (sizing verdict, §5–§6)**.
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

The conflict is structural: 8 service threads spinning wide windows
steal runnable-tokio CPU on daemon-CPU-heavy shapes (the sync lane);
yielding defers the doorbell pickup under queued load (the libaio
fleet); an adaptive density gate (spin only when serve-to-serve gaps
fit the window) was tried and REVERTED — it flapped on the 28 µs
fleet-arrival shapes (16-proc −4 %, s8 −5 %). **Verdict: default 0.**
Every nonzero default bought its libaio-fleet gain by taxing the
protected sync lane (−4 % at 20 µs, −22 % at 100 µs), while the
counted §6 A/B shows the fleet wins that matter ship default-on from
the event-driven reap + the epoch-cached snapshot alone. libaio-only
fleets raise the knob (the 20–100 µs rows above price what it buys).

**Deep-qd reap sizing (client side, same discipline)**: fully
event-parking the reap regressed t32qd32 −6 % — at qd32 saturation an
event park costs a waitv cycle client-side plus one daemon
`futex_wake` syscall PER COMPLETION (the WAITER bit), where the old
ladder amortized several completions per wake, and 42 % of
`futex_waitv` calls failed admission (EAGAIN). Fixes measured back:
deep-pending sets (> `REAP_EVENT_PARK_MAX` = 24 in flight) batch on a
50 µs bounded sleep (¼ the old ladder quantum, latency share bounded
by depth) — t32qd32 recovered to baseline (342–358k); pure-ring merge
passes skip the kernel `io_getevents` probe (was one wasted syscall
per pass); `wait_any` pre-checks admission in userspace (deletes the
EAGAIN syscalls); the pre-park done-scan is 4 sweeps, not 64 (2048
cross-cacheline shm probes per empty pass at qd32). The sparse regime
(qd ≤ 16 per ctx — the charter's tail territory) stays fully
event-driven.

## 6. A/B (ship config `1508ea9` pair vs baseline `26fe3f9` pair; medians of 3, 15 s rows unless noted, engagement exact on every il row)

The decisive pair ran **back-to-back in one session window** (baseline
side 07:52, ship side 08:03 local) because the box drifts ~2 %
downward within long sessions — the morning baseline side is also
recorded where it differs. Kernel rows bracket the context (same
binary class both sides).

| Row | baseline `26fe3f9` | ship `1508ea9` | Δ |
|---|---|---|---|
| kernel t16 qd16 (context) | 349.0k | 357.5k | — |
| il libaio t16 qd16, s4 | 292.7k (303.3/292.7/291.1) | **294.2k** (294.3/294.2/291.9) | ~flat |
| il libaio t32 qd32, s4 (the field shape) | 334.2k (342.6/334.2/320.7) | **335.1k** (343.2/335.1/316.6) | ~flat IOPS; **completion avg 2.99→2.98 ms, max-lat class intact** |
| 16-process fio libaio qd16 (one session/proc) | 284k (286/284/280), clat avg 893–912 µs | **289k** (295/289/276), clat avg 866–927 µs | +1.8 % |
| il libaio t1 qd1 RTT | 3,696 IOPS / avg 269–273 µs | 3,634 IOPS / avg 273–277 µs | ~flat — **il per-op still beats kernel (307–327 µs) by ~35 µs** |
| il sync t32 qd1 (protected psync-class row) | 100.6k (100.6/101.1/100.1) | 98.0k (98.4/96.8/98.0) | −2.6 %, inside the observed inter-session band for this row (96–108k across six mount sessions of identical sync-path code; the sync data path is untouched by this branch) |
| **sessions sweep (10 s rows, t16 qd16)** s1 | 301.1k | **328.9k** | **+9.2 %** |
| s2 | 296.2k | **304.8k** | +2.9 % |
| s4 | 280.7k | **282.9k** | +0.8 % |
| s8 | 267.4k | **274.9k** | **+2.8 %** |

Intermediate counted sides (recorded for lineage, all 3-run medians):
the reap-fix-only pair (`4fd1695`) cut il t32qd32 **max latency
13.6 → 9.3 ms** and t16qd16 max 7.3 → 5.1 ms vs the morning baseline
(the field's 783 ms max-latency class is exactly this park/wake-tail
term); the w20-window side (`43a4668`+20 µs) traded sync −4 % for
fleet +3 % and was rejected as a default.

**What the charter asked vs what the boot delivered (honest)**: the
84 % cold-firehose ratio did NOT close to ≥ 1.0 on this rig — il
t16qd16 sits at 0.82–0.86× kernel on both sides; the two named
residuals were real but their sum was worth single-digit percent plus
the tail class, not the 16-point ratio. The remaining gap term is the
per-op async-handoff/service path (kernel-lane clat 712–764 µs vs il
854–927 µs at equal offered load ⇒ ~130 µs/op of daemon-side queueing
the kernel transport does not pay); that is a different program item
(per-op handoff cost), recorded in §7. Per-op (qd1) the shim beats the
kernel path on every run of every side.

**Warm fast-path unregressed** (constraint row, measured ×3 each):
default interception mount (no ddt), 4 × 200 MiB warm set, elbencho
sync t8 rand-4k 10 s — baseline 22.1k median (22.1/22.2/21.1) vs ship
**23.0k** (23.0/24.7/21.6), serve mix comparable (~66k fast-path
serves / ~138k handoffs per 204.8k ops both sides). The cold ddt rows
above never touch the fast path by policy (`ipc_fast_path_serves`
delta 0 on every ddt row).

**Gates**: full cargo gate on the final commit — clippy `-D warnings`
+ fmt clean (both workspaces), `cargo doc --no-deps` clean, bench
smoke green (all four bench binaries), loom 42/42 green
(`tests/run_loom.sh`), `cargo test --all-features -- --test-threads=1`
green across the suite EXCEPT two pre-existing failures that
reproduce identically at the branch point `26fe3f9` (verified on the
baseline worktree, untouched read-path surface):
`hybrid_io_tests::escape_direct_device_true_is_device_true` and
`read_tier_refetch_churn_tests::fetched_blocks_are_tier_visible_and_never_refetched`
— both entered dev with the 2026-07-26 read-admission program and
belong to it, recorded here so they are not silently inherited.
Preload gate leg 1 (unprivileged) and leg 2 (root: mount parity +
engagement, dup/close_range/lseek rows, notify delivery, foreign-netns
rendezvous, fio/elbencho/libaio verify, **kill-9 soak ×5 and
fork-kill-parent soak — zero residue**) both PASSED on the final
binaries.

## 7. Found while sizing (recorded)

- **OPEN: the residual cold-firehose term is per-op handoff cost, not
  wake economy**: with the reap event-driven and the park churn gone,
  il completion latency still runs ~130 µs/op above the kernel lane at
  equal offered load (854–927 vs 712–764 µs clat). perf on the daemon
  under the il row shows the cost spread across the async-handoff
  path (`DataPlaneSink::enqueue_read` + tokio `schedule_task` +
  `moka`/attr probes — §6 of the profile capture), i.e. one
  spawned-task round trip per miss that the FUSE-over-uring path does
  not pay per op. Next lever if the ratio matters: batched handoff
  (drain N ring ops into one task) or sync-submit into the
  `NvmeBlockDev` queue from the service thread — both pre-agreed
  fallback shapes in design §5.5/§12.

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
