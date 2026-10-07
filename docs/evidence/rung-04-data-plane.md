# Rung 4 (the data plane) — files of any size through a real mount

Status: landed 2026-10-07 on `dev` (four waves: `a66d04c` layouts/staging/packing/router + allocator/
durable refs/reclaim + transforms/set config/data superblock; `3797b08` the write path in admission
order and the read path with its tiers; this commit the plane bound to the mount, the zero-copy legs
end to end, the big-file smoke, the storm and the kill matrix). Specs: `docs/porting/data_*.md`;
decisions 0001–0003; 0002 ledger rows 95–165 (the waves), **166–174 (this commit)** plus the three
rows below. The executor and FUSE notes are `rung-03-*.md`.

## What landed in this commit

| Layer | Modules (tests) | Contents |
|---|---|---|
| plane | `data/plane.zig` (5; Linux) | `DataPlane.open`: per data volume an `nvme_dev.Device` with its identity re-read through the opened device, a `block_alloc` allocator sized from the capacity and SEEDED from the durable reference ledger, the router's placement table, the reclaimer (lanes on the exec pools, every volume a target), `block_refs.RefStore` over the backend with the `RefsSeam` installed into `layout.publishLayout` (the `+ref`/`−ref` records ride the layout's own transaction), the staging ring and the NVMe read tier under the FORMAT-declared cache paths (none ⇒ both OFF, said loud: inline ≤ `inline_max`, everything larger striped), `CryptoState` from the mount key, `WritePath` + `ReadPath` + `WarmLadder` + `fsync` wired through `write_seams`/`write_side_view`/`purge`; the budget components registered; `close()` = the dismount pass (promote every staged file → seal packs DISMOUNT → drain the reclaimer → close devices) before the backend's final checkpoint; `writeFamiliesJson` = every data family under one object |
| filesystem | `fuse/handler/kv_fs.zig` (41), `handler.zig` | `KvFs(Store, Data)`: `read/write/fsync/flush/release/setattr(truncate)/create/open/unlink` route through the plane. The write routing: inline ≤ `inline_max` on an inline file; staged ≤ one block while the set stages; else striped — a resident payload promoted to its own block DURABLY first (`DmaProof`-minting, so the type flip never precedes its bytes), then ONE admission per piece, then the inode's size claim (SIZE NEVER LEADS DATA). Truncate per layout (inline clip; staged re-stage / empty-inline at 0; packed passthrough clip = a mapping clip, transformed re-stage; striped map clip with the kept partial block zero-completed through the write path). `unlink` → the destroy rider: inode + xattr Deletes + every binding's `−ref` in ONE entry, the blocks to the reclaimer after the commit; `statfs` from the allocators; the KILLPRIV_V2 law unchanged; `fsync` → `fsync.zig`'s legs. The rung-3 inline-only EFBIG stopgap is DELETED (row 166): the honest refusals left are a set with NO data volumes and the inline block-map record cap (`fuse_write_beyond_inline_map_refused`, the owed `kvmap` crossing). **New: the corpse sweep at mount** (`sweepCorpsesAtMount`) |
| transport binding | `fuse/transport/{filesystem_vtable,worker,session}.zig`, `data/warm_ladder.zig` | `probe_read` → `WarmLadder.probeRead` answering `ProbeReply = reply \| fd_source{fd, off, len, done}` — the transport bridges `READ_FIXED(fd → slot)` and runs `done` at the CQE (row 168); `direct_leg` → `WarmLadder.directLegFor` ARMED with `CompletionHook`: the post-DMA incarnation + binding recheck; a failed recheck withdraws the prefilled reply and the slot takes the ordinary dispatch (`transport_zc_direct_leg_recheck_withdrawn`, row 167); `write_hold_eligible` → `plane.writeHoldEligible` = `patch_w1.eligibleShape` over the lock-free shape table, a HELD WRITE reaching `WritePath` as `Payload.slot` and stored `WRITE_FIXED(device ← slot)` through the session's `storeHeld` face (`held_stores`, row 169) |
| CLI | `cli/{mount,status,smoke}.zig` | `mount`: the plane opened between the backend and the filesystem, armed with the session's legs after `Session.mount`, closed after the sampler and before the backend (row 173); `stats data:` in the SIGUSR1 dump; `status`: the reference census per data volume (`refs vol-…: N reference(s) over M block(s)`); the smoke below |

