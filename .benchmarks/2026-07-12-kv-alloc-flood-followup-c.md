# Follow-up C — the "CoW-KV metadata-core allocation flood": root cause, fix, acceptance

Date: 2026-07-12 · Branch `fix/kv-alloc-flood` off dev @ `e1601bc` · Rails: 16-core cap
(`taskset -c 0-15`, `CARGO_BUILD_JOBS=12`), **CPU capped 3.2 GHz** (new vs the 3.5 GHz-era
lineage rows), 8G cages (`systemd-run --scope -p MemoryMax=8G -p MemorySwapMax=0`),
Tctl-gated. Repro sandbox: `~/tmp/kvflood` (meta 2G / data 8G / staging on NVMe) +
`/tmp/kvflood_mnt` (bench-auto agent owns `/tmp/sqperf` + `/tmp/squeezefs_mount` — untouched).

## 1. The verdict up front

The PR 7 fingerprint ("RSS +1.3 GiB in ~1.3 s, journal ~1,300 entries/s with data I/O
frozen, aged daemon ~5.9 M fuse_ops" — the intermittent QUICK tier-tail cage-kill class)
is **NOT a KV metadata-core flood**. dhat attribution on the aged daemon shows the
KV core allocating modestly; the journal-entry rate is a *co-symptom* (each storm op
commits size/mtime = one journal entry). The flood is the **staged-file whole-image RMW
in `write_file`**: every sub-block write/punch/copy_range to a staged file rebuilt the
whole image through fresh heap. The dirty-node hypothesis (4096 × 256 KiB = 1.0 GiB)
was checked and **overturned**: dirty state is ring-bounded by §4.4 pt 5 admission and
the checkpoint cadence kept up; node-cache bytes stayed within budget throughout.

**Bonus finding (P0, pre-existing, exposed by the aged repro):** the staged
truncate-shrink path could silently fail to clip physical tiers, letting a truncate-up
resurrect dead bytes — an aged-fsx data corruption family. Fixed on this branch
(three legs); the residual zeros-LOSS family reproduced on base e1601bc 3/3 and is
explicitly NOT introduced here (see §5).

## 2. Root cause — evidence

Protocol (PR 7 lineage): fsstress aging (`-n 30000 -p 8` rename/creat/unlink/setxattr/
mkdir/rmdir mix) then an fsx storm (`-S 0 -U -N 10M -p 100000 -o 128000 -l 600000`,
180 s) against a caged release daemon; 1 s `smaps_rollup` Anonymous tracer + `.stats`
`meta_kv_*` correlation; then `--features dhat-on` daemon runs (foreground, exit dump).

dhat (aged daemon, 6 k-file age + 30 k-op fsx storm), BEFORE the fix
(`~/tmp/kvflood/dhat_daemon.log`):

| Site (at global max) | Bytes | Lifetime |
|---|---|---|
| `read_staged` → seed `Vec` (write/punch/copy_file_range callers) | 575 + 109 + 86 MB | ~19.5 GB |
| `Bytes::from(existing_data)` assembly | ~1.4 GB live-max share | ~1.4 GB |
| **Total churn** | **~1.8 GB at gmax** | **53.8 GB total; ~21 GB of 4 MiB-class malloc/free per 30 k ops** |

jemalloc retention of that churn is the cage-kill: anon RSS grows in bursts exactly
when the storm's RMW rate spikes, data I/O frozen (the image rebuild is RAM-only),
journal at op rate (the co-symptom). KV `meta_kv_*` counters stayed nominal
(node cache within `SQUEEZEFS_META_NODE_CACHE_MB`, checkpoints on cadence, zero
`journal_full_stalls`).

AFTER (pooled seeds, same protocol): lifetime churn **53.8 GB → 33.0 GB** (−20.8 GB;
the remainder is the payload-lease/write-path churn that is by-design transient),
and the caged real-rate repro holds **peak anon 3,928 MB** during aging /
**1,354 MB** after the storm in an 8G cage, daemon alive, zero cage kills.

## 3. Mechanism shipped

1. **Pooled staged-RMW seeds** (`1994663`): the whole-image seed lives in a recycled
   `BUFFER_POOL` backing (`PooledBuf`) for all seed sources — ring
   (`read_staged_into`, new), mapping-fallback decode, inline LRU/data_key copies.
   Assembly: inline re-materializes exact-size before RETAINING (a ≤ 4 KiB inline file
   never pins a 4 MiB backing); staged/spill shapes convert zero-copy
   (`PooledBuf::into_bytes`) and recycle on last-drop — transient by construction
   (ring authoritative; both LRUs removed). Counter: `staged_rmw_pooled_seeds`.
2. **Defense in depth** (`5117222`): the KV node cache registers with the PR 7 memory
   authority — gauge = nodes × node_size (+ dirty share observable via
   `node_cache_gauge()`), shed = **never-lossy checkpoint kick**
   (`checkpoint_wake().notify_one()` — forces a checkpoint *tick*, drops no dirty
   state, keeps checkpoint strictly off the commit path). KV commit path itself:
   **untouched** (the evidence exonerated it).
3. **Truncate-shrink integrity** (`5ed0828`, `44949e9` — the P0 found by the repro):
   - `NvmeShard::shrink_staged_value`: in-place 8-byte `original_size` header patch
     under the shard write lock + msync — a logical shrink can never be refused by
     ring pressure (the swallowed-`stage_write`-refusal leg A), and is durable-ordered
     before the KV size commit. `NvmeStaging::shrink_staged` bumps the stage
     generation under the ledger entry lock so an in-flight promotion of the pre-clip
     image fails its commit-time check.
   - `truncate_layout` staged arm rewritten with the promote-commit discipline:
     unlocked ring clip → unlocked durable clip-rewrite (read → clip → write NEW
     block; data I/O before the meta flip; bounded re-resolve vs racing re-promotion)
     for the promoted/ring-miss leg B → single commit under `INODE_META_LOCKS` with
     binding revalidation (publish-then-free). Loud failure — a truncate that cannot
     prove the tail is gone fails the SETATTR.
   - **Size-carrying mappings** `bk:0:packed_len` from promotion/spill/truncate-clip
     (leg C): a bare key forced whole-block reads whose recycled-tenant tail a
     passthrough transform cannot strip — the RMW ring-miss seed ballooned files to
     exactly `block_size` and codified another tenant's stale bytes.
     `parse_block_mapping` is decoration-robust ("://" backends) and reports
     exactness; legacy bare mappings are bounded by `meta.size` on the fallback-seed
     leg only (provably fresh there — promotion/spill/clip persist size in the same
     commit). `free_block`/`increment_refcount` are decoration-tolerant
     (`clean_block_key`) so no block leaks.
   - Deliberately **no** `meta.size` clamp on ring-hit seeds or the ring shrink gate:
     physical stays authoritative when the cached size lags LOW — the pinned
     `truncate_down_stale_size` contract (`tests/hole_read_zeros_tests.rs`) stands.
   - Counters: `staged_truncate_inplace_shrinks`, `staged_truncate_durable_clips`.

## 4. Red tests (all red-first on this branch; scenario tests also red on base e1601bc)

| Test | Pins | Red shape before fix |
|---|---|---|
| `staged_rmw_alloc_tests::staged_rmw_storm_is_pool_backed_recycling_and_byte_exact` | pooled-seed count == ops; pool idle non-bleeding; byte-exact | counter absent / fresh-heap churn |
| `staged_truncate_stale_tests::trunc_cycle_reads_zeros_under_ring_fragmentation` | leg A: refusal-proof shrink | stale bytes at exactly the truncate-down boundary |
| `…::trunc_cycle_reads_zeros_after_promotion` | leg B: durable clip | stale bytes from the promoted image |
| `…::rmw_after_trunc_cycle_stays_clipped_and_sized` | fsx shape: no resurrection, no size regrowth | size regrew to pre-truncate blob length |
| `…::trunc_cycles_under_promotion_churn_never_resurrect` | leg C: 60 truncate/extend/RMW cycles under constant promotion churn | round 1: size ballooned to exactly 4 MiB (block_size) |
| `…::staged_minifsx_under_promotion_churn_byte_exact` | full fsx op mix (punch/zero/copy/truncate), model-exact | pins the §5 residual class at cargo level (green on tip) |
| `…::ring_in_place_shrink_clips_reads_and_refuses_growth` | the shrink primitive contract | n/a (new primitive) |

## 5. The residual zeros-LOSS family — pre-existing, not this branch

The round-4 aged storm (post-fix tip) still miscompares *later* in the storm with a
**different** shape: previously-acked bytes reading **zeros** under the heavy
punch/zero_range/copy/truncate mix (fsx `GOOD nonzero / BAD 0x0000`;
`staged_payload_lost_reads = 0` — codified in-image, not the degrade leg).
**Base lineage:** the identical aged protocol on **e1601bc fails 3/3 rounds with the
same GOOD→zeros shape** (artifacts: `~/tmp/kvflood/basefail/`). On this branch the
storms run 1000+ ops before that class fires; pre-fix they corrupted at op 64/214
with the (now dead) resurrection shape. Verdict: endemic aged-ring class, strictly
older than this branch, out of C's scope — follow-up item with artifacts preserved
(`~/tmp/kvflood/fsxfail{1,2,3}/`, `basefail/`). The QUICK tier (fresh mounts, fixed
seeds) does not reach it; QUICK ×3 acceptance below is unaffected.

## 6. Acceptance evidence

| Gate | Result |
|---|---|
| Aged-daemon repro bounded | peak anon **3,928 MB** (aging) / 1,354 MB (post-storm) in the 8G cage; daemon ALIVE; `staged_rmw_pooled_seeds` 35,493, `staged_truncate_inplace_shrinks` 1,387, `staged_truncate_durable_clips` 287 visible in `.stats` |
| QUICK ×3 | **`{003,213}` only, 3/3 runs** (generic/616 — the historical cage-kill vehicle — passed all three, incl. late soak blocks). The tier-tail cage-kill class is gone. |
| KV/crash cargo suites | kv_journal 17, kv_backend 4 (+89.8 s scale), kv_node 14, kv_tree 32, kv_alloc 21, kv_scale 12, crash_contract 15 (2 root-gated ignored), crash_kill 14 — all green, serial |
| kill9 deep churn | `SQUEEZEFS_CRASH_ROUNDS=60` green — round 59: 859-line ledger, replay 430 entries, 0 dropped-torn beyond design |
| unmount/kill soak | `sudo tests/run_unmount_kill_soak.sh`: **PASS 30 cycles** — 0 coredumps, 0 SIGABRT, 0 panics |
| loom | `tests/run_loom.sh` green — all models pass (belt-and-braces; no loom-modeled core touched — the shard shrink is a parking_lot write-lock patch, the gen bump rides the existing scc ledger entry lock, `gauge_core`/journal ring untouched) |
| LTP | **174 PASS / 0 FAIL / 0 BROKEN / 9 SKIPPED** |
| 1M-dir storm (quiet gate, 3.2 GHz cap) | creates **23,827/s** (lineage band 23.4–26.6 K/s ✓ despite the new frequency cap), matched-shape lookup p50 **4.54 µs vs 3.04 µs small-dir = 1.49×** (≤ 2× ✓), streamed readdir **1.48 M entries/s** ✓, unlinks 18.4 K/s, rmdir clean. Commit latency shape unchanged (creates/unlinks within band). |
| Rows 1–3 (paired tip-vs-base, same sandbox, quiet) | seq write **1,118 vs 1,151 MiB/s**, seq read **5,058 vs 4,685 MiB/s**, rand-4k **148.8k vs 151.9k IOPS** — flat within noise (an initial tip smoke taken while QUICK+storm still ran read low; the quiet rerun is the row) |
| Full cargo gate | on the merge tip: clippy `-D warnings` clean, fmt clean, `cargo test --all-features -- --test-threads=1` green, doc 0 warnings, bench smoke green (see merge commit) |
