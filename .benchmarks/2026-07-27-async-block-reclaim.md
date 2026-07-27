# 2026-07-27 — Async block reclaim: the overwrite discard stream leaves the write path

| | |
|---|---|
| **Branch / SHAs** | `perf/async-block-reclaim` off dev `2bd041e` — tests `c2ecbdc` (red), impl `f5bae63`, integration `69a85fa`/`159eb55`, test venue `8011864`/`3e594dc`/`2d1fdc2`, docs `6b3d207` |
| **Field conviction** | 4-node NVMe-oF/TCP cluster, elbencho 16t 1 MiB O_DIRECT, A-B-B-A order-controlled: FIRST write ~5.4–5.5 GB/s on kernel and shim paths alike; OVERWRITE ~3.46–3.49 GB/s, order-independent. Amp already ~1.0 (e8bab83 BLKDISCARD fix) — the residual: each displaced block's discard issued SYNCHRONOUSLY in `free_block` on the write path, a serialized fabric-RTT (~235 µs) stream |
| **Fix** | `src/block_reclaim.rs`: terminal frees enqueue `{allocator, inflight guard, device, offset, size}` to a lock-free per-router queue; a background worker batches (`SQUEEZEFS_RECLAIM_BATCH_BLOCKS`=64 / `_MS`=2), coalesces adjacent ranges, issues BLKDISCARD/PUNCH_HOLE on the blocking pool, then `finish_free`s. The begin_free→reclaim→finish_free window law is UNCHANGED — its tail just runs off the write path, so a queued discard can never race a new owner's DMA. ENOSPC pressure valve: `allocate_block` force-drains before refusing (`block_free_reclaim_sync_drains`, 0 except under real pressure). Clean unmount and mover-job convergence drain the queue |
| **Crash posture** | The queue is RAM-only space-RETURN work; free ACCOUNTING (begin_free + recovery walk) is untouched. A discard lost to kill-9 is un-returned thin-device space, re-covered when the offset is reused (allocation prefers the free list; write-before-publish rewrites the range) or freed terminally again. No replay kept — pinned by the kill-9 soak (`tests/async_block_reclaim_tests.rs` contract 5). The same posture governs a FENCED holder (contract 6, review pin): the reclaimer observes the D0 `failed` latch once per batch and ceases all device commands permanently — halted entries drop without `finish_free` into `block_free_reclaim_fence_halts` (0 on healthy mounts), the successor writer's recovery owning the accounting; the ENOSPC valve refuses to sync-drain on a fenced daemon (allocation just fails) |
| **Suites** | `tests/async_block_reclaim_tests.rs` (7 tests: queue+worker completion, ENOSPC valve red-first, unmount drain, exactly-once under concurrent frees+drains, kill-9 ×5 soak); `tests/block_free_reclaim_tests.rs` re-pinned (same ledger, background venue). Full gate from zero at `2d1fdc2`+docs: clippy `-D warnings` clean, fmt clean, `cargo test --all-features -- --test-threads=1` **141 binaries / 1460 tests / 0 failed**, doc clean, bench smoke 23/23. No new lock-free core (crossbeam `SegQueue` + counters) ⇒ no new loom model |

## 1. Substrate + instrument (stated, per house rules)

**Substrate:** the DIALED fabric-latency rig on **nvmet-tcp** (two-substrate
rule: fabric-sensitive write rows) — data = configfs null_blk 24 GiB,
`memory_backed=1`, **`completion_nsec=235000`, `irqmode=2`** (the field's
~235 µs RTT modeled at the device — `.benchmarks/2026-07-25-odirect-randread-concurrency.md`
recipe), `discard=1`, 8 squeues, hw QD 128; meta = fast null_blk 3 GiB,
256 MiB write-back cache, completion 0. Both exported over **nvmet-tcp on
127.0.0.1:54130** (TCP service slice 54100–54199), `nvme connect -i 8`.
Latency verified: fio psync QD1 randread clat avg **248 µs**. BLKDISCARD
verified supported end-to-end. Box: 32 CPUs, 109 GiB RAM, quiet.

