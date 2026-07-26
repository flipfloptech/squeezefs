# 2026-07-26 — DIALED P2: the kernel-path per-op economy

Branch `perf/fuse-per-op-economy` (off dev `1463a67`, left UNMERGED for
review). Commits: red `189a7d9` (compact custody-fingerprint contract),
green `1aab61b` (fingerprint economy + handler snapshot reuse),
`36b1553` (ahash hot caches), `b91cee7` (transport lock economy),
`be82794` (in-place READ replies), `442adae` (the arc-swap forensics
fix — see §6, the campaign's most instructive finding). Charter: the
shim saturates the device (480–517k, DIALED P1); the KERNEL path —
every non-preloaded application — sat at ~290–360k with qd1 per-op at
306–332 µs vs the 242 µs raw device RTT. Decompose the ~70–90 µs/op
transport+daemon term first, attack the biggest terms red-first,
adjudicate the kernel-interface floor honestly.

## 1. Substrate (labeled; same fabric-latency rig as the P1 chain)

32-CPU box, 109 GiB RAM, kernel 7.1.4-1-cachyos. Rig verified still up
knob-by-knob: configfs null_blk `sqzlat_oss0` (36 GiB memory-backed,
`completion_nsec=235000`, `irqmode=2` timer, bs 4096, 8 squeues, hw QD
128) → nvmet-loop → `/dev/nvme1n1` (data); `sqzlat_mds0` (3 GiB,
completion 0) → `/dev/nvme2n1` (meta). **Raw ceilings this session**
(fio 3.42 on /dev/nvme1n1): psync QD1 **4,120 IOPS / clat avg
242.4 µs**; libaio 16×QD16 **487k**. Filesystem: cache-less format
(`sqmeta:///dev/nvme2n1 sqdata:///dev/nvme1n1`), 4 MiB blocks; mount
`--daemon --allow-other --interception -o direct_device_true`
(queues=32 depth=32, ipc_service_threads=8). Dataset 16 × 1.5 GiB.
Instruments: **elbencho 3.1-10 (dynamic)**, fio 3.42; engagement
printed per row (`ranged_reads Δ == read_device_true_reads Δ == row
ops`, `ipc Δ = 0` on kernel rows, `ipc_ops_read Δ == ops` on il rows).

## 2. The budget table (mission item 1 — the map)

**qd1 (kernel path, single 4k O_DIRECT randread in flight).** Terms
measured three ways, all agreeing: (a) client avg 308 µs (clean mount)
vs 242 µs raw ⇒ **66 µs of transport+daemon per op**; (b) a 1 s
`strace -tt -T` chain reconstruction over 2,196 ops (ptrace-inflated,
shape-true); (c) temporary env-gated nanosecond probes (fuse3 delivery
stamp → handler entry → backend span → submit_reply; nvme worker
SQE-build → CQE; **stripped from the tree before any perf commit** —
working-tree only, never committed). Probe numbers (100k-op averages):

| Term | µs (default C-states) | µs (cpuidle ≤ C2) | What it is |
|---|---:|---:|---|
| dispatch lag (ring CQE delivery → read-handler entry) | **25.1** | 18.4 | queue-worker parse+publish, inbound push, dispatch-task wake, `tpc_spawn` → per-core lane wake |
| handler prelude (entry → backend await) | 4.8 | 3.8 | inode-lock read section, attr/metadata probes, pre-fingerprint, parked-run capture |
| routing descent CPU (backend span − nvme await) | 7.7 | ~6 | fetch_metadata, ranged dispatch, post-read binding validation |
| nvme hop + completion wake (await span − device span) | **19.8** | 13.3 | crossbeam enqueue → worker recv → SQE push; CQE → oneshot → handler-task wake |
| device span (worker submit → CQE observed) | 245.1 | 243.4 | the 235 µs null_blk + nvme-loop stack (matches 242 µs raw psync) |
| post + reply (handler return → COMMIT submit) | ~7.2 | ~6 | post-fingerprint, reply channel → reply task → `submit_reply` (pre-fix) |
| kernel transit + client (COMMIT submit → next delivery) | ~10.3 | ~9 | kernel copies payload, wakes app, app's next read(2), request re-queue, CQE post |
| **total client-visible** | **320 (probe mount)** / 308 clean | 279 | — |

Two structural reads: **(1) ~20 µs of the qd1 gap is cpuidle exit
latency, not code** (capping C-states moved dispatch+hop by −13 µs and
total to 279 µs — the handoff-economy note's qd1 finding, reproduced on
the kernel path); **(2) the daemon-side wake CHAIN (dispatch 25 + nvme
hop 20 + reply 7 ≈ 52 µs) dominates the code-owned share** — five
cross-thread wakes per op at qd1: queue-worker → dispatch task →
handler lane → nvme worker → handler lane → queue-worker.

**Depth (t16qd16, 256 in flight; clean baseline mount, 352–376k).**
Daemon CPU 11.1–11.6 cores ⇒ **~32.8 µs CPU/op**; device inflight
sampled 28–118 of 1024 offered ⇒ the pipeline is wake/queue-bound, not
device-bound (raw ceiling 487k) and not core-bound (32 cores, 11.5
used). perf (dwarf, 8 s window) userspace self-weight families:

| Family | % cycles | Content |
|---|---:|---|
| kernel-mode | ~51% | io_uring submit/complete ×2 rings, eventfd, futex, epoll, 4k payload copy, scheduler |
| fuse3 transport | 10.5% | dispatch closure 4.3%, `get_payload_buffer` 1.6% (global `pending` Mutex×3/op + arena Mutex), queue_worker, reply task, `Mutex::lock_contended` 1.6% |
| moka + hashing | 9.0% | ~5 metadata/attr gets per read (SipHash u64 keys, LRU deque maintenance) |
| tokio runtime | 7.0% | task wakes, inject/poll, timer wheel (`process_at_time`) |
| fingerprint/binding | 3.9% | `read_custody_fingerprint` ×2/op (get + per-key String clones), `CachedMetadata::clone` 1.1%, `parse_block_key`/StrSearcher |
| clock_gettime | 3.1% | diffuse: moka expiry stamps, `cached_at.elapsed()`, tokio driver |
| jemalloc | 2.2% | fingerprint vecs/strings, reply headers, delivery Vec |

## 3. What landed (mission item 2)

- **Fingerprint economy** (`189a7d9` red → `1aab61b`): `ReadCustodyFp`
  holds the shared `block_map` **Arc** + the window's custody-epoch
  words instead of cloning every covered key String twice per read;
  `matches()` keeps the exact per-block key+epoch verdict (epoch
  monotonicity kills string ABA; CoW publication makes `Arc::ptr_eq` a
  content proof — fast path). The pre-read side reuses the handler's
  size-coherency snapshot (one moka get saved); the post-read side
  keeps its fresh probe (that IS the retry authority); the anomalous
  map-id-without-map shape still rides the authoritative async resolve.
  13 contract tests (movement faces, CoW republish, window scoping,
  shape flips) red-first.
- **Handler snapshot reuse in the router** (`1aab61b`):
  `read_file_range_zero_copy_with_meta` — first resolve attempt uses
  the handler's snapshot under the EXACT `fetch_metadata` freshness
  gate (`metadata_entry_fresh_or_dirty`, extracted); retries re-resolve
  fresh. One moka get + `CachedMetadata` clone per read deleted.
- **ahash on the hot caches** (`36b1553`): `metadata_cache` +
  `attr_cache` (u64 keys, ~5 probes/op) off SipHash.
- **Sharded transport pending map + gated classical-inflight probe**
  (`b91cee7` + `442adae`): `pending` (insert at delivery / get in
  handler / remove at reply — the ~3 contended global-mutex
  acquisitions per op) sharded 64-way on `unique >> 1`;
  `classical_inflight` len-gated (armed steady-state set is empty; a
  unique's own classical insert is ordered before its reply, so the
  zero gate can never miss its own entry). `Mutex::lock_contended` left
  the profile.
- **In-place READ replies on armed sessions** (`be82794`): the READ
  handler completes its reply via `write_vectored` directly (armed-arm
  = synchronous COMMIT enqueue) instead of the unbounded reply channel
  + reply-task wake — one task wake per READ deleted at every depth;
  the channel path stays for INIT-phase/classical sessions. Error
  semantics mirror `reply_fuse` (NotFound = interrupted/double reply).
  WRITE and metadata replies deliberately stay on the channel (bounded
  blast radius; follow-on if the write rows want it).

## 4. A/B (final pair; medians, interleaved BASE/SHIP pairs ×4 — see protocol note §5)

Protocol: **interleaved pairs** (BASE rep *n* immediately followed by
SHIP rep *n*; fresh mount per side, one volume + dataset, ×4 pairs;
12 s depth rows / 10 s qd1 rows), medians of 4 with all runs shown —
the drift-cancelling protocol §5 explains. BASE = dev `1463a67`
daemon+shim pair; SHIP = branch `442adae` pair (KD-7). Engagement
exact on every row (kernel rows `ipc Δ = 0`; il rows
`ipc_ops_read Δ == ranged_reads Δ == ops`).

| Row | BASE med (runs) | SHIP med (runs) | pairwise Δ med |
|---|---|---|---|
| kernel t16 qd16 | 359.0k (349.5/362.4/361.0/357.0), avg 712 µs | 358.7k (361.0/363.2/356.4/354.6), avg 712 µs | **−0.2% — flat** |
| kernel t32 qd32 | 356.7k (343.8/359.3/354.2/362.9), avg 2.87 ms | 361.1k (361.2/363.1/360.9/360.9), avg 2.83 ms | **+1.5%** (SHIP variance visibly tighter) |
| kernel t1 qd1 | 3,208 (avg 310 µs) | 3,219 (avg 310 µs) | +0.2% — flat |
| il t16 qd16 s4 | 478.5k | 481.7k | +0.3% — unregressed (direct-drive ceiling class) |
| il t32 qd32 s4 | 525.2k | 523.4k | −0.4% — unregressed |
| il t1 qd1 | 3,914 / 254 µs | 3,904 / 255 µs | −0.2% — unregressed |
| il sync t32 qd1 | 123.4k | 123.4k | +0.1% — unregressed |
| md create storm (8t, 16,384 × 4k files) | 16.4k files/s | 15.6k files/s | −4.6% median but overlapping runs (B 16.1–17.2k, S 15.0–17.7k); the earlier −10–12% regression was found and FIXED (§6) — the fresh-volume interleaved md A/B after `442adae` read create **+7.9% / del +0.7%** (MID 17.9k/31.1k vs SHIP 19.3k/31.3k ×4); this table's md rows ran on the aged shared volume and are noise-banded |
| md stat storm | 283.7k | 287.5k | +1.3% |
| md del storm | 28.4k | 29.3k | +3.0% |

**Daemon CPU per op (the measured win)**: same-session back-to-back
t16qd16 rows, `/proc/<pid>/stat` over a 6 s window inside the row:
BASE **9.29 cores @ 309.1k = 30.1 µs CPU/op** vs SHIP **8.05 cores @
304.7k = 26.4 µs CPU/op** — **−12% daemon CPU per op** at equal
throughput. Ship-side profile confirms the named terms are GONE:
moka `get_with_hash` 2.25 → 0.86%, `hash_one` family ~3.3 → ~1.3%,
`Mutex::lock_contended` 1.62% → out of the profile,
`read_custody_fingerprint`/`load_striped_block_keys` symbols out,
`get_payload_buffer` 1.6% → `PendingMap::get` 0.56%.

**Tripwires (both sides, post-session)**: `write_path_seed_read_bytes`
0, `patch_edge_rmw_reads` 0, `ipc_descriptor_rejects` 0,
`ipc_sessions_poisoned` 0, `read_admission_governor_denials` 0,
`fsck_findings` 0, `stale_binding_rebinds` 0, `ranged_read_rebinds` 0,
`fuse_op_watchdog_overdue` 0.

**Bars adjudicated (honest)**: the mission's "cold kernel rows
meaningfully up" and "qd1 meaningfully closer to 242 µs" are **NOT
met on this rig** — the depth rows are pinned by delivered-concurrency
× wake-chain latency (359k × 712 µs ≈ 256 = exactly the in-flight the
client offers at t16qd16; device inflight samples 28–118 of it, so
~150+ ops queue inside daemon/kernel stages), and the deleted CPU was
not the binder on a 32-core box (11.5 cores used). Raising
`max_background` to 1024 live via fusectl moved t32qd32 only
336 → 346k — the kernel gate is not the residual binder either; the
wake pipeline is. What WAS delivered: −12% daemon CPU/op (the term
that matters on CPU-tight fleet boxes — the field's coherence-collapse
class), tighter depth-row variance, md-storm rows unregressed after
the §6 fix, and the §2 map + §8 residual board that names the
wake-topology work a successor must do to move IOPS on this rig. The
architectural qd1 answer remains the shim's direct-drive path (252 µs,
P1) — §7.

