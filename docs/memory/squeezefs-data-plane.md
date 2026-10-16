# SqueezeFS-Zig data plane, format limits, and lock order
The rung-4/5 data plane under `src/data/`, on-disk width and per-file limits, allocator memory cost, lock-order law, mount teardown order, rung-9 amplification bounds (write, read, metadata journal), transform decode, memory components, and measured large-file numbers.

## Modules (src/data/)
- `layout` (v4 32 B `LayoutHeaderWire`, ONE Delta kind; `MapEntryWire` is 48 B), `staging` (checksummed SQZS ring; segment record v3 since format v5, two header slots and value-first streaming checksum), `pack`, `router`, `block_alloc` (ONE dense cell table + free bitmap), `block_refs`, `reclaim` (io_uring discards, no ioctl), `enospc`, `transform` (first-party LZ4 block codec; vendored zstd 1.5.7 via build.zig `addTranslateC`), `set_config`, `data_superblock` (v2 since format v5).
- Write side: `write_*` modules — admission ladder A→L, `device_overlay`, `sole_owner_patch` (the W1 sole-owner extent patch arm; renamed from `patch_w1.zig` in afbbb22), `accumulate`, `extent_overlay` (W2), `write_pipeline`, `writeback`, `fsync`. `write_side_view.zig` is THE read seam.
- Read side: `read_*` modules — `hot_tier`, `read_lru`, `read_hold`, `nvme_read_cache`, `purge` (comptime census of all four tiers), `single_flight_fill`, `prefetch`, `admission`, `warm_ladder`.
- `plane.zig`: `DataPlane.open/close` mount wiring; `plane.ownBlock` / `publishLayout` must agree on grain; `plane.republishBinding` is the ONE block mover (rung 8); `plane.deviceRead(vol, off, out, purpose)` is the ONE device-read choke point (rung 9). `src/fuse/handler/kv_fs.zig` is rewired through the plane; `kv_fs.readLayout` must use the token for foreign inos; `kv_fs.writeInline` publishes the whole image per write(2) (a kill can lose a whole transaction, never truncate one).
- Rung 5 added the block-map tree (kvmap crossing) for PB-class files, D0 guard B1 (NVMe PR) / B2 (writer_claim + heartbeats), `.stats` inode, dynamic routing, multi-volume mounts, `migrate-meta-slot`.
- Rung 6 added the symmetric plane: appender region, manager + extent grants, slot leases + commit door, tokens, cross-owner intents, record ship, directory striping, custody, recovery, join ladder, N-daemon posture, `-o ro` reader.

## Write path arms and amplification bounds (rung 9, 0003 §J)
- 512 B writes are below the 4 KiB W1 patch grain, so they are patch-ineligible and go to the W2 extent overlay.
- W2 fold is page-indexed with a RAM trigger (`pages ≥ B/4P`); `FOLD_MAX_EXTENTS` is retired; seed is off the write path (byte masks); fold is detached. Bounds: in-place `pages×P/e` (8× at 512 B), hole block `B/u_flush`, CoW base `(B+u)/u`.
- Staged append happens in place with slack `max(P, V/4)` → moved ≤ 11× appended, ≤ 1.05 journal entries/append. Budget component `parked_extents`.
- Harness verdicts in `brk.py` and `bench.zig` (rail-pinned equal): `write_amplification_bound`, `staged_append_bound`, `journal_entries_per_append`.
- Measured A/B: frag-512b-write 130× → 7.06× at 1.9× IOPS; append-4b 3.0× appends/s; generic/069 3 h → 521 s; generic/074 OOM → 319 s at 2.13× with flat RSS.

## Parked write stamps (rung 9, 0003 §L.6, §J.2 amended; defect 27; landed abba028, 997960a, 7e6a481)
- Law: a size-neutral write(2) produces ZERO journal entries. Before the fix every 512 B write = one FUSE_WRITE = one Δ(size,mtime,ctime) entry (76 B) + 2,536 B pad (n̄ = 1.56 entries/window) + meta_io 110 B = 2,738 B/op = 5.35× META on frag-512b-write (psync QD1 × 4, not QD32). With one entry per write a closed loop of c committers caps journal B/op ∈ [g/c, g] whatever the pacing — so never accept 'at its floor' without the arithmetic closing to the measured number.
- Fix: a size-neutral striped write parks its stamp under the ino guard (`PendingTimes`, fold-visible), tx empty; batched drain (≤ 16 inos/entry, `retireIfStill`) on the flush cadence (`TimesDrainCadence`), the 512 cap, `fsync(ino)`, `fsyncdir`, dismount; extending writes commit as before; strict mode drains per write.
- Owner design notes: a write's stamp never regresses a future utimes mtime (signed-max fold); the slot-lease handover hook drains nothing — parked stamps are dropped and counted at the door refusal (owed: handover draining parked stamps).
- A/B: frag-512b META÷user 5.9 → 0.019, IOPS 7.8k → 12k, p50 276 → 116 µs; rand-4k-write META 0.59 → 0.0008, IOPS 11k → 24.5k, p50 8.7 → 2.6 ms; fsync-4k META 0.96 → 0.128. Docs: charter §5.2, 0002 rows 481–482.