**Instrument:** elbencho (dynamic), kernel FUSE path (unintercepted),
`-t 16 -b 1m --direct`, 16 files × 192 MiB (3 GiB dataset = 768 × 4 MiB
blocks). Rows per leg: `fw` fresh-file first write; `ow1`/`ow2` full
overwrites of the same files (every block displaced); `sr` cold seq read
after remount. Each leg = fresh format + mount. Amplification columns per
the standing instrument: `/proc/diskstats` deltas on the data namespace
(AMP = device write bytes ÷ user bytes), stats-inode deltas per row.

**A-B-B-A order control:** legs run A1(before) → B1(after) → B2(after) →
A2(before), plus a third pair B3 → A3. Before = dev `2bd041e` release
build; after = branch `6b3d207` release build.

(An earlier attempt on the standard zram-backed tcp devsub measured
~330 MiB/s zram-zstd-bound writes with ow ≈ fw — the zram CPU cost and
µs-class localhost RTT hide the discard term entirely; scoping evidence
only, which is why the dialed rig above is the measurement venue.)

## 2. Write rows (MiB/s; medians of 3 legs; per-bracket deltas cited)

| Row | before (A) | after (B) | Δ median | bracket 1 (A1→B1) | bracket 2 (B2→A2) | bracket 3 (B3→A3) |
|---|---|---|---|---|---|---|
| first write `fw` | 5119 / 5043 / 4783 → **5043** | 4916 / 5373 / 4666 → **4916** | −2.5 % (noise band) | −4.0 % | +6.5 % | −2.4 % |
| overwrite `ow1` | 3992 / 3500 / 3668 → **3668** | 4217 / 3862 / 3976 → **3976** | **+8.4 %** | +5.6 % | +10.3 % | +8.4 % |
| overwrite `ow2` | 3821 / 3606 / 3746 → **3746** | 3917 / 3904 / 3902 → **3904** | **+4.2 %** | +2.5 % | +8.3 % | +4.2 % |

* **Overwrite improves in BOTH bracket orders on every pair** (the
  A-B-B-A rule this campaign adds to AGENTS.md); first-write is parity
  within the rig's ±5 % noise band.
* **AMP = 1.000 on every overwrite row, both binaries** (fw 1.005 — the
  layout spill block); `wareq-sz` full-block (w_MiB ≡ user bytes).
* **Engagement (the ledger):** after-binary overwrite rows show
  `block_free_reclaim_queued = 768` ≡ displaced blocks ≡
  `block_free_discards = 768` (now counted from the background worker),
  `block_free_reclaim_batches` 168–269 (batching engaged, ~3–4.5
  blocks/batch at this arrival rate), diskstats `d_ops` ≈ 712–744 within
  the row window + settle (remainder lands in the drain). Tripwires:
  `block_free_reclaim_sync_drains = 0`, `write_path_seed_read_bytes = 0`,
  `patch_edge_rmw_reads = 0`, `block_free_reclaim_queue_bytes` returns
  to 0 after every row.

## 3. Honest residual (counter-attributed, NOT discard)

The after-binary still pays fw → ow ≈ −22 % on this rig. Counter diff
(B1.fw vs B1.ow1) attributes it to pre-existing displacement-path costs,
identical in the before binary:

* `fuse_ops` **doubles** on overwrite (3968 → 7936 for the same 3 GiB —
  ~1.6 extra FUSE ops per 1 MiB write against an existing file; identical
  in A1), `meta_kv_journal_bytes` 597 KB → 951 KB (CoW republish),
  `overwrite_seed_{deferred,skipped} = 768` (per-block bookkeeping).