## 5. Protocol notes (honest)

- The first A/B protocol (side-sequential, 3 runs per row per side)
  drowned the effect in monotone box/volume drift and was retired for
  **interleaved pairs** (BASE rep n immediately followed by SHIP rep n,
  fresh mount each, same volume): drift lands on both sides of every
  pair. Sequential-side rows are recorded in `/tmp/sqz-pe/ab-session3.log`
  but not credited.
- Two harness failures aborted early counted sessions (elbencho tree
  prep without mkdir; `pipefail` SIGPIPE from `| awk 'exit'`); each fix
  restarted the count from zero.
- md-storm rows reformat per side once the aging confound was
  identified; the counted md table is fresh-volume interleaved ×4.

## 6. FOUND: arc-swap's debt registry couples foreign hot paths (the campaign's forensics lesson)

The transport commit originally converted the `over_uring` pool handle
and per-queue `arena` to `ArcSwapOption`. The md-storm A/B caught
create/del storms **−10–12%, consistent across every interleaved run**
— on rows that never touch the READ path. Bisect (BASE/MID/LK/SHIP
4-way, fresh volume per side): the entire regression entered at the
transport commit. `perf diff` on the del storm: `arc_swap::Debt::
pay_all` **4.4% → 7.9%** — on the KV CONVEYOR side. arc-swap's debt
registry is process-global: high-rate loads from 30+ transport threads
lengthened every kv node-snapshot `store()`'s `pay_all` walk on the
serialized conveyor path. Converting the hot loads to Guard `load()`
did NOT recover it (still −10%); reverting both handles to plain
`Mutex<Option<Arc>>` (`442adae`) recovered md rows exactly (create
19.3k vs MID 17.9k; del 31.3k ≈ 31.1k). The lesson, recorded for the
next person who "optimizes" an uncontended mutex into arc-swap: **the
contended lock was `pending` (sharding stays); an uncontended futex
mutex has no global write-side, arc-swap does.** The daemon's existing
per-op `session_connection.load()` + the kv store() traffic were
already coupled through that registry pre-campaign (pay_all 4.4%
baseline on del storms) — a standing residual worth its own look.

