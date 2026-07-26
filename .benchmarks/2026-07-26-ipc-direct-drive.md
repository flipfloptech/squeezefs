# 2026-07-26 — DIALED P1: direct-drive ranged reads on the shim miss path

Branch `perf/ipc-direct-drive` (off dev `a1290c0`). Commits: red
`24a8474` (the direct-drive contract — engagement, bounce leg, prelude
ledger, the 795 CQE revalidation, prompt teardown, stats surface),
green `c951707` (the engine + prelude + CQE wiring), `8223b8e` (gate
leg 2l — the direct-drive kill-9 soak), `a97bd09` (submit-batch
economy — the counted fix, see §5). The pre-agreed charter
(`.benchmarks/2026-07-26-ipc-handoff-economy.md` §7, agreed as THIS
follow-on): delete the remaining per-miss cost — handler-lane task
spawn + the full async read-path descent (~14 µs/op handler CPU) +
completion hop — by having the IPC service thread submit the governed
miss shape's device read DIRECTLY on an ipc-host-owned io_uring and
complete the ring slot from the CQE. **No task, no tokio, no handler.**

## 1. Design (the decisions the charter asked to be reported)

**The governed shape** (all six clauses required; any miss ⇒ the
existing handler path — fallback-is-correctness, the aio slot-reroute
posture): `direct_device_true` mount + O_DIRECT binding; single-block;
device-class 4–64 KiB; striped file with a RAM-resident metadata entry
whose `block_map` holds an undecorated whole-block binding for the
block; passthrough crypto; **zero overlay presence** (no
`active_block_buffers` entry, no staging-ring sibling, no W2 extent
record, stable fill incarnation — correctness owns ambiguity).

- **The prelude is exact, synchronous, and complete**
  (`SqueezefsFilesystem::ipc_direct_read_probe`): RAM-authoritative
  probes only (moka metadata cache — the same binding authority
  `current_block_binding` uses on a live mount, D0 excludes remote
  writers; scc/dashmap/atomic screens), and it captures the **795
  custody snapshot**: binding key + `BLOCK_CUSTODY_EPOCHS` word + the
  allocator fill-incarnation seqlock. Refusals ride a decision ledger
  (`ipc_direct_ineligible_{shape,meta,layout,overlay,backend}` — the
  W1 `patch_ineligible_*` pattern; growth on a shape that should
  direct-drive = prelude rot).
- **The 795 protocol at the CQE** (`ipc_direct_revalidate`): epoch
  equality proves no custody TRANSFER (overlay/sibling/record retire)
  crossed the DMA window; the overlay re-screen catches custody
  CREATED inside it (creation does not bump the epoch — the handler's
  post-read `capture_parked_runs` face); binding + `incarnation_still`
  prove the DMA'd bytes belong to the block's live incarnation — the
  handler's validated-ranged serve rule, the identical proof
  obligation. Any failure ⇒ `ipc_direct_drive_fallbacks_post` + the op
  re-runs on the UNCHANGED handler path (same venue: the factored
  `spawn_read_handoff` → per-core handler lanes). Falsified live: the
  §6 fio randrw crc32c row raced 30/70 writers against direct-drive
  reads — 4,956 post-DMA fallbacks fired, **err=0**.
- **Singleflight: BYPASS, by design.** R3 ranged reads are "NOT
  single-flighted by design" (read-path §5.6 — deduping 4 KiB windows
  under a 4 MiB block key serializes independent sub-reads for zero
  byte savings), and device-true reads never publish to any tier, so
  there is no fill/publish coherence to join. A concurrent handler
  whole-block fetch of the same block composes safely because serve
  validity comes from the post-DMA revalidation, not from dedup.
