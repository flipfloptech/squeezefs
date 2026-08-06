# 2026-08-06 — fsync-vs-writeback tail loss (P0 data loss): the proven interleave and the fencing-convergence fix

Hand-off: `.benchmarks/2026-08-06-fuse-zc-serve.md` §5b (the zc rig's FATAL
write→readback md5 smoke). Branch `fix/fsync-writeback-tail-loss`; red suite
`tests/fsync_writeback_tail_loss_tests.rs` (red at `8df69c7b`, green at the
fix commit).

## 1. The bug as characterized

`cp <32 MiB file> mnt/f && sync mnt/f` (buffered writes + `fsync(2)` ON THE
FILE, racing in-flight kernel writeback) → the file's LAST 4 MiB block reads
back **all-zeros, persistently** (the STORED state — buffered and O_DIRECT
re-reads agree after a cache drop). Field rates: 5/10 corrupt on base
`50ad803d`; zc-independent. Every upstream tripwire silent.

## 2. Local reproduction (devsub loop substrate, this campaign)

Venue: `tests/dev_substrate.sh` loop substrate (4×nullb mds + 4×zram oss),
release build of the branch base, instrument = the exact field shape scripted
(cp + `sync <file>` + buffered/O_DIRECT md5 + per-trial stats-inode deltas).

* **Pre-fix: 9/10 corrupt** (first trial = fresh create, clean; every
  overwrite trial corrupt). One whole 4 MiB block all-zeros per corrupt
  trial — block index varies (5/6/7 observed; the LAST block whose detached
  upload was still in flight at close).
* Persistence proven: same wrong md5 across repeated buffered+direct reads
  and after `echo 3 > drop_caches` — the stored state.
* **Per-trial corrupt signature** (stats-inode deltas):
  `write_through_blocks +6..7` (one block of 8 NEVER published),
  `write_pipeline_supersessions +1..2`, everything else silent —
  indistinguishable from the benign flush-wins race, which is why nothing
  upstream ever noticed. (The field note's `overwrite_seed_materialized +1`
  is a co-traveler of its venue's partial-coverage timing, not the loss
  mechanism: local corrupt trials show it at 0.)

## 3. The proven interleave (SQZ_TAIL_TRACE tape, corrupt trial, ino 2 block 7)

Temporary park/merge/flush/retire/publish/supersession tracing (uncommitted)
produced this tape for every corrupt trial:

```
merge  b=7 rel=[3932160,4194304) completed=true epoch=282   ← union complete; detached task admitted; WRITE ACKs
flush-decision key=block_7 deferred=false complete=true driver=false
retire key=block_7 removed=true epoch=282
       site=[flush_memory_buffers_driven ← flush_memory_buffers_for_inode ← release::{{closure}}]
       ← NO PUBLISH LINE FOR b=7 ANYWHERE — custody retired unpublished
supersession ino=2 b=7 epoch=282 current=None offset=0      ← detached task: "flush leg won", frees orphan, returns
```

Every healthy block shows `publish b=N new_key=…` before its retire; the
victim never publishes. The chain:

1. Kernel writeback delivers the tail block's WRITEs; the last merge
   completes the coverage union; the **detached pipeline upload** is
   admitted; the WRITE ACKs with custody parked (writeback-cache law).
2. `close(2)` → RELEASE captures the write-era fencing token, spawns the
   **background flush** with it, then — last close — releases the cached
   lease. (FUSE_FLUSH is elided connection-wide after the first
   clean-handle ENOSYS latch, so the background flush is the only
   close-time flusher.)
3. `sync <file>` → FSYNC → `acquire_write_lease` mints a NEW lease: the
   ino's fencing generation advances. **A process-local rotation.**