## 7. The kernel-interface floor (mission item 3 — adjudicated)

At qd1 with the daemon terms removed, the floor the architecture pays
for kernel-FUSE compatibility on this rig is:

- **device 242 µs** (raw psync, includes the nvme-loop stack);
- **kernel FUSE transit + client resubmit ≈ 10 µs/op** (COMMIT_AND_FETCH
  processing, 4 KiB payload copy out of the registered buffer, app
  wake, next read(2), request re-queue, CQE post — measured as the
  COMMIT-submit → next-delivery gap);
- **≥ 2 cross-thread wakes** that no userspace restructuring can
  delete while requests arrive on per-CPU ring workers and complete on
  a device ring (~10–15 µs at idle C-states, ~5 µs capped).

So ~255–265 µs is the honest qd1 floor for THIS daemon shape on this
rig; the shim's direct-drive qd1 (252 µs, P1 note) beats it by
deleting the FUSE transit entirely — that is the architectural answer
for latency-critical fleets, not further kernel-path shaving. At depth
the floor is the device's 487k; the kernel path's remaining distance
(see §4) is wake-topology and per-op CPU listed in §8.

## 8. Residuals (recorded, not chased)

- **Dispatch venue (25 µs qd1 / inject-queue-free but two-wake at
  depth)**: same-lane `spawn_local` for the handler would delete one
  wake but serializes per-queue handler CPU (~30 µs/op) onto one lane —
  a regression for single-thread deep-qd clients (one queue). Needs a
  load-aware venue pick; left designed-not-implemented.