- **Arena-DMA alignment (the charter question, answered):** arena
  slabs are 4 KiB-aligned by the client slab allocator's construction,
  but `arena_off` is client-chosen — the daemon screens per op:
  **direct arena DMA only when window == request AND
  (base + arena_off) % 4096 == 0** (the conservative LBA the ranged
  path already rounds to). Then the serve is **zero-copy**: DMA lands
  in the completion payload region itself — one better than the
  handoff path's pool-bounce + `payload.write`. LBA-skewed windows or
  unaligned arena offsets bounce through the existing 64 KiB
  `RANGED_BUF_POOL` (its own R5 component) and pay exactly the handoff
  path's one request-slice copy (`ipc_direct_drive_bounces`; 0 on
  every counted row — 4k-aligned instruments). Direct-arena DMA is
  §5.3.1-rule-3 legal: read payloads are uninterpreted bytes, and
  transform volumes are prelude-ineligible (the ranged window read
  requires passthrough crypto, same as the handler's ranged leg). R5:
  zero new buffer classes — arena + the existing ranged pool only.
- **Lock-order lattice:** no inode guards, no node locks, no leases
  across the device I/O — the prelude and revalidation are lock-free
  probes; the engine's ONE private mutex guards {SQ push + in-flight
  slab} and is never held across I/O or a wait. This matches the
  shipped ranged descent (which also runs outside the inode lock); no
  metadata factoring was needed beyond using the RAM entry — shapes
  whose block map is NOT RAM-resident (indirect maps, evicted entries)
  fall back honestly (`ipc_direct_ineligible_meta`; 0 on every counted
  row — the rig dataset's maps are fully resident, so the recorded
  "sibling split" was never exercised: the whole shape direct-drove).
