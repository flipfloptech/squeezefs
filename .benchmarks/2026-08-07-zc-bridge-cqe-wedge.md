# The zc bridge-CQE wedge — bounded outcomes, the wedge census, and the D14 flip decision (2026-08-07)

Branch `fix/zc-bridge-cqe-wedge` off the merged wave `4c3c1190`. The campaign
closes the **zcws-9 W4 armed-mount wedge** — the DO-NOT-FLIP finding of
`.benchmarks/2026-08-06-fuse-zc-write-side.md` §9 and THE blocker on the
`SQUEEZEFS_FUSE_ZC` default-ON flip. Load-dependent hangs are first-class
product bugs (repo law); this one is treated red-first end to end.

Commits: `64ee2bba` (red: census + bounded outcomes) → `84310e74` (green) →
`7c8e1d0f` (red: the lost-CQE resolution ladder) → `68f10c0f` (green).

---

## 1. The field tape, mined (`/scratch/tmp/logs/sqz-W4-zc1.log`, 42 MB)

Venue: squeeze-test, 6.19.14-sqz (kmbuf 37/38), armed hybrid (`b4bcbb20`),
the zcws-9 W4 leg (seq 1 MiB durable writes over the aged W1–W3 store).

Wedge anatomy (all numbers from the tape):

* **Onset 17 s after a FRESH mount** (05:14:16 mount → 05:14:33 the first
  `rewrite epoch FENCED at close` + `FencingTokenExpired`, → 05:14:46 the
  first stall; first overdue-slot line 05:14:52 at 5.9 s age).
* **140 distinct requests** (`unique=` census) stranded across **28 of 32
  rings** inside one ~5 s delivery window (delivered-age spread of the
  first scan: 5 919–10 943 ms) — a SHARED choke, not per-op loss.
* **Only 24 distinct inos** ever appear in `FUSE op watchdog: write` lines
  (27 504 lines ÷ 1 146 scans = 24 concurrent overdue handler ops), and the
  lock census names just **3 inos** parked on `INODE_META_LOCKS`.
  **⇒ ~116 of the 140 held slots never dispatched a handler**: on the W4
  shape (1 MiB writes are never `hold_candidate`s) those are **at-delivery
  `WriteExtract` bridge ops whose CQEs never resolved** — invisible to the
  op watchdog AND the lock census, which is why the wedge had no name.
* **Zero** zc bridge warnings (`zc fetch|store|extract` refusals/push
  failures: 0 lines), **zero** `transport_cq_overflows` (the FUSE-3f
  instrument existed and stayed silent), zero NVMe/dmesg events in the
  window. Every worker parked in `io_cqring_wait` — the healthy posture —
  for 96+ minutes.
* Context (not the mechanism, but the load that selected the schedule):
  data volume `nvme12n1` ran **100 % full** (12288/12288) through the
  earlier W4 attempts — 107 StorageFull fsync failures, publish refusals
  (`reclaim in flight`), and the close-storm placement skew (out of scope
  here, its own campaign).
* Teardown: 140 × `unanswered request … at teardown; synthesizing EIO
  (row 8)` — the transport's own shutdown pass knew exactly which slots
  were owed; nothing at runtime could resolve them.

Adjudication: workers parked in `io_cqring_wait` wake on ANY posted CQE, so
the stranded ops' completions were never reaped — either never POSTED
(stuck kernel-side) or posted-and-lost. Daemon SQE construction is clean
(no `IOSQE_IO_DRAIN`/`IO_LINK` anywhere on the ring; `WRITE_FIXED`
`buf_index = ent_idx` targets the request's kernel-registered sparse bvec,
disjoint from the headers index — verified against `docker/kernel-sqz/
patches/0023-0024`). No kernel-side artifact (dmesg, hung-task on io-wq)
survived to discriminate the two classes — so the fix builds the
discriminating instrument into the transport itself (§3) and the
patch-0028 (kernel) decision keys on what that instrument reports from the
field (§6).

## 2. Instrument 1 — the wedge census (`64ee2bba` → `84310e74`)

The wedge produced 16 k watchdog lines, a 3-entry lock census, and NO way
to name what the stuck ops awaited — the `.stats` read itself hung, so the
gauges were unreachable exactly when they mattered.

