# 2026-07-27 — Async block reclaim: the overwrite discard stream leaves the write path

| | |
|---|---|
| **Branch / SHAs** | `perf/async-block-reclaim` off dev `2bd041e` — tests `c2ecbdc` (red), impl `f5bae63`, integration `69a85fa`/`159eb55`, test venue `8011864`/`3e594dc`/`2d1fdc2`, docs `6b3d207` |
| **Field conviction** | 4-node NVMe-oF/TCP cluster, elbencho 16t 1 MiB O_DIRECT, A-B-B-A order-controlled: FIRST write ~5.4–5.5 GB/s on kernel and shim paths alike; OVERWRITE ~3.46–3.49 GB/s, order-independent. Amp already ~1.0 (e8bab83 BLKDISCARD fix) — the residual: each displaced block's discard issued SYNCHRONOUSLY in `free_block` on the write path, a serialized fabric-RTT (~235 µs) stream |
| **Fix** | `src/block_reclaim.rs`: terminal frees enqueue `{allocator, inflight guard, device, offset, size}` to a lock-free per-router queue; a background worker batches (`SQUEEZEFS_RECLAIM_BATCH_BLOCKS`=64 / `_MS`=2), coalesces adjacent ranges, issues BLKDISCARD/PUNCH_HOLE on the blocking pool, then `finish_free`s. The begin_free→reclaim→finish_free window law is UNCHANGED — its tail just runs off the write path, so a queued discard can never race a new owner's DMA. ENOSPC pressure valve: `allocate_block` force-drains before refusing (`block_free_reclaim_sync_drains`, 0 except under real pressure). Clean unmount and mover-job convergence drain the queue |
| **Crash posture** | The queue is RAM-only space-RETURN work; free ACCOUNTING (begin_free + recovery walk) is untouched. A discard lost to kill-9 is un-returned thin-device space, re-covered when the offset is reused (allocation prefers the free list; write-before-publish rewrites the range) or freed terminally again. No replay kept — pinned by the kill-9 soak (`tests/async_block_reclaim_tests.rs` contract 5) |
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

## 6. Open questions

* **OQ-1 (pre-existing):** why does a 1 MiB O_DIRECT overwrite stream
  issue ~2× the FUSE ops of the fresh-write stream (7936 vs 3968 for
  3 GiB, both binaries)? Candidate next metadata-economy target; it is
  the dominant residual in the rig's fw→ow gap.
* **OQ-2:** field re-measure on the 4-node cluster (the ~2 GB/s claim) —
  the counters above are the engagement instrument; expect
  `block_free_reclaim_queued ≡` displaced blocks and `sync_drains = 0`.
* **OQ-3:** discard batching currently coalesces adjacent ranges only;
  a range-list DSM submit (multiple ranges per command) is available if
  a field profile shows command-count pressure.