- **NVMe completion hop (~20 µs qd1)**: the direct-drive engine's
  submit-from-caller + shared-reaper shape (P1) applied to the kernel
  path would delete the worker-queue hop; the FUSE reply would still
  ride the handler task unless fuse3 grows detached replies (the READ
  analog of the write-side lease-severance boundary is the blocker to
  completing FUSE replies from a foreign CQE thread — evaluated,
  recorded as the P3 candidate).
- **Per-op string keys** (`inode_path` format! + `parse_inode_from_path`
  round-trips + block-key parsing — StrSearcher 0.9% + fmt 0.8% +
  allocator share): an ino-native routing surface is a wide but
  mechanical refactor.
- **`tokio::time::timeout(30 s)` per nvme read** — per-op timer-wheel
  churn (the M4 per-op-timeout lesson in miniature); replacing with
  watchdog-class liveness changes wedged-device semantics, needs its
  own ruling.
- **arc-swap debt coupling baseline** (§6): `pay_all` 4.4% on md storms
  pre-campaign, from the daemon's existing arc-swap surfaces.
- **The `.stats` short-read via `cat`** (odirect note §7) — still open.

## 9. The folded-in kv flake (chartered, timeboxed per the mission)

`kv_smo_crash_completeness_tests::pending_free_at_cap_forced_cycle_
completes_and_conserves_extents` — reproduced **1/40, then 7/60 under
build load** on clean dev `1463a67` (declared rate gathering). Root
cause established with temporary env-gated diagnostics (stripped, never
committed): **NOT a test-side race — the at-cap drive occasionally
lands the volume in the genuine §4.7 wedge shape** the SIBLING test
constructs deliberately. Diag trace of a failing run (flake3-11):