`squeezefs::fuse_client::wedge_census_line()` — ONE log line built from
process-global atomics only (readable from the watchdog task no matter
what is wedged), emitted by the D1.b op watchdog alongside the lock census
whenever ops are overdue. 13 fields: conveyor passes/queued, commit/publish
parked (`ParkedGaugeGuard` RAII at both `KvMetaBackend` park sites),
journal entries, checkpoints, pipeline inflight/admission-wait mirrors,
reclaim queue bytes, rewrite open epochs, transport leases outstanding /
parked / unparked commits. Contracts: `tests/wedge_census_tests.rs`
(field-list contract + a LIVE stalled-conveyor signature driven through
the standing `TEST_CONVEYOR_HOLD_STAGE` seam: `meta_commit_parked` ∧
`meta_conveyor_queued` grow while `meta_conveyor_leader_passes` stays flat,
release drains to 0).

## 3. Instrument 2 + the fix — bounded bridge outcomes (`7c8e1d0f` → `68f10c0f`)

**The law**: every zc bridge op resolves by completion, error, or the
deadline ladder. A parked worker waiting forever on a CQE that never comes
violates the transport's own FUSE-2/watchdog discipline.

* **`BridgeDeadlines`** (per drain-group worker, ent-indexed like
  `zc_pend`): stamp at every pend set (HandlerFetch / HandlerStore /
  LazyExtract / at-delivery WriteExtract / BounceFetch), clear at every
  resolution, `overdue()` yields each pend exactly once per deadline
  (cancel-once). Pool gauge `zc_bridge_pends`; the `connection_watch`
  thread arms the group wake eventfds while any pend is outstanding, so
  parked workers run the scan (a worker in `io_cqring_wait` never scans on
  its own).