4. The background flush reaches the still-parked tail block first (the
   detached task is pre-DMA in its unlocked window), drives the
   write-through leg with the now-stale captured token, and the publish
   merge refuses `FencingTokenExpired` — whereupon the leg **RETIRED the
   parked overlay and published NOTHING** ("a fenced writer must not
   publish anywhere").
5. The detached task revalidates under the block lock, finds
   `current=None`, and per its contract assumes *"the surviving generation
   is durable (flush leg)"* — frees its orphan and returns. The ONLY copy
   of the acked bytes is gone. The block map never names the block; the
   layout persists size = 32 MiB; reads = hole = zeros.

## 4. Root cause and the fix's mechanism

**Root cause:** the flush write-through legs treated `FencingTokenExpired`
as the writer-era fence and dropped live parked custody. Within one
process that error class can ONLY be lease churn in this same daemon (the
merge compares the presented token against the process-local generation);
the genuine cross-mount fence is the D0 latch, which surfaces as
`WriterGuardFenced` from `authorize_dma` BEFORE any merge runs. The staged
ladder already learned exactly this lesson (FIND-M11-A / incident_013 —
`flush_one_active_block`'s doc: *"the MERGE presents the ino's current DLM
generation, read fresh per attempt… a racing bump surfaces as a TRANSIENT
FencingTokenExpired that the retry ladder converges"*), and the WRITE
handler carries the same law as FIND-RW5-A face 3. The RAM-custody flush
legs never got it.

**Fix (fix commit on this branch):** one convergence law across staged and
RAM custody —

* `SqueezefsFilesystem::fencing_retry_token(ino, presented)`: re-read the
  current generation; advancement (monotone — a failed merge proves the
  generation moved past the presentation) re-presents and retries, counted
  on `writeback_stale_token_retries`; non-advancement (structurally
  unreachable for the process-local class) keeps custody **PARKED** and
  propagates loud. Custody is never retired on this error class.
* `flush_memory_buffers_driven` write-through leg: presents a FRESH-read
  token (never the caller's captured one) + the convergence loop.
* `flush_memory_buffers_driven` durable-escalation leg,
  `write_through_complete_block` (pipeline/serialized/inline unit), and
  the teardown leg in `flush_all_memory_buffers_to_staging`: the same
  convergence; the teardown non-progress arm leaves the buffer parked
  until process end instead of retiring it unpublished.
* `pipeline_disposition`: `FencingTokenExpired` → `StayParked` (an
  escaping expiry kept custody parked); `WriterGuardFenced` remains the
  `FenceDrop` class — the W5/remount law is untouched, as are the loud
  merge-primitive stale-token rejection and staging recovery's
  prior-generation discard ("stale fencing tokens discard staged work"
  stays the REMOUNT contract).

Superseded pins updated with the fix: `write_pipeline_tests` T2 (the
custody-drop pin WAS the bug's contract) now pins converge-and-publish;
the disposition mapping test gains the `WriterGuardFenced` row.

## 5. Local verification

* Red first: all 3 cases of `tests/fsync_writeback_tail_loss_tests.rs`
  FAIL at `8df69c7b` with the exact loss signatures
  (`Err(FencingTokenExpired)` + unpublished block + zero readback; the
  end-to-end release→fsync loop zeroes a tail block within 20 rounds).
* Green ×10 after the fix: `fsync_writeback_tail_loss_tests` +
  `write_through_coverage_tests` + `fuse_zc_serve_tests` +
  `read_dest_lease_tests`, ten consecutive serial passes, all green.
* Neighbors green: `write_pipeline_tests` (22), `write_through_tests`
  (26), `writeback_fencing_livelock_tests` (5), `rw5a_never_lossy_tests`
  (10). Both clippy configs `-D warnings` clean; `cargo fmt --check`
  clean.
* Live devsub, exact field shape, fixed binary: **0/20 corrupt** (pre-fix
  9/10), `write_through_blocks +8/8` per trial (every block now
  publishes), `overwrite_seed_materialized` delta 0 on every overwrite
  trial, `write_pipeline_supersessions +1` per trial = the now-benign
  flush-wins race.

## 6. Field verification (squeeze-test — memp-s3ds-aqs-37, kernel 6.19.14-sqz)

Deploy: `go-task build:rocky8` at the fix commit `fabeebf0` →
`dist/rocky8/{squeezefs, libsqueezefs_il.so}` rsync'd (temp+rename, never
`--inplace`), md5-verified (`1a5994b1…` / `59bbb758…`), old daemon drained
(`umount` + pid wait), remounted with the standing command
(`--daemon --interception --allow-other`).

* **Acceptance, the exact corrupting shape** (`cp <32 MiB urandom> mnt/f &&
  sync mnt/f` → buffered md5 + O_DIRECT md5, 20 trials, one file
  overwritten in place — trial 1 fresh-create, 2–20 the corrupting
  overwrite class): **20/20 clean** (base `50ad803d` was 5/10 corrupt on
  this host — a 10-trial-clean would have been weak evidence at that
  rate; 20/20 puts the per-trial corruption probability < 14 % at 95 %
  confidence vs the observed ~50 %). `overwrite_seed_materialized` delta
  **0** on every overwrite trial (the +1 on trial 1 is the fresh-create
  partial-tail seed — the benign class); `write_through_blocks` delta
  **8/8** on every overwrite trial (every block publishes; pre-fix corrupt
  trials showed 6–7).
* **Throughput row (no regression):** fresh ARMED mount
  (`SQUEEZEFS_FUSE_ZC=1`, `fuse3_zc_negotiated=1`), the zc campaign's
  headline recipe verbatim (fio libaio direct=1 rw=read bs=1M iodepth=8
  numjobs=16 nrfiles=8 size=8g time_based 60 s ramp 10 s over exa_perf):
  **39.52 GB/s sustained** (io=2372 GB, clat mean 3.38 ms) vs the
  campaign's armed 39.84/39.92 GB/s A-B-B-A (within the venue's ~1 %
  run-to-run band; control posture ≈ 28). zc engagement exact:
  `read_zc_serve_bytes` 0 → 2.76 TB across the row.
* Host restored to the standing posture (the specified mount command, no
  zc env) on the fixed binary, mountpoint + `.stats` gated.

## 7. Honest notes

* The corrupt-trial counter signature is invisible by construction
  (`write_pipeline_supersessions` fires on benign flush-wins races too).
  The convergence retry is observable on `writeback_stale_token_retries`;
  a future tripwire distinguishing retire-without-publish would need a
  publish-ledger closure check (not built here — out of scope).
* `write(2)`'s own entry-check `FencingTokenExpired` retry stays the
  FIND-RW5-A one-fresh-lease ladder (bounded, user-facing) — deliberately
  not unified with the flush legs' unbounded-but-monotone convergence
  (a flush holds the only copy; a write handler can surface EIO and the
  application retries).