```
[KVDIAG] flush skip-defer node=0x870000 tree=1 pending=2      (every cycle)
[KVDIAG] advance_durable seq=34431 durable=34431 parked=2 front=Some((79680, 3))
```

The two parked frees carry `retire_seq 79680`; the durable tail is
pinned at **34431** by node `0x870000`'s dirty floor; that node's flush
needs a compaction SMO; the SMO is refused at admission (FIFO at cap
2); the flush pass skip-defers it (clause c), restoring the ancient
floor — a closed dependency cycle. `checkpoint_now()` cycles COMPLETE
(one node image-write each, +8 KiB rewrite_bytes/cycle) without ever
advancing the tail past 79680, so point B's `pending == 0` can never
be reached; the volume never fails loud either, because the test calls
`checkpoint_now()` directly and the clause-b progress audit
(`force_pending_free_cycle` → `PENDING_FREE_FORCE_CYCLES` →
`fail_stop_loud`) lives only on the `run_maintenance` arms. Notably,
**the same wedge was hit ON DISK during this campaign's md storms**
(mount refused with "conveyor pass parked 30001 ms waiting for
journal-ring admission"; wedged meta image preserved at
`/home/justin/sqz-wedged-meta-20260726.img.zst`) — so this is a REAL
product wedge class at tiny FIFO caps AND (apparently, image pending
analysis) reachable at production caps under md-storm churn+kill
timing. That is genuinely deep kv work: the fix space is (a) let the
flush pass rewrite a log-full node WITHOUT retiring the old extent
inline (park the retirement against the NEW cycle's tail — breaks the
cycle structurally), or (b) extend the clause-b audit to
`checkpoint_now` callers. Per the mission timebox this is **chartered
to the kv program with this analysis + the preserved image + the
KVDIAG probe recipe**, not fixed here; the test stays as-is (it is
correctly red on a real wedge).

