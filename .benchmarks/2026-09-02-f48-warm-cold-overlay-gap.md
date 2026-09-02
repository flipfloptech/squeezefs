# Finding 48 — the promotion-first block's acked bytes lost across a clean unmount (2026-09-02)

**Boarded as** "same-mount WARM read ≠ durable (cold) bytes on a freshly
written file" (`.benchmarks/2026-09-02-f46-kvmap-stream-publish.md`
§Boarded item 1). **Re-attributed here: the warm read was right; the
DURABLE image was wrong.** Branch `fix/f48-warm-read-overlay-gap` off the
f46 tip (`591d6e11`): red `5bbcd742`, fix `aa464b7e`.

**Venue:** squeeze-test — 5 storage nodes over nvme-tcp (memory-backed
targets, `cluster_reset_v4` shape), 32-core client, cacheless mount with
`--interception`, 4 MiB blocks. **Instruments:** the f46 A/B job (24 ×
8 GiB, 1 MiB sequential libaio `direct=1` iodepth 16, crc32c headers,
NOT time_based) and `/scratch/tmp/fio_jobs/write_BW.job`; `.stats`
snapshots pre / post-write / post-warm; warm and cold `md5sum` of three
files; a `dd` of block 0 of EVERY file warm and cold with per-file `cmp`;
a REAL crc32c verification (see the evidence note below). Rig:
`.benchmarks/rigs/2026-09-02-f48-field.sh <label> ab|bw` (fresh reset per
run); artifacts `/scratch/tmp/f48-{f46-ab,f48-ab,f48-bw}-20260902-*`.

## Evidence note: the board's "cold verify clean" was a no-op

The f46 board vouched for the cold image with `fio <write job>
--verify_only --do_verify=1` ("196,608 blocks, 0 bad"). On a LOCAL file
with one byte flipped inside a crc32c block that invocation reports
`err= 0`, `issued rwts: total=0,8,0,0`, and an EMPTY run-status block —
in both option orders. It verifies nothing. A READ job over the same
files with `verify=crc32c do_verify=1` fails the corrupted block
(`err=84 … verify failed at offset 1048576`). The field rig uses the
latter.

## Root cause (code anchors)

1. **The promotion arm publishes a PARTIAL image under a bare key.** A
   fresh file's first beyond-inline write on a cacheless mount rides
   `DataRouter::write_file` → the sparse striped promotion
   (`src/routing.rs` "Transition layout → striped, SPARSELY"):
   `durable_write_sparse_blocks` writes the 1 MiB chunk at a fresh
   offset and binds block 0 to `persist_block_key(be, off)` — one
   segment's bytes, the rest of the 4 MiB slot never written.
   Fingerprint: `write_lock_scope_entire` = `layout_striped_writes` = 24.
2. **The block's other segments install an OVERWRITE-shape overlay
   record with a permanent gap.** `try_device_overlay_store`
   (`src/fuse_client.rs`): `overwrite_shape = bm.contains_key(&b)` →
   record with `old_binding` = the promotion key; coverage [1M, 4M) can
   never reach the whole block. `overlay_overwrite_installs` 24,
   `overlay_open` 24 after fio closed every file.
3. **RELEASE never drained overlays.** `release` (`:24376`) backgrounds
   `flush_memory_buffers_for_inode` / `flush_active_blocks_with_retry` /
   `persist_dirty_layout_if_needed` for a dirty handle and nothing else;
   the §6.2 boundary set (`docs/design-device-overlay.md` "fsync (and
   every durability boundary: flush legs, RELEASE-last-close, unmount
   drain)") was stated, not built. The record outlives its writer.
4. **The read drain feeds an epoch nobody owns.** The first read crossing
   the block boundary composes nothing (`try_serve_overlay_read` →
   `None` on a multi-block span) and drains
   (`drain_device_overlays_range` → `settle_overlay_block_locked`):
   gap seeded from the old key (`read_nvme_block_old_image` — correct),
   `publish_block`, then the §5.4 arm (a) feed `rewrite_shadow_record`
   → the RAM map binds block 0 to the dest, `layout_dirty = true`,
   `Ok(coverage_complete = false)` → no close. The reader's handle is
   clean (`fuse_release_clean_fastpath`), the writer already closed,
   and `close_rewrite_epoch` fires only on fsync, KD-1.6 coverage
   completion, or the idle sweeper (`EPOCH_IDLE_HORIZON_MS` = 30 s).
5. **The dismount teardown fed 22 more epochs and closed none.**
   `run_dismount_teardown` drains every remaining record
   (`drain_device_overlay_block(ino, b, true)` → the same feed) and
   proceeds to `vol.shutdown()`; no site closed open epochs. The field
   log of the control run shows the sweeper losing the race: `rewrite
   epoch close for ino 26 failed transiently (meta volume /dev/nvme8n1:
   volume is shutting down) — the epoch stays registered` — and then
   the process exits. The durable map keeps naming the promotion key,
   so the cold read serves one segment plus never-written device bytes
   (zeros on a fresh target). Acked, closed bytes lost on a CLEAN
   unmount — the finding-44 corpse shape.

Why the A/B saw "first file identical, second file differs" in BOTH
runs: file 0's read-fed epoch had ~30 s (the md5 of the next file) to
reach the sweeper (`rewrite_shadow_swaps` +1 inside the md5 window);
file 1's was still RAM-only when umount ran.