* On this rig the discard term itself is bounded (~768 × ~250 µs spread
  over 16 write lanes); the measured +4–10 % A/B gain is what the discard
  stream cost HERE. The field's ~2 GB/s magnitude rides the real
  cluster's fabric and target deallocate cost — **field re-measure is the
  follow-up**, with the same counters as the engagement instrument.
* The overwrite `fuse_ops` doubling is a separate, pre-existing
  metadata-economy question (open question OQ-1 below).

## 4. Seq-read bracket — the 0.90× ordering ghost: RETIRED

Fresh-dataset cold seq read (write fresh files, remount, read; NO
overwrites — the aging-free venue), A-B-B-A on the same rig:

| Leg | MiB/s |
|---|---|
| FA1 (before) | 3591 |
| FB1 (after) | 3719 |
| FB2 (after) | 3607 |
| FA2 (before) | 3381 |

Both bracket orders show after ≥ before (bracket 1: +3.6 %; bracket 2:
+6.7 %); medians 3486 (A) vs 3663 (B). The post-overwrite `sr` rows from
§2 scatter 3300–3746 across BOTH binaries with mixed direction — an
aged-store/noise band, not a binary effect. **Verdict: the suspected
0.90× after-binary seq-read regression was an ordering artifact of
single-order comparison on an aging store; retired.** (Exactly the
failure mode the new A-B-B-A rule exists to prevent.)

## 5. Reproduction

```bash
# dialed rig (recipe in /tmp/dialed_rig.sh form): null_blk data 24G
#   memory_backed=1 completion_nsec=235000 irqmode=2 discard=1, meta 3G fast,
#   both nvmet-tcp on 127.0.0.1:54130; nvme connect -i 8
# per leg (fresh format each):
squeezefs format sqmeta://$MDS sqdata://$OSS --force
squeezefs mount sqmeta://$MDS /mnt/rb --daemon --allow-other --mem-cache-size 1GB
elbencho -w -t 16 -s 192m -b 1m --direct /mnt/rb/d/f{1..16}   # fw, then ow1, ow2
# per row: /proc/diskstats delta on $OSS + .stats deltas
#   (block_free_{discards,reclaim_queued,reclaim_batches,reclaim_sync_drains})
```

## 6. Field ledger inversion (three-session matrix) — 2026-07-27 follow-up

**Branch / SHAs:** `fix/reclaim-field-ledger` off dev `78b9498` — tests
`c84c655` (red: all three contracts fail on their conviction), fix
`f3c5a27`, docs `a125ab9`. Full gate from zero at the branch tip: clippy
`-D warnings` clean, fmt clean, `cargo test --all-features --
--test-threads=1` **142 binaries / 1482 tests / 0 failed**, doc clean,
bench smoke 23/23. (Two earlier gate attempts were aborted-and-restarted
per the counted-run discipline: one starvation trip of the PRE-EXISTING
fuse3 debug-only ≥1 s payload-lease wall-clock assert while a sibling
campaign's fio matrix held the box at load 43–54 — signature-matched
environmental, standalone reruns green, zero overlap with this diff —
and one self-inflicted mid-run docs commit tripping the build-commit
identity test.) Field venue: the user's 4-node
NVMe-oF/TCP cluster, zram-lz4 data targets, dev tip `78b9498`, ONE daemon
across all three sessions (counters cumulative). OQ-2's field re-measure
arrived and INVERTED the design contract:

| Session | Store shape | Workload | Ledger observed |
|---|---|---|---|
| **A** | `backend_fill_ratio = 1.0` | 2× elbencho 32t×4MiB O_DIRECT, 16×10 GiB (run 1 fresh, run 2 full rewrite, ~40,960 blocks "displaced") | rewrite FASTER than fresh (7.7 vs 6.26 GB/s); `queued=0, batches=0, discards=0, punches=0, skipped=0` — **zero device reclaim commands ever** — while `sync_drains=67,376` (> displaced, ≈ per allocation), later climbing to 99,361 during idle-ish reading |
| **B** | user `rm`'d ~160 GiB | unlink storm | frees DID engage the engine |
| **C** | fill ≈ 0.62 (cursor still at cap) | fresh h-files then full rewrite, 16t×1MiB, ~80 GiB each (~20,480 displaced) | rewrite tax RETURNED: 5,466 → 3,672 MiB/s (−33 %); B+C deltas: `queued +49,278, batches +727, discards +56,615 (237 GB), sync_drains +77,428` (≈ 1–2 per allocation at 38 % headroom), gauge back to 0 |

### Root causes (one per conviction; repro contracts 7–9 in `tests/async_block_reclaim_tests.rs`)

1. **Valve ≈ per allocation regardless of headroom — the lost-race
   fresh-mint fallback.** `try_allocate_block` read ONE free-list head;
   a loser of the `DashSet::remove` race fell straight to
   `next_fresh_block()`, which refuses `StorageFull` on any
   **cursor-at-cap** store (any store that has EVER been full —
   `highest_block` never shrinks; the field store lived in that regime
   from session A onward). Every lost race fired the ENOSPC valve: a
   `sync_drains` bump **plus a synchronous whole-queue discard drain on
   the write path** — and `drain_sync` also waits out the worker's
   in-flight batch (`processing == 0`), so a single lost race stalls the
   writing task behind up to a full 64-entry fabric discard batch. With
   16–32 lanes all reading the same head, losing was the common case:
   ≈ 1–2 valve passes per allocation at 38 % headroom. **This is also
   conviction 3's −33 % rewrite tax** — the discard stream was back on
   the write path, just wearing the valve's clothes. Fix: the claim
   loop retries the next candidate until the list is observed empty
   (livelock-free — every lost race is another thread's claim).
2. **Session A's `queued=0` / zero device commands at fill 1.0 — the
   brim staging spiral, not a bypassed free.** CoW's
   allocate-before-free can never converge at fill 1.0: the write-through
   allocation genuinely failed, the never-lossy fallback ACKed every
   write into staging, and the writeback backlog re-failed the same
   allocation forever (each attempt another empty-queue valve pass —
   the "idle-ish" `sync_drains` climb to 99,361). **No displacement free
   ever happened** — nothing merged, so nothing displaced: `queued=0`
   honestly reported an engine that was never reached. "Space reuse
   worked" was staging absorption (and why the rewrite was *faster* than
   fresh: mmap staging vs device path); the rm in session B is what let
   the backlog drain. Fix: a genuine-StorageFull full-block rewrite of a
   sole-owned, undecorated, passthrough mapping lands **in place** at
   its own offset (the W1 `begin_patch_sole_owner` incarnation fence,
   whole-block face; same-key merge for the size floor only) — no
   allocation, no free, no staging detour; counted in
   `write_through_inplace_rewrites`. Genuine frees at the brim
   (rm/truncate) ride the queue with device commands, unchanged.
3. **The engaged-engine tax at fill 0.62** — root cause 1's write-path
   drains (there was no separate discard-contention term at this
   arrival rate; killing the valve storm removes the synchronous work).
   Queue-cap inline backpressure was NOT engaged (cap 4096 ≫ the ~930
   blocks/s arrival), but its arm was found under-counting `queued` —
   fixed: `queued` now means "entered the reclaim engine" in both arms,
   so `queued ≡ displaced` holds under backpressure too.

### Why the existing cargo suite stayed green

Every pre-existing test hand-allocated offsets and freed them via
`backend_router.free_block(&offset.to_string())` directly — single
blocks, no concurrency on the allocator, never through the striped write
path. The valve test freed FIRST and allocated second (queue non-empty,
single-threaded: no lost race, no empty-queue pass), and nothing ever
allocated in the cursor-at-cap free-list regime or rewrote a file at the
brim through `fs.write`. Contracts 7–9 close the gap: real
`fs.create`/`fs.write`/`fs.setattr` traffic, concurrent per-block rewrite
tasks, cursor-at-cap premises asserted.