## 10. Gates

Final branch tip `442adae` (+ the clippy-hygiene test commit):

- `cargo clippy --all-targets --all-features -- -D warnings` clean;
  `cargo fmt --check` clean (root + fork; the fork's fmt config warns
  about a nightly-only key — pre-existing).
- `cargo test --all-features -- --test-threads=1`: the from-zero
  acceptance run is **green end-to-end — 139 test binaries, 0
  failures** (`--no-fail-fast`, rc 0). Two earlier aborted attempts
  are recorded per the multi-run discipline, both adjudicated
  NOT-this-branch: (a) `crash_kill_tests::test_kill9_remount_soak_v3_
  batched` — **pre-existing kill-9 timing flake, 1/8 failures on the
  UNTOUCHED dev `1463a67` worktree** ("acked create lost after a
  mid-batch kill-9"), 0/8 on the branch binary; recorded so it is not
  silently inherited — it needs its own red-first loop in the kv
  program (same neighborhood as §9); (b) `read_prefetch_pipeline_
  tests::pipeline_phases` — one-shot failure while this campaign's own
  repro loops ran concurrently on the box (self-inflicted co-tenancy);
  5/5 green isolated on BOTH dev and branch.
- `cargo doc --no-deps`: 4 warnings — byte-identical count on dev
  `1463a67` (pre-existing intra-doc-link nits, zero delta).
- `cargo bench --benches -- --test`: green (bench smoke).
- Loom: `tests/run_loom.sh` **42/42 green**. No new lock-free protocol
  shipped: the pending shards are plain mutexes; the pool/arena
  handles are plain mutexes (§6); the `ClassicalInflight` len gate's
  safety is a happens-before argument through the request's own
  delivery→reply chain (documented at the type), not a park/wake
  protocol.
- Preload gate: **leg 1 (unprivileged) PASSED; leg 2 (root) PASSED**
  on the final pair — mount parity + engagement, dup/close_range/
  lseek, notify delivery, netns rendezvous, fio/elbencho/libaio
  verify, kill-9 soak ×5 + fork-kill-parent, direct-drive kill-9 soak
  (engaged +15,360 serves, zero residue).
- fuse3 fork suite: 36/36 green.
- Warm fast path (constraint row, quiet box, ×3 each): BASE 20.5k
  median (21.4/20.5/20.4) vs SHIP 20.7k (22.5/20.7/19.8), serve mix
  identical (~65k fast-path serves / ~139k handoffs per 204.8k ops
  both sides) — unregressed.
- KD-7 lockstep: daemon+shim measured as same-commit pairs both sides;
  no wire/ABI change.
- Temporary decomposition probes (fuse3 delivery stamps, nvme worker
  stamps, kv KVDIAG): working-tree only, verified stripped
  (`git status` clean apart from the campaign's commits; no
  `XPROBE`/`KVDIAG` strings in the tree).

## 11. Substrate teardown

As the P1 chain: disconnect the two nvmet-loop subsystems, unlink port
52126, rmdir nvmet objects, power-off + rmdir the `sqzlat_*` configfs
null_blk items. Left up while the branch is under review (RAM-backed,
reboot-ephemeral).