## Read path bounds and transform decode (rung 9, 0003 §K)
- Every READ-side DMA byte is counted in a purpose arm (ranged/demand, prefetch, lane block/image/window, write-side, compose, pack); `device_read ÷ user_read` per shape has a derived bound. nvmet namespace diskstats count reads even when the backing file is page-cached.
- Transform: `transform.Transform` (immutable) + `transform.Codec` (per-lane static `ZSTD_CCtx`/`ZSTD_DCtx`, ≈ 96 KiB each) in a `CodecPool` of `max(lanes, 1)` plus a `ScratchPool` of `clamp(cpus/4, 4, 16)` held THROUGH the DMA. `transform_codec_pool_waits` is 0 by derivation. Every transform ≈ 19–21k IOPS = 0.21–0.28× passthrough (74k) on the venue probe. Harness rows `rand-4k-read-{zstd,lz4,aes}`.
- Framed transform image (v3 since format v5): 96 B header ‖ compact table ‖ 64 KiB frames each with own nonce/tag; `features_incompat` bit 0 `feature_framed_images`.
- Inode shape is the filesystem's alone: `noteShape` is the sole shape publisher (defect 25).

## Metadata journal pad (rung 9, 0003 §L)
- `conveyor.windowCost`: one 76 B entry per 4 KiB page = 97.6 % pad; 53 entries fill a page. A pad-majority window waits until the flight is empty (bounded by RFC 6298 `srtt + 4·rttvar`, capped at the flush interval; a lone committer never waits; strict cadence exempt), gated by `WorthGate` (opens at ½ entry/hold, closes at ¼). Model 8 `HoldPark` in `conveyor_core_model.zig`.
- Exact-sum META ledger = `journal_lane_write_bytes + meta_io_write_bytes` (matches diskstats ×1.0000). Tail-page rewrite was rejected; remaining lever is 512 B LBAs on metadata namespaces.

## Memory components (R5)
- Staged cell streaming `Hasher` bounded to 1,056 B, gauged as `staging_cell_state` (d390207; details in the sqzhash128 topic).
- Rung 10 (69ac3f6): `RecordIndex` index arrays charged to `kv_node_cache` (node index residual 1.4 %, was 14.1 % uncharged; 352f7f2); `Segment` per-cell tables gauged as component `staging_tables` (≈ 5.5 % of ring; 31.5 MB at a 546 MiB ring; 1d91625); fabric sampler renders with ONE allocation (8b0df62).

## Format and width limits
- `src/data/data_superblock.zig`: `volume_len` u64, `block_size`/`chunk_size` u32, `vol_tag` u64.
- `src/data/set_config.zig`: `n_data` u16 → ≤ 65,535 data volumes.
- `src/data/block_alloc.zig`: `capacity_blocks` u64; `Cell` asserted `@sizeOf == 24`; RAM = cells.len×24 + (bits.len + debt_bits.len)×8 — practical volume size is bounded by host RAM (1 PiB at 4 MiB blocks ≈ 268M blocks ≈ 6.4 GB cell table).
- `--block-size` must be a power of two in [`transform.MIN_BLOCK_SIZE`, `transform.MAX_BLOCK_SIZE`] (4 KiB–4 MiB); `--meta-node-kib` ∈ {64,128,256,512,1024}; meta journal ring = clamp(volume_len/64, 8 MiB, 32 MiB).
- Record value cap = min(65,536, node_size/4) (`RECORD_VALUE_CAP_CEILING = 65_536`, `src/meta/record.zig`).
- Inode routing width W = 2^16 (u16 slot-id); native local inos < 2^40 (`src/meta/inos.zig`).
- Per-file size: since the block-map tree landed, EFBIG is at u32::MAX blocks (inline map held 1,364 × 48 B entries at the 256 KiB node).
- Data strips across data volumes (a 1.5 GiB file → 781M + 773M on two volumes).

## Lock order
- The plane's per-ino layout lock L is level (3.7), between (3.5) meta stripe and (4a). `layout.PublishLocks.lockPromotion` is the ONE promotion order. kv_fs truncate/corpse-mark take L before their 4a set.
- Debug-only lock-level rail `src/sync/lock_order.zig` trips `lock_order_inversions` (must-stay-0).
- Release-wedge symptom (N `release` ops of one ino parked past `handler_entry`, `lock_ticked_reregisters` climbing) was the overlay settle sleeping under the (3) block guard; fixed in 85fe283.
- Diagnosis path: daemon watchdog log (`FUSE op watchdog … parked past station [handler_entry]`), the `wedge census` line, `dmesg` for device WRITE I/O errors, `SIGUSR1` stats dump. A wedge only on a merged tree can be an environment artifact (concurrent gate runs filling the disk).

## Mount teardown order (d5c780b)
- `DataPlane.dismount` is its own phase and runs BEFORE the wire stops (otherwise the holder's grant source is nulled before the promotion pass and `block_alloc.nextFreshBlock` hit a SET bit on a clean `umount`).
- `nextFreshBlock` on a SET bit restores the cursor, fires `block_claim_anomalies` and returns typed `PlaneRefused` — never an assert.
- Pinned by the `sym_planes` two-writer fixture and the `FSCK AND SCRUB` smoke row (`200 promoted, 0 failed`).

## Measurements (venue, virtual disk)
- 1.5 GiB file round-trips byte-exact across a remount at ~900 MB/s write+fsync, ~960 MB/s first-touch read (Debug build).
- ReleaseSafe scoping rows: rand-4k-read 47k, rand-4k-write 12.5k (24.5k after parked stamps), seq-read 18 GB/s, seq-write 1.7 GB/s, fsync mean 277 µs, shim libaio 24.7k.
- Block-map smoke stage uses `--block-size 1048576` with a 1.5 GiB file.
- Through virtiofs on /build/ext: 256 MiB write+fsync ~39 MB/s (USB bound).

## Recent state
- The user's question "what is the current maximum filesystem size squeezefs-zig can create given system constraints" was never answered; the limits above are the gathered inputs.