### Free call-site table (every caller of the free path and where it routes)

| Call site | Route | Device reclaim? |
|---|---|---|
| `upload_full_block` displaced keys (write-through overwrite) | `BackendRouter::free_block` → **queue** | yes (worker) |
| `flush_one_active_block` displaced keys (writeback merge) | queue | yes |
| striped RMW `write_all_blocks` displaced keys | queue | yes |
| staged→striped promote: displaced `prev` mapping | queue | yes |
| truncate/unlink/punch sweeps (`truncate_layout`, delete sweeps, `RemoveBlocks`) | `free_blocks` → queue | yes |
| indirect-blob displacement (`old_indirect_to_free`) | queue | yes |
| brim in-place rewrite (NEW) | **no free at all** — same offset reused, space-neutral | n/a (no space returned, none owed) |
| error-unwind frees of freshly-allocated blocks (`allocator.free_block(offset)` in upload/flush/promote/spill/clone failure arms) | **accounting-only** `begin_free`+`finish_free` | **no** — deliberate: mostly-unpublished blocks; where the DMA already landed (write-ok-merge-failed), the un-returned thin space is re-covered on reuse (the documented kill-9 posture) |
| fenced daemon (any of the above) | entries drop without `finish_free` | never (`fence_halts`) |

### Corrected counter semantics (normative)

* `block_free_reclaim_queued` — terminal frees that **entered the
  reclaim engine**: background queue AND the cap-forced inline
  backpressure arm. `queued ≡ displaced/terminal frees` always.
* `block_free_reclaim_sync_drains` — valve passes that **actually
  reclaimed ≥ 1 queued entry** for a genuinely-failing allocation.
  Empty-queue passes are no-ops and uncounted; explicit drains
  (`reclaim_drain`, unmount) never count; a fenced daemon never drains.
  Still "≈ 0 except under real space pressure" — and now it can't lie.
* `write_through_inplace_rewrites` — NEW: brim in-place full-block
  rewrites (no allocation, no free, no staging). 0 except at genuine
  space pressure; growth here with `write_through_fallbacks` quiet is
  the designed brim posture.

### A-B-B-A rewrite-latency guard

**Substrate: LOOP devsub** (`sudo tests/dev_substrate.sh create` — mds
null_blk ×4, oss zram-zstd 8 GiB ×4 = 32 GiB data). The **tcp devsub was
unavailable**: it exists on this box but every namespace is owned by the
sibling campaign's live daemon (`/mnt/oq1`, pid-recorded by `status`),
and the script's per-transport names admit no second tcp instance —
tcp re-run is the standing follow-up (OQ-2b below). The box was also
heavily contended throughout (load 43–54 / 32 CPUs: this campaign's own
full cargo gate + the sibling campaign) — **every throughput column
below is scoping-only; the LEDGER identities are the acceptance.**