## The fix (law level, no read-path change)

* **RELEASE settles the ino's Open records first** — the dirty-handle
  background task runs `drain_device_overlays_for_ino(ino, u64::MAX,
  false)` ahead of the memory-buffer flush (the `flush_inode_to_backend`
  order; no data barrier — write-back class like the rest of that task),
  so its existing `persist_dirty_layout_if_needed` carries the fed
  binding and `overlay_open` → 0 at quiesce (the §9.1 law).
* **The dismount teardown closes every open rewrite epoch**
  (`DataRouter::close_open_rewrite_epochs`) after the staged sweep and
  its drain wait, behind a DUR-1 `flush_data_devices` barrier and before
  `reclaim_drain` (the closes' displaced frees are what it must return)
  — a clean unmount is a durability boundary for RAM-only shadow
  bindings whatever fed them (a read drain, the unmount drain, an
  un-fsynced rewrite inside the idle horizon).

The ACK-early A-leg, the compose (`try_serve_overlay_read`) and the
settle are untouched.

## In-process red/green (`tests/f48_warm_read_overlay_gap_tests.rs`)

FUSE-level cacheless fixture (64 KiB blocks, quarter-block segments,
overlay ON + ACK-early ON, patch cap 0 — the field's oversize shape):
promotion-arm first segment (asserted by `write_lock_scope_entire`) +
overlay remainder (asserted by `overlay_overwrite_installs` = 1), then
close / no close, kernel-shaped warm reads (1/16-block windows, two in
flight; the boundary-crossing window drains), the product's DESTROY,
remount, cold read. Three legs (sequential, concurrent segments, handle
open at unmount):

| leg | tip `591d6e11` | fix `aa464b7e` |
|---|---|---|
| warm windowed / warm whole | == written | == written |
| cold after clean unmount | **block 0 segment 1: 16384/16384 bytes zero** | == written |
| `overlay_open` after close | 1 (5 s poll never reached 0) | 0 |

The f44 venue (`tests/f44_overlay_rewrite_tests.rs`, both overlay legs)
now asserts warm == cold == acknowledged on every pass — a warm/cold
disagreement is never a legal outcome.

## Field rows (fresh reset per run; medians unnecessary — the verdicts are byte-exact)

| row | binary | fio write | `overlay_open` after close → after warm md5 | epochs after warm | warm md5 == cold md5 (3 files) | block-0 warm-vs-cold diffs | cold crc32c READ-verify |
|---|---|---|---|---|---|---|---|
| ab (24 × 8 GiB, crc32c) | control `08130de3` (f46) | 15.58 GiB/s, clat 22.0 ms | 24 → 21 | 2 open, 1 swapped | **MISMATCH** (2 of 3) | **23 of 24** — every one: first diff at byte 1048577, [1M, 4M) = 3,145,728 zero bytes cold | **err=84** (verify failed, aborted at 8.6 GiB read) |
| ab (24 × 8 GiB, crc32c) | fix `aa464b7e` | 15.82 GiB/s, clat 21.7 ms | 0 → 0 (24 gap seeds + 24 feeds at close) | 24 → 0 (24 swaps in the md5 window) | IDENTICAL | 0 of 24 | **exit 0**, 0 failures over 192 GiB |
| `write_BW.job` (time_based 30 s + 10 s ramp) | fix `aa464b7e` | **32.82 GiB/s**, clat 11.2 ms (f46's row: 32.7) | 0 → 0 | 24 → 0 | IDENTICAL | 0 of 24 | — |

Tripwires 0 on every row: `invariant_tripwires`, `fuse_op_watchdog_overdue`,
`meta_kv_block_refs_drift`, `writeback_errors_latched`,
`overlay_fence_drops`, `overlay_unpublished_at_fsync`, `fsck_findings`.
The control's `overlay_open` = 24 after close is the field face of the
missing release drain; the fix's `overlay_gap_seeds` = 24 with
`overlay_gap_seed_old_bytes` = 24 × 1 MiB is the release drain seeding
exactly the promotion quarter of every file.

## Interaction with f46

None on the mechanism: f46 changed the kvmap publish economy
(`MapTrainClaims::window`); this finding is the overlay record's
lifecycle across close/unmount and reproduces identically on the pre-f46
baseline. The branch is built on the f46 tip and every f46 row above
carries its fix (32.8 GiB/s on `write_BW.job`). One observation for the
f46 owner: `f46_kvmap_stream_publish_tests::
a_streaming_extend_on_a_kvmap_ino_publishes_in_o_window_tree_reads`
failed its `window_saves == publishes` equality once in four runs here
— the one run that overlapped a concurrent `task build:rocky8` — and
passed 3/3 alone. Load-dependent accounting equality, not touched by
this fix (that suite calls neither release nor destroy).

## Boarded (NOT fixed here)

1. The idle sweeper's 30 s horizon is still the crash-exposure window
   for a RAM-only epoch fed by a READ drain on a file whose writer is
   still open and never fsyncs — legal (un-fsynced acked writes carry
   no crash guarantee), but a read-triggered feed could close its epoch
   directly instead of leaving the durability to a timer.
2. The promotion arm binds a partial block to a bare key whose slot
   tail is never-written device content; every consumer (the compose,
   the gap seed, the escalation base) tolerates it by construction
   today, and the eventual publish always covers the whole block, but
   the shape is what made this finding's loss read as zeros rather than
   fail loud.