- **The uring** (`src/ipc_direct.rs`): ONE shared ring (512 entries),
  data volumes opened O_DIRECT (buffered fallback on refusing
  substrates — the NvmeBlockDev worker's own posture) and registered
  as **fixed files**; submission from service threads under the engine
  mutex; ONE dedicated reaper thread blocked in
  `io_uring_enter(GETEVENTS, min_complete=1)` completes slots straight
  from CQEs (slot result + futex doorbell — the existing loom-modeled
  `SlotCompletion`). Profile decided shared-ring-first: the first
  counted side showed the binder was **syscalls, not SQ contention**
  (§5), so per-service-thread rings stay the recorded fallback.
- **Crash/teardown:** every in-flight op's `DataOp` + `SlotCompletion`
  pin the session mapping `Arc` (§5.3.1 rule 4) — a client kill-9
  mid-DMA can never unmap the destination under the CQE (structural,
  not behavioral). Engine shutdown (sink drop) flags + NOP-wakes the
  reaper, which drains every in-flight CQE before exiting — bounded by
  device latency.
- **Loom posture:** no new lock-free protocol — one ordinary mutex,
  single CQ consumer by construction, completion reuses the modeled
  slot machinery (`ipc_slot_core`); no park/wake or publish ordering
  was added (the flush hook is called before any park, a plain
  program-order rule pinned by the gate soak + teardown test). Loom
  suite re-ran green 42/42.

New stats (`.stats`): `ipc_direct_drive_{submits,serves,bounces,
fallbacks_post}` + the `ipc_direct_ineligible_*` ledger. The governed
accounting is PRESERVED on the direct path — `ranged_reads`,
`ranged_read_bytes`, `read_device_true_reads`, `read_odirect_requests`,
governor `note_foreground`, `get_obj` — so the amplification bounds
and the §3 rule-4 engagement instruments never went dark.

## 2. Substrate (labeled; same rig as the handoff-economy note §2)

32-CPU box, 109 GiB RAM, kernel 7.1.4-1-cachyos. The fabric-latency
substrate was STILL UP from the handoff campaign (verified knob-by-
knob): configfs null_blk `sqzlat_oss0` (36 GiB memory-backed,
`completion_nsec=235000`, `irqmode=2`, bs 4096, 8 squeues, hw QD 128)
→ nvmet-loop → `/dev/nvme1n1` (data); `sqzlat_mds0` (3 GiB) →
`/dev/nvme2n1` (meta). **Raw ceilings this session** (fio 3.42 on
/dev/nvme1n1): libaio 16×QD16 **480.5k** (clat 514 µs) — the bar's
"474k" reference re-measured same-boot; libaio 32×QD32 **477.3k**
(clat 2114 µs); psync QD1 **4,115 IOPS / 242 µs**.

Filesystem: cache-less format (`sqmeta:///dev/nvme2n1
sqdata:///dev/nvme1n1`), 4 MiB blocks; mount `--daemon --allow-other
--interception --mem-cache-size 1GB -o direct_device_true`
(queues=32 depth=32, `SQUEEZEFS_IPC_SERVICE_THREADS=8`). Dataset
16 × 1.5 GiB (cold-dominated; ddt il rows are 100 % misses by policy).
Instruments: **elbencho 3.1-10 (dynamic)** threaded rows, **fio 3.42**
16-forked-process libaio fleet. Engagement printed per row: on every
ship il row `ipc_ops_read Δ == ranged_reads Δ == read_device_true_reads
Δ == ipc_direct_drive_serves Δ == ipc_direct_drive_submits Δ` with
`ipc_async_handoffs Δ = 0` — **exact, 100 % direct-drive**; baseline il
rows engagement-exact as 100 % handoffs; kernel rows `ipc Δ = 0`.
Baseline side = `a1290c0` daemon+shim pair (KD-7), ship side =
`a97bd09` pair — both clean commits, no dev override. Same volume +
dataset, fresh mount per side, sides in one session window.

**Instrument note (honest):** at ship speeds the 15 s il rand rows are
now WORK-bounded — elbencho's random coverage exhausts the 6,291,456
dataset blocks in ~12–13 s; IOPS is ops/elapsed (both columns agreed).
Baseline rows and all kernel rows stayed time-bounded.

## 3. A/B (medians of 3; per-run values in parentheses)

| Row | baseline `a1290c0` | ship `a97bd09` | Δ / fraction |
|---|---|---|---|
| kernel t16 qd16 (context) | 358.0k (347/363/358) | 361.4k (361/361/365) | — (untouched) |
| **il t16 qd16, s4 (bar shape 1)** | 365.2k (370/365/361) | **480.1k** (486/480/475) | **+31.5 %; 1.33× kernel; ≈ 1.00× the 480.5k raw ceiling — the ~36 % gap is CLOSED to ceiling parity** |
| kernel t32 qd32 (context) | 351.8k (334/352/353) | 354.0k (359/345/354) | — |
| **il t32 qd32, s4 (bar shape 2)** | 370.5k (371/371/369) | **517.5k** (515/519/517) | **+39.7 %; 1.46× kernel; 1.08× the same-shape raw fio measurement (477.3k) — device-saturated** |
| 16-process fio libaio qd16 (multi-process probe) | 345.8k (347/346/345), clat 738 µs | **445.8k** (446/449/446), clat 573 µs | **+28.9 %** |
| il t1 qd1 (10 s) | 3,420 (3420/3422/3414) ≈ 292 µs/op | **3,964** (3968/3964/3964) ≈ **252 µs/op** | **+15.9 %; 10 µs over the 242 µs raw device RTT; kernel qd1 = 3,235 (309 µs) both sides** |
| il sync t32 qd1 (protected psync-class row, 10 s) | 107.8k (107.8/107.9/107.8) | **123.4k** (123.4/123.4/123.4) | **+14.5 %** (sync-lane ddt serves are governed misses — they direct-drive too) |
| warm fast path (default mount, no ddt, 4×200 MiB, sync t8, ×3) | 19.58k (19.8/19.6/19.4), mix ~32.0k fast / ~164k handoffs | 19.45k (19.6/19.5/19.3), mix ~32.1k / ~163k | unregressed (−0.7 %, inside band; serve mix identical; direct-drive never engages off-ddt by policy) |

**Bars adjudicated:** cold il t16qd16 moved 365k → **480k = 0.999× the
474–480k raw ceiling** (the charter's "meaningfully closer" is met at
parity); t32qd32 517.5k exceeds the same-shape raw fio number (the
single shared engine ring submits more efficiently than 32 per-thread
fio rings against 8 null_blk squeues — both are ceiling-class; stated,
not celebrated); qd1 per-op improved 292 → 252 µs (no task = less
latency, now 10 µs off the raw device); warm/sync/multi-process/kernel
rows unregressed or improved.

**Tripwires (ship mount, post-ddt-rows snapshot):**
`write_path_seed_read_bytes` 0, `patch_edge_rmw_reads` 0,
`ipc_descriptor_rejects` 0, `ipc_sessions_poisoned` 0,
`read_admission_governor_denials` 0, `fsck_findings` 0,
`ipc_direct_drive_fallbacks_post` 0 (quiet-read rows),
`ipc_direct_drive_bounces` 0, ineligible ledger 0 on every counted row.

## 4. What the CPU does now (post-fix thread economy, recorded)

During a live ship t32qd32 row (524.8k in that capture): the 4 owning
service threads ~83 % CPU each (drain + prelude + SQE publish), the
reaper ~88 % (enter + revalidate + complete), tokio workers ~0, TPC
lanes ~0, NvmeBlockDev workers ~0 — the miss path is now 5 threads ≈
4.2 cores serving 525k device-true IOPS (~8 µs of daemon CPU per op,
all-in), vs the pre-fix shape of 4 saturated service threads + TPC
handler lanes + uring workers + tokio wakes for 371k.

## 5. The intermediate counted side (lineage — multi-run discipline)

The FIRST ship side (`c951707` pair, counted ×3, recorded): t16qd16
366.9k median (+0.5 %), qd1 3,958 (+15.7 %), sync t32 123.5k
(+14.5 %), fio fleet 345.8k (flat) — but **t32qd32 350.8k = −5.3 % vs
baseline**. perf during the row: 4 service threads + the reaper ALL
~100 % CPU with the dominant samples in the kernel syscall path — a
**per-op `io_uring_enter` from every service thread** (the
M3/transport-commit-batch lesson, re-learned on a new ring). Fix
(`a97bd09`): `submit()` only publishes the SQE; `SessionSink` grew an
end-of-sweep **`flush()`** hook the service loop calls once per drain
pass (deep-qd bursts pay ONE enter per batch; qd1 keeps its one enter
per op), with the LIVENESS RULE documented and applied at both sweep
sites (incl. the pre-park rescan); the reaper takes its CQE batch out
of the slab under ONE lock; the snapshot carries its probe keys so
CQE revalidation is allocation-free. The count restarted from zero —
§3's ship rows are all `a97bd09`; the first side's rows are recorded
here and were never credited.

## 6. Falsification duties (all on the final pair)

- **Cargo contract suite** `tests/ipc_direct_drive_tests.rs` (red at
  `24a8474`): engagement-exact direct serves; bounce-leg parity
  (unaligned offset AND unaligned arena_off); prelude fallback ledger
  rows (sub-4 KiB, EOF-crossing, hole block, live overlay, staged
  layout — byte-correct via the handler every time); 795 snapshot
  revalidation (epoch bump / overlay park / truncate all invalidate;
  weakening evidence: the red ran against a stub that never
  revalidates and failed every row); host-shutdown promptness under a
  live direct-drive stream with mid-life client unmap.
- **Kill-9 during in-flight direct-drive DMA into a shared arena**:
  new preload-gate leg 2l — device-true remount, preload'd O_DIRECT
  reader SIGKILLed mid-stream ×5, engagement-asserted (+15,360 serves
  this box), sessions AND arena bytes drained to baseline every cycle,
  daemon alive, reject/poison tripwires 0. The arena teardown/reclaim
  path handles in-flight CQEs structurally (the mapping Arc rides the
  in-flight table). Full gate `sudo tests/run_preload_gate.sh` green
  end-to-end (incl. the existing kill-9 write soak, fork-kill-parent,
  netns, dup/close_range/lseek, engagement rows).
- **Mid-drive fold/patch/movement races**: fio randrw 30/70 crc32c
  `--verify` on the ddt mount THROUGH the shim — writers race
  direct-drive reads on the same blocks: 48,808 direct serves, 4,956
  post-DMA fallbacks (the 795 protocol firing), 11,772 overlay-ledger
  prelude refusals, **err=0, zero verify failures**;
  `write_visibility_tests`, `write_through_coverage_tests`,
  `extent_{overlay,patch,record_recovery}_tests`, `hybrid_io_tests`,
  `preload_{parity,lifecycle,session}_tests`, `ipc_host_tests` all
  green with direct-drive live.
- **Umount promptness**: after an 8 s il direct-drive row, `umount`
  took **0.014 s** and the daemon exited within 1 s (the reap-note's
  fixed 5 s term stays fixed; the separate ~11 s SIGTERM client-
  lifecycle term was not in this path's teardown shape here).
- **Loom**: no new lock-free core (§1); the 42 existing models green.

## 7. Gates

Full cargo gate on `a97bd09`: fmt --check clean, clippy
`--all-targets --all-features -D warnings` clean, `cargo doc
--no-deps` clean, bench smoke green, loom 42/42 (`tests/run_loom.sh`).
`cargo test --all-features -- --test-threads=1`: the pre-economy tree
(`c951707` + gate leg) ran the full suite GREEN end-to-end (138
binaries); the final-commit re-run was green across the suite EXCEPT
one failure of `kv_smo_crash_completeness_tests::
pending_free_at_cap_forced_cycle_completes_and_conserves_extents`
("every retirement that entered the FIFO must drain at point B",
2 ≠ 0) — **a pre-existing low-rate timing flake reproduced on the
UNTOUCHED dev baseline `a1290c0`** (declared rate gathering, never
acceptance: dev 1 failure in 19 executions of the test, identical
assertion; branch 2 in 10; the branch's diff touches no
kv/meta/checkpoint surface, and
the test is env-var + parked-cadence-poll shaped). Recorded here so
it is not silently inherited — it needs its own red-first loop in
the kv program. Preload gate leg 1 (unprivileged) and leg 2 (root,
incl. the new 2l soak) PASSED on the final binaries. KD-7 lockstep:
daemon+shim measured as same-commit pairs both sides; no wire/ABI
change (the engine is daemon-internal).

## 8. Residuals (recorded, not chased)

- **Data volumes added AFTER engine spawn** (VL3 online `add-data`)
  are prelude-ineligible (`ipc_direct_ineligible_backend`) — their
  reads stay on the handler until remount; re-registering fixed files
  on volume-set change is the follow-on if a fleet mixes online
  volume adds with il direct-drive. A REMOVED volume's fd stays open
  in the engine until umount (reads for its blocks stop arriving once
  evacuated; recorded, not a correctness issue).
- **Non-ddt (hybrid/warm) miss shapes** stay on the handler by policy
  — they need the tier-admission machinery (ghost/second-touch/R5
  publishes) that direct-drive deliberately does not carry.
- **The reaper (~88 % CPU at 525k)** is the next binder on a faster
  substrate: per-service-thread rings or a second reaper are the
  pre-agreed shapes; SQ-mutex contention measured NOT the binder at
  these rates.
- **Registered-buffer (ReadFixed) arena DMA** on the engine ring is
  the L4-7-class deepening left unclaimed (plain Read opcodes today;
  the win would be gup/pin economy, unquantified here).
- `SQUEEZEFS_FSTESTS_QUICK`/release-gate external suites: untouched
  paths (kernel-lane FUSE identical both sides); they run at their
  release cadence per the test-tier table.

## 9. Substrate teardown

As the miss-path note §8: disconnect the two nvmet-loop subsystems,
unlink the port, rmdir nvmet objects, power-off + rmdir the `sqzlat_*`
configfs null_blk items. Left up while the branch is under review
(RAM-backed, reboot-ephemeral).