Owed bindings closed: arm H/I (staging ↔ write path: `releaseStaged`/`stageImage`/the staged
sibling supersede through the plane's ring face), the prefetch issuer as a lane fiber
(`ReadPath.issuer` → `exec.spawnGroupOn(homeLane)`, `prefetch_fibers`), the hold-serve ceremony on
the probe path (`WarmLadder.Ceremony` → a lane fiber running `ReadPath.holdCeremony`),
`layout.MetaStripes` as a PARKING `sync.Mutex` (row 171 — the spin would hang at the first
same-stripe collision because the bound merge commits inside the section). Left as recorded: the NVMe
tier publish as a memcpy (Q10 `WRITE_FIXED` into the tier file — the fill pool's memfd would be the
source; not cheap, owed).

## The smoke (`zig build smoke`, part of the gate; venue 7.2.6-sqz, root, 8 vCPU aarch64, debug build)

Three new tests beside rung 3's four (`cli/smoke.zig`): a SET of two sparse 2 GiB data volumes
(510 usable 4 MiB blocks each) + the 256 MiB meta volume, formatted WITH a cache path (staging + the
NVMe tier ON) and WITHOUT (both OFF), every stage `fsync`'d, a 256 MiB urandom image on ext4 as the
oracle every stage slices and a mirror the mutations are replayed on. Numbers from the gate run
(throughput is scoping only — a debug build on a virtiofs-backed file volume):

| Stage | with cache path (staging ON) | without (striped only) |
|---|---|---|
| `dd` 200 MiB (1 MiB writes) + fsync; sha256 round trip | **793–899 MB/s**; image-exact | **985–1073 MB/s**; image-exact |
| `cp` 100 MiB + fsync + `cmp` (128 KiB writes) | 177–203 MB/s | 402–457 MB/s |
| O_DIRECT read back (1 MiB) cold / third pass | 2.5–3.4 GB/s / 2.7 GB/s | 2.5–3.5 GB/s / 3.0 GB/s |
| `truncate -s 150M` → `220M` + `stat`; 64 aligned + 16 unaligned 4 KiB overwrites | image-exact (`cmp` against the mirror, sha recorded) | image-exact |
| 64 KiB files | 1000 staged: 49–51 MB/s → after dismount **1000 references over 16 pack blocks** (bound ceil(1000×64 KiB/2 MiB)+2 = 34; 15.6 of 4 MiB) | 100 striped (one block each): 28–33 MB/s → 100 references over 100 blocks |
| 8 × 50 MiB `cp` storm + fsync each + `cmp` | 226–318 MB/s aggregate | 684–896 MB/s aggregate |
| after the storm (quiescent) | 14 529 fuse ops, 18 048 ring requests, **4.13–4.16 enters/request**, 0 bounce copies; READ 741 MB user: 88 MB dest memcpy, **653 MB dest DMA, 559 MB zc serve over 3 519 direct legs**; WRITE: **51 held stores / 64 hold admits / 51 W1 patches**, NT copy 485 MB of 625 MB write-through, 10 own-block DMAs, 194–196 map merges | 10 025 ops, 13 544 requests, 4.36–4.39 enters/request; the same read figures; 51/64/51 W1; NT copy 523 MB of 1 082 MB write-through, 0 own-block DMAs, 304–309 map merges |
| `rm` all → `df` at the baseline | 0 KiB used; 1001 destroys, 16 terminal frees | 0 KiB used; 101 destroys, 100 terminal frees |
| `status` after | `summary: v4, 256 MiB, checkpoint seq 17, 1011 inode(s) minted, not mounted`; refs census 0 blocks | `checkpoint seq 10, 111 inode(s)`; 0 blocks |

Asserted on every quiescent stats dump: `transport_requests_abandoned`, `open_count_stranded`, the
transport park ticks, `exec_lane_tick_rescues`, `invariant_tripwires`, `patch_edge_rmw_reads`,
`overlay_fence_drops`, `data_unresolved_volume`, `block_double_frees`, `meta_kv_block_refs_drift`,
`pack_pin_refcount_drift`, `staged_payload_lost_reads`, `write_shared_class_violation`,
`read_dest_overruns`, `layout_map_merge_failures`, `promotions_failed`, `held_store_failures`,
`transport_zc_direct_leg_recheck_withdrawn`, both EFBIG refusals, `writeback_errors_latched`,
`fsync_promote_failures` all **0**; `volume_failed false`. The ledgers close: `Σ admit_write_*_bytes
≡ transport WRITE payload bytes (extracted + direct) + the truncate's zero tails`; `read_user_bytes ≡
dest + bounce + dest_dma + zc_serve + zc_pool_serve`; `terminal_frees ≡ queued + elided`;
`unpublished_mint_abandons ≡ write_pipeline_supersessions + write_pipeline_discards`.

The staged→striped promotion engaged 10× (the `dd`, the `cp` and the eight storm files each start
staged and promote at the fifth MiB); the W1 held-slot store engaged for every page-aligned
overwrite of a mapped block (51 of 64 aligned — the rest fell in holes or stream-adjacent and took
the extent/overlay arms, which is the predicate working); the direct device leg served every
whole-block READ of a settled binding (3 519 legs, 559 MB); the fd-source probe serve stayed at 0 in
these passes (every READ was either a first touch — a fill, served by the dispatch's dest DMA — or a
direct leg; the probe's hot/hold serve needs a second buffered touch of a resident block, which the
O_DIRECT re-read passes route around; the arm is tested at the transport e2e level).

### The kill matrix (`SQUEEZEFS_BLOCK_REFS_VERIFY=1` on every remount)

Each cycle: an 8 MiB file written `conv=fsync` (the strict ack law's subject), then a 200 MiB `dd`
killed `-9` once the inode claims the point, `umount2` of the dead connection, remount with the C8
oracle and the C2 reconcile, then the census: every `census_grain` (4 KiB) page inside a file's
claimed size is byte-exact against the image or a hole (zeros) — anything else is TORN.

| Kill | journal replay | staging ring | the in-flight file | acked files | C8 / C2 |
|---|---|---|---|---|---|
| mid-`dd` @ 50 MiB | 74–77 entries, 0 torn | 0 records | claims 50–52 MiB: 12 288–13 312 pages exact, 0–768 holes, **0 torn** | 1 byte-exact | 0 drift / 0 leaked, 0 lost |
| mid-`dd` @ 100 MiB | 138–139 entries, 0 torn | 0 records | 100–101 MiB: 25 600 exact, 0–256 holes, 0 torn | 2 byte-exact | 0 / 0, 0 |
| mid-`dd` @ 150 MiB | 194–201 entries, 0 torn | 0 records | 150–152 MiB: 37 888–38 912 exact, 0–768 holes, 0 torn | 3 byte-exact | 0 / 0, 0 |
| mid 8-way `cp` storm @ ≥ 100 MiB claimed | 376–636 entries, 0 torn | 0–4 records recovered (0–2 torn, dropped) | 8 of 8 present, 100–118 MiB claimed: 21 504–28 160 exact, 1 088–5 504 holes, 0 torn | 3 byte-exact | 0 / 0, 0 |
| then `rm` all → `df` baseline → clean cycle → `status` | `checkpoint seq 12, 14 inode(s) minted, not mounted`, refs census 0 blocks | | | | |

The holes are the pipeline's write-back window: a block whose DMA or publish had not landed reads as
zeros (SIZE NEVER LEADS DATA puts the claim after the pieces ack, so a crash leaves holes, never
garbage); `replay_dropped_torn` was 0 in every cycle (a process kill tears no O_DIRECT journal
write); the staging ring's torn records (a `cp` mid-stage) are dropped by its checksum and the file
reads its last intact image.

## Found and fixed on the way (red first)

- **Row 174 — a dangling held-lease pointer**: `heldPayload` handed the filesystem a `ctx` into a
  by-value COPY of the lease; the W1 direct store never dereferenced it, so the bug surfaced as `EIO`
  on the first 4 KiB overwrite into a HOLE of a striped file (held, patch-ineligible ⇒ materialize
  through the dead pointer). Pointer-identity test in `lifecycle.zig`; the smoke's overwrite stage is
  the belt.
- **The inline map fold assumed ascending inserts**: a rewrite epoch's save lists its shadowed blocks
  in feed order; the merge produced a non-ascending map and `publishLayout` refused it
  (`MapNotAscending`), the fsync after the overwrites answered `EIO` and every upload abandoned its
  mint. `DataPlane.foldInserts` is the pure fold (sorts, replaces, displaces) with its tie test.
- **Two flushes of one parked block minted two blocks**: a RELEASE's `flushIno` racing an fsync's
  `flushParked` (or two RELEASEs of one file under `dd`'s open/write/close per overwrite) both
  snapshotted the same entry; the loser found it retired after its DMA and abandoned the mint.
  `uploadParkedBlock` now JOINS an in-flight upload (`Table.upload_settled` notify, register-then-
  recheck, a 30 s bark `upload_join_overlong`) — `Disposition.joined`, `write_pipeline_joins`; the
  in-flight flag is cleared on every exit; a two-thread test pins one mint, one merge, `joined`.
- **`unpublished_mint_abandons` was a Tripwire** fired by a healthy outcome (a write landing during
  the upload's DMA — latest-wins); it is an event counter now with the smoke's identity above
  (row 175 below).
- **Range-scan resume cursors were malformed**: `key.keySuccessor` (one byte longer) is refused by
  the backend's routing for every slot-tree kind, so the C8 walk, the ledger population and the
  mount sweep failed past one page (512/1024 records): `key.sameKindSuccessor` + the slot tree's
  kind interleaving handled at the two ino-major walks (row 176).
- **Corpses left by a crash leaked their blocks**: a `rm` whose FORGET/gather died with the daemon
  (`kill -9` within the reclaim cadence's 5 s) left an inode at `nlink == 0` reachable by nothing,
  its references on the ledger forever — the kill matrix's `df` sat 74 blocks above the baseline.
  `KvFs.sweepCorpsesAtMount` destroys them before the mount serves (row 177); two tests over the RAM
  store (one past `SWEEP_PAGE`), the kill matrix's `df` baseline is the belt.
- The transport stats line outgrew its 3 KiB buffer (the zc legs' keys) and hid the must-stay-0
  gauges; 16 KiB.

## Models (explorer, default limits; every weakening proven to fail)

| Model | States | Weakening that fails |
|---|---|---|
| `coverage_model.TriggerModel` (3 disjoint / overlapping segments, every order) | 16–200 (tie bound) | the end-offset proxy fires on partial coverage |
| `device_overlay_model.FreezeDrainModel` (the Dekker pair) | 29 | EITHER SeqCst point weakened: a store invisible to the drain |
| `device_overlay_model.ValidatedServeModel` | 249 | skipping the generation revalidation serves a mix of two passes |
| `write_pipeline_model.AdmissionModel` (the depth governor) | 239 | the blocks-counter bypass admits two oversized bypassers; a check-then-store roll lets both roll |
| `patch_w1` / `block_alloc` W1 pair (`incarnation_model.PatchCloneModel`) | 27 | either SeqCst fence: the stale clone |
| `incarnation_model.FillModel` ×6 laws (2 reuse cycles) | 106 | both fence weakenings per law; a load/store acquire resurrects a freed block; a load/store release releases twice |
| `staging.GenerationAbaModel` | 4 | recycled generations: a stale release frees the re-staged sole copy |
| `pack.PinModel` / `pack.SealModel` | 9 / 4 | the nonzero check removed: the pin arithmetic; a load-then-store seal releases the pin twice |
| `write_epoch_model.CloseVisibilityModel` (the save vs a writer on its block; the follow-up commit) | 22 | `unbind_before_save` (the epoch out of sight before its merge): the writer patches/bases on the A key the save displaces; `base_from_map` (the base from the durable map under a shadow): the park's complement misses the bytes only B held |

## Gate totals

- Linux venue (`timeout 3000 scripts/venue zig build gate`): unit **1 346 pass, 8 skip**, rails 3/3,
  smoke **23/23** (≈ 52 s), fmt clean; afterwards `mount | grep fuse` shows only `fusectl`,
  `/build/tmp` carries no `smoke-*`/`mnt-*`, no `squeezefs` process lives.
- macOS (`zig build gate`): unit **1 129 pass, 15 skip** (Linux-only modules and every `smoke:`
  test), rails 3/3, fmt clean.

### The follow-up commit (the lost overwrite + the phantom stall)

- Linux venue (`timeout 3000 scripts/venue zig build gate`): unit **1 356 pass, 9 skip**, rails 3/3,
  smoke **24 pass, 1 skip** (the looped stage test skips without its knob), fmt clean; both
  data-plane passes report `ticked parks lock 0 / channel 0`, no daemon alert.
- The looped acceptance (`SQUEEZEFS_TEST_SMOKE_STAGE_LOOPS=24 … smoke -Dtest-filter=LOOPED`,
  one mount): **24/24 overwrite passes image-exact** (380–530 ms each), drift 0, untracked frees
  0, channel ticks 0, watchdog overdue 0; **24/24 storm passes** (1.3–2.1 s, 274–604 MB/s
  aggregate), every ledger closed, every op class's slowest bucket ≤ 1 s,
  `rewrite_shadow_close_waits = 179`.
- macOS (`zig build gate`): unit **1 138 pass, 15 skip**, rails 3/3, fmt clean.

## Deviations (this commit) and owed items

- **Block-map tree crossing** (`kvmap`, charter §5.6): a striped file whose inline map would exceed
  the record cap (1 364 entries at 4 MiB blocks ≈ 5.3 GiB) is refused `EFBIG`, counted
  (`fuse_write_beyond_inline_map_refused`, `layout_kvmap_crossing_refused`); the crossing (residue
  sweep + chunked Puts + the flip tx) is owed.
- **zcrx lane**: the kernel's zero-copy receive lane is not wired (the direct device leg and the
  fd-source leg are the two zc read legs this rung ships).
- **Rung-6 seams** stay stubs by design: the grant-armed allocator (`statfs` already reads
  `usedBlocks`/`reachableFreeBlocks`, the holder-view rule), `mapFencing` = 1 (one writer, one
  generation), the foreign-slot forest.
- **Per-op NVMe phase stamps** (`dev_queue`/`dev_service` as exact-sum legs of the write pipeline's
  `dma`) stay owed to `nvme_dev`.
- **NT stores** for the accumulation copy (`nt_copy_bytes` counts the bytes the NT path would move;
  the copy is `@memcpy` today).
- **The NVMe tier publish** is a memcpy into the tier (Q10's `WRITE_FIXED` from the fill pool owed).
- **The fd-source probe serve** never engaged in the smoke's passes (above); a buffered re-read
  storm with the hot tier primed is the row to add.
- **CLOSED (2026-10-07, the follow-up commit) — the lost 4 KiB overwrite on the staging set** (was: 1
  run in ~12 of the overwrite stage, byte 6 995 829 of `big` in block 1, with `rewrite_shadow_
  displaced_drift` and `block_untracked_free_refusals` in the same run). ROOT CAUSE, two faces of
  one flaw — the base of an overwrite was read from the DURABLE map, not the write-side authority:
  (1) `admitPiece` D captured `live_binding = map.lookup` into the entry's `old_binding` while the
  rewrite epoch already shadowed the block with B (fed by the previous `dd`'s RELEASE flush, not yet
  saved — RELEASE takes no inode op guard, so the next `dd`'s WRITE runs concurrently); the park's
  complement was later seeded from A (the bytes only B held — the previous overwrite — were lost),
  its upload fed a fresh epoch parking A (already freed by the first save), and that epoch's save
  displaced B unparked (the drift) and freed A twice (the untracked refusal). (2) `Epochs.close`
  took the epoch OUT of the table before its merge landed, so during the save `bindsBlock` read
  false: W1 patched the A key the save was about to displace (the patch vanished under the swap)
  and the overlay captured A as its old binding (dead at its settle ⇒ superseded ⇒ the acked bytes
  dropped). FIX (`write_epoch.zig` module doc — the save's visibility law; 0002 rows 178–179): the
  saving epoch stays visible (`Epochs.closing`) until its save landed or dropped; a record landing
  during the save opens the next epoch over the CLOSING shadow (its displaced key is B, parked as
  the map's — a transient save folds it back as the fresh epoch's own orphan mint); closes of one
  ino serialize in epoch order (`close_settled`, `rewrite_shadow_close_waits`); `Epochs.baseBinding`
  (the newest shadow, open or saving, else the map) is THE base at every use — the checkout (D),
  both seed reads, the read seam's partial gaps — and the feed hands `record` the LIVE map key
  (`record` prefers its shadows). The arm gate is written into the predicate list (`write_admit.zig`
  module doc). Also closed on the way: `record` caps an epoch at `MergeOutcome.max_displaced`
  blocks (the 65th answers `.not_shadowed` — the per-block durable publish — instead of being
  dropped from the save); the unused `CloseMode.try_`/`.busy` arm is deleted. RED FIRST: two tests
  in `write_admit.zig` reproduce both faces deterministically over the RAM seams (the dd-per-
  overwrite shape; a W1 + overlay write inside the save window through `RamMap.before_merge`) —
  both fail with either mechanism weakened; `write_epoch.zig` pins the serialized closes (a second
  close parks on the running save; the saves land B then C; A and B freed once each) and the
  transient fold; `write_epoch_model.CloseVisibilityModel` holds and BOTH weakenings fail
  (`unbind_before_save`: the pre-fix table removal; `base_from_map`: the pre-fix `old_binding`).
  ACCEPTANCE: the looped single-stage smoke (`SQUEEZEFS_TEST_SMOKE_STAGE_LOOPS=24`): **24/24
  overwrite passes image-exact** (distinct source pages per pass), drift 0, untracked frees 0, and
  24/24 storm passes with every ledger closed and `rewrite_shadow_close_waits = 179` (the
  serialization engaging).
- **CLOSED — the "5 s stall under the staging storm" was a phantom**: two observability defects,
  no parked op. Attributed by a stack dump at every channel tick in the venue (46 ticks over two
  gate runs, ALL from `reclaim.zig parkWake` — the reclaimer worker's idle park on its wake notify,
  2 s apart, with no work queued) and by the shape of every watchdog report (always exactly
  `5326–5331 ms`, never a second observation at ~10 s): (1) the watchdog judged an op's age as
  `now − coarse stamp`, and the stamp is the PREVIOUS tick's time for any op claimed since, so
  every op in flight when a tick landed read as one tick (+ the tick's ~326 ms jitter) old and the
  floor is one tick — eight healthy 300 ms storm writes "parked 5.3 s at `meta_backend_start`".
  FIX: the age is judged by its LOWER bound, `now − (the first refresh after the claim)`
  (`Registry.ageLowerBound`, `REFRESH_HISTORY` = 16 stamps); an op is named within two ticks of
  truly crossing; the tie test replays the eight writes under a 326 ms-late tick. (2)
  `channel_ticked_reregisters` counted every tick elapsed, so a long-lived worker idling on an
  unbounded notify wait grew it on a healthy daemon. FIX (`tick.zig` module doc; 0002 row 180): a
  channel-class park counts a RESCUE only — a tick whose re-check under the primitive's lock finds
  the condition satisfied with no wake token delivered (notify, oneshot/mpsc/watch, single-flight,
  semaphore, the read fill cohort); the lock counters keep their contract. Both are must-stay-0 in
  the smoke now (`lock_ticked_reregisters`, `channel_ticked_reregisters`, `fuse_op_watchdog_
  overdue`), the per-class `fuse_op_latency_ns` histograms are in the dump (`stats ops:`), and the
  looped smoke bounds every op class's slowest bucket at `<=1s` on all 48 passes (the real tail of
  the storm's writes is the conveyor window, ≤ 256 ms). The notify test pins both halves: an idle
  waiter past a tick counts nothing; a dropped wake is rescued and counts exactly one.
- **CLOSED (2026-10-07) — the RELEASE wedge on the block-map-tree smoke stage** (five `release`
  ops of one ino in flight ≥ 10–30 s, `lock_ticked_reregisters` climbing, `invariant_tripwires`
  0, nothing else in flight). Reproduced in the venue with the daemon log: `overlay ino 2 block
  355: settle transient, attempt 1, 2, 4 … 128` preceding every watchdog line, and `dmesg`
  `I/O error, dev vdc … op WRITE` in the same seconds — the venue's `/build` volume image on the
  macOS host had hit a FULL host disk (the container volume is never discarded; the three agents'
  smoke runs grew its backing file past the host's free space), so the daemon's data-volume DMAs
  answered EIO. The daemon's defect behind the picture: the overlay write-arm settle retried the
  transient (`seedGaps`'s device write) forever with its backoff SLEEPING UNDER the (3) block
  guard, and every RELEASE's `drainForIno` queued on that guard — a park under the block guard,
  with no holder the watchdog could name (the settler is a detached task, not an op). FIX (0002
  row 208; `device_overlay.zig` module doc): one (3) acquisition per settle attempt, the ladder's
  sleep with the guard dropped and the record FROZEN never-lossy between attempts; the
  guard-holding venues (the store's steal, the write ladder's one-authority screen) make ONE
  attempt and the screen drops its guard for the ladder and restarts from A; the read side's
  `.settle` venue goes through `drainBlock` (it had called the settle with no guard at all);
  `VolumeFailed`/`WriterGuardFenced` from the seed read/write or the flush is the FENCE (W5,
  `overlay_fence_drops`), not a transient. RED FIRST: `device_overlay.zig` — a device that keeps
  failing the gap seed lets a sibling take the guard mid-ladder (fails on the old code with the
  sibling never getting the guard while the settler barks under it), the fence-immediate legs;
  `write_admit.zig` — the one-authority screen's guard drop + restart. OWED: a device that never
  recovers leaves its settlers retrying with the guard dropped (named by the watchdog) until the
  fail-stop lattice's device escalation lands; the venue's volume now mounts `discard` and the
  smoke loop trims between passes.
- **CLOSED (2026-10-07) — the read-path single-flight cohort flake** (`expected 1, found 2`
  device fetches, 6 in 120 runs under load, both `Io`s): a reader whose tier probes missed was
  preempted in the probe→enter window while the sibling's whole flight ran to completion (filled,
  deposited, published, closed — the hold entry consumed by the cohort's credits), then claimed a
  FRESH flight for a block the hot tier already held and fetched it again. FIX: the primary
  re-probes the tiers after claiming its flight (`read_fill_reprobe_hits`; a pool-backed hit
  carries to the cohort like a fill, an LRU hit dissolves the flight to the elected waiter). RED
  FIRST: the `before_flight` seam holds a reader in the window while a sibling's read completes
  — `expected 1, found 2` on the old code, deterministic; 150/150 loop passes after.
- **Per-ino inline layout record** is the map authority (read latch-free through the node cache,
  one decode per probe); a RAM layout cache is owed with the kvmap crossing.
- `status`'s reference census walks the ledger (one page per 512 refs); fsck C8 offline is rung 8.