* **Deadline** `SQUEEZEFS_ZC_BRIDGE_TIMEOUT_MS` (default 30 000 = the D1.b
  threshold's transport twin; range 100–600 000; registry entry). Past it:
  ONE `AsyncCancel` per pend (`RingOp::Cancel` user_data class).
* **The resolution ladder** (`zc::cancel_cqe_action`, unit-pinned) — the
  cancel's OWN CQE classifies the overdue op:
  * `0` → found + canceled: the original op's `-ECANCELED` CQE resolves it
    through the existing loud fallback ladders.
  * `-ENOENT` **with the pend still live** → **the PROVEN lost-completion
    shape**: the kernel has no such op, so its completion already POSTED —
    and the CQ is FIFO, so that CQE precedes the cancel's in reap order; a
    live pend at processing time proves the completion never reached us
    AND that no kernel op still references the slot (what makes synthesis
    safe). Synthesize via `zc_fetch_complete(-ETIMEDOUT)`: handler
    oneshots error (fallback/EIO), at-delivery extracts `fail_ent` EIO +
    re-REGISTER. Ledger: **`fuse3_zc_bridge_lost`** (must stay 0).
  * `-EALREADY`/other **with the pend live** → the op is RUNNING
    kernel-side and unstoppable from userspace; synthesis would let a late
    kernel DMA alias a recycled slot/bounce. **Re-stamp** the deadline —
    loud every period, the slot stays watchdog-named. This is the class
    only a kernel fix closes.
  * pend gone → nothing (the original resolved in this or an earlier
    batch — FIFO guarantees the ordering).
* **The field discriminator**: `fuse3_zc_bridge_cancels` (deadline fired)
  vs `fuse3_zc_bridge_lost` (loss PROVEN). Losses ⇒ ring-completion loss,
  kernel-side evidence (the patch-0028 case); cancels-without-losses ⇒
  stuck-op class (io-wq/punt investigation). Both must-stay-0 tripwires on
  the stats surface.
* **Loud-never-lossy**: every affected request is pre-ACK by construction
  (an unresolved bridge means the WRITE never completed), so an EIO drops
  no acked byte. Held payloads stay in the slot until COMMIT — the
  extraction fallback is recoverable. P0 gate:
  `fsync_writeback_tail_loss_tests` green **×10**.

## 4. The deterministic repro (`tests/zc_bridge_cqe_wedge_tests.rs`)

Seam `SQUEEZEFS_TEST_ZC_DROP_WRITE_CQES=N` (registry entry): the worker
consumes-and-drops the first N WRITE-class bridge CQEs — pend + deadline
stay live, the exact field posture, selected deterministically instead of
by load. Production cost: one relaxed load of a process-lifetime zero.

Venue: LIVE armed mount, local 7.1.6-1-cachyos-sqz (kmbuf 38/39), root
(zc arm needs CAP_SYS_ADMIN; unprivileged runs skip via the testkit
ledger). Both field classes in one leg: an unaligned write (at-delivery
extraction — the ~116-slot never-dispatched class) and an aligned held
write (lazy materialization — the parked-oneshot class); a healthy leg
pins both tripwires at 0 with byte-exact durable writes.

| Leg | Result |
|---|---|
| Green (1 s deadline, drop 2): both writes resolve **loud EIO** inside the ladder, `cancels ≥ 2`, `lost ≥ 2`, post-recovery write+fsync+read byte-exact | **green ×10** (consecutive, final binary; runs 1–9 of a first pass + the 10th re-run after the harness wrapper — not the test — hit its own timeout mid-run) |
| RED control (`SQUEEZEFS_ZC_BRIDGE_TIMEOUT_MS=600000` — the ladder cannot fire inside the 60 s bound) | reproduces the field wedge EXACTLY: write+fsync hangs, watchdog names the slot, test deadline fires |
| Healthy (seam unloaded) | both writes durable byte-exact, `cancels = 0`, `lost = 0` |

## 5. Gates

* Repro suite green ×10 (live armed 7.1-sqz, root).
* `fsync_writeback_tail_loss_tests` green ×10 (P0).
* Fork suite 153 green; root targeted suites (wedge_census, metrics,
  fuse_zc_write, env_knob_convention, transport_lease_overlong,
  derivation_sweep) green.
* Both-workspace `clippy --all-targets -D warnings` (root also
  `--all-features`) + `fmt --check` clean.

## 6. Field acceptance (squeeze-test, 6.19-sqz, `68f10c0f` rocky8)

Deploy: `go-task build:rocky8` @ `68f10c0f`, zstd+split-chunk transfer
(one corrupted part caught and resent by per-part md5), binary md5
`893817330f98ee4f212763ac3fc2d89b` verified, umount drain, install.

### 6.1 The zcws-9 W4 recipe ×3 — CLEAN, engagement exact

`w4x3-rig.sh` (the standing rig's W4 leg ×3, fresh armed mount each,
every gate + the tripwires asserted flat per leg), artifacts
`/scratch/tmp/zcw-w4x3`:

| leg | seqwr | dur | rand4kow (direct share) | tripwires |
|---|---|---|---|---|
| W4a | 22.168 GB/s | 31.802 GB/s | 1.104 GB/s, 99.5 % | cancels=0 lost=0 overdue=0 |
| W4b | 21.013 GB/s | 20.580 GB/s | 1.120 GB/s, 99.5 % | cancels=0 lost=0 overdue=0 |
| W4c | 20.941 GB/s | 22.834 GB/s | 1.135 GB/s, 99.5 % | cancels=0 lost=0 overdue=0 |

All 12 rows engagement-exact (≥ 95 % vehicle-byte closure, fallbacks/
slot-skips 0), correctness smokes green per leg. **The wedge did not
recur** across three sustained saturations of its exact shape — and the
zcws-9 context it fired in included a 100 %-full data volume
(`nvme12n1` 12288/12288) plus the fencing storm, a state the store no
longer carries. The bounded-outcome ladder is the standing guard for
whatever schedule selects it next: recovery ≤ deadline + one watchdog
tick, and the cancels/lost split names the kernel class in the tape.

### 6.2 The full D14 bracket (zcws-10) — wedge closed, perf rule FAILS

`2026-08-06-zc-write-side-rig.sh` verbatim (W1 zc / W2 W3 control / W4
zc / R5 zc / R6 control), artifacts `/scratch/tmp/zcws-10`, **ALL LEGS
PASSED** (engagement gates), **zero tripwires on every armed leg**
(cancels=0, lost=0, overdue=0 — W1/W4/R5 after-snapshots):

| row | armed median | control median | ratio | rule | verdict |
|---|---|---|---|---|---|
| seqwr | 21.675 GB/s | 21.795 GB/s | **0.995×** | ≥ 0.97 | PASS |
| dur | 19.040 GB/s | 19.592 GB/s | **0.972×** | ≥ 0.97 | PASS |
| rand4k (hole) | 1.176 GB/s | 1.476 GB/s | **0.796×** | ≥ 0.97 | **FAIL** |
| rand4kow | 1.121 GB/s | 1.213 GB/s | **0.924×** | ≥ 0.97 | **FAIL** |
| read sentinel | 33.989 GB/s (R5; R6 control 25.234) | — | — | ≥ 39.5 GB/s | **FAIL** |

### 6.3 Flip decision — **DO NOT FLIP** (honest)

The blocker this campaign was chartered on is CLOSED: three W4
saturations + a full bracket ran wedge-free with the tripwires flat.
But the standing flip rule fails 3 of 5 gates:

* **rand4k (hole regime) 0.796×** — the armed leg pays the
  handler→worker→oneshot bridge hop per 4 KiB extraction/store
  (~287 k vs ~360 k IOPS); the hop is structural under
  `IORING_SETUP_SINGLE_ISSUER` + per-ring bvec registration (§8).
* **rand4kow 0.924×** — same hop on the 99.5 %-direct patch path
  (~273 k vs ~296 k IOPS).
* **read sentinel 33.99 GB/s < 39.5** — even though the armed read leg
  beats its own in-bracket control by +35 % (25.23) at −70 % daemon
  CPU (182 s vs 610 s), the rule's bar is absolute and this store/day
  did not reach it.

The default stays **OFF** (`SQUEEZEFS_FUSE_ZC=off`, pinned in
`tests/env_knob_convention_tests.rs`); `docs/operations.md` carries the
knob trio + the bounded-outcome law either way. The flip pre-requisite
is now purely a PERF program (the §8 fusion lever + the read-sentinel
gap), no longer a correctness one.

### 6.4 Root-cause status + the patch-0028 question

No kernel patch is proposed in this campaign — honestly, because the
field wedge left no kernel-side artifact (no dmesg, no CQ-overflow
count, no io-wq hung-task) and did not recur under ×4 armed
saturations of its shape, so the exact race cannot be named with
evidence. What ships instead is the DISCRIMINATOR: any recurrence now
resolves within the deadline and stamps its class on the stats surface
— `fuse3_zc_bridge_lost > 0` = the kernel posted-and-lost class
(the patch-0028 case, tape in hand: op/flag shape is in the mount log's
ladder lines); cancels-without-losses + repeated re-arm warns = the
stuck-op class (io-wq/punt investigation). D13 sanctions the kernel
patch the day the instrument produces its evidence.

## 7. Out of scope (recorded hand-offs)

* Attr-floor staleness and close-storm placement skew — own campaigns
  (per the campaign brief).

## 8. The rand-4k task-hop lever — priced, declined (structural)

The zcws-9 rand4kow row ran 99.5 % direct share and still lost ~5 % to
the hop chain (handler task → `commit_tx` send + eventfd wake → worker
pushes `WRITE_FIXED` → CQE → oneshot → handler wake: two cross-thread
wakes per 4 KiB patch). Dispatching the DMA without the hop is
**structurally unavailable under the current registration model**:

1. The queue rings are `IORING_SETUP_SINGLE_ISSUER` +
   `DEFER_TASKRUN` (`fuse_over_uring.rs:3474` — the counted submit-economy
   posture); a handler thread cannot legally submit to the worker's ring.
2. The payload's pages exist ONLY as the kernel-side sparse-table bvec
   registered into THAT ring at delivery (`io_buffer_register_bvec`,
   patch 0023/0024) — `WRITE_FIXED`'s `buf_index` resolves per-ring, and
   zc means there is no daemon VA to write from anywhere else.
3. The oneshot is load-bearing: the W1 patch leg replies completion for
   O_DIRECT writes, so the ACK cannot precede the DMA's CQE.

The remaining lever is handler-lane/queue-worker FUSION for patch-class
writes (run the eligibility ladder + store on the worker thread itself,
skipping dispatch entirely) — a transport-architecture campaign, not a
cheap lever; recorded as a hand-off, not built here (the brief's
"do NOT let them delay the wedge").
