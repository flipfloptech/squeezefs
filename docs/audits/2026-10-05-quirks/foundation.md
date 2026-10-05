# Foundation design audit — rung 1 (core, io, mem, model, build, rails)

Reviewer: read-only design review. Repo `/Users/joberdorf/Source/squeezefs-zig` @ `dev`, 2026-10-05.
Ruling applied: `docs/decisions/0002-no-compatibility-obligation.md:9-12` ("the charter is the
specification; the reference is evidence … where the reference's code embodies a choice the charter does
not require and a better design exists, we take the better design and record it"). Items the rung-01
evidence note already lists under "Deviations" (`docs/evidence/rung-01-io-layer-and-budget.md:40-50`)
are not re-reported unless I disagree. Line numbers are from the tree as read today.

Legend for the verdict column: **KEEP** · **NOW** (change before rung 3 starts) · **R3/R4/R5**
(change at that rung, record now) · **ASK** (owner ruling needed).

---

## 1. Findings

One block per finding; the nine columns of the requested row are the nine labelled lines.

### #1 — `max_stripes = 64` inline stripe arrays

- **Where:** `src/core/counters.zig:20-27,50-58,150-157`
- **Reference did / port did:** Ref root crate: 64 FIXED stripes (`knobs_and_rails.md:669-676`, "fixed, not derived"); ref fuse3 fork: DERIVED `next_pow2(cpus).clamp(1,64)` with LAZILY allocated stripes (`:703-721`). Port: derives the LIVE count (`stripeCount`, :25-27) but the ARRAY is a comptime `[64]Line` in every instance (:57, :156).
- **Why it may be poor:** (a) 64 is the Rust-era ceiling, not a machine fact: on a 192-CPU box 3 threads share every stripe — the charter's "never a shared-line RMW" degrades silently. (b) The doc comment says "64 × 64 B = 4 KiB per counter" (:22) but `std.atomic.cache_line` is **128** on x86_64 AND aarch64 (`lib/zig/std/atomic.zig:430-449`), so a `ShardedCounter` is 8 KiB and a `ShardedLatencyHistogram` 16 KiB. 136 counters + 24 histograms + 41 gauges declared today ≈ 1.5 MiB of BSS; the reference has hundreds more families coming. (c) Inline arrays make every per-object embedding pay 8 KiB (`uring_fs.ReaderCount` embeds `[max_stripes]Line` per epoch bucket, `src/io/uring_fs.zig:1484`).
- **Charter requires:** §1.5 "Every constant is DERIVED … A fixed clamp needs a documented physical/format/kernel/measured reason on the line"; §10 "counters are per-thread sharded … no shared-line RMW on hot paths".
- **Proposed design:** ONE process-wide stripe arena: `ShardedCounter` = `u32` base index into a `[]Line` table sized `next_pow2(possible_cpus)` (no cap; the reason for a cap, if any, is cache footprint of the fold at `.stats` read, which is measurable), allocated once at first use. Declaring a global stays zero-cost; per-object embeddings cost 4 bytes. Fix the 4 KiB comment now regardless.
- **Cost now vs later:** Now: a 60-line refactor touching one file + 2 users of `max_stripes` (`node_cache.zig:114`). Later: every new family embeds the shape; the stats-inode fold (rung 5) hard-codes it.
- **Verdict:** **R5** (land with the `.stats` inode); comment fix **NOW**

### #2 — `Tripwire` 1-s log rate limit

- **Where:** `src/core/counters.zig:184-206`
- **Reference did / port did:** Ref: same (`knobs_and_rails.md:742-748`, `CLOCK_REALTIME_COARSE` second). Port: `last_log_s.swap(now)` (:202).
- **Why it may be poor:** None: a log-rate limiter, not a semantic; one line per site per second is the right loudness; COARSE clock keeps a fire cheap (`src/core/clock.zig:3-6`).
- **Charter requires:** §1.8 "Fail loud, never silent"; §10 "never a panic in a handler".
- **Proposed design:** —
- **Cost now vs later:** —
- **Verdict:** **KEEP**

### #3 — `m<x>` mount-tag suffix truncating thread bases to 13 chars

- **Where:** `src/core/thread_name.zig:13-45`; owed note `rung-01…md:57-58`
- **Reference did / port did:** Ref: `comm_name` appends `m{slot&0xf:x}` when a per-process `COMM_TAG` is set (`knobs_and_rails.md:778-784`). Port: same, `setMountTag` (:20-22).
- **Why it may be poor:** `setMountTag` has **zero callers** in the tree — the tag path is dead code live only in its own tests (:83-96). The tag exists for cross-PROCESS readability in `ps -L`; the accounting contract (`daemon_cpu_ns_by_class`) samples `/proc/self/task` and never needs it. Its cost is real: `sqz-uring-fs1x` loses its digit (the evidence note's own owed item), and `tag & 0xf` can only name 16 mounts. Putting the index first ("sqz-uf12-m3") would break the charter's literal names.
- **Charter requires:** §9 names are the contract (`sqz-jrnl<N>`, `sqz-nvme<N>`, `f3-ur<qid>` — no tag in any of them); §1.9 "No dead code".
- **Proposed design:** Delete `setMountTag`/`commNameWith`'s tag branch; `commName` = 15-char budget, and a `comptime`/tie test that every base pattern × its max index fits (so no truncation ever happens silently). Process identity belongs in `argv[0]`/`PR_SET_NAME` of the main thread, not in every thread's comm.
- **Cost now vs later:** Now: −30 lines. Later: rung 3 spawns `f3-ur<qid>` for 512 qids under a tag that nothing sets.
- **Verdict:** **NOW**

### #4 — Knob parser's `threadlocal last_error` + `want_buf`

- **Where:** `src/core/knobs.zig:216-238,342-372`
- **Reference did / port did:** Ref: `Result<Option<T>, KnobError>` — the error CARRIES the detail (`knobs_and_rails.md:915-917`). Port: Zig errors carry no payload, so the parser stores the detail in a thread-local and the formatted `want` string in a 160-B thread-local buffer (:219-220); `KnobError.want` is a slice INTO that buffer, invalidated by the next parse on the thread.
- **Why it may be poor:** A pure parser (file header :144-146 "Pure: no logging, no env reads") with a hidden global side channel is not pure; `lastError()` (:222-224) returns a struct with a dangling-prone slice; a reader that parses two knobs then formats the first prints the second's range. `validateOne` happens to format immediately (:267-280) so it works today — by discipline, not by type.
- **Charter requires:** §12 "Errors are error unions with typed classes"; the knob convention's own law (:17-19) "a malformed knob refuses the PROCESS … naming every offender".
- **Proposed design:** Return a value, not an error: `fn parseIntIn(T, raw, lo, hi) Parsed(T)` with `Parsed(T) = union(enum){ absent, value: T, invalid: Invalid }`, `Invalid = struct{ key, raw, want: Want }`, `Want = union(enum){ boolean, int_range: struct{lo,hi: i128}, enum_set: []const []const u8, integer }` with a `format(self, *Writer)` that prints on demand. No thread-locals, no buffer, `validateOne`/the four readers switch on the union.
- **Cost now vs later:** Now: ~80 lines in one file, 10 tests adjust. Later: the registry grows to ~250 live rows by rung 7 and every reader is written against the side channel.
- **Verdict:** **NOW**

### #5 — `intKnob` ignores the row's range; readers take a runtime `key`

- **Where:** `src/core/knobs.zig:350-356,182-192`; ref `knobs_and_rails.md:109` ("NOT range-checked here")
- **Reference did / port did:** Ref: same. Port: same, plus `std.debug.assert(lookup(key) != null)` at runtime (:351).
- **Why it may be poor:** The registry row ALREADY carries `lo..hi` and the typed default is duplicated at every call site (e.g. `checkpoint.zig:144` `DEFAULT_FLUSH_INTERVAL_MS` vs the row's `"50"` at `knobs.zig:104`). Two sources of truth for the default; the range is applied only by the gate.
- **Charter requires:** §1.6 "ONE knob convention … a registry (name, kind, admissible range, default, purpose)".
- **Proposed design:** `pub fn intKnob(comptime key: []const u8, comptime T: type, env) T` — `comptime lookup(key)` resolves the row at compile time (unregistered key = compile error, so the census rail becomes partly redundant), applies `lo..hi`, and the row's default becomes typed (`default: union(enum){derived, int: i128, on, off, …}`).
- **Cost now vs later:** Now: mechanical across ~12 readers. Later: 100+ readers.
- **Verdict:** **NOW**

### #6 — `sizing.zig` has a SECOND, lenient knob parser (`parseFloat`, clamps `>100` to 100)

- **Where:** `src/mem/sizing.zig:62-86,101-112,148-154`; spec `knobs_and_rails.md:61-80`
- **Reference did / port did:** Ref: "In-process this reader is more lenient than the gate — it accepts `12.5` … `>100` clamped to 100 WITH a warning" (:73-78). Port: byte-for-byte.
- **Why it may be poor:** Two parsers disagree on what a value MEANS: the gate refuses `12.5` and `250` for `SQUEEZEFS_TRANSPORT_MEM_PCT` (`knobs.zig:97`, int 1..=100) but the resolver accepts `12.5` and CLAMPS 250 — the charter forbids clamping and the registry header claims "the ONE parser". It is unreachable post-gate, which is exactly why it is dead logic with a different contract.
- **Charter requires:** §1.6 "integers have an admissible range and out-of-range REFUSES (never clamps)"; `knobs.zig:1-2` "the ONE parsing convention".
- **Proposed design:** Resolvers take `?u64` already parsed by `knobs.optIntKnob` (which the gate guarantees in-range); delete the float/clamp branches and their warn lines. Keep `explicit > pct > derived` as the pure function it is.
- **Cost now vs later:** Now: −40 lines, 3 tests. Later: the node-cache and IPC families copy the shape (`knobs_and_rails.md:82-84`).
- **Verdict:** **NOW**

### #7 — `derived_override` rows whose doc names an A/B control arm

- **Where:** `src/core/knobs.zig:107-108` (`NVME_*_LANES`: "1 = the single-worker A/B posture"), `:118` (`DLM_STRIPES`: "MEASUREMENT lever … 1 the everything-serializes crucible") vs `:129-130` (`META_COMMIT_BATCH_*` correctly `ab_lever`)
- **Reference did / port did:** Ref: classes exist only in the spec legend (`knobs_and_rails.md:227-239`). Port: added a `Class` field — good — but hand-assigned, and three rows contradict their own doc.
- **Why it may be poor:** §1.6 says A/B levers are "logged at mount when non-default"; a lever classed `derived_override` escapes that log. No code logs non-default levers yet (grep: only the doc comment at `knobs.zig:61`). A `derived_override` Int never chooses a data path (it sizes), so the class itself is sound — the drift is in assignment.
- **Charter requires:** §1.6 "Knobs are classified: delete / `SQUEEZEFS_TEST_*` (seams) / measurement A/B levers (logged at mount when non-default)".
- **Proposed design:** Make the class structural: `ab_arm: ?[]const u8` ("1 = single-worker") on the row, `class` derived at comptime (`ab_arm != null ⇒ ab_lever`); a registry test that any doc containing "A/B" or "control arm" has `ab_arm`. Rung 5 mount logs every row with `ab_arm` whose value is present.
- **Cost now vs later:** Trivial now; at rung 3 the transport adds ~25 levers.
- **Verdict:** **NOW**

### #8 — `SQUEEZEFS_TEST_NVME_READ_TIMEOUT_MS` is a `test_seam` with an ACTIVE 30 000 ms default

- **Where:** `src/core/knobs.zig:109`; `src/io/nvme_derive.zig:49-52,310-312`; `src/io/nvme_dev.zig:1398-1403`
- **Reference did / port did:** Ref: same row, same default (`knobs_and_rails.md:496`; `nvme_dev.md:210-219`).
- **Why it may be poor:** A seam must be "default-inert" (`knobs.zig:59`, spec §2.2) — this one governs a product behaviour. See #22 for the behaviour itself.
- **Charter requires:** §1.6 classification; §11 "seams hold the exact window a pin needs".
- **Proposed design:** Either the bound is product → a `tuning`/derived row without `TEST_`, or it is not a knob at all (derived, #22) and tests use an in-process `Device.setReadTimeoutForTest`.
- **Cost now vs later:** Trivial.
- **Verdict:** **NOW** (reclassify)

### #9 — `log.zig` thread-local expectation seam

- **Where:** `src/core/log.zig:12-22,36-54`; plumbing it forces into product structs: `src/meta/journal_lane.zig:458,499-500,780,802`, `src/meta/backend.zig:175-176,474`
- **Reference did / port did:** Ref: Rust `log` + per-test capture; no analogue. Port: `expected_errors` is `threadlocal` (:36), so a lane thread cannot see the test thread's expectation; the fix was `test_error_logs: if (builtin.is_test) usize else void` fields in two product structs.
- **Why it may be poor:** Test scaffolding in product struct layouts; every future thread-owning module (nvme lanes already log `err` from the worker, `nvme_dev.zig:830`) needs the same plumbing. The Zig test runner is single-threaded per test binary (`compiler/test_runner.zig:16,332-354` counts `log_err_count` globally), so a process-global budget is exactly as safe as the thread-local one.
- **Charter requires:** §11 red-first, no sleeps; `AGENTS.md` "a test of a path whose contract IS a loud error log declares it".
- **Proposed design:** `expected_errors`/`captured_errors` become process-global atomics (`fetchSub` with a floor); delete the `test_error_logs` fields. Optional: an expectation keyed by a comptime site label so a test cannot swallow an UNRELATED error.
- **Cost now vs later:** Now: −20 lines in product.
- **Verdict:** **NOW**

### #10 — `Facts.probe()` is bypassed; `page_size` unused; fleet share never divides the CPU root

- **Where:** `src/core/derive.zig:73-101`; bypasses: `counters.zig:34`, `uring_fs.zig:149`, `nvme_dev.zig:204` (`std.Thread.getCpuCount()` directly); `Facts.page_size` consumers: none in product (only `budget.zig:1392` test); spec `knobs_and_rails.md:894` item 31, `uring_fs.md:32-34` ("`cpus` MUST be process_parallelism ÷ fleet share, never the calling thread's `available_parallelism`").
- **Reference did / port did:** Ref: `cpu::process_parallelism()` = affinity ÷ fleet share, cached once. Port: `Facts.cpus = getCpuCount()` (affinity of the CALLING thread), NOT divided by `SQUEEZEFS_FLEET_SHARE`; three modules read the CPU count themselves.
- **Why it may be poor:** (a) The reference's "Hang-1 pinned-first-toucher sizing poison" is reproducible here: whichever thread first calls `stripeCountLive()`/`configInForce()`/`initPools()` sizes process-wide pools by ITS affinity. (b) Fleet share divides memory roots (`budget.zig:96-106`) but not CPUs — half the spec's law. (c) `page_size` is dead weight: `nvme_dev.mapAdhoc` calls `std.heap.pageSize()` (:99-105) and `budget.readRssBytes` too (:234).
- **Charter requires:** §1.5; `derive.zig:10-12` "probed once … passed explicitly; a derivation that takes `Facts` is pure".
- **Proposed design:** `Facts.probe()` once in `main` (from the main thread, with `sched_getaffinity(0)` of the PROCESS, i.e. pid not tid), stored in a process-global `facts_in_force` that the three bypassers read; add `effectiveParallelism(raw, share)` and apply it (or derive share, #19). Either use `page_size` in `mapAdhoc`/`readRssBytes` or drop the field until rung 3's FUSE geometry needs it.
- **Cost now vs later:** Now: small. Later: rung 3 geometry keys on `possible_cpus` AND `page_size`; the bypassers become precedent.
- **Verdict:** **NOW**

### #11 — `derive.tenths` justified by byte-identity with the reference

- **Where:** `src/core/derive.zig:56-64` ("so budgets agree to the byte")
- **Reference did / port did:** Ref: `c/10*8`. Port: same, for cross-check parity.
- **Why it may be poor:** Harmless (loses ≤ 7 bytes), but the stated REASON was voided by decision 0002 ("byte-identity … was a correctness oracle of convenience, never a requirement", `0002…md:13-16`). Keep integer arithmetic (no float on a budget), fix the WHY.
- **Charter requires:** `0002` §Decision 2.
- **Proposed design:** `v * 8 / 10` in `u128` is exact; or keep `/10*8` with the honest reason ("integer, monotone, no float").
- **Cost now vs later:** Trivial.
- **Verdict:** **KEEP** (comment fix)

### #12 — `uring_fs.OwnedRing` is dead code

- **Where:** `src/io/uring_fs.zig:1740-1909` (165 lines + tests :2590-2650); decision `0001…md:33-35` ("`io/uring_fs.OwnedRing` already is that ring"); `src/meta/journal_lane.zig:39-44` ("The ring is the lane's own `linux.IoUring`, not `uring_fs.OwnedRing`: that reactor's face carries … neither an FSYNC SQE nor a fixed file")
- **Reference did / port did:** Ref: `OwnedRing`/`OwnedReactor` used by its journal lane (`uring_fs.md:14-17`). Port: built it, then the journal lane declined it for good reasons and re-implemented the park (#13). No product caller remains (grep: only a doc mention in `journal.zig:8`).
- **Why it may be poor:** 165 lines of untested-in-anger ring code; decision 0001 §2 now states a falsehood.
- **Charter requires:** §1.9 "No dead code. Delete unused code; never suppress it."
- **Proposed design:** Delete `OwnedRing`/`RingWake`/`journal_ring_entries`'s owner (move the constant to `journal_lane.zig`); amend decision 0001 §2 to name `journal_lane.Worker`'s ring. Its two correct lessons (submit-first then `to_submit=0` timed wait; `wake_ud = u64::MAX`) already live in `journal_lane.zig:1108-1143`.
- **Cost now vs later:** Now: −200 lines.
- **Verdict:** **NOW**

### #13 — The eventfd-wake + armed-READ + timed-park machinery exists THREE times; raw futex wrappers THREE times

- **Where:** Park/arm: `uring_fs.zig:1845-1908` (OwnedRing), `nvme_dev.zig:676-690,743-785,927-` (armWake/workerPasses/cancelWake), `journal_lane.zig:1087-1166` (armWake/park/parkDead). Futex: `uring_fs.zig:267-277`, `nvme_dev.zig:301-307`, `journal_lane.zig:173-181`. Coalescer: ONE (`nvme_derive.zig:354-365`) — good — but `uring_fs.RingWake.wake` (:1746-1757) bypasses it and writes unconditionally.
- **Reference did / port did:** Ref: `wake_core.rs` shared; the park shapes were per-module there too. Port: copied the per-module shape.
- **Why it may be poor:** The protocol is identical and already specified once for the transport (§4: "arm the wake coalescer, write the eventfd only if arm returned true … drain → disarm → scan, the model-verified order"); rung 3 adds a FOURTH copy for `f3-ur<qid>` workers. The `FUTEX_WAKE` C-int bug was found red-first in one copy (`rung-01…md:36`); nvme_dev and journal_lane use `maxInt(i32)` while uring_fs clamps — three places for the next such bug.
- **Charter requires:** §12 "prefer plain procedures over generic machinery" cuts both ways: ONE procedure, not three. §4 wake law.
- **Proposed design:** `src/io/ring_park.zig`: `ParkedRing{ ring, event_fd, wake_buf, coalescer, wake_armed }` with `armWake`, `park(tick_ns) Verdict{event,tick}` (submit-first + EXT_ARG wait, EINVAL → untimed), `consumeWakeCqe`, `cancelWake`, `wakeHandle() -> RingWake` (through the coalescer, always). `src/sync/futex.zig`: raw `wait/wake/wakeAll` (no `Io`) used by every lane. nvme lanes, the journal lane and rung-3 workers embed `ParkedRing`.
- **Cost now vs later:** Now: ~250 lines consolidated, 3 modules touched, tests already exist for every behaviour. Later: 4 copies, each with its own teardown race.
- **Verdict:** **NOW** (before rung 3)

### #14 — Four `Oneshot` shapes; `uring_fs` allocates one per op

- **Where:** `uring_fs.Oneshot` heap + 2-party refcount (`uring_fs.zig:301-351`; `WriteDone.create(self.gpa)` per op at :1261,1272,1291,1313,1348,1365); `nvme_dev.Cell.state` inline 3-state word (`nvme_dev.zig:290-298,519-563`); `journal_lane` cell (`:425,824`); `sync.Oneshot` over `Io.Mutex` + waiter queue (`src/sync/channel.zig:77-170`).
- **Reference did / port did:** Ref: `sqz_channel::oneshot` = `Arc<Mutex<…>>` per op (`uring_fs.md:51-56`). Port: kept a per-op heap object for `uring_fs`, invented the inline word for nvme (better), and has the Io-aware channel for tasks (right for its class).
- **Why it may be poor:** The metadata device path (`device_loader.zig:115-181`: node reads, bset appends, ledger/bitmap writes, journal batches) allocates a `WriteDone`/`ReadDone` AND often a `PreparedOp[]` per op. The nvme cell design (preallocated cells = slots = `user_data`, abandon state) is the right hot-path oneshot and is already proven.
- **Charter requires:** §12 "hot paths own pools and allocate nothing per op"; §1.3 latch-free hot paths.
- **Proposed design:** `src/sync/futex_oneshot.zig`: the inline `{pending, completed, abandoned}` word with raw futex + optional deadline (nvme's `Lane.wait`), used by `uring_fs` (preallocated request cells like nvme) and the lanes. `sync.Oneshot` stays for `Io` task code.
- **Cost now vs later:** Now: medium (uring_fs request model changes). Pairs with #16/#17 — if the metadata device moves to a fixed-file lane class, `uring_fs.Pool` shrinks to the control path where a per-op alloc is fine.
- **Verdict:** **R4** (with #17); record now

### #15 — Two bounded-MPMC implementations

- **Where:** `uring_fs.Mpmc(T, comptime cap)` with futex-parked consumer (`uring_fs.zig:353-454`); `nvme_derive.BoundedMpmcQueue(T)` runtime cap, never parks (`nvme_derive.zig:373-453`)
- **Reference did / port did:** Ref: crossbeam bounded channel (uring_fs) vs its own Vyukov (nvme). Port: two Vyukov queues.
- **Why it may be poor:** Same algorithm, two files, two sets of ordering comments to keep correct; only the park differs (and the park belongs to `ParkedRing`, #13, not the queue).
- **Charter requires:** §12 one acquisition style per table; §1.9.
- **Proposed design:** ONE `sync/mpmc.zig` (runtime cap; `tryPush/tryPop`; optional `epoch` word for a futex-parked consumer). `uring_fs.queue_cap` becomes runtime (#16).
- **Cost now vs later:** Now: small.
- **Verdict:** **NOW**

### #16 — `uring_fs` 4096-deep queue and `nvme` 1024-entry ring / 4096 cells per lane are fixed, not derived

- **Where:** `uring_fs.zig:54-64` (`queue_cap 4096` "parity with nvme_dev", `ring_entries 512`, `admit_cap 256`); `nvme_derive.zig:17-27` (`uring_req_queue_cap 4096`, `ring_entries 1024` "the reference's constant"); per-lane cost `nvme_dev.zig:426-437` (4096 cells + two 4096 queues ≈ 0.5 MiB/lane; 32 lanes/device ⇒ 16 MiB/device)
- **Reference did / port did:** Ref: both constants, no measured reason beyond "backpressure bound" (`uring_fs.md:431-433`, `nvme_dev.md:555`). Port: copied.
- **Why it may be poor:** (a) `uring_fs`: 64 workers × `admit_cap` 256 = 16 384 in-flight capacity behind a 4 096 queue — on a big box the queue is SMALLER than the pool's own appetite ⇒ spurious `QueueFull` under load (a load-dependent failure, §1.1). (b) nvme: SQ 1024 < 4096 queued cells; CQ = 2048 < cells ⇒ CQ overflow is reachable (kernel buffers it since 5.5, but the charter treats `transport_cq_overflows` as must-stay-0 — here there is no overflow tripwire at all; `reap` never checks `IORING_SQ_CQ_OVERFLOW`). (c) Per-lane cells should scale with the lane's SHARE of a device's admitted depth, not a flat 4096 each.
- **Charter requires:** §1.5 "never free-floating … constants"; §1.1 "load-dependent hangs are first-class product bugs"; §4 `transport_cq_overflows` must-stay-0.
- **Proposed design:** `uring_fs.queue_cap = next_pow2(workers × admit_cap × 2)`; nvme: `cells_per_lane = next_pow2(ceil(device_depth_target / lanes))` with `device_depth_target` from the write-pipeline governor's BDP (rung 4) — until then `4096 / lanes`, floor 64; `ring_entries = cells_per_lane` so one pass can always push everything; CQ sized ≥ cells; a `cq_overflow` tripwire. Tie tests restate each.
- **Cost now vs later:** Now: cheap for uring_fs (needs #15). nvme: do with the governor.
- **Verdict:** **NOW** (uring_fs, overflow tripwire) / **R4** (nvme depth)

### #17 — The METADATA DEVICE rides the ad-hoc layer (no fixed files, shared queue, per-op heap)

- **Where:** Consumers: `src/meta/device_loader.zig:115-181`, `src/meta/backend.zig:199-200,281-282,602`; layer: `uring_fs.zig:20-24` ("No fixed files … this is the ad-hoc local file layer … the fd set churns per path"); the journal lane already took a fixed-file ring (`journal_lane.zig:22`)
- **Reference did / port did:** Ref: the same split (`uring_fs.md:20-21`). Port: faithful.
- **Why it may be poor:** The premise "the fd set churns per path" is FALSE for a metadata device: one fd per volume for the daemon's life. Node-cache misses, checkpoint node images, ledger slots and bitmap pages are block I/O on a device the charter classifies with fixed files; today each pays fdget/fdput per SQE, an MPMC hop, a futex oneshot alloc and a shared pool (`clamp(cpus/4,4,64)` workers shared with config-file writes). The evidence note admits "the reference's fixed-file/fixed-buffer measurements are not re-measured here" (`:59-60`).
- **Charter requires:** §1.2 "Block I/O = io_uring workers with fixed files; ad-hoc local file I/O = an io_uring file layer"; §13 rung 1 "io_uring file layer (O_DIRECT metadata posture …) + NVMe block device workers".
- **Proposed design:** A `MetaDevice` = an `nvme_dev`-class lane set (fixed file 0, preallocated cells, `ParkedRing`) parameterised by grain (widen/slice from `align_probe`) serving device_loader/backend/checkpoint; `uring_fs.Pool` keeps config records, images and `format`'s plain-file path. Generalising `nvme_dev.Device` (it already has O_DIRECT, fixed file, bounce pools, watermark barrier) is the smaller step.
- **Cost now vs later:** Now: medium; rung 2 code is written against `Pool.readAt/writeAt` (10 call sites). Later: rung 4 would have ONE device-lane primitive anyway; moving metadata onto it then is cheap IF the facade stays narrow — keep `device_loader` the ONLY consumer.
- **Verdict:** **R4** (with the data path); record now; keep `device_loader` as the single seam

### #18 — `budget` reads `memory.max` at the LEAF only; `memory.high` ignored

- **Where:** `src/mem/budget.zig:181-215,237-245` (`parseCgroupV2Path` → `/sys/fs/cgroup<path>/memory.max`); spec `mem_budget.md:49-58` (v2 only)
- **Reference did / port did:** Ref: same leaf read. Port: same. v2-only itself: correct for Linux ≥ 6.14 (v1 is deprecated and systemd ≥ 256 dropped it) — KEEP that part.
- **Why it may be poor:** systemd commonly caps the SLICE and leaves the service leaf at `max`; the port then reads `max` ⇒ "no cgroup tier" ⇒ 70 % of RAM while the real cage is smaller — a silent weaker posture, the exact class §1.8 bans. `memory.high` (throttle point) is the kernel's own Yellow.
- **Charter requires:** §6.2 "budget = flag → cgroup `memory.max × 0.8` → 70 % RAM"; §1.8 "Never a silent fallback to a weaker posture".
- **Proposed design:** Walk the cgroup path from the leaf to `/sys/fs/cgroup` taking `min(memory.max)` (and `min(memory.high)` as an input to Yellow); log which level bound. ~25 lines + 2 pure tests.
- **Cost now vs later:** Trivial now.
- **Verdict:** **NOW**

### #19 — `SQUEEZEFS_FLEET_SHARE` is a configured N for a derivable fact

- **Where:** `src/core/knobs.zig:112`; `src/mem/budget.zig:94-106,136-140,627-634,653`; `src/mem/sizing.zig:129-145` (floor refusal)
- **Reference did / port did:** Ref: a knob, cached once (`mem_budget.md:34,356`). Port: the knob, re-read every tick (:633).
- **Why it may be poor:** §1.5's spirit: a hand-set N that every daemon on the box must agree on is a free-floating constant with an operator in the loop (the N=32 fleet row that motivated the unreclaimable division was itself a misconfiguration class). The budget is ALREADY re-derived every tick, so a live census costs nothing extra.
- **Charter requires:** §1.5 "Every constant is DERIVED … Floors are legitimate only as physical minima"; §1.6 "the user never chooses …".
- **Proposed design:** Derive `share` per tick = number of live SqueezeFS daemons sharing my cage: pids in my cgroup's `cgroup.procs` whose `/proc/<pid>/exe` is this binary (when each daemon has its own cgroup this yields 1 automatically — the cage already divides); off-cgroup: same census over all pids. Keep an override for tests only: `SQUEEZEFS_TEST_FLEET_SHARE`. The floor refusal (`sizing.fleetShareFloorRefusal`) becomes a per-tick loud warning + Red, not a mount refusal (a daemon that starts 2nd cannot refuse the 1st).
- **Cost now vs later:** Now: ~80 lines + tests; the knob has no product reader beyond `budget.zig` yet. Later: `clients`/`status` (rung 5) want the census anyway.
- **Verdict:** **ASK** — I recommend derive + test override

### #20 — `SQUEEZEFS_TEST_*` seams compiled into product hot paths

- **Where:** `src/io/nvme_derive.zig:621-641` (`Seams`, `takeArmed`); hot-path reads at `nvme_dev.zig:796-808` (2 relaxed loads per read, 1 per write); `budget.zig:446-453` (`forceLevelForTest`); `nvme_derive.zig:593-595` (`clearCustodyPoisonForTest`)
- **Reference did / port did:** Ref: same ("one relaxed atomic load when disarmed"). Port: same; none `is_test`-gated.
- **Why it may be poor:** Two seam classes are conflated: IN-PROCESS pins (stall/slow/fail-next, force level, clear poison) that only `zig build test` ever arms, and BINARY-level seams a re-exec'd child or a fleet leg arms through the env (`SQUEEZEFS_TEST_CRASH_CHILD`, `TEST_META_BUFFERED`). Only the second class needs to exist in the shipped binary; the first is zero-cost under `if (builtin.is_test) Seams else void` and becomes IMPOSSIBLE to arm in production.
- **Charter requires:** §11 sanctions seams ("seams hold the exact window a pin needs"); §1.9 no dead code (an un-armable seam in a dist binary is dead code).
- **Proposed design:** Split: `Seams` / `*ForTest` → `if (builtin.is_test) … else void`; env-driven seams stay product, registered `test_seam`, and the startup gate logs "TEST SEAM ARMED" for every present `SQUEEZEFS_TEST_*` row (loud).
- **Cost now vs later:** Now: small. Later: rung 3 adds ~15 seams (`knobs_and_rails.md:356-365`).
- **Verdict:** **R3** (decide the split before the transport's seams land)

### #21 — Metadata `SQUEEZEFS_META_FLUSH_INTERVAL_MS` default duplicated

- **Where:** `src/core/knobs.zig:104` (`"50"`), `src/meta/checkpoint.zig:144` (`DEFAULT_FLUSH_INTERVAL_MS`)
- **Reference did / port did:** —
- **Why it may be poor:** Two literals for one default (see #5).
- **Charter requires:** §1.6 registry = the default.
- **Proposed design:** Part of #5 (typed default on the row).
- **Cost now vs later:** —
- **Verdict:** **NOW** (with #5)

### #22 — Per-read 30 s caller timeout on device reads

- **Where:** `src/io/nvme_dev.zig:519-545,1398-1403`; `nvme_derive.zig:49-52,253-258` (`ReadTimedOut` retryable → `ETIMEDOUT`, :302); ref `nvme_dev.md:210-220,569` ("retained; ≤ 3 % CPU")
- **Reference did / port did:** Ref: `sqz_time::timeout(read_timeout_ms, rx)`; writes/barriers unbounded. Port: same shape (the worker keeps the DMA buffer until the CQE — MEM-1 — so nothing is dropped mid-flight).
- **Why it may be poor:** The charter's ban is on handler-level wrappers ("dropping a handler mid-commit is the ring-wedge vector"); this wrapper drops nothing in the ring, so it is NOT the wedge vector — but it IS a per-op timeout, its default lives in a TEST row (#8), and its outcome is the wrong class: a device that stops completing reads is a volume-level event (the fail-stop lattice), not 10 000 app-level EIOs after 30 s each while writes park forever on the same lane (`device_lock_waiters` census, `nvme_dev.zig:1326-1328`).
- **Charter requires:** §4 "a deadline watchdog that logs overdue ops loudly (per-op timeout wrappers are BANNED …)"; §1.8 "Fail-stop lattice: … a device fence latches the volume FAILED (EIO until remount)".
- **Proposed design:** The watchdog (rung 3) owns deadlines: a lane whose oldest in-flight op exceeds the bound (derived `2 × routine fence bound, floor 1 s` as §6.2 already does for the allocation park) logs loudly; a second bound latches the VOLUME failed through `Fence` (`nvme_derive.zig:597-614`), failing every parked caller at once. Until the watchdog exists, keep the caller bound but as a derived constant, not a `TEST_` knob.
- **Cost now vs later:** Now: reclassify (#8). Rung 3: move the bound to the watchdog.
- **Verdict:** **ASK** (the lattice vs per-op EIO is a product-behaviour ruling)

### #23 — Two errno tables, two error sets; EINVAL for backpressure

- **Where:** `uring_fs.zig:216-257` (table, `toErrno`: "The reference replies EINVAL for backpressure and for every refusal"); `nvme_derive.zig:277-306` (`errnoToError`/`errnoFor`: `QueueFull → INVAL`); `journal_lane.zig:164-167` reuses uring_fs's
- **Reference did / port did:** Ref: per-module mappings; EINVAL for a full queue (`uring_fs.md:44-46`). Port: two tables with different shapes (array vs switch) and different sets (`uring_fs.Error` vs `nd.DeviceError`).
- **Why it may be poor:** §12 names ONE table. Rung 3's FUSE reply codec needs one errno for every error the handlers can see; with two sets it will grow a third mapping. EINVAL for `QueueFull`/`WorkerClosed` lies to the application ("bad argument") and defeats retry logic that keys on EAGAIN/EBUSY — a reference quirk the charter does not require.
- **Charter requires:** §12 "errno mapping lives in one table"; §1.8 fail loud (honestly).
- **Proposed design:** `src/core/ioerror.zig`: ONE error set partitioned by class (`Retryable`, `Terminal`, `Refusal` as sub-sets; modules declare `Error = ioerror.Retryable || …` subsets so coercion is free), ONE two-way table; backpressure → `EAGAIN`, refusals → `EINVAL`, fence/IO → `EIO`, dismount → `EROFS`. Add the EINVAL-for-backpressure row to decision 0002's ledger.
- **Cost now vs later:** Now: ~120 lines, 2 modules.
- **Verdict:** **NOW**

### #24 — `build.zig` `poisonCache()` every build; `build_options` imported by the LIBRARY module

- **Where:** `build.zig:156-170`; `:43-50` (`squeezefs_mod` imports `build_options`), `src/squeezefs.zig:12-18`
- **Reference did / port did:** Ref: `build.rs` env vars (`knobs_and_rails.md:246-252`). Port: `b.graph.poisonCache()` + two git subprocesses per `zig build`.
- **Why it may be poor:** Correct intent (`-dirty` must be true) and cheap ("milliseconds") — but because the LIBRARY module imports the commit string, every HEAD move invalidates `squeezefs-unit` and `squeezefs-rails` too, not just the exe. The idiomatic form (declare `.git/HEAD`, `.git/index` and the packed-refs file as inputs of a write-files step) gets the same truth without poisoning.
- **Charter requires:** §11 "The binary names its profile"; the daemon/shim pairing (rung 7).
- **Proposed design:** Move `version` to `main.zig` (only the exe imports `build_options`); replace `poisonCache` with `addFileInput` on `.git/HEAD` + `.git/index`.
- **Cost now vs later:** Trivial.
- **Verdict:** **KEEP** the mechanism; **NOW** for the module split (gate speed)

### #25 — `dist` = ReleaseFast

- **Where:** `build.zig:21-33`
- **Reference did / port did:** Ref: Rust `dist` profile = fat LTO + 1 codegen unit; Rust keeps bounds checks and `panic = abort` in release. Port: `ReleaseFast` drops every safety check: `unreachable`, slice bounds, integer overflow become UB.
- **Why it may be poor:** Decision 0002 row 9 ("a true bug aborts loud") and §1.8 are contradicted by a profile where a bug is UB, not an abort. Bounds checks on the hot paths cost 1–3 % — a candidate for a counted A/B, which §1 says must PRECEDE taking the faster posture.
- **Charter requires:** §1 "a mechanism holds its slot on measured evidence and may be displaced only by a counted A/B"; §1.8; `0002` row 9.
- **Proposed design:** `dist` = `ReleaseSafe` + `.full` LTO; a measured `-Doptimize=ReleaseFast` row decides later. Also run the gate's unit tests at least once in the shipped optimize mode (the evidence note says tests ran under Debug, `rung-01…md:20`) — ordering bugs surface only with optimisation.
- **Cost now vs later:** Trivial.
- **Verdict:** **NOW**

### #26 — Explorer is SC-only; TSO is hand-modelled, acquire/release not modelled

- **Where:** `src/model/explorer.zig:15-21` ("Memory model: sequential consistency … store buffering modelled explicitly where a core depends on it"); Dekker self-test `:227-286`
- **Reference did / port did:** Ref: `loom` (models C11 relaxed/acq/rel/SeqCst with happens-before + store buffering — the reference's "loom-verified protocol", `nvme_derive.zig:347-348`). Port: a plain SC interleaving explorer.
- **Why it may be poor:** Strictly weaker than the reference's checker: it proves ALGORITHMIC interleavings but cannot distinguish `.monotonic` from `.acquire`, so a wrong ordering (e.g. `Cell.in_flight` is a plain bool "handed off with the queue push (release)", `nvme_dev.zig:276-279`) is unverifiable; the charter's own claim for the payload-lease gate — "release/acquire alone permits skip-wake ∧ park" (§4) — can only be re-proved by hand-adding a store buffer per model, which is x86-TSO only. The venue and a likely target are **aarch64**, where load→load and load→store reorder too.
- **Charter requires:** §9 "Model-check every lock-free core … record the weakening that fails each model"; §1.4 portable by default (ARM is in scope).
- **Proposed design:** Add an operational weak-memory mode: per-location modification order + per-thread view (vector clock), `load(order)` picks any store not yet overtaken by the view, `.acquire` joins the store's release view, `.seq_cst` totally orders. Models declare each step's ordering instead of hand-draining buffers; "weakening" = demote one ordering. ~300 lines; the four existing models re-run under it.
- **Cost now vs later:** Now: medium. Must land BEFORE the payload-lease gate, doorbell and Dekker-pair models (rung 3/7) — otherwise their evidence notes claim more than the tool proves.
- **Verdict:** **R3** (before the transport models)

### #27 — Rails: inverse census accepts a reader in TEST code / a resolver nobody calls; the quote scan desyncs on `'"'`

- **Where:** `tests/rails.zig:55-66,135-158`; unconsumed resolvers: `src/mem/sizing.zig:148-170` (`SQUEEZEFS_TRANSPORT_MEM_*`, `SQUEEZEFS_IPC_*` have no caller outside `sizing.zig`)
- **Reference did / port did:** Ref: same scan ("exactly the reference's scan", :54). Port: same.
- **Why it may be poor:** Five registered rows are "read" only by functions with zero callers — dead knobs the rail cannot see; a knob read only inside a `test` block also passes. A char literal `'"'` or `\"` desyncs the naive quote pairing (false negatives, silently).
- **Charter requires:** §1.6 "A build-time convention test fails on any unregistered knob literal"; `AGENTS.md` "a row nothing reads is dead code and goes red".
- **Proposed design:** Scan with a real tokenizer (`std.zig.Tokenizer` — string literals come out as tokens, no pairing heuristics); track `test` block nesting and require a product-scope reader; with #5's `comptime key` the forward census becomes a compile error and the rail keeps only the inverse direction.
- **Cost now vs later:** Now: ~60 lines.
- **Verdict:** **NOW**

### #28 — `budget` sampler is its own OS thread (`sqz-membudget`), spawned with `std.Thread` while holding an `Io`

- **Where:** `src/mem/budget.zig:56,372-374,665`
- **Reference did / port did:** Ref: a tokio blocking-pool task. Port: dedicated thread + `Io.Event` stop.
- **Why it may be poor:** §9 names ONE timer thread; a 1 Hz sampler is a timer job. `sqz-membudget` is not a charter class (lands in `other`). Not wrong today (no timer thread exists yet).
- **Charter requires:** §9 "A single timer thread (binary heap + condvar, cancel-safe tombstones)".
- **Proposed design:** When `src/exec/` lands, the tick becomes a timer-thread job; keep `tickInner` pure as it is.
- **Cost now vs later:** Nil now.
- **Verdict:** **R3**

### #29 — `log.zig` is `std.log` → blocking `write(2)` to stderr from lane threads

- **Where:** `src/core/log.zig:10-34`; lane-side `log.warn/err` on product threads: `nvme_dev.zig:776,830,1401`, `uring_fs.zig:245` (`fromErrno` warns on every unknown errno)
- **Reference did / port did:** Ref: `log` crate + file appender. Port: std default handler.
- **Why it may be poor:** A lane blocked in `write(2)` on a full stderr pipe is a hang vector; §1.2's exceptions list does not include logging but the intent (no blocking syscalls on a lane) is clear. `--log-file` (0600, `O_NOFOLLOW`) is a charter requirement anyway.
- **Charter requires:** §8 `--log-file`; §1.1 load-dependent hangs are product bugs.
- **Proposed design:** A fixed ring-buffered logger (drop-counted, `log_dropped` gauge) drained by an `other`-class thread; lanes never block on a log.
- **Cost now vs later:** Rung 5 with the CLI.
- **Verdict:** **R5**

### #30 — `align_probe` grain ceiling 4096 is a FORMAT reason; `dma_align 4096` buffer-side

- **Where:** `src/io/align_probe.zig:12-20` ("the ring's page arithmetic places every unit at a 4 KiB multiple"); `nvme_derive.zig:29-33`
- **Reference did / port did:** Ref: same.
- **Why it may be poor:** These are FORMAT/kernel constants (journal page, ledger slot, 4Kn LBA) with the reason on the line — exactly what §1.5 asks. Not a CPU-page assumption: O_DIRECT needs LBA alignment, which 4096 satisfies on 16 K/64 K-page ARM kernels too.
- **Charter requires:** §1.5.
- **Proposed design:** —
- **Cost now vs later:** —
- **Verdict:** **KEEP**

### #31 — `max_lanes = 1024` static arrays

- **Where:** `nvme_dev.zig:1006-1007,1035,1486-1491` (`[1024]?*Lane` per device; `targets: [1024]u64` on the stack per `flush`)
- **Reference did / port did:** Ref: knob ceiling 1024. Port: static.
- **Why it may be poor:** 8 KiB per device + 8 KiB stack per barrier; harmless, but the ceiling is the KNOB's range, not a machine fact — an explicit `SQUEEZEFS_NVME_READ_LANES=1024` spawns 1024 threads on a 4-CPU box ("explicit wins verbatim" vs the derived cap at `cpus`).
- **Charter requires:** §1.5 (floors/ceilings need reasons).
- **Proposed design:** Lane pointers in a growable slice under the existing `grow_lock`; knob range `1..=possible_cpus × 2` evaluated at the gate from `Facts` (ranges may be runtime-derived — the registry row keeps the formula).
- **Cost now vs later:** Trivial.
- **Verdict:** **R4**

### #32 — `thread_name.classes` carries Rust-crate names (`fuse3-tpc`, `f3-ur`)

- **Where:** `src/core/thread_name.zig:62-73`; charter §9 line 268
- **Reference did / port did:** Ref: the `fuse3` crate's thread-per-core lanes. Port: faithful to the charter text.
- **Why it may be poor:** `fuse3-tpc` encodes a Rust crate and a tokio idiom; the Zig executor's per-core lanes are `SqzIo` threads. The charter lists these names verbatim, so they are a REQUIREMENT today — but a stale one.
- **Charter requires:** §9 "Thread naming is part of the observability contract (`fuse3-tpcN`, `f3-ur<qid>`, …)".
- **Proposed design:** Ask whether rung 3 may name lanes `sqz-lane<N>` / `sqz-ur<qid>` (the classifier is a table; the change is one row and a charter errata).
- **Cost now vs later:** Nil now; a rename after rung 3 breaks dashboards.
- **Verdict:** **ASK** (before rung 3)

### #33 — `venue` image recipe not in-repo

- **Where:** `scripts/venue:17` (`IMAGE=squeezefs-build:ubuntu2404`, no Dockerfile here)
- **Reference did / port did:** —
- **Why it may be poor:** The kernel series is a product surface the charter wants shipped as "a containerized, pinned build" (§3); the Zig venue depends on an image whose recipe lives elsewhere.
- **Charter requires:** §3 "ship its recipe as a containerized, pinned build".
- **Proposed design:** `scripts/venue/Containerfile` (or a pinned digest in `venue`) so `venue up` is reproducible.
- **Cost now vs later:** Trivial.
- **Verdict:** **R9** (release gates) — record now

---

## 2. Knob CLASSES in the 314-row reference table — which are poor (judgement for the rows still to be ported)

Source: `docs/porting/knobs_and_rails.md:227-239` (legend) and `:243-559` (table). The charter's classification
is three-valued — delete / `SQUEEZEFS_TEST_*` seam / A/B lever — plus "the user never chooses a data path"
(§1.6). The spec's legend has NINE classes; six of them exist only because the reference never finished the
charter's triage. Per class:

| Class (spec) | Rows | Judgement | Examples (row #) | What the port should do |
|---|---|---|---|---|
| **switch** | ~17 Bools | **Poor as a class.** Every one is a posture/data-path fork the user flips. §1.6: "a wrong path outcome is a bug, not a posture". | `INPLACE_OVERWRITE` (43: "opt a substrate into in-place … measured loss on zram") — a measured loss is a reason to DERIVE from the substrate probe, not to ask; `KVMAP`/`KVMAP_OVERLAY` (35-36) — a FORMAT decision; `OVERLAY_CLOSE_BARRIER` (46); `ZC_ACK_EARLY_ODIRECT` (110: "bytes sampled at ACK" — a correctness posture!); `SYMMETRIC_META` (264) and `META_PR_WERO` (261) — format/mount decisions; `IPC` (126: "env half of `-o interception`") — duplicate of a mount option; `TRANSPORT_DEBUG` (125) → log level; `IPC_ARENA_THP` (129) → probe `MADV_COLLAPSE` support. | Each row becomes: a probe-derived behaviour, a format bit, a mount option (`-o`), or a `TEST_` seam. Zero `switch` rows in the Zig registry; the `Class.switch_` enum arm (`knobs.zig:55`) should not exist. `DIRECT_DEVICE_TRUE` (75) is the one the charter itself names as a mount option (`-o direct_device_true`, §6.3) — mount option, not env. |
| **A/B lever** (default-on Bools) | ~60 Bools + a few Ints | **Legitimate ONLY while the measurement is open.** §1 sanctions a counted A/B; it does not sanction keeping both arms forever. ~45 of these rows cite a CONCLUDED benchmark in their doc (`.benchmarks/2026-…`). A concluded lever = a dead control arm compiled into every op. | `READ_LANE` (71), `NT_COPY` (78), `DEVICE_OVERLAY` (44), `REWRITE_SHADOW` (47), `DISCARD_ELISION` (57), `FUSE_KMBUF`/`FUSE_ZC` (96-97), `WRITE_SHARED` (76), `NUMA` (82), `DELEGATION` (194), `UPDATE_INTENTS`/`SLOT_PLACEMENT` (196-197), ten `FREE_GRACE_*` (210-219) — each "0 = the A/B control" of a measurement already recorded. | A lever row MUST carry `ab_arm` (#7) and `retire_at: rung`; the evidence note that records the A/B deletes the row and the control arm in the same PR. Port only the levers whose measurement is still OWED on the Zig code (e.g. `SMALL_FILE_PACKING` 30 and `FSYNC_PROMOTE_STAGED` 109 are the two the charter text itself names as levers). |
| **tuning** (Int with a literal default) | ~45 | **Poor by definition under §1.5** — a literal default is "a free-floating byte/thread constant" with a knob on top. | `WRITEBACK_QUEUE_CAP=4096` (49), `RECLAIM_LANES_PER_DEV=32` (54), `MAP_MIGRATE_CHUNK=512` (37), `RECLAIM_BATCH_BLOCKS=64` (51), `READ_RANGED_THRESHOLD=262144` (69), `NT_COPY_MIN=262144` (79), `FUSE_IO_URING_ENTRIES=1024` (88), `IL_REAP_PARK_MAX=24` (149), `PUBLISH_COALESCE_MAX=64` (32), `LAYOUT_DELTA_MAX_CHAIN=64` (33), `FOLD_MAX_EXTENTS=64` (39), `INODE_RECLAIM_CONCURRENCY=max(4,cpus)` (60 — labelled tuning but IS derived). | Each becomes `derived_override` with a formula (block/ring/budget/measured rate) and a tie test, or a constant with its measured reason ON THE LINE and no knob. The remaining honest `tuning` rows are timing ceilings the charter states (`TIMEOUT` 13, `*_TTL_MS` 119-122, `META_FLUSH_INTERVAL_MS` 24, `SUPERVISE_*` 14-15, `FSCK_SETTLE_MS` 224). |
| **derived override** | ~55 Ints | **Good class** — absent = derive, explicit wins verbatim, never a data path … EXCEPT rows where a sentinel value disables a mechanism. | `PATCH_MAX_BYTES` (38: "0 = the acceptance A/B lever" — 0 turns W1 OFF, yet §6.2 says "W1 is un-disableable on a production mount"); `READ_LANE_DEPTH` (72: "0 = ahead-issue off"); `WRITE_PIPELINE_DEPTH_BLOCKS` (50: "0 = …"); `RECLAIM_CAP_PARK_MS` (56). | A sentinel that disables is a disguised switch: split into the sizing override (range excludes 0) and, if a control arm is still owed, a `TEST_`/lever row with `ab_arm`. The port's 11 `derived_override` rows today are clean in this respect (`knobs.zig:95-131`). |
| **test seam** | 25 | **Right class, wrong members.** Several are not inert; some testing-only rows hide under other classes. | `TEST_NVME_READ_TIMEOUT_MS` (249, default 30000 — #8); `FUSE_OVER_IO_URING_QUEUES` (86: "TESTING only") classed tuning; `FUSE_NO_KILLPRIV` (118: "TESTING escape") not named `TEST_`; `IL_OP_TIMEOUT_MS` (146) a per-op timeout wrapper in the shim. | Rename to `SQUEEZEFS_TEST_*`, default-inert; a registry test: `class == test_seam ⇔ key starts with "SQUEEZEFS_TEST_"` and `default ∈ {"0","off","-"}`. |
| **harness** | 58 | **Should not be in the PRODUCT registry at all.** They are read by test binaries and shell scripts; registering them "so the census passes" (`:230`) is the census bending to the table. | 165-176 (`SQZ_*` rig rows), 270-310 (`TEST_REQUIRE_*`, crash-suite markers). | Split the census: product scope (`src/`) requires registry rows; test scope (`tests/`, `test` blocks) has its own small allow-list file. The port already has one harness row (`TEST_CRASH_CHILD`, `knobs.zig:132`) — the gate validating `.harness` as "never validated" (`:48-49`) is a row that does nothing. |
| **retired** | 18 | **Good** — forward-only refusals naming a successor. | 48, 55, 61-63, 102, 178-181, 206-209, 220-221, 234, 236. | Port only the ones a Zig user could plausibly still have set (the ENG-10 renames 61-63); the symmetric-flip retirements (178-181 etc.) retire behaviour the Zig port never had — a successor message for a knob that never existed here is noise. |
| **build-time** | 7 | **Not env knobs in Zig.** `build_options` (`build.zig:35-40`) is the identity. | 1-7. | Delete the class. |
| **config** (Str) | 14 | Mostly fine (paths, NQNs, key file). Two are not config: `DAEMON_PIPE` (12: an fd number passed parent→child via env — a CLI argument), `META_URI` (8: a clap default — CLI). `JOURNAL_FLUSH_INTERVAL_MS` (25: "Legacy alias … the new spelling wins") violates the convention's own "never a silent alias" and must be `Retired`. | | |
| **enum select** | 3 | Fine. `READ_TIER_ADMISSION` (66: always/second-touch/never) IS a data-path choice though — `never`/`always` are measurement arms → `ab_arm`. | | |

Net: of the reference's 314 rows the Zig registry should expect roughly **~55 derived overrides + ~10 timing
ceilings + ~15 open A/B levers + ~25 seams + ~10 config + a handful of retirements ≈ 115 rows**, not 314.
The port's current 24 rows (`knobs.zig:89-133`) are consistent with that trajectory except for #7/#8 above.

---

## 3. Good decisions worth protecting

1. **The `Class` field on registry rows** (`knobs.zig:52-68`) — the reference never had it; it is what makes
   the triage in §2 enforceable by a test. Keep it, make it structural (#7).
2. **Startup gate semantics** (`knobs.zig:244-334`): every offender at once, sorted, exact messages, unknown =
   warn-not-refuse. Byte-faithful to a GOOD part of the reference and pinned by tests (`:455-484`).
3. **Registry collision law** test (`knobs.zig:437-453`) — the ENG-10 class of bug cannot recur.
4. **`derive.clamp` with floor-wins** (`derive.zig:23-29`) and every sizing law as a pure function of `Facts`
   with a tie test (`derive.zig:73-101`; `nvme_derive.zig:70-83`; `sizing.zig:33-121`). This is the charter's
   §1.5 made mechanical; protect the "no derivation without a tie test" habit.
5. **Tripwire as a never-fatal, per-site counted, rate-limited log** (`counters.zig:173-238`) with the
   intrusive lock-free registration list — exactly §10's instrument, and `tripwireCount(site)` lets a test
   allow-list ONE site while every other stays 0.
6. **Exact-sum histograms, 26 log2 buckets, JSON shape pinned** (`counters.zig:94-167`).
7. **O_DIRECT metadata posture law** (`align_probe.zig:23-31,85-131`; `uring_fs.zig:1710-1732`): refuse a
   misaligned product write (tripwire), bounce a misaligned pointer, widen a narrow read — applied on the
   CALLER's thread before the queue ("fail before any byte is submitted" literal). The ONE sanctioned
   fallback (EINVAL on a regular file ⇒ buffered, loud, counted; a block device REFUSES) is the right
   asymmetry (`uring_fs.zig:1649-1696`).
8. **Grain probe ladder** `statx DIOALIGN → sysfs (own, parent) → 4096`, admit-or-refuse, never clamp
   (`align_probe.zig:183-218`). Probe, never a table — §3's discipline applied below the kernel-feature level.
9. **No classical fallback anywhere** (`uring_fs.zig:853-857`, `nvme_dev.zig:1324-1328`): a box without
   io_uring refuses at init; a dead ring fails its ops loud. The reference's `pread/pwrite` fallback was
   correctly NOT carried (evidence note).
10. **nvme lane shape**: arrival wakes the lane THROUGH the ring (eventfd READ armed, ONE
    `submit_and_wait(1)` per pass), preallocated cells = slots = `user_data`, the SeqCst coalescer with the
    drain→disarm→scan order, the abandon-state oneshot where the WORKER owns the DMA buffer until the CQE
    (`nvme_dev.zig:1-13,271-298,519-563,743-785`). This is the right hot-path reactor — make it THE one (#13,
    #14, #17).
11. **Runtime probe ladder for ring flags** (`nvme_dev.zig:619-635`): `SINGLE_ISSUER|DEFER_TASKRUN → +COOP →
    SINGLE_ISSUER → plain`, never a kernel-version table — §3 verbatim.
12. **Fixed buffer only where it pays** (`nvme_dev.zig:151-155,719-731`): the 64 KiB ranged slab registered,
    the 4 MiB pool deliberately not (pin cost amortised across 4 MiB DMAs; GiBs of pinned RAM avoided) — a
    reasoned, written-down choice with an engagement gauge (`dev_fixed_buf_fills`).
13. **Write watermark barrier** (`nvme_dev.zig:309-316,1478-1491`): snapshot every lane's `submitted` SeqCst
    FIRST, then wait each lane's `completed` to its snapshot — chosen over an `IO_DRAIN` fsync fan-out with the
    WHY on the line. Correct and cheaper.
14. **The fence latch + process-wide custody poison** (`nvme_derive.zig:562-614`): the false→true edge logs
    once and poisons; every later DMA re-derives the refusal from the latch. §1.8's lattice at the device layer.
15. **Memory budget as a pure `tickInner`** over injected inputs (`budget.zig:484-574`) with hysteresis edges,
    proportional floors, remainder-to-last shedding, the kernel-reclaimable exclusion and the 3-tick backstop —
    every law testable without a sampler; the production `tick` is a 3-line composition (`:651-655`).
16. **cgroup v2 only** (`budget.zig:181-189`) — right for the platform; just read the whole path (#18).
17. **`Facts.possible_cpus` separated from `cpus`** (`derive.zig:76-81`) with the WHY (qid == CPU id) — the
    rung-3 geometry will need exactly this.
18. **Thread-name classification table, first-prefix-wins** (`thread_name.zig:58-79`) — simple, testable.
19. **Explorer: state-graph memoisation + mandatory `expectFails` weakening** (`explorer.zig:1-31,134-146`):
    a model that cannot fail when weakened "is not checking anything". Keep this law when the memory model
    improves (#26).
20. **Two build profiles with the identity baked in and `dist` refusing under the wrong profile**
    (`build.zig:124-146`); the `-dirty` truth (#24 keeps it).
21. **Red-first bugs recorded in the evidence note** (`rung-01…md:35-38`): the `FUTEX_WAKE` C-int and the
    `io_uring_enter` ETIME-discard findings are exactly the kind of knowledge that must live in ONE primitive
    (#13) so it cannot be re-learned.
22. **Decision 0002's ledger** itself: nine quirks already refused with reasons. Add rows for #23 (EINVAL for
    backpressure), #3 (comm tag), #6 (lenient second parser), #19 if ruled.

---

## 4. Suggested order of the NOW items (smallest blast radius first)

1. Comment/doc fixes: #1 (8 KiB), #11; decision 0001 §2 and the 0002 ledger rows (#3, #6, #23).
2. Deletions: #12 `OwnedRing`; #3 mount tag; #6 lenient parser branches.
3. Knobs: #4 value-returning parser → #5 `comptime key` readers with typed defaults (#21) → #7 structural
   class + the `test_seam ⇔ TEST_ prefix ∧ inert default` test (#8).
4. #9 global log expectation (deletes product test fields).
5. #10 `Facts` in force (process affinity, not thread), used by the three bypassers.
6. #23 one errno table; #15 one MPMC; #13 `ParkedRing` + `sync/futex.zig` (adopt in nvme_dev and
   journal_lane; this is the big one, ~1 day, and it is what makes rung 3's workers a composition rather than
   a fourth copy); #16 uring_fs queue derivation + nvme CQ-overflow tripwire.
7. #18 cgroup path walk; #25 dist = ReleaseSafe + ship-mode test run; #24 module split; #27 tokenizer rail.

Open rulings requested from the owner: #19 (derive the fleet share), #22 (device-read deadline: lattice vs
per-op EIO), #32 (thread-name errata before rung 3).