**Instrument:** elbencho 3.1-10 (dynamic), kernel FUSE path,
`-w -t 16 -b 1m --direct`, 16 × 384 MiB (6 GiB = 1,536 × 4 MiB blocks).
**Field shape per leg** (fresh format each): overfill to ENOSPC
(cursor → cap on every volume), `rm` the filler (free-list regime — the
field's session-B/C store), then rows `fw` (fresh h-files), `ow1`/`ow2`
(full overwrites, 1,536 displaced each). A = before (`78b9498`),
B = after (`f3c5a27`), order A1→B1→B2→A2. Recipe:
`/tmp/reclaim_abba_rig.sh` form (this note §5 + fill-to-cap + rm).

| Leg·row | MiB/s (scoping-only) | queued | discards | sync_drains | batches | wt_fallbacks |
|---|---|---|---|---|---|---|
| A1 fw / ow1 / ow2 | 2923 / 3201 / 3185 | **6157** / 1536 / 1536 | **7179** / 1536 / 1536 | 0 / 0 / 0 | 102 / 218 / 186 | 0 |
| B1 fw / ow1 / ow2 | 2734 / 3279 / 3196 | **6628** / 1536 / 1536 | **6628** / 1536 / 1536 | 0 / 0 / 0 | 95 / 191 / 124 | 0 |
| B2 fw / ow1 / ow2 | 3013 / 3045 / 3075 | 6628 / 1536 / 1536 | 6628 / 1536 / 1536 | 0 / 0 / 0 | 99 / 298 / 217 | 0 |
| A2 fw / ow1 / ow2 | 2388 / **1274** / **669** | 6542 / 1536 / 1536 | **7730** / 1536 / 1536 | 0 / 1 / 0 | 106 / 456 / 651 | 0 |

* **Ledger acceptance (after-binary, both legs): exact.** `queued ≡
  discards ≡ displaced` on every row (ow rows 1,536 exactly),
  `sync_drains = 0` everywhere, `write_through_fallbacks = 0`,
  `write_through_inplace_rewrites = 0` (free-list regime — the brim path
  correctly never engaged), gauge back to 0 per row.
* **The rig caught root-cause 3's under-count LIVE on the before
  binary:** A-leg `fw` rows show `queued < discards` (6,157 vs 7,179;
  6,542 vs 7,730) — the rm burst overflowed the 4,096-entry cap and the
  inline-backpressure arm processed (and discard-counted) entries it
  never `queued`-counted. B-legs: exact identity (6,628 ≡ 6,628). (The
  fw rows absorb the filler-rm frees because unlink destroys are
  batched/deferred past the settle window — stated, not hidden.)
* **Rewrite ≈ fresh on the after binary:** ow1/fw = **1.20 (B1)** and
  **1.01 (B2)**; fw is itself depressed by the ~6,600-discard rm backlog
  landing in its window. The before binary's A2 collapse (ow2/fw = 0.28,
  batches 651) coincided with peak sibling+gate contention —
  scoping-only, not attributed. The loop rig cannot price the fabric
  discard term by design (two-substrate rule); the field convictions'
  performance face is carried by the red cargo repros (contract 7
  reproduced `sync_drains > 0` under 8-way concurrency in a debug build)
  and the field ledger itself.

## 7. Open questions

* **OQ-1 (pre-existing):** why does a 1 MiB O_DIRECT overwrite stream
  issue ~2× the FUSE ops of the fresh-write stream (7936 vs 3968 for
  3 GiB, both binaries)? Candidate next metadata-economy target; it is
  the dominant residual in the rig's fw→ow gap.
* **OQ-2:** ~~field re-measure on the 4-node cluster~~ — ARRIVED
  2026-07-27 and inverted the ledger; adjudicated in §6 (root causes,
  fixes, contracts 7–9). **OQ-2b (new):** re-run the §6 A-B-B-A on the
  tcp devsub once the sibling campaign releases it, and a second field
  re-measure on the fixed binary — expect `queued ≡ displaced`,
  `sync_drains = 0` at any fill, `rewrite ≈ fresh` in the free-list
  regime, and `write_through_inplace_rewrites > 0` only at genuine brim.
* **OQ-2c (new, residual by design):** at the brim, PARTIAL-block
  rewrites (and transformed/decorated/shared mappings) still take the
  never-lossy staging ladder — genuine space pressure, now honestly
  counted (`sync_drains` no longer climbs on empty-queue passes). If a
  field profile shows a partial-write brim workload mattering, the
  in-place swap generalizes to the writeback flush unit
  (`flush_one_active_block`) behind the same predicate.
* **OQ-3:** discard batching currently coalesces adjacent ranges only;
  a range-list DSM submit (multiple ranges per command) is available if
  a field profile shows command-count pressure.